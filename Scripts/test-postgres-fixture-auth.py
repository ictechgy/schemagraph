#!/usr/bin/env python3
"""소유한 임시 PostgreSQL이 같은 호스트의 다른 계정에 열리지 않는지 가짜 PG 도구로 검사한다.

loopback TCP는 실행마다 만든 비밀번호(scram)로만 받아야 하고, trust로 둔 Unix socket은
공용 /tmp가 아닌 0700 디렉터리에 있어야 한다. 서버는 띄우지 않고 각 하네스의 기동 코드를
그대로 실행한다.
"""
import importlib.util
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest import mock

ROOT = Path(__file__).resolve().parents[1]
FIXTURES = (ROOT / 'Scripts/verify-fixtures.sh').read_text()
# 가짜 PG 도구다. 호출 시점의 argv와 비밀번호 파일·socket 디렉터리 상태를 기록하고 성공한다.
# 하네스가 끝나며 임시 파일을 지워도 판정할 수 있게 내용과 권한을 그때 읽어 둔다.
STUB = r'''#!{python}
import json, os, shlex, sys
def inspect(path):
    if not path or not os.path.exists(path):
        return None
    content = open(path).read() if os.path.isfile(path) else None
    return {{'mode': os.stat(path).st_mode & 0o777, 'content': content}}
args = sys.argv[1:]
record = {{'tool': os.path.basename(sys.argv[0]), 'args': args,
          'pgpass': inspect(os.environ.get('PGPASSFILE')), 'pgpassword': os.environ.get('PGPASSWORD')}}
for index, arg in enumerate(args):
    if arg.startswith('--pwfile='):
        record['pwfile'] = inspect(arg.split('=', 1)[1])
    elif arg in ('-f', '-qf'):
        record['script'] = inspect(args[index + 1])
    elif arg == '-o':
        options = shlex.split(args[index + 1])
        if '-k' in options:
            record['socket_dir'] = options[options.index('-k') + 1]
            record['socket'] = inspect(record['socket_dir'])
with open(os.environ['SG_PG_CALLS'], 'a') as out:
    out.write(json.dumps(record) + '\n')
'''


def load(name):
    """main 가드가 있는 하네스를 실행하지 않고 함수만 불러온다."""
    spec = importlib.util.spec_from_file_location(name.replace('-', '_'), ROOT / 'Scripts' / f'{name}.py')
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


class OwnedPostgresAuthentication(unittest.TestCase):
    """여섯 하네스의 cluster 기동이 같은 비공개 인증 계약을 지키는지 본다."""

    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix='sg-pg-auth-test-')
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name).resolve()
        self.bin = self.root / 'bin'
        self.bin.mkdir()
        for tool in ('initdb', 'pg_ctl', 'psql', 'createdb'):
            (self.bin / tool).write_text(STUB.format(python=sys.executable))
            (self.bin / tool).chmod(0o755)
        self.calls = self.root / 'calls'
        # 개발자 셸의 PGPASSWORD가 새 비밀번호를 가리면 TCP 인증이 깨지므로 일부러 넣어 둔다.
        patcher = mock.patch.dict(os.environ, {'SG_PG_CALLS': str(self.calls), 'PGBIN': str(self.bin),
                                               'PGPASSWORD': 'PLACEHOLDER'})
        patcher.start()
        self.addCleanup(patcher.stop)
        self.work = self.root / 'work'
        self.work.mkdir(mode=0o700)

    def records(self, tool):
        return [record for record in map(json.loads, self.calls.read_text().splitlines())
                if record['tool'] == tool]

    def assert_private_cluster(self):
        """initdb는 host trust 없이 0600 비밀번호 파일을 받고 socket은 끄거나 비공개 디렉터리에 둬야 한다."""
        [initdb] = self.records('initdb')
        args = initdb['args']
        self.assertIn('--auth-host=scram-sha-256', args)
        self.assertNotIn('--auth-host=trust', args)
        self.assertFalse(any(flag in ('-A', '--auth') and value == 'trust' for flag, value in zip(args, args[1:])),
                         args)
        self.assertFalse(any(arg in ('-Atrust', '--auth=trust') for arg in args), args)
        self.assertIsNotNone(initdb.get('pwfile'), args)
        self.assertEqual(initdb['pwfile']['mode'], 0o600)
        password = initdb['pwfile']['content'].strip()
        self.assertGreaterEqual(len(password), 32)
        [start] = [record for record in self.records('pg_ctl') if 'start' in record['args']]
        # -k가 없으면 기본 socket 디렉터리(/tmp)를 쓴다. 빈 -k('')는 Unix socket을 아예 열지 않는다.
        self.assertIn('socket_dir', start, start['args'])
        if start['socket_dir']:
            self.assertNotEqual(Path(start['socket_dir']).resolve(), Path('/tmp').resolve())
            self.assertEqual(start['socket']['mode'], 0o700, start['socket_dir'])
        return password, start['args'][start['args'].index('-o') + 1].split()[1]

    def assert_password_file(self, record, password, port):
        """TCP 클라이언트는 이 cluster 포트의 비밀번호만 담은 0600 PGPASSFILE을 받아야 한다."""
        self.assertIsNotNone(record, 'TCP client has no PGPASSFILE')
        self.assertEqual(record['mode'], 0o600)
        self.assertEqual(record['content'], f'*:{port}:*:*:{password}\n')

    def assert_tcp_psql_has_password(self, password, port):
        clients = [record for record in self.records('psql') if '127.0.0.1' in record['args']]
        self.assertTrue(clients)
        for record in clients:
            self.assert_password_file(record['pgpass'], password, port)
            self.assertIsNone(record['pgpassword'])

    def assert_client_environment(self, environment, password, port):
        self.assertNotIn('PGPASSWORD', environment)
        path = Path(environment['PGPASSFILE'])
        self.assert_password_file({'mode': path.stat().st_mode & 0o777, 'content': path.read_text()},
                                  password, port)

    def test_accuracy_cluster(self):
        accuracy = load('verify-accuracy')
        with accuracy.postgres(self.work, self.bin) as (url, psql, environment):
            password, port = self.assert_private_cluster()
            self.assertEqual(url, f'postgres://corpus@127.0.0.1:{port}/corpus')
            self.assert_client_environment(environment, password, port)
            # 소비자가 만드는 LOGIN 역할도 같은 비밀번호로 TCP 인증을 통과해야 한다.
            accuracy.create_login_role(psql, 'collector', 'NOSUPERUSER', environment)
            self.assert_tcp_psql_has_password(password, port)
        scripts = {record['script']['content']: record['script']['mode']
                   for record in self.records('psql') if record.get('script')}
        self.assertEqual(scripts, {
            f"CREATE ROLE corpus LOGIN NOSUPERUSER NOCREATEDB NOCREATEROLE PASSWORD '{password}';\n": 0o600,
            f"CREATE ROLE collector LOGIN NOSUPERUSER PASSWORD '{password}';\n": 0o600})
        self.assertEqual(list(self.work.glob('*.sql')), [])
        # 역할 비밀번호는 argv에 싣지 않는다(다른 계정이 프로세스 목록으로 읽을 수 있다).
        for record in self.records('psql'):
            self.assertNotIn(password, ' '.join(record['args']))

    def test_dml_accuracy_cluster(self):
        load('verify-dml-accuracy').validate_postgres({'cases': []}, None, self.bin)
        password, port = self.assert_private_cluster()
        self.assert_tcp_psql_has_password(password, port)

    def test_dml_producer_cluster(self):
        producers = load('verify-dml-producers')
        with producers.PostgresRunner(self.bin, self.work) as runner:
            password, port = self.assert_private_cluster()
            self.assert_client_environment(runner.env, password, port)
        self.assert_tcp_psql_has_password(password, port)

    def test_catalog_dependency_cluster(self):
        catalog = load('verify-catalog-dependencies')
        started = catalog.start_postgres(self.work)
        password, port = self.assert_private_cluster()
        _process, url, environment, sockets = started
        self.addCleanup(sockets.cleanup)
        self.assertTrue(url.endswith(f'@127.0.0.1:{port}/postgres'), url)
        self.assert_client_environment(environment, password, port)

    def test_probe_benchmark_cluster(self):
        benchmark = load('benchmark-probe-postgres')
        started = benchmark.start_postgres(self.bin, self.root)
        self.addCleanup(benchmark.stop_postgres, self.bin, started[0])
        password, port = self.assert_private_cluster()
        _cluster, reserved, _user, returned_password, environment = started
        self.assertEqual(str(reserved), port)
        self.assertEqual(returned_password, password)
        self.assert_client_environment(environment, password, port)
        # 측정 DB를 만드는 psql도 loopback TCP로 붙으므로 같은 비밀번호 파일만 받아야 한다.
        self.assert_tcp_psql_has_password(password, port)
        # --latency-ms의 relay 포트는 그 포트 줄만 더하고, 다른 포트에는 비밀번호를 주지 않는다.
        relay_port = int(port) + 1
        benchmark.allow_relay_port(environment['PGPASSFILE'], relay_port, password)
        passfile = Path(environment['PGPASSFILE'])
        self.assertEqual(passfile.stat().st_mode & 0o777, 0o600)
        self.assertEqual(passfile.read_text(), f'*:{port}:*:*:{password}\n*:{relay_port}:*:*:{password}\n')

    def test_fixture_sweep_cluster(self):
        start = 'if [ -z "$pg_url" ] && [ -n "$PGBIN" ] && [ -x "$PGBIN/initdb" ]; then\n'
        block = FIXTURES.split(start, 1)[1].split('\nfi\n', 1)[0]
        script = ('set -euo pipefail\ntrap() { :; }\ntmp="$(mktemp -d "$TEST_ROOT/fixture.XXXXXX")"\n'
                  f'PGBIN="$TEST_BIN"\npg_url=""\n{block}\n'
                  'printf "%s\\n%s\\n%s\\n" "${PGPASSFILE-}" "${PGPASSWORD-unset}" "$pg_url"\n')
        result = subprocess.run(['bash', '-c', script], cwd=ROOT, capture_output=True, text=True, timeout=10,
                                env=dict(os.environ, TEST_ROOT=str(self.root), TEST_BIN=str(self.bin)))
        self.assertEqual(result.returncode, 0, result.stderr)
        password, port = self.assert_private_cluster()
        passfile, pgpassword, url = result.stdout.splitlines()
        self.assertEqual(url, f'postgres://postgres@localhost:{port}/sgfix')
        self.assertEqual(pgpassword, 'unset')
        self.assert_client_environment({'PGPASSFILE': passfile}, password, port)


if __name__ == '__main__':
    unittest.main()
