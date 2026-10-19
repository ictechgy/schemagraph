#!/usr/bin/env python3
"""네이티브 PostgreSQL reader의 TLS 동작을 실제 PostgreSQL 서버로 검증한다.

실행마다 openssl로 임시 CA 두 개와 서버 인증서(SAN: localhost)를 만들고, TLS 연결만
받는(hostssl) 클러스터와 TLS를 끈 클러스터를 루프백에만 띄운다. 그다음 엔진 scan의 종료
코드와 graph.json limitations로 다음을 확인한다.

- verify-full·verify-ca는 맞는 CA와 인증서에 있는 이름(localhost)으로 성공하고, 다른 CA를
  주면 종료 2다. 인증서에 없는 이름(127.0.0.1)이면 verify-full은 종료 2다.
- verify-ca도 지금은 인증서에 없는 이름에서 종료 2다. libpq 의미로는 이름을 보지 않아야
  하지만, sqlx 0.8.6이 rustls 0.23의 이름 불일치 오류(NotValidForNameContext)를 알아보지
  못한다. README가 이 동작을 적으므로, sqlx를 올려 의미가 돌아오면 이 사례가 실패해 문서를
  고치게 한다.
- require와 기본 prefer는 TLS로 성공하고, disable은 hostssl 서버가 거절한다(종료 2).
- TLS를 끈 서버에서 require는 종료 2이고, 기본 prefer는 평문으로 성공한다.
- 루프백 평문 연결에는 `connection-not-encrypted:` limitation이 없다.
- 네트워크 호스트로 분류되는 연결은 서버가 본 전송 상태를 따른다. TLS 서버에서는 기본
  prefer가 pg_stat_ssl로 TLS를 확인해 limitation이 없고, TLS를 끈 서버에서는 평문 관측
  limitation이 남는다. 확인 질의가 실패했을 때의 문구("could not be confirmed")와 구분한다.
  호스트는 `0.0.0.0`을 쓴다. reader는 루프백 주소가 아니라서 네트워크로 보지만, Linux·macOS
  커널은 0.0.0.0으로 가는 접속을 자기 호스트로 보내 루프백 리스너에 닿는다. 그래서 클러스터를
  외부에 열지 않고도 원격 판정(서버 확인 질의 포함)을 탄다. `127.1` 같은 축약 표기는 rustls가
  서버 이름으로 받지 않아 TLS 서버에는 쓸 수 없다.

사용자 PG 설정이 결과를 바꾸지 않도록 PG로 시작하는 환경 변수(PGSSLMODE 등)를 지운
환경으로 돌린다. 비밀번호는 0600 PGPASSFILE로만 넘겨 URL·argv에 싣지 않는다.
"""

import argparse
import json
import os
from pathlib import Path
import secrets
import shutil
import socket
import subprocess
import sys
import tempfile
import urllib.parse

# 평문 원격 연결을 알리는 limitation 접두사다(엔진 계약).
LIMITATION_PREFIX = 'connection-not-encrypted:'
# 서버가 평문이라고 답했을 때의 문구다. 확인 질의 실패("could not be confirmed")와 가른다.
OBSERVED_PLAINTEXT = 'read over an unencrypted connection'
# reader가 네트워크 호스트로 분류하지만 커널이 자기 호스트로 보내는 주소다.
NETWORK_HOST = '0.0.0.0'
# 연결마다 확인하는 테이블이다. 그래프에 이 정점이 있으면 카탈로그를 실제로 읽은 것이다.
PROBE_TABLE = 'public.tls_probe'
# 사례별 전송 limitation 기대값: 없음, 또는 서버가 답한 평문 관측 하나.
EXPECT_NONE = 'none'
EXPECT_PLAINTEXT = 'plaintext'

# 인증서 확장과 이름은 인증서마다 설정 파일로 준다. LibreSSL 등 `-addext`가 없는 openssl도
# 같은 절차로 돈다. LibreSSL은 `prompt = no` 설정의 이름을 쓰고 `-subj`를 무시하므로 이름도
# 설정 파일에 싣는다.
OPENSSL_CONFIG = '''[req]
distinguished_name = subject
prompt = no
[subject]
CN = {common_name}
[ca]
basicConstraints = critical,CA:TRUE
keyUsage = critical,keyCertSign,cRLSign
subjectKeyIdentifier = hash
[server]
basicConstraints = critical,CA:FALSE
keyUsage = critical,digitalSignature,keyEncipherment
extendedKeyUsage = serverAuth
subjectAltName = DNS:localhost
'''


def run(command, *, env=None):
    """명령을 실행하고 실패하면 stderr 끝부분과 함께 멈춘다."""
    result = subprocess.run([str(value) for value in command], capture_output=True,
                            text=True, env=env, timeout=120)
    if result.returncode:
        raise RuntimeError(f'{Path(str(command[0])).name} failed: {result.stderr[-2000:]}')
    return result.stdout


def require_tools(bindir, openssl):
    """필요한 도구가 없으면 원인과 해결 방향을 담아 멈춘다."""
    missing = [str(bindir/tool) for tool in ('initdb', 'pg_ctl', 'psql') if not (bindir/tool).is_file()]
    if shutil.which(openssl) is None:
        missing.append(openssl)
    if missing:
        raise RuntimeError(f'required tools are missing: {", ".join(missing)}; '
                           'set PGBIN (or --postgres-bin) and --openssl')


def private_file(path, text):
    """비밀번호 파일을 처음부터 0600으로 만든다(만든 뒤 chmod하면 잠깐 열린다)."""
    with os.fdopen(os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600), 'w') as stream:
        stream.write(text)
    return path


def free_port():
    """루프백에서 비어 있는 TCP 포트를 하나 고른다."""
    with socket.socket() as candidate:
        candidate.bind(('127.0.0.1', 0))
        return candidate.getsockname()[1]


def openssl_config(work, name, common_name):
    """인증서 하나의 openssl 설정 파일을 쓴다. 두 CA의 이름이 같으면 다른 CA 사례가 발급자
    불일치(UnknownIssuer)가 아니라 같은 이름·다른 키(BadSignature)를 검사하게 된다."""
    config = work/f'{name}.cnf'
    config.write_text(OPENSSL_CONFIG.format(common_name=common_name))
    return config


def make_ca(openssl, work, name):
    """자체 서명 CA 인증서와 키를 만든다. webpki는 basicConstraints CA:TRUE가 있어야 CA로 본다."""
    key, cert = work/f'{name}.key', work/f'{name}.crt'
    config = openssl_config(work, name, f'schemagraph {name}')
    run([openssl, 'req', '-x509', '-new', '-newkey', 'rsa:2048', '-nodes', '-keyout', key,
         '-out', cert, '-days', '2', '-config', config, '-extensions', 'ca'])
    return key, cert


def make_server_cert(openssl, work, ca_key, ca_cert):
    """CA로 서명한 서버 인증서를 만든다. rustls는 CN만 있고 SAN이 없는 인증서를 거부한다."""
    key, request, cert = work/'server.key', work/'server.csr', work/'server.crt'
    config = openssl_config(work, 'server', 'localhost')
    run([openssl, 'req', '-new', '-newkey', 'rsa:2048', '-nodes', '-keyout', key, '-out', request,
         '-config', config])
    run([openssl, 'x509', '-req', '-in', request, '-CA', ca_cert, '-CAkey', ca_key,
         '-set_serial', str(secrets.randbelow(2**62) + 1), '-days', '2', '-out', cert,
         '-extfile', config, '-extensions', 'server'])
    # PostgreSQL은 그룹·기타가 읽을 수 있는 서버 키로는 기동하지 않는다.
    key.chmod(0o600)
    return key, cert


def make_certificates(openssl, work):
    """신뢰할 CA·다른 CA·서버 인증서를 만들고 경로를 돌려준다."""
    ca_key, ca_cert = make_ca(openssl, work, 'trusted-ca')
    _, other_cert = make_ca(openssl, work, 'other-ca')
    server_key, server_cert = make_server_cert(openssl, work, ca_key, ca_cert)
    return {'ca': ca_cert, 'other_ca': other_cert, 'key': server_key, 'cert': server_cert}


def clean_environment():
    """PG로 시작하는 변수를 뺀 환경. libpq·sqlx가 이 값으로 sslmode·호스트를 바꾸기 때문이다."""
    return {key: value for key, value in os.environ.items() if not key.startswith('PG')}


def configure_cluster(data, tls):
    """pg_hba와 SSL 설정을 쓴다. TLS 클러스터는 hostssl만 받아 평문 접속을 거절한다."""
    method = 'hostssl' if tls else 'host'
    (data/'pg_hba.conf').write_text(f'{method} all all 127.0.0.1/32 scram-sha-256\n')
    if tls:
        settings = ['ssl = on\n', f"ssl_cert_file = '{tls['cert']}'\n", f"ssl_key_file = '{tls['key']}'\n"]
        with (data/'postgresql.conf').open('a') as stream:
            stream.writelines(settings)


def start_cluster(bindir, work, name, tls, password, env):
    """임시 클러스터를 만들고 루프백 TCP로만 띄운다. Unix 소켓은 끈다(`-k ''`)."""
    data, log = work/f'{name}-data', work/f'{name}.log'
    password_file = private_file(work/f'{name}-password', password+'\n')
    run([bindir/'initdb', '-D', data, '-U', 'postgres', '--auth-local=trust',
         '--auth-host=scram-sha-256', f'--pwfile={password_file}', '--no-locale',
         '--encoding=UTF8'], env=env)
    configure_cluster(data, tls)
    port = free_port()
    try:
        # 서버 출력은 -l 로그로 가서 이 프로세스의 파이프를 붙잡지 않는다.
        run([bindir/'pg_ctl', '-D', data, '-l', log, '-o', f"-p {port} -k '' -h 127.0.0.1",
             '-w', '-t', '60', 'start'], env=env)
    except RuntimeError as error:
        server_log = log.read_text()[-2000:] if log.exists() else '(no server log)'
        raise RuntimeError(f'temporary {name} cluster failed to start: {server_log}') from error
    return data, port


def stop_cluster(bindir, data, env):
    """띄운 클러스터만 멈춘다. 기동 전에 실패했으면 할 일이 없다."""
    if (data/'postmaster.pid').exists():
        run([bindir/'pg_ctl', '-D', data, '-m', 'fast', '-w', 'stop'], env=env)


def create_probe_table(bindir, port, env):
    """scan이 읽을 테이블을 만든다. psql은 libpq 기본(prefer)이라 hostssl에도 붙는다."""
    run([bindir/'psql', '-X', '-h', '127.0.0.1', '-p', port, '-U', 'postgres', '-d', 'postgres',
         '-v', 'ON_ERROR_STOP=1', '-qc', f'CREATE TABLE {PROBE_TABLE} (id integer PRIMARY KEY)'],
        env=env)


def scan_url(host, port, parameters):
    """비밀번호 없는 접속 URL을 만든다. 인증서 경로는 퍼센트 인코딩한다."""
    query = urllib.parse.urlencode(parameters, quote_via=urllib.parse.quote)
    return f'postgres://postgres@{host}:{port}/postgres' + (f'?{query}' if query else '')


def scan(engine, url, work, label, env):
    """엔진 scan을 돌려 종료 코드, stderr, 성공 시 graph.json을 돌려준다."""
    graph = work/f'{label}.graph.json'
    result = subprocess.run([str(engine), 'scan', url, '-o', str(graph)], capture_output=True,
                            text=True, env=env, timeout=120)
    loaded = json.loads(graph.read_text()) if result.returncode == 0 else None
    return result.returncode, result.stderr, loaded


def transport_limitations(graph):
    """그래프 limitations 중 전송 암호화 limitation만 고른다."""
    return [item for item in graph.get('limitations', []) if item.startswith(LIMITATION_PREFIX)]


def check_transport(label, found, expected):
    """전송 limitation이 기대와 같은지 본다. 평문 관측은 확인 실패 문구와 구분한다."""
    if expected == EXPECT_NONE and found:
        raise AssertionError(f'{label}: expected no transport limitation, got {found}')
    if expected == EXPECT_PLAINTEXT and (len(found) != 1 or OBSERVED_PLAINTEXT not in found[0]):
        raise AssertionError(f'{label}: expected one observed-plaintext limitation, got {found}')


def check_success(label, graph, expected, hidden):
    """성공한 scan이 카탈로그를 읽었고 전송 limitation이 기대와 같은지 본다."""
    if not any(vertex.get('id') == PROBE_TABLE for vertex in graph.get('vertices', [])):
        raise AssertionError(f'{label}: {PROBE_TABLE} was not collected')
    found = transport_limitations(graph)
    check_transport(label, found, expected)
    for item in found:
        if any(value in item for value in hidden):
            raise AssertionError(f'{label}: limitation leaks connection details: {item}')
    return found


def last_line(text):
    """오류 출력의 마지막 줄. 원인 줄을 요약에 남겨 CI 산출물만으로 실패 이유를 보게 한다."""
    lines = text.strip().splitlines()
    return lines[-1] if lines else ''


def check_case(engine, work, env, case, hidden):
    """사례 하나를 돌려 종료 코드·오류 문구·limitation을 판정하고 요약을 돌려준다."""
    label, url, code, stderr_fragment, expected = case
    actual, stderr, graph = scan(engine, url, work, label, env)
    if actual != code:
        raise AssertionError(f'{label}: exit {actual}, expected {code}: {stderr[-1500:]}')
    if stderr_fragment and stderr_fragment.lower() not in stderr.lower():
        raise AssertionError(f'{label}: stderr lacks {stderr_fragment!r}: {stderr[-1500:]}')
    if any(value in stderr for value in hidden['stderr']):
        raise AssertionError(f'{label}: stderr leaks the password')
    if code:
        return {'case': label, 'exit': actual, 'error': last_line(stderr)}
    found = check_success(label, graph, expected, hidden['limitation'])
    return {'case': label, 'exit': actual, 'limitations': found}


def tls_cluster_cases(port, certificates):
    """TLS만 받는 서버의 사례: (이름, URL, 종료 코드, stderr 조각, 전송 limitation 기대)."""
    trusted = {'sslrootcert': str(certificates['ca'])}
    other = {'sslrootcert': str(certificates['other_ca'])}
    full, ca_only = {'sslmode': 'verify-full'}, {'sslmode': 'verify-ca'}
    return [
        ('verify-full-trusted-ca', scan_url('localhost', port, full | trusted), 0, None, EXPECT_NONE),
        ('verify-ca-trusted-ca', scan_url('localhost', port, ca_only | trusted), 0, None, EXPECT_NONE),
        ('verify-full-other-ca', scan_url('localhost', port, full | other), 2, 'UnknownIssuer', None),
        ('verify-ca-other-ca', scan_url('localhost', port, ca_only | other), 2, 'UnknownIssuer', None),
        ('verify-full-name-mismatch', scan_url('127.0.0.1', port, full | trusted), 2, 'not valid for name', None),
        ('verify-ca-checks-host-name', scan_url('127.0.0.1', port, ca_only | trusted), 2, 'not valid for name', None),
        ('require', scan_url('127.0.0.1', port, {'sslmode': 'require'}), 0, None, EXPECT_NONE),
        ('default-prefer-hostssl-only', scan_url('127.0.0.1', port, {}), 0, None, EXPECT_NONE),
        ('default-prefer-network-host', scan_url(NETWORK_HOST, port, {}), 0, None, EXPECT_NONE),
        ('disable-hostssl-only', scan_url('127.0.0.1', port, {'sslmode': 'disable'}), 2, 'pg_hba.conf', None),
    ]


def plain_cluster_cases(port):
    """TLS를 끈 서버의 사례. 루프백은 limitation이 없고, 네트워크 호스트는 평문 관측이 남는다."""
    return [
        ('require-without-server-tls', scan_url('127.0.0.1', port, {'sslmode': 'require'}), 2,
         'server does not support TLS', None),
        ('default-prefer-loopback', scan_url('127.0.0.1', port, {}), 0, None, EXPECT_NONE),
        ('default-prefer-localhost', scan_url('localhost', port, {}), 0, None, EXPECT_NONE),
        ('disable-loopback', scan_url('127.0.0.1', port, {'sslmode': 'disable'}), 0, None, EXPECT_NONE),
        ('default-prefer-network-host', scan_url(NETWORK_HOST, port, {}), 0, None, EXPECT_PLAINTEXT),
        ('disable-network-host', scan_url(NETWORK_HOST, port, {'sslmode': 'disable'}), 0, None, EXPECT_PLAINTEXT),
    ]


def run_cluster(args, work, name, tls, password, cases_for):
    """클러스터 하나를 띄워 사례를 모두 돌리고 멈춘다."""
    env = args.environment
    data, port = start_cluster(args.postgres_bin, work, name, tls, password, env)
    try:
        create_probe_table(args.postgres_bin, port, env)
        # 비밀번호는 오류에도 limitation에도, 호스트·포트는 limitation에 실리면 안 된다.
        hidden = {'stderr': [password], 'limitation': [password, NETWORK_HOST, f':{port}']}
        return [check_case(args.engine, work, env, case, hidden) for case in cases_for(port)]
    finally:
        stop_cluster(args.postgres_bin, data, env)


def verify(args, work):
    """두 클러스터의 사례를 모두 돌린 요약을 돌려준다.

    비밀번호는 0600 PGPASSFILE로만 넘긴다. 자식 프로세스 환경이나 argv에 싣지 않는다.
    """
    require_tools(args.postgres_bin, args.openssl)
    password = secrets.token_hex(16)
    args.environment = clean_environment()
    args.environment['PGPASSFILE'] = str(private_file(work/'pgpass', f'*:*:*:*:{password}\n'))
    certificates = make_certificates(args.openssl, work)
    tls = run_cluster(args, work, 'tls', certificates, password,
                      lambda port: tls_cluster_cases(port, certificates))
    plain = run_cluster(args, work, 'plain', None, password, plain_cluster_cases)
    return {'tls_server': tls, 'plaintext_server': plain}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--engine', type=Path, required=True)
    parser.add_argument('--postgres-bin', type=Path,
                        default=Path(os.environ.get('PGBIN', '/opt/homebrew/opt/postgresql@16/bin')))
    parser.add_argument('--openssl', default='openssl')
    parser.add_argument('--output', type=Path)
    args = parser.parse_args()
    args.engine = args.engine.resolve()
    with tempfile.TemporaryDirectory(prefix='sg-native-tls-') as directory:
        try:
            result = verify(args, Path(directory))
        except (AssertionError, RuntimeError) as error:
            sys.exit(f'verify-native-tls: {error}')
    text = json.dumps(result, sort_keys=True, indent=2)+'\n'
    if args.output:
        args.output.write_text(text)
    sys.stdout.write(text)


if __name__ == '__main__':
    main()
