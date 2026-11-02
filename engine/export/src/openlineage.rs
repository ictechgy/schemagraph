//! 그래프 계보를 OpenLineage RunEvent(스펙 2-0-2)로 내보낸다.
//!
//! 작업(SQL 몸체)마다 COMPLETE 이벤트 하나를 만들고, 출력 데이터셋에 스키마와
//! 컬럼 계보 facet을 싣는다. 값 계보는 DIRECT(subtype 없음 — 그래프는 그대로
//! 옮김과 변환을 구분해 기록하지 않는다), 조인·필터 사용은 데이터셋 단위
//! INDIRECT JOIN·FILTER다. 간접 사용은 단일 SELECT인 뷰에만 싣는다 — 루틴은
//! 조건이 어느 문장(적재·삭제 등)에 속하는지 그래프가 기록하지 않기 때문이다.
//! 분석이 완전하지 않은 작업에는 그 상태를 job facet으로 실어 "계보 없음"과
//! "못 읽음"을 구분하게 한다. 정적 분석 결과이므로 실행(run)을 주장하지 않으며,
//! runId는 같은 내용이면 같은 값이 되도록 호출자가 내용에서 만든다.

use crate::diagnostics::state_str;
use schemagraph_analysis::lineage::{IndirectUse, Job};
use schemagraph_core::{split_namespaced_id, AnalysisState, Graph, VertexId, VertexKind};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};

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
    /// 데이터셋·작업 이름 앞에 붙일 데이터베이스 이름(OpenLineage 명명 규칙).
    pub database: Option<&'a str>,
    /// 합친 그래프의 source별 데이터셋 이름 자리(source id → namespace·데이터베이스). 비어
    /// 있으면 단일 그래프로 보고 `namespace`·`database`를 쓴다. 작업 이름에는 쓰지 않는다.
    pub sources: BTreeMap<String, SourceNaming>,
    /// 이벤트 시각(RFC 3339).
    pub event_time: &'a str,
    /// 생산자 URI — 도구와 버전을 가리킨다.
    pub producer: &'a str,
    /// 분석 상태 job facet을 설명하는 문서 URL(facet의 `_schemaURL`).
    pub analysis_facet_url: &'a str,
}

/// 합친 그래프에서 한 source의 데이터셋이 이름을 얻는 자리.
///
/// 정점의 스키마·객체 이름은 source 안의 로컬 이름이라, source마다 데이터베이스 자리(기본은
/// source id)를 따로 둬야 같은 이름의 테이블이 서로 다른 데이터셋으로 남는다. 작업 이름은
/// 바꾸지 않는다 — 합친 정점 id(`app::public.load`)가 이미 source마다 달라 작업은 겹친 적이
/// 없고, 그대로 둬야 소비자에 쌓인 작업 이력이 이어진다.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceNaming {
    /// 데이터셋 namespace — `--source-namespace`가 없으면 전역 `--namespace`다.
    pub namespace: String,
    /// 데이터셋 이름의 데이터베이스 자리 — `--source-database`가 없으면 source id다.
    pub database: String,
}

impl EventContext<'_> {
    /// 작업의 (namespace, 이름) — 단일·합친 그래프 모두 `[database.]<작업 정점 id>`다.
    fn job_identity(&self, job: &VertexId) -> (&str, String) {
        (self.namespace, qualified(self.database, job.as_str()))
    }

    /// 데이터셋의 (namespace, 이름). 단일 그래프는 `[database.]schema.object`, 합친 그래프는
    /// 정점의 source 자리에서 `database.schema.object`다.
    fn dataset_identity(&self, graph: &Graph, id: &VertexId) -> (&str, String) {
        let local_name = |fallback: &VertexId| {
            graph.vertex(id).map_or_else(
                || fallback.as_str().to_owned(),
                |v| format!("{}.{}", v.schema, v.name),
            )
        };
        if !self.sources.is_empty() {
            if let Some((source, local)) = split_namespaced_id(id) {
                if let Some(naming) = self.sources.get(source) {
                    let name = qualified(Some(&naming.database), &local_name(&local));
                    return (&naming.namespace, name);
                }
            }
        }
        (self.namespace, qualified(self.database, &local_name(id)))
    }
}

/// 내보내기 결과.
pub struct Events {
    /// 작업 id 순의 RunEvent.
    pub events: Vec<Value>,
    /// 간접 사용이 있지만 싣지 않은 작업 — 뷰가 아니라 조건이 속한 문장을 그래프가 구분하지 않는다.
    pub indirect_omitted: Vec<VertexId>,
    /// 분석이 완전하지 않아 계보가 빠졌을 수 있는 작업.
    pub incomplete: Vec<VertexId>,
}

/// 데이터셋별 공유 값 — 컬럼 목록과 스키마 facet을 한 번만 만든다.
struct Datasets {
    columns: BTreeMap<VertexId, Vec<VertexId>>,
    schemas: BTreeMap<VertexId, Value>,
}

/// 작업들을 RunEvent로 만든다. `run_id`는 runId를 뺀 이벤트 내용으로 id를 만든다.
pub fn events(
    graph: &Graph,
    jobs: &BTreeMap<VertexId, Job>,
    context: &EventContext,
    run_id: impl Fn(&Value) -> String,
) -> Events {
    let datasets = datasets(graph, context);
    let mut result = Events {
        events: Vec::new(),
        indirect_omitted: Vec::new(),
        incomplete: Vec::new(),
    };
    for (job_id, job) in jobs {
        let include_indirect = is_view(graph, job_id);
        if !include_indirect && !job.indirect.is_empty() {
            result.indirect_omitted.push(job_id.clone());
        }
        let mut event = event(graph, context, &datasets, job_id, job, include_indirect);
        if let Some(facet) = analysis_facet(graph, context, job_id) {
            event["job"]["facets"] = json!({"schemagraphAnalysis": facet});
            result.incomplete.push(job_id.clone());
        }
        event["run"] = json!({"runId": run_id(&event)});
        result.events.push(event);
    }
    result
}

/// runId를 뺀 이벤트 본문.
fn event(
    graph: &Graph,
    context: &EventContext,
    datasets: &Datasets,
    job_id: &VertexId,
    job: &Job,
    include_indirect: bool,
) -> Value {
    let outputs: Vec<Value> = job
        .outputs
        .iter()
        .map(|id| output(graph, context, datasets, job, id, include_indirect))
        .collect();
    let (namespace, name) = context.job_identity(job_id);
    json!({
        "eventType": "COMPLETE",
        "eventTime": context.event_time,
        "producer": context.producer,
        "schemaURL": RUN_EVENT_SCHEMA,
        "job": {"namespace": namespace, "name": name},
        "inputs": job.inputs.iter().map(|id| dataset(graph, context, id)).collect::<Vec<_>>(),
        "outputs": outputs,
    })
}

/// 단일 SELECT로 정의되는 뷰·materialized view인지.
fn is_view(graph: &Graph, id: &VertexId) -> bool {
    graph
        .vertex(id)
        .is_some_and(|v| matches!(v.kind, VertexKind::View | VertexKind::MaterializedView))
}

/// 분석이 완전하지 않은 작업의 상태 facet — 계보가 빠졌을 수 있음을 이벤트 안에 남긴다.
fn analysis_facet(graph: &Graph, context: &EventContext, job: &VertexId) -> Option<Value> {
    let analysis = graph.analysis().get(job)?;
    (analysis.state != AnalysisState::Complete).then(|| {
        json!({
            "_producer": context.producer,
            "_schemaURL": context.analysis_facet_url,
            "state": state_str(analysis.state),
            "diagnostics": analysis.diagnostics.len(),
        })
    })
}

/// 데이터베이스 자리가 있으면 접두를 붙인다 — 작업과 데이터셋 이름이 같은 규칙을 따른다.
fn qualified(database: Option<&str>, name: &str) -> String {
    match database {
        Some(database) => format!("{database}.{name}"),
        None => name.to_owned(),
    }
}

/// 입력 데이터셋 참조.
fn dataset(graph: &Graph, context: &EventContext, id: &VertexId) -> Value {
    let (namespace, name) = context.dataset_identity(graph, id);
    json!({"namespace": namespace, "name": name})
}

/// 데이터셋별 컬럼(수집된 선언 순서, 없으면 id 순)과 스키마 facet을 만든다.
fn datasets(graph: &Graph, context: &EventContext) -> Datasets {
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
        list.sort_by_cached_key(|id| (ordinal(id), id.clone()));
    }
    let schemas = columns
        .iter()
        .map(|(id, owned)| (id.clone(), schema_facet(graph, context, owned)))
        .collect();
    Datasets { columns, schemas }
}

/// 출력 데이터셋과 스키마·컬럼 계보 facet.
fn output(
    graph: &Graph,
    context: &EventContext,
    datasets: &Datasets,
    job: &Job,
    id: &VertexId,
    include_indirect: bool,
) -> Value {
    let mut value = dataset(graph, context, id);
    let owned: &[VertexId] = datasets.columns.get(id).map_or(&[], Vec::as_slice);
    let schema = datasets
        .schemas
        .get(id)
        .cloned()
        .unwrap_or_else(|| schema_facet(graph, context, &[]));
    let mut facets = json!({"schema": schema});
    let fields = direct_fields(graph, context, job, owned);
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
fn schema_facet(graph: &Graph, context: &EventContext, columns: &[VertexId]) -> Value {
    let fields: Vec<Value> = columns
        .iter()
        .filter_map(|id| graph.vertex(id))
        .map(|column| {
            let mut field = json!({"name": column.name});
            let metadata = graph
                .schema_metadata()
                .and_then(|metadata| metadata.columns.get(&column.id));
            if let Some(metadata) = metadata {
                field["type"] = json!(metadata.data_type);
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
    columns: &[VertexId],
) -> serde_json::Map<String, Value> {
    let direct = [json!({"type": "DIRECT"})];
    let mut fields = serde_json::Map::new();
    for column in columns {
        let Some(sources) = job.direct.get(column) else {
            continue;
        };
        let inputs: Vec<Value> = sources
            .iter()
            .filter_map(|source| input_field(graph, context, source, &direct))
            .collect();
        if inputs.is_empty() {
            continue;
        }
        if let Some(vertex) = graph.vertex(column) {
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
    let (namespace, name) = context.dataset_identity(graph, &parent);
    Some(json!({
        "namespace": namespace,
        "name": name,
        "field": vertex.name,
        "transformations": transformations,
    }))
}

/// 서로 다른 데이터셋 정점이 같은 OpenLineage 정체성(namespace, 이름)을 얻는 경우를 찾는다.
///
/// 소비자는 정체성이 같은 데이터셋을 하나로 합치므로, 내보내면 없는 계보가 생기고 스키마·
/// 컬럼 계보가 서로를 덮는다. 이벤트가 실제로 가리키는 데이터셋만 본다. 작업 이름은 서로
/// 다른 정점 id라 겹치지 않는다. 합친 그래프에서는 사용자가 두 source를 같은 namespace·
/// 데이터베이스로 매핑했을 때만 생기고, 단일 그래프에서는 스키마·객체 이름 안의 `.`이 경계를
/// 흐릴 때(`a.b`의 `c`와 `a`의 `b.c`) 생긴다.
pub fn identity_collisions(
    graph: &Graph,
    jobs: &BTreeMap<VertexId, Job>,
    context: &EventContext,
) -> Vec<String> {
    let datasets = datasets(graph, context);
    let mut names: BTreeMap<(String, String), BTreeSet<VertexId>> = BTreeMap::new();
    for (job_id, job) in jobs {
        for id in referenced_datasets(graph, &datasets, job_id, job) {
            let (namespace, name) = context.dataset_identity(graph, &id);
            names
                .entry((namespace.to_owned(), name))
                .or_default()
                .insert(id);
        }
    }
    names
        .into_iter()
        .filter(|(_, ids)| ids.len() > 1)
        .map(|((namespace, name), ids)| {
            let ids: Vec<&str> = ids.iter().map(VertexId::as_str).collect();
            format!(
                "dataset ({namespace}, {name}) would name {}",
                ids.join(", ")
            )
        })
        .collect()
}

/// 작업의 이벤트가 가리키는 데이터셋 — 입력, 출력, 출력 컬럼의 DIRECT 입력 컬럼과 (뷰라면)
/// INDIRECT 사용 컬럼이 속한 데이터셋. `event`·`output`이 싣는 것과 같은 범위다.
fn referenced_datasets(
    graph: &Graph,
    datasets: &Datasets,
    job_id: &VertexId,
    job: &Job,
) -> BTreeSet<VertexId> {
    let mut referenced: BTreeSet<VertexId> = job.inputs.union(&job.outputs).cloned().collect();
    let column_dataset = |column: &VertexId| {
        graph.vertex(column)?;
        let parent = column.parent()?;
        graph.vertex(&parent).map(|_| parent)
    };
    for output in &job.outputs {
        let owned = datasets.columns.get(output).map_or(&[][..], Vec::as_slice);
        for column in owned {
            let sources = job.direct.get(column).into_iter().flatten();
            referenced.extend(sources.filter_map(column_dataset));
        }
    }
    if is_view(graph, job_id) {
        referenced.extend(job.indirect.keys().filter_map(column_dataset));
    }
    referenced
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
    use schemagraph_core::{Edge, EdgeKind, ObjectAnalysis, Origin, Vertex};

    fn id(raw: &str) -> VertexId {
        VertexId::from_raw(raw)
    }

    /// 뷰 report(orders 조인, name 값 계보), orders에 쓰는 load, 두 테이블에 쓰는 fan을 갖는다.
    fn graph() -> Graph {
        let mut graph = Graph::new();
        for (raw, kind) in [
            ("s.orders", VertexKind::Table),
            ("s.orders.cid", VertexKind::Column),
            ("s.orders.name", VertexKind::Column),
            ("s.audit", VertexKind::Table),
            ("s.report", VertexKind::View),
            ("s.report.name", VertexKind::Column),
            ("s.load", VertexKind::Function),
            ("s.fan", VertexKind::Function),
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
            "s.fan",
            "s.orders",
            EdgeKind::Writes,
            "dml-owner:s.fan:write",
        );
        edge(
            "s.fan",
            "s.audit",
            EdgeKind::Writes,
            "dml-owner:s.fan:write",
        );
        graph
    }

    fn context() -> EventContext<'static> {
        EventContext {
            namespace: "postgres://db:5432",
            database: Some("app"),
            sources: BTreeMap::new(),
            event_time: "2026-09-25T00:00:00Z",
            producer: "https://example.test/schemagraph",
            analysis_facet_url: "https://example.test/analysis",
        }
    }

    fn find<'a>(result: &'a Events, name: &str) -> &'a Value {
        result
            .events
            .iter()
            .find(|e| e["job"]["name"] == name)
            .unwrap()
    }

    #[test]
    fn view_event_carries_direct_and_join_lineage_with_matching_names() {
        let graph = graph();
        let result = events(&graph, &jobs(&graph), &context(), |_| "run".into());
        let report = find(&result, "app.s.report");
        assert_eq!(report["eventType"], "COMPLETE");
        assert_eq!(report["run"]["runId"], "run");
        // 뷰의 job 이름과 출력 데이터셋 이름이 같은 규칙이다.
        assert_eq!(report["outputs"][0]["name"], "app.s.report");
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
        assert_eq!(
            lineage["dataset"][0]["transformations"],
            json!([{"type": "INDIRECT", "subtype": "JOIN"}])
        );
    }

    /// 루틴의 조건은 어느 문장에 속하는지 몰라 출력이 하나여도 싣지 않고 알린다.
    #[test]
    fn routine_conditions_are_omitted_instead_of_guessing_a_statement() {
        let graph = graph();
        let result = events(&graph, &jobs(&graph), &context(), |_| "run".into());
        assert_eq!(result.indirect_omitted, vec![id("s.load")]);
        let load = find(&result, "app.s.load");
        assert!(load["outputs"][0]["facets"].get("columnLineage").is_none());
    }

    /// `s.load`가 `{schema}.src`를 `{schema}.dst`로 옮기는 그래프를 source `source`로 합친 꼴로
    /// 만든다(모든 정점 id가 `source::` namespace를 가진다).
    fn merged_source(graph: &mut Graph, source: &str, schema: &str) {
        let ns = |local: &str| id(&format!("{source}::{local}"));
        let src = VertexId::object(schema, "src");
        let dst = VertexId::object(schema, "dst");
        let src_val = VertexId::member(schema, "src", "val");
        let dst_val = VertexId::member(schema, "dst", "val");
        let load = VertexId::object(schema, "load");
        for (local, kind, name) in [
            (&src, VertexKind::Table, "src"),
            (&dst, VertexKind::Table, "dst"),
            (&src_val, VertexKind::Column, "val"),
            (&dst_val, VertexKind::Column, "val"),
            (&load, VertexKind::Procedure, "load"),
        ] {
            graph.add_vertex(Vertex {
                id: ns(local.as_str()),
                kind,
                name: name.into(),
                schema: schema.into(),
            });
        }
        let mut edge = |from: &VertexId, to: &VertexId, kind, role: &str| {
            let (from, to) = (ns(from.as_str()), ns(to.as_str()));
            graph.add_edge(Edge {
                from: from.clone(),
                to: to.clone(),
                kind,
                evidence: vec![],
            });
            graph.add_origin(
                (from, to, kind),
                Origin {
                    body_hash: "sha256:t".into(),
                    role: role.into(),
                    location: None,
                },
            );
        };
        let owner = ns(load.as_str());
        let role = |role: &str| Origin::dml_owner_role(&owner, role);
        edge(&load, &dst, EdgeKind::Writes, &role("write"));
        edge(&load, &src, EdgeKind::Reads, &role("projection"));
        edge(&dst_val, &src_val, EdgeKind::DerivesFrom, &role("value"));
    }

    /// 합친 그래프 이름 규칙 — (source, 데이터셋 namespace, 데이터베이스 자리).
    fn merged_context(sources: &[(&str, &str, &str)]) -> EventContext<'static> {
        EventContext {
            namespace: "postgres://shared:5432",
            database: None,
            sources: sources
                .iter()
                .map(|(source, namespace, database)| {
                    (
                        (*source).to_owned(),
                        SourceNaming {
                            namespace: (*namespace).to_owned(),
                            database: (*database).to_owned(),
                        },
                    )
                })
                .collect(),
            ..context()
        }
    }

    /// 이벤트의 (작업, 입력, 출력, 첫 inputField) 이름.
    fn names(result: &Events) -> Vec<[String; 4]> {
        result
            .events
            .iter()
            .map(|event| {
                let text = |value: &Value| value.as_str().unwrap().to_owned();
                let input = &event["outputs"][0]["facets"]["columnLineage"]["fields"]["val"]
                    ["inputFields"][0];
                [
                    text(&event["job"]["name"]),
                    format!(
                        "{} {}",
                        text(&event["inputs"][0]["namespace"]),
                        text(&event["inputs"][0]["name"])
                    ),
                    format!(
                        "{} {}",
                        text(&event["outputs"][0]["namespace"]),
                        text(&event["outputs"][0]["name"])
                    ),
                    format!("{} {}", text(&input["namespace"]), text(&input["name"])),
                ]
            })
            .collect()
    }

    #[test]
    fn merged_sources_name_same_named_tables_as_distinct_datasets() {
        let mut graph = Graph::new();
        merged_source(&mut graph, "app", "public");
        merged_source(&mut graph, "wh", "public");
        // 기본값: 공유 namespace에 source id를 데이터베이스 자리로 둔다. 작업은 그대로다.
        let context = merged_context(&[
            ("app", "postgres://shared:5432", "app"),
            ("wh", "postgres://shared:5432", "wh"),
        ]);
        let jobs = jobs(&graph);
        assert!(identity_collisions(&graph, &jobs, &context).is_empty());
        let result = events(&graph, &jobs, &context, |_| "run".into());
        let shared = "postgres://shared:5432";
        assert_eq!(
            names(&result),
            [
                [
                    "app::public.load".to_owned(),
                    format!("{shared} app.public.src"),
                    format!("{shared} app.public.dst"),
                    format!("{shared} app.public.src"),
                ],
                [
                    "wh::public.load".to_owned(),
                    format!("{shared} wh.public.src"),
                    format!("{shared} wh.public.dst"),
                    format!("{shared} wh.public.src"),
                ],
            ]
        );
        assert!(result
            .events
            .iter()
            .all(|event| event["job"]["namespace"] == shared));
    }

    #[test]
    fn mapped_sources_use_their_namespace_and_database_and_others_keep_the_default() {
        let mut graph = Graph::new();
        merged_source(&mut graph, "app", "public");
        merged_source(&mut graph, "wh", "public");
        let context = merged_context(&[
            ("app", "postgres://a:5432", "appdb"),
            ("wh", "postgres://shared:5432", "wh"),
        ]);
        let result = events(&graph, &jobs(&graph), &context, |_| "run".into());
        assert_eq!(
            names(&result)[0],
            [
                "app::public.load".to_owned(),
                "postgres://a:5432 appdb.public.src".to_owned(),
                "postgres://a:5432 appdb.public.dst".to_owned(),
                "postgres://a:5432 appdb.public.src".to_owned(),
            ]
        );
        assert_eq!(names(&result)[1][2], "postgres://shared:5432 wh.public.dst");
        // 작업 namespace는 매핑과 무관하게 전역 namespace다.
        assert_eq!(
            result.events[0]["job"]["namespace"],
            "postgres://shared:5432"
        );
    }

    #[test]
    fn identity_collisions_list_distinct_datasets_that_would_share_a_name() {
        let mut graph = Graph::new();
        merged_source(&mut graph, "app", "public");
        merged_source(&mut graph, "wh", "public");
        let jobs = jobs(&graph);
        // 두 source를 같은 namespace·데이터베이스로 두면 같은 이름의 테이블이 겹친다.
        let shared = merged_context(&[
            ("app", "postgres://shared:5432", "corpus"),
            ("wh", "postgres://shared:5432", "corpus"),
        ]);
        assert_eq!(
            identity_collisions(&graph, &jobs, &shared),
            [
                "dataset (postgres://shared:5432, corpus.public.dst) would name app::public.dst, wh::public.dst",
                "dataset (postgres://shared:5432, corpus.public.src) would name app::public.src, wh::public.src",
            ]
        );
        // namespace가 다르면 같은 데이터베이스 이름이어도 겹치지 않는다.
        let distinct = merged_context(&[
            ("app", "postgres://app:5432", "corpus"),
            ("wh", "postgres://wh:5432", "corpus"),
        ]);
        assert!(identity_collisions(&graph, &jobs, &distinct).is_empty());
    }

    #[test]
    fn dotted_names_that_blur_the_name_boundary_are_collisions() {
        // source `a.b`의 스키마 `c`와 source `a`의 스키마 `b.c`는 기본 이름이 같다.
        let mut merged = Graph::new();
        merged_source(&mut merged, "a.b", "c");
        merged_source(&mut merged, "a", "b.c");
        let naming = merged_context(&[
            ("a", "postgres://shared:5432", "a"),
            ("a.b", "postgres://shared:5432", "a.b"),
        ]);
        let collisions = identity_collisions(&merged, &jobs(&merged), &naming);
        assert!(
            collisions.iter().any(|c| {
                c
                == "dataset (postgres://shared:5432, a.b.c.dst) would name a.b::c.dst, a::b%2Ec.dst"
            }),
            "{collisions:?}"
        );
        // 보통의 단일 그래프는 정점마다 이름이 달라 아무것도 찾지 않는다.
        let single = graph();
        assert!(identity_collisions(&single, &jobs(&single), &context()).is_empty());
    }

    /// 분석이 부분적인 작업은 job facet으로 상태를 드러낸다.
    #[test]
    fn incomplete_analysis_is_carried_as_a_job_facet() {
        let mut graph = graph();
        graph.set_analysis(
            id("s.report"),
            ObjectAnalysis {
                state: AnalysisState::Partial,
                scope: "column-dependencies-and-lineage".into(),
                body_hash: None,
                diagnostics: vec![],
                source: None,
            },
        );
        let result = events(&graph, &jobs(&graph), &context(), |_| "run".into());
        assert_eq!(result.incomplete, vec![id("s.report")]);
        let facet = &find(&result, "app.s.report")["job"]["facets"]["schemagraphAnalysis"];
        assert_eq!(facet["state"], "partial");
        assert!(find(&result, "app.s.load")["job"].get("facets").is_none());
    }
}
