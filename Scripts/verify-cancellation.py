#!/usr/bin/env python3
"""Verify real CLI signal handling and MCP request cancellation over stdio."""

from __future__ import annotations

import argparse
import errno
import json
import os
import selectors
import signal
import subprocess
import sys
import tempfile
import time
from pathlib import Path
from typing import Any


ACK = b"Cancellation requested; press Ctrl+C again to exit immediately.\n"
ROOT = "main.root"
CHILD = "main.child"
TARGET = "main.target"
DEADLINE = 20.0


def fail(message: str) -> None:
    """검증 실패를 하나의 예외로 모아 main에서 영어 오류로 출력한다."""
    raise RuntimeError(message)


def deadline(seconds: float = DEADLINE) -> float:
    """모든 대기 루프가 공유할 단조 시계 기준을 만든다."""
    return time.monotonic() + seconds


def wait_slice(end: float) -> None:
    """바쁜 대기를 피하면서도 고정된 지연을 테스트 동기화에 쓰지 않는다."""
    remaining = end - time.monotonic()
    if remaining > 0:
        selector = selectors.DefaultSelector()
        try:
            selector.select(min(remaining, 0.05))
        finally:
            selector.close()


def ensure_running(process: subprocess.Popen[bytes]) -> None:
    """프로세스가 FIFO를 열기 전에 죽은 경우 원인을 잃지 않게 한다."""
    returncode = process.poll()
    if returncode is not None:
        fail(f"engine exited {returncode} before opening the graph FIFO")


def open_fifo_writer(process: subprocess.Popen[bytes], path: Path) -> int:
    """읽기 쪽이 열린 순간까지 nonblocking writer open을 재시도한다."""
    end = deadline()
    while True:
        try:
            descriptor = os.open(path, os.O_WRONLY | os.O_NONBLOCK)
            os.set_blocking(descriptor, True)
            return descriptor
        except OSError as error:
            if error.errno != errno.ENXIO:
                raise
            ensure_running(process)
            if time.monotonic() >= end:
                fail("timed out waiting for the engine to open the graph FIFO")
            wait_slice(end)


def wait_for_ack(process: subprocess.Popen[bytes]) -> None:
    """첫 SIGINT의 정확한 stderr 한 줄을 selector로 기다린다."""
    assert process.stderr is not None
    selector = selectors.DefaultSelector()
    selector.register(process.stderr, selectors.EVENT_READ)
    pending = bytearray()
    end = deadline()
    try:
        while True:
            if b"\n" in pending:
                line, _separator, _rest = bytes(pending).partition(b"\n")
                actual = line + b"\n"
                if actual != ACK:
                    fail(f"unexpected cancellation stderr: {actual!r}")
                return
            if time.monotonic() >= end:
                fail("timed out waiting for the cancellation acknowledgement")
            events = selector.select(max(0.0, min(end - time.monotonic(), 0.2)))
            if not events:
                ensure_running(process)
                continue
            chunk = os.read(process.stderr.fileno(), 4096)
            if not chunk:
                fail(f"engine closed stderr before cancellation acknowledgement: {bytes(pending)!r}")
            pending.extend(chunk)
    finally:
        selector.close()


def write_all(descriptor: int, payload: bytes) -> None:
    """FIFO writer가 부분 write를 해도 graph JSON을 모두 전송한다."""
    offset = 0
    while offset < len(payload):
        offset += os.write(descriptor, payload[offset:])


def reap(process: subprocess.Popen[bytes], expected: int) -> tuple[bytes, bytes]:
    """남은 stdout/stderr를 수거하고 종료 코드와 함께 검증한다."""
    try:
        stdout, stderr = process.communicate(timeout=DEADLINE)
    except subprocess.TimeoutExpired:
        process.terminate()
        try:
            stdout, stderr = process.communicate(timeout=2)
        except subprocess.TimeoutExpired:
            process.kill()
            stdout, stderr = process.communicate()
        fail("engine did not exit after its graph input was closed")
    if process.returncode != expected:
        detail = (stderr or stdout).decode("utf-8", errors="replace").strip()
        fail(f"engine exited {process.returncode}, expected {expected}: {detail}")
    return stdout, stderr


def simple_graph() -> dict[str, Any]:
    """세 정점으로 query, impact, path의 유효한 endpoint를 고정한다."""
    vertices = [
        {"id": ROOT, "kind": "table", "level": "object", "name": "root", "schema": "main"},
        {"id": CHILD, "kind": "view", "level": "object", "name": "child", "schema": "main"},
        {"id": TARGET, "kind": "view", "level": "object", "name": "target", "schema": "main"},
    ]
    edges = [
        {"from": CHILD, "kind": "reads", "to": ROOT},
        {"from": TARGET, "kind": "reads", "to": CHILD},
    ]
    return {"version": 2, "vertices": vertices, "edges": edges}


def cancellation_command(
    engine: Path,
    work: Path,
    arguments: list[str],
    graph_bytes: bytes,
) -> tuple[bytes, bytes]:
    """FIFO를 graph 입력으로 삼아 첫 취소가 로딩 중 관측되는지 검사한다."""
    fifo = work / f"blocked-{arguments[0]}.graph.json"
    os.mkfifo(fifo)
    process = subprocess.Popen(
        [str(engine), *arguments, "--graph", str(fifo)],
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    )
    writer = -1
    try:
        writer = open_fifo_writer(process, fifo)
        os.kill(process.pid, signal.SIGINT)
        wait_for_ack(process)
        write_all(writer, graph_bytes)
        os.close(writer)
        writer = -1
        return reap(process, 130)
    finally:
        if writer >= 0:
            os.close(writer)
        if process.poll() is None:
            process.terminate()
            try:
                process.wait(timeout=2)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait()


def assert_json(stdout: bytes, label: str) -> dict[str, Any]:
    """취소 결과가 stdout의 단일 JSON object인지 확인한다."""
    try:
        value = json.loads(stdout.decode("utf-8"))
    except (UnicodeDecodeError, json.JSONDecodeError) as error:
        fail(f"{label} did not emit JSON after cancellation: {error}")
    if not isinstance(value, dict):
        fail(f"{label} cancellation output is not an object: {value!r}")
    return value


def assert_cancelled_report(value: dict[str, Any], label: str, *, complete: bool | None) -> None:
    """CLI 분석 결과가 취소를 partial/truncated 사실로 보존하는지 확인한다."""
    reasons = value.get("truncationReasons")
    if not isinstance(reasons, list) or "cancelled" not in reasons:
        fail(f"{label} omitted cancelled from truncationReasons: {value}")
    if complete is not None and value.get("complete") is not complete:
        fail(f"{label} complete flag was {value.get('complete')!r}, expected {complete}")


def check_cli_queries(engine: Path, work: Path) -> None:
    """query, impact, path가 graph reader를 막은 채 첫 SIGINT를 처리하는지 검사한다."""
    graph_bytes = json.dumps(simple_graph(), sort_keys=True, separators=(",", ":")).encode()
    cases = (
        (
            "query",
            ["query", ROOT, "--depth", "8", "--max", "256", "--max-visited", "100000", "--max-examined-edges", "1000000"],
            False,
        ),
        (
            "impact",
            ["impact", ROOT, "--max", "256", "--max-visited", "100000", "--max-examined-edges", "1000000"],
            False,
        ),
        (
            "path",
            ["path", TARGET, ROOT, "--depth", "8", "--max-paths", "32", "--max-visited", "100000", "--max-edges", "1000000"],
            None,
        ),
    )
    for label, arguments, complete in cases:
        stdout, _stderr = cancellation_command(engine, work, arguments, graph_bytes)
        value = assert_json(stdout, label)
        assert_cancelled_report(value, label, complete=complete)
        if label == "path" and value.get("truncated") is not True:
            fail(f"path cancellation did not set truncated=true: {value}")


def check_inherited_sigint_ignore(engine: Path, work: Path) -> None:
    """SIGINT 무시 상태로 exec된 정상 query도 handler 설치 후 실행되는지 검사한다."""
    graph = work / "normal.graph.json"
    graph.write_text(json.dumps(simple_graph(), sort_keys=True), encoding="utf-8")
    wrapper = (
        "import os, signal, sys; "
        "signal.signal(signal.SIGINT, signal.SIG_IGN); "
        "os.execv(sys.argv[1], [sys.argv[1], *sys.argv[2:]])"
    )
    try:
        result = subprocess.run(
            [
                sys.executable,
                "-c",
                wrapper,
                str(engine),
                "query",
                ROOT,
                "--graph",
                str(graph),
            ],
            capture_output=True,
            timeout=DEADLINE,
            check=False,
        )
    except subprocess.TimeoutExpired:
        fail("normal query inherited SIGINT=SIG_IGN and did not finish")
    if result.returncode != 0:
        detail = (result.stderr or result.stdout).decode("utf-8", errors="replace").strip()
        fail(f"normal query with inherited SIGINT=SIG_IGN exited {result.returncode}: {detail}")
    value = assert_json(result.stdout, "normal query")
    if value.get("complete") is not True or "cancelled" in value.get("truncationReasons", []):
        fail(f"normal query was unexpectedly cancelled: {value}")


def check_second_interrupt(engine: Path, work: Path) -> None:
    """두 번째 SIGINT가 graph loading처럼 협력 취소 불가능한 상태도 끝내는지 검사한다."""
    fifo = work / "second-interrupt.graph.json"
    os.mkfifo(fifo)
    process = subprocess.Popen(
        [str(engine), "impact", ROOT, "--graph", str(fifo)],
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    )
    writer = -1
    try:
        writer = open_fifo_writer(process, fifo)
        os.kill(process.pid, signal.SIGINT)
        wait_for_ack(process)
        os.kill(process.pid, signal.SIGINT)
        end = deadline()
        while process.poll() is None and time.monotonic() < end:
            wait_slice(end)
        if process.poll() is None:
            fail("second SIGINT did not force the blocked graph load to exit")
        if process.returncode != 130:
            fail(f"second SIGINT exited {process.returncode}, expected 130")
    finally:
        if writer >= 0:
            os.close(writer)
        if process.poll() is None:
            process.terminate()
            try:
                process.wait(timeout=2)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait()
        if process.stdout is not None:
            process.stdout.close()
        if process.stderr is not None:
            process.stderr.close()


def catalog_document() -> dict[str, Any]:
    """review가 읽을 최소한의 v1 catalog document를 만든다."""
    return {
        "version": 1,
        "dialect": "sqlite",
        "reader": "verify-cancellation",
        "limitations": [],
        "schemas": [
            {
                "name": "main",
                "objects": [
                    {
                        "name": "root",
                        "kind": "table",
                        "columns": [],
                        "constraints": [],
                        "indexes": [],
                        "triggers": [],
                    }
                ],
                "routines": [],
            }
        ],
    }


def check_closed_stderr(engine: Path, work: Path) -> None:
    """취소 안내를 쓸 수 없어도 handler panic으로 입력 대기에 남지 않게 한다."""
    fifo = work / "closed-stderr.graph.json"
    os.mkfifo(fifo)
    process = subprocess.Popen(
        [str(engine), "impact", ROOT, "--graph", str(fifo)],
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    )
    writer = -1
    try:
        writer = open_fifo_writer(process, fifo)
        assert process.stderr is not None
        process.stderr.close()
        process.stderr = None
        os.kill(process.pid, signal.SIGINT)
        reap(process, 130)
    finally:
        if writer >= 0:
            os.close(writer)
        if process.poll() is None:
            process.kill()
            process.wait()


def check_review(engine: Path, work: Path) -> None:
    """review 준비 단계도 첫 catalog 입력 전에 핸들러를 설치하는지 검사한다."""
    fifo = work / "blocked-before.json"
    after = work / "after.json"
    os.mkfifo(fifo)
    after.write_text(json.dumps(catalog_document(), sort_keys=True), encoding="utf-8")
    process = subprocess.Popen(
        [str(engine), "review", str(fifo), str(after)],
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    )
    writer = -1
    try:
        writer = open_fifo_writer(process, fifo)
        os.kill(process.pid, signal.SIGINT)
        wait_for_ack(process)
        # catalog loader는 rewind를 요구하므로 FIFO를 유효한 입력으로 끝까지
        # 읽어 review 분석을 검증할 수 없다. 준비 단계의 강제 종료만 확인한다.
        os.kill(process.pid, signal.SIGINT)
        try:
            process.wait(timeout=DEADLINE)
        except subprocess.TimeoutExpired:
            fail("second SIGINT did not force blocked review preparation to exit")
        if process.returncode != 130:
            fail(f"blocked review preparation exited {process.returncode}, expected 130")
    finally:
        if writer >= 0:
            os.close(writer)
        if process.poll() is None:
            process.terminate()
            try:
                process.wait(timeout=2)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait()


def dense_graph() -> dict[str, Any]:
    """MCP worker가 취소 통지를 읽기 전에 끝나지 않도록 중간 규모 graph를 만든다."""
    count = 18_000
    vertices: list[dict[str, str]] = [
        {"id": ROOT, "kind": "table", "level": "object", "name": "root", "schema": "main"}
    ]
    edges: list[dict[str, str]] = []
    for index in range(count):
        identifier = f"main.node_{index:05d}"
        vertices.append(
            {
                "id": identifier,
                "kind": "view",
                "level": "object",
                "name": f"node_{index:05d}",
                "schema": "main",
            }
        )
        edges.append({"from": identifier, "kind": "reads", "to": ROOT})
        for offset in range(1, 10):
            target = (index - offset) % count
            edges.append(
                {"from": identifier, "kind": "reads", "to": f"main.node_{target:05d}"}
            )
    return {"version": 2, "vertices": vertices, "edges": edges}


def id_key(value: object) -> tuple[str, str]:
    """JSON-RPC id의 숫자/문자열 구분을 보존한다."""
    return type(value).__name__, json.dumps(value, sort_keys=True, separators=(",", ":"))


class McpReader:
    """stdout/stderr를 함께 읽어 응답 순서와 종료 오류를 잃지 않는다."""

    def __init__(self, process: subprocess.Popen[bytes]) -> None:
        assert process.stdout is not None
        assert process.stderr is not None
        self.process = process
        self.selector = selectors.DefaultSelector()
        self.selector.register(process.stdout, selectors.EVENT_READ, "stdout")
        self.selector.register(process.stderr, selectors.EVENT_READ, "stderr")
        self.stdout_buffer = bytearray()
        self.stderr = bytearray()

    def close(self) -> None:
        """selector와 pipe descriptor를 정리한다."""
        self.selector.close()

    def wait_for(self, expected: set[tuple[str, str]], forbidden: tuple[str, str] | None = None) -> dict[tuple[str, str], dict[str, Any]]:
        """기대한 JSON-RPC response가 모두 올 때까지 id별로 수집한다."""
        responses: dict[tuple[str, str], dict[str, Any]] = {}
        end = deadline()
        while set(responses) != expected:
            if time.monotonic() >= end:
                fail(f"timed out waiting for MCP responses; got {sorted(responses)}; stderr={bytes(self.stderr)!r}")
            events = self.selector.select(max(0.0, min(end - time.monotonic(), 0.2)))
            if not events:
                if self.process.poll() is not None:
                    fail(f"MCP server exited {self.process.returncode}; stderr={bytes(self.stderr)!r}")
                continue
            for key, _mask in events:
                chunk = os.read(key.fileobj.fileno(), 65536)
                if not chunk:
                    if key.data == "stdout":
                        fail(f"MCP stdout closed before responses; stderr={bytes(self.stderr)!r}")
                    self.selector.unregister(key.fileobj)
                    continue
                if key.data == "stderr":
                    self.stderr.extend(chunk)
                    continue
                self.stdout_buffer.extend(chunk)
                while b"\n" in self.stdout_buffer:
                    raw, _separator, rest = bytes(self.stdout_buffer).partition(b"\n")
                    self.stdout_buffer = bytearray(rest)
                    try:
                        message = json.loads(raw.decode("utf-8"))
                    except (UnicodeDecodeError, json.JSONDecodeError) as error:
                        fail(f"MCP emitted invalid JSON: {error}")
                    if not isinstance(message, dict) or "id" not in message:
                        continue
                    key_id = id_key(message["id"])
                    if forbidden is not None and key_id == forbidden:
                        fail(f"canceled MCP request unexpectedly received a response: {message}")
                    if key_id in expected:
                        responses[key_id] = message
        return responses


def send_json(process: subprocess.Popen[bytes], message: dict[str, Any]) -> None:
    """한 JSON-RPC line을 flush해 stdin open 상태로 전달한다."""
    assert process.stdin is not None
    try:
        process.stdin.write(json.dumps(message, separators=(",", ":")).encode() + b"\n")
        process.stdin.flush()
    except OSError as error:
        fail(f"could not write MCP request: {error}")


def check_mcp(engine: Path, work: Path) -> None:
    """취소된 impact 뒤에도 같은 MCP worker가 ping/query를 처리하는지 검사한다."""
    graph = work / "dense.graph.json"
    graph.write_text(json.dumps(dense_graph(), sort_keys=True, separators=(",", ":")), encoding="utf-8")
    process = subprocess.Popen(
        [str(engine), "serve", "--graph", str(graph)],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    )
    reader = McpReader(process)
    canceled = id_key(9001)
    try:
        send_json(
            process,
            {
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": {
                    "protocolVersion": "2025-11-25",
                    "capabilities": {},
                    "clientInfo": {"name": "verify-cancellation", "version": "1"},
                },
            },
        )
        initialized = reader.wait_for({id_key(1)})[id_key(1)]
        if "error" in initialized or "result" not in initialized:
            fail(f"MCP initialize failed: {initialized}")
        send_json(process, {"jsonrpc": "2.0", "method": "notifications/initialized"})
        send_json(process, {"jsonrpc": "2.0", "method": "notifications/unknown", "params": {}})
        send_json(process, {"jsonrpc": "2.0", "method": "notifications/cancelled", "params": {}})
        send_json(
            process,
            {
                "jsonrpc": "2.0",
                "id": 9001,
                "method": "tools/call",
                "params": {
                    "name": "impact",
                    "arguments": {
                        "name": ROOT,
                        "max": 10_000,
                        "maxVisited": 100_000,
                        "maxExaminedEdges": 1_000_000,
                    },
                },
            },
        )
        # 알 수 없거나 잘못된 control notification도 worker를 망가뜨리지 않아야 한다.
        send_json(
            process,
            {
                "jsonrpc": "2.0",
                "method": "notifications/cancelled",
                "params": {"requestId": "9001"},
            },
        )
        send_json(
            process,
            {
                "jsonrpc": "2.0",
                "method": "notifications/cancelled",
                "params": {"requestId": {}},
            },
        )
        send_json(
            process,
            {
                "jsonrpc": "2.0",
                "method": "notifications/cancelled",
                "params": {"requestId": 9001},
            },
        )
        send_json(process, {"jsonrpc": "2.0", "id": "ping-after-cancel", "method": "ping"})
        send_json(
            process,
            {
                "jsonrpc": "2.0",
                "id": 3,
                "method": "tools/call",
                "params": {"name": "query", "arguments": {"name": ROOT, "depth": 1, "max": 1}},
            },
        )
        responses = reader.wait_for(
            {id_key("ping-after-cancel"), id_key(3)}, forbidden=canceled
        )
        for key_id in (id_key("ping-after-cancel"), id_key(3)):
            if "error" in responses[key_id] or "result" not in responses[key_id]:
                fail(f"MCP request {key_id} failed after cancellation: {responses[key_id]}")
        query_result = responses[id_key(3)]["result"]
        if not isinstance(query_result, dict) or query_result.get("isError") is not False:
            fail(f"MCP query returned a tool error after cancellation: {responses[id_key(3)]}")
        structured = query_result.get("structuredContent")
        if not isinstance(structured, dict):
            fail(f"MCP query omitted structuredContent after cancellation: {responses[id_key(3)]}")
        subject = structured.get("subject")
        if not isinstance(subject, dict) or subject.get("id") != ROOT:
            fail(f"MCP follow-up query returned the wrong subject: {structured}")
        if structured.get("complete") is not True:
            fail(f"MCP follow-up query was incomplete: {structured}")
        if "cancelled" in structured.get("truncationReasons", []):
            fail(f"MCP follow-up query leaked cancellation state: {structured}")
        send_json(
            process,
            {
                "jsonrpc": "2.0",
                "id": 4,
                "method": "tools/call",
                "params": {"name": "query", "arguments": {"name": ROOT, "depth": 1, "max": 1}},
            },
        )
        assert process.stdin is not None
        process.stdin.close()
        drained = reader.wait_for({id_key(4)}, forbidden=canceled)[id_key(4)]
        if "error" in drained or "result" not in drained:
            fail(f"MCP accepted request was not drained at EOF: {drained}")
        drained_result = drained["result"]
        if not isinstance(drained_result, dict) or drained_result.get("isError") is not False:
            fail(f"MCP EOF-drained query returned a tool error: {drained}")
        try:
            process.wait(timeout=DEADLINE)
        except subprocess.TimeoutExpired:
            fail("MCP server did not drain ordinary requests and exit at stdin EOF")
        if process.returncode != 0:
            fail(f"MCP server exited {process.returncode} after EOF: {bytes(reader.stderr)!r}")
    finally:
        reader.close()
        if process.stdin is not None and not process.stdin.closed:
            process.stdin.close()
        if process.poll() is None:
            process.terminate()
            try:
                process.wait(timeout=2)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait()
        if process.stdout is not None:
            process.stdout.close()
        if process.stderr is not None:
            process.stderr.close()


def main() -> int:
    parser = argparse.ArgumentParser(description="Run process-level cancellation regression checks.")
    parser.add_argument("--engine", required=True, type=Path, help="path to an executable schemagraph binary")
    arguments = parser.parse_args()
    engine = arguments.engine.resolve()
    if os.name != "posix":
        print("cancellation process regression requires POSIX FIFOs", file=sys.stderr)
        return 2
    if not engine.is_file() or not engine.stat().st_mode & 0o111:
        print(f"error: engine is not an executable file: {engine}", file=sys.stderr)
        return 2
    try:
        with tempfile.TemporaryDirectory(prefix="schemagraph-cancellation-") as directory:
            work = Path(directory)
            check_cli_queries(engine, work)
            check_inherited_sigint_ignore(engine, work)
            check_review(engine, work)
            check_second_interrupt(engine, work)
            check_closed_stderr(engine, work)
            check_mcp(engine, work)
    except (OSError, RuntimeError, ValueError) as error:
        print(f"cancellation process regression failed: {error}", file=sys.stderr)
        return 1
    print("cancellation process regression: ok")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
