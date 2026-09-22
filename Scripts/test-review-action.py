#!/usr/bin/env python3
"""Action 경계의 입력 격리·실패 전달·산출물 일관성을 실제 프로세스로 검사한다."""
import json
import os
from pathlib import Path
import signal
import subprocess
import sys
import tempfile
import time
import unittest

RUNNER = Path(__file__).with_name('run-review-action.py')


class ReviewActionTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix='sg-action-test-')
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name).resolve()
        self.workspace = self.root / 'workspace'
        self.workspace.mkdir()
        self.runner_temp = self.root / 'runner'
        self.runner_temp.mkdir()
        self.before = self.workspace / 'before;literal.json'
        self.after = self.workspace / 'after snapshot.json'
        self.before.write_text('{}')
        self.after.write_text('{}')
        self.engine = self.root / 'fake engine'
        self.engine.write_text(f'''#!{sys.executable}
import json, os, pathlib, sys, time
args = sys.argv[1:]
assert args[0] == 'review'
with open(os.environ['SG_TEST_CALLS'], 'a') as out: out.write(json.dumps(args)+'\\n')
if os.environ.get('SG_TEST_PID'): pathlib.Path(os.environ['SG_TEST_PID']).write_text(str(os.getpid()))
mode = args[args.index('--format')+1]
if os.environ.get('SG_TEST_SLEEP'): time.sleep(3)
if os.environ.get('SG_TEST_CHANGE'): pathlib.Path(os.environ['SG_REVIEW_AFTER']).write_text('{{"changed":true}}')
if mode == 'markdown': print('# Schema change review\\n')
elif mode == 'sarif': print(json.dumps({{'version':'2.1.0','runs':[{{'tool':{{'driver':{{'name':'schemagraph'}}}},'results':[]}}]}}))
else: print(json.dumps({{'kind':'review','changes':[],'totalChanges':0}}))
raise SystemExit(int(os.environ.get('SG_TEST_EXIT', '0')))
''')
        self.engine.chmod(0o755)
        self.env = {**os.environ, 'GITHUB_WORKSPACE': str(self.workspace),
                    'RUNNER_TEMP': str(self.runner_temp), 'GITHUB_OUTPUT': str(self.root/'outputs'),
                    'GITHUB_STEP_SUMMARY': str(self.root/'summary'),
                    'SG_REVIEW_ENGINE': str(self.engine), 'SG_REVIEW_BEFORE': str(self.before),
                    'SG_REVIEW_AFTER': str(self.after), 'SG_REVIEW_TIMEOUT': '10',
                    'SG_REVIEW_COMPLETE': 'true', 'SG_TEST_CALLS': str(self.root/'calls')}
        for key in ('SG_REVIEW_POLICY', 'SG_REVIEW_BASELINE', 'SG_REVIEW_AS_OF', 'SG_REVIEW_OUTPUT',
                    'SG_TEST_EXIT', 'SG_TEST_SLEEP', 'SG_TEST_CHANGE'):
            self.env.pop(key, None)

    def run_action(self, **values):
        return subprocess.run([sys.executable, str(RUNNER)], env={**self.env, **values},
                              capture_output=True, text=True, timeout=20)

    def outputs(self):
        return dict(line.split('=', 1) for line in (self.root/'outputs').read_text().splitlines())

    def test_reports_and_strict_decision_are_preserved_with_literal_paths(self):
        result = self.run_action(SG_TEST_EXIT='1')
        self.assertEqual(result.returncode, 1, result.stderr)
        outputs = self.outputs()
        self.assertEqual(outputs['exit_code'], '1')
        self.assertEqual(json.loads(Path(outputs['json']).read_text())['kind'], 'review')
        self.assertEqual(json.loads(Path(outputs['sarif']).read_text())['version'], '2.1.0')
        calls = [json.loads(line) for line in (self.root/'calls').read_text().splitlines()]
        self.assertEqual(len(calls), 3)
        for call in calls:
            self.assertIn(str(self.before), call)
            self.assertIn('--strict', call)
            self.assertIn('--require-complete', call)
        self.assertTrue((self.root/'summary').read_text().startswith('# Schema change review'))

    def test_success_keeps_exit_zero(self):
        result = self.run_action()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.outputs()['exit_code'], '0')

    def test_incomplete_collection_is_not_a_success(self):
        result = self.run_action(SG_TEST_EXIT='2')
        self.assertEqual(result.returncode, 2)
        self.assertEqual(self.outputs()['exit_code'], '2')

    def test_policy_arguments_are_not_shell_commands(self):
        policy = self.workspace / 'policy $(touch injected).toml'
        policy.write_text('version = 1')
        baseline = self.workspace/'baseline.json'
        baseline.write_text('{}')
        result = self.run_action(SG_REVIEW_POLICY=str(policy), SG_REVIEW_BASELINE=str(baseline),
                                 SG_REVIEW_AS_OF='2026-09-22')
        self.assertEqual(result.returncode, 0, result.stderr)
        for line in (self.root/'calls').read_text().splitlines():
            args = json.loads(line)
            self.assertEqual(args[args.index('--policy')+1], str(policy))
            self.assertEqual(args[args.index('--baseline')+1], str(baseline))
        self.assertFalse((self.workspace/'injected').exists())

    def test_workspace_escape_is_rejected_before_running_engine(self):
        outside = self.root/'outside.json'
        outside.write_text('{}')
        result = self.run_action(SG_REVIEW_BEFORE=str(outside))
        self.assertEqual(result.returncode, 2)
        self.assertFalse((self.root/'calls').exists())

    def test_symlink_to_outside_workspace_is_rejected(self):
        outside = self.root/'outside.json'
        outside.write_text('{}')
        link = self.workspace/'linked.json'
        link.symlink_to(outside)
        result = self.run_action(SG_REVIEW_BEFORE=str(link))
        self.assertEqual(result.returncode, 2)
        self.assertFalse((self.root/'calls').exists())

    def test_existing_output_is_not_overwritten(self):
        output = self.runner_temp/'old-report'
        output.mkdir()
        marker = output/'review.json'
        marker.write_text('keep this')
        result = self.run_action(SG_REVIEW_OUTPUT=str(output))
        self.assertEqual(result.returncode, 2)
        self.assertEqual(marker.read_text(), 'keep this')

    def test_changing_snapshots_cannot_mix_reports(self):
        result = self.run_action(SG_TEST_CHANGE='1')
        self.assertEqual(result.returncode, 2)
        self.assertIn('changed during review', result.stderr)

    def test_timeout_fails_the_action(self):
        result = self.run_action(SG_TEST_SLEEP='1', SG_REVIEW_TIMEOUT='1')
        self.assertEqual(result.returncode, 2)
        self.assertIn('timed out', result.stderr)

    @unittest.skipUnless(os.name == 'posix', 'process-group cancellation is POSIX-specific')
    def test_cancel_stops_the_owned_review_process(self):
        self.check_cancellation(signal.SIGINT)

    @unittest.skipUnless(os.name == 'posix', 'process-group cancellation is POSIX-specific')
    def test_sigterm_stops_the_owned_review_process(self):
        self.check_cancellation(signal.SIGTERM)

    def check_cancellation(self, requested_signal):
        pid_file = self.root/'engine.pid'
        process = subprocess.Popen([sys.executable, str(RUNNER)],
                                   env={**self.env, 'SG_TEST_SLEEP': '1', 'SG_TEST_PID': str(pid_file)},
                                   stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
        deadline = time.monotonic()+5
        while not pid_file.exists() and time.monotonic()<deadline:
            time.sleep(0.02)
        self.assertTrue(pid_file.exists())
        pid = int(pid_file.read_text())
        process.send_signal(requested_signal)
        _stdout, stderr = process.communicate(timeout=5)
        self.assertEqual(process.returncode, 130, stderr)
        with self.assertRaises(ProcessLookupError):
            os.kill(pid, 0)


if __name__ == '__main__':
    unittest.main()
