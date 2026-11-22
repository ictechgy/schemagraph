#!/usr/bin/env python3
"""취소 회귀 검사가 실패하면 어느 검사·어느 위치에서 실패했는지 알리는지 검증한다.

macOS 릴리스 job에서 `[Errno 32] Broken pipe`만 남고 검사 이름이 없어 원인을 좁히지 못한
적이 있다. 실제 엔진을 띄우지 않고 검사 목록만 바꿔 보고 형식을 고정한다.
"""
import contextlib
import importlib.util
import io
import json
import os
from pathlib import Path
import sys
import tempfile
import unittest
from unittest.mock import patch

SPEC = importlib.util.spec_from_file_location(
    'verify_cancellation', Path(__file__).with_name('verify-cancellation.py'))
CANCELLATION = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(CANCELLATION)


def broken_pipe_check(engine, work):
    """실제 검사처럼 자식 프로세스 stdin 쓰기에서 EPIPE가 난 상황을 흉내 낸다."""
    raise BrokenPipeError(32, 'Broken pipe')


def failing_assertion_check(engine, work):
    """검사가 `fail`로 알린 실패는 `fail`이 아니라 부른 쪽의 위치를 적어야 한다."""
    CANCELLATION.fail('engine exited 3, expected 0: boom')


def passing_check(engine, work):
    """앞선 검사가 통과해도 뒤 검사의 실패가 그 검사 이름으로 보고되는지 본다."""


def library_error_check(engine, work):
    """표준 라이브러리 안에서 난 오류는 그 라이브러리가 아니라 이 호출 줄을 가리켜야 한다."""
    json.loads('{')


class FailingCleanup:
    """임시 디렉터리 정리가 OSError로 실패하는 상황을 흉내 낸다."""

    def __init__(self, prefix):
        self.directory = tempfile.mkdtemp(prefix=prefix)

    def __enter__(self):
        return self.directory

    def __exit__(self, *exc_info):
        os.rmdir(self.directory)
        raise OSError(39, 'Directory not empty')


class FailureNames(unittest.TestCase):
    """main의 종료 코드와 stderr 한 줄을 검사 목록만 바꿔 확인한다."""

    def run_main(self, checks):
        """실행 가능한 파일이면 되는 엔진 인자로 Python 자신을 넘기고 stderr를 모은다."""
        stderr = io.StringIO()
        argv = ['verify-cancellation.py', '--engine', sys.executable]
        with patch.object(CANCELLATION, 'CHECKS', checks), patch.object(sys, 'argv', argv), \
                contextlib.redirect_stderr(stderr):
            code = CANCELLATION.main()
        return code, stderr.getvalue()

    def test_broken_pipe_names_the_check_and_the_raising_function(self):
        code, stderr = self.run_main((passing_check, broken_pipe_check))
        self.assertEqual(code, 1)
        self.assertIn('failed in broken_pipe_check (broken_pipe_check line ', stderr)
        self.assertIn('[Errno 32] Broken pipe', stderr)
        self.assertNotIn('passing_check', stderr)

    def test_fail_reports_its_caller_not_fail_itself(self):
        code, stderr = self.run_main((failing_assertion_check,))
        self.assertEqual(code, 1)
        self.assertIn('failed in failing_assertion_check (failing_assertion_check line ', stderr)
        self.assertIn('engine exited 3, expected 0: boom', stderr)

    def test_library_errors_point_at_the_calling_line_of_this_script(self):
        code, stderr = self.run_main((library_error_check,))
        self.assertEqual(code, 1)
        self.assertIn('failed in library_error_check (library_error_check line ', stderr)
        self.assertNotIn('decode', stderr.split('):', 1)[0])

    def test_cleanup_failure_keeps_the_failed_check(self):
        with patch.object(CANCELLATION.tempfile, 'TemporaryDirectory', FailingCleanup):
            code, stderr = self.run_main((broken_pipe_check,))
            self.assertEqual(code, 1)
            self.assertIn('failed in broken_pipe_check (', stderr)
            self.assertIn('; temporary directory cleanup also failed: [Errno 39] Directory not empty', stderr)
            code, stderr = self.run_main((passing_check,))
            self.assertEqual(code, 1)
            self.assertIn('failed outside the checks: [Errno 39] Directory not empty', stderr)

    def test_all_checks_passing_reports_ok(self):
        stdout = io.StringIO()
        with contextlib.redirect_stdout(stdout):
            code, stderr = self.run_main((passing_check,))
        self.assertEqual((code, stderr), (0, ''))
        self.assertEqual(stdout.getvalue(), 'cancellation process regression: ok\n')

    def test_default_check_list_keeps_every_process_check(self):
        names = [check.__name__ for check in CANCELLATION.CHECKS]
        self.assertEqual(names, [
            'check_cli_queries',
            'check_inherited_sigint_ignore',
            'check_review',
            'check_second_interrupt',
            'check_closed_stderr',
            'check_blocked_stderr',
            'check_mcp',
        ])


if __name__ == '__main__':
    unittest.main()
