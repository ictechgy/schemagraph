#!/usr/bin/env python3
"""소켓 초기화 서버와 최종 TCP 서버를 구분하고 실패 때 적용을 막는지 검증한다."""
import importlib.util
from pathlib import Path
import subprocess
import unittest
from unittest.mock import patch

SPEC = importlib.util.spec_from_file_location('mysql_ready', Path(__file__).with_name('wait-mysql-ready.py'))
READY = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(READY)


class Startup:
    """외부 Docker 경계에서 초기화·DB 생성·종료와 지연을 결정적으로 재현한다."""

    def __init__(self, *, tcp_at=2, database_at=4, stops_at=float('inf'), failure=None):
        self.now = 0
        self.tcp_at = tcp_at
        self.database_at = database_at
        self.stops_at = stops_at
        self.failure = failure

    def sleep(self, seconds):
        """실제 수 분을 기다리지 않고 같은 기동 시간표를 검증한다."""
        self.now += seconds

    def run(self, command, *, timeout, **kwargs):
        """소켓은 먼저 열리고 TCP 연결 후에도 DB가 없을 수 있는 경계를 재현한다."""
        if timeout <= 0:
            raise AssertionError('Docker calls must have a positive timeout')
        inspect = command[1] == 'inspect'
        if self.failure == ('inspect-hang' if inspect else 'query-hang'):
            self.now += timeout
            raise subprocess.TimeoutExpired(command, timeout)
        if inspect:
            return subprocess.CompletedProcess(command, int(self.failure == 'inspect-error'),
                                               'true' if self.now < self.stops_at else 'false')
        # 기본 소켓과 admin ping은 초기화 서버에서도 성공하므로 준비 근거가 아니다.
        if '--protocol=TCP' not in command:
            return subprocess.CompletedProcess(command, 0)
        available = self.now >= self.tcp_at
        if '--database=sgfix' in command:
            available = available and self.now >= self.database_at
        return subprocess.CompletedProcess(command, 0 if available else 1)


class ReadinessTests(unittest.TestCase):
    """재시도 성공과 기동 실패를 같은 관측 경계에서 대조한다."""

    def wait(self, startup, client='mysql', timeout=10):
        """운영 코드의 Docker·시간 경계만 격리해 실제 대기 분기를 실행한다."""
        with patch.object(READY.subprocess, 'run', side_effect=startup.run), \
                patch.object(READY.time, 'monotonic', side_effect=lambda: startup.now), \
                patch.object(READY.time, 'sleep', side_effect=startup.sleep):
            READY.wait_for_mysql('fixture-test', client, timeout)

    def test_waits_past_temporary_server_and_database_creation(self):
        """두 DB 모두 초기화 소켓과 TCP listener만으로 성공하지 않아야 한다."""
        for client in ('mysql', 'mariadb'):
            with self.subTest(client=client):
                startup = Startup()
                self.wait(startup, client)
                self.assertGreaterEqual(startup.now, startup.database_at)

    def test_ready_server_returns_immediately(self):
        """준비된 서버에는 불필요한 고정 기동 지연을 추가하지 않는다."""
        startup = Startup(tcp_at=0, database_at=0)
        self.wait(startup)
        self.assertEqual(startup.now, 0)

    def test_timeout_is_failure(self):
        """대기 만료가 fixture 적용으로 이어지는 기존 결함을 잡는다."""
        startup = Startup(database_at=float('inf'))
        with self.assertRaisesRegex(RuntimeError, 'Timed out'):
            self.wait(startup, timeout=3)
        self.assertEqual(startup.now, 3)

    def test_container_exit_fails_before_deadline(self):
        """이미 종료된 컨테이너를 전체 기한까지 기다리지 않는다."""
        startup = Startup(stops_at=1)
        with self.assertRaisesRegex(RuntimeError, 'stopped'):
            self.wait(startup)
        self.assertLess(startup.now, 10)

    def test_inspection_failure_is_reported(self):
        """Docker 장애를 SQL 서버의 준비 지연으로 오인하지 않는다."""
        with self.assertRaisesRegex(RuntimeError, 'Cannot inspect'):
            self.wait(Startup(failure='inspect-error'))

    def test_hung_query_respects_overall_deadline(self):
        """Docker exec가 응답하지 않아도 총 대기 시간을 늘리지 않는다."""
        startup = Startup(failure='query-hang')
        with self.assertRaisesRegex(RuntimeError, 'Timed out'):
            self.wait(startup, timeout=3)
        self.assertEqual(startup.now, 3)

    def test_hung_inspection_is_bounded(self):
        """Docker 데몬 무응답도 명시적인 기동 실패로 끝낸다."""
        startup = Startup(failure='inspect-hang')
        with self.assertRaisesRegex(RuntimeError, 'inspection timed out'):
            self.wait(startup, timeout=3)
        self.assertEqual(startup.now, 3)


if __name__ == '__main__':
    unittest.main()
