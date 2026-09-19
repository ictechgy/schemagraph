//! catalog document — 엔진과 reader 사이의 버전 달린 계약.
//!
//! 이 타입이 곧 P2 프로브 프로토콜의 씨앗이다(DESIGN.md "미결 사항").
//! 네이티브 reader든 JVM 프로브든 같은 document를 뱉어야 하므로, 필드 이름은
//! 와이어 계약이다 — 바꾸면 `version`을 올리고 양쪽을 함께 고친다.

use serde::{Deserialize, Serialize};

/// 계약 버전. 형식이 깨지는 변경은 이 숫자를 올린다.
pub const DOCUMENT_VERSION: u32 = 1;

/// reader가 채우는 스키마 스냅샷. 결정적 출력을 위해 모든 컬렉션은
/// reader가 이름 순으로 정렬해 넣는다.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CatalogDocument {
    pub version: u32,
    /// "sqlite" | "postgres" | "mysql" | ...
    pub dialect: String,
    /// document를 만든 경로. "native-sqlx" | "probe-jdbc" | ...
    pub reader: String,
    pub schemas: Vec<SchemaDoc>,
    /// 스캔 중 실측한 한계. 모든 응답에 그대로 실어야 한다.
    pub limitations: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SchemaDoc {
    pub name: String,
    pub objects: Vec<ObjectDoc>,
    pub routines: Vec<RoutineDoc>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ObjectDoc {
    pub name: String,
    /// "table" | "view" | "materialized-view" | "sequence" | "type" | "synonym"
    pub kind: String,
    pub columns: Vec<ColumnDoc>,
    pub constraints: Vec<ConstraintDoc>,
    pub indexes: Vec<IndexDoc>,
    /// 이 객체에 붙는 트리거(테이블·뷰 소유).
    pub triggers: Vec<TriggerDoc>,
    /// view 정의·객체 DDL 원문. 파싱은 엔진의 일 — reader는 옮기기만 한다.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    /// 사용 통계(pg_stat·sys 스키마 등). 통계가 없는 DB는 None.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<UsageDoc>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ColumnDoc {
    pub name: String,
    /// 카탈로그가 보고한 원문 타입("INTEGER", "varchar(20)" 등).
    pub data_type: String,
    pub nullable: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default: Option<String>,
    /// 1부터 시작하는 선언 순서.
    pub ordinal: u32,
    /// PK이면 1부터 시작하는 키 내 위치, 아니면 0.
    pub pk_position: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConstraintDoc {
    /// 카탈로그가 이름을 주지 않는 DB(SQLite)에서는 reader가 만들어 쓴다
    /// ("orders_fk_0"). 이름이 없으면 그래프 정점을 못 만든다.
    pub name: String,
    /// "pk" | "fk" | "unique" | "check"
    pub kind: String,
    /// 제약이 걸린 로컬 컬럼(선언 순서).
    pub columns: Vec<String>,
    /// fk일 때 참조 대상.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub referenced: Option<ReferencedDoc>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReferencedDoc {
    /// 대상 스키마. 카탈로그가 알려주지 않으면 None — 같은 스키마로 추정하지
    /// 않고 엔진이 이름 해석하게 둔다.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub schema: Option<String>,
    pub table: String,
    /// `columns`와 같은 순서로 대응되는 대상 컬럼들.
    pub columns: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IndexDoc {
    pub name: String,
    pub unique: bool,
    pub columns: Vec<String>,
    /// 인덱스 사용 통계(pg_stat_user_indexes, sys.schema_unused_indexes 등).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<UsageDoc>,
}

/// 사용 통계 — DB가 리셋 이후 관측한 작업량. 통계는 "since 이후만 유효"라는
/// 것이 계약의 핵심이라, since 없는 0은 "미사용"이 아니라 "모름"이다.
/// additive 필드라 document 버전은 올리지 않는다(없는 reader는 None).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UsageDoc {
    /// 통계 유효 시작 시점(리셋·재시작 시각). 모르면 None.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub since: Option<String>,
    /// 관측된 읽기 작업량(방언별 스캔·fetch 합산).
    pub reads: u64,
    /// 관측된 쓰기 작업량(insert·update·delete 합산).
    pub writes: u64,
    /// routine 누적 실행 시간 ms(pg_stat_user_functions.total_time).
    /// 중첩 호출 시간을 포함한다. routine이 아닌 정점·미지원 방언은 None.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_ms: Option<f64>,
    /// routine 자기 실행 시간 ms(pg_stat_user_functions.self_time).
    /// 안에서 부른 다른 routine의 시간을 뺀 값 — 비용 핫스팟 판별용.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub self_ms: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TriggerDoc {
    pub name: String,
    /// 트리거 몸체 원문. 파싱은 엔진이 한다.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RoutineDoc {
    pub name: String,
    /// "function" | "procedure" | "package"
    pub kind: String,
    /// "sql" | "plpgsql" | "pl/sql" 등. 파서 선택에 쓴다.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    /// 몸체 원문.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    /// 같은 이름의 오버로드 구분자(Postgres 인자 시그니처 등). 없으면 생략.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,
    /// 사용 통계(pg_stat_user_functions의 calls 등). reads의 단위는
    /// kind에 따라 다르다 — routine에선 호출 횟수다. 없는 reader는 None.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<UsageDoc>,
}
