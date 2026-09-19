//! schemagraph-source — catalog document 계약과 DB reader.
//!
//! reader는 document를 뱉고, [`graph::document_to_graph`]가 그래프로 바꾼다.
//! 프로브(P2)도 같은 document를 뱉으므로 변환기는 경로를 모른다.

pub mod document;
pub mod graph;
pub mod postgres;
pub mod sqlite;

pub use document::CatalogDocument;

/// URL 스킴으로 네이티브 reader를 고른다. 지원 안 하는 스킴은 명확한 오류 —
/// 조용히 다른 reader로 돌리지 않는다(엉뚱한 DB를 읽는 것보다 낫다).
pub async fn read(url: &str) -> Result<CatalogDocument, SourceError> {
    if url.starts_with("postgres://") || url.starts_with("postgresql://") {
        postgres::read(url).await
    } else if url.starts_with("mysql://") || url.starts_with("mysqlx://") {
        Err(SourceError::Connect(format!(
            "mysql reader는 아직 없다 — URL: {url}"
        )))
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
