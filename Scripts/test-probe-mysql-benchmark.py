#!/usr/bin/env python3
"""MySQL·MariaDB 수집 벤치마크가 문장 수를 바르게 세고 비밀번호를 명령줄에 싣지 않는지 확인한다(Docker 없이)."""
import importlib.util
from pathlib import Path
import re
import subprocess
import sys
import tempfile
import unittest

HERE = Path(__file__).resolve().parent
SPEC = importlib.util.spec_from_file_location('probe_mysql_benchmark', HERE / 'benchmark-probe-mysql.py')
BENCH = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(BENCH)


class FixtureTests(unittest.TestCase):
    """fixture는 문서 모양 검사가 기대하는 PK·FK 수와 같은 DDL을 써야 한다."""

    def test_fixture_matches_the_expected_document_shape(self):
        with tempfile.TemporaryDirectory(prefix='sg-probe-mysql-benchmark-test-') as directory:
            path = Path(directory) / 'fixture.sql'
            shape = BENCH.write_fixture(path, 3, 8, 2)
            text = path.read_text()
        self.assertEqual(shape, dict(schemas=1, objects=3, routines=2, columns=24, constraints=5))
        self.assertEqual(text.count('CREATE TABLE'), 3)
        self.assertEqual(text.count('PRIMARY KEY'), 3)
        self.assertEqual(re.findall(r'CONSTRAINT (\w+) FOREIGN KEY', text), ['t00001_fk', 't00002_fk'])
        self.assertEqual(re.findall(r'\bKEY (\w+) \(parent_id\)', text),
                         ['t00000_parent', 't00001_parent', 't00002_parent'])
        self.assertEqual(text.count('CREATE PROCEDURE'), 2)

    def test_one_table_has_no_foreign_key(self):
        with tempfile.TemporaryDirectory(prefix='sg-probe-mysql-benchmark-test-') as directory:
            shape = BENCH.write_fixture(Path(directory) / 'fixture.sql', 1, 2, 0)
        self.assertEqual(shape['constraints'], 1)


class StatusTests(unittest.TestCase):
    """문장 수는 상태 변수 증가분에서 상태 읽기 자신의 몫을 뺀 값이다."""

    def test_status_lines_are_parsed(self):
        self.assertEqual(BENCH.parse_status('Com_stmt_prepare\t7\nQuestions\t120\n'),
                         {'Questions': 120, 'Com_stmt_prepare': 7})

    def test_missing_status_variable_fails(self):
        with self.assertRaisesRegex(RuntimeError, 'Com_stmt_prepare'):
            BENCH.parse_status('Questions\t1\n')

    def test_delta_removes_the_status_read_overhead(self):
        before = {'Questions': 100, 'Com_stmt_prepare': 10}
        after = {'Questions': 153, 'Com_stmt_prepare': 30}
        overhead = {'Questions': 1, 'Com_stmt_prepare': 0}
        self.assertEqual(BENCH.status_delta(before, after, overhead), {'Questions': 52, 'Com_stmt_prepare': 20})


class SecretTests(unittest.TestCase):
    """비밀번호는 docker CLI·probe의 환경으로만 넘어가고 명령줄에는 없어야 한다."""

    def test_commands_name_the_password_variables_without_values(self):
        probe = BENCH.collector_command(Path('/bin/probe'), 'json', Path('/tmp/doc.json'))
        self.assertEqual(probe, ['/bin/probe', '--url-env', 'SG_BENCH_MYSQL_URL', '--format', 'json',
                                 '--document-version', '2', '-o', '/tmp/doc.json'])
        client = BENCH.client_command('box', 'mysql', '-e', 'SELECT 1')
        self.assertEqual(client, ['docker', 'exec', '-e', 'MYSQL_PWD', 'box', 'mysql', '--no-defaults', '-uroot',
                                  '-e', 'SELECT 1'])
        self.assertEqual(BENCH.client_command('box', 'mariadb', 'sgfix', interactive=True)[:4],
                         ['docker', 'exec', '-i', '-e'])

    def test_password_reaches_only_the_environment(self):
        env = BENCH.password_environment('s3cret')
        self.assertEqual((env['MYSQL_ROOT_PASSWORD'], env['MARIADB_ROOT_PASSWORD'], env['MYSQL_PWD']),
                         ('s3cret', 's3cret', 's3cret'))


class ArgumentTests(unittest.TestCase):
    """범위를 벗어난 인자는 Docker를 부르기 전에 사용 오류(종료 2)로 끝나야 한다."""

    def test_invalid_arguments_are_usage_errors(self):
        base = ['--server', 'mysql', '--baseline-probe', 'a', '--new-probe', 'b', '--output', 'out']
        for extra, message in ((['--latency-ms', '-1'], '--latency-ms'), (['--latency-ms', 'inf'], '--latency-ms'),
                               (['--repeat', '2'], 'repeat'), (['--tables', '0'], 'tables'),
                               (['--startup-timeout', '0'], '--startup-timeout')):
            result = subprocess.run([sys.executable, '-B', str(HERE / 'benchmark-probe-mysql.py'), *base, *extra],
                                    capture_output=True, text=True, check=False)
            self.assertEqual(result.returncode, 2, extra)
            self.assertIn(message, result.stderr)


if __name__ == '__main__':
    unittest.main()
