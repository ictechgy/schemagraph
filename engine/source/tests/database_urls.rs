//! 네이티브 reader가 다른 수집기용 접속 URL을 SQLite 경로로 재해석하지 않는지 검증한다.
//!
//! 재해석하면 sqlx가 퍼센트 디코딩한 URL 원문이 경로 오류에 실려 비밀번호가 새고,
//! 디코딩된 형태라 원문 기준 로그 마스킹도 비켜 간다. 스킴 없는 접속 문자열처럼
//! 경로로 남는 입력은 오류에 자격 증명 형태를 싣지 않는지도 본다.

use schemagraph_source::{
    local_database_path, read, read_with_dependencies, redact_connection_secrets,
};

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

/// JVM 프로브용 `jdbc:` URL은 `//`의 위치와 무관하게 거절한다. 하위 프로토콜 뒤의
/// 속성(`;password=…`)이나 `사용자/비밀번호@` 형태는 SQLite 경로 오류로도 남지 않는다.
#[tokio::test]
async fn jdbc_urls_are_rejected_without_their_properties() {
    for (url, scheme) in [
        (
            "jdbc:sqlserver://db:1433;databaseName=app;user=sa;password=FAKEPW1",
            "jdbc",
        ),
        ("jdbc:oracle:thin:scott/FAKEPW2@db:1521/app", "jdbc"),
        ("JDBC:db2://db:50000/app:user=u;password=FAKEPW3;", "JDBC"),
        ("jdbc:sqlite:/data/app.db", "jdbc"),
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
            // 해결 방향: JDBC URL은 JVM 프로브로 수집해 document로 넘긴다.
            assert!(
                message.contains("JDBC URLs") && message.contains("scan --document"),
                "{message}"
            );
            for fragment in ["FAKEPW", "scott", "databaseName", "/data/app.db"] {
                assert!(!message.contains(fragment), "{message}");
            }
        }
    }
}

/// 경로로 남은 URL 오타나 접속 문자열이 열리지 않으면 접속 오류는 자격 증명을 가린
/// 경로만 싣는다.
#[tokio::test]
async fn sqlite_connect_errors_redact_credentials() {
    // 테스트 작업 디렉터리에 없는 상대 경로라 read-only 접속이 실패한다.
    for (input, shown, secret) in [
        ("postgres:/u:pw@db/app", "postgres:/***@db/app", ":pw"),
        (
            "host=db user=u password=FAKEPW1 dbname=app",
            "host=db user=u password=*** dbname=app",
            "FAKEPW1",
        ),
    ] {
        let message = read(input).await.unwrap_err().to_string();
        assert!(message.contains(shown), "{message}");
        assert!(!message.contains(secret), "{message}");
    }
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
        assert_eq!(redact_connection_secrets(text), expected, "{text}");
    }
    // 자격 증명 형태가 아닌 경로는 그대로 보여야 사용자가 무엇을 고칠지 안다.
    for text in [
        "fixture.db",
        "/data/backup@2024.db",
        "sqlite:/data/backup@2024.db",
        "C:\\data\\backup@2024.db",
    ] {
        assert_eq!(redact_connection_secrets(text), text);
    }
}

/// 스킴이 없는 접속 문자열(libpq DSN, ODBC·ADO.NET, JDBC식 속성 목록)도 SQLite 경로로
/// 남는다. 비밀 속성 값은 구분자(`;`·`&`·공백) 뒤에 다음 속성(`이름=`)이 오거나 문자열이 끝나는
/// 곳까지, 그런 경계가 없거나 따옴표·중괄호로 감싼 값은 끝까지 가린다.
#[test]
fn secret_options_are_redacted_from_connection_strings() {
    for (text, expected) in [
        (
            "host=db user=u password=FAKEPW1 dbname=app",
            "host=db user=u password=*** dbname=app",
        ),
        (
            "host=db password = FAKEPW2 dbname=app",
            "host=db password = *** dbname=app",
        ),
        (
            "Server=db;Database=app;Uid=sa;Pwd=FAKEPW3;",
            "Server=db;Database=app;Uid=sa;Pwd=***;",
        ),
        (
            "db:1433;user=sa;PASSWORD=FAKEPW4;trustStorePassword=FAKEPW5",
            "db:1433;user=sa;PASSWORD=***;trustStorePassword=***",
        ),
        (
            "app&sslpassword=FAKEPW6&client_secret=FAKEPW7&accessToken=FAKEPW8",
            "app&sslpassword=***&client_secret=***&accessToken=***",
        ),
        (
            "host=db password='FAKE PW9' dbname=app",
            "host=db password=***",
        ),
        (
            "Server=db;Password={FAKE;PW10};Database=app",
            "Server=db;Password=***",
        ),
        (
            "postgres:/u:pw@db/app;password=FAKEPW11",
            "postgres:/***@db/app;password=***",
        ),
        // 형식이 깨져 다음 속성 이름이 앞에서 가린 값 안에 들어가도 그 값을 가린다. 뒤에 `이름=`
        // 경계가 없으므로 끝까지 가린다.
        ("pwd=x.password =FAKEPW12", "pwd=***"),
        ("password=xpwd=FAKEPW13;user=u", "password=***;user=u"),
        // libpq 값은 `;`·`&`를 담을 수 있고 백슬래시로 공백을 이스케이프한다. 구분자 하나만으로
        // 끝을 정하면 값의 뒷부분이 드러나므로, 다음 `이름=`이나 문자열 끝이 올 때까지 가린다.
        ("password=FAKE;PW14", "password=***"),
        ("password=FAKE\\ PW15", "password=***"),
        (
            "host=db password=FAKE;PW16 dbname=app",
            "host=db password=*** dbname=app",
        ),
        (
            "host=db password=FAKE&PW17 sslmode=require",
            "host=db password=*** sslmode=require",
        ),
        ("Pwd=FAKE PW&18;", "Pwd=***;"),
        (
            "Server=db;Pwd=FAKE PW&19;Database=app",
            "Server=db;Pwd=***;Database=app",
        ),
    ] {
        assert_eq!(redact_connection_secrets(text), expected, "{text}");
    }
    // 비밀 속성이 아닌 `이름=값`과 일반 경로는 그대로 둔다.
    for text in [
        "host=db user=u dbname=app",
        "/data/mode=ro/app.db",
        "/data/passwords/app.db",
        "/data/passwordless=1.db",
    ] {
        assert_eq!(redact_connection_secrets(text), text);
    }
}
