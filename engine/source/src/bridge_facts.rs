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

use schemagraph_core::VertexId;
use serde_json::{json, Value};

use crate::document::CatalogDocument;

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
    for schema in &doc.schemas {
        for object in &schema.objects {
            if !RELATION_KINDS.contains(&object.kind.as_str()) {
                continue;
            }
            let channel = format!(
                "{}.{}",
                escape_channel_segment(&schema.name),
                escape_channel_segment(&object.name)
            );
            let qualified = VertexId::object(&schema.name, &object.name);
            facts.push(declaration_fact(&channel, None, qualified.as_str()));
            for column in &object.columns {
                let member = VertexId::member(&schema.name, &object.name, &column.name);
                facts.push(declaration_fact(
                    &channel,
                    Some(&column.name),
                    member.as_str(),
                ));
            }
        }
    }
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
}
