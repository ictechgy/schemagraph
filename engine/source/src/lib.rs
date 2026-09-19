//! schemagraph-source — catalog document 계약과 DB reader.
//!
//! reader는 document를 뱉고, [`graph::document_to_graph`]가 그래프로 바꾼다.
//! 프로브(P2)도 같은 document를 뱉으므로 변환기는 경로를 모른다.

pub mod document;
pub mod graph;
pub mod sqlite;

pub use document::CatalogDocument;

/// reader 실패. Connect(접속 자체 실패)와 Query(카탈로그 읽기 실패)를 구분해
/// 사용자에게 다른 해결 방향을 보여준다.
#[derive(Debug, thiserror::Error)]
pub enum SourceError {
    #[error("{0}")]
    Connect(String),
    #[error("카탈로그 읽기 실패: {0}")]
    Query(#[from] sqlx::Error),
}
