//! schemagraph-export — 그래프와 질의 결과의 직렬화.
//!
//! 계약: **결정적 JSON.** serde_json의 Map은 기본이 BTreeMap이라 Value로
//! 변환하면 객체 키가 자동 정렬된다 — `to_pretty` 전에 반드시 Value를 거친다.
//! 같은 입력이 같은 바이트여야 리포트 diff와 캐시가 성립한다
//! (AGENTS.md "JSON 출력은 결정적이어야 합니다").

use schemagraph_analysis::{
    CyclesReport, DeadReason, DeadReport, ImpactReport, QueryReport, RulesReport,
};
use schemagraph_core::{Edge, EdgeKind, EvidenceLayer, Graph, Level, Vertex, VertexKind};
use serde::{Deserialize, Serialize};

pub mod mermaid;

/// graph.json의 와이어 버전. 형식이 깨지는 변경은 올린다.
pub const GRAPH_VERSION: u32 = 1;

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
    }
}

fn vertex_kind_parse(s: &str) -> Option<VertexKind> {
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
}

#[derive(Debug, Serialize, Deserialize)]
pub struct VertexDoc {
    pub id: String,
    pub kind: String,
    pub level: String,
    pub name: String,
    pub schema: String,
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
    for v in &doc.vertices {
        match vertex_kind_parse(&v.kind) {
            Some(kind) => g.add_vertex(Vertex {
                id: schemagraph_core::VertexId::from_raw(&v.id),
                kind,
                name: v.name.clone(),
                schema: v.schema.clone(),
            }),
            None => dropped_v += 1,
        }
    }
    for e in &doc.edges {
        let Some(kind) = edge_kind_parse(&e.kind) else {
            dropped_e += 1;
            continue;
        };
        g.add_edge(Edge {
            from: schemagraph_core::VertexId::from_raw(&e.from),
            to: schemagraph_core::VertexId::from_raw(&e.to),
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

/// DeadReport → JSON Value.
pub fn dead_to_value(report: &DeadReport) -> serde_json::Value {
    serde_json::json!({
        "candidates": report.candidates.iter().map(|c| {
            serde_json::json!({
                "id": c.vertex.id.as_str(),
                "kind": vertex_kind_str(c.vertex.kind),
                "reason": match c.reason {
                    DeadReason::NoDependents => "noDependents",
                    DeadReason::AllDependentsDead => "allDependentsDead",
                },
            })
        }).collect::<Vec<_>>(),
        "limitations": report.limitations,
        "truncated": report.truncated,
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
}
