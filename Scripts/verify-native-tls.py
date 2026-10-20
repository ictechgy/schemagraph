#!/usr/bin/env python3
"""네이티브 PostgreSQL(선택하면 MySQL·MariaDB도) reader의 TLS 동작을 실제 서버로 검증한다.

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
- 네트워크 호스트로 분류되는 연결은 reader가 직접 한 TLS 협상 결과를 따른다. 기본 prefer는
  TLS를 요구해 먼저 붙고, 서버가 TLS를 거절했을 때만 평문으로 다시 붙어 그 사실("the server
  did not accept TLS")을 limitation에 남긴다. TLS 서버에서는 limitation이 없고, TLS를 끈
  서버에서는 거절 limitation이 남는다. disable은 모드 문구("the catalog was read over an
  unencrypted connection")를 남기므로 두 문구를 가른다.
- 공격 재현: 클라이언트 쪽 TLS만 거절하고 서버와는 TLS로 붙는 중간자를 TLS 서버 앞에 둔다.
  서버가 보는 세션은 TLS라 서버의 자기 보고(pg_stat_ssl)는 암호화됐다고 답하지만, reader의
  연결은 평문이다. 기본 prefer가 거절 limitation을 남겨야 한다.
  호스트는 `0.0.0.0`을 쓴다. reader는 루프백 주소가 아니라서 네트워크로 보지만, Linux·macOS
  커널은 0.0.0.0으로 가는 접속을 자기 호스트로 보내 루프백 리스너에 닿는다. 그래서 클러스터를
  외부에 열지 않고도 원격 판정(TLS를 요구한 첫 접속 포함)을 탄다. `127.1` 같은 축약 표기는
  rustls가 서버 이름으로 받지 않아 TLS 서버에는 쓸 수 없다.

사용자 PG 설정이 결과를 바꾸지 않도록 PG로 시작하는 환경 변수(PGSSLMODE 등)를 지운
환경으로 돌린다. 비밀번호는 0600 PGPASSFILE로만 넘겨 URL·argv에 싣지 않는다.

`--mysql-environments`(정확도 하네스와 같은 고정 이미지 목록)를 주면 MySQL·MariaDB도
Docker로 검증한다. 엔진마다 기본 설정(자체 서명 인증서로 TLS를 켠다) 서버와 TLS를 끈
서버를 루프백 포트로 띄운다. 네트워크 호스트(`0.0.0.0`)의 기본 PREFERRED 연결은 MySQL
reader가 REQUIRED로 먼저 붙어 보는 경로다. TLS 서버에서는 limitation이 없어야 하고, TLS를
끈 서버에서는 거절 limitation이 남아야 한다. 루프백 fixture(`127.0.0.1`)만으로는 이 경로를
타지 않는다.
"""

import argparse
from contextlib import contextmanager
import json
import os
from pathlib import Path
import secrets
import select
import shutil
import socket
import ssl
import subprocess
import sys
import tempfile
import threading
import urllib.parse

ROOT = Path(__file__).resolve().parents[1]

# 평문 원격 연결을 알리는 limitation 접두사다(엔진 계약).
LIMITATION_PREFIX = 'connection-not-encrypted:'
# 모드가 TLS를 시도하지 않은 평문 연결(disable·DISABLED)의 문구다.
PLAINTEXT_BY_MODE = f'{LIMITATION_PREFIX} the catalog was read over an unencrypted connection;'
# 기본 prefer에서 서버가 TLS를 거절해 평문으로 다시 붙은 연결의 문구다.
PLAINTEXT_AFTER_REFUSAL = (f'{LIMITATION_PREFIX} the server did not accept TLS, so the catalog '
                           'was read over an unencrypted connection;')
# reader가 네트워크 호스트로 분류하지만 커널이 자기 호스트로 보내는 주소다.
NETWORK_HOST = '0.0.0.0'
# 연결마다 확인하는 테이블이다. 그래프에 이 정점이 있으면 카탈로그를 실제로 읽은 것이다.
PROBE_TABLE = 'public.tls_probe'
# 사례별 전송 limitation 기대값: 없음, 모드로 끈 평문 하나, 또는 서버가 거절한 평문 하나.
EXPECT_NONE = 'none'
EXPECT_PLAINTEXT = 'plaintext'
EXPECT_REFUSED = 'refused'
EXPECTED_LIMITATION = {EXPECT_PLAINTEXT: PLAINTEXT_BY_MODE, EXPECT_REFUSED: PLAINTEXT_AFTER_REFUSAL}
# PostgreSQL SSLRequest(길이 8, 요청 코드 80877103)다. 중간자 재현이 TLS 요청을 알아본다.
PG_SSL_REQUEST = bytes([0x00, 0x00, 0x00, 0x08, 0x04, 0xD2, 0x16, 0x2F])
# 중간자 중계가 한쪽을 기다리는 상한(초). scan 하나보다 넉넉하다.
RELAY_IDLE_SECONDS = 60

# MySQL·MariaDB 사례가 쓸 데이터베이스다. Scripts/wait-mysql-ready.py가 이 이름으로 최종
# 서버의 TCP 준비를 확인한다(fixture 하네스와 같은 절차).
MYSQL_DATABASE = 'sgfix'
MYSQL_PROBE_TABLE = f'{MYSQL_DATABASE}.tls_probe'
# 이 실행이 띄운 컨테이너의 표식이다. 정리할 때 표식 값이 다른 컨테이너는 건드리지 않는다.
MYSQL_LABEL = 'io.github.ictechgy.schemagraph.native-tls'
# TLS를 끈 서버의 기동 옵션. MySQL 8.4는 빈 tls_version, MariaDB 11.4는 --skip-ssl로 끈다.
MYSQL_PLAINTEXT_OPTIONS = {'mysql': ['--tls-version='], 'mariadb': ['--skip-ssl']}

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


def run(command, *, env=None, timeout=120):
    """명령을 실행하고 실패하면 stderr 끝부분과 함께 멈춘다."""
    result = subprocess.run([str(value) for value in command], capture_output=True,
                            text=True, env=env, timeout=timeout)
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
    """전송 limitation이 기대와 같은지 본다. 모드로 끈 평문과 서버가 거절한 평문을 문구로 가른다."""
    if expected == EXPECT_NONE:
        if found:
            raise AssertionError(f'{label}: expected no transport limitation, got {found}')
        return
    prefix = EXPECTED_LIMITATION[expected]
    if len(found) != 1 or not found[0].startswith(prefix):
        raise AssertionError(f'{label}: expected one limitation starting with {prefix!r}, got {found}')


def check_success(label, graph, expected, hidden, probe=PROBE_TABLE):
    """성공한 scan이 카탈로그를 읽었고 전송 limitation이 기대와 같은지 본다."""
    if not any(vertex.get('id') == probe for vertex in graph.get('vertices', [])):
        raise AssertionError(f'{label}: {probe} was not collected')
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


def check_case(engine, work, env, case, hidden, probe=PROBE_TABLE):
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
    found = check_success(label, graph, expected, hidden['limitation'], probe)
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
    """TLS를 끈 서버의 사례. 루프백은 limitation이 없다. 네트워크 호스트는 기본 prefer면 거절
    limitation이, disable이면 모드 limitation이 남는다."""
    return [
        ('require-without-server-tls', scan_url('127.0.0.1', port, {'sslmode': 'require'}), 2,
         'server does not support TLS', None),
        ('default-prefer-loopback', scan_url('127.0.0.1', port, {}), 0, None, EXPECT_NONE),
        ('default-prefer-localhost', scan_url('localhost', port, {}), 0, None, EXPECT_NONE),
        ('disable-loopback', scan_url('127.0.0.1', port, {'sslmode': 'disable'}), 0, None, EXPECT_NONE),
        ('default-prefer-network-host', scan_url(NETWORK_HOST, port, {}), 0, None, EXPECT_REFUSED),
        ('disable-network-host', scan_url(NETWORK_HOST, port, {'sslmode': 'disable'}), 0, None, EXPECT_PLAINTEXT),
    ]


def recv_exact(sock, size):
    """`size`바이트를 받는다. 상대가 먼저 닫으면 그때까지 받은 바이트를 돌려준다."""
    data = b''
    while len(data) < size:
        chunk = sock.recv(size - len(data))
        if not chunk:
            break
        data += chunk
    return data


class UpstreamTls:
    """중간자와 서버 사이의 TLS 구간. 서버 인증서는 시험 CA와 인증서 이름(localhost)으로 검증한다.

    MemoryBIO로 한 스레드가 읽기와 쓰기를 번갈아 하므로 SSL 객체를 두 스레드가 함께 쓰지 않는다.
    """

    def __init__(self, sock, cafile):
        context = ssl.create_default_context(cafile=str(cafile))
        self.sock = sock
        self.incoming, self.outgoing = ssl.MemoryBIO(), ssl.MemoryBIO()
        self.tls = context.wrap_bio(self.incoming, self.outgoing, server_hostname='localhost')
        self.handshake()

    def flush(self):
        """TLS가 보낼 바이트를 서버로 보낸다."""
        data = self.outgoing.read()
        if data:
            self.sock.sendall(data)

    def handshake(self):
        """서버와 TLS 핸드셰이크를 끝낸다."""
        while True:
            try:
                self.tls.do_handshake()
                break
            except ssl.SSLWantReadError:
                self.flush()
                data = self.sock.recv(65536)
                if not data:
                    raise RuntimeError('the server closed the connection during the TLS handshake')
                self.incoming.write(data)
        self.flush()

    def send(self, data):
        """평문을 암호화해 서버로 보낸다."""
        self.tls.write(data)
        self.flush()

    def receive(self, data):
        """서버가 보낸 TLS 바이트를 넣고 풀린 평문을 돌려준다. 레코드가 덜 왔으면 빈 바이트다."""
        self.incoming.write(data)
        plain = []
        while True:
            try:
                chunk = self.tls.read(65536)
            except ssl.SSLWantReadError:
                break
            if not chunk:
                break  # 서버가 TLS를 닫았다(close_notify). 이어지는 소켓 EOF가 중계를 끝낸다.
            plain.append(chunk)
        self.flush()
        return b''.join(plain)


def relay(client, upstream):
    """클라이언트 평문과 서버 TLS 사이에서 바이트를 옮긴다. 어느 쪽이든 닫으면 끝낸다."""
    while True:
        readable, _, _ = select.select([client, upstream.sock], [], [], RELAY_IDLE_SECONDS)
        if not readable:
            raise RuntimeError(f'the relay was idle for {RELAY_IDLE_SECONDS} seconds')
        for source in readable:
            data = source.recv(65536)
            if not data:
                return
            if source is client:
                upstream.send(data)
            else:
                client.sendall(upstream.receive(data))


class DowngradeProxy:
    """클라이언트 쪽 TLS만 거절하고 서버와는 TLS로 붙는 중간자(공격 재현).

    클라이언트의 SSLRequest에 `N`으로 답해 클라이언트 구간을 평문으로 만들고, 서버에는 자기가
    SSLRequest를 보내 TLS로 붙어 바이트를 옮긴다. 서버가 보는 세션은 TLS라 pg_stat_ssl은
    ssl = true를 답한다. 그 답으로 판정하는 reader는 클라이언트 평문 구간의 limitation을 잃는다.
    """

    def __init__(self, upstream_port, cafile):
        self.upstream_port = upstream_port
        self.cafile = cafile
        self.listener = socket.create_server(('127.0.0.1', 0))
        self.port = self.listener.getsockname()[1]
        self.errors = []
        self.thread = threading.Thread(target=self.serve, daemon=True)

    def __enter__(self):
        self.thread.start()
        return self

    def __exit__(self, *exc_info):
        self.listener.close()
        self.thread.join(timeout=10)

    def serve(self):
        """접속마다 중계 스레드를 띄운다. __exit__가 리스너를 닫으면 accept가 OSError로 끝난다."""
        while True:
            try:
                client, _ = self.listener.accept()
            except OSError:
                return
            threading.Thread(target=self.handle, args=(client,), daemon=True).start()

    def handle(self, client):
        """중계 하나를 돌린다. 실패 원인은 모아 두었다가 사례 판정에서 드러낸다."""
        try:
            with client:
                client.settimeout(RELAY_IDLE_SECONDS)
                self.downgrade(client)
        except Exception as error:  # 스레드 밖으로 원인을 옮긴다(사례 판정이 실패로 낸다).
            self.errors.append(f'{type(error).__name__}: {error}')

    def downgrade(self, client):
        """클라이언트의 TLS 요청을 거절하고, 평문으로 이어지는 접속만 서버의 TLS 세션에 잇는다."""
        first = recv_exact(client, len(PG_SSL_REQUEST))
        if first == PG_SSL_REQUEST:
            client.sendall(b'N')
            # TLS를 요구한 접속은 여기서 닫히고, 서버 자기 보고에 기대는 prefer는 같은 접속으로
            # 평문 StartupMessage를 잇는다.
            first = recv_exact(client, len(PG_SSL_REQUEST))
        if len(first) < len(PG_SSL_REQUEST):
            return
        with socket.create_connection(('127.0.0.1', self.upstream_port), timeout=RELAY_IDLE_SECONDS) as sock:
            sock.sendall(PG_SSL_REQUEST)
            if recv_exact(sock, 1) != b'S':
                raise RuntimeError('the server refused TLS to the proxy')
            upstream = UpstreamTls(sock, self.cafile)
            upstream.send(first)
            try:
                relay(client, upstream)
            except (ConnectionResetError, BrokenPipeError):
                return  # scan이 끝나며 접속을 끊었다. 정상 종료와 같다.


def check_downgrade_proxy(args, work, env, port, cafile, hidden):
    """TLS 서버 앞에 클라이언트 쪽 TLS만 거절하는 중간자를 두고 기본 prefer로 scan한다.

    서버는 TLS 세션을 보지만 reader의 연결은 평문이다. reader가 직접 한 TLS 협상으로 판정하면
    거절 limitation이 남고, 서버의 자기 보고(pg_stat_ssl)로 판정하면 limitation이 사라져 실패한다.
    """
    with DowngradeProxy(port, cafile) as proxy:
        case = ('downgrade-proxy-network-host', scan_url(NETWORK_HOST, proxy.port, {}), 0, None,
                EXPECT_REFUSED)
        concealed = {**hidden, 'limitation': [*hidden['limitation'], f':{proxy.port}']}
        try:
            result = check_case(args.engine, work, env, case, concealed)
        except AssertionError as error:
            raise AssertionError(f'{error}; relay errors: {proxy.errors}') from error
    if proxy.errors:
        raise AssertionError(f'downgrade-proxy-network-host: relay failed: {proxy.errors}')
    return result


def run_cluster(args, work, name, tls, password, cases_for):
    """클러스터 하나를 띄워 사례를 모두 돌리고 멈춘다. TLS 클러스터에서는 중간자 사례도 돌린다."""
    env = args.environment
    data, port = start_cluster(args.postgres_bin, work, name, tls, password, env)
    try:
        create_probe_table(args.postgres_bin, port, env)
        # 비밀번호는 오류에도 limitation에도, 호스트·포트는 limitation에 실리면 안 된다.
        hidden = {'stderr': [password], 'limitation': [password, NETWORK_HOST, f':{port}']}
        results = [check_case(args.engine, work, env, case, hidden) for case in cases_for(port)]
        if tls:
            results.append(check_downgrade_proxy(args, work, env, port, tls['ca'], hidden))
        return results
    finally:
        stop_cluster(args.postgres_bin, data, env)


def require_docker():
    """MySQL·MariaDB 사례에 필요한 Docker가 없으면 원인과 해결 방향을 담아 멈춘다."""
    if shutil.which('docker') is None:
        raise RuntimeError('docker is required for --mysql-environments; install Docker or omit the option')
    run(['docker', 'info'], timeout=60)


def published_port(name):
    """컨테이너 3306이 IPv4 루프백에만 열렸는지 확인하고 호스트 포트를 돌려준다."""
    output = run(['docker', 'container', 'inspect', '--format',
                  '{{json (index .NetworkSettings.Ports "3306/tcp")}}', name])
    bindings = json.loads(output)
    if not bindings or len(bindings) != 1 or bindings[0]['HostIp'] != '127.0.0.1':
        raise RuntimeError(f'temporary database must be published only on IPv4 loopback: {bindings}')
    return int(bindings[0]['HostPort'])


def remove_owned_container(name, owner):
    """이 실행이 띄운 컨테이너만 지운다. 이미 없으면(--rm으로 사라짐) 할 일이 없다."""
    result = subprocess.run(['docker', 'container', 'inspect', '--format',
                             f'{{{{index .Config.Labels "{MYSQL_LABEL}"}}}}', name],
                            capture_output=True, text=True, timeout=60)
    if result.returncode:
        if 'No such' in result.stderr:
            return
        raise RuntimeError(f'cannot inspect temporary container {name}: {result.stderr[-500:]}')
    if result.stdout.strip() != owner:
        raise RuntimeError(f'container ownership differs for {name}; refusing to remove it')
    run(['docker', 'rm', '--force', '--volumes', name])


def mysql_client(name, client, statement):
    """컨테이너 안의 클라이언트로 TCP에서 문장을 실행한다. 준비 확인과 같은 TCP 경로를 쓴다.
    MariaDB 11.4 클라이언트는 TLS를 끈 서버에 소켓으로 붙으면 TLS를 요구하며 실패한다."""
    run(['docker', 'exec', name, client, '--no-defaults', '--protocol=TCP', '--host=127.0.0.1',
         '--port=3306', '-uroot', f'--database={MYSQL_DATABASE}', f'--execute={statement}'])


@contextmanager
def mysql_server(kind, entry, tls):
    """MySQL·MariaDB 서버 하나를 루프백 포트로 띄우고 포트를 넘긴다. 끝나면 지운다.

    root 비밀번호는 비워 둔다. 비밀번호가 없으면 평문 서버에서도 caching_sha2_password가
    RSA 교환 없이 끝나고, URL·argv에 비밀값이 실리지 않는다. 컨테이너는 루프백에만 열린다.
    """
    owner = secrets.token_hex(8)
    name = f'sg-native-tls-{kind}-{"tls" if tls else "plain"}-{owner}'
    options = [] if tls else MYSQL_PLAINTEXT_OPTIONS[kind]
    try:
        run(['docker', 'run', '--detach', '--rm', '--name', name, '--label', f'{MYSQL_LABEL}={owner}',
             '--memory', '1g', '--cpus', '1', '--publish', '127.0.0.1::3306',
             '--env', 'MYSQL_ALLOW_EMPTY_PASSWORD=yes', '--env', f'MYSQL_DATABASE={MYSQL_DATABASE}',
             entry['image'], '--performance-schema=OFF', '--skip-log-bin', *options], timeout=300)
        run([sys.executable, ROOT/'Scripts/wait-mysql-ready.py', name, entry['client']], timeout=240)
        mysql_client(name, entry['client'], 'CREATE TABLE tls_probe (id INT PRIMARY KEY)')
        yield published_port(name)
    finally:
        remove_owned_container(name, owner)


def mysql_url(host, port, parameters):
    """비밀번호 없는 MySQL 접속 URL을 만든다."""
    query = urllib.parse.urlencode(parameters, quote_via=urllib.parse.quote)
    return f'mysql://root@{host}:{port}/{MYSQL_DATABASE}' + (f'?{query}' if query else '')


def mysql_tls_cases(port):
    """TLS를 켠 서버의 사례. 네트워크 호스트의 기본 PREFERRED는 REQUIRED로 먼저 붙어 TLS로
    읽으므로 limitation이 없다. DISABLED는 모드 limitation을 남긴다."""
    return [
        ('preferred-network-host', mysql_url(NETWORK_HOST, port, {}), 0, None, EXPECT_NONE),
        ('required-network-host', mysql_url(NETWORK_HOST, port, {'ssl-mode': 'REQUIRED'}), 0, None, EXPECT_NONE),
        ('disabled-network-host', mysql_url(NETWORK_HOST, port, {'ssl-mode': 'DISABLED'}), 0, None,
         EXPECT_PLAINTEXT),
        ('preferred-loopback', mysql_url('127.0.0.1', port, {}), 0, None, EXPECT_NONE),
    ]


def mysql_plain_cases(port):
    """TLS를 끈 서버의 사례. 네트워크 호스트의 기본 PREFERRED는 서버가 TLS를 거절해 평문으로
    다시 붙으므로 거절 limitation을 남긴다."""
    return [
        ('preferred-network-host', mysql_url(NETWORK_HOST, port, {}), 0, None, EXPECT_REFUSED),
        ('required-without-server-tls', mysql_url('127.0.0.1', port, {'ssl-mode': 'REQUIRED'}), 2,
         'server does not support TLS', None),
        ('preferred-loopback', mysql_url('127.0.0.1', port, {}), 0, None, EXPECT_NONE),
    ]


def run_mysql_server(args, work, kind, entry, tls):
    """MySQL·MariaDB 서버 하나를 띄워 사례를 모두 돌린다. 사례 이름에 엔진과 서버 종류를 붙인다."""
    server, cases_for = ('tls', mysql_tls_cases) if tls else ('plain', mysql_plain_cases)
    with mysql_server(kind, entry, tls) as port:
        # 호스트·포트는 limitation에 실리면 안 된다. 비밀번호는 쓰지 않는다.
        hidden = {'stderr': [], 'limitation': [NETWORK_HOST, f':{port}']}
        return [check_case(args.engine, work, args.environment, (f'{kind}-{server}-{label}', *rest),
                           hidden, MYSQL_PROBE_TABLE)
                for label, *rest in cases_for(port)]


def verify_mysql(args, work):
    """MySQL·MariaDB마다 TLS 서버와 TLS를 끈 서버의 사례를 돌린 요약을 돌려준다."""
    require_docker()
    engines = json.loads(args.mysql_environments.read_text())['engines']
    return {kind: {'tls_server': run_mysql_server(args, work, kind, engines[kind], True),
                   'plaintext_server': run_mysql_server(args, work, kind, engines[kind], False)}
            for kind in ('mysql', 'mariadb')}


def verify(args, work):
    """두 클러스터(그리고 요청하면 MySQL·MariaDB 서버)의 사례를 모두 돌린 요약을 돌려준다.

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
    result = {'tls_server': tls, 'plaintext_server': plain}
    if args.mysql_environments:
        result |= verify_mysql(args, work)
    return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--engine', type=Path, required=True)
    parser.add_argument('--postgres-bin', type=Path,
                        default=Path(os.environ.get('PGBIN', '/opt/homebrew/opt/postgresql@16/bin')))
    parser.add_argument('--openssl', default='openssl')
    parser.add_argument('--mysql-environments', type=Path,
                        help='pinned MySQL/MariaDB images (Fixtures/accuracy/mysql-environments.json); '
                             'also verifies both engines in Docker')
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
