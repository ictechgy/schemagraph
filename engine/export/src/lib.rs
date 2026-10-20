//! schemagraph-export — 그래프와 질의 결과의 직렬화.
//!
//! 계약: **결정적 JSON.** serde_json의 Map은 기본이 BTreeMap이라 Value로
//! 변환하면 객체 키가 자동 정렬된다 — `to_pretty` 전에 반드시 Value를 거친다.
//! 같은 입력이 같은 바이트여야 리포트 diff와 캐시가 성립한다
//! (AGENTS.md "JSON 출력은 결정적이어야 합니다").

use schemagraph_analysis::{
    budget::BudgetedReport, CyclesReport, DeadReason, DeadReport, EdgeKey, GraphDiff, ImpactReport,
    QueryReport, RulesReport,
};
use schemagraph_core::{Edge, EdgeKind, EvidenceLayer, Graph, Level, Vertex, VertexKind};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

pub mod diagnostics;
pub mod explain;
pub mod html;
pub mod language_traversal;
pub mod lint;
pub mod mermaid;
pub mod openlineage;
pub mod review;
pub mod sarif;
pub mod schema_metadata;
pub mod search;
pub mod stream;
pub mod unused;

/// graph.json의 와이어 버전. 형식이 깨지는 변경은 올린다.
pub const GRAPH_VERSION: u32 = 2;

/// `query` JSON 문서의 `format` 값 — 저장된 파일만 받은 소비자도 어느 명령의
/// 결과인지 구분할 수 있게 한다. 계열의 `kartograph-impact`와 같은 작명이다.
pub const QUERY_FORMAT: &str = "schemagraph-query";

/// `query` JSON 문서의 와이어 버전. 키 추가는 그대로 두고, 기존 키의 의미나
/// 형태가 바뀔 때만 올린다.
pub const QUERY_VERSION: u32 = 1;

/// `impact` JSON 문서의 `format` 값.
pub const IMPACT_FORMAT: &str = "schemagraph-impact";

/// `impact` JSON 문서의 와이어 버전. `QUERY_VERSION`과 같은 규칙으로 올린다.
pub const IMPACT_VERSION: u32 = 1;

// ---------- kind 문자열 (와이어 계약) ----------

fn vertex_kind_str(kind: VertexKind) -> &'static str {
    match kind {
        VertexKind::Schema => "schema",
        VertexKind::Table => "table",
        VertexKind::View => "view",
        VertexKind::MaterializedView => "materialized-view",
        VertexKind::Sequence => "sequence",
        VertexKind::Type => "type",
        VertexKind::Synonym => "synonym",
        VertexKind::Column => "column",
        VertexKind::Index => "index",
        VertexKind::Constraint => "constraint",
        VertexKind::Trigger => "trigger",
        VertexKind::Function => "function",
        VertexKind::Procedure => "procedure",
        VertexKind::Package => "package",
        VertexKind::Query => "query",
    }
}

/// 정점 종류의 와이어 라벨 전체 — MCP 입력 스키마가 허용 값을 알리는 데 쓴다.
pub const VERTEX_KIND_LABELS: &[&str] = &[
    "schema",
    "table",
    "view",
    "materialized-view",
    "sequence",
    "type",
    "synonym",
    "column",
    "index",
    "constraint",
    "trigger",
    "function",
    "procedure",
    "package",
    "query",
];

/// 와이어 종류 라벨을 해석한다 — CLI·MCP의 종류 필터가 공유한다.
pub fn vertex_kind_parse(s: &str) -> Option<VertexKind> {
    Some(match s {
        "schema" => VertexKind::Schema,
        "table" => VertexKind::Table,
        "view" => VertexKind::View,
        "materialized-view" => VertexKind::MaterializedView,
        "sequence" => VertexKind::Sequence,
        "type" => VertexKind::Type,
        "synonym" => VertexKind::Synonym,
        "column" => VertexKind::Column,
        "index" => VertexKind::Index,
        "constraint" => VertexKind::Constraint,
        "trigger" => VertexKind::Trigger,
        "function" => VertexKind::Function,
        "procedure" => VertexKind::Procedure,
        "package" => VertexKind::Package,
        "query" => VertexKind::Query,
        _ => return None,
    })
}

fn level_str(level: Level) -> &'static str {
    match level {
        Level::Schema => "schema",
        Level::Object => "object",
        Level::Member => "member",
    }
}

/// 간선 종류의 JSON 라벨 — 그래프 문서와 규칙 파일이 같은 문자열을 쓴다.
pub fn edge_kind_str(kind: EdgeKind) -> &'static str {
    match kind {
        EdgeKind::References => "references",
        EdgeKind::Reads => "reads",
        EdgeKind::Writes => "writes",
        EdgeKind::Calls => "calls",
        EdgeKind::Fires => "fires",
        EdgeKind::UsesSequence => "uses-sequence",
        EdgeKind::UsesType => "uses-type",
        EdgeKind::Contains => "contains",
        EdgeKind::Inferred => "inferred",
        EdgeKind::DerivesFrom => "derives-from",
        EdgeKind::DependsOn => "depends-on",
    }
}

/// JSON 라벨 → 간선 종류. 규칙 파일의 kinds 필드도 이 변환을 쓴다.
pub fn edge_kind_parse(s: &str) -> Option<EdgeKind> {
    Some(match s {
        "references" => EdgeKind::References,
        "reads" => EdgeKind::Reads,
        "writes" => EdgeKind::Writes,
        "calls" => EdgeKind::Calls,
        "fires" => EdgeKind::Fires,
        "uses-sequence" => EdgeKind::UsesSequence,
        "uses-type" => EdgeKind::UsesType,
        "contains" => EdgeKind::Contains,
        "inferred" => EdgeKind::Inferred,
        "derives-from" => EdgeKind::DerivesFrom,
        "depends-on" => EdgeKind::DependsOn,
        _ => return None,
    })
}

fn layer_str(layer: EvidenceLayer) -> &'static str {
    match layer {
        EvidenceLayer::Catalog => "catalog",
        EvidenceLayer::BodyParse => "body-parse",
        EvidenceLayer::Stats => "stats",
        EvidenceLayer::Inferred => "inferred",
    }
}

fn layer_parse(s: &str) -> Option<EvidenceLayer> {
    Some(match s {
        "catalog" => EvidenceLayer::Catalog,
        "body-parse" => EvidenceLayer::BodyParse,
        "stats" => EvidenceLayer::Stats,
        "inferred" => EvidenceLayer::Inferred,
        _ => return None,
    })
}

// ---------- graph.json DTO ----------

#[derive(Debug, Serialize, Deserialize)]
pub struct GraphDoc {
    pub version: u32,
    pub vertices: Vec<VertexDoc>,
    pub edges: Vec<EdgeDoc>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub limitations: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub analysis: Vec<diagnostics::AnalysisDoc>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub origins: Vec<diagnostics::OriginDoc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schema_metadata: Option<schema_metadata::SchemaMetadataDoc>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct VertexDoc {
    pub id: String,
    pub kind: String,
    pub level: String,
    pub name: String,
    pub schema: String,
    /// 사용 통계 — 없는 것(미수집)은 키가 빠진다. 0 관측은 usage가 있는 채로
    /// reads=0/writes=0이다 — 둘을 구분하는 게 계약이다.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<UsageDoc>,
}

/// graph.json 안의 사용 통계. `since` 없이 수치만 있으면 소비자가
/// "0 = 미사용"으로 오독할 수 있어 since를 같은 필드로 싣는다.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UsageDoc {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub since: Option<String>,
    pub reads: u64,
    pub writes: u64,
    /// 테이블 스캔 횟수 — 수집한 경우에만 싣는다.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scans: Option<u64>,
    /// routine 누적 실행 시간 ms — routine 정점에만 온다.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_ms: Option<f64>,
    /// routine 자기 실행 시간 ms(중첩 호출 제외).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub self_ms: Option<f64>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct EdgeDoc {
    pub from: String,
    pub to: String,
    pub kind: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub evidence: Vec<EvidenceDoc>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct EvidenceDoc {
    pub layer: String,
    pub detail: String,
}

/// Graph → GraphDoc. 정점·간선 모두 id/(from,to,kind) 순 정렬이다 —
/// graph.json이 항상 같은 바이트가 되게 하는 절반이다(나머지 절반은
/// BTreeMap 키 정렬).
pub fn graph_to_doc(g: &Graph) -> GraphDoc {
    let mut vertices: Vec<VertexDoc> = g
        .vertices()
        .map(|v| VertexDoc {
            id: v.id.as_str().to_owned(),
            kind: vertex_kind_str(v.kind).to_owned(),
            level: level_str(v.kind.level()).to_owned(),
            name: v.name.clone(),
            schema: v.schema.clone(),
            usage: g.usage(&v.id).map(|u| UsageDoc {
                since: u.since.clone(),
                reads: u.reads,
                writes: u.writes,
                scans: u.scans,
                total_ms: u.total_ms,
                self_ms: u.self_ms,
            }),
        })
        .collect();
    vertices.sort_by(|a, b| a.id.cmp(&b.id));

    let edges: Vec<EdgeDoc> = g
        .edges()
        .iter()
        .map(|e| EdgeDoc {
            from: e.from.as_str().to_owned(),
            to: e.to.as_str().to_owned(),
            kind: edge_kind_str(e.kind).to_owned(),
            evidence: e
                .evidence
                .iter()
                .map(|ev| EvidenceDoc {
                    layer: layer_str(ev.layer).to_owned(),
                    detail: ev.detail.clone(),
                })
                .collect(),
        })
        .collect();

    GraphDoc {
        version: GRAPH_VERSION,
        vertices,
        edges,
        limitations: g.limitations().to_vec(),
        analysis: diagnostics::analysis_docs(g),
        origins: diagnostics::origin_docs(g),
        schema_metadata: g.schema_metadata().map(schema_metadata::to_doc),
    }
}

/// GraphDoc → Graph. 알 수 없는 kind 문자열은 정점/간선을 버리고
/// limitation으로 남긴다 — 미래 버전의 파일을 읽을 때 조용히 반쪽짜리
/// 그래프가 되는 것보다 낫다.
pub fn graph_from_doc(doc: &GraphDoc) -> Graph {
    let mut g = Graph::new();
    for l in &doc.limitations {
        g.add_limitation(l.clone());
    }
    let mut dropped_v = 0usize;
    let mut dropped_e = 0usize;
    let mut dangling_e = 0usize;
    for v in &doc.vertices {
        match vertex_kind_parse(&v.kind) {
            Some(kind) => {
                let id = schemagraph_core::VertexId::from_raw(&v.id);
                g.add_vertex(Vertex {
                    id: id.clone(),
                    kind,
                    name: v.name.clone(),
                    schema: v.schema.clone(),
                });
                if let Some(u) = &v.usage {
                    g.set_usage(
                        id,
                        schemagraph_core::Usage {
                            since: u.since.clone(),
                            reads: u.reads,
                            writes: u.writes,
                            scans: u.scans,
                            total_ms: u.total_ms,
                            self_ms: u.self_ms,
                        },
                    );
                }
            }
            None => dropped_v += 1,
        }
    }
    // 다른 도구가 만든 문서는 같은 (from, to, kind)를 반복할 수 있어, 간선을 모아
    // add_edges로 근거를 키마다 한 번에 합친다 — 간선마다 add_edge하면 중복 간선이
    // 근거 병합을 되풀이해 적재가 이차 시간이 된다.
    let mut edges = Vec::with_capacity(doc.edges.len());
    for e in &doc.edges {
        let Some(kind) = edge_kind_parse(&e.kind) else {
            dropped_e += 1;
            continue;
        };
        // 정점의 공유 id를 써서, 모아 두는 동안 간선마다 id 문자열을 따로 들지 않는다.
        let shared_id = |raw: &str| {
            g.vertex(&schemagraph_core::VertexId::from_raw(raw))
                .map(|vertex| vertex.id.clone())
        };
        let (Some(from), Some(to)) = (shared_id(&e.from), shared_id(&e.to)) else {
            dangling_e += 1;
            continue;
        };
        edges.push(Edge {
            from,
            to,
            kind,
            evidence: e
                .evidence
                .iter()
                .filter_map(|ev| {
                    layer_parse(&ev.layer).map(|layer| schemagraph_core::Evidence {
                        layer,
                        detail: ev.detail.clone(),
                    })
                })
                .collect(),
        });
    }
    g.add_edges(edges);
    if dropped_v > 0 {
        g.add_limitation(format!(
            "graph.json의 정점 {dropped_v}개가 알 수 없는 kind라 버림 (버전 불일치?)"
        ));
    }
    if dropped_e > 0 {
        g.add_limitation(format!(
            "graph.json의 간선 {dropped_e}개가 알 수 없는 kind라 버림 (버전 불일치?)"
        ));
    }
    if dangling_e > 0 {
        g.add_limitation(format!(
            "graph.json의 간선 {dangling_e}개가 존재하지 않는 정점을 가리켜 버림"
        ));
    }
    diagnostics::restore(&mut g, &doc.analysis, &doc.origins);
    if let Some(metadata) = &doc.schema_metadata {
        match schema_metadata::from_doc(metadata, &g) {
            Ok(metadata) => g.set_schema_metadata(metadata),
            Err(error) => g.add_limitation(format!("invalid schema metadata: {error}")),
        }
    }
    g
}

/// 결정적 pretty JSON 문자열. Value를 거쳐 키를 정렬한다.
pub fn to_pretty_json<T: Serialize>(value: &T) -> serde_json::Result<String> {
    let value = serde_json::to_value(value)?;
    serde_json::to_string_pretty(&value)
}

// ---------- query/cycles 보고의 와이어 표현 ----------

/// 이웃 목록의 공통 직렬화 — query의 dependents/dependencies와 impact의
/// impacted가 같은 형태다. `via`는 subject에서 온 최단 경로의 직전 정점이라
/// 소비자가 이웃마다 경로를 따로 묻지 않고도 전파 경로를 복원할 수 있다.
fn neighbors_value(ns: &[schemagraph_analysis::Neighbor]) -> serde_json::Value {
    serde_json::Value::Array(
        ns.iter()
            .map(|n| {
                serde_json::json!({
                    "distance": n.distance,
                    "edges": n.edges.iter().map(|k| edge_kind_str(*k)).collect::<Vec<_>>(),
                    "id": n.vertex.id.as_str(),
                    "kind": vertex_kind_str(n.vertex.kind),
                    "via": n.via.as_str(),
                })
            })
            .collect(),
    )
}

fn subject_value(v: &schemagraph_core::Vertex) -> serde_json::Value {
    serde_json::json!({
        "id": v.id.as_str(),
        "kind": vertex_kind_str(v.kind),
        "level": level_str(v.kind.level()),
        "name": v.name,
        "schema": v.schema,
    })
}

/// QueryReport → JSON Value. 키 순서는 BTreeMap 정렬에 맡긴다.
pub fn query_to_value(report: &QueryReport) -> serde_json::Value {
    let mut value = serde_json::json!({
        "dependencies": neighbors_value(&report.dependencies),
        "dependents": neighbors_value(&report.dependents),
        "depth": report.depth,
        "format": QUERY_FORMAT,
        "limitations": report.limitations,
        "subject": subject_value(&report.subject),
        "truncated": report.truncated,
        "version": QUERY_VERSION,
    });
    // 자기 참조 간선이 있을 때만 selfEdges를 싣는다 — 빈 선택 필드는 키를
    // 빼는 것이 계약이다.
    if !report.self_edges.is_empty() {
        value["selfEdges"] = serde_json::json!(report
            .self_edges
            .iter()
            .map(|k| edge_kind_str(*k))
            .collect::<Vec<_>>());
    }
    value
}

/// 스냅샷 요약 — 종류별 정점·간선 수, 스키마, 수집 한계를 싣는다.
///
/// 에이전트가 첫 요청에서 그래프의 크기와 사각지대를 알고 나서 탐색 범위를
/// 정하도록 한다. 키 순서가 정렬된 맵이라 같은 그래프면 같은 문서다.
pub fn graph_summary_value(g: &Graph) -> serde_json::Value {
    let mut vertices = std::collections::BTreeMap::<&str, usize>::new();
    let mut schemas = std::collections::BTreeSet::<&str>::new();
    for vertex in g.vertices() {
        *vertices.entry(vertex_kind_str(vertex.kind)).or_default() += 1;
        schemas.insert(vertex.schema.as_str());
    }
    let mut edges = std::collections::BTreeMap::<&str, usize>::new();
    for edge in g.edges() {
        *edges.entry(edge_kind_str(edge.kind)).or_default() += 1;
    }
    serde_json::json!({
        "edges": edges,
        "limitations": g.limitations(),
        "schemas": schemas,
        "vertices": vertices,
    })
}

/// 대상을 못 찾은 경우의 notFound 응답 — limitations도 싣는다.
pub fn not_found_value(
    name: &str,
    candidates: &[schemagraph_core::VertexId],
    limitations: &[String],
) -> serde_json::Value {
    serde_json::json!({
        "candidates": candidates.iter().map(|c| c.as_str()).collect::<Vec<_>>(),
        "found": false,
        "limitations": limitations,
        "name": name,
    })
}

/// 후보를 기존 순서의 앞 `max_candidates`개까지만 싣는 notFound 응답이다.
///
/// 말단 이름이 같은 정점이 수천 개인 그래프(예: 모든 테이블의 `id` 컬럼)에서 후보를 전부
/// 실으면 응답 하나가 그래프 크기만큼 커진다. 잘렸을 때만 전체 후보 수 `candidatesTotal`과
/// `candidatesTruncated: true`를 더한다 — 값이 없는 선택 필드는 키를 빼는 계약이라, 잘리지
/// 않은 응답은 [`not_found_value`]와 같다.
pub fn not_found_value_with_max(
    name: &str,
    candidates: &[schemagraph_core::VertexId],
    limitations: &[String],
    max_candidates: usize,
) -> serde_json::Value {
    let shown = &candidates[..candidates.len().min(max_candidates)];
    let mut value = not_found_value(name, shown, limitations);
    if shown.len() < candidates.len() {
        value["candidatesTotal"] = serde_json::json!(candidates.len());
        value["candidatesTruncated"] = serde_json::json!(true);
    }
    value
}

/// ImpactReport → JSON Value.
pub fn impact_to_value(report: &ImpactReport) -> serde_json::Value {
    serde_json::json!({
        "format": IMPACT_FORMAT,
        "impacted": neighbors_value(&report.impacted),
        "limitations": report.limitations,
        "subject": subject_value(&report.subject),
        "truncated": report.truncated,
        "version": IMPACT_VERSION,
    })
}

/// 예산 적용 질의의 방향별 탐색 수치를 기록한다. 기존 `max` 결과 제한과
/// 탐색 예산 소진을 소비자가 구분할 수 있도록 합계와 상세를 함께 싣는다.
pub fn budgeted_query_to_value(
    subject: &schemagraph_core::Vertex,
    dependents: &BudgetedReport,
    dependencies: &BudgetedReport,
    depth: u32,
    self_edges: &[EdgeKind],
    limitations: &[String],
) -> serde_json::Value {
    let mut reasons: BTreeSet<&str> = BTreeSet::new();
    reasons.extend(dependents.truncation_reasons.iter().map(String::as_str));
    reasons.extend(dependencies.truncation_reasons.iter().map(String::as_str));
    let mut value = serde_json::json!({
        "dependencies": neighbors_value(&dependencies.neighbors),
        "dependents": neighbors_value(&dependents.neighbors),
        "depth": depth,
        "format": QUERY_FORMAT,
        "version": QUERY_VERSION,
        "limitations": limitations,
        "subject": subject_value(subject),
        "truncated": !reasons.is_empty(),
        "complete": dependents.complete && dependencies.complete,
        "visited": dependents.visited + dependencies.visited,
        "examinedEdges": dependents.examined_edges + dependencies.examined_edges,
        "truncationReasons": reasons.into_iter().collect::<Vec<_>>(),
        "traversals": {
            "dependents": budgeted_traversal_value(dependents),
            "dependencies": budgeted_traversal_value(dependencies),
        },
    });
    if !self_edges.is_empty() {
        value["selfEdges"] = serde_json::json!(self_edges
            .iter()
            .map(|kind| edge_kind_str(*kind))
            .collect::<Vec<_>>());
    }
    value
}

/// 예산 적용 impact 보고를 기존 impact JSON 계약과 탐색 수치로 표현한다.
pub fn budgeted_impact_to_value(
    subject: &schemagraph_core::Vertex,
    report: &BudgetedReport,
    limitations: &[String],
) -> serde_json::Value {
    serde_json::json!({
        "format": IMPACT_FORMAT,
        "version": IMPACT_VERSION,
        "impacted": neighbors_value(&report.neighbors),
        "limitations": limitations,
        "subject": subject_value(subject),
        "truncated": !report.truncation_reasons.is_empty(),
        "complete": report.complete,
        "visited": report.visited,
        "examinedEdges": report.examined_edges,
        "truncationReasons": report.truncation_reasons,
    })
}

fn budgeted_traversal_value(report: &BudgetedReport) -> serde_json::Value {
    serde_json::json!({
        "visited": report.visited,
        "examinedEdges": report.examined_edges,
        "truncationReasons": report.truncation_reasons,
        "complete": report.complete,
        "rootFound": report.root_found,
    })
}

/// DeadReport → JSON Value. usage는 미수집(None)이면 키가 빠진다 —
/// 0 관측은 usage가 있는 채로 reads=0/writes=0이다.
pub fn dead_to_value(report: &DeadReport) -> serde_json::Value {
    let mut value = serde_json::json!({
        "candidates": report.candidates.iter().map(|c| {
            let mut v = serde_json::json!({
                "id": c.vertex.id.as_str(),
                "kind": vertex_kind_str(c.vertex.kind),
                "reason": match c.reason {
                    DeadReason::NoDependents => "noDependents",
                    DeadReason::AllDependentsDead => "allDependentsDead",
                },
            });
            if let Some(u) = &c.usage {
                // UsageDoc으로 직렬화해야 since 생략 계약(없으면 키 없음)이 유지된다.
                v["usage"] = serde_json::to_value(UsageDoc {
                    since: u.since.clone(),
                    reads: u.reads,
                    writes: u.writes,
                    scans: u.scans,
                    total_ms: u.total_ms,
                    self_ms: u.self_ms,
                })
                .unwrap_or(serde_json::Value::Null);
            }
            if let Some(reason) = &c.suppression {
                v["suppressed"] = serde_json::json!(true);
                v["suppressionReason"] = serde_json::json!(reason);
            }
            v
        }).collect::<Vec<_>>(),
        "limitations": report.limitations,
        "truncated": report.truncated,
        "totalCandidates": report.total_candidates,
        "unsuppressedCount": report.unsuppressed_count,
    });
    if !report.retained.is_empty() {
        value["retained"] = serde_json::json!(report
            .retained
            .iter()
            .map(|(id, reason)| serde_json::json!({"id":id.as_str(),"reason":reason}))
            .collect::<Vec<_>>());
    }
    value
}

/// 그래프에 싣린 사용 통계를 그대로 보고한다 — 목록엔 관측된 정점만 나오고,
/// 미수집 정점은 totals로 소비자가 짐작한다. 통계는 since 이후만 유효하므로
/// "미사용" 판정이 아니라 증거 나열이다.
pub fn stats_to_value(g: &Graph) -> serde_json::Value {
    stats_to_value_with_max(g, usize::MAX)
}

/// `stats` 목록을 기존 순서(정점 id 순)의 앞 `max`개로 자른 보고다.
///
/// usage를 수집한 대형 그래프에서 관측 정점 전부를 한 응답에 실으면 응답이 그래프
/// 크기만큼 커진다. 자를 항목은 아예 만들지 않는다. 잘렸을 때만 `truncated: true`를
/// 더한다 — `totals.observed`가 이미 전체 관측 수라 총수 키를 따로 두지 않는다. 잘리지
/// 않은 보고는 [`stats_to_value`]와 같다.
pub fn stats_to_value_with_max(g: &Graph, max: usize) -> serde_json::Value {
    let stats: Vec<serde_json::Value> = g
        .usages()
        .take(max)
        .map(|(id, u)| usage_entry_value(g, id, u))
        .collect();
    let observed = g.usages().count();
    let truncated = stats.len() < observed;
    let mut value = serde_json::json!({
        "limitations": g.limitations(),
        "stats": stats,
        "totals": {
            "observed": observed,
            "vertices": g.vertices().count(),
        },
    });
    if truncated {
        value["truncated"] = serde_json::json!(true);
    }
    value
}

/// `stats` 항목 하나 — 수집하지 않은 값(since·scans·실행 시간)은 키를 뺀다.
fn usage_entry_value(
    g: &Graph,
    id: &schemagraph_core::VertexId,
    u: &schemagraph_core::Usage,
) -> serde_json::Value {
    let mut v = serde_json::json!({
        "id": id.as_str(),
        "reads": u.reads,
        "writes": u.writes,
    });
    if let Some(vertex) = g.vertex(id) {
        v["kind"] = serde_json::json!(vertex_kind_str(vertex.kind));
    }
    if let Some(since) = &u.since {
        v["since"] = serde_json::json!(since);
    }
    if let Some(scans) = u.scans {
        v["scans"] = serde_json::json!(scans);
    }
    if let Some(ms) = u.total_ms {
        v["total_ms"] = serde_json::json!(ms);
    }
    if let Some(ms) = u.self_ms {
        v["self_ms"] = serde_json::json!(ms);
    }
    v
}

/// CyclesReport → JSON Value.
pub fn cycles_to_value(report: &CyclesReport) -> serde_json::Value {
    cycles_to_value_with_max(report, usize::MAX)
}

/// `cycles` 목록을 기존 순서(구성원 id 순)의 앞 `max`개로 자른 보고다.
///
/// 잘렸을 때만 `truncated: true`와 전체 순환 수 `total`을 더한다 — 잘리지 않은 보고는
/// [`cycles_to_value`]와 같다.
pub fn cycles_to_value_with_max(report: &CyclesReport, max: usize) -> serde_json::Value {
    let mut value = serde_json::json!({
        "cycles": report.cycles.iter().take(max).map(|c| {
            serde_json::json!({
                "members": c.members.iter().map(|m| m.as_str()).collect::<Vec<_>>(),
                "selfLoop": c.self_loop,
            })
        }).collect::<Vec<_>>(),
        "level": level_str(report.level),
        "limitations": report.limitations,
    });
    if report.cycles.len() > max {
        value["truncated"] = serde_json::json!(true);
        value["total"] = serde_json::json!(report.cycles.len());
    }
    value
}

/// GraphDiff → JSON Value. usage는 델타가 아니라서 실리지 않는다 —
/// summary 숫자와 목록이 항상 일치하는 게 계약이다.
pub fn graph_diff_to_value(report: &GraphDiff) -> serde_json::Value {
    fn vertex_json(v: &Vertex) -> serde_json::Value {
        serde_json::json!({
            "id": v.id.as_str(),
            "kind": vertex_kind_str(v.kind),
        })
    }
    fn edge_json(e: &EdgeKey) -> serde_json::Value {
        serde_json::json!({
            "kind": edge_kind_str(e.kind),
            "from": e.from.as_str(),
            "to": e.to.as_str(),
        })
    }
    serde_json::json!({
        "kind": "graph",
        "summary": {
            "added": report.vertices_added.len() + report.edges_added.len(),
            "removed": report.vertices_removed.len() + report.edges_removed.len(),
            "changed": report.vertices_changed.len(),
        },
        "vertices": {
            "added": report.vertices_added.iter().map(vertex_json).collect::<Vec<_>>(),
            "removed": report.vertices_removed.iter().map(vertex_json).collect::<Vec<_>>(),
            "changed": report.vertices_changed.iter().map(|c| {
                serde_json::json!({
                    "id": c.id.as_str(),
                    "kind": {
                        "old": vertex_kind_str(c.old_kind),
                        "new": vertex_kind_str(c.new_kind),
                    },
                })
            }).collect::<Vec<_>>(),
        },
        "edges": {
            "added": report.edges_added.iter().map(edge_json).collect::<Vec<_>>(),
            "removed": report.edges_removed.iter().map(edge_json).collect::<Vec<_>>(),
        },
        "limitations": report.limitations,
    })
}

/// RulesReport → JSON Value. `checked`가 0이면 "검사한 규칙이 없다"이지
/// 통과가 아니라는 것을 숫자로 보여준다.
pub fn rules_to_value(report: &RulesReport) -> serde_json::Value {
    serde_json::json!({
        "checked": report.checked,
        "limitations": report.limitations,
        "violations": report.violations.iter().map(|v| {
            serde_json::json!({
                "edge": {
                    "from": v.from.as_str(),
                    "kind": edge_kind_str(v.kind),
                    "to": v.to.as_str(),
                },
                "rule": v.rule,
            })
        }).collect::<Vec<_>>(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use schemagraph_core::{Evidence, VertexId};

    /// 라벨 목록이 해석기·출력기와 어긋나면 MCP가 받을 수 없는 값을 광고하게 된다.
    #[test]
    fn vertex_kind_labels_round_trip() {
        for label in VERTEX_KIND_LABELS {
            let kind = vertex_kind_parse(label).expect("advertised label must parse");
            assert_eq!(vertex_kind_str(kind), *label);
        }
    }

    fn sample() -> Graph {
        let mut g = Graph::new();
        g.add_vertex(Vertex {
            id: VertexId::schema("main"),
            kind: VertexKind::Schema,
            name: "main".into(),
            schema: "main".into(),
        });
        g.add_vertex(Vertex {
            id: VertexId::object("main", "b"),
            kind: VertexKind::Table,
            name: "b".into(),
            schema: "main".into(),
        });
        g.add_vertex(Vertex {
            id: VertexId::object("main", "a"),
            kind: VertexKind::Table,
            name: "a".into(),
            schema: "main".into(),
        });
        g.add_edge(Edge {
            from: VertexId::object("main", "a"),
            to: VertexId::object("main", "b"),
            kind: EdgeKind::References,
            evidence: vec![Evidence {
                layer: EvidenceLayer::Catalog,
                detail: "fk".into(),
            }],
        });
        g
    }

    #[test]
    fn 출력이_결정적이고_정렬된다() {
        let doc = graph_to_doc(&sample());
        let a = to_pretty_json(&doc).unwrap();
        let b = to_pretty_json(&doc).unwrap();
        assert_eq!(a, b);
        let v: serde_json::Value = serde_json::from_str(&a).unwrap();
        // 정점 id가 정렬돼 있다.
        let ids: Vec<&str> = v["vertices"]
            .as_array()
            .unwrap()
            .iter()
            .map(|x| x["id"].as_str().unwrap())
            .collect();
        let mut sorted = ids.clone();
        sorted.sort_unstable();
        assert_eq!(ids, sorted);
        // 최상위 객체 키도 정렬돼 있다(결정성의 절반은 키 정렬).
        let keys: Vec<&String> = v.as_object().unwrap().keys().collect();
        let mut sorted_keys = keys.clone();
        sorted_keys.sort();
        assert_eq!(keys, sorted_keys);
    }

    /// sample(a -> b)에 c -> a를 더해 b <- a <- c 의존 체인을 만든다.
    fn chain_sample() -> Graph {
        let mut g = sample();
        g.add_vertex(Vertex {
            id: VertexId::object("main", "c"),
            kind: VertexKind::View,
            name: "c".into(),
            schema: "main".into(),
        });
        g.add_edge(Edge {
            from: VertexId::object("main", "c"),
            to: VertexId::object("main", "a"),
            kind: EdgeKind::Reads,
            evidence: vec![],
        });
        g
    }

    /// 이웃 JSON 배열을 (id, via) 쌍으로 줄인다.
    fn id_via_pairs(neighbors: &serde_json::Value) -> Vec<(String, String)> {
        neighbors
            .as_array()
            .expect("neighbors are an array")
            .iter()
            .map(|n| {
                (
                    n["id"].as_str().expect("id").to_owned(),
                    n["via"].as_str().expect("via").to_owned(),
                )
            })
            .collect()
    }

    fn pairs(expected: &[(&str, &str)]) -> Vec<(String, String)> {
        expected
            .iter()
            .map(|(id, via)| ((*id).to_owned(), (*via).to_owned()))
            .collect()
    }

    #[test]
    fn impact_이웃은_subject까지의_최단_경로_부모를_via로_싣는다() {
        use schemagraph_analysis::budget::{walk, Budget};
        let g = chain_sample();
        let b = VertexId::object("main", "b");
        let report = walk(
            &g,
            &b,
            u32::MAX,
            usize::MAX,
            true,
            Budget::unlimited(),
            None,
        );
        let value = budgeted_impact_to_value(g.vertex(&b).unwrap(), &report, g.limitations());
        let expected = pairs(&[("main.a", "main.b"), ("main.c", "main.a")]);
        assert_eq!(id_via_pairs(&value["impacted"]), expected);
        // 예산 없는 legacy 직렬화도 같은 이웃 계약을 쓴다.
        let legacy = impact_to_value(&schemagraph_analysis::impact(&g, &b, usize::MAX));
        assert_eq!(id_via_pairs(&legacy["impacted"]), expected);
    }

    #[test]
    fn query_이웃은_방향마다_via를_싣는다() {
        use schemagraph_analysis::budget::{walk, Budget};
        let g = chain_sample();
        let a = VertexId::object("main", "a");
        let dependents = walk(&g, &a, 2, usize::MAX, true, Budget::unlimited(), None);
        let dependencies = walk(&g, &a, 2, usize::MAX, false, Budget::unlimited(), None);
        let value = budgeted_query_to_value(
            g.vertex(&a).unwrap(),
            &dependents,
            &dependencies,
            2,
            &[],
            g.limitations(),
        );
        assert_eq!(
            id_via_pairs(&value["dependents"]),
            pairs(&[("main.c", "main.a")])
        );
        assert_eq!(
            id_via_pairs(&value["dependencies"]),
            pairs(&[("main.b", "main.a")])
        );
        let legacy = query_to_value(&schemagraph_analysis::query(&g, &a, 2, usize::MAX));
        assert_eq!(
            id_via_pairs(&legacy["dependents"]),
            id_via_pairs(&value["dependents"])
        );
    }

    /// JSON 객체의 최상위 키 목록(정렬됨).
    fn keys(value: &serde_json::Value) -> Vec<&str> {
        value
            .as_object()
            .expect("report is an object")
            .keys()
            .map(String::as_str)
            .collect()
    }

    /// 머리 키는 추가일 뿐이다 — 기존 키 집합은 그대로 두고 format·version만
    /// 더해졌는지 키 목록 전체로 고정한다. 키가 빠지거나 바뀌면 소비자가 깨진다.
    #[test]
    fn query와_impact_문서는_기존_키에_format과_version만_더한다() {
        use schemagraph_analysis::budget::{walk, Budget};
        let g = chain_sample();
        let a = VertexId::object("main", "a");
        let subject = g.vertex(&a).unwrap();
        let dependents = walk(&g, &a, 1, usize::MAX, true, Budget::unlimited(), None);
        let dependencies = walk(&g, &a, 1, usize::MAX, false, Budget::unlimited(), None);
        let query =
            budgeted_query_to_value(subject, &dependents, &dependencies, 1, &[], g.limitations());
        assert_eq!(
            keys(&query),
            [
                "complete",
                "dependencies",
                "dependents",
                "depth",
                "examinedEdges",
                "format",
                "limitations",
                "subject",
                "traversals",
                "truncated",
                "truncationReasons",
                "version",
                "visited",
            ]
        );
        assert_eq!(
            (&query["format"], &query["version"]),
            (
                &serde_json::json!("schemagraph-query"),
                &serde_json::json!(1)
            )
        );
        assert_eq!(
            keys(&query["dependents"][0]),
            ["distance", "edges", "id", "kind", "via"]
        );

        let impact = budgeted_impact_to_value(subject, &dependents, g.limitations());
        assert_eq!(
            keys(&impact),
            [
                "complete",
                "examinedEdges",
                "format",
                "impacted",
                "limitations",
                "subject",
                "truncated",
                "truncationReasons",
                "version",
                "visited",
            ]
        );
        assert_eq!(
            (&impact["format"], &impact["version"]),
            (
                &serde_json::json!("schemagraph-impact"),
                &serde_json::json!(1)
            )
        );

        // 예산 없는 legacy 직렬화도 같은 머리를 싣는다.
        let legacy_query = query_to_value(&schemagraph_analysis::query(&g, &a, 1, usize::MAX));
        assert_eq!(
            keys(&legacy_query),
            [
                "dependencies",
                "dependents",
                "depth",
                "format",
                "limitations",
                "subject",
                "truncated",
                "version",
            ]
        );
        assert_eq!(legacy_query["format"], QUERY_FORMAT);
        let legacy_impact = impact_to_value(&schemagraph_analysis::impact(&g, &a, usize::MAX));
        assert_eq!(
            keys(&legacy_impact),
            [
                "format",
                "impacted",
                "limitations",
                "subject",
                "truncated",
                "version"
            ]
        );
        assert_eq!(legacy_impact["version"], IMPACT_VERSION);
    }

    #[test]
    fn 왕복이_그래프를_보존한다() {
        let g = sample();
        let doc = graph_to_doc(&g);
        let json = to_pretty_json(&doc).unwrap();
        let doc2: GraphDoc = serde_json::from_str(&json).unwrap();
        let g2 = graph_from_doc(&doc2);
        assert_eq!(g.vertices().count(), g2.vertices().count());
        assert_eq!(g.edges().len(), g2.edges().len());
    }

    #[test]
    fn 존재하지_않는_정점을_가리키는_간선은_복원하지_않는다() {
        let mut doc = graph_to_doc(&sample());
        doc.edges.push(EdgeDoc {
            from: "main.a".into(),
            to: "main.missing".into(),
            kind: "references".into(),
            evidence: vec![],
        });
        let graph = graph_from_doc(&doc);
        assert!(!graph
            .edges()
            .iter()
            .any(|edge| edge.to.as_str() == "main.missing"));
        assert!(graph
            .limitations()
            .iter()
            .any(|note| note.contains("존재하지 않는 정점")));
    }

    /// 근거를 (layer, detail) 쌍으로 받는 graph.json 간선.
    fn edge_doc(from: &str, to: &str, kind: &str, evidence: &[(&str, &str)]) -> EdgeDoc {
        EdgeDoc {
            from: from.into(),
            to: to.into(),
            kind: kind.into(),
            evidence: evidence
                .iter()
                .map(|(layer, detail)| EvidenceDoc {
                    layer: (*layer).into(),
                    detail: (*detail).into(),
                })
                .collect(),
        }
    }

    /// 인접 리스트 순서와 근거까지 펼친 그래프 상태 — Graph에는 PartialEq가 없다.
    fn adjacency(graph: &Graph) -> Vec<String> {
        graph
            .vertices()
            .flat_map(|vertex| {
                let outgoing = graph.outgoing(&vertex.id).iter();
                let incoming = graph.incoming(&vertex.id).iter();
                outgoing
                    .map(|edge| format!("out {edge:?}"))
                    .chain(incoming.map(|edge| format!("in {edge:?}")))
                    .collect::<Vec<_>>()
            })
            .collect()
    }

    /// 다른 도구가 만든 graph.json에는 같은 (from, to, kind) 간선이 반복될 수 있다.
    #[test]
    fn 중복_간선은_첫_등장_순서와_정렬된_근거로_복원한다() {
        let mut doc = graph_to_doc(&chain_sample());
        // 키의 첫 등장 순서가 정렬 순서와 다르고, 같은 키가 근거를 뒤섞인 채 반복한다.
        // 알 수 없는 kind·layer와 없는 정점을 가리키는 간선도 사이에 끼운다.
        doc.edges = vec![
            edge_doc("main.c", "main.b", "reads", &[("body-parse", "d3")]),
            edge_doc("main.c", "main.a", "reads", &[("body-parse", "x")]),
            edge_doc(
                "main.c",
                "main.b",
                "reads",
                &[("stats", "d1"), ("body-parse", "d2")],
            ),
            edge_doc("main.a", "main.b", "references", &[]),
            edge_doc("main.c", "main.missing", "reads", &[("body-parse", "gone")]),
            edge_doc(
                "main.c",
                "main.b",
                "future-kind",
                &[("body-parse", "later")],
            ),
            edge_doc(
                "main.c",
                "main.b",
                "reads",
                &[
                    ("future-layer", "z"),
                    ("body-parse", "d0"),
                    ("body-parse", "d3"),
                ],
            ),
            edge_doc("main.c", "main.a", "reads", &[("catalog", "w")]),
        ];
        let graph = graph_from_doc(&doc);
        let (b, c) = (VertexId::object("main", "b"), VertexId::object("main", "c"));
        let outgoing: Vec<_> = graph
            .outgoing(&c)
            .iter()
            .map(|edge| {
                let evidence: Vec<_> = edge
                    .evidence
                    .iter()
                    .map(|e| format!("{}:{}", layer_str(e.layer), e.detail))
                    .collect();
                (edge.to.as_str(), evidence)
            })
            .collect();
        let owned =
            |items: &[&str]| -> Vec<String> { items.iter().map(|item| item.to_string()).collect() };
        assert_eq!(
            outgoing,
            [
                (
                    "main.b",
                    owned(&[
                        "body-parse:d0",
                        "body-parse:d2",
                        "body-parse:d3",
                        "body-parse:d3",
                        "stats:d1",
                    ]),
                ),
                ("main.a", owned(&["catalog:w", "body-parse:x"])),
            ]
        );
        let incoming: Vec<_> = graph.incoming(&b).iter().map(|e| e.from.as_str()).collect();
        assert_eq!(incoming, ["main.c", "main.a"]);
        assert_eq!(
            graph.incoming(&b)[0].evidence,
            graph.outgoing(&c)[0].evidence
        );
        assert_eq!(
            graph.limitations(),
            [
                "graph.json의 간선 1개가 알 수 없는 kind라 버림 (버전 불일치?)",
                "graph.json의 간선 1개가 존재하지 않는 정점을 가리켜 버림",
            ]
        );
    }

    /// 외부 의존성 없이 무작위 문서를 만드는 결정적 xorshift64*.
    struct TestRng(u64);

    impl TestRng {
        fn below(&mut self, bound: usize) -> usize {
            self.0 ^= self.0 >> 12;
            self.0 ^= self.0 << 25;
            self.0 ^= self.0 >> 27;
            (self.0.wrapping_mul(0x2545_F491_4F6C_DD1D) % bound as u64) as usize
        }

        fn pick<'a>(&mut self, items: &[&'a str]) -> &'a str {
            items[self.below(items.len())]
        }
    }

    /// 고치기 전 적재 — 유효한 간선을 문서 순서대로 하나씩 add_edge한다.
    fn sequential_graph(doc: &GraphDoc) -> Graph {
        let mut graph = Graph::new();
        for v in &doc.vertices {
            let kind = vertex_kind_parse(&v.kind).expect("test vertices use known kinds");
            graph.add_vertex(Vertex {
                id: VertexId::from_raw(&v.id),
                kind,
                name: v.name.clone(),
                schema: v.schema.clone(),
            });
        }
        for e in &doc.edges {
            let (from, to) = (VertexId::from_raw(&e.from), VertexId::from_raw(&e.to));
            let Some(kind) = edge_kind_parse(&e.kind) else {
                continue;
            };
            if graph.vertex(&from).is_none() || graph.vertex(&to).is_none() {
                continue;
            }
            let evidence = e.evidence.iter().filter_map(|ev| {
                let layer = layer_parse(&ev.layer)?;
                let detail = ev.detail.clone();
                Some(Evidence { layer, detail })
            });
            let evidence = evidence.collect();
            graph.add_edge(Edge {
                from,
                to,
                kind,
                evidence,
            });
        }
        graph
    }

    #[test]
    fn 무작위_중복_간선_문서는_하나씩_add_edge한_그래프와_같다() {
        let ids = ["main.a", "main.b", "main.c", "main.missing"];
        let kinds = ["reads", "writes", "references", "future-kind"];
        let layers = ["catalog", "body-parse", "stats", "inferred", "future-layer"];
        let details = ["d0", "d1", "d2", "d3"];
        for seed in 1..=300u64 {
            let mut rng = TestRng(seed);
            let mut doc = graph_to_doc(&chain_sample());
            doc.edges = (0..30)
                .map(|_| {
                    let evidence: Vec<_> = (0..rng.below(4))
                        .map(|_| (rng.pick(&layers), rng.pick(&details)))
                        .collect();
                    let (from, to) = (rng.pick(&ids), rng.pick(&ids));
                    edge_doc(from, to, rng.pick(&kinds), &evidence)
                })
                .collect();
            let restored = graph_from_doc(&doc);
            assert_eq!(
                adjacency(&restored),
                adjacency(&sequential_graph(&doc)),
                "seed {seed}"
            );
        }
    }

    #[test]
    fn usage는_graph_json을_왕복하고_미수집은_키가_없다() {
        let mut g = sample();
        g.set_usage(
            VertexId::object("main", "a"),
            schemagraph_core::Usage {
                since: Some("2025-06-01".into()),
                reads: 9,
                writes: 2,
                scans: None,
                total_ms: Some(42.5),
                self_ms: Some(10.0),
            },
        );
        let doc = graph_to_doc(&g);
        let json = to_pretty_json(&doc).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        let a = v["vertices"]
            .as_array()
            .unwrap()
            .iter()
            .find(|x| x["id"] == "main.a")
            .unwrap();
        assert_eq!(a["usage"]["reads"], 9);
        assert_eq!(a["usage"]["since"], "2025-06-01");
        // 미수집 정점(main.b)은 usage 키가 아예 없다 — 0 관측과 구분된다.
        let b = v["vertices"]
            .as_array()
            .unwrap()
            .iter()
            .find(|x| x["id"] == "main.b")
            .unwrap();
        assert!(b.get("usage").is_none());

        let g2 = graph_from_doc(&serde_json::from_str::<GraphDoc>(&json).unwrap());
        let u = g2.usage(&VertexId::object("main", "a")).unwrap();
        assert_eq!((u.reads, u.writes), (9, 2));
        assert_eq!((u.total_ms, u.self_ms), (Some(42.5), Some(10.0)));
        assert!(g2.usage(&VertexId::object("main", "b")).is_none());
    }

    #[test]
    fn stats는_관측된_정점만_나열하고_총계를_싣는다() {
        let mut g = sample();
        g.set_usage(
            VertexId::object("main", "a"),
            schemagraph_core::Usage {
                since: None,
                reads: 0,
                writes: 0,
                scans: None,
                total_ms: None,
                self_ms: None,
            },
        );
        let v = stats_to_value(&g);
        let stats = v["stats"].as_array().unwrap();
        assert_eq!(stats.len(), 1);
        assert_eq!(stats[0]["id"], "main.a");
        assert_eq!(stats[0]["reads"], 0);
        // since 없음 → 키 생략 계약.
        assert!(stats[0].get("since").is_none());
        assert_eq!(v["totals"]["observed"], 1);
        // 스키마+테이블2 = 정점 3개 중 1개만 관측됐다.
        assert_eq!(v["totals"]["vertices"], 3);
        // 출력이 결정적이어야 한다.
        assert_eq!(
            serde_json::to_string(&stats_to_value(&g)).unwrap(),
            serde_json::to_string(&stats_to_value(&g)).unwrap()
        );
    }

    #[test]
    fn stats_상한_변형은_잘렸을_때만_truncated를_더하고_총계는_전체를_센다() {
        let mut g = sample();
        for name in ["a", "b"] {
            g.set_usage(VertexId::object("main", name), Default::default());
        }
        let cut = stats_to_value_with_max(&g, 1);
        let ids: Vec<&str> = cut["stats"]
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| entry["id"].as_str().unwrap())
            .collect();
        // 기존 순서(정점 id 순)의 앞부분을 남긴다.
        assert_eq!(ids, ["main.a"]);
        assert_eq!(cut["truncated"], true);
        assert_eq!(cut["totals"]["observed"], 2);
        assert_eq!(stats_to_value_with_max(&g, 2), stats_to_value(&g));
        assert!(stats_to_value(&g).get("truncated").is_none());
    }

    #[test]
    fn cycles_상한_변형은_잘렸을_때만_truncated와_total을_더한다() {
        let report = CyclesReport {
            cycles: vec![
                schemagraph_analysis::Cycle {
                    members: vec![VertexId::object("main", "a"), VertexId::object("main", "b")],
                    self_loop: false,
                },
                schemagraph_analysis::Cycle {
                    members: vec![VertexId::object("main", "c")],
                    self_loop: true,
                },
            ],
            level: Level::Object,
            limitations: vec!["probe limitation".into()],
        };
        let cut = cycles_to_value_with_max(&report, 1);
        assert_eq!(cut["cycles"].as_array().unwrap().len(), 1);
        assert_eq!(
            cut["cycles"][0]["members"],
            serde_json::json!(["main.a", "main.b"])
        );
        assert_eq!(
            (cut["truncated"].clone(), cut["total"].clone()),
            (true.into(), 2.into())
        );
        assert_eq!(cut["limitations"], serde_json::json!(["probe limitation"]));
        let whole = cycles_to_value(&report);
        assert_eq!(cycles_to_value_with_max(&report, 2), whole);
        assert!(whole.get("truncated").is_none() && whole.get("total").is_none());
    }

    #[test]
    fn not_found_상한_변형은_잘렸을_때만_후보_총수와_잘림을_싣는다() {
        let candidates: Vec<VertexId> = ["a", "b", "c"]
            .iter()
            .map(|schema| VertexId::object(schema, "id"))
            .collect();
        let limitations = vec!["probe limitation".to_owned()];
        let cut = not_found_value_with_max("id", &candidates, &limitations, 2);
        assert_eq!(cut["candidates"], serde_json::json!(["a.id", "b.id"]));
        assert_eq!(cut["candidatesTotal"], 3);
        assert_eq!(cut["candidatesTruncated"], true);
        assert_eq!(
            (cut["found"].clone(), cut["name"].clone()),
            (false.into(), "id".into())
        );
        assert_eq!(cut["limitations"], serde_json::json!(["probe limitation"]));
        assert_eq!(
            not_found_value_with_max("id", &candidates, &limitations, 3),
            not_found_value("id", &candidates, &limitations)
        );
    }
}
