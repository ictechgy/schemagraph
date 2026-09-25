//! 규칙 검사에 필요한 카탈로그 사실을 그래프 산출물에 보존한다.

use crate::VertexId;
use std::collections::BTreeMap;

/// DB 연결 없이 컬럼 사실을 확인한다. 기본값 SQL 원문은 복제하지 않는다.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnMetadata {
    pub data_type: String,
    pub nullable: bool,
    pub ordinal: u32,
    /// PK 안에서의 1-based 순서. 0은 PK가 아니거나 순서를 수집하지 못했음을 뜻한다.
    pub pk_position: u32,
}

/// 키 순서와 불완전 상태를 보존해 복잡한 인덱스를 단순 인덱스로 오인하지 않는다.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexMetadata {
    pub table: VertexId,
    pub columns: Vec<VertexId>,
    pub unique: bool,
    pub has_predicate: bool,
    pub complete: bool,
}

/// 복합 FK의 컬럼 대응과 미수집 대상을 별도로 표현한다.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForeignKeyMetadata {
    pub table: VertexId,
    pub columns: Vec<VertexId>,
    pub target_table: Option<VertexId>,
    pub target_columns: Vec<VertexId>,
    pub complete: bool,
}

/// None과 비어 있는 수집 결과를 구분하기 위해 Graph는 이 값을 Option으로 보관한다.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SchemaMetadata {
    pub columns: BTreeMap<VertexId, ColumnMetadata>,
    pub indexes: BTreeMap<VertexId, IndexMetadata>,
    pub foreign_keys: BTreeMap<VertexId, ForeignKeyMetadata>,
    /// 수집기가 요청 범위의 카탈로그를 빠짐없이 읽었다고 선언했는지 여부.
    ///
    /// `pk_position` 0은 "PK 아님"과 "못 읽음"을 구분하지 못하므로, 이 값이
    /// 참일 때만 PK 부재를 사실로 확정한다. 이 필드가 없던 옛 그래프는 거짓이다.
    pub catalog_complete: bool,
}
