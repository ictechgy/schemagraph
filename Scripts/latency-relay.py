#!/usr/bin/env python3
"""로컬 DB 앞에서 TCP 바이트를 방향마다 정해진 시간만큼 늦춰 넘기는 relay다.

원격 DB의 왕복 지연을 로컬에서 흉내 내어, 수집 시간이 왕복 수에 비례하는지 잴 때 쓴다.
대역폭은 제한하지 않는다. 받은 청크마다 도착 시각에 지연을 더해 받은 순서대로 넘기므로,
방향마다 d ms면 요청-응답 한 번이 약 2d ms 늘어난다. 벤치마크 스크립트는 LatencyRelay를
모듈로 불러 쓰고, 다른 DB 앞에는 명령으로 띄운다:

    python3 Scripts/latency-relay.py --upstream-port 3306 --one-way-ms 1
"""

from __future__ import annotations

import argparse
import asyncio
import json
import signal
import sys
import threading
import time

CHUNK_BYTES = 65536


async def pump(reader: asyncio.StreamReader, writer: asyncio.StreamWriter, delay: float) -> None:
    """한 방향의 청크를 도착 순서대로 delay초 뒤에 넘기고, 끝(EOF)은 반쪽 닫기로 전한다.

    읽기와 쓰기를 따로 돌린다. 읽기가 쓰기를 기다리면 뒤 청크의 지연이 앞 청크의 대기만큼 더
    늘어나 실제 네트워크보다 느린 relay가 된다.
    """
    queue: asyncio.Queue[tuple[float, bytes]] = asyncio.Queue()

    async def receive() -> None:
        while True:
            data = await reader.read(CHUNK_BYTES)
            await queue.put((time.monotonic() + delay, data))
            if not data:
                return

    async def deliver() -> None:
        while True:
            due, data = await queue.get()
            wait = due - time.monotonic()
            if wait > 0:
                await asyncio.sleep(wait)
            if not data:
                if writer.can_write_eof():
                    writer.write_eof()
                return
            writer.write(data)
            await writer.drain()

    # 한쪽이 실패하면 남은 쪽은 끝나지 않으므로(빈 큐를 기다린다) 함께 취소한다.
    tasks = [asyncio.ensure_future(receive()), asyncio.ensure_future(deliver())]
    try:
        await asyncio.gather(*tasks)
    finally:
        for task in tasks:
            task.cancel()


class LatencyRelay:
    """127.0.0.1의 빈 포트에서 받아 upstream으로 잇는 지연 relay를 별도 스레드에서 돌린다.

    with 블록 안에서만 열려 있다. 연결이 오류로 끝나면 그 사유를 errors에 모은다. 쓰는 쪽은 수집
    프로세스의 종료 코드로 성패를 가리고, errors는 진단용으로 결과에 함께 남긴다(정상 종료 직후
    상대가 먼저 닫아 반쪽 닫기가 실패하는 경우도 여기에 남는다).
    """

    def __init__(self, upstream_port: int, one_way_ms: float, upstream_host: str = "127.0.0.1", listen_port: int = 0):
        if one_way_ms < 0:
            raise ValueError("one-way latency must not be negative")
        self.upstream_host = upstream_host
        self.upstream_port = upstream_port
        self.delay = one_way_ms / 1000
        self.listen_port = listen_port
        self.port = 0
        self.connections = 0
        self.errors: list[str] = []
        self._lock = threading.Lock()
        self._loop: asyncio.AbstractEventLoop | None = None
        self._thread: threading.Thread | None = None
        self._startup_error: BaseException | None = None

    def __enter__(self) -> LatencyRelay:
        self._loop = asyncio.new_event_loop()
        ready = threading.Event()
        self._thread = threading.Thread(target=self._serve, args=(ready,), name="latency-relay", daemon=True)
        self._thread.start()
        if not ready.wait(10):
            raise RuntimeError("latency relay did not start within 10 s")
        if self._startup_error is not None:
            raise RuntimeError(f"latency relay could not listen: {self._startup_error}") from self._startup_error
        return self

    def __exit__(self, *_exc: object) -> None:
        if self._loop is not None and self._thread is not None:
            self._loop.call_soon_threadsafe(self._loop.stop)
            self._thread.join(10)
            if self._thread.is_alive():
                raise RuntimeError("latency relay thread did not stop within 10 s")

    def snapshot(self) -> dict[str, object]:
        """지금까지의 연결 수와 오류 사본을 돌려준다."""
        with self._lock:
            return {"connections": self.connections, "errors": list(self.errors)}

    def _record(self, message: str) -> None:
        with self._lock:
            self.errors.append(message)

    def _serve(self, ready: threading.Event) -> None:
        loop = self._loop
        assert loop is not None
        asyncio.set_event_loop(loop)
        try:
            server = loop.run_until_complete(asyncio.start_server(self._relay, "127.0.0.1", self.listen_port))
        except OSError as error:
            self._startup_error = error
            ready.set()
            loop.close()
            return
        self.port = int(server.sockets[0].getsockname()[1])
        ready.set()
        try:
            loop.run_forever()
        finally:
            server.close()
            pending = asyncio.all_tasks(loop)
            for task in pending:
                task.cancel()
            loop.run_until_complete(asyncio.gather(*pending, return_exceptions=True))
            loop.close()

    async def _relay(self, client_reader: asyncio.StreamReader, client_writer: asyncio.StreamWriter) -> None:
        with self._lock:
            self.connections += 1
        try:
            upstream_reader, upstream_writer = await asyncio.open_connection(self.upstream_host, self.upstream_port)
        except OSError as error:
            self._record(f"upstream {self.upstream_host}:{self.upstream_port} refused the connection: {error}")
            client_writer.close()
            return
        directions = [
            asyncio.ensure_future(pump(client_reader, upstream_writer, self.delay)),
            asyncio.ensure_future(pump(upstream_reader, client_writer, self.delay)),
        ]
        # 한 방향이 오류로 끝나면 다른 방향은 더 받을 것이 없으므로 취소하고, 두 방향의 결과를 한 번씩 모은다.
        _, unfinished = await asyncio.wait(directions, return_when=asyncio.FIRST_EXCEPTION)
        for direction in unfinished:
            direction.cancel()
        for outcome in await asyncio.gather(*directions, return_exceptions=True):
            if isinstance(outcome, Exception):
                self._record(f"connection ended with {type(outcome).__name__}: {outcome}")
        for writer in (client_writer, upstream_writer):
            writer.close()


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--upstream-host", default="127.0.0.1")
    parser.add_argument("--upstream-port", type=int, required=True)
    parser.add_argument("--one-way-ms", type=float, required=True, help="delay added in each direction")
    parser.add_argument("--listen-port", type=int, default=0, help="loopback port to listen on (default: any free port)")
    args = parser.parse_args()
    if args.one_way_ms < 0:
        parser.error("--one-way-ms must not be negative")
    stop = threading.Event()
    for number in (signal.SIGINT, signal.SIGTERM):
        signal.signal(number, lambda *_: stop.set())
    try:
        with LatencyRelay(args.upstream_port, args.one_way_ms, args.upstream_host, args.listen_port) as relay:
            print(json.dumps({"port": relay.port, "upstream": f"{args.upstream_host}:{args.upstream_port}",
                              "one_way_ms": args.one_way_ms}), flush=True)
            stop.wait()
            summary = relay.snapshot()
    except (OSError, RuntimeError, ValueError) as error:
        print(f"error: {error}", file=sys.stderr)
        return 1
    for message in summary["errors"]:
        print(f"relay: {message}", file=sys.stderr)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
