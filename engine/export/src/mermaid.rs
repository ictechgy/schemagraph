//! mermaid 출력. flowchart로 의존 간선만 그린다 — contains는 구조 정보지
//! 의존성이 아니라 넣으면 다이어그램이 소음으로 묻힌다.

use schemagraph_core::{EdgeKind, Graph};

/// Graph → mermaid flowchart 문자열.
///
/// 노드 id는 mermaid가 허용하는 문자로 정규화하고, 라벨에는 원래 id를 쓴다.
/// 의존성이 아닌 간선(contains·inferred)은 그리지 않는다.
pub fn to_mermaid(g: &Graph) -> String {
    let mut out = String::from("flowchart LR\n");
    // 간선이 하나라도 닿는 정점만 노드로 — 고립 정점까지 그리면 대형 스키마에서
    // 다이어그램이 읽히지 않는다.
    for edge in g.edges() {
        if !edge.kind.is_dependency() {
            continue;
        }
        out.push_str(&format!(
            "    {}[\"{}\"] -->|{}| {}[\"{}\"]\n",
            node_id(edge.from.as_str()),
            edge.from.as_str(),
            kind_label(edge.kind),
            node_id(edge.to.as_str()),
            edge.to.as_str(),
        ));
    }
    out
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
    }
}

/// mermaid 노드 id로 쓸 수 있게 비영숫자를 `_`로 바꾼다.
fn node_id(id: &str) -> String {
    id.chars()
        .map(|c| if c.is_alphanumeric() { c } else { '_' })
        .collect()
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
}
