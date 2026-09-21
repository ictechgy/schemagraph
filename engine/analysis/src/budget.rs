//! 예산을 분리한 의존성 이웃 탐색.
//!
//! 결과 개수(`max_results`)와 탐색 예산은 서로 다른 계약이다. 결과 개수는
//! 전체 탐색이 끝난 뒤 출력만 자르며, 정점·간선 예산과 취소는 탐색을 일찍
//! 끝내고 `complete`를 false로 만든다. 따라서 예산이 소진된 보고서는
//! "없다"를 증명하는 보고서로 소비하면 안 된다.

use crate::Neighbor;
use schemagraph_core::{Edge, EdgeKind, Graph, VertexId};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};

/// 이웃 탐색이 소비할 수 있는 최대 작업량.
///
/// `max_visited`에는 루트 정점도 포함한다. 두 값이 모두 충분히 크면
/// 탐색은 그래프의 요청된 `depth` 범위에서 완료된다. 한 노드의 인접 간선을
/// 결정적으로 정렬하기 위해 해당 노드의 인접 리스트를 임시로 복사하므로,
/// 이 예산은 dense node 하나의 정렬 비용을 상수 시간으로 만들지는 않는다.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Budget {
    /// 탐색 중 방문해 기록할 수 있는 정점 수(루트 포함).
    pub max_visited: usize,
    /// 탐색 중 검사할 수 있는 의존성 간선 수.
    pub max_examined_edges: usize,
}

impl Budget {
    /// 제한 없는 탐색 예산.
    pub const fn unlimited() -> Self {
        Self {
            max_visited: usize::MAX,
            max_examined_edges: usize::MAX,
        }
    }
}

impl Default for Budget {
    fn default() -> Self {
        Self::unlimited()
    }
}

/// 예산을 적용한 이웃 탐색 결과.
///
/// `complete`가 false이면 예산 소진·취소·없는 루트 중 하나 때문에 그래프
/// 부재를 결론내릴 수 없다. `result-limit`만 있는 경우에는 탐색 자체는
/// 완료했으므로 `complete`가 true다.
#[derive(Debug, Clone)]
pub struct BudgetedReport {
    /// 최단 거리 순회로 발견한 이웃들. 최종 순서는 정점 id 순이다.
    pub neighbors: Vec<Neighbor>,
    /// 실제로 방문해 기록한 정점 수(루트 포함, 루트가 없으면 0).
    pub visited: usize,
    /// 실제로 검사한 의존성 간선 수.
    pub examined_edges: usize,
    /// 탐색 또는 출력이 제한된 이유. 항상 사전식으로 정렬·중복 제거한다.
    pub truncation_reasons: Vec<String>,
    /// 요청된 depth 범위의 탐색이 완전히 끝났는가.
    pub complete: bool,
    /// `root`가 그래프에 존재했는가.
    pub root_found: bool,
}

/// 한 루트에서 의존성 이웃을 예산에 맞춰 탐색한다.
///
/// `reverse == false`이면 나가는 간선(루트가 의존하는 대상)을 따르고,
/// `reverse == true`이면 들어오는 간선(루트에 의존하는 정점)을 따른다.
/// `contains`와 `inferred`를 포함한 비의존성 간선은 검사 대상에서 제외한다.
/// 같은 이웃에 여러 경로가 닿으면 최단 거리를 유지하면서 그 이웃에
/// 도착하는 모든 의존성 간선 종류를 모은다. 자기 간선은 이웃에서 제외해
/// 기존 `query`의 `self_edges` 처리와 겹치지 않게 한다.
///
/// `max_results`는 출력만 제한한다. 이 값 때문에 탐색을 중단하지 않으며,
/// 결과가 잘리면 `result-limit`을 기록해도 `complete`는 true로 남을 수 있다.
/// 반대로 정점·간선 예산 소진과 취소는 `complete`를 false로 만든다.
pub fn walk(
    graph: &Graph,
    root: &VertexId,
    depth: u32,
    max_results: usize,
    reverse: bool,
    budget: Budget,
    cancel: Option<&AtomicBool>,
) -> BudgetedReport {
    let mut reasons = BTreeSet::new();
    if cancelled(cancel) {
        reasons.insert("cancelled");
    }
    let root_found = graph.vertex(root).is_some();
    if !root_found {
        reasons.insert("root-not-found");
        return report(Vec::new(), 0, 0, reasons, false, false);
    }

    // `max_visited == 0`은 명시적인 무작업 예산이다. 정점 예산이 없을 때는
    // 루트의 인접 리스트에도 손대지 않는다.
    if budget.max_visited == 0 {
        reasons.insert("visited-limit");
        return report(Vec::new(), 0, 0, reasons, false, true);
    }

    let mut seen: BTreeMap<VertexId, (u32, BTreeSet<EdgeKind>)> = BTreeMap::new();
    let mut queue = VecDeque::new();
    seen.insert(root.clone(), (0, BTreeSet::new()));
    queue.push_back(root.clone());
    let mut visited = 1usize;
    let mut examined_edges = 0usize;

    'walk: while let Some(id) = queue.pop_front() {
        if cancelled(cancel) {
            reasons.insert("cancelled");
            break;
        }
        let distance = seen[&id].0;
        if distance >= depth {
            continue;
        }

        // 간선 예산을 이미 다 썼으면 dense adjacency를 스캔하거나 복사하지
        // 않는다. 이 검사는 수집보다 먼저 해야 한다.
        if examined_edges >= budget.max_examined_edges {
            reasons.insert("edge-limit");
            break;
        }

        let adjacency = if reverse {
            graph.incoming(&id)
        } else {
            graph.outgoing(&id)
        };
        let mut edges: Vec<&Edge> = Vec::new();
        for edge in adjacency {
            if cancelled(cancel) {
                reasons.insert("cancelled");
                break 'walk;
            }
            if edge.kind.is_dependency() {
                edges.push(edge);
            }
        }
        // 그래프 삽입 순서는 질의 계약이 아니다. 이 정점의 참조만 정렬하면
        // 그래프 전체를 복사하지 않고도 탐색을 결정적으로 만들 수 있다. 남는
        // 비용은 이 정점의 차수에 비례한다.
        edges.sort_by(|a, b| (&a.from, &a.to, a.kind).cmp(&(&b.from, &b.to, b.kind)));
        if cancelled(cancel) {
            reasons.insert("cancelled");
            break;
        }

        for edge in edges {
            if cancelled(cancel) {
                reasons.insert("cancelled");
                break 'walk;
            }
            if examined_edges >= budget.max_examined_edges {
                reasons.insert("edge-limit");
                break 'walk;
            }
            examined_edges += 1;

            let next = if reverse { &edge.from } else { &edge.to };
            // 자기 간선은 호출자가 query의 별도 self_edges 필드로 보고한다.
            // 이웃 거리를 바꾸면 안 된다.
            if next == root {
                continue;
            }
            // 잘못 만들어진 그래프에는 등록되지 않은 id를 가리키는 간선이
            // 있을 수 있다. 유령 id를 노출하거나 정점 예산을 쓰지 않는다.
            if graph.vertex(next).is_none() {
                continue;
            }

            let candidate_distance = distance + 1;
            if let Some((_, kinds)) = seen.get_mut(next) {
                kinds.insert(edge.kind);
                continue;
            }
            if seen.len() >= budget.max_visited {
                reasons.insert("visited-limit");
                break 'walk;
            }
            let mut kinds = BTreeSet::new();
            kinds.insert(edge.kind);
            seen.insert(next.clone(), (candidate_distance, kinds));
            visited += 1;
            queue.push_back(next.clone());
        }
    }

    let mut neighbors = Vec::new();
    for (id, (distance, edges)) in seen {
        if cancelled(cancel) {
            reasons.insert("cancelled");
            break;
        }
        if id == *root {
            continue;
        }
        if let Some(vertex) = graph.vertex(&id) {
            neighbors.push(Neighbor {
                vertex: vertex.clone(),
                edges: edges.into_iter().collect(),
                distance,
            });
        }
    }
    neighbors.sort_by(|a, b| a.vertex.id.cmp(&b.vertex.id));
    if cancelled(cancel) {
        reasons.insert("cancelled");
    }

    if neighbors.len() > max_results {
        reasons.insert("result-limit");
        neighbors.truncate(max_results);
    }
    let complete = !reasons.iter().any(|reason| *reason != "result-limit");
    report(neighbors, visited, examined_edges, reasons, complete, true)
}

fn cancelled(cancel: Option<&AtomicBool>) -> bool {
    cancel.is_some_and(|flag| flag.load(Ordering::Relaxed))
}

fn report(
    neighbors: Vec<Neighbor>,
    visited: usize,
    examined_edges: usize,
    reasons: BTreeSet<&str>,
    complete: bool,
    root_found: bool,
) -> BudgetedReport {
    BudgetedReport {
        neighbors,
        visited,
        examined_edges,
        truncation_reasons: reasons.into_iter().map(str::to_owned).collect(),
        complete,
        root_found,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::query;
    use schemagraph_core::{Edge, Vertex, VertexKind};

    fn vertex(name: &str) -> Vertex {
        Vertex {
            id: VertexId::object("s", name),
            kind: VertexKind::Table,
            name: name.into(),
            schema: "s".into(),
        }
    }

    fn edge(from: &str, to: &str, kind: EdgeKind) -> Edge {
        Edge {
            from: VertexId::object("s", from),
            to: VertexId::object("s", to),
            kind,
            evidence: vec![],
        }
    }

    fn fixture() -> Graph {
        let mut graph = Graph::new();
        for name in ["a", "b", "c", "d"] {
            graph.add_vertex(vertex(name));
        }
        // a -> b에는 여러 종류가 있고 b -> c -> a가 순환을 만든다. d는 두
        // 경로로 도달하므로 먼저 발견한 최단 거리를 유지해야 한다.
        for (from, to, kind) in [
            ("a", "b", EdgeKind::Reads),
            ("a", "b", EdgeKind::Calls),
            ("b", "c", EdgeKind::Writes),
            ("c", "a", EdgeKind::References),
            ("b", "d", EdgeKind::Reads),
            ("a", "d", EdgeKind::Calls),
            ("a", "a", EdgeKind::References),
            ("a", "d", EdgeKind::Contains),
            ("a", "d", EdgeKind::Inferred),
        ] {
            graph.add_edge(edge(from, to, kind));
        }
        graph
    }

    fn ids(report: &BudgetedReport) -> Vec<&str> {
        report
            .neighbors
            .iter()
            .map(|neighbor| neighbor.vertex.id.as_str())
            .collect()
    }

    #[test]
    fn complete_walk_matches_query_and_keeps_all_kinds() {
        let graph = fixture();
        let root = VertexId::object("s", "a");
        let report = walk(
            &graph,
            &root,
            3,
            usize::MAX,
            false,
            Budget::unlimited(),
            None,
        );
        let expected = query(&graph, &root, 3, usize::MAX);
        assert!(report.complete);
        assert_eq!(ids(&report), vec!["s.b", "s.c", "s.d"]);
        assert_eq!(report.neighbors.len(), expected.dependencies.len());
        for (actual, expected) in report.neighbors.iter().zip(expected.dependencies) {
            assert_eq!(actual.vertex.id, expected.vertex.id);
            assert_eq!(actual.edges, expected.edges);
            assert_eq!(actual.distance, expected.distance);
        }
        assert_eq!(
            report.neighbors[0].edges,
            vec![EdgeKind::Reads, EdgeKind::Calls]
        );
        assert_eq!(report.neighbors[2].distance, 1);
        assert!(!report.neighbors[2].edges.contains(&EdgeKind::Contains));
        assert!(!report.neighbors[2].edges.contains(&EdgeKind::Inferred));
    }

    #[test]
    fn reverse_walk_is_deterministic_and_preserves_shortest_distance() {
        let graph = fixture();
        let report = walk(
            &graph,
            &VertexId::object("s", "d"),
            4,
            usize::MAX,
            true,
            Budget::unlimited(),
            None,
        );
        assert_eq!(ids(&report), vec!["s.a", "s.b", "s.c"]);
        assert_eq!(report.neighbors[0].distance, 1);
        assert_eq!(report.neighbors[1].distance, 1);
        assert_eq!(report.neighbors[2].distance, 2);
    }

    #[test]
    fn output_limit_does_not_stop_traversal() {
        let graph = fixture();
        let report = walk(
            &graph,
            &VertexId::object("s", "a"),
            3,
            1,
            false,
            Budget::unlimited(),
            None,
        );
        assert_eq!(ids(&report), vec!["s.b"]);
        assert_eq!(report.truncation_reasons, vec!["result-limit"]);
        assert!(report.complete);
        assert_eq!(report.visited, 4);
    }

    #[test]
    fn zero_limits_and_cancellation_are_explicit() {
        let graph = fixture();
        let root = VertexId::object("s", "a");
        let no_vertices = walk(
            &graph,
            &root,
            3,
            usize::MAX,
            false,
            Budget {
                max_visited: 0,
                max_examined_edges: usize::MAX,
            },
            None,
        );
        assert_eq!(no_vertices.visited, 0);
        assert_eq!(no_vertices.truncation_reasons, vec!["visited-limit"]);
        assert!(!no_vertices.complete);

        let no_edges = walk(
            &graph,
            &root,
            3,
            usize::MAX,
            false,
            Budget {
                max_visited: usize::MAX,
                max_examined_edges: 0,
            },
            None,
        );
        assert_eq!(no_edges.visited, 1);
        assert_eq!(no_edges.examined_edges, 0);
        assert_eq!(no_edges.truncation_reasons, vec!["edge-limit"]);
        assert!(!no_edges.complete);

        let cancelled_flag = AtomicBool::new(true);
        let cancelled = walk(
            &graph,
            &root,
            3,
            usize::MAX,
            false,
            Budget::unlimited(),
            Some(&cancelled_flag),
        );
        assert_eq!(cancelled.visited, 1);
        assert_eq!(cancelled.truncation_reasons, vec!["cancelled"]);
        assert!(!cancelled.complete);

        let cancelled_zero_budget = walk(
            &graph,
            &root,
            3,
            usize::MAX,
            false,
            Budget {
                max_visited: 0,
                max_examined_edges: usize::MAX,
            },
            Some(&cancelled_flag),
        );
        assert_eq!(
            cancelled_zero_budget.truncation_reasons,
            vec!["cancelled", "visited-limit"]
        );
        assert!(!cancelled_zero_budget.complete);
    }

    #[test]
    fn missing_root_is_a_safe_incomplete_outcome() {
        let graph = fixture();
        let report = walk(
            &graph,
            &VertexId::object("s", "missing"),
            2,
            10,
            false,
            Budget::unlimited(),
            None,
        );
        assert!(!report.root_found);
        assert!(!report.complete);
        assert!(report.neighbors.is_empty());
        assert_eq!(report.truncation_reasons, vec!["root-not-found"]);

        let cancelled_missing = walk(
            &graph,
            &VertexId::object("s", "missing"),
            2,
            10,
            false,
            Budget::unlimited(),
            Some(&AtomicBool::new(true)),
        );
        assert_eq!(
            cancelled_missing.truncation_reasons,
            vec!["cancelled", "root-not-found"]
        );
    }
}
