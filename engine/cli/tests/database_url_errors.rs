//! 네이티브 reader가 읽지 못하는 접속 URL·접속 문자열이 비밀번호를 오류 메시지에 싣지 않는지 검증한다.
//!
//! 다른 수집기용 URL(`sqlserver://` 등)을 SQLite 경로로 재해석하면 URL 원문이
//! 경로 오류에 그대로 실린다. 퍼센트 디코딩된 형태라 원문 기준 로그 마스킹도 비켜 간다.

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

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
