//! schemagraph-source — catalog document 계약과 DB reader.
//!
//! reader는 document를 뱉고, [`graph::document_to_graph`]가 그래프로 바꾼다.
//! 프로브(P2)도 같은 document를 뱉으므로 변환기는 경로를 모른다.

pub mod bridge_facts;
pub mod codec;
pub mod context;
pub mod dependencies;
pub mod diff;
pub mod document;
pub mod graph;
pub mod imports;
pub mod mysql;
pub mod ndjson;
pub mod postgres;
pub mod sql_files;
pub mod sqlite;

pub use document::CatalogDocument;

/// 네이티브 reader와 SQLite 경로 해석이 직접 다루는 URL 스킴. reader 분기와 같이
/// 대소문자를 구분한다. `mysqlx`는 전용 미지원 안내가 있어 여기서 거절하지 않는다.
const NATIVE_URL_SCHEMES: [&str; 6] = [
    "sqlite",
    "file",
    "postgres",
    "postgresql",
    "mysql",
    "mysqlx",
];

/// 출력 파일 보호가 접속 URL을 임의로 재해석하지 않도록 로컬 DB 경로를 제공한다.
pub fn local_database_path(url: &str) -> Result<Option<std::path::PathBuf>, SourceError> {
    reject_foreign_scheme(url)?;
    if ["postgres://", "postgresql://", "mysql://", "mysqlx://"]
        .iter()
        .any(|scheme| url.starts_with(scheme))
    {
        Ok(None)
    } else {
        sqlite::database_path(url)
    }
}

/// URL 스킴으로 네이티브 reader를 고른다. 지원 안 하는 스킴은 명확한 오류 —
/// 조용히 다른 reader로 돌리지 않는다(엉뚱한 DB를 읽는 것보다 낫다).
pub async fn read(url: &str) -> Result<CatalogDocument, SourceError> {
    reject_foreign_scheme(url)?;
    if url.starts_with("postgres://") || url.starts_with("postgresql://") {
        postgres::read(url).await
    } else if url.starts_with("mysql://") {
        mysql::read(url).await
    } else if url.starts_with("mysqlx://") {
        // X Protocol(33060)은 classic 프로토콜과 달라 sqlx로 못 읽는다.
        Err(SourceError::Connect(
            "mysqlx:// (X Protocol)는 미지원 — mysql:// URL을 써라".to_owned(),
        ))
    } else {
        // sqlite는 URL 형태가 다양하다(파일 경로까지) — 나머지는 sqlite로.
        sqlite::read(url).await
    }
}

/// 호출자가 추가 메타데이터 수집을 명시하도록 기본 read와 구분한다.
pub async fn read_with_dependencies(url: &str) -> Result<CatalogDocument, SourceError> {
    if url.starts_with("postgres://") || url.starts_with("postgresql://") {
        return postgres::read_with_dependencies(url, true).await;
    }
    let mut doc = read(url).await?;
    doc.limitations.push("native catalog dependency collection is supported for PostgreSQL; use the JDBC or Go probe for Oracle and SQL Server".into());
    if let Some(context) = &mut doc.context {
        context.catalog_complete = false;
    }
    Ok(doc)
}

/// 다른 수집기용 `scheme://` URL을 SQLite 파일 경로로 재해석하지 않고 거절한다.
///
/// 재해석하면 sqlx가 퍼센트 디코딩한 URL 원문(비밀번호 포함)이 경로 오류에 실린다.
/// 그래서 오류에는 스킴만 싣는다.
fn reject_foreign_scheme(url: &str) -> Result<(), SourceError> {
    match url_scheme(url) {
        Some(scheme) if !NATIVE_URL_SCHEMES.contains(&scheme) => Err(SourceError::Connect(format!(
            "unsupported database URL scheme '{scheme}'; native readers accept sqlite:, postgres://, postgresql://, and mysql:// URLs; for other databases, run the JDBC or Go probe and pass its output to scan --document"
        ))),
        _ => Ok(()),
    }
}

/// `scheme://`로 시작하는 입력의 스킴. 2자 이상만 스킴으로 본다 — `C://data/app.db`
/// 같은 Windows 드라이브 경로는 계속 SQLite 경로로 읽어야 하기 때문이다.
fn url_scheme(url: &str) -> Option<&str> {
    let (scheme, rest) = url.split_once(':')?;
    (scheme.len() >= 2 && is_scheme_like(scheme) && rest.starts_with("//")).then_some(scheme)
}

/// RFC 3986 스킴 문자 규칙(글자로 시작, 글자·숫자·`+`·`-`·`.`)을 따르는지 본다.
fn is_scheme_like(text: &str) -> bool {
    text.starts_with(|c: char| c.is_ascii_alphabetic())
        && text
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
}

/// 경로·URL 문자열의 `사용자:비밀번호@` 부분을 `***@`로 가린다.
///
/// `postgres:/u:pw@db`처럼 슬래시가 빠진 URL 오타는 SQLite 경로로 남아 경로 오류에
/// 실리므로, 경로를 오류 메시지에 넣는 곳이 이 함수를 거친다. 디코딩된 비밀번호는
/// `@`·`/`를 담을 수 있어 마지막 `@`까지, 앞의 스킴·드라이브 접두 다음부터 가린다.
/// 그 구간에 `:`가 없으면 자격 증명 형태가 아니라 그대로 둔다(`backup@2024.db`).
pub fn redact_url_userinfo(text: &str) -> String {
    let Some(at) = text.rfind('@') else {
        return text.to_owned();
    };
    let start = userinfo_start(&text[..at]);
    if text[start..at].contains(':') {
        format!("{}***{}", &text[..start], &text[at..])
    } else {
        text.to_owned()
    }
}

/// `scheme:` 또는 드라이브 문자 뒤에 `/`·`\`가 이어지면 그 구분자 다음이 userinfo 시작이다.
/// 구분자가 없으면(`user:pw@host`) 앞의 `글자:`도 userinfo일 수 있어 처음부터 본다.
fn userinfo_start(head: &str) -> usize {
    let Some((prefix, rest)) = head.split_once(':') else {
        return 0;
    };
    let separators = rest.len() - rest.trim_start_matches(['/', '\\']).len();
    if is_scheme_like(prefix) && separators > 0 {
        prefix.len() + 1 + separators
    } else {
        0
    }
}

/// reader 실패. Connect(접속 자체 실패)와 Query(카탈로그 읽기 실패)를 구분해
/// 사용자에게 다른 해결 방향을 보여준다.
#[derive(Debug, thiserror::Error)]
pub enum SourceError {
    #[error("{0}")]
    Connect(String),
    #[error("카탈로그 읽기 실패: {0}")]
    Query(#[from] sqlx::Error),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_database_path_uses_the_reader_url_semantics() {
        for url in [
            "fixture.db",
            "sqlite:fixture.db",
            "sqlite://fixture.db?mode=ro",
        ] {
            assert_eq!(local_database_path(url).unwrap(), Some("fixture.db".into()));
        }
        assert_eq!(
            local_database_path("sqlite:my%20db.db").unwrap(),
            Some("my db.db".into())
        );
        for url in [
            "file:my%20db.db?mode=ro",
            "sqlite:file:my%20db.db?mode=ro",
            "sqlite://file:my%2520db.db?mode=rwc",
        ] {
            assert_eq!(local_database_path(url).unwrap(), Some("my db.db".into()));
        }
        for url in ["file:///tmp/fixture.db", "file://localhost/tmp/fixture.db"] {
            assert_eq!(
                local_database_path(url).unwrap(),
                Some("/tmp/fixture.db".into())
            );
        }
        for url in [
            "sqlite::memory:",
            "postgres://localhost/db",
            "postgresql://localhost/db",
            "mysql://localhost/db",
            "mysqlx://localhost/db",
        ] {
            assert_eq!(local_database_path(url).unwrap(), None);
        }
    }

    /// mysql://는 mysql reader로, mysqlx://는 명시적 미지원 오류로 간다 —
    /// 어떤 스킴도 조용히 sqlite로 돌지 않는다는 계약을 고정한다.
    #[tokio::test]
    async fn mysql_스킴은_mysql_reader로_디스패치된다() {
        // 포트 범위 밖 URL이라 파싱 단계에서 실패한다 — "미지원"이 아니라
        // mysql::read가 낸 "URL 해석 실패"여야 reader까지 도달한 증거가 된다.
        let err = read("mysql://nobody@localhost:99999/none")
            .await
            .unwrap_err();
        assert!(matches!(err, SourceError::Connect(_)), "got: {err}");
        assert!(err.to_string().contains("mysql"), "got: {err}");
        assert!(!err.to_string().contains("미지원"), "got: {err}");
    }

    #[tokio::test]
    async fn mysqlx_스킴은_명시적_미지원_오류다() {
        let err = read("mysqlx://localhost/none").await.unwrap_err();
        assert!(err.to_string().contains("미지원"), "got: {err}");
    }
}
