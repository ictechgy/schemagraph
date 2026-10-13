#!/usr/bin/env python3
"""PG 진단의 실패가 JDBC 결과·정리·외부 서버 접속 계약을 바꾸지 않는지 검증한다."""
import os
from pathlib import Path
import subprocess
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[1]
FIXTURES = (ROOT / 'Scripts/verify-fixtures.sh').read_text()
START = FIXTURES.index('        if [[ "$pg_url" =~')
END = FIXTURES.index('            "$BIN" scan --document "$tmp/probe-pg-doc.json"', START)
JDBC_BLOCK = FIXTURES[START:END] + '\nfi\n'
HELPER = ROOT / 'Scripts/postgres-fixture-diagnostics.sh'


class PostgresDiagnostics(unittest.TestCase):
    """운영 JDBC 호출 구문을 실행하되 DB·Java 경계만 대체한다."""

    def shell(self, *, jdbc=2, ready=0, pg_status=0, owned=True,
              missing_log=False, blocked_directory=False):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            work, binaries, artifacts = (root / name for name in ('work', 'bin', 'artifacts'))
            work.mkdir()
            binaries.mkdir()
            if blocked_directory:
                artifacts.write_text('blocked')
            else:
                artifacts.mkdir()
            (work / 'pgdata').mkdir()
            original_log = b'old fixture data\n' * 8192 + b'last PostgreSQL diagnostic\n'
            if not missing_log:
                (work / 'pg.log').write_bytes(original_log)
            for name, body in {
                'pg_isready': 'echo "readiness $*" >> "$CALLS"; echo mock-readiness; exit "$READY"',
                'pg_ctl': 'echo "pg_ctl $*" >> "$CALLS"; echo mock-status; exit "$PG_STATUS"',
            }.items():
                binary = binaries / name
                binary.write_text('#!/usr/bin/env bash\n' + body + '\n')
                binary.chmod(0o700)
            env = dict(os.environ, TEST_ROOT=str(root), CALLS=str(root / 'calls'),
                       JDBC=str(jdbc), READY=str(ready), PG_STATUS=str(pg_status),
                       OWNED='1' if owned else '')
            prelude = r'''
set -euo pipefail
source "$1"
tmp="$TEST_ROOT/work"
PGBIN="$TEST_ROOT/bin"
SG_FIXTURE_ARTIFACTS="$TEST_ROOT/artifacts"
pg_data="$tmp/pgdata"
PGHOST_TMP="$tmp"
pg_own="$OWNED"
pg_port=55476
pg_url='postgres://postgres@localhost:55476/sgfix'
if [ -z "$pg_own" ]; then
    pg_url='postgres://external:secret@example.invalid:5433/private'
fi
trap 'echo cleanup >> "$CALLS"; rm -rf "$tmp"' EXIT
mock_java() {
    printf 'java' >> "$CALLS"
    printf ' <%s>' "$@" >> "$CALLS"
    printf '\n' >> "$CALLS"
    printf 'jdbc stdout\n'
    printf 'jdbc stderr\n' >&2
    return "$JDBC"
}
JAVABIN=mock_java
JAR=fixture.jar
'''
            result = subprocess.run(['bash', '-c', prelude + JDBC_BLOCK, 'fixture-test', str(HELPER)],
                                    env=env, cwd=ROOT, text=True, capture_output=True, timeout=5)
            files = ({path.name: path.read_bytes() for path in artifacts.iterdir()}
                     if artifacts.is_dir() else {})
            return result, (root / 'calls').read_text().splitlines(), files, original_log

    def test_failure_preserves_exit_and_evidence_before_cleanup(self):
        result, calls, files, original = self.shell()
        self.assertEqual(result.returncode, 2, result.stderr)
        self.assertEqual(result.stdout, 'jdbc stdout\n')
        self.assertEqual(result.stderr, 'jdbc stderr\n')
        self.assertEqual(calls[0], 'readiness -h localhost -p 55476 -U postgres -d sgfix -t 2')
        self.assertIn('<jdbc:postgresql://localhost:55476/sgfix>', calls[1])
        self.assertEqual(sum(call.startswith('java ') for call in calls), 1)
        self.assertTrue(calls[2].startswith('pg_ctl -D '))
        self.assertTrue(calls[2].endswith(' status'))
        self.assertIn(' -h 127.0.0.1 -p 55476 ', calls[4])
        self.assertIn(' -h ::1 -p 55476 ', calls[5])
        self.assertEqual(calls[-1], 'cleanup')
        self.assertFalse(any(' stop' in call or ' start' in call for call in calls))
        self.assertEqual(files['postgres-server.log'], original[-65536:])
        self.assertIn(b'JDBC exit: 2', files['postgres-jdbc-status.log'])

    def test_precheck_failure_does_not_gate_successful_collection(self):
        result, calls, files, _ = self.shell(jdbc=0, ready=2)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(len(calls), 3)
        self.assertTrue(calls[1].startswith('java '))
        self.assertEqual(calls[2], 'cleanup')
        self.assertIn(b'pg_isready exit: 2', files['postgres-jdbc-status.log'])
        self.assertNotIn('postgres-server.log', files)

    def test_failed_status_tools_preserve_jdbc_exit(self):
        result, calls, files, _ = self.shell(jdbc=7, ready=127, pg_status=127)
        self.assertEqual(result.returncode, 7, result.stderr)
        self.assertEqual(calls[-1], 'cleanup')
        self.assertIn('postgres-jdbc-status.log', files)
        self.assertIn(b'pg_ctl status exit: 127', files['postgres-jdbc-status.log'])
        self.assertEqual(files['postgres-jdbc-status.log'].count(b'pg_isready exit: 127'), 4)

    def test_missing_server_log_preserves_jdbc_exit(self):
        result, calls, files, _ = self.shell(jdbc=9, missing_log=True)
        self.assertEqual(result.returncode, 9)
        self.assertIn('could not preserve all owned PostgreSQL diagnostics', result.stderr)
        self.assertEqual(calls[-1], 'cleanup')
        self.assertIn(b'JDBC exit: 9', files['postgres-jdbc-status.log'])

    def test_unwritable_artifact_target_does_not_change_collection(self):
        for jdbc in (0, 2):
            with self.subTest(jdbc=jdbc):
                result, calls, files, _ = self.shell(jdbc=jdbc, blocked_directory=True)
                self.assertEqual(result.returncode, jdbc, result.stderr)
                self.assertTrue(calls[0].startswith('java '))
                self.assertEqual(calls[1:], ['cleanup'])
                self.assertFalse(files)

    def test_external_server_bypasses_all_diagnostics(self):
        for jdbc in (0, 2):
            with self.subTest(jdbc=jdbc):
                result, calls, files, _ = self.shell(jdbc=jdbc, owned=False)
                self.assertEqual(result.returncode, jdbc, result.stderr)
                self.assertIn('<jdbc:postgresql://example.invalid:5433/private>', calls[0])
                self.assertIn('<--password> <secret>', calls[0])
                self.assertEqual(calls[1:], ['cleanup'])
                self.assertFalse(files)


if __name__ == '__main__':
    unittest.main()
