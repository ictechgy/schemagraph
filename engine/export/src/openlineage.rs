//! 그래프 계보를 OpenLineage RunEvent(스펙 2-0-2)로 내보낸다.
//!
//! 작업(SQL 몸체)마다 COMPLETE 이벤트 하나를 만들고, 출력 데이터셋에 스키마와
//! 컬럼 계보 facet을 싣는다. 값 계보는 DIRECT(subtype 없음 — 그래프는 그대로
//! 옮김과 변환을 구분해 기록하지 않는다), 조인·필터 사용은 데이터셋 단위
//! INDIRECT JOIN·FILTER다. 정적 분석 결과이므로 실행(run) 자체를 주장하지 않으며,
//! runId는 같은 내용이면 같은 값이 되도록 호출자가 내용에서 만든다.

use schemagraph_analysis::lineage::{IndirectUse, Job};
use schemagraph_core::{Graph, VertexId, VertexKind};
use serde_json::{json, Value};
use std::collections::BTreeMap;

/// RunEvent 스키마 URL.
const RUN_EVENT_SCHEMA: &str = "https://openlineage.io/spec/2-0-2/OpenLineage.json#/$defs/RunEvent";
/// 컬럼 계보 facet 스키마 URL.
const COLUMN_LINEAGE_SCHEMA: &str =
    "https://openlineage.io/spec/facets/1-2-0/ColumnLineageDatasetFacet.json#/$defs/ColumnLineageDatasetFacet";
/// 스키마 facet 스키마 URL.
const SCHEMA_FACET_SCHEMA: &str =
    "https://openlineage.io/spec/facets/1-1-1/SchemaDatasetFacet.json#/$defs/SchemaDatasetFacet";

/// 모든 이벤트에 공통인 값.
pub struct EventContext<'a> {
    /// 데이터셋·작업 namespace — 예: `postgres://db.example:5432`.
    pub namespace: &'a str,
    /// 데이터셋 이름 앞에 붙일 데이터베이스 이름(OpenLineage 명명 규칙).
    pub database: Option<&'a str>,
    /// 이벤트 시각(RFC 3339).
    pub event_time: &'a str,
    /// 생산자 URI — 도구와 버전을 가리킨다.
    pub producer: &'a str,
}

/// 내보내기 결과.
pub struct Events {
    /// 작업 id 순의 RunEvent.
    pub events: Vec<Value>,
    /// 출력이 여럿이라 간접 사용을 싣지 않은 작업 — 어느 출력의 조건인지 그래프가 구분하지 않는다.
    pub indirect_omitted: Vec<VertexId>,
}

/// 작업들을 RunEvent로 만든다. `run_id`는 runId를 뺀 이벤트 내용으로 id를 만든다.
pub fn events(
    graph: &Graph,
    jobs: &BTreeMap<VertexId, Job>,
    context: &EventContext,
    run_id: impl Fn(&Value) -> String,
) -> Events {
    let columns = columns_by_dataset(graph);
    let mut result = Events {
        events: Vec::new(),
        indirect_omitted: Vec::new(),
    };
    for (job_id, job) in jobs {
        let include_indirect = job.outputs.len() == 1;
        if !include_indirect && !job.indirect.is_empty() {
            result.indirect_omitted.push(job_id.clone());
        }
        let mut event = json!({
            "eventType": "COMPLETE",
            "eventTime": context.event_time,
            "producer": context.producer,
            "schemaURL": RUN_EVENT_SCHEMA,
            "job": {"namespace": context.namespace, "name": job_id.as_str()},
            "inputs": job.inputs.iter().map(|id| dataset(graph, context, id)).collect::<Vec<_>>(),
            "outputs": job.outputs.iter().map(|id| {
                output(graph, context, &columns, job, id, include_indirect)
            }).collect::<Vec<_>>(),
        });
        let id = run_id(&event);
        event["run"] = json!({"runId": id});
        result.events.push(event);
    }
    result
}

/// OpenLineage 데이터셋 이름 — `[database.]schema.object`.
fn dataset_name(graph: &Graph, context: &EventContext, id: &VertexId) -> String {
    let (schema, name) = graph
        .vertex(id)
        .map_or((String::new(), id.as_str().to_owned()), |v| {
            (v.schema.clone(), v.name.clone())
        });
    match context.database {
        Some(database) => format!("{database}.{schema}.{name}"),
        None => format!("{schema}.{name}"),
    }
}

/// 입력 데이터셋 참조.
fn dataset(graph: &Graph, context: &EventContext, id: &VertexId) -> Value {
    json!({"namespace": context.namespace, "name": dataset_name(graph, context, id)})
}

/// 데이터셋별 컬럼 정점(순서: 수집된 선언 순서, 없으면 id).
fn columns_by_dataset(graph: &Graph) -> BTreeMap<VertexId, Vec<VertexId>> {
    let ordinal = |id: &VertexId| {
        graph
            .schema_metadata()
            .and_then(|metadata| metadata.columns.get(id))
            .map_or(u32::MAX, |column| column.ordinal)
    };
    let mut columns = BTreeMap::<VertexId, Vec<VertexId>>::new();
    for vertex in graph.vertices().filter(|v| v.kind == VertexKind::Column) {
        if let Some(parent) = vertex.id.parent() {
            columns.entry(parent).or_default().push(vertex.id.clone());
        }
    }
    for list in columns.values_mut() {
        list.sort_by_key(|id| (ordinal(id), id.clone()));
    }
    columns
}

/// 출력 데이터셋과 스키마·컬럼 계보 facet.
fn output(
    graph: &Graph,
    context: &EventContext,
    columns: &BTreeMap<VertexId, Vec<VertexId>>,
    job: &Job,
    id: &VertexId,
    include_indirect: bool,
) -> Value {
    let mut value = dataset(graph, context, id);
    let owned: Vec<&VertexId> = columns.get(id).into_iter().flatten().collect();
    let mut facets = json!({"schema": schema_facet(graph, context, &owned)});
    let fields = direct_fields(graph, context, job, &owned);
    let indirect = if include_indirect {
        indirect_fields(graph, context, job)
    } else {
        Vec::new()
    };
    if !fields.is_empty() || !indirect.is_empty() {
        let mut lineage = json!({
            "_producer": context.producer,
            "_schemaURL": COLUMN_LINEAGE_SCHEMA,
            "fields": fields,
        });
        if !indirect.is_empty() {
            lineage["dataset"] = json!(indirect);
        }
        facets["columnLineage"] = lineage;
    }
    value["facets"] = facets;
    value
}

/// 스키마 facet — 컬럼 이름과 수집된 선언 타입.
fn schema_facet(graph: &Graph, context: &EventContext, columns: &[&VertexId]) -> Value {
    let fields: Vec<Value> = columns
        .iter()
        .filter_map(|id| graph.vertex(id))
        .map(|column| {
            let mut field = json!({"name": column.name});
            let data_type = graph
                .schema_metadata()
                .and_then(|metadata| metadata.columns.get(&column.id))
                .map(|metadata| metadata.data_type.clone());
            if let Some(data_type) = data_type {
                field["type"] = json!(data_type);
            }
            field
        })
        .collect();
    json!({"_producer": context.producer, "_schemaURL": SCHEMA_FACET_SCHEMA, "fields": fields})
}

/// 출력 컬럼별 DIRECT 입력 컬럼.
fn direct_fields(
    graph: &Graph,
    context: &EventContext,
    job: &Job,
    columns: &[&VertexId],
) -> serde_json::Map<String, Value> {
    let mut fields = serde_json::Map::new();
    for column in columns {
        let Some(sources) = job.direct.get(*column) else {
            continue;
        };
        let inputs: Vec<Value> = sources
            .iter()
            .filter_map(|source| input_field(graph, context, source, &[json!({"type": "DIRECT"})]))
            .collect();
        if let (false, Some(vertex)) = (inputs.is_empty(), graph.vertex(column)) {
            fields.insert(vertex.name.clone(), json!({"inputFields": inputs}));
        }
    }
    fields
}

/// 데이터셋 단위 INDIRECT 사용(조인·필터 컬럼).
fn indirect_fields(graph: &Graph, context: &EventContext, job: &Job) -> Vec<Value> {
    job.indirect
        .iter()
        .filter_map(|(column, uses)| {
            let transformations: Vec<Value> = uses
                .iter()
                .map(|kind| json!({"type": "INDIRECT", "subtype": subtype(*kind)}))
                .collect();
            input_field(graph, context, column, &transformations)
        })
        .collect()
}

/// 입력 컬럼 참조 — 컬럼의 데이터셋을 알 수 없으면 싣지 않는다(유령 참조 금지).
fn input_field(
    graph: &Graph,
    context: &EventContext,
    column: &VertexId,
    transformations: &[Value],
) -> Option<Value> {
    let vertex = graph.vertex(column)?;
    let parent = column.parent()?;
    graph.vertex(&parent)?;
    Some(json!({
        "namespace": context.namespace,
        "name": dataset_name(graph, context, &parent),
        "field": vertex.name,
        "transformations": transformations,
    }))
}

/// OpenLineage INDIRECT subtype 이름.
fn subtype(kind: IndirectUse) -> &'static str {
    match kind {
        IndirectUse::Join => "JOIN",
        IndirectUse::Filter => "FILTER",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use schemagraph_analysis::lineage::jobs;
    use schemagraph_core::{Edge, EdgeKind, Origin, Vertex};

    fn id(raw: &str) -> VertexId {
        VertexId::from_raw(raw)
    }

    /// 뷰 report(orders 조인, name 값 계보)와 두 테이블에 쓰는 함수 load를 갖는다.
    fn graph() -> Graph {
        let mut graph = Graph::new();
        for (raw, kind) in [
            ("s.orders", VertexKind::Table),
            ("s.orders.cid", VertexKind::Column),
            ("s.orders.name", VertexKind::Column),
            ("s.audit", VertexKind::Table),
            ("s.audit.name", VertexKind::Column),
            ("s.report", VertexKind::View),
            ("s.report.name", VertexKind::Column),
            ("s.load", VertexKind::Function),
        ] {
            graph.add_vertex(Vertex {
                id: id(raw),
                kind,
                name: raw.rsplit('.').next().unwrap().into(),
                schema: "s".into(),
            });
        }
        let mut edge = |from: &str, to: &str, kind, role: &str| {
            graph.add_edge(Edge {
                from: id(from),
                to: id(to),
                kind,
                evidence: vec![],
            });
            graph.add_origin(
                (id(from), id(to), kind),
                Origin {
                    body_hash: "sha256:t".into(),
                    role: role.into(),
                    location: None,
                },
            );
        };
        edge("s.report", "s.orders", EdgeKind::Reads, "relation");
        edge("s.report", "s.orders.cid", EdgeKind::Reads, "join");
        edge(
            "s.report.name",
            "s.orders.name",
            EdgeKind::DerivesFrom,
            "value",
        );
        edge(
            "s.load",
            "s.orders.cid",
            EdgeKind::Reads,
            "dml-owner:s.load:predicate",
        );
        edge(
            "s.load",
            "s.orders",
            EdgeKind::Writes,
            "dml-owner:s.load:write",
        );
        edge(
            "s.load",
            "s.audit",
            EdgeKind::Writes,
            "dml-owner:s.load:write",
        );
        graph
    }

    fn context() -> EventContext<'static> {
        EventContext {
            namespace: "postgres://db:5432",
            database: Some("app"),
            event_time: "2026-09-25T00:00:00Z",
            producer: "https://example.test/schemagraph",
        }
    }

    #[test]
    fn view_event_carries_direct_and_join_lineage() {
        let graph = graph();
        let result = events(&graph, &jobs(&graph), &context(), |_| "run".into());
        let report = result
            .events
            .iter()
            .find(|e| e["job"]["name"] == "s.report")
            .unwrap();
        assert_eq!(report["eventType"], "COMPLETE");
        assert_eq!(report["run"]["runId"], "run");
        assert_eq!(
            report["inputs"],
            json!([{"namespace": "postgres://db:5432", "name": "app.s.orders"}])
        );
        let lineage = &report["outputs"][0]["facets"]["columnLineage"];
        assert_eq!(
            lineage["fields"]["name"]["inputFields"][0],
            json!({"namespace": "postgres://db:5432", "name": "app.s.orders", "field": "name",
                   "transformations": [{"type": "DIRECT"}]})
        );
        assert_eq!(lineage["dataset"][0]["field"], "cid");
        assert_eq!(
            lineage["dataset"][0]["transformations"],
            json!([{"type": "INDIRECT", "subtype": "JOIN"}])
        );
    }

    #[test]
    fn multi_output_job_omits_indirect_instead_of_guessing() {
        let graph = graph();
        let result = events(&graph, &jobs(&graph), &context(), |_| "run".into());
        assert_eq!(result.indirect_omitted, vec![id("s.load")]);
        let load = result
            .events
            .iter()
            .find(|e| e["job"]["name"] == "s.load")
            .unwrap();
        for output in load["outputs"].as_array().unwrap() {
            assert!(output["facets"].get("columnLineage").is_none(), "{output}");
        }
    }
}
