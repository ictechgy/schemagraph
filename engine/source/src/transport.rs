//! 네이티브 reader 연결의 전송 암호화 판정 — 네트워크를 지난 평문 연결을 limitation으로 남긴다.
//!
//! 암호화되지 않은 원격 연결로 읽은 카탈로그는 경로상 누구든 엿보거나 바꿀 수 있었다는
//! 뜻이다. 그 사실을 문서에 남겨 소비자가 결과를 얼마나 믿을지 스스로 정하게 한다. Unix
//! 소켓과 루프백 연결은 네트워크를 지나지 않으므로 남기지 않는다. 로컬 fixture마다 붙는
//! 상투적 경보는 읽히지 않기 때문이다(AGENTS.md "limitations를 장식으로 쓰지 마세요").
//!
//! 판정은 클라이언트가 직접 아는 사실로만 한다. 서버에 세션의 TLS 여부를 묻는 답(pg_stat_ssl,
//! Ssl_cipher)은 평문 연결에서 위조될 수 있고, 클라이언트 쪽 TLS만 거절한 중간자 뒤에서는
//! 중간자와 서버 사이 구간을 말해 경보를 지운다. 그래서 네트워크를 지나는 prefer 연결은 sqlx
//! 자체 prefer에 맡기지 않고 TLS를 요구해 먼저 열며, 서버가 거절했을 때만 평문으로 다시 연다.

use std::future::Future;
use std::net::IpAddr;

use crate::document::CatalogDocument;

/// 평문 원격 연결을 알리는 limitation의 접두사. 소비자가 문구 대신 이 접두사로 찾는다.
pub(crate) const NOT_ENCRYPTED: &str = "connection-not-encrypted:";

/// sqlx 0.8.6이 TLS를 요구한 접속에서 서버가 TLS를 거절했을 때 내는 `Error::Tls` 문구.
/// PostgreSQL은 SSLRequest에 `N`을 받았을 때, MySQL은 서버 핸드셰이크에 CLIENT_SSL이 없을
/// 때다(sqlx-postgres·sqlx-mysql `connection/tls.rs`).
///
/// 같은 변형의 다른 오류(서버 이름으로 쓸 수 없는 호스트, 클라이언트 인증서 오류)는 서버의
/// 거절이 아니므로 문구로 가른다. sqlx를 올려 문구가 바뀌면 거절도 접속 실패(종료 2)가 된다.
/// 평문으로 내려가는 쪽으로는 틀리지 않으며, 가짜 서버 테스트가 그 변화를 잡는다.
const SERVER_REFUSED_TLS: &str = "server does not support TLS";

/// 접속 모드가 TLS를 다루는 방식. sqlx 0.8.6의 PostgreSQL·MySQL 모드를 세 갈래로 접는다.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TlsPolicy {
    /// TLS를 시도하지 않는다(PG disable·allow, MySQL DISABLED). 연결은 늘 평문이다.
    Never,
    /// 서버가 받으면 TLS, 아니면 평문이다(PG prefer, MySQL PREFERRED). 네트워크를 지나는
    /// 연결은 TLS를 요구해 열어 봐야 어느 쪽인지 안다.
    Prefer,
    /// TLS가 아니면 sqlx가 접속을 거절한다(require·verify-ca·verify-full과 MySQL 대응 모드).
    Required,
}

/// reader가 풀을 여는 방식. reader마다 자기 sslmode로 옮긴다.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ConnectMode {
    /// URL(과 libpq 환경 변수)이 정한 모드 그대로 연다.
    AsGiven,
    /// TLS를 요구한다(PG require, MySQL REQUIRED). prefer처럼 인증서는 검증하지 않는다.
    RequireTls,
    /// TLS를 시도하지 않는다(PG disable, MySQL DISABLED).
    Plaintext,
}

/// 카탈로그를 읽은 연결의 전송. 풀의 연결은 모두 같은 모드로 열리므로 첫 접속에서 정해진다.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Transport {
    /// Unix 소켓이나 루프백이라 네트워크를 지나지 않는다.
    Local,
    /// 모든 연결이 TLS다. 모드가 TLS를 요구하므로 sqlx가 연결마다 강제한다.
    Encrypted,
    /// 모드가 TLS를 시도하지 않아 평문으로 읽었다.
    Plaintext,
    /// prefer에서 서버가 TLS를 거절해 평문으로 다시 열어 읽었다.
    TlsRefused,
}

/// 접속 대상이 네트워크를 지나지 않는지 본다. Unix 소켓과 루프백 주소(127.0.0.0/8, ::1)와
/// 이름 `localhost`만 참이다.
///
/// 다른 이름의 해석 결과는 보지 않는다. 접속 전에 어느 주소로 붙을지 확정할 수 없으니,
/// 경보를 끄는 쪽으로 추측하지 않는다.
pub(crate) fn is_local_endpoint(host: &str, uses_socket: bool) -> bool {
    uses_socket || is_loopback_host(host)
}

/// 호스트 문자열이 루프백인지 본다. url 크레이트는 IPv6 주소를 대괄호째 돌려주고 sqlx도
/// 접속 직전에 대괄호를 벗기므로 같게 벗긴다.
fn is_loopback_host(host: &str) -> bool {
    let host = host.trim_matches(['[', ']']);
    host.eq_ignore_ascii_case("localhost") || host.parse().is_ok_and(is_loopback_address)
}

/// IPv4-mapped IPv6(`::ffff:127.0.0.1`)도 IPv4 루프백으로 본다. 커널이 같은 루프백으로 보낸다.
fn is_loopback_address(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(v4) => v4.is_loopback(),
        IpAddr::V6(v6) => {
            v6.is_loopback() || v6.to_ipv4_mapped().is_some_and(|v4| v4.is_loopback())
        }
    }
}

/// 접속 전에 정해지는 판정 재료: 대상이 네트워크를 지나는지와 URL의 TLS 모드.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TransportCheck {
    local: bool,
    policy: TlsPolicy,
}

impl TransportCheck {
    /// 접속 대상 종류와 모드로 판정을 준비한다.
    pub(crate) fn new(local: bool, policy: TlsPolicy) -> Self {
        Self { local, policy }
    }

    /// 접속 전에 전송을 아는지 본다. 네트워크를 지나는 prefer만 모른다 — 서버가 TLS를
    /// 받을지는 시도해 봐야 안다. 다른 모드는 TLS를 시도하지 않거나, TLS가 아니면 접속하지 않는다.
    fn known(self) -> Option<Transport> {
        match (self.local, self.policy) {
            (true, _) => Some(Transport::Local),
            (false, TlsPolicy::Never) => Some(Transport::Plaintext),
            (false, TlsPolicy::Required) => Some(Transport::Encrypted),
            (false, TlsPolicy::Prefer) => None,
        }
    }

    /// TLS를 요구해 먼저 열어 봐야 하는지 본다(reader의 URL 분류 테스트용).
    #[cfg(test)]
    pub(crate) fn negotiates_tls(self) -> bool {
        self.known().is_none()
    }

    /// `connect`로 풀을 열고 카탈로그를 읽을 연결의 전송을 함께 돌려준다. 전송을 아는 모드는
    /// URL대로 한 번 연다. 네트워크를 지나는 prefer는 `negotiate`가 정한다.
    ///
    /// `connect`는 받은 방식으로 풀을 연다. 풀의 첫 접속이 곧 시도라서 sqlx 풀의 접속 재시도
    /// (서버 기동 중의 접속 거절 등)가 그대로 살고, 시도용 연결을 따로 열지 않는다.
    pub(crate) async fn open<P, F, Fut>(self, connect: F) -> Result<(P, Transport), sqlx::Error>
    where
        F: Fn(ConnectMode) -> Fut,
        Fut: Future<Output = Result<P, sqlx::Error>>,
    {
        match self.known() {
            Some(transport) => Ok((connect(ConnectMode::AsGiven).await?, transport)),
            None => negotiate(connect).await,
        }
    }
}

/// 네트워크를 지나는 prefer 연결을 연다. TLS를 요구해 먼저 열고, 서버가 TLS를 거절했을 때만
/// 평문으로 다시 연다. 어느 쪽이든 풀의 모든 연결이 같은 모드라서, 이후 연결도 클라이언트가
/// 아는 대로다 — TLS 풀은 sqlx가 연결마다 TLS를 강제하고, 평문 풀은 TLS를 시도하지 않는다.
async fn negotiate<P, F, Fut>(connect: F) -> Result<(P, Transport), sqlx::Error>
where
    F: Fn(ConnectMode) -> Fut,
    Fut: Future<Output = Result<P, sqlx::Error>>,
{
    match after_tls_attempt(connect(ConnectMode::RequireTls).await)? {
        TlsAttempt::Accepted(pool) => Ok((pool, Transport::Encrypted)),
        TlsAttempt::Refused => Ok((
            connect(ConnectMode::Plaintext).await?,
            Transport::TlsRefused,
        )),
    }
}

/// TLS를 요구한 첫 시도에서 고른 다음 걸음.
#[derive(Debug, PartialEq, Eq)]
enum TlsAttempt<P> {
    /// 서버가 TLS를 받았다. 연 풀을 그대로 쓴다.
    Accepted(P),
    /// 서버(또는 경로상의 누군가)가 TLS를 거절했다. 평문으로 다시 연다.
    Refused,
}

/// TLS를 요구한 시도의 결과를 다음 걸음으로 바꾼다. 서버의 TLS 거절만 평문 재시도로 이어진다.
///
/// 인증·네트워크 오류, 서버가 TLS를 받은 뒤의 핸드셰이크 실패(sqlx에서는 `Error::Io`),
/// 클라이언트 TLS 설정 오류는 그대로 접속 실패다. sqlx 자체 prefer도 이때 평문으로 다시
/// 붙지 않았고, 평문으로 내려가면 사용자가 준 TLS 설정을 무시하게 된다.
fn after_tls_attempt<P>(attempt: Result<P, sqlx::Error>) -> Result<TlsAttempt<P>, sqlx::Error> {
    match attempt {
        Ok(pool) => Ok(TlsAttempt::Accepted(pool)),
        Err(error) if refuses_tls(&error) => Ok(TlsAttempt::Refused),
        Err(error) => Err(error),
    }
}

/// 서버가 TLS를 거절해 난 오류인지 본다(`SERVER_REFUSED_TLS`).
fn refuses_tls(error: &sqlx::Error) -> bool {
    matches!(error, sqlx::Error::Tls(cause) if cause.to_string() == SERVER_REFUSED_TLS)
}

impl Transport {
    /// 문서 limitations 맨 앞에 전송 limitation을 둔다. 접속 단계의 사실이라 수집 단계의
    /// 한계보다 앞선다. 수집 범위가 아니라 전송 사실이므로 catalog_complete는 건드리지 않는다.
    pub(crate) fn record_limitation(self, doc: &mut CatalogDocument, remedy: &str) {
        if let Some(limitation) = connection_limitation(self, remedy) {
            doc.limitations.insert(0, limitation);
        }
    }
}

/// 전송으로 limitation을 정한다. 문구에는 호스트·사용자·비밀번호를 싣지 않고, 원인과 해결
/// 방향(`remedy`)을 함께 싣는다.
fn connection_limitation(transport: Transport, remedy: &str) -> Option<String> {
    let cause = match transport {
        Transport::Local | Transport::Encrypted => return None,
        Transport::Plaintext => "the catalog was read over an unencrypted connection",
        Transport::TlsRefused => {
            "the server did not accept TLS, so the catalog was read over an unencrypted connection"
        }
    };
    Some(format!("{NOT_ENCRYPTED} {cause}; {remedy}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    const REMEDY: &str = "add sslmode=verify-full to the URL";

    #[test]
    fn loopback_hosts_and_sockets_are_local() {
        for host in [
            "127.0.0.1",
            "127.255.255.254",
            "::1",
            "[::1]",
            "[::ffff:127.0.0.1]",
            "localhost",
            "LocalHost",
        ] {
            assert!(is_local_endpoint(host, false), "{host}");
        }
        assert!(is_local_endpoint("db.example", true));
        assert!(is_local_endpoint("/var/run/postgresql", true));
    }

    /// 이름 해석 결과를 추측하지 않는다 — 루프백으로 풀릴 수 있는 이름이라도 정규 표기가 아니면
    /// 네트워크로 본다.
    #[test]
    fn other_hosts_cross_the_network() {
        for host in [
            "db.example",
            "10.0.0.5",
            "0.0.0.0",
            "[2001:db8::1]",
            "[::ffff:10.0.0.5]",
            "localhost.example",
            "127.0.0.1.example",
            "127.1",
            "",
        ] {
            assert!(!is_local_endpoint(host, false), "{host}");
        }
    }

    /// 로컬 연결은 모드와 무관하게 limitation이 없고, 원격 연결은 prefer만 TLS 협상으로 정한다.
    #[test]
    fn the_mode_decides_every_connection_except_remote_prefer() {
        for policy in [TlsPolicy::Never, TlsPolicy::Prefer, TlsPolicy::Required] {
            assert_eq!(
                TransportCheck::new(true, policy).known(),
                Some(Transport::Local)
            );
        }
        let remote = |policy| TransportCheck::new(false, policy).known();
        assert_eq!(remote(TlsPolicy::Never), Some(Transport::Plaintext));
        assert_eq!(remote(TlsPolicy::Required), Some(Transport::Encrypted));
        assert_eq!(remote(TlsPolicy::Prefer), None);
        assert!(TransportCheck::new(false, TlsPolicy::Prefer).negotiates_tls());
        assert!(!TransportCheck::new(true, TlsPolicy::Prefer).negotiates_tls());
    }

    /// 평문 전송만 limitation을 남긴다. 모드로 끈 평문은 기존 문구 그대로이고, 서버가 거절한
    /// 평문은 그 원인을 앞에 싣는다.
    #[test]
    fn only_plaintext_transports_carry_the_limitation() {
        assert_eq!(connection_limitation(Transport::Local, REMEDY), None);
        assert_eq!(connection_limitation(Transport::Encrypted, REMEDY), None);
        assert_eq!(
            connection_limitation(Transport::Plaintext, REMEDY).unwrap(),
            "connection-not-encrypted: the catalog was read over an unencrypted connection; add sslmode=verify-full to the URL"
        );
        assert_eq!(
            connection_limitation(Transport::TlsRefused, REMEDY).unwrap(),
            "connection-not-encrypted: the server did not accept TLS, so the catalog was read over an unencrypted connection; add sslmode=verify-full to the URL"
        );
    }

    /// sqlx가 서버의 TLS 거절에 내는 오류.
    fn refusal() -> sqlx::Error {
        sqlx::Error::Tls(SERVER_REFUSED_TLS.into())
    }

    /// 결정 함수: 성공은 TLS 풀을 그대로 쓰고, 서버의 TLS 거절만 평문 재시도이며, 나머지
    /// 오류(같은 `Error::Tls`의 다른 원인, 핸드셰이크 I/O 실패, 시간 초과)는 접속 실패다.
    #[test]
    fn only_a_tls_refusal_leads_to_a_plaintext_retry() {
        assert_eq!(after_tls_attempt(Ok(7)).unwrap(), TlsAttempt::Accepted(7));
        assert_eq!(
            after_tls_attempt::<()>(Err(refusal())).unwrap(),
            TlsAttempt::Refused
        );
        let failures = [
            sqlx::Error::Tls("invalid dns name".into()),
            sqlx::Error::Io(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "handshake",
            )),
            sqlx::Error::PoolTimedOut,
        ];
        for failure in failures {
            let shown = failure.to_string();
            let error = after_tls_attempt::<()>(Err(failure)).unwrap_err();
            assert_eq!(error.to_string(), shown);
        }
    }

    /// `open` 흉내: 받은 방식을 기록하고, 방식마다 준비한 결과로 답한다.
    struct Recorder {
        modes: RefCell<Vec<ConnectMode>>,
        answer: fn(ConnectMode) -> Result<ConnectMode, sqlx::Error>,
    }

    impl Recorder {
        fn new(answer: fn(ConnectMode) -> Result<ConnectMode, sqlx::Error>) -> Self {
            Self {
                modes: RefCell::new(Vec::new()),
                answer,
            }
        }

        /// 풀 대신 연 방식을 돌려준다. 어느 방식의 풀이 남았는지 결과로 보인다.
        fn open(&self, mode: ConnectMode) -> std::future::Ready<Result<ConnectMode, sqlx::Error>> {
            self.modes.borrow_mut().push(mode);
            std::future::ready((self.answer)(mode))
        }

        fn modes(&self) -> Vec<ConnectMode> {
            self.modes.borrow().clone()
        }
    }

    /// 모드가 전송을 정하는 연결은 URL대로 한 번만 연다.
    #[tokio::test]
    async fn known_transports_open_once_as_given() {
        let cases = [
            (true, TlsPolicy::Prefer, Transport::Local),
            (false, TlsPolicy::Never, Transport::Plaintext),
            (false, TlsPolicy::Required, Transport::Encrypted),
        ];
        for (local, policy, transport) in cases {
            let recorder = Recorder::new(Ok);
            let opened = TransportCheck::new(local, policy)
                .open(|mode| recorder.open(mode))
                .await
                .unwrap();
            assert_eq!(opened, (ConnectMode::AsGiven, transport));
            assert_eq!(recorder.modes(), [ConnectMode::AsGiven]);
        }
    }

    /// 원격 prefer는 TLS를 요구해 먼저 열고, 받아 주면 그 풀을 쓴다.
    #[tokio::test]
    async fn remote_prefer_keeps_the_tls_pool_when_the_server_accepts() {
        let recorder = Recorder::new(Ok);
        let opened = TransportCheck::new(false, TlsPolicy::Prefer)
            .open(|mode| recorder.open(mode))
            .await
            .unwrap();
        assert_eq!(opened, (ConnectMode::RequireTls, Transport::Encrypted));
        assert_eq!(recorder.modes(), [ConnectMode::RequireTls]);
    }

    /// 서버가 TLS를 거절하면 평문으로 다시 열고 그 사실을 전송으로 남긴다.
    #[tokio::test]
    async fn remote_prefer_reopens_without_tls_only_after_a_refusal() {
        let recorder = Recorder::new(|mode| match mode {
            ConnectMode::RequireTls => Err(refusal()),
            other => Ok(other),
        });
        let opened = TransportCheck::new(false, TlsPolicy::Prefer)
            .open(|mode| recorder.open(mode))
            .await
            .unwrap();
        assert_eq!(opened, (ConnectMode::Plaintext, Transport::TlsRefused));
        assert_eq!(
            recorder.modes(),
            [ConnectMode::RequireTls, ConnectMode::Plaintext]
        );
    }

    /// 거절이 아닌 실패는 평문으로 다시 열지 않고 그대로 돌려준다.
    #[tokio::test]
    async fn remote_prefer_fails_without_a_plaintext_retry_on_other_errors() {
        let recorder = Recorder::new(|mode| match mode {
            ConnectMode::RequireTls => Err(sqlx::Error::Io(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "handshake",
            ))),
            other => Ok(other),
        });
        let error = TransportCheck::new(false, TlsPolicy::Prefer)
            .open(|mode| recorder.open(mode))
            .await
            .unwrap_err();
        assert!(matches!(error, sqlx::Error::Io(_)), "{error}");
        assert_eq!(recorder.modes(), [ConnectMode::RequireTls]);
    }

    fn empty_document() -> CatalogDocument {
        CatalogDocument {
            context: None,
            dependencies: Vec::new(),
            version: crate::document::DOCUMENT_VERSION,
            dialect: "postgres".into(),
            reader: "native-sqlx".into(),
            schemas: Vec::new(),
            limitations: vec!["track_functions=none".into()],
        }
    }

    /// 전송 limitation은 다른 한계보다 앞에 서고, limitation이 없는 전송은 문서를 바꾸지 않는다.
    #[test]
    fn the_transport_limitation_comes_first() {
        let mut doc = empty_document();
        Transport::TlsRefused.record_limitation(&mut doc, REMEDY);
        assert_eq!(doc.limitations.len(), 2);
        assert!(doc.limitations[0].starts_with(NOT_ENCRYPTED), "{doc:?}");
        assert_eq!(doc.limitations[1], "track_functions=none");
        for transport in [Transport::Local, Transport::Encrypted] {
            let mut unchanged = empty_document();
            transport.record_limitation(&mut unchanged, REMEDY);
            assert_eq!(unchanged.limitations, ["track_functions=none"]);
        }
    }
}
