//! schemagraph-analysis — 그래프 위의 판정 질의.
//!
//! 출력은 그래프 사실이지 판정이 아니다. "도달할 수 없다"고 말할 뿐
//! "지워도 된다"고는 말하지 않는다(AGENTS.md "삭제 판정 금지").

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use schemagraph_core::{EdgeKind, Graph, Level, Vertex, VertexId};

/// 질의 대상을 못 찾았을 때. `notFound`에도 limitations를 싣는다 —
/// 없는 것과 이 도구가 못 보는 것을 소비자가 구분해야 한다.
#[derive(Debug)]
pub enum Resolve {
    Found(VertexId),
    /// 유일 후보가 없다. 가까운 후보들을 실어준다.
    NotFound { candidates: Vec<VertexId> },
}

/// 이름 또는 정규 id를 정점으로 해석한다.
///
/// 정확한 id가 먼저이고, 없으면 말단 이름 일치가 유일할 때만 받아들인다 —
/// 모호한 짧은 이름을 임의로 골라 잡으면 소비자가 엉뚱한 객체를 보게 된다.
pub fn resolve(graph: &Graph, name: &str) -> Resolve {
    // 정규 id 그대로 입력된 경우가 먼저다.
    let exact = VertexId::schema(name);
    if graph.vertex(&exact).is_some() {
        return Resolve::Found(exact);
    }
    // 말단 이름 일치는 유일할 때만 받아들인다 — 모호한 짧은 이름을 임의로
    // 골라 잡으면 소비자가 엉뚱한 객체를 보게 된다.
    let mut candidates: Vec<VertexId> = graph
        .vertices()
        .filter(|v| v.name == name)
        .map(|v| v.id.clone())
        .collect();
    match candidates.len() {
        1 => Resolve::Found(candidates.remove(0)),
        _ => Resolve::NotFound { candidates },
    }
}

/// 이웃 하나와 그쪽으로 가는 간선 종류들.
///
/// 이웃에 닿는 간선은 전부 준다 — 하나만 고르면 나머지 관계가 사라진다
/// (DESIGN.md "에이전트 출력 계약").
#[derive(Debug, Clone)]
pub struct Neighbor {
    pub vertex: Vertex,
    /// subject와 이 이웃 사이의 간선 종류(정렬·중복 제거).
    pub edges: Vec<EdgeKind>,
    /// subject로부터의 최단 의존 거리.
    pub distance: u32,
}

/// `query` 결과 보고.
#[derive(Debug)]
pub struct QueryReport {
    /// 해석된 대상 정점.
    pub subject: Vertex,
    /// 이 대상을 의존하는 이웃들(들어오는 방향).
    pub dependents: Vec<Neighbor>,
    /// 이 대상이 의존하는 이웃들(나가는 방향).
    pub dependencies: Vec<Neighbor>,
    /// 대상이 자기 자신을 가리키는 의존 간선의 종류(자기 참조 FK 등).
    /// 이웃 목록에 섞지 않고 따로 싣는다 — 자기를 이웃으로 세면 거리가
    /// 왜곡되고, 안 실으면 자기 의존이라는 사실이 사라진다.
    pub self_edges: Vec<EdgeKind>,
    pub depth: u32,
    /// 최대 이웃 수에 걸려 잘렸으면 true — 잘렸으면 잘렸다고 말한다.
    pub truncated: bool,
    /// 그래프의 실측 한계를 그대로 계승.
    pub limitations: Vec<String>,
}

/// BFS로 depth까지의 의존성 이웃을 모은다. `contains`는 의존성이 아니라
/// 따라가지 않는다. `inferred`는 추정이라 거리 계산에서도 제외한다 —
/// 추정 간선으로 도달성이 오염되지 않게 한다.
pub fn query(graph: &Graph, subject: &VertexId, depth: u32, max_neighbors: usize) -> QueryReport {
    let vertex = graph
        .vertex(subject)
        .expect("query는 resolve로 확인된 id만 받는다");

    let (dependents, dep_trunc) = bfs(graph, subject, depth, max_neighbors, Direction::In);
    let (dependencies, out_trunc) = bfs(graph, subject, depth, max_neighbors, Direction::Out);

    // 자기 참조는 이웃이 아니라 자기 간선으로 보고한다(cycles의 selfLoop와 같은 처리).
    let mut self_edges: Vec<EdgeKind> = graph
        .outgoing(subject)
        .iter()
        .filter(|e| e.to == *subject && e.kind.is_dependency())
        .map(|e| e.kind)
        .collect();
    self_edges.sort();
    self_edges.dedup();

    QueryReport {
        subject: vertex.clone(),
        dependents,
        dependencies,
        self_edges,
        depth,
        truncated: dep_trunc || out_trunc,
        limitations: graph.limitations().to_vec(),
    }
}

enum Direction {
    In,
    Out,
}

fn bfs(
    graph: &Graph,
    root: &VertexId,
    depth: u32,
    max_neighbors: usize,
    direction: Direction,
) -> (Vec<Neighbor>, bool) {
    // 정점 → (최단 거리, 그 방향 간선 종류 집합)
    let mut seen: BTreeMap<VertexId, (u32, BTreeSet<EdgeKind>)> = BTreeMap::new();
    let mut queue: VecDeque<(VertexId, u32)> = VecDeque::new();
    queue.push_back((root.clone(), 0));
    let mut visited = BTreeSet::new();
    visited.insert(root.clone());
    let mut truncated = false;

    while let Some((id, dist)) = queue.pop_front() {
        if dist >= depth {
            continue;
        }
        let edges = match direction {
            Direction::Out => graph.outgoing(&id),
            Direction::In => graph.incoming(&id),
        };
        for edge in edges {
            if !edge.kind.is_dependency() {
                continue;
            }
            let next = match direction {
                Direction::Out => edge.to.clone(),
                Direction::In => edge.from.clone(),
            };
            if next == *root {
                continue;
            }
            seen.entry(next.clone())
                .or_insert_with(|| (dist + 1, BTreeSet::new()))
                .1
                .insert(edge.kind);
            if visited.insert(next.clone()) {
                queue.push_back((next, dist + 1));
            }
        }
    }

    let mut neighbors: Vec<Neighbor> = seen
        .into_iter()
        .filter_map(|(id, (distance, kinds))| {
            graph.vertex(&id).map(|v| Neighbor {
                vertex: v.clone(),
                edges: kinds.into_iter().collect(),
                distance,
            })
        })
        .collect();
    neighbors.sort_by(|a, b| (&a.vertex.id, a.distance).cmp(&(&b.vertex.id, b.distance)));
    if neighbors.len() > max_neighbors {
        neighbors.truncate(max_neighbors);
        truncated = true;
    }
    (neighbors, truncated)
}

/// 순환 하나. 크기 1은 자기 참조(`self_loop`)다.
#[derive(Debug)]
pub struct Cycle {
    pub members: Vec<VertexId>,
    pub self_loop: bool,
}

/// `cycles` 보고.
#[derive(Debug)]
pub struct CyclesReport {
    pub cycles: Vec<Cycle>,
    pub level: Level,
    pub limitations: Vec<String>,
}

/// 주어진 레벨로 투영한 뒤 SCC로 순환을 잡는다.
///
/// 크기>1 SCC는 순환이고, 크기 1은 자기 의존 간선이 있을 때만 순환이다
/// (SCC 알고리즘은 자기 루프도 크기 1로 돌려주므로 간선 존재를 별도 확인).
pub fn cycles(graph: &Graph, level: Level) -> CyclesReport {
    let projected = graph.project(level);
    let mut cycles: Vec<Cycle> = Vec::new();
    for component in projected.strongly_connected() {
        if component.len() > 1 {
            cycles.push(Cycle {
                members: component,
                self_loop: false,
            });
        } else {
            let id = &component[0];
            let has_self = projected
                .outgoing(id)
                .iter()
                .any(|e| e.to == *id && e.kind.is_dependency());
            if has_self {
                cycles.push(Cycle {
                    members: component,
                    self_loop: true,
                });
            }
        }
    }
    cycles.sort_by(|a, b| a.members.cmp(&b.members));
    CyclesReport {
        cycles,
        level,
        limitations: graph.limitations().to_vec(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use schemagraph_core::{Edge, Evidence, EvidenceLayer, Vertex, VertexKind};

    fn table(schema: &str, name: &str) -> Vertex {
        Vertex {
            id: VertexId::object(schema, name),
            kind: VertexKind::Table,
            name: name.into(),
            schema: schema.into(),
        }
    }

    fn dep(from: &VertexId, to: &VertexId) -> Edge {
        Edge {
            from: from.clone(),
            to: to.clone(),
            kind: EdgeKind::References,
            evidence: vec![Evidence {
                layer: EvidenceLayer::Catalog,
                detail: "test".into(),
            }],
        }
    }

    /// a -> b -> c 체인.
    fn chain() -> Graph {
        let mut g = Graph::new();
        for n in ["a", "b", "c"] {
            g.add_vertex(table("s", n));
        }
        g.add_edge(dep(&VertexId::object("s", "a"), &VertexId::object("s", "b")));
        g.add_edge(dep(&VertexId::object("s", "b"), &VertexId::object("s", "c")));
        g
    }

    #[test]
    fn 이름은_유일할_때만_해석된다() {
        let g = chain();
        match resolve(&g, "a") {
            Resolve::Found(id) => assert_eq!(id.as_str(), "s.a"),
            _ => panic!("resolve 실패"),
        }
        match resolve(&g, "s.a") {
            Resolve::Found(id) => assert_eq!(id.as_str(), "s.a"),
            _ => panic!("정규 id 해석 실패"),
        }
        assert!(matches!(resolve(&g, "nope"), Resolve::NotFound { .. }));
    }

    #[test]
    fn query는_양방향_이웃과_간선종류를_준다() {
        let g = chain();
        let report = query(&g, &VertexId::object("s", "b"), 2, 256);
        // a가 b를 의존 → dependents에 a.
        assert!(report
            .dependents
            .iter()
            .any(|n| n.vertex.id.as_str() == "s.a"));
        // b는 c를 의존 → dependencies에 c.
        assert!(report
            .dependencies
            .iter()
            .any(|n| n.vertex.id.as_str() == "s.c"));
        assert!(!report.truncated);
    }

    #[test]
    fn query는_contains를_타지_않는다() {
        let mut g = chain();
        g.add_vertex(Vertex {
            id: VertexId::member("s", "a", "id"),
            kind: VertexKind::Column,
            name: "id".into(),
            schema: "s".into(),
        });
        g.add_edge(Edge {
            from: VertexId::object("s", "a"),
            to: VertexId::member("s", "a", "id"),
            kind: EdgeKind::Contains,
            evidence: vec![],
        });
        let report = query(&g, &VertexId::object("s", "a"), 3, 256);
        // contains는 의존성이 아니므로 dependencies에 컬럼이 나오지 않는다.
        assert!(!report
            .dependencies
            .iter()
            .any(|n| n.vertex.id.as_str() == "s.a.id"));
    }

    #[test]
    fn cycles는_순환과_자기루프를_보고한다() {
        let mut g = chain();
        // c -> a 추가로 a->b->c->a 순환 완성.
        g.add_edge(dep(&VertexId::object("s", "c"), &VertexId::object("s", "a")));
        // 자기 참조 d.
        g.add_vertex(table("s", "d"));
        g.add_edge(dep(&VertexId::object("s", "d"), &VertexId::object("s", "d")));
        // 순환에 안 끼는 e.
        g.add_vertex(table("s", "e"));

        let report = cycles(&g, Level::Object);
        assert_eq!(report.cycles.len(), 2);
        let big = &report.cycles[0];
        assert_eq!(big.members.len(), 3);
        assert!(!big.self_loop);
        let solo = &report.cycles[1];
        assert_eq!(solo.members.len(), 1);
        assert!(solo.self_loop);
    }

    #[test]
    fn member의_순환은_object로_투영해_잡는다() {
        let mut g = Graph::new();
        for (t, c) in [("a", "x"), ("b", "y")] {
            g.add_vertex(table("s", t));
            g.add_vertex(Vertex {
                id: VertexId::member("s", t, c),
                kind: VertexKind::Column,
                name: c.into(),
                schema: "s".into(),
            });
        }
        g.add_edge(dep(
            &VertexId::member("s", "a", "x"),
            &VertexId::member("s", "b", "y"),
        ));
        g.add_edge(dep(
            &VertexId::member("s", "b", "y"),
            &VertexId::member("s", "a", "x"),
        ));
        // object 레벨에서도 순환이 보여야 한다.
        let report = cycles(&g, Level::Object);
        assert_eq!(report.cycles.len(), 1);
        assert_eq!(report.cycles[0].members.len(), 2);
    }
}
