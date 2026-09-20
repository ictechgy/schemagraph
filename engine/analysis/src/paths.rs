//! 경로의 중간 객체와 모든 간선 근거를 보존하며 탐색·출력 예산을 구분한다.

use schemagraph_core::{Edge, Graph, ObjectAnalysis, Origin, Usage, Vertex, VertexId};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};

/// 출력 수와 별개로 수행할 수 있는 그래프 탐색량을 제한한다.
#[derive(Debug, Clone, Copy)]
pub struct SearchOptions {
    pub max_paths: usize,
    pub max_depth: u32,
    pub max_visited: usize,
    pub max_edges: usize,
    pub reverse: bool,
}

impl Default for SearchOptions {
    fn default() -> Self {
        Self {
            max_paths: 32,
            max_depth: 32,
            max_visited: 100_000,
            max_edges: 1_000_000,
            reverse: false,
        }
    }
}

/// 찾지 못함과 예산 때문에 탐색을 마치지 못함을 구분하는 경로 보고다.
#[derive(Debug)]
pub struct PathReport {
    pub from: VertexId,
    pub to: VertexId,
    pub reverse: bool,
    pub paths: Vec<Vec<VertexId>>,
    pub edges: Vec<Edge>,
    pub visited: usize,
    pub examined_edges: usize,
    pub truncated: bool,
    pub truncation_reasons: Vec<String>,
    pub limitations: Vec<String>,
}

/// 공개 그래프의 실제 간선만 따라 모든 최단 경로를 사전식으로 보고한다.
pub fn paths(
    graph: &Graph,
    from: &VertexId,
    to: &VertexId,
    options: SearchOptions,
    cancelled: Option<&AtomicBool>,
) -> PathReport {
    let mut reasons = BTreeSet::new();
    let mut distance = BTreeMap::new();
    let mut parents: BTreeMap<VertexId, BTreeSet<VertexId>> = BTreeMap::new();
    let mut queue = VecDeque::new();
    let mut examined = 0usize;
    let mut target_depth = None;
    if graph.vertex(from).is_none() || graph.vertex(to).is_none() {
        return PathReport {
            from: from.clone(),
            to: to.clone(),
            reverse: options.reverse,
            paths: vec![],
            edges: vec![],
            visited: 0,
            examined_edges: 0,
            truncated: false,
            truncation_reasons: vec![],
            limitations: vec!["path endpoints must exist in the graph".into()],
        };
    }
    if options.max_visited == 0 {
        reasons.insert("visited-limit");
    } else {
        distance.insert(from.clone(), 0u32);
        queue.push_back(from.clone());
    }
    if from == to {
        target_depth = Some(0);
    }
    'search: while let Some(id) = queue.pop_front() {
        if cancelled.is_some_and(|flag| flag.load(Ordering::Relaxed)) {
            reasons.insert("cancelled");
            break;
        }
        let depth = distance[&id];
        if target_depth.is_some_and(|d| depth >= d) {
            continue;
        }
        let edges = if options.reverse {
            graph.incoming(&id)
        } else {
            graph.outgoing(&id)
        };
        let mut edges: Vec<_> = edges.iter().filter(|e| e.kind.is_dependency()).collect();
        edges.sort_by(|a, b| (&a.from, &a.to, a.kind).cmp(&(&b.from, &b.to, b.kind)));
        if depth >= options.max_depth {
            if edges
                .iter()
                .any(|e| !distance.contains_key(if options.reverse { &e.from } else { &e.to }))
            {
                reasons.insert("depth-limit");
            }
            continue;
        }
        for edge in edges {
            if examined >= options.max_edges {
                reasons.insert("edge-limit");
                break 'search;
            }
            examined += 1;
            let next = if options.reverse {
                &edge.from
            } else {
                &edge.to
            };
            if graph.vertex(next).is_none() {
                continue;
            }
            let candidate = depth + 1;
            if !distance.contains_key(next) {
                if distance.len() >= options.max_visited {
                    reasons.insert("visited-limit");
                    break 'search;
                }
                distance.insert(next.clone(), candidate);
                queue.push_back(next.clone());
            }
            if distance[next] == candidate {
                parents.entry(next.clone()).or_default().insert(id.clone());
            }
            if next == to {
                target_depth = Some(candidate);
            }
        }
    }
    let mut found = Vec::new();
    if distance.contains_key(to) {
        let mut pending = vec![vec![to.clone()]];
        while let Some(path) = pending.pop() {
            if cancelled.is_some_and(|flag| flag.load(Ordering::Relaxed)) {
                reasons.insert("cancelled");
                break;
            }
            let last = path.last().expect("a path is initialized with the target");
            if last == from {
                if found.len() >= options.max_paths {
                    reasons.insert("path-limit");
                    break;
                }
                found.push(path.into_iter().rev().collect::<Vec<_>>());
            } else if let Some(predecessors) = parents.get(last) {
                // 경로 복원의 펼침 순서도 id 순으로 고정한다.
                for predecessor in predecessors.iter().rev() {
                    let mut next = path.clone();
                    next.push(predecessor.clone());
                    pending.push(next);
                }
            }
        }
    }
    found.sort();
    let mut used = BTreeMap::new();
    for path in &found {
        for pair in path.windows(2) {
            let (a, b) = if options.reverse {
                (&pair[1], &pair[0])
            } else {
                (&pair[0], &pair[1])
            };
            for edge in graph
                .outgoing(a)
                .iter()
                .filter(|e| &e.to == b && e.kind.is_dependency())
            {
                used.insert(
                    (edge.from.clone(), edge.to.clone(), edge.kind),
                    edge.clone(),
                );
            }
        }
    }
    PathReport {
        from: from.clone(),
        to: to.clone(),
        reverse: options.reverse,
        paths: found,
        edges: used.into_values().collect(),
        visited: distance.len(),
        examined_edges: examined,
        truncated: !reasons.is_empty(),
        truncation_reasons: reasons.into_iter().map(str::to_owned).collect(),
        limitations: graph.limitations().to_vec(),
    }
}

/// 하나의 근거에 여러 SQL 위치나 카탈로그 출처가 있을 수 있다.
#[derive(Debug)]
pub struct ExplainedEdge {
    pub edge: Edge,
    pub origins: Vec<Origin>,
}

/// 직접 연결된 관계와 SQL 근거를 전달하며 추정·소유 관계도 종류 그대로 보여준다.
#[derive(Debug)]
pub struct Explanation {
    pub subject: Vertex,
    pub edges: Vec<ExplainedEdge>,
    pub analysis: Option<ObjectAnalysis>,
    pub usage: Option<Usage>,
    pub total_edges: usize,
    pub truncated: bool,
    pub limitations: Vec<String>,
}

/// 간선 원본과 provenance를 연결한다. 이 설명은 새 간선을 추론하지 않는다.
pub fn explain(graph: &Graph, subject: &VertexId, max_edges: usize) -> Option<Explanation> {
    let vertex = graph.vertex(subject)?.clone();
    let mut edges = BTreeMap::new();
    for edge in graph
        .outgoing(subject)
        .iter()
        .chain(graph.incoming(subject))
    {
        edges.insert((edge.from.clone(), edge.to.clone(), edge.kind), edge);
    }
    let total_edges = edges.len();
    let edges = edges
        .into_iter()
        .take(max_edges)
        .map(|(key, edge)| ExplainedEdge {
            edge: edge.clone(),
            origins: graph
                .origins()
                .get(&key)
                .map(|o| o.iter().cloned().collect())
                .unwrap_or_default(),
        })
        .collect();
    Some(Explanation {
        subject: vertex,
        edges,
        analysis: graph.analysis().get(subject).cloned(),
        usage: graph.usage(subject).cloned(),
        total_edges,
        truncated: total_edges > max_edges,
        limitations: graph.limitations().to_vec(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use schemagraph_core::{EdgeKind, VertexKind};
    fn fixture() -> Graph {
        let mut g = Graph::new();
        for name in ["a", "b", "c", "d"] {
            g.add_vertex(Vertex {
                id: VertexId::object("s", name),
                kind: VertexKind::View,
                name: name.into(),
                schema: "s".into(),
            });
        }
        for (a, b, kind) in [
            ("a", "b", EdgeKind::Reads),
            ("a", "b", EdgeKind::Calls),
            ("a", "c", EdgeKind::Reads),
            ("b", "d", EdgeKind::Reads),
            ("c", "d", EdgeKind::Reads),
            ("d", "a", EdgeKind::Reads),
        ] {
            g.add_edge(Edge {
                from: VertexId::object("s", a),
                to: VertexId::object("s", b),
                kind,
                evidence: vec![],
            });
        }
        g
    }
    #[test]
    fn shortest_paths_include_parallel_kinds_and_terminate_on_cycles() {
        let g = fixture();
        let report = paths(
            &g,
            &VertexId::object("s", "a"),
            &VertexId::object("s", "d"),
            SearchOptions::default(),
            None,
        );
        assert_eq!(report.paths.len(), 2);
        assert_eq!(report.edges.len(), 5);
        assert!(!report.truncated);
        assert_eq!(
            report.paths[0]
                .iter()
                .map(VertexId::as_str)
                .collect::<Vec<_>>(),
            vec!["s.a", "s.b", "s.d"]
        );
    }
    #[test]
    fn output_and_exploration_limits_are_separate() {
        let g = fixture();
        let from = VertexId::object("s", "a");
        let to = VertexId::object("s", "d");
        let limited = paths(
            &g,
            &from,
            &to,
            SearchOptions {
                max_paths: 1,
                ..Default::default()
            },
            None,
        );
        assert_eq!(limited.paths.len(), 1);
        assert_eq!(limited.truncation_reasons, vec!["path-limit"]);
        let bounded = paths(
            &g,
            &from,
            &to,
            SearchOptions {
                max_visited: 1,
                ..Default::default()
            },
            None,
        );
        assert!(bounded.paths.is_empty());
        assert_eq!(bounded.visited, 1);
        assert_eq!(bounded.truncation_reasons, vec!["visited-limit"]);
        let flag = AtomicBool::new(true);
        let cancelled = paths(&g, &from, &to, SearchOptions::default(), Some(&flag));
        assert_eq!(cancelled.truncation_reasons, vec!["cancelled"]);
    }
    #[test]
    fn reverse_direction_keeps_original_edge_direction() {
        let g = fixture();
        let report = paths(
            &g,
            &VertexId::object("s", "d"),
            &VertexId::object("s", "a"),
            SearchOptions {
                reverse: true,
                ..Default::default()
            },
            None,
        );
        assert_eq!(report.paths.len(), 2);
        assert!(report.edges.iter().all(|e| e.from.as_str() != "s.d"));
    }
}
