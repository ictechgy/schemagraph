//! 네이티브 reader 연결의 전송 암호화 판정 — 네트워크를 지난 평문 연결을 limitation으로 남긴다.
//!
//! 암호화되지 않은 원격 연결로 읽은 카탈로그는 경로상 누구든 엿보거나 바꿀 수 있었다는
//! 뜻이다. 그 사실을 문서에 남겨 소비자가 결과를 얼마나 믿을지 스스로 정하게 한다. Unix
//! 소켓과 루프백 연결은 네트워크를 지나지 않으므로 남기지 않는다. 로컬 fixture마다 붙는
//! 상투적 경보는 읽히지 않기 때문이다(AGENTS.md "limitations를 장식으로 쓰지 마세요").

use std::net::IpAddr;
use std::sync::{Arc, OnceLock};

use crate::document::CatalogDocument;

/// 평문 원격 연결을 알리는 limitation의 접두사. 소비자가 문구 대신 이 접두사로 찾는다.
pub(crate) const NOT_ENCRYPTED: &str = "connection-not-encrypted:";

/// 접속 모드가 TLS를 다루는 방식. sqlx 0.8.6의 PostgreSQL·MySQL 모드를 세 갈래로 접는다.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TlsPolicy {
    /// TLS를 시도하지 않는다(PG disable·allow, MySQL DISABLED). 연결은 늘 평문이다.
    Never,
    /// 서버가 받으면 TLS, 아니면 평문이다(PG prefer, MySQL PREFERRED). 결과는 서버에 물어야 안다.
    Prefer,
    /// TLS가 아니면 sqlx가 접속을 거절한다(require·verify-ca·verify-full과 MySQL 대응 모드).
    Required,
}

/// 연결 하나에서 서버에 물어 확인한 전송 상태.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Transport {
    Encrypted,
    Plaintext,
    /// 확인 질의가 실패했거나 답이 없었다. 암호화됐다고 주장하지 않고 원인을 싣는다.
    Unconfirmed(String),
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

/// 연결마다 서버에 물은 전송 상태를 문서 전체의 사실로 모은다.
///
/// 풀은 연결을 여럿 열 수 있고 after_connect는 연결마다 돈다. 어느 연결이든 평문이거나
/// 확인하지 못했으면 문서 전체의 한계이므로, 처음 나온 비암호화 관측 하나만 남긴다.
/// 복제본은 같은 기록을 공유한다(after_connect 클로저가 연결마다 복제해 쓴다).
#[derive(Debug, Clone)]
pub(crate) struct TransportCheck {
    local: bool,
    policy: TlsPolicy,
    first_unencrypted: Arc<OnceLock<Transport>>,
}

impl TransportCheck {
    /// 접속 대상 종류와 모드로 판정을 준비한다.
    pub(crate) fn new(local: bool, policy: TlsPolicy) -> Self {
        Self {
            local,
            policy,
            first_unencrypted: Arc::new(OnceLock::new()),
        }
    }

    /// 서버에 물어야 판정할 수 있는지 본다. 네트워크를 지나는 prefer 연결만 그렇다.
    /// 다른 모드는 클라이언트가 결과를 이미 안다 — TLS를 시도하지 않거나, TLS가 아니면
    /// 접속하지 않는다. 클라이언트가 아는 사실은 평문 연결에서 위조될 수 있는 서버 답보다 강하다.
    pub(crate) fn needs_server_check(&self) -> bool {
        !self.local && self.policy == TlsPolicy::Prefer
    }

    /// 연결 하나의 관측을 기록한다. 암호화된 연결은 남길 것이 없다.
    pub(crate) fn record(&self, observed: Transport) {
        if observed != Transport::Encrypted {
            self.first_unencrypted.get_or_init(|| observed);
        }
    }

    /// 문서 limitations 맨 앞에 전송 limitation을 둔다. 접속 단계의 사실이라 수집 단계의
    /// 한계보다 앞선다. 수집 범위가 아니라 전송 사실이므로 catalog_complete는 건드리지 않는다.
    pub(crate) fn record_limitation(&self, doc: &mut CatalogDocument, remedy: &str) {
        let observed = self.first_unencrypted.get();
        if let Some(limitation) = connection_limitation(self.local, self.policy, observed, remedy) {
            doc.limitations.insert(0, limitation);
        }
    }
}

/// 연결 종류·모드·서버 관측으로 전송 limitation을 정한다. 문구에는 호스트·사용자·비밀번호를
/// 싣지 않고, 원인과 해결 방향(`remedy`)을 함께 싣는다.
pub(crate) fn connection_limitation(
    local: bool,
    policy: TlsPolicy,
    observed: Option<&Transport>,
    remedy: &str,
) -> Option<String> {
    if local {
        return None;
    }
    let transport = match policy {
        TlsPolicy::Never => &Transport::Plaintext,
        TlsPolicy::Required => return None,
        TlsPolicy::Prefer => observed?,
    };
    match transport {
        Transport::Encrypted => None,
        Transport::Plaintext => Some(format!(
            "{NOT_ENCRYPTED} the catalog was read over an unencrypted connection; {remedy}"
        )),
        Transport::Unconfirmed(cause) => Some(format!(
            "{NOT_ENCRYPTED} the catalog connection could not be confirmed as encrypted ({cause}); {remedy}"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn local_connections_never_carry_the_limitation() {
        for policy in [TlsPolicy::Never, TlsPolicy::Prefer, TlsPolicy::Required] {
            assert_eq!(
                connection_limitation(true, policy, Some(&Transport::Plaintext), REMEDY),
                None
            );
        }
    }

    /// 모드가 결과를 정하는 원격 연결은 서버 관측 없이 판정한다.
    #[test]
    fn the_mode_decides_remote_connections_without_tls_negotiation() {
        let disabled = connection_limitation(false, TlsPolicy::Never, None, REMEDY).unwrap();
        assert_eq!(
            disabled,
            "connection-not-encrypted: the catalog was read over an unencrypted connection; add sslmode=verify-full to the URL"
        );
        assert_eq!(
            connection_limitation(false, TlsPolicy::Required, None, REMEDY),
            None
        );
    }

    /// prefer 원격 연결은 서버 관측을 따르고, 확인하지 못했으면 암호화를 주장하지 않는다.
    #[test]
    fn prefer_follows_the_server_observation() {
        let limitation =
            |observed| connection_limitation(false, TlsPolicy::Prefer, observed, REMEDY);
        assert_eq!(limitation(None), None);
        assert_eq!(limitation(Some(&Transport::Encrypted)), None);
        assert!(limitation(Some(&Transport::Plaintext))
            .unwrap()
            .starts_with("connection-not-encrypted: the catalog was read over an unencrypted"));
        let unconfirmed = limitation(Some(&Transport::Unconfirmed("no row".into()))).unwrap();
        assert_eq!(
            unconfirmed,
            "connection-not-encrypted: the catalog connection could not be confirmed as encrypted (no row); add sslmode=verify-full to the URL"
        );
    }

    /// 서버 확인은 네트워크를 지나는 prefer 연결에서만 한다.
    #[test]
    fn only_remote_prefer_connections_ask_the_server() {
        assert!(TransportCheck::new(false, TlsPolicy::Prefer).needs_server_check());
        assert!(!TransportCheck::new(true, TlsPolicy::Prefer).needs_server_check());
        assert!(!TransportCheck::new(false, TlsPolicy::Never).needs_server_check());
        assert!(!TransportCheck::new(false, TlsPolicy::Required).needs_server_check());
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

    /// 연결 여러 개 중 하나라도 평문이면 문서의 한계다. 처음 관측한 것만 맨 앞에 남는다.
    #[test]
    fn the_first_unencrypted_connection_becomes_the_first_limitation() {
        let check = TransportCheck::new(false, TlsPolicy::Prefer);
        let shared = check.clone();
        shared.record(Transport::Encrypted);
        shared.record(Transport::Unconfirmed("query failed".into()));
        check.record(Transport::Plaintext);
        let mut doc = empty_document();
        check.record_limitation(&mut doc, REMEDY);
        assert_eq!(doc.limitations.len(), 2);
        assert!(
            doc.limitations[0].contains("(query failed)"),
            "{:?}",
            doc.limitations
        );
        assert_eq!(doc.limitations[1], "track_functions=none");
    }

    #[test]
    fn encrypted_connections_leave_the_document_unchanged() {
        let check = TransportCheck::new(false, TlsPolicy::Prefer);
        check.record(Transport::Encrypted);
        let mut doc = empty_document();
        check.record_limitation(&mut doc, REMEDY);
        assert_eq!(doc.limitations, vec!["track_functions=none".to_owned()]);
    }
}
