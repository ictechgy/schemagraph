//! schemagraph-source — catalog document 계약과 DB reader.
//!
//! reader는 document를 뱉고, [`graph::document_to_graph`]가 그래프로 바꾼다.
//! 프로브(P2)도 같은 document를 뱉으므로 변환기는 경로를 모른다.

pub mod diff;
pub mod document;
pub mod graph;
pub mod mysql;
pub mod ndjson;
pub mod postgres;
pub mod sqlite;

pub use document::CatalogDocument;

/// URL 스킴으로 네이티브 reader를 고른다. 지원 안 하는 스킴은 명확한 오류 —
/// 조용히 다른 reader로 돌리지 않는다(엉뚱한 DB를 읽는 것보다 낫다).
pub async fn read(url: &str) -> Result<CatalogDocument, SourceError> {
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
