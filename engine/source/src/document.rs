//! catalog document — 엔진과 reader 사이의 버전 달린 계약.
//!
//! 이 타입이 곧 P2 프로브 프로토콜의 씨앗이다(DESIGN.md "미결 사항").
//! 네이티브 reader든 JVM 프로브든 같은 document를 뱉어야 하므로, 필드 이름은
//! 와이어 계약이다 — 바꾸면 `version`을 올리고 양쪽을 함께 고친다.

use serde::{Deserialize, Serialize};

/// 내부 모델과 기본 출력의 버전. v2 전송 메타데이터는 codec에서 정규화한다.
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
    /// 패키지 멤버면 부모 패키지 이름 — 독립 routine이면 생략. 멤버는
    /// `schema.package.member` 정점이 되고 패키지→멤버 contains 간선이 생긴다.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub member_of: Option<String>,
}

/// 역직렬화로 읽은 문서에서 serde가 조용히 무시한 필드를 찾는다.
/// additive 필드는 같은 버전 안에서 허용하지만, reader가 의미를 실어 보낸
/// 키를 엔진이 못 읽었으면 소비자에게 알려야 한다 — "모른다"와 "없다"를
/// 구분하는 것이 이 도구의 계약이다. 경로는 `schemas[].objects[].x` 꼴로
/// 인덱스 없이 정규화해 중복을 합친다.
pub fn unknown_field_paths(doc: &serde_json::Value) -> Vec<String> {
    use std::collections::BTreeSet;
    const MAX: usize = 20;

    // 알려진 컨테이너 키 → 하위 레코드의 알려진 키 집합. 나머지 키는
    // 잎(scalar·문자열 배열)이라 내려갈 곳이 없다.
    fn container_keys(key: &str) -> Option<&'static [&'static str]> {
        Some(match key {
            "schemas" => SCHEMA_KEYS,
            "objects" => OBJECT_KEYS,
            "routines" => ROUTINE_KEYS,
            "columns" => COLUMN_KEYS,
            "constraints" => CONSTRAINT_KEYS,
            "referenced" => REFERENCED_KEYS,
            "indexes" => INDEX_KEYS,
            "triggers" => TRIGGER_KEYS,
            "usage" => USAGE_KEYS,
            "producer" => &["name"],
            _ => return None,
        })
    }

    fn walk(v: &serde_json::Value, path: &str, known: &[&str], out: &mut BTreeSet<String>) {
        let Some(map) = v.as_object() else { return };
        for (k, child) in map {
            if !known.contains(&k.as_str()) {
                out.insert(if path.is_empty() {
                    k.clone()
                } else {
                    format!("{path}.{k}")
                });
                continue;
            }
            let Some(keys) = container_keys(k) else {
                continue;
            };
            let base = if path.is_empty() {
                k.clone()
            } else {
                format!("{path}.{k}")
            };
            if let Some(items) = child.as_array() {
                for item in items {
                    walk(item, &format!("{base}[]"), keys, out);
                }
            } else {
                walk(child, &base, keys, out);
            }
        }
    }

    let mut out = BTreeSet::new();
    let keys = if doc.get("version").and_then(serde_json::Value::as_u64) == Some(2) {
        DOC_V2_KEYS
    } else {
        DOC_KEYS
    };
    walk(doc, "", keys, &mut out);
    out.into_iter().take(MAX).collect()
}

const DOC_KEYS: &[&str] = &["version", "dialect", "reader", "schemas", "limitations"];
const DOC_V2_KEYS: &[&str] = &[
    "version",
    "dialect",
    "producer",
    "required_features",
    "schemas",
    "limitations",
];
const SCHEMA_KEYS: &[&str] = &["name", "objects", "routines"];
const OBJECT_KEYS: &[&str] = &[
    "name",
    "kind",
    "columns",
    "constraints",
    "indexes",
    "triggers",
    "body",
    "usage",
];
const COLUMN_KEYS: &[&str] = &[
    "name",
    "data_type",
    "nullable",
    "default",
    "ordinal",
    "pk_position",
];
const CONSTRAINT_KEYS: &[&str] = &["name", "kind", "columns", "referenced"];
const REFERENCED_KEYS: &[&str] = &["schema", "table", "columns"];
const INDEX_KEYS: &[&str] = &["name", "unique", "columns", "usage"];
const TRIGGER_KEYS: &[&str] = &["name", "body"];
const ROUTINE_KEYS: &[&str] = &[
    "name",
    "kind",
    "language",
    "body",
    "signature",
    "usage",
    "member_of",
];
const USAGE_KEYS: &[&str] = &["since", "reads", "writes", "total_ms", "self_ms"];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_fields_are_reported_with_paths() {
        let doc = serde_json::json!({
            "version": 1, "dialect": "sqlite", "reader": "x",
            "future_field": true,
            "schemas": [{
                "name": "main",
                "objects": [{
                    "name": "t", "kind": "table",
                    "columns": [{"name": "id", "data_type": "int", "nullable": false,
                                 "ordinal": 1, "pk_position": 1, "checks": []}],
                    "constraints": [], "indexes": [], "triggers": []
                }],
                "routines": []
            }],
            "limitations": []
        });
        let paths = unknown_field_paths(&doc);
        assert!(paths.contains(&"future_field".to_string()), "{paths:?}");
        assert!(
            paths.contains(&"schemas[].objects[].columns[].checks".to_string()),
            "{paths:?}"
        );
    }

    #[test]
    fn known_fields_produce_no_warnings() {
        let doc = serde_json::json!({
            "version": 1, "dialect": "sqlite", "reader": "x",
            "schemas": [{"name": "main", "objects": [], "routines": []}],
            "limitations": []
        });
        assert!(unknown_field_paths(&doc).is_empty());
    }
}
