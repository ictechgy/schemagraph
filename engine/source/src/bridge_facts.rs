//! isthmus `bridge-facts` v1 — 카탈로그 선언을 relation-decl 사실로 옮긴다.
//!
//! 계약의 정본은 ../isthmus/docs/GRAPH-EXCHANGE.md다. 이 모듈이 하는 일은
//! 카탈로그 객체를 조인 가능한 이름 형태로 옮기는 것뿐이다 — 사용과의
//! 매칭, 모호성 판정, 진단은 모두 isthmus의 조인기가 한다.
//!
//! 카탈로그 객체에는 소스 위치가 없으므로 relation-decl의 location을
//! 생략하고 `symbol.qualifiedName`에 정규 id(`schema.object[.member]`)를
//! 싣는다 — 이 id는 그래프 정점 id와 같아 소비자가 두 문서를 잇는다.
//! 같은 id를 `symbol.usr`에도 싣는다. 계약상 usr는 생산자의 안정 정점
//! 식별자 자리이고, 다른 생산자 문서도 usr를 impact 질의 id로 쓰므로 소비자가
//! 플랫폼별 예외 없이 usr를 그대로 `query`·`impact`에 넘길 수 있어야 한다.
//! 예외는 SQLite 예약 관계(`sqlite_master` 등)다 — 그래프 정점이 아니라서 usr 없이
//! qualifiedName만 싣는다(`sqlite_system_relations`).

use schemagraph_core::VertexId;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

use crate::document::{CatalogDocument, ObjectDoc};

/// 카탈로그가 관계로 취급하는 객체 종류다 — sequence·type·synonym 같은
/// 비관계 객체는 SQL 관계 조인 키와 다른 의미라 선언하지 않는다.
const RELATION_KINDS: &[&str] = &["table", "view", "materialized-view"];

/// 카탈로그 문서를 isthmus bridge-facts v1 문서로 변환한다.
///
/// `project`는 호출 측 문서와 공유하는 realpath여야 하고 `generated_at`은
/// RFC 3339 타임스탬프다 — 두 값 모두 이 문서가 아니라 호출자의 관측이다.
/// 사실 목록은 (channel, method) 순으로 정렬해 결정적 출력을 보장한다.
pub fn bridge_facts_document(
    doc: &CatalogDocument,
    project: &str,
    tool_version: &str,
    generated_at: &str,
) -> Value {
    let mut facts: Vec<Value> = Vec::new();
    // 소비자가 거부하는 이름의 선언 수 — 빼되 센다.
    let mut omitted = Omitted::default();
    for schema in &doc.schemas {
        let schema_safe = is_exchange_safe(&schema.name);
        for object in &schema.objects {
            if !RELATION_KINDS.contains(&object.kind.as_str()) {
                continue;
            }
            if !schema_safe {
                omitted.schema_named += 1 + object.columns.len();
                continue;
            }
            if !is_exchange_safe(&object.name) {
                omitted.member_named += 1 + object.columns.len();
                continue;
            }
            let channel = format!(
                "{}.{}",
                escape_channel_segment(&schema.name),
                escape_channel_segment(&object.name)
            );
            let qualified = VertexId::object(&schema.name, &object.name);
            let mut relation = declaration_fact(&channel, None, qualified.as_str());
            relation["dataContract"] = relation_contract(object);
            facts.push(relation);
            for column in &object.columns {
                if !is_exchange_safe_member(&column.name) {
                    omitted.member_named += 1;
                    continue;
                }
                let member = VertexId::member(&schema.name, &object.name, &column.name);
                let mut fact = declaration_fact(&channel, Some(&column.name), member.as_str());
                fact["dataContract"] =
                    bounded_contract(vec![json!({"in": "column", "path": column.name,
                    "type": column.data_type, "nullable": column.nullable})]);
                facts.push(fact);
            }
        }
    }
    let (system_facts, system_note) = sqlite_system_relations(doc);
    facts.extend(system_facts);
    // 카탈로그 수집 순서를 신뢰하지 않고 사실을 정렬한다 — reader가 달라도
    // 같은 선언 집합은 같은 문서가 되어야 diff와 캐시가 유효하다.
    facts.sort_by(|a, b| fact_key(a).cmp(&fact_key(b)));

    let mut limitations: Vec<String> = doc.limitations.clone();
    // 카탈로그가 불완전하면 "선언이 없다"가 아니라 "보지 못했다"가 되어야
    // 한다 — isthmus가 이 접두사로 미선언 진단을 -unverified로 내린다.
    match &doc.context {
        Some(context) if context.catalog_complete => {}
        Some(_) => limitations.push(
            "catalog-coverage: the collector declared an incomplete catalog; unreported relations may exist".into(),
        ),
        None => limitations.push(
            "catalog-coverage: no collection context; catalog completeness is unknown".into(),
        ),
    }
    // catalog_complete는 "필터 안에서" 완전하다는 뜻이다. 필터 밖 스키마는 보지
    // 않았으므로 거기 있는 관계의 사용을 미선언 error로 만들면 안 된다. 놓친
    // 관계 이름을 열거할 수 없어 limitationScopes로 좁히지 않는다.
    if let Some(filter) = doc
        .context
        .as_ref()
        .and_then(|context| context.schema_filter.as_ref())
        .filter(|filter| !filter.is_empty())
    {
        limitations.push(schema_filter_limitation(filter));
    }
    // 스키마 이름이 원인이면 비한정 사용이 빠진 선언을 가리킬 수 있어 커버리지
    // 공백이다. 객체·컬럼 이름이 원인이면 같은 이름을 쓰는 유효한 사용 사실이 있을
    // 수 없으므로(사용 쪽도 같은 검증을 받는다) 미선언 진단을 낮추지 않고 세기만 한다.
    if omitted.schema_named > 0 {
        limitations.push(format!(
            "catalog-coverage: {} relation or column declaration(s) were omitted because their schema name is empty or contains control characters the exchange format rejects; unqualified uses may name them",
            omitted.schema_named
        ));
    }
    if omitted.member_named > 0 {
        limitations.push(format!(
            "unexchangeable-identifiers: {} relation or column declaration(s) were omitted because the exchange format rejects their names (empty, whitespace-only column names, or control characters); no valid relation-use can name them",
            omitted.member_named
        ));
    }
    limitations.extend(system_note);
    limitations.sort();
    limitations.dedup();

    // 계약: target은 사실이 있을 때만 설정된다 — 빈 문서는 target null이다.
    let target = if facts.is_empty() {
        Value::Null
    } else {
        json!("persistence")
    };
    json!({
        "format": "bridge-facts",
        "version": 1,
        "tool": { "name": "schemagraph", "version": tool_version },
        "generatedAt": generated_at,
        "platform": "sql",
        "target": target,
        "project": project,
        "facts": facts,
        "limitations": limitations,
    })
}

/// 컬럼 선언과 제약의 구조 해시를 전달한다. CHECK 본문·런타임 의미는 카탈로그가 주지 않아 추측하지 않는다.
fn relation_contract(object: &ObjectDoc) -> Value {
    let mut fields: Vec<Value> = object.columns.iter().map(|column| json!({
        "in": "column", "path": column.name, "type": column.data_type, "nullable": column.nullable,
    })).collect();
    for constraint in &object.constraints {
        // 이름 변경과 구조 변경을 구분하려고 이름은 키, 나머지 구조는 해시로 보존한다.
        let structure = json!({"kind": constraint.kind, "columns": constraint.columns, "referenced": constraint.referenced});
        let digest = Sha256::digest(serde_json::to_vec(&structure).expect("JSON values serialize"));
        fields.push(json!({"in": "constraint", "path": constraint.name, "type": format!("sha256:{digest:x}")}));
    }
    bounded_contract(fields)
}

/// 교환 계약의 입력 상한을 지키며 빠진 근거는 명시한다. 한계가 빈 성공으로 바뀌지 않는다.
fn bounded_contract(mut fields: Vec<Value>) -> Value {
    let mut identities = BTreeMap::new();
    for field in &fields {
        *identities
            .entry((
                field["in"].as_str().unwrap_or_default().to_owned(),
                field["path"].as_str().unwrap_or_default().to_owned(),
            ))
            .or_insert(0usize) += 1;
    }
    let ambiguous = identities.values().filter(|count| **count > 1).count();
    fields.sort_by_key(|field| {
        (
            field["in"].as_str().unwrap_or_default().to_owned(),
            field["path"].as_str().unwrap_or_default().to_owned(),
        )
    });
    let original = fields.len();
    fields.retain(|field| {
        identities.get(&(
            field["in"].as_str().unwrap_or_default().to_owned(),
            field["path"].as_str().unwrap_or_default().to_owned(),
        )) == Some(&1)
            && ["path", "type"].iter().all(|key| {
                field[key].as_str().is_some_and(|text| {
                    !text.is_empty()
                        && text.encode_utf16().count() <= 1024
                        && !text.chars().any(char::is_control)
                })
            })
    });
    fields.truncate(512);
    let mut limitations = vec!["Catalog field evidence only; defaults, CHECK expressions, runtime values and application compatibility are not assessed.".to_owned()];
    if ambiguous > 0 {
        limitations.push(format!(
            "contract-field-ambiguity: {ambiguous} duplicate field identities omitted"
        ));
    }
    if fields.len() < original {
        limitations.push(format!(
            "contract-field-limit: {} fields omitted by ambiguity or the 512-field/string budget",
            original - fields.len()
        ));
    }

    json!({"version": 1, "fields": fields, "limitations": limitations})
}

/// relation-decl 사실 하나를 만든다. `method`가 있으면 컬럼 선언이다.
///
/// `vertex_id`는 같은 카탈로그로 만든 그래프의 정점 id다. qualifiedName(사람이
/// 읽는 정규 이름)과 usr(조인에 쓰는 안정 식별자)에 같은 값을 싣는다 — 카탈로그
/// 정점의 정규 이름이 곧 정점 id이기 때문이다.
fn declaration_fact(channel: &str, method: Option<&str>, vertex_id: &str) -> Value {
    let mut fact = json!({
        "kind": "relation-decl",
        "channel": channel,
        "dynamic": false,
        "symbol": { "qualifiedName": vertex_id, "usr": vertex_id },
    });
    if let Some(column) = method {
        fact["method"] = json!(column);
    }
    fact
}

/// 사실의 정렬 키다 — 관계 이름, 그다음 컬럼 이름 순이다.
fn fact_key(fact: &Value) -> (&str, &str) {
    (
        fact["channel"].as_str().unwrap_or_default(),
        fact["method"].as_str().unwrap_or_default(),
    )
}

/// 한계 문구에 싣는 필터 스키마 이름의 상한이다. 이름 목록은 사람이 확인하는
/// 단서일 뿐이라, 긴 필터가 한계 문자열을 끝없이 키우지 않게 한다.
const SCHEMA_FILTER_NAMES_SHOWN: usize = 10;

/// 스키마 필터 수집의 범위 공백 문구다. 이름의 제어문자는 이스케이프해
/// 한계 문자열 자체가 출력이나 소비자 표시를 깨지 않게 한다.
fn schema_filter_limitation(filter: &[String]) -> String {
    let shown: Vec<String> = filter
        .iter()
        .take(SCHEMA_FILTER_NAMES_SHOWN)
        .map(|name| printable(name))
        .collect();
    let rest = filter.len().saturating_sub(SCHEMA_FILTER_NAMES_SHOWN);
    let more = if rest > 0 {
        format!(" and {rest} more")
    } else {
        String::new()
    };
    format!(
        "catalog-coverage: collection was limited to {} requested schema(s) ({}{more}); relations in other schemas were not scanned",
        filter.len(),
        shown.join(", ")
    )
}

/// 제어문자만 `\u{..}` 형태로 바꾼다 — 나머지 이름은 그대로 읽히게 둔다.
fn printable(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_control() || c == '\u{2028}' || c == '\u{2029}' {
                c.escape_unicode().to_string()
            } else {
                c.to_string()
            }
        })
        .collect()
}

/// isthmus가 채널·컬럼 이름으로 받아들이는 문자열인지 본다.
///
/// 소비자는 비어 있거나 C0·DEL·C1 제어문자, U+2028·U+2029가 든 이름을 문서
/// 전체의 입력 오류로 거부한다(parse.ts `controlCharacterPattern`). 그런 선언
/// 하나 때문에 조인 전체가 실패하지 않도록 생산자가 먼저 걸러야 한다.
fn is_exchange_safe(name: &str) -> bool {
    !name.is_empty()
        && !name
            .chars()
            .any(|c| c.is_control() || c == '\u{2028}' || c == '\u{2029}')
}

/// isthmus가 컬럼(`method`) 이름으로 받아들이는 문자열인지 본다.
///
/// 채널 세그먼트 규칙에 더해, 소비자는 trim한 뒤 비는 이름(공백·NBSP·BOM만 있는 이름)도
/// `isNonEmptyString`으로 거부한다. 채널은 `schema.name` 한정 형태라 trim 뒤에도 비지
/// 않지만 `method`는 컬럼 이름 그 자체라 그런 컬럼 하나가 조인 전체를 실패시킨다.
fn is_exchange_safe_member(name: &str) -> bool {
    is_exchange_safe(name) && !is_blank_for_consumer(name)
}

/// JavaScript `String.prototype.trim`이 모두 지우는 이름인가.
///
/// Rust `char::is_whitespace`(Unicode White_Space)에는 JS가 공백으로 보는 U+FEFF(BOM)가
/// 없어 따로 더한다. 반대로 Rust만 공백으로 보는 U+0085는 제어문자라 이미 거부된다.
fn is_blank_for_consumer(name: &str) -> bool {
    name.chars().all(|c| c.is_whitespace() || c == '\u{feff}')
}

/// 교환 형식이 거부해 뺀 선언 수를 원인별로 센다.
#[derive(Default)]
struct Omitted {
    /// 스키마 이름이 원인인 선언 수다. 비한정 사용이 가리킬 수 있어 커버리지 공백이다.
    schema_named: usize,
    /// 객체·컬럼 이름이 원인인 선언 수다. 같은 이름의 유효한 사용이 있을 수 없다.
    member_named: usize,
}

/// SQLite가 모든 스키마에 두는 스키마 테이블의 이름이다(sqlite.org/schematab.html).
const SQLITE_SCHEMA_TABLES: [&str; 2] = ["sqlite_schema", "sqlite_master"];

/// 모든 연결에 있는 temp 스키마의 스키마 테이블 이름이다.
const SQLITE_TEMP_SCHEMA_TABLES: [&str; 2] = ["sqlite_temp_schema", "sqlite_temp_master"];

/// SQLite 스키마 테이블의 컬럼이다.
const SQLITE_SCHEMA_COLUMNS: [&str; 5] = ["type", "name", "tbl_name", "rootpage", "sql"];

/// AUTOINCREMENT 테이블이 있으면 SQLite가 만드는 `sqlite_sequence`의 컬럼이다.
const SQLITE_SEQUENCE_COLUMNS: [&str; 2] = ["name", "seq"];

/// SQLite 문서에서 존재가 확실한 예약 관계의 선언 사실과, 판정하지 못한 공백을 만든다.
///
/// 수집기는 `sqlite_` 접두 객체를 카탈로그와 그래프에서 뺀다 — 그래프 의미론의 대상이
/// 아니다. 그러나 애플리케이션은 `sqlite_master`·`sqlite_sequence`를 흔히 쓰므로,
/// 선언이 없으면 isthmus가 그 사용을 미선언 error로 만든다. 존재가 확실한 것만
/// 선언한다. 스키마 테이블은 모든 스키마에 있고, `sqlite_sequence`는 그 스키마의 테이블
/// DDL에 AUTOINCREMENT가 있을 때만 있다. `sqlite_stat*`는 ANALYZE 이력에 달려 선언하지
/// 않는다. 그래프 정점이 아니므로 usr 없이 qualifiedName만 싣는다 — usr를 받은 소비자가
/// impact로 넘기는 체인에 정점 없는 id가 들어가지 않게 한다.
fn sqlite_system_relations(doc: &CatalogDocument) -> (Vec<Value>, Option<String>) {
    if doc.dialect != "sqlite" {
        return (Vec::new(), None);
    }
    let mut relations: Vec<(&str, &str, &[&str])> = Vec::new();
    let mut undetermined = 0usize;
    for schema in doc
        .schemas
        .iter()
        .filter(|schema| is_exchange_safe(&schema.name))
    {
        for table in SQLITE_SCHEMA_TABLES {
            relations.push((&schema.name, table, &SQLITE_SCHEMA_COLUMNS));
        }
        let tables: Vec<&ObjectDoc> = schema
            .objects
            .iter()
            .filter(|object| object.kind == "table")
            .collect();
        let autoincrement = tables.iter().any(|table| {
            table
                .body
                .as_deref()
                .is_some_and(crate::sqlite::declares_autoincrement)
        });
        if autoincrement {
            relations.push((&schema.name, "sqlite_sequence", &SQLITE_SEQUENCE_COLUMNS));
        } else {
            undetermined += tables.iter().filter(|table| table.body.is_none()).count();
        }
    }
    for table in SQLITE_TEMP_SCHEMA_TABLES {
        relations.push(("temp", table, &SQLITE_SCHEMA_COLUMNS));
    }
    let mut facts = Vec::new();
    for (schema, name, columns) in relations {
        let channel = format!(
            "{}.{}",
            escape_channel_segment(schema),
            escape_channel_segment(name)
        );
        let relation = VertexId::object(schema, name);
        facts.push(system_declaration_fact(&channel, None, relation.as_str()));
        for column in columns {
            let member = VertexId::member(schema, name, column);
            facts.push(system_declaration_fact(
                &channel,
                Some(column),
                member.as_str(),
            ));
        }
    }
    let note = (undetermined > 0).then(|| {
        format!(
            "sqlite-system-relations: {undetermined} table(s) had no collected DDL, so the reserved sqlite_sequence table was not declared; uses of it may be reported as undeclared"
        )
    });
    (facts, note)
}

/// 그래프 정점이 아닌 예약 관계의 선언 사실이다 — usr 없이 qualifiedName만 싣는다.
fn system_declaration_fact(channel: &str, method: Option<&str>, qualified: &str) -> Value {
    let mut fact = json!({
        "kind": "relation-decl",
        "channel": channel,
        "dynamic": false,
        "symbol": { "qualifiedName": qualified },
    });
    if let Some(column) = method {
        fact["method"] = json!(column);
    }
    fact
}

/// channel의 한 세그먼트를 escape한다 — `%`를 먼저 escape해야 생산자가
/// 받은 이름과 escape된 이름이 충돌하지 않는다. 계약의 구분자는 `.`다.
fn escape_channel_segment(segment: &str) -> String {
    segment.replace('%', "%25").replace('.', "%2E")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::document::{CollectionContext, ColumnDoc, IndexDoc, ObjectDoc, SchemaDoc};
    use schemagraph_core::VertexKind;

    #[test]
    fn contract_omits_ambiguous_columns_constraints_and_oversized_utf16() {
        let contract = bounded_contract(vec![
            json!({"in":"column","path":"email","type":"text"}),
            json!({"in":"column","path":"email","type":"integer"}),
            json!({"in":"constraint","path":"unique_email","type":"hash-a"}),
            json!({"in":"constraint","path":"unique_email","type":"hash-b"}),
            json!({"in":"column","path":"😀".repeat(513),"type":"text"}),
            json!({"in":"column","path":"valid","type":"text"}),
        ]);
        assert_eq!(
            contract["fields"],
            json!([{"in":"column","path":"valid","type":"text"}])
        );
        assert!(contract["limitations"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item
                .as_str()
                .unwrap()
                .contains("2 duplicate field identities omitted")));
    }

    fn catalog() -> CatalogDocument {
        CatalogDocument {
            version: 1,
            dialect: "postgres".into(),
            reader: "test".into(),
            schemas: vec![SchemaDoc {
                name: "public".into(),
                objects: vec![
                    ObjectDoc {
                        name: "users".into(),
                        kind: "table".into(),
                        columns: vec![ColumnDoc {
                            name: "id".into(),
                            data_type: "bigint".into(),
                            nullable: false,
                            default: None,
                            ordinal: 1,
                            pk_position: 1,
                        }],
                        constraints: vec![],
                        indexes: vec![],
                        triggers: vec![],
                        body: None,
                        usage: None,
                    },
                    ObjectDoc {
                        name: "events".into(),
                        kind: "view".into(),
                        columns: vec![],
                        constraints: vec![],
                        indexes: vec![],
                        triggers: vec![],
                        body: None,
                        usage: None,
                    },
                    // 비관계 객체는 선언으로 내지 않는다.
                    ObjectDoc {
                        name: "user_seq".into(),
                        kind: "sequence".into(),
                        columns: vec![],
                        constraints: vec![],
                        indexes: vec![],
                        triggers: vec![],
                        body: None,
                        usage: None,
                    },
                ],
                routines: vec![],
            }],
            limitations: vec![],
            context: Some(CollectionContext {
                source_id: "test".into(),
                database: None,
                schema_filter: None,
                catalog_complete: true,
                lower_case_table_names: None,
                collation: None,
            }),
            dependencies: vec![],
        }
    }

    #[test]
    fn relation_decls_carry_qualified_channels_and_columns() {
        let doc = catalog();
        let value = bridge_facts_document(&doc, "/repo", "0.5.0", "2026-01-01T00:00:00Z");
        assert_eq!(value["format"], "bridge-facts");
        assert_eq!(value["platform"], "sql");
        assert_eq!(value["target"], "persistence");
        assert_eq!(value["project"], "/repo");

        let facts = value["facts"].as_array().unwrap();
        // users(테이블)+id(컬럼)+events(뷰) — sequence는 없어야 한다.
        assert_eq!(facts.len(), 3, "{facts:?}");
        assert_eq!(facts[0]["channel"], "public.events");
        assert_eq!(facts[1]["channel"], "public.users");
        assert_eq!(facts[1]["symbol"]["qualifiedName"], "public.users");
        assert_eq!(facts[2]["channel"], "public.users");
        assert_eq!(facts[2]["method"], "id");
        assert_eq!(facts[2]["symbol"]["qualifiedName"], "public.users.id");
        assert_eq!(
            facts[2]["dataContract"]["fields"],
            json!([{"in":"column", "path":"id", "type":"bigint", "nullable":false}])
        );
        for fact in facts {
            assert_eq!(fact["kind"], "relation-decl");
            assert_eq!(fact["dynamic"], false);
            // usr는 qualifiedName과 같은 정점 id다 — symbol에는 두 키만 있다.
            let qualified = &fact["symbol"]["qualifiedName"];
            assert_eq!(
                fact["symbol"],
                json!({ "qualifiedName": qualified, "usr": qualified })
            );
            // 카탈로그 객체에는 소스 위치가 없다 — 키가 아예 없어야 한다.
            assert!(fact.get("location").is_none(), "{fact:?}");
        }
        // 완전한 카탈로그에는 catalog-coverage limitation이 없다.
        assert_eq!(value["limitations"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn same_name_type_nullability_and_constraint_changes_remain_observable() {
        use crate::document::ConstraintDoc;
        let before = catalog();
        let mut after = before.clone();
        let object = &mut after.schemas[0].objects[0];
        object.columns[0].data_type = "text".into();
        object.columns[0].nullable = true;
        object.constraints.push(ConstraintDoc {
            name: "id_unique".into(),
            kind: "unique".into(),
            columns: vec!["id".into()],
            referenced: None,
        });
        let old = bridge_facts_document(&before, "/repo", "test", "2026-10-22T00:00:00Z");
        let new = bridge_facts_document(&after, "/repo", "test", "2026-10-22T00:00:00Z");
        assert_eq!(old["facts"][1]["symbol"], new["facts"][1]["symbol"]);
        assert_ne!(
            old["facts"][1]["dataContract"],
            new["facts"][1]["dataContract"]
        );
        assert_eq!(
            new["facts"][2]["dataContract"]["fields"][0]["nullable"],
            true
        );
        assert_eq!(
            new["facts"][1]["dataContract"]["fields"][1]["in"],
            "constraint"
        );
    }

    #[test]
    fn contract_budget_is_explicit_and_deterministic() {
        let fields = (0..600)
            .rev()
            .map(|i| json!({"in":"column", "path":format!("c{i:03}"), "type":"text"}))
            .collect();
        let contract = bounded_contract(fields);
        assert_eq!(contract["fields"].as_array().unwrap().len(), 512);
        assert!(contract["limitations"][1]
            .as_str()
            .unwrap()
            .contains("88 fields omitted"));
        assert_eq!(contract["fields"][0]["path"], "c000");
    }

    /// usr는 조인 키라서 같은 카탈로그 그래프의 실제 정점이어야 한다. 컬럼과 같은
    /// 이름의 인덱스가 `@index`로 분리돼도 컬럼 선언의 usr는 컬럼 정점을 가리킨다.
    #[test]
    fn every_usr_is_a_vertex_of_the_graph_from_the_same_catalog() {
        let mut doc = catalog();
        doc.schemas[0].objects[0].indexes.push(IndexDoc {
            has_predicate: None,
            definition_complete: None,
            predicate: None,
            name: "id".into(),
            unique: true,
            columns: vec!["id".into()],
            usage: None,
        });
        let graph = crate::graph::document_to_graph(&doc);
        let value = bridge_facts_document(&doc, "/repo", "0.5.0", "2026-01-01T00:00:00Z");
        for fact in value["facts"].as_array().unwrap() {
            let usr = fact["symbol"]["usr"].as_str().expect("usr is a string");
            let vertex = graph
                .vertex(&VertexId::from_raw(usr))
                .unwrap_or_else(|| panic!("ghost usr {usr}"));
            let expected_column = fact.get("method").is_some();
            assert_eq!(vertex.kind == VertexKind::Column, expected_column, "{usr}");
        }
    }

    #[test]
    fn incomplete_catalog_reports_coverage_gap() {
        let mut doc = catalog();
        doc.context.as_mut().unwrap().catalog_complete = false;
        let value = bridge_facts_document(&doc, "/repo", "0.5.0", "2026-01-01T00:00:00Z");
        let limitations = value["limitations"].as_array().unwrap();
        assert!(
            limitations.iter().any(|l| l
                .as_str()
                .unwrap_or_default()
                .starts_with("catalog-coverage:")),
            "{limitations:?}"
        );
    }

    #[test]
    fn missing_context_reports_coverage_gap() {
        let mut doc = catalog();
        doc.context = None;
        let value = bridge_facts_document(&doc, "/repo", "0.5.0", "2026-01-01T00:00:00Z");
        let limitations = value["limitations"].as_array().unwrap();
        assert_eq!(limitations.len(), 1, "{limitations:?}");
        assert!(limitations[0]
            .as_str()
            .unwrap_or_default()
            .starts_with("catalog-coverage:"));
    }

    #[test]
    fn dotted_names_escape_the_segment_not_the_qualifier() {
        let mut doc = catalog();
        doc.schemas[0].objects[0].name = "a.b".into();
        let value = bridge_facts_document(&doc, "/repo", "0.5.0", "2026-01-01T00:00:00Z");
        let facts = value["facts"].as_array().unwrap();
        // 이름 안의 점은 escape되고 한정자는 그대로다.
        assert_eq!(facts[0]["channel"], "public.a%2Eb");
    }

    #[test]
    fn usr_follows_vertex_id_escaping_not_channel_escaping() {
        let mut doc = catalog();
        // `@`·`(`는 channel escape 대상이 아니지만 정점 id 컴포넌트에서는 escape된다.
        doc.schemas[0].objects[0].name = "odd@name(1)".into();
        let value = bridge_facts_document(&doc, "/repo", "0.5.0", "2026-01-01T00:00:00Z");
        let facts = value["facts"].as_array().unwrap();
        let relation = facts
            .iter()
            .find(|fact| fact["channel"] == "public.odd@name(1)" && fact.get("method").is_none())
            .expect("relation fact keeps the raw channel");
        // usr는 그래프 정점 id와 같아야 query·impact가 그대로 해석한다.
        assert_eq!(relation["symbol"]["usr"], "public.odd%40name%281%29");
        assert_eq!(
            relation["symbol"]["usr"],
            VertexId::object("public", "odd@name(1)").as_str()
        );
        let column = facts
            .iter()
            .find(|fact| fact["method"] == "id")
            .expect("column fact exists");
        assert_eq!(
            column["symbol"]["usr"],
            VertexId::member("public", "odd@name(1)", "id").as_str()
        );
    }

    #[test]
    fn empty_catalog_carries_null_target() {
        let mut doc = catalog();
        doc.schemas = vec![];
        let value = bridge_facts_document(&doc, "/repo", "0.5.0", "2026-01-01T00:00:00Z");
        // 계약: target은 사실이 있을 때만 설정된다.
        assert!(value["target"].is_null());
        assert_eq!(value["facts"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn document_limitations_pass_through_sorted() {
        let mut doc = catalog();
        doc.limitations = vec!["z-limit".into(), "a-limit".into()];
        let value = bridge_facts_document(&doc, "/repo", "0.5.0", "2026-01-01T00:00:00Z");
        let limitations: Vec<&str> = value["limitations"]
            .as_array()
            .unwrap()
            .iter()
            .map(|l| l.as_str().unwrap_or_default())
            .collect();
        assert_eq!(limitations, ["a-limit", "z-limit"]);
    }

    /// 스키마 필터로 일부만 수집한 카탈로그는 필터 안에서만 완전하다. 필터 밖
    /// 관계를 쓰는 코드가 isthmus에서 error가 되지 않도록 범위 공백을 신고한다.
    #[test]
    fn schema_filtered_collection_reports_coverage_gap() {
        let mut doc = catalog();
        doc.context.as_mut().unwrap().schema_filter = Some(vec!["public".into()]);
        let value = bridge_facts_document(&doc, "/repo", "0.5.0", "2026-01-01T00:00:00Z");
        let limitations = value["limitations"].as_array().unwrap();
        assert_eq!(limitations.len(), 1, "{limitations:?}");
        let message = limitations[0].as_str().unwrap();
        assert!(message.starts_with("catalog-coverage:"), "{message}");
        assert!(message.contains("1 requested schema"), "{message}");
        assert!(message.contains("public"), "{message}");
        // 이 공백은 채널 집합으로 증명할 수 없어 스코프를 붙이지 않는다.
        assert!(value.get("limitationScopes").is_none());
    }

    /// 필터 이름 목록은 상한이 있고 제어문자를 그대로 싣지 않는다.
    #[test]
    fn schema_filter_coverage_message_is_bounded_and_printable() {
        let mut doc = catalog();
        let mut names: Vec<String> = (0..30).map(|i| format!("s{i:02}")).collect();
        names.push("bad\nname".into());
        doc.context.as_mut().unwrap().schema_filter = Some(names);
        let value = bridge_facts_document(&doc, "/repo", "0.5.0", "2026-01-01T00:00:00Z");
        let message = value["limitations"][0].as_str().unwrap();
        assert!(message.contains("31 requested schema"), "{message}");
        assert!(message.contains("and 21 more"), "{message}");
        assert!(!message.chars().any(char::is_control), "{message}");
    }

    /// isthmus는 제어문자가 든 채널·컬럼 하나로 문서 전체를 거부한다. 그 선언만
    /// 뺀다. 객체·컬럼 이름이 원인이면 그 이름을 쓰는 유효한 사용이 없으므로 미선언
    /// 진단을 낮추는 커버리지 공백이 아니라 계수로 남긴다.
    #[test]
    fn identifiers_with_control_characters_are_omitted_and_counted() {
        let mut doc = catalog();
        let mut odd = doc.schemas[0].objects[0].clone();
        odd.name = "weird\nname".into();
        doc.schemas[0].objects.push(odd);
        let mut column = doc.schemas[0].objects[0].columns[0].clone();
        column.name = "bad\u{2028}col".into();
        doc.schemas[0].objects[0].columns.push(column);
        let value = bridge_facts_document(&doc, "/repo", "0.5.0", "2026-01-01T00:00:00Z");
        let facts = value["facts"].as_array().unwrap();
        // users+id+events만 남는다. weird 테이블(관계+컬럼)과 bad 컬럼은 빠진다.
        assert_eq!(facts.len(), 3, "{facts:?}");
        for fact in facts {
            for key in ["channel", "method"] {
                if let Some(text) = fact[key].as_str() {
                    assert!(is_exchange_safe(text), "{text:?}");
                }
            }
        }
        let limitations = value["limitations"].as_array().unwrap();
        assert!(
            limitations.iter().any(|l| l.as_str().unwrap().starts_with(
                "unexchangeable-identifiers: 3 relation or column declaration(s) were omitted"
            )),
            "{limitations:?}"
        );
        assert!(
            !limitations
                .iter()
                .any(|l| l.as_str().unwrap().starts_with("catalog-coverage:")),
            "{limitations:?}"
        );
    }

    /// isthmus는 trim한 뒤 비는 컬럼 이름(공백·NBSP·BOM만)도 문서 전체의 입력 오류로
    /// 거부한다. 그런 컬럼만 빼고 센다. 공백만 있는 관계 이름은 `schema.name` 채널로는
    /// 받아들여지므로 그대로 선언한다.
    #[test]
    fn blank_column_names_are_omitted_but_blank_relations_are_declared() {
        let mut doc = catalog();
        let template = doc.schemas[0].objects[0].columns[0].clone();
        for name in [" ", "\u{00a0}", "\u{feff}", "\t "] {
            let mut column = template.clone();
            column.name = name.into();
            doc.schemas[0].objects[0].columns.push(column);
        }
        let mut blank = doc.schemas[0].objects[1].clone();
        blank.name = " ".into();
        doc.schemas[0].objects.push(blank);
        let value = bridge_facts_document(&doc, "/repo", "0.5.0", "2026-01-01T00:00:00Z");
        let facts = value["facts"].as_array().unwrap();
        assert!(facts
            .iter()
            .all(|fact| fact["method"].as_str().is_none_or(|column| !column
                .trim_matches(|c: char| c.is_whitespace() || c == '\u{feff}')
                .is_empty())));
        assert!(
            facts.iter().any(|fact| fact["channel"] == "public. "),
            "{facts:?}"
        );
        let limitations = value["limitations"].as_array().unwrap();
        assert!(
            limitations.iter().any(|l| l.as_str().unwrap().starts_with(
                "unexchangeable-identifiers: 4 relation or column declaration(s) were omitted"
            )),
            "{limitations:?}"
        );
    }

    /// 스키마 이름 때문에 뺀 선언은 비한정 사용이 가리킬 수 있어 커버리지 공백이다.
    #[test]
    fn unsafe_schema_names_are_a_coverage_gap() {
        let mut doc = catalog();
        doc.schemas[0].name = "bad\nschema".into();
        let value = bridge_facts_document(&doc, "/repo", "0.5.0", "2026-01-01T00:00:00Z");
        assert_eq!(value["facts"], json!([]));
        let limitations = value["limitations"].as_array().unwrap();
        assert!(
            limitations.iter().any(|l| l.as_str().unwrap().starts_with(
                "catalog-coverage: 3 relation or column declaration(s) were omitted because their schema name"
            )),
            "{limitations:?}"
        );
    }

    fn sqlite_catalog(tables: &[(&str, Option<&str>)]) -> CatalogDocument {
        let mut doc = catalog();
        doc.dialect = "sqlite".into();
        doc.schemas[0].name = "main".into();
        doc.schemas[0].objects = tables
            .iter()
            .map(|(name, body)| ObjectDoc {
                name: (*name).into(),
                kind: "table".into(),
                columns: vec![],
                constraints: vec![],
                indexes: vec![],
                triggers: vec![],
                body: body.map(str::to_owned),
                usage: None,
            })
            .collect();
        doc
    }

    fn channels(value: &Value) -> Vec<String> {
        value["facts"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|fact| fact.get("method").is_none())
            .map(|fact| fact["channel"].as_str().unwrap().to_owned())
            .collect()
    }

    /// 수집기가 뺀 SQLite 예약 관계 중 존재가 확실한 것만 선언한다. 정점이 아니라
    /// usr 없이 qualifiedName만 싣고, 다른 방언에는 붙지 않는다.
    #[test]
    fn sqlite_reserved_relations_are_declared_without_usr() {
        let plain = bridge_facts_document(
            &sqlite_catalog(&[("users", Some("CREATE TABLE users (id INTEGER PRIMARY KEY)"))]),
            "/repo",
            "0.5.0",
            "2026-01-01T00:00:00Z",
        );
        assert_eq!(
            channels(&plain),
            [
                "main.sqlite_master",
                "main.sqlite_schema",
                "main.users",
                "temp.sqlite_temp_master",
                "temp.sqlite_temp_schema"
            ]
        );
        let reserved: Vec<&Value> = plain["facts"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|fact| fact["channel"].as_str().unwrap().contains(".sqlite_"))
            .collect();
        assert_eq!(reserved.len(), 4 * 6, "{reserved:?}");
        assert!(reserved
            .iter()
            .all(|fact| fact["symbol"].get("usr").is_none()
                && fact["symbol"]["qualifiedName"].is_string()));
        assert!(plain["facts"].as_array().unwrap().iter().any(|fact| {
            fact["channel"] == "main.sqlite_master" && fact["method"] == "tbl_name"
        }));
        assert!(
            plain["limitations"].as_array().unwrap().is_empty(),
            "{plain}"
        );

        // AUTOINCREMENT 테이블이 있을 때만 sqlite_sequence가 있다. 인용 이름 안의 글자는 아니다.
        let sequenced = bridge_facts_document(
            &sqlite_catalog(&[
                (
                    "a",
                    Some("CREATE TABLE a (id INTEGER PRIMARY KEY AUTOINCREMENT)"),
                ),
                ("b", Some("CREATE TABLE b (\"autoincrement\" TEXT)")),
            ]),
            "/repo",
            "0.5.0",
            "2026-01-01T00:00:00Z",
        );
        assert!(channels(&sequenced).contains(&"main.sqlite_sequence".to_owned()));
        let quoted = bridge_facts_document(
            &sqlite_catalog(&[("b", Some("CREATE TABLE b (\"autoincrement\" TEXT)"))]),
            "/repo",
            "0.5.0",
            "2026-01-01T00:00:00Z",
        );
        assert!(!channels(&quoted).contains(&"main.sqlite_sequence".to_owned()));

        // DDL을 모으지 않은 수집기 문서는 sqlite_sequence 존재를 모른다고 알린다.
        let unknown = bridge_facts_document(
            &sqlite_catalog(&[("a", None)]),
            "/repo",
            "0.5.0",
            "2026-01-01T00:00:00Z",
        );
        assert!(!channels(&unknown).contains(&"main.sqlite_sequence".to_owned()));
        assert!(unknown["limitations"].as_array().unwrap().iter().any(|l| l
            .as_str()
            .unwrap()
            .starts_with("sqlite-system-relations: 1 table(s)")));

        let postgres = bridge_facts_document(&catalog(), "/repo", "0.5.0", "2026-01-01T00:00:00Z");
        assert!(!channels(&postgres)
            .iter()
            .any(|channel| channel.contains("sqlite_")));
    }
}
