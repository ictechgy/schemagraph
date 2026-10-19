//! 네이티브 reader가 다른 수집기용 접속 URL을 SQLite 경로로 재해석하지 않는지 검증한다.
//!
//! 재해석하면 sqlx가 퍼센트 디코딩한 URL 원문이 경로 오류에 실려 비밀번호가 새고,
//! 디코딩된 형태라 원문 기준 로그 마스킹도 비켜 간다.

use schemagraph_source::{local_database_path, read, read_with_dependencies, redact_url_userinfo};

/// 다른 수집기용 `scheme://` URL은 오류에 스킴만 싣고 userinfo는 원문·디코딩 형태 모두 남기지 않는다.
#[tokio::test]
async fn unsupported_url_schemes_are_rejected_without_credentials() {
    for (url, scheme) in [
        ("sqlserver://u:p%40ss@h/db", "sqlserver"),
        ("oracle://scott:tiger@db:1521/app", "oracle"),
        ("Postgres://u:p%40ss@h/db", "Postgres"),
    ] {
        let messages = [
            local_database_path(url).unwrap_err().to_string(),
            read(url).await.unwrap_err().to_string(),
            read_with_dependencies(url).await.unwrap_err().to_string(),
        ];
        for message in messages {
            assert!(
                message.contains(&format!("unsupported database URL scheme '{scheme}'")),
                "{message}"
            );
            for secret in ["p@ss", "p%40ss", "scott", "tiger"] {
                assert!(!message.contains(secret), "{message}");
            }
        }
    }
}

/// 경로로 남은 URL 오타가 열리지 않으면 접속 오류는 자격 증명을 가린 경로만 싣는다.
#[tokio::test]
async fn sqlite_connect_errors_redact_userinfo() {
    // 테스트 작업 디렉터리에 없는 상대 경로라 read-only 접속이 실패한다.
    let message = read("postgres:/u:pw@db/app").await.unwrap_err().to_string();
    assert!(message.contains("postgres:/***@db/app"), "{message}");
    assert!(!message.contains(":pw"), "{message}");
}

/// 한 글자 스킴은 Windows 드라이브 문자라 계속 SQLite 파일 경로로 읽는다.
#[test]
fn drive_letter_paths_remain_sqlite_paths() {
    for path in ["C://data/app.db", "C:/data/app.db", "C:\\data\\app.db"] {
        assert_eq!(local_database_path(path).unwrap(), Some(path.into()));
    }
}

/// 슬래시가 빠진 URL 오타처럼 경로로 남는 입력은 마지막 `@` 앞의 자격 증명만 가린다.
#[test]
fn userinfo_is_redacted_from_path_like_inputs() {
    for (text, expected) in [
        ("postgres:/u:pw@db/app", "postgres:/***@db/app"),
        ("postgres:u:pw@db/app", "***@db/app"),
        ("user:pw@db/app", "***@db/app"),
        ("postgres:/u:p@ss@db/app", "postgres:/***@db/app"),
        ("postgres:/u:p/ss@db/app", "postgres:/***@db/app"),
        (
            "sqlserver://sa:S3cr@t@db:1433/app",
            "sqlserver://***@db:1433/app",
        ),
    ] {
        assert_eq!(redact_url_userinfo(text), expected, "{text}");
    }
    // 자격 증명 형태가 아닌 경로는 그대로 보여야 사용자가 무엇을 고칠지 안다.
    for text in [
        "fixture.db",
        "/data/backup@2024.db",
        "sqlite:/data/backup@2024.db",
        "C:\\data\\backup@2024.db",
    ] {
        assert_eq!(redact_url_userinfo(text), text);
    }
}
