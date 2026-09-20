//! mermaid 출력. flowchart로 의존 간선만 그린다 — contains는 구조 정보지
//! 의존성이 아니라 넣으면 다이어그램이 소음으로 묻힌다.

use schemagraph_core::{EdgeKind, Graph, VertexKind};
use std::collections::{BTreeMap, BTreeSet};

/// Graph → mermaid flowchart 문자열.
///
/// 의존성이 아닌 간선(contains·inferred)과 고립 정점은 그리지 않는다 —
/// 대형 스키마에서 다이어그램이 읽히지 않기 때문이다. 노드 id는 원본 id와
/// 무관하게 `n0..`를 쓴다 — 정규화 방식은 `@` 접미사 id와 평범한 이름이
/// 같은 노드로 찌그러지는 충돌이 있다. 스키마별 subgraph로 묶고, 정점
/// kind마다 다른 도형을 쓴다(테이블은 실린더, view는 평행사변형, routine은
/// 육각형, trigger는 비대칭형). 간선 화살표는 종류별로: 쓰기는 굵게,
/// calls/fires 같은 몸체 증거는 점선으로.
pub fn to_mermaid(g: &Graph) -> String {
    // 간선이 하나라도 닿는 정점만 — 정렬 후 번호를 매겨야 결정적이다.
    let mut touched: BTreeSet<String> = BTreeSet::new();
    let mut dep_edges = Vec::new();
    for edge in g.edges() {
        if !edge.kind.is_dependency() {
            continue;
        }
        touched.insert(edge.from.as_str().to_owned());
        touched.insert(edge.to.as_str().to_owned());
        dep_edges.push(edge);
    }
    let node_ids: BTreeMap<String, String> = touched
        .iter()
        .enumerate()
        .map(|(i, id)| (id.clone(), format!("n{i}")))
        .collect();

    let mut out = String::from("flowchart LR\n");
    // 스키마별 subgraph — 멀티 스키마 그래프에서 소속이 보이게 한다.
    let mut by_schema: BTreeMap<String, Vec<&str>> = BTreeMap::new();
    for id in &touched {
        let schema = g
            .vertex(&schemagraph_core::VertexId::from_raw(id))
            .map(|v| v.schema.as_str())
            .unwrap_or_default();
        by_schema
            .entry(schema.to_owned())
            .or_default()
            .push(id.as_str());
    }
    for (schema, ids) in &by_schema {
        if schema.is_empty() {
            // 스키마가 안 잡히는 정점(투영된 집계 id 등)은 subgraph 밖에 둔다.
            for id in ids {
                out.push_str(&node_decl(g, id, &node_ids));
            }
            continue;
        }
        out.push_str(&format!(
            "    subgraph {}[\"{}\"]\n",
            node_id(schema),
            esc(schema)
        ));
        for id in ids {
            out.push_str(&node_decl(g, id, &node_ids));
        }
        out.push_str("    end\n");
    }
    for edge in dep_edges {
        out.push_str(&format!(
            "    {} {}|{}| {}\n",
            node_ids[edge.from.as_str()],
            arrow(edge.kind),
            kind_label(edge.kind),
            node_ids[edge.to.as_str()],
        ));
    }
    out
}

/// 정점 하나의 노드 선언문 — kind별 도형으로 무엇을 보는지 읽히게 한다.
fn node_decl(g: &Graph, id: &str, node_ids: &BTreeMap<String, String>) -> String {
    let label = esc(id);
    let nid = &node_ids[id];
    let kind = g
        .vertex(&schemagraph_core::VertexId::from_raw(id))
        .map(|v| v.kind);
    let decl = match kind {
        Some(VertexKind::Table) => format!("{nid}[(\"{label}\")]"),
        Some(VertexKind::View) | Some(VertexKind::MaterializedView) => {
            format!("{nid}[/\"{label}\"/]")
        }
        Some(VertexKind::Function) | Some(VertexKind::Procedure) | Some(VertexKind::Package) => {
            format!("{nid}{{{{\"{label}\"}}}}")
        }
        Some(VertexKind::Trigger) => format!("{nid}>\"{label}\"]"),
        Some(VertexKind::Sequence) => format!("{nid}((\"{label}\"))"),
        _ => format!("{nid}[\"{label}\"]"),
    };
    format!("        {decl}\n")
}

/// 간선 화살표 — 쓰기는 굵게, 몸체 파싱에서 온 동적 간선은 점선으로.
fn arrow(kind: EdgeKind) -> &'static str {
    match kind {
        EdgeKind::Writes => "==>",
        EdgeKind::Calls | EdgeKind::Fires => "-.->",
        _ => "-->",
    }
}

fn kind_label(kind: EdgeKind) -> &'static str {
    match kind {
        EdgeKind::References => "references",
        EdgeKind::Reads => "reads",
        EdgeKind::Writes => "writes",
        EdgeKind::Calls => "calls",
        EdgeKind::Fires => "fires",
        EdgeKind::UsesSequence => "uses-seq",
        EdgeKind::UsesType => "uses-type",
        // 아래 둘은 호출자가 걸러서 여기 오지 않는다.
        EdgeKind::Contains => "contains",
        EdgeKind::Inferred => "inferred",
        EdgeKind::DerivesFrom => "derives-from",
        EdgeKind::DependsOn => "depends-on",
    }
}

/// subgraph id용 정규화 — 여기만 쓰고 노드 id에는 쓰지 않는다.
fn node_id(id: &str) -> String {
    id.chars()
        .map(|c| if c.is_alphanumeric() { c } else { '_' })
        .collect()
}

/// 라벨 안의 따옴표는 mermaid를 깬다 — 작은따옴표로 바꾼다.
fn esc(s: &str) -> String {
    s.replace('"', "'")
}

#[cfg(test)]
mod tests {
    use super::*;
    use schemagraph_core::{Edge, Evidence, EvidenceLayer, Vertex, VertexId, VertexKind};

    #[test]
    fn 의존_간선만_그린다() {
        let mut g = Graph::new();
        let a = VertexId::object("main", "a");
        let b = VertexId::object("main", "b");
        for (id, n) in [(&a, "a"), (&b, "b")] {
            g.add_vertex(Vertex {
                id: id.clone(),
                kind: VertexKind::Table,
                name: n.into(),
                schema: "main".into(),
            });
        }
        g.add_edge(Edge {
            from: a.clone(),
            to: b.clone(),
            kind: EdgeKind::References,
            evidence: vec![Evidence {
                layer: EvidenceLayer::Catalog,
                detail: "fk".into(),
            }],
        });
        g.add_edge(Edge {
            from: a,
            to: VertexId::member("main", "a", "id"),
            kind: EdgeKind::Contains,
            evidence: vec![],
        });
        let m = to_mermaid(&g);
        assert!(m.contains("-->|references|"));
        assert!(!m.contains("contains"));
    }

    #[test]
    fn 노드는_한_번만_선언되고_kind_도형을_쓴다() {
        let mut g = Graph::new();
        for (id, kind) in [
            ("main.t", VertexKind::Table),
            ("main.v", VertexKind::View),
            ("main.f", VertexKind::Function),
        ] {
            g.add_vertex(Vertex {
                id: VertexId::from_raw(id),
                kind,
                name: id.into(),
                schema: "main".into(),
            });
        }
        for (from, to, kind) in [
            ("main.t", "main.v", EdgeKind::References),
            ("main.v", "main.t", EdgeKind::Reads),
            ("main.f", "main.t", EdgeKind::Writes),
        ] {
            g.add_edge(Edge {
                from: VertexId::from_raw(from),
                to: VertexId::from_raw(to),
                kind,
                evidence: vec![Evidence {
                    layer: EvidenceLayer::Catalog,
                    detail: "t".into(),
                }],
            });
        }
        let m = to_mermaid(&g);
        // 각 정점은 정확히 한 번만 선언된다.
        assert_eq!(m.matches("[(\"").count(), 1, "테이블 실린더 1개: {m}");
        assert!(m.contains("[/\"") && m.contains("{{\""));
        // writes는 굵은 화살표다.
        assert!(m.contains("==>|writes|"), "{m}");
        // 스키마 subgraph로 묶인다.
        assert!(m.contains("subgraph main"), "{m}");
        // 같은 쌍의 여러 간선도 노드 재선언이 없어야 한다.
        assert_eq!(m.matches("main.f").count(), 1, "{m}");
    }
}
