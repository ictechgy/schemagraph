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
pub mod lint;
pub mod mermaid;
pub mod review;
pub mod sarif;
pub mod schema_metadata;
pub mod search;
pub mod stream;

/// graph.json의 와이어 버전. 형식이 깨지는 변경은 올린다.
pub const GRAPH_VERSION: u32 = 2;

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
                            total_ms: u.total_ms,
                            self_ms: u.self_ms,
                        },
                    );
                }
            }
            None => dropped_v += 1,
        }
    }
    for e in &doc.edges {
        let Some(kind) = edge_kind_parse(&e.kind) else {
            dropped_e += 1;
            continue;
        };
        let from = schemagraph_core::VertexId::from_raw(&e.from);
        let to = schemagraph_core::VertexId::from_raw(&e.to);
        if g.vertex(&from).is_none() || g.vertex(&to).is_none() {
            dangling_e += 1;
            continue;
        }
        g.add_edge(Edge {
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
/// impacted가 같은 형태다.
fn neighbors_value(ns: &[schemagraph_analysis::Neighbor]) -> serde_json::Value {
    serde_json::Value::Array(
        ns.iter()
            .map(|n| {
                serde_json::json!({
                    "distance": n.distance,
                    "edges": n.edges.iter().map(|k| edge_kind_str(*k)).collect::<Vec<_>>(),
                    "id": n.vertex.id.as_str(),
                    "kind": vertex_kind_str(n.vertex.kind),
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
        "limitations": report.limitations,
        "subject": subject_value(&report.subject),
        "truncated": report.truncated,
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

/// ImpactReport → JSON Value.
pub fn impact_to_value(report: &ImpactReport) -> serde_json::Value {
    serde_json::json!({
        "impacted": neighbors_value(&report.impacted),
        "limitations": report.limitations,
        "subject": subject_value(&report.subject),
        "truncated": report.truncated,
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
    let stats: Vec<serde_json::Value> = g
        .usages()
        .map(|(id, u)| {
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
            if let Some(ms) = u.total_ms {
                v["total_ms"] = serde_json::json!(ms);
            }
            if let Some(ms) = u.self_ms {
                v["self_ms"] = serde_json::json!(ms);
            }
            v
        })
        .collect();
    let total = g.vertices().count();
    serde_json::json!({
        "limitations": g.limitations(),
        "stats": stats,
        "totals": {
            "observed": g.usages().count(),
            "vertices": total,
        },
    })
}

/// CyclesReport → JSON Value.
pub fn cycles_to_value(report: &CyclesReport) -> serde_json::Value {
    serde_json::json!({
        "cycles": report.cycles.iter().map(|c| {
            serde_json::json!({
                "members": c.members.iter().map(|m| m.as_str()).collect::<Vec<_>>(),
                "selfLoop": c.self_loop,
            })
        }).collect::<Vec<_>>(),
        "level": level_str(report.level),
        "limitations": report.limitations,
    })
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

    #[test]
    fn usage는_graph_json을_왕복하고_미수집은_키가_없다() {
        let mut g = sample();
        g.set_usage(
            VertexId::object("main", "a"),
            schemagraph_core::Usage {
                since: Some("2025-06-01".into()),
                reads: 9,
                writes: 2,
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
}
