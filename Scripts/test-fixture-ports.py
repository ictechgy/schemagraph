#!/usr/bin/env python3
"""실제 fixture 기동 구문을 가짜 Docker로 실행해 포트 추측·재사용을 막는다."""
import json
import os
from pathlib import Path
import re
import shlex
import subprocess
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[1]
FIXTURES = (ROOT / 'Scripts/verify-fixtures.sh').read_text()
HELPER = ROOT / 'Scripts/docker-fixture-port.sh'
ACCURACY = ROOT / 'Fixtures/accuracy'
MYSQL_IMAGES = json.loads((ACCURACY / 'mysql-environments.json').read_text())['engines']
# verify-sqlserver-oracle-accuracy.py가 정확도 이미지에 강제하는 것과 같은 규칙이다.
DIGEST_IMAGE = re.compile(r'.+@sha256:[0-9a-f]{64}')


def owned_block(start, end):
    """검증 대상 기동 코드를 복제하지 않고 운영 스크립트에서 가져온다."""
    return FIXTURES.split(start, 1)[1].split(end, 1)[0]


def docker_runs():
    """운영 스크립트의 docker run마다 (컨테이너 이름 인자, 이미지) 쌍을 순서대로 돌려준다."""
    runs = []
    for command in re.findall(r'^\s*docker run (?:.*\\\n)*.*$', FIXTURES, re.MULTILINE):
        words = shlex.split(command.replace('\\\n', ' '))[2:]
        names = []
        while words[0].startswith('-'):
            option = words.pop(0)
            if option in ('--name', '-e', '-p'):
                value = words.pop(0)
                names += [value] if option == '--name' else []
            elif option != '-d':
                # 값 개수를 모르는 옵션 뒤의 낱말을 이미지로 오인하지 않게 멈춘다.
                raise AssertionError(f'unknown docker run option {option}; teach docker_runs() its arity')
        runs.append((names[0], words[0]))
    return runs


class FixturePorts(unittest.TestCase):
    """외부 호출만 격리하고 할당 결과가 실제 URL에 전달되는지 확인한다."""

    def shell(self, code, *, binding='127.0.0.1:49152', failure='', private='3306'):
        """모든 Docker·DB 경계를 대체하므로 서버나 네트워크를 사용하지 않는다."""
        with tempfile.TemporaryDirectory() as directory:
            log = Path(directory) / 'calls'
            count = Path(directory) / 'count'
            count.write_text('0')
            env = dict(os.environ, MOCK_BINDING=binding, MOCK_FAILURE=failure,
                       MOCK_PRIVATE=private, MOCK_LOG=str(log), MOCK_COUNT=str(count))
            prelude = r'''
set -euo pipefail
source "$1"
trap() { :; }
python3() { printf 'readiness %s\n' "$*" >> "$MOCK_LOG"; }
mock_java() { printf 'java %s\n' "$*" >> "$MOCK_LOG"; }
docker() {
    printf '%s\n' "$*" >> "$MOCK_LOG"
    case "$1" in
        run)
            [[ " $* " == *" -p 127.0.0.1::$MOCK_PRIVATE "* ]] || return 91
            [[ $MOCK_FAILURE != run ]] || return 125
            echo fixture-container
            ;;
        port)
            [[ $3 == "$MOCK_PRIVATE/tcp" ]] || return 92
            [[ $MOCK_FAILURE != port ]] || return 23
            if [[ $MOCK_BINDING == changing ]]; then
                local n; n=$(cat "$MOCK_COUNT"); n=$((n+1)); echo "$n" > "$MOCK_COUNT"
                printf '127.0.0.1:%s\n' "$((49000+n))"
            else
                printf '%s\n' "$MOCK_BINDING"
            fi
            ;;
        rm|exec) ;;
        *) return 93 ;;
    esac
}
tmp=$(dirname "$MOCK_LOG")
MYFIX=Fixtures/mysql
my_container=fixture
maria_container=fixture
JAVABIN=mock_java
JAR=fixture.jar
OJAR=oracle.jar
ms_user=fixture
ms_pass=fixture
oracle_user=fixture
oracle_pass=fixture
'''
            result = subprocess.run(['bash', '-c', prelude + code, 'fixture-test', str(HELPER)],
                                    cwd=ROOT, env=env, text=True, capture_output=True, timeout=5)
            return result, log.read_text() if log.exists() else ''

    def test_accepts_only_one_valid_ipv4_loopback_binding(self):
        """빈 값·공개 주소·중복 매핑·잘못된 숫자를 URL로 사용하지 않는다."""
        for value in ('', '0.0.0.0:49152', '[::1]:49152', '127.0.0.1:0',
                      '127.0.0.1:65536', '127.0.0.1:abc', '127.0.0.1:01',
                      '127.0.0.1:49152\n127.0.0.1:49153'):
            with self.subTest(binding=value):
                result, _ = self.shell('fixture_port fixture 3306', binding=value)
                self.assertNotEqual(result.returncode, 0)
                self.assertIn('expected one IPv4 loopback port', result.stderr)
        for port in ('1', '49152', '65535'):
            with self.subTest(port=port):
                result, calls = self.shell('fixture_port fixture 3306', binding='127.0.0.1:'+port)
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual(result.stdout.strip(), port)
                self.assertEqual(calls.strip(), 'port fixture 3306/tcp')

    def test_docker_lookup_failure_is_not_an_empty_success(self):
        """실패한 Docker 조회는 fixture 적용 전에 같은 오류로 멈춘다."""
        result, _ = self.shell('fixture_port fixture 3306', failure='port')
        self.assertEqual(result.returncode, 23)

    def mysql(self):
        """MySQL 소유 컨테이너 분기 전체를 선택한다."""
        return '    my_container="sg-verify-mysql-$$"\n' + owned_block(
            '    my_container="sg-verify-mysql-$$"\n', '\nfi\n\nif [ -n "$my_url" ]')

    def test_mysql_uses_assigned_port_without_guessing(self):
        """Docker가 고른 포트로 native·Go·JDBC 공통 URL을 구성한다."""
        result, calls = self.shell(self.mysql() + '\nprintf "%s" "$my_url"')
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout, 'mysql://root@127.0.0.1:49152/sgfix')
        self.assertIn('readiness Scripts/wait-mysql-ready.py', calls)

    def test_failed_creation_and_mapping_stop_before_readiness(self):
        """생성 오류·잘못된 바인딩을 무시하고 DB 검증을 건너뛰지 않는다."""
        for failure, binding, code in [('run', '127.0.0.1:49152', 125),
                                       ('port', '127.0.0.1:49152', 23),
                                       ('', '0.0.0.0:49152', 1)]:
            with self.subTest(failure=failure, binding=binding):
                result, calls = self.shell(self.mysql(), failure=failure, binding=binding)
                self.assertEqual(result.returncode, code, result.stderr)
                self.assertNotIn('readiness', calls)
                self.assertNotIn('exec -i', calls)

    def test_mariadb_recreation_refreshes_the_url(self):
        """PFS OFF와 ON이 서로 다른 할당을 받아도 이전 포트에 접속하지 않는다."""
        function = 'maria_up() {' + owned_block('maria_up() {', '\n\nif [ -n "${SG_MARIADB_URL:-}" ]')
        result, calls = self.shell(function + '\nmaria_up\nprintf "%s\\n" "$maria_url"\n'
                                  'maria_up --performance-schema=ON\nprintf "%s\\n" "$maria_url"',
                                  binding='changing')
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout.splitlines(), ['mysql://root@127.0.0.1:49001/sgfix',
                                                      'mysql://root@127.0.0.1:49002/sgfix'])
        self.assertEqual(calls.count('port fixture 3306/tcp'), 2)
        self.assertIn(MYSQL_IMAGES['mariadb']['image'] + ' --performance-schema=ON', calls)
        self.assertEqual(calls.count('readiness'), 2)

    def test_jdbc_startup_uses_each_assigned_port(self):
        """SQL Server·Oracle의 준비 조회와 최종 JDBC URL을 같은 할당에 묶는다."""
        for name, private, variable, expected in [
                ('mssql', '1433', 'mssql_jdbc', 'jdbc:sqlserver://127.0.0.1:49152;databaseName=sgfix;encrypt=false'),
                ('oracle', '1521', 'oracle_jdbc', 'jdbc:oracle:thin:@127.0.0.1:49152/FREEPDB1')]:
            with self.subTest(database=name):
                start = f'        {name}_container="sg-verify-{name}-$$"\n'
                block = start + owned_block(start, f'\n    fi\n\n    if [ -n "${variable}" ]')
                result, calls = self.shell(block + f'\nprintf "%s" "${variable}"', private=private)
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual(result.stdout, expected)
                self.assertIn('127.0.0.1:49152', calls)


class FixtureImages(unittest.TestCase):
    """CI가 실행하는 fixture 컨테이너 이미지를 정적으로 검사한다."""

    def test_every_fixture_image_is_pinned_by_digest(self):
        """업스트림 태그가 옮겨 가거나 교체돼도 검토하지 않은 이미지를 실행하지 않는다."""
        runs = docker_runs()
        self.assertEqual([name for name, _ in runs],
                         ['$my_container', '$maria_container', '$mssql_container', '$oracle_container'])
        for name, image in runs:
            with self.subTest(container=name):
                self.assertTrue(DIGEST_IMAGE.fullmatch(image), image)

    def test_shared_engines_use_the_accuracy_environment_images(self):
        """정확도 코퍼스와 fixture 패리티가 같은 고정 이미지를 쓴다는 SUPPORT-MATRIX 서술을 지킨다."""
        oracle = json.loads((ACCURACY / 'sqlserver-oracle-environments.json').read_text())['engines']['oracle']
        images = dict(docker_runs())
        self.assertEqual(images['$my_container'], MYSQL_IMAGES['mysql']['image'])
        self.assertEqual(images['$maria_container'], MYSQL_IMAGES['mariadb']['image'])
        self.assertEqual(images['$oracle_container'], oracle['image'])


if __name__ == '__main__':
    unittest.main()
