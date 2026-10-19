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

/// 다른 수집기용 URL(`scheme://`, JVM 프로브의 `jdbc:`)을 SQLite 파일 경로로 재해석하지
/// 않고 거절한다.
///
/// 재해석하면 sqlx가 퍼센트 디코딩한 URL 원문(비밀번호 포함)이 경로 오류에 실린다.
/// 그래서 오류에는 스킴만 싣는다.
fn reject_foreign_scheme(url: &str) -> Result<(), SourceError> {
    match foreign_scheme(url) {
        Some(scheme) => Err(SourceError::Connect(format!(
            "unsupported database URL scheme '{scheme}'; native readers accept sqlite:, postgres://, postgresql://, and mysql:// URLs; for JDBC URLs and other databases, run the JDBC or Go probe and pass its output to scan --document"
        ))),
        None => Ok(()),
    }
}

/// 네이티브 reader가 읽지 않는 URL의 스킴.
///
/// JDBC URL은 `jdbc:하위프로토콜:…`이라 `//`가 하위 프로토콜 뒤에 오거나
/// (`jdbc:sqlserver://…;password=…`) 아예 없다(`jdbc:oracle:thin:…`). 그래서
/// `scheme://` 검사로는 잡히지 않아 따로 본다.
fn foreign_scheme(url: &str) -> Option<&str> {
    jdbc_scheme(url)
        .or_else(|| url_scheme(url).filter(|scheme| !NATIVE_URL_SCHEMES.contains(scheme)))
}

/// `jdbc:` 접두의 스킴을 입력 표기 그대로 돌려준다. URL 스킴은 대소문자를 가리지 않으므로
/// (RFC 3986) `JDBC:`도 같게 본다 — 어느 표기든 SQLite 경로로 남으면 속성이 경로 오류에 실린다.
fn jdbc_scheme(url: &str) -> Option<&str> {
    let scheme = url.get(..4)?;
    // get(..4)가 성공했으니 4는 문자 경계라 슬라이스가 패닉하지 않는다.
    (scheme.eq_ignore_ascii_case("jdbc") && url[4..].starts_with(':')).then_some(scheme)
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

/// 경로 오류에 싣는 문자열에서 접속 자격 증명 형태를 가린다.
///
/// 네이티브 reader가 읽지 않는 입력 중 스킴 검사로 거절되지 않는 것은 SQLite 경로로
/// 남아 경로 오류에 실린다. 스킴이 없는 접속 문자열(libpq `host=… password=…`,
/// ODBC·ADO.NET `Server=…;Pwd=…`)과 슬래시가 빠진 URL 오타(`postgres:/u:pw@db`)가
/// 그렇다. 그래서 경로를 오류 메시지에 넣는 곳이 이 함수를 거친다. 비밀 속성의 값과
/// URL userinfo를 각각 `***`로 바꾸고, 나머지는 사용자가 무엇을 고칠지 알도록 그대로 둔다.
pub fn redact_connection_secrets(text: &str) -> String {
    redact_url_userinfo(&redact_secret_options(text))
}

/// 이름이 이 접미사로 끝나는 접속 속성은 값이 비밀이다(대소문자 무관). 예를 들어
/// `password`·`sslpassword`·`trustStorePassword`·ODBC `PWD`·`client_secret`·`accessToken`이다.
const SECRET_OPTION_SUFFIXES: [&str; 5] = ["password", "passwd", "pwd", "secret", "token"];

/// `이름=값` 접속 속성 중 비밀 속성의 값을 `***`로 가린다.
///
/// 구분자 형식(`;`·`&`·공백)을 몰라도 되도록 원문의 `=`마다 바로 앞 속성 이름을 본다.
/// 이름은 가리기 전 원문에서 본다. 형식이 깨진 입력(`pwd=x.password =y`)에서는 다음
/// 속성 이름이 앞에서 가린 값 안에 들어가기 때문이다. 값 구간이 겹치면 같은 `***`로
/// 이어 가린다.
fn redact_secret_options(text: &str) -> String {
    let mut shown = String::with_capacity(text.len());
    let mut copied = 0;
    for (start, end) in secret_value_ranges(text) {
        if start > copied {
            shown.push_str(&text[copied..start]);
            shown.push_str("***");
        }
        copied = copied.max(end);
    }
    shown.push_str(&text[copied..]);
    shown
}

/// 원문에서 비밀 속성 값의 바이트 구간을 `=` 순서(곧 시작 위치 순서)로 돌려준다.
fn secret_value_ranges(text: &str) -> impl Iterator<Item = (usize, usize)> + '_ {
    text.match_indices('=')
        .filter(|(equals, _)| is_secret_option(&text[..*equals]))
        .map(|(equals, _)| {
            let value = equals + 1;
            let (start, end) = secret_value_span(&text[value..]);
            (value + start, value + end)
        })
}

/// `=` 앞 문자열의 마지막 속성 이름(영숫자·`_`·`-`·`.`)이 비밀 속성인지 본다.
/// libpq·ODBC는 `=` 앞뒤 공백을 허용하므로 이름 뒤 공백은 건너뛴다.
fn is_secret_option(before_equals: &str) -> bool {
    let name = before_equals
        .trim_end()
        .rsplit(|c: char| !(c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.')))
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
    SECRET_OPTION_SUFFIXES
        .iter()
        .any(|suffix| name.ends_with(suffix))
}

/// `=` 뒤에서 비밀 값의 (시작, 끝) 바이트 위치를 찾는다. 앞쪽 공백은 값이 아니다.
///
/// 따옴표·중괄호로 감싼 값은 형식마다 이스케이프 규칙이 달라(libpq `\'`, ODBC `}}`)
/// 문자열 끝까지 가린다. 덜 가리는 것보다 더 가리는 편이 안전하기 때문이다. 그 밖의
/// 값은 구분자 뒤에 다음 속성이 오거나 문자열이 끝나는 곳에서 끝난다([`is_value_boundary`]).
/// libpq 값은 `;`·`&`를 담을 수 있고 백슬래시로 공백을 이스케이프하므로, 구분자 하나만으로
/// 끝을 정하면 값의 뒷부분이 오류 메시지에 남는다. 그런 경계가 없으면 끝까지 가린다.
fn secret_value_span(after_equals: &str) -> (usize, usize) {
    let start = after_equals.len() - after_equals.trim_start().len();
    let value = &after_equals[start..];
    let length = if value.starts_with(['\'', '"', '{']) {
        value.len()
    } else {
        value
            .char_indices()
            .find(|&(index, c)| is_value_boundary(value, index, c))
            .map_or(value.len(), |(index, _)| index)
    };
    (start, start + length)
}

/// 값 안 `index` 위치의 글자 `c`에서 값이 끝나는지 본다. 구분자이고, 바로 앞이 백슬래시가
/// 아니며, 뒤에 다음 속성이 오거나 구분자만 남아야 한다([`ends_value_at`]).
///
/// libpq에서 백슬래시 뒤의 구분자는 값의 글자다(`password=a\ b=c`의 값은 `a b=c`). 이를
/// 경계로 보면 이스케이프 뒤의 값 조각이 오류 메시지에 남는다. ODBC처럼 백슬래시가
/// 이스케이프가 아닌 형식에서는 다음 속성까지 더 가릴 뿐이라 안전하다.
fn is_value_boundary(value: &str, index: usize, c: char) -> bool {
    is_option_separator(c)
        && !value[..index].ends_with('\\')
        && ends_value_at(&value[index + c.len_utf8()..])
}

/// 접속 문자열 속성 사이에 오는 구분자(`;`·`&`·공백)인지 본다.
fn is_option_separator(c: char) -> bool {
    matches!(c, ';' | '&') || c.is_whitespace()
}

/// 구분자 뒤의 나머지가 값의 끝을 뜻하는지 본다. 구분자만 남았거나, 다음 속성 이름
/// (영숫자·`_`·`-`·`.`) 뒤에 `=`가 오면(libpq처럼 `=` 앞 공백 허용) 끝이다.
fn ends_value_at(rest: &str) -> bool {
    let rest = rest.trim_start_matches(is_option_separator);
    if rest.is_empty() {
        return true;
    }
    let name = rest
        .find(|c: char| !(c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.')))
        .unwrap_or(rest.len());
    name > 0 && rest[name..].trim_start().starts_with('=')
}

/// 경로·URL 문자열의 `사용자:비밀번호@` 부분을 `***@`로 가린다.
///
/// 디코딩된 비밀번호는 `@`·`/`를 담을 수 있어 마지막 `@`까지, 앞의 스킴·드라이브
/// 접두 다음부터 가린다. 그 구간에 `:`가 없으면 자격 증명 형태가 아니라 그대로
/// 둔다(`backup@2024.db`).
fn redact_url_userinfo(text: &str) -> String {
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
