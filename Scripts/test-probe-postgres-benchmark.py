#!/usr/bin/env python3
"""원격 PostgreSQL 수집 벤치마크의 지연 relay와 문장 계수가 측정값을 왜곡하지 않는지 확인한다."""
import importlib.util
import os
from pathlib import Path
import socket
import socketserver
import subprocess
import sys
import tempfile
import threading
import time
import unittest

HERE = Path(__file__).resolve().parent


def load(name, filename):
    """하이픈이 든 스크립트 파일을 모듈로 불러온다."""
    spec = importlib.util.spec_from_file_location(name, HERE / filename)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


RELAY = load('latency_relay', 'latency-relay.py')
BENCH = load('probe_postgres_benchmark', 'benchmark-probe-postgres.py')


class EchoHandler(socketserver.BaseRequestHandler):
    """받은 바이트를 그대로 돌려주고, 상대가 쓰기를 닫으면 연결을 닫는다."""

    def handle(self):
        while True:
            data = self.request.recv(65536)
            if not data:
                return
            self.request.sendall(data)


class EchoServer(socketserver.ThreadingTCPServer):
    daemon_threads = True
    allow_reuse_address = True


def receive_exactly(connection, size):
    """size바이트를 다 받거나, 그 전에 연결이 끝나면 실패한다."""
    chunks, received = [], 0
    while received < size:
        chunk = connection.recv(min(65536, size - received))
        if not chunk:
            raise AssertionError(f'connection ended after {received} of {size} bytes')
        chunks.append(chunk)
        received += len(chunk)
    return b''.join(chunks)


class LatencyRelayTests(unittest.TestCase):
    """relay는 방향마다 지연을 더하되 바이트·순서·끝(EOF)을 바꾸지 않아야 한다."""

    def setUp(self):
        self.server = EchoServer(('127.0.0.1', 0), EchoHandler)
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()
        self.upstream_port = self.server.server_address[1]

    def tearDown(self):
        self.server.shutdown()
        self.server.server_close()
        self.thread.join(5)

    def test_round_trip_pays_the_delay_in_both_directions(self):
        with RELAY.LatencyRelay(self.upstream_port, 40) as relay:
            with socket.create_connection(('127.0.0.1', relay.port), timeout=5) as client:
                start = time.monotonic()
                client.sendall(b'ping')
                self.assertEqual(receive_exactly(client, 4), b'ping')
                elapsed = time.monotonic() - start
        self.assertGreaterEqual(elapsed, 0.08, 'each direction must add the one-way delay')
        self.assertLess(elapsed, 2.0)

    def test_large_payload_and_half_close_pass_through_unchanged(self):
        payload = os.urandom(1 << 20)
        with RELAY.LatencyRelay(self.upstream_port, 1) as relay:
            with socket.create_connection(('127.0.0.1', relay.port), timeout=10) as client:
                sender = threading.Thread(target=lambda: (client.sendall(payload), client.shutdown(socket.SHUT_WR)))
                sender.start()
                echoed = receive_exactly(client, len(payload))
                sender.join(10)
                self.assertEqual(client.recv(1), b'', 'the upstream close must reach the client as EOF')
            deadline = time.monotonic() + 5
            while relay.snapshot()['connections'] < 1 and time.monotonic() < deadline:
                time.sleep(0.01)
            summary = relay.snapshot()
        self.assertEqual(echoed, payload)
        self.assertEqual(summary, {'connections': 1, 'errors': []})

    def test_refused_upstream_is_recorded_and_closes_the_client(self):
        with socket.socket() as unused:
            unused.bind(('127.0.0.1', 0))
            closed_port = unused.getsockname()[1]
        with RELAY.LatencyRelay(closed_port, 1) as relay:
            with socket.create_connection(('127.0.0.1', relay.port), timeout=5) as client:
                self.assertEqual(client.recv(1), b'')
            deadline = time.monotonic() + 5
            while not relay.snapshot()['errors'] and time.monotonic() < deadline:
                time.sleep(0.01)
            errors = relay.snapshot()['errors']
        self.assertEqual(len(errors), 1)
        self.assertIn('refused the connection', errors[0])

    def test_negative_delay_is_rejected(self):
        with self.assertRaisesRegex(ValueError, 'negative'):
            RELAY.LatencyRelay(self.upstream_port, -1)


class StatementCounterTests(unittest.TestCase):
    """문장 수는 측정 DB의 세션이 끝나고 카운터가 멈춘 뒤에만 읽어야 한다."""

    def setUp(self):
        self.original = BENCH.database_activity

    def tearDown(self):
        BENCH.database_activity = self.original

    def test_waits_for_sessions_to_end_and_the_counter_to_settle(self):
        readings = iter([(1, 10), (0, 12), (0, 13), (0, 13)])
        BENCH.database_activity = lambda *_: next(readings)
        self.assertEqual(BENCH.committed_statements(Path('/pg'), 1, 'user', {}), 13)

    def test_counter_that_never_settles_fails(self):
        counter = iter(range(1_000_000))
        BENCH.database_activity = lambda *_: (0, next(counter))
        original_limit = BENCH.STATEMENT_SETTLE_SECONDS
        BENCH.STATEMENT_SETTLE_SECONDS = 0.5
        try:
            with self.assertRaisesRegex(RuntimeError, 'did not settle'):
                BENCH.committed_statements(Path('/pg'), 1, 'user', {})
        finally:
            BENCH.STATEMENT_SETTLE_SECONDS = original_limit


class BenchmarkInputTests(unittest.TestCase):
    """명령·fixture·요약은 비밀을 싣지 않고, 측정 대상과 warm-up을 섞지 않아야 한다."""

    def test_collector_commands_target_the_given_port_without_passwords(self):
        document = Path('/tmp/doc.json')
        go = BENCH.collector_command({'kind': 'go', 'path': Path('/bin/probe')}, 7001, 'bench', 'json', document, None)
        self.assertEqual(go, ['/bin/probe', '--url', 'postgres://bench@127.0.0.1:7001/sgbench?sslmode=disable',
                              '--format', 'json', '--document-version', '2', '-o', '/tmp/doc.json'])
        jdbc = BENCH.collector_command({'kind': 'jdbc', 'path': Path('/p/probe.jar')}, 7001, 'bench', 'ndjson',
                                       document, Path('/java/bin/java'))
        self.assertEqual(jdbc, ['/java/bin/java', '-jar', '/p/probe.jar', '--url',
                                'jdbc:postgresql://127.0.0.1:7001/sgbench?sslmode=disable', '--user', 'bench',
                                '--format', 'ndjson', '--document-version', '2', '-o', '/tmp/doc.json'])

    def test_indexes_add_one_index_per_table_without_changing_counted_shape(self):
        with tempfile.TemporaryDirectory(prefix='sg-probe-pg-benchmark-test-') as directory:
            plain, indexed = Path(directory) / 'plain.sql', Path(directory) / 'indexed.sql'
            self.assertEqual(BENCH.write_fixture(plain, 2, 3, 8), BENCH.write_fixture(indexed, 2, 3, 8, indexes=True))
            self.assertNotIn('CREATE INDEX', plain.read_text())
            self.assertEqual(indexed.read_text().count('CREATE INDEX'), 6)

    def test_summary_leaves_out_warmups(self):
        def sample(seconds, statements, warmup):
            return {'producer': 'baseline', 'warmup': warmup, 'statements': statements, 'document_sha256': 'a',
                    'probe': {'seconds': seconds, 'peak_rss_bytes': 1}}
        results = [sample(9.0, 99, True), sample(1.0, 52, False), sample(2.0, 52, False), sample(3.0, 52, False)]
        summary = BENCH.summarize(results, [{'label': 'baseline', 'kind': 'go'}])['baseline']
        self.assertEqual((summary['median_seconds'], summary['statements']), (2.0, [52]))

    def test_invalid_arguments_are_usage_errors(self):
        for arguments, message in ((['--latency-ms', '-1'], '--latency-ms'),
                                   (['--latency-ms', 'nan'], '--latency-ms'),
                                   (['--baseline-jar', 'a.jar'], 'together'),
                                   (['--baseline-jar', 'a.jar', '--new-jar', 'b.jar'], '--java')):
            result = subprocess.run([sys.executable, '-B', str(HERE / 'benchmark-probe-postgres.py'), *arguments],
                                    capture_output=True, text=True, check=False)
            self.assertEqual(result.returncode, 2, arguments)
            self.assertIn(message, result.stderr)


if __name__ == '__main__':
    unittest.main()
