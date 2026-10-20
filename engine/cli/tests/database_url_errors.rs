//! 네이티브 reader가 읽지 못하는 접속 URL·접속 문자열이 비밀번호를 오류 메시지에 싣지 않는지 검증한다.
//!
//! 다른 수집기용 URL(`sqlserver://` 등)을 SQLite 경로로 재해석하면 URL 원문이
//! 경로 오류에 그대로 실린다. 퍼센트 디코딩된 형태라 원문 기준 로그 마스킹도 비켜 간다.
//!
//! PostgreSQL·MySQL URL은 가짜 서버로 접속의 첫 메시지를 본다. 기본 접속이 TLS를 먼저
//! 청하는지, `sslmode=disable`·`ssl-mode=DISABLED`가 그 요청을 끄는지, TLS를 요구한 접속이
//! 서버가 거절하면 종료 2로 끝나는지를 서버 없이 고정한다. 네트워크 호스트의 기본 접속은
//! 접속 순서까지 본다. TLS를 요구해 먼저 붙고, 서버가 TLS를 거절했을 때만 새 평문 접속으로
//! 다시 붙으며, 핸드셰이크가 실패하면 다시 붙지 않는다.

use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::process::{Command, Output};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

/// URL 하나를 scan한다. 산출물은 `root`의 graph.json이라 실패 시 남지 않았는지 볼 수 있다.
fn scan(root: &Path, url: &str) -> Output {
    Command::new(env!("CARGO_BIN_EXE_schemagraph"))
        .current_dir(root)
        .args(["scan", url, "-o", "graph.json"])
        .output()
        .unwrap()
}

/// 실패는 기존과 같은 종료 코드 2이고, 안내 문구는 있되 비밀값 조각은 없어야 한다.
fn assert_redacted_error(output: &Output, message: &str, secrets: &[&str]) {
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(2), "{output:?}");
    assert!(stderr.contains(message), "{stderr}");
    for secret in secrets {
        assert!(!stderr.contains(secret), "{secret} leaked: {stderr}");
    }
}

/// 다른 수집기용 URL은 출력 전에 스킴만 담은 오류로 거절된다. JVM 프로브용 `jdbc:` URL도
/// `scheme://` 형태가 아니지만 같은 거절을 받는다.
#[test]
fn unsupported_url_schemes_report_only_the_scheme() {
    let dir = tempfile::tempdir().unwrap();
    for (url, scheme) in [
        ("sqlserver://sa:S3cr%40t@db:1433/app", "sqlserver"),
        ("oracle://scott:tiger@db:1521/app", "oracle"),
        (
            "jdbc:sqlserver://db:1433;databaseName=app;user=sa;password=FAKEPW1",
            "jdbc",
        ),
    ] {
        assert_redacted_error(
            &scan(dir.path(), url),
            &format!("unsupported database URL scheme '{scheme}'"),
            &[
                "S3cr",
                "%40",
                "scott",
                "tiger",
                "@db",
                "FAKEPW1",
                "databaseName",
            ],
        );
    }
    assert!(!dir.path().join("graph.json").exists());
}

/// 스킴 검사를 비켜 SQLite 경로가 되는 입력과, 오류에 실릴 가린 표기와, 새면 안 되는 조각.
/// 슬래시 하나짜리 URL 오타와 스킴 없는 접속 문자열(libpq DSN, ODBC)이다.
const PATH_LIKE_CREDENTIALS: [(&str, &str, &str); 3] = [
    ("postgres:/u:pw@db/app", "postgres:/***@db/app", ":pw"),
    (
        "host=db user=u password=FAKEPW4 dbname=app",
        "host=db user=u password=*** dbname=app",
        "FAKEPW4",
    ),
    (
        "Server=db;Database=app;Uid=sa;Pwd=FAKEPW6;",
        "Server=db;Database=app;Uid=sa;Pwd=***;",
        "FAKEPW6",
    ),
];

/// 입력 경로 확인 실패 메시지는 URL 오타와 접속 문자열의 자격 증명을 가린다.
#[test]
fn unresolved_url_like_inputs_hide_credentials() {
    let dir = tempfile::tempdir().unwrap();
    for (input, shown, secret) in PATH_LIKE_CREDENTIALS {
        assert_redacted_error(
            &scan(dir.path(), input),
            &format!("cannot resolve input {shown}"),
            &[secret],
        );
    }
}

/// 경로 확인을 통과한 뒤 SQLite 접속이 실패해도 자격 증명을 싣지 않는다.
#[cfg(unix)]
#[test]
fn sqlite_open_errors_hide_credentials() {
    let dir = tempfile::tempdir().unwrap();
    for (input, shown, secret) in PATH_LIKE_CREDENTIALS {
        // 경로가 있어 입력 보호 검사는 통과하지만 SQLite가 열 수 없는 디렉터리다.
        fs::create_dir_all(dir.path().join(input)).unwrap();
        let output = scan(dir.path(), input);
        assert_redacted_error(&output, "database scan failed", &[secret]);
        assert_redacted_error(&output, shown, &[secret]);
    }
}

/// 스킴 검사가 베어 경로·`sqlite:`·`file:` URI·인코딩된 메모리 DB를 막지 않는다.
#[test]
fn local_sqlite_inputs_still_scan() {
    let dir = tempfile::tempdir().unwrap();
    // SQLite는 빈 파일을 빈 데이터베이스로 연다.
    fs::write(dir.path().join("empty.db"), b"").unwrap();
    let absolute = format!("file://{}", dir.path().join("empty.db").display());
    for url in [
        "empty.db",
        "sqlite:empty.db",
        "sqlite://empty.db?mode=ro",
        "file:empty.db?mode=ro",
        absolute.as_str(),
        "sqlite:%3Amemory%3A",
    ] {
        let output = scan(dir.path(), url);
        assert!(output.status.success(), "{url}: {output:?}");
    }
}

/// PostgreSQL SSLRequest: 길이 8과 요청 코드 80877103(`04 D2 16 2F`). 서버는 이 8바이트에
/// `S`(TLS 수락)나 `N`(거절) 한 바이트로 답한다.
const PG_SSL_REQUEST: [u8; 8] = [0x00, 0x00, 0x00, 0x08, 0x04, 0xD2, 0x16, 0x2F];

/// 평문 접속의 첫 메시지인 StartupMessage는 길이 바로 뒤에 프로토콜 버전 3.0을 싣는다.
const PG_PROTOCOL_3_0: [u8; 4] = [0x00, 0x03, 0x00, 0x00];

/// sqlx-mysql 0.8.6 디코더 테스트에 실린 MySQL 8.0.18의 실제 HandshakeV10 페이로드다.
/// 능력 비트 하위 2바이트가 `ff ff`라 CLIENT_SSL을 광고한다.
const MYSQL_HANDSHAKE_V10: &[u8] = b"\n8.0.18\x00\x19\x00\x00\x00\x114aB0c\x06g\x00\xff\xff\xff\x02\x00\xff\xc7\x15\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00tL\x03s\x0f[4\rl4. \x00caching_sha2_password\x00";

/// MySQL 능력 비트 CLIENT_SSL. 클라이언트가 이 비트를 켠 32바이트 SSLRequest를 보내면
/// 인증 응답 전에 TLS로 올라간다는 뜻이다.
const MYSQL_CLIENT_SSL: u32 = 0x0800;

/// MySQL SSLRequest 페이로드 길이(능력 4, 최대 패킷 4, 문자 집합 1, 예약 23바이트).
const MYSQL_SSL_REQUEST_LEN: usize = 32;

/// 가짜 서버가 접속과 첫 메시지를 기다리는 상한. 클라이언트가 오지 않아도 테스트가 멈추지 않는다.
const FAKE_SERVER_TIMEOUT: Duration = Duration::from_secs(30);

/// reader가 네트워크를 지나는 접속으로 분류하는 주소. 루프백 주소가 아니지만 Linux·macOS
/// 커널은 0.0.0.0으로 가는 접속을 자기 호스트로 보내므로, 루프백 가짜 서버로 원격 경로를 탄다.
const NETWORK_HOST: &str = "0.0.0.0";

/// 가짜 서버가 접속 하나와 나누는 교환. 클라이언트가 보낸 바이트를 돌려준다.
type Exchange = Box<dyn FnOnce(&mut TcpStream) -> Vec<u8> + Send>;

/// libpq 환경 변수(`PGSSLMODE` 등)를 지우고 scan한다.
///
/// sqlx는 이 변수로 기본 sslmode·호스트를 바꾼다. 개발자 환경과 무관하게 URL만으로
/// 접속 방식이 정해져야 첫 메시지 판정이 고정된다.
fn scan_without_libpq_env(root: &Path, url: &str) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_schemagraph"));
    for (name, _) in std::env::vars_os() {
        if name.to_string_lossy().starts_with("PG") {
            command.env_remove(name);
        }
    }
    command
        .current_dir(root)
        .args(["scan", url, "-o", "graph.json"])
        .output()
        .unwrap()
}

/// 루프백에 가짜 서버를 띄운다. 접속 하나를 받아 `exchange`를 돌리고 그 결과(클라이언트가
/// 보낸 바이트)를 핸들로 돌려준다. 끝나면 소켓을 닫으므로 클라이언트는 다음 단계에서
/// 오류로 끝난다.
fn fake_server<F>(exchange: F) -> (u16, JoinHandle<Vec<u8>>)
where
    F: FnOnce(&mut TcpStream) -> Vec<u8> + Send + 'static,
{
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = thread::spawn(move || {
        accept_within(&listener, FAKE_SERVER_TIMEOUT)
            .map(|mut stream| exchange(&mut stream))
            .unwrap_or_default()
    });
    (port, server)
}

/// 루프백에 가짜 서버를 띄워 들어오는 접속마다 `exchanges`를 차례로 돌린다. 교환이 바닥난
/// 뒤의 접속은 첫 8바이트만 받는다. scan이 끝났다고 `finished`가 켜진 뒤 대기열이 비면 멈추고,
/// 접속마다 받은 바이트를 순서대로 돌려준다. 접속 수로 reader가 다시 붙었는지를 판정한다.
fn fake_server_sessions(
    exchanges: Vec<Exchange>,
    finished: Arc<AtomicBool>,
) -> (u16, JoinHandle<Vec<Vec<u8>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    listener.set_nonblocking(true).unwrap();
    let server = thread::spawn(move || {
        let mut exchanges = exchanges.into_iter();
        let mut sessions = Vec::new();
        let deadline = Instant::now() + FAKE_SERVER_TIMEOUT;
        while let Some(mut stream) = next_session(&listener, &finished, deadline) {
            let exchange = exchanges.next().unwrap_or_else(|| Box::new(first_message));
            sessions.push(exchange(&mut stream));
        }
        sessions
    });
    (port, server)
}

/// 다음 접속을 받는다. `finished`를 accept보다 먼저 읽으므로, scan이 끝난 뒤에 본 빈
/// 대기열이면 더 올 접속이 없다(None). 상한이 지나도 None이다.
fn next_session(
    listener: &TcpListener,
    finished: &AtomicBool,
    deadline: Instant,
) -> Option<TcpStream> {
    loop {
        let done = finished.load(Ordering::SeqCst) || Instant::now() >= deadline;
        match listener.accept() {
            Ok((stream, _)) => return Some(blocking_with_timeout(stream)),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                if done {
                    return None;
                }
                thread::sleep(Duration::from_millis(10));
            }
            Err(error) => panic!("fake server could not accept a connection: {error}"),
        }
    }
}

/// 예상하지 않은 접속의 교환. 무엇을 보냈는지 실패 메시지에 보이도록 첫 8바이트를 받는다.
fn first_message(stream: &mut TcpStream) -> Vec<u8> {
    read_up_to(stream, PG_SSL_REQUEST.len())
}

/// 가짜 서버 앞에서 scan을 돌리고, scan이 끝난 뒤 서버가 접속마다 받은 바이트를 모은다.
fn scan_against(
    exchanges: Vec<Exchange>,
    scan: impl FnOnce(u16) -> Output,
) -> (Output, Vec<Vec<u8>>) {
    let finished = Arc::new(AtomicBool::new(false));
    let (port, server) = fake_server_sessions(exchanges, Arc::clone(&finished));
    let output = scan(port);
    finished.store(true, Ordering::SeqCst);
    (output, server.join().unwrap())
}

/// 상한 안에 들어온 접속 하나를 받는다. 클라이언트가 오지 않으면 None이다.
fn accept_within(listener: &TcpListener, timeout: Duration) -> Option<TcpStream> {
    listener.set_nonblocking(true).unwrap();
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        match listener.accept() {
            Ok((stream, _)) => return Some(blocking_with_timeout(stream)),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(10));
            }
            Err(error) => panic!("fake server could not accept a connection: {error}"),
        }
    }
    None
}

/// 받은 소켓을 블로킹으로 되돌리고 읽기 상한을 건다. 플랫폼에 따라 받은 소켓이 리스너의
/// 논블로킹 설정을 물려받기 때문이다.
fn blocking_with_timeout(stream: TcpStream) -> TcpStream {
    stream.set_nonblocking(false).unwrap();
    stream.set_read_timeout(Some(FAKE_SERVER_TIMEOUT)).unwrap();
    stream
}

/// 클라이언트가 보낸 바이트를 `length`까지 읽는다. 클라이언트가 끊거나 상한에 걸리면 그때까지
/// 받은 바이트로 판정한다 — 단언이 기대값과의 차이를 보여 준다.
fn read_up_to(stream: &mut TcpStream, length: usize) -> Vec<u8> {
    let mut received = vec![0; length];
    let mut filled = 0;
    while filled < length {
        match stream.read(&mut received[filled..]) {
            Ok(0) => break,
            Ok(count) => filled += count,
            Err(error) => {
                eprintln!("fake server stopped reading after {filled} bytes: {error}");
                break;
            }
        }
    }
    received.truncate(filled);
    received
}

/// PostgreSQL 첫 메시지의 앞 8바이트를 읽고, `reply`가 있으면 SSLRequest 응답으로 보낸다.
fn postgres_exchange(reply: Option<u8>) -> impl FnOnce(&mut TcpStream) -> Vec<u8> + Send {
    move |stream| {
        let first = read_up_to(stream, PG_SSL_REQUEST.len());
        if let Some(byte) = reply {
            stream
                .write_all(&[byte])
                .expect("send the SSLRequest reply");
        }
        first
    }
}

/// CLIENT_SSL을 광고하는 HandshakeV10을 보내고 클라이언트의 첫 패킷을 읽는다.
fn mysql_exchange(stream: &mut TcpStream) -> Vec<u8> {
    mysql_exchange_with(MYSQL_HANDSHAKE_V10.to_vec())(stream)
}

/// HandshakeV10 `handshake`를 보내고 클라이언트의 첫 패킷(4바이트 머리와 페이로드)을 읽는다.
/// 클라이언트가 답하지 않고 접속을 닫으면 빈 바이트다.
fn mysql_exchange_with(handshake: Vec<u8>) -> impl FnOnce(&mut TcpStream) -> Vec<u8> + Send {
    move |stream| {
        stream
            .write_all(&mysql_packet(0, &handshake))
            .expect("send HandshakeV10");
        let mut packet = read_up_to(stream, 4);
        if let [low, middle, high, _] = packet[..] {
            let length = u32::from_le_bytes([low, middle, high, 0]) as usize;
            packet.extend(read_up_to(stream, length.min(4096)));
        }
        packet
    }
}

/// CLIENT_SSL을 광고하지 않는 HandshakeV10. TLS를 끈 서버나, 그 비트를 지운 중간자가 보내는
/// 모양이다. 프로토콜 버전 1바이트, NUL로 끝나는 서버 버전, 접속 id 4바이트, 인증 자료 8바이트,
/// 채움 1바이트 뒤에 능력 비트 하위 2바이트(리틀 엔디언)가 온다.
fn mysql_handshake_without_tls() -> Vec<u8> {
    let mut handshake = MYSQL_HANDSHAKE_V10.to_vec();
    let version_end = 1 + handshake[1..].iter().position(|&byte| byte == 0).unwrap();
    let at = version_end + 1 + 4 + 8 + 1;
    let lower = u16::from_le_bytes([handshake[at], handshake[at + 1]]);
    assert_ne!(
        u32::from(lower) & MYSQL_CLIENT_SSL,
        0,
        "the sample must offer TLS"
    );
    let without_tls = lower & !(MYSQL_CLIENT_SSL as u16);
    handshake[at..at + 2].copy_from_slice(&without_tls.to_le_bytes());
    handshake
}

/// MySQL 패킷 머리(3바이트 길이와 1바이트 순번)를 페이로드 앞에 붙인다.
fn mysql_packet(sequence: u8, payload: &[u8]) -> Vec<u8> {
    let length = u32::try_from(payload.len()).unwrap().to_le_bytes();
    let mut packet = vec![length[0], length[1], length[2], sequence];
    packet.extend_from_slice(payload);
    packet
}

/// 클라이언트 첫 패킷의 순번과 페이로드 앞 4바이트(능력 비트, 리틀 엔디언)를 읽는다.
fn mysql_client_capabilities(packet: &[u8]) -> u32 {
    assert!(packet.len() >= 8, "short MySQL client packet: {packet:?}");
    assert_eq!(
        packet[3], 1,
        "the client answers the handshake with sequence 1"
    );
    u32::from_le_bytes([packet[4], packet[5], packet[6], packet[7]])
}

/// 기본 PostgreSQL URL(`sslmode=prefer`)은 StartupMessage 전에 SSLRequest를 보낸다. TLS 없이
/// 빌드되면 prefer가 요청 없이 평문으로 시작해 인증 교환과 카탈로그가 그대로 오간다.
#[test]
fn default_postgres_url_requests_tls_first() {
    let dir = tempfile::tempdir().unwrap();
    let (port, server) = fake_server(postgres_exchange(None));
    let url = format!("postgres://reader:FAKEPW7@127.0.0.1:{port}/app");
    let output = scan_without_libpq_env(dir.path(), &url);
    assert_eq!(server.join().unwrap(), PG_SSL_REQUEST);
    assert_redacted_error(&output, "database scan failed", &["FAKEPW7"]);
}

/// `sslmode=disable`이면 TLS를 청하지 않고 프로토콜 3.0 StartupMessage로 바로 시작한다.
#[test]
fn postgres_sslmode_disable_starts_without_tls() {
    let dir = tempfile::tempdir().unwrap();
    let (port, server) = fake_server(postgres_exchange(None));
    let url = format!("postgres://reader:FAKEPW7@127.0.0.1:{port}/app?sslmode=disable");
    let output = scan_without_libpq_env(dir.path(), &url);
    let first = server.join().unwrap();
    assert_eq!(first.len(), 8, "{first:?}");
    assert_eq!(first[4..], PG_PROTOCOL_3_0, "{first:?}");
    assert_redacted_error(&output, "database scan failed", &["FAKEPW7"]);
}

/// `sslmode=require`는 서버가 TLS를 거절하면(`N`) 평문으로 내려가지 않고 종료 2로 끝난다.
/// TLS 없이 빌드되면 SSLRequest조차 보내지 못하고 "built without TLS support"로 끝난다.
#[test]
fn postgres_sslmode_require_fails_when_the_server_refuses_tls() {
    let dir = tempfile::tempdir().unwrap();
    let (port, server) = fake_server(postgres_exchange(Some(b'N')));
    let url = format!("postgres://reader:FAKEPW7@127.0.0.1:{port}/app?sslmode=require");
    let output = scan_without_libpq_env(dir.path(), &url);
    assert_eq!(server.join().unwrap(), PG_SSL_REQUEST);
    assert_redacted_error(&output, "server does not support TLS", &["FAKEPW7"]);
}

/// 기본 MySQL URL(`ssl-mode=PREFERRED`)은 서버가 CLIENT_SSL을 광고하면 인증 응답 전에
/// CLIENT_SSL을 켠 SSLRequest를 보낸다. TLS 없이 빌드되면 그 비트를 지우고 평문으로 인증한다.
#[test]
fn default_mysql_url_requests_tls_when_the_server_offers_it() {
    let dir = tempfile::tempdir().unwrap();
    let (port, server) = fake_server(mysql_exchange);
    let url = format!("mysql://reader:FAKEPW8@127.0.0.1:{port}/app");
    let output = scan(dir.path(), &url);
    let packet = server.join().unwrap();
    assert_ne!(mysql_client_capabilities(&packet) & MYSQL_CLIENT_SSL, 0);
    assert_eq!(packet.len(), 4 + MYSQL_SSL_REQUEST_LEN, "{packet:?}");
    assert_redacted_error(&output, "database scan failed", &["FAKEPW8"]);
}

/// `ssl-mode=DISABLED`면 SSLRequest 없이 사용자 이름이 담긴 HandshakeResponse를 바로 보낸다.
#[test]
fn mysql_ssl_mode_disabled_answers_without_tls() {
    let dir = tempfile::tempdir().unwrap();
    let (port, server) = fake_server(mysql_exchange);
    let url = format!("mysql://reader:FAKEPW8@127.0.0.1:{port}/app?ssl-mode=DISABLED");
    let output = scan(dir.path(), &url);
    let packet = server.join().unwrap();
    assert_eq!(mysql_client_capabilities(&packet) & MYSQL_CLIENT_SSL, 0);
    let after_fixed_fields = &packet[4 + MYSQL_SSL_REQUEST_LEN..];
    assert!(after_fixed_fields.starts_with(b"reader\0"), "{packet:?}");
    assert_redacted_error(&output, "database scan failed", &["FAKEPW8"]);
}

/// 네트워크 호스트의 기본 PostgreSQL URL은 TLS를 요구해 먼저 붙는다. 서버가 거절하면(`N`) 그
/// 접속에 아무것도 더 보내지 않고 닫은 뒤, TLS를 청하지 않는 새 접속(StartupMessage)으로 다시
/// 붙는다. 거절한 접속에서 평문으로 이어 가지 않으므로, 이후 연결이 평문이라는 사실을 서버에
/// 묻지 않고 reader가 안다(서버의 답은 평문 구간에서 위조될 수 있다).
#[test]
fn remote_default_postgres_url_reconnects_without_tls_after_the_server_refuses_it() {
    let dir = tempfile::tempdir().unwrap();
    let refuse: Exchange = Box::new(|stream| {
        let mut received = postgres_exchange(Some(b'N'))(stream);
        // 거절 뒤 같은 접속으로 보낸 바이트가 있으면 함께 남긴다 — 있으면 안 된다.
        received.extend(read_up_to(stream, PG_SSL_REQUEST.len()));
        received
    });
    let exchanges = vec![refuse, Box::new(postgres_exchange(None)) as Exchange];
    let (output, sessions) = scan_against(exchanges, |port| {
        let url = format!("postgres://reader:FAKEPW7@{NETWORK_HOST}:{port}/app");
        scan_without_libpq_env(dir.path(), &url)
    });
    assert_eq!(sessions.len(), 2, "{sessions:?}");
    assert_eq!(sessions[0], PG_SSL_REQUEST, "{sessions:?}");
    assert_eq!(sessions[1].len(), 8, "{sessions:?}");
    assert_eq!(sessions[1][4..], PG_PROTOCOL_3_0, "{sessions:?}");
    assert_redacted_error(&output, "database scan failed", &["FAKEPW7"]);
}

/// 서버가 TLS를 받겠다고 한 뒤(`S`) 핸드셰이크가 실패하면 평문으로 다시 붙지 않고 종료 2로
/// 끝난다. 평문 재접속은 서버가 TLS를 거절했을 때뿐이다(sqlx 자체 prefer와 같다).
#[test]
fn remote_default_postgres_url_does_not_retry_without_tls_after_a_failed_handshake() {
    let dir = tempfile::tempdir().unwrap();
    let exchanges = vec![Box::new(postgres_exchange(Some(b'S'))) as Exchange];
    let (output, sessions) = scan_against(exchanges, |port| {
        let url = format!("postgres://reader:FAKEPW7@{NETWORK_HOST}:{port}/app");
        scan_without_libpq_env(dir.path(), &url)
    });
    assert_eq!(sessions, [PG_SSL_REQUEST.to_vec()]);
    assert_redacted_error(&output, "database scan failed", &["FAKEPW7"]);
}

/// 네트워크 호스트의 기본 MySQL URL도 TLS를 요구해 먼저 붙는다. 서버가 CLIENT_SSL을 광고하지
/// 않으면 인증 응답 없이 그 접속을 닫고, CLIENT_SSL을 끈 새 접속으로 다시 붙어 평문으로 인증한다.
#[test]
fn remote_default_mysql_url_reconnects_without_tls_when_the_server_lacks_it() {
    let dir = tempfile::tempdir().unwrap();
    let handshake = mysql_handshake_without_tls();
    let exchanges: Vec<Exchange> = vec![
        Box::new(mysql_exchange_with(handshake.clone())),
        Box::new(mysql_exchange_with(handshake)),
    ];
    let (output, sessions) = scan_against(exchanges, |port| {
        scan(
            dir.path(),
            &format!("mysql://reader:FAKEPW8@{NETWORK_HOST}:{port}/app"),
        )
    });
    assert_eq!(sessions.len(), 2, "{sessions:?}");
    assert!(
        sessions[0].is_empty(),
        "answered without TLS: {:?}",
        sessions[0]
    );
    let packet = &sessions[1];
    assert_eq!(mysql_client_capabilities(packet) & MYSQL_CLIENT_SSL, 0);
    let after_fixed_fields = &packet[4 + MYSQL_SSL_REQUEST_LEN..];
    assert!(after_fixed_fields.starts_with(b"reader\0"), "{packet:?}");
    assert_redacted_error(&output, "database scan failed", &["FAKEPW8"]);
}
