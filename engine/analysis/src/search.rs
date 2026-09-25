//! 이름 패턴으로 정점을 찾는다 — 에이전트가 정확한 id를 모를 때 쓰는 첫 단계다.
//!
//! 전체 그래프를 한 번에 내보내지 않고 이름·요약 단계로 나눠 공개하므로,
//! 큰 스키마에서도 호출자가 필요한 만큼만 받는다. 판정은 하지 않고 일치한
//! 정점과 의존 이웃 수만 보고한다.

use crate::glob_match;
use schemagraph_core::{Graph, Vertex, VertexId, VertexKind};
use std::collections::BTreeSet;

/// 한 정점의 의존 이웃 수 — 병렬 간선은 한 이웃으로 센다.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NeighborCounts {
    /// 이 정점이 의존하는 서로 다른 정점 수(자기 자신 제외).
    pub dependencies: usize,
    /// 이 정점에 의존하는 서로 다른 정점 수(자기 자신 제외).
    pub dependents: usize,
}

/// 검색 결과 한 건.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchHit<'a> {
    /// 패턴과 일치한 정점.
    pub vertex: &'a Vertex,
    /// `count_neighbors`를 요청했을 때만 계산한 의존 이웃 수.
    pub neighbors: Option<NeighborCounts>,
}

/// 검색 결과.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchReport<'a> {
    /// id 순으로 최대 `max`개까지 담은 일치 정점.
    pub matches: Vec<SearchHit<'a>>,
    /// 상한과 무관한 전체 일치 수.
    pub total: usize,
    /// `total`이 담긴 수보다 커서 결과를 잘랐는지 여부.
    pub truncated: bool,
}

/// 검색 조건.
#[derive(Debug, Clone, Copy)]
pub struct SearchQuery<'p> {
    /// 부분 문자열 또는 `*`·`?` 글롭. 대소문자는 무시한다.
    pub pattern: &'p str,
    /// 지정하면 이 종류의 정점만 찾는다.
    pub kind: Option<VertexKind>,
    /// 담을 최대 결과 수.
    pub max: usize,
    /// 결과마다 의존 이웃 수를 셀지 여부 — 이름만 필요한 호출은 간선을 훑지 않는다.
    pub count_neighbors: bool,
}

/// 조건에 맞는 정점을 id 순으로 돌려준다.
///
/// `*`·`?`가 있으면 글롭, 없으면 부분 문자열 일치이며, 두 방식 모두 정규 id와
/// 말단 이름을 함께 비교한다 — 이름만 아는 호출자가 `orders*`처럼 한정자 없이
/// 찾아도 결과가 나와야 한다. 대소문자는 무시한다. SQL 방언마다 식별자 대소문자
/// 규칙이 달라 호출자가 저장된 표기를 알기 어렵기 때문이다.
pub fn search<'a>(graph: &'a Graph, query: SearchQuery) -> SearchReport<'a> {
    let needle = query.pattern.to_lowercase();
    let is_glob = needle.contains(['*', '?']);
    let mut report = SearchReport {
        matches: Vec::new(),
        total: 0,
        truncated: false,
    };
    let candidates = graph
        .vertices()
        .filter(|vertex| query.kind.is_none_or(|kind| vertex.kind == kind))
        .filter(|vertex| matches_vertex(vertex, &needle, is_glob));
    for vertex in candidates {
        report.total += 1;
        if report.matches.len() < query.max {
            let neighbors = query
                .count_neighbors
                .then(|| neighbor_counts(graph, vertex));
            report.matches.push(SearchHit { vertex, neighbors });
        }
    }
    report.truncated = report.total > report.matches.len();
    report
}

/// 한 정점의 id 또는 말단 이름이 소문자로 접은 패턴과 맞는지 판정한다.
fn matches_vertex(vertex: &Vertex, needle: &str, is_glob: bool) -> bool {
    let id = vertex.id.as_str().to_lowercase();
    let name = vertex.name.to_lowercase();
    if is_glob {
        glob_match(needle, &id) || glob_match(needle, &name)
    } else {
        id.contains(needle) || name.contains(needle)
    }
}

/// 의존 간선(`contains`·`inferred` 제외)의 서로 다른 이웃 수를 센다.
fn neighbor_counts(graph: &Graph, vertex: &Vertex) -> NeighborCounts {
    let distinct = |ids: Vec<&VertexId>| {
        ids.into_iter()
            .filter(|id| **id != vertex.id)
            .collect::<BTreeSet<_>>()
            .len()
    };
    let dependencies = graph
        .outgoing(&vertex.id)
        .iter()
        .filter(|e| e.kind.is_dependency());
    let dependents = graph
        .incoming(&vertex.id)
        .iter()
        .filter(|e| e.kind.is_dependency());
    NeighborCounts {
        dependencies: distinct(dependencies.map(|e| &e.to).collect()),
        dependents: distinct(dependents.map(|e| &e.from).collect()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use schemagraph_core::{Edge, EdgeKind, Evidence, EvidenceLayer};

    fn vertex(id: &str, kind: VertexKind, name: &str) -> Vertex {
        Vertex {
            id: VertexId::from_raw(id),
            kind,
            name: name.into(),
            schema: "public".into(),
        }
    }

    fn edge(from: &str, to: &str, kind: EdgeKind) -> Edge {
        Edge {
            from: VertexId::from_raw(from),
            to: VertexId::from_raw(to),
            kind,
            evidence: vec![Evidence {
                layer: EvidenceLayer::Catalog,
                detail: "test".into(),
            }],
        }
    }

    fn graph() -> Graph {
        let mut graph = Graph::new();
        graph.add_vertex(vertex("public.Orders", VertexKind::Table, "Orders"));
        graph.add_vertex(vertex(
            "public.order_totals",
            VertexKind::View,
            "order_totals",
        ));
        graph.add_vertex(vertex("public.customers", VertexKind::Table, "customers"));
        graph.add_vertex(vertex("public.Orders.id", VertexKind::Column, "id"));
        graph.add_edge(edge(
            "public.Orders",
            "public.Orders.id",
            EdgeKind::Contains,
        ));
        graph.add_edge(edge(
            "public.order_totals",
            "public.Orders",
            EdgeKind::Reads,
        ));
        graph.add_edge(edge(
            "public.order_totals",
            "public.Orders",
            EdgeKind::References,
        ));
        graph.add_edge(edge(
            "public.Orders",
            "public.customers",
            EdgeKind::References,
        ));
        graph
    }

    fn find<'a>(
        graph: &'a Graph,
        pattern: &str,
        kind: Option<VertexKind>,
        max: usize,
    ) -> SearchReport<'a> {
        search(
            graph,
            SearchQuery {
                pattern,
                kind,
                max,
                count_neighbors: true,
            },
        )
    }

    fn ids(report: &SearchReport) -> Vec<String> {
        report
            .matches
            .iter()
            .map(|hit| hit.vertex.id.as_str().to_owned())
            .collect()
    }

    #[test]
    fn substring_is_case_insensitive_and_sorted_by_id() {
        let graph = graph();
        let report = find(&graph, "ORDER", None, 10);
        assert_eq!(
            ids(&report),
            ["public.Orders", "public.Orders.id", "public.order_totals"]
        );
        assert_eq!((report.total, report.truncated), (3, false));
    }

    #[test]
    fn glob_matches_the_whole_id() {
        let graph = graph();
        assert_eq!(
            ids(&find(&graph, "public.orders.*", None, 10)),
            ["public.Orders.id"]
        );
        assert_eq!(
            ids(&find(&graph, "*s", None, 10)),
            ["public.Orders", "public.customers", "public.order_totals"]
        );
    }

    #[test]
    fn glob_also_matches_unqualified_names_and_names_skip_counts() {
        let graph = graph();
        assert_eq!(ids(&find(&graph, "orders*", None, 10)), ["public.Orders"]);
        let names_only = search(
            &graph,
            SearchQuery {
                pattern: "orders",
                kind: None,
                max: 10,
                count_neighbors: false,
            },
        );
        assert!(names_only.matches.iter().all(|hit| hit.neighbors.is_none()));
    }

    #[test]
    fn kind_filter_and_truncation_keep_the_full_total() {
        let graph = graph();
        let tables = find(&graph, "public", Some(VertexKind::Table), 1);
        assert_eq!(ids(&tables), ["public.Orders"]);
        assert_eq!((tables.total, tables.truncated), (2, true));
    }

    #[test]
    fn neighbor_counts_skip_contains_and_merge_parallel_edges() {
        let graph = graph();
        let report = find(&graph, "public.Orders", Some(VertexKind::Table), 10);
        let counts = report.matches[0].neighbors.expect("counts were requested");
        // contains(→ id 컬럼)는 의존이 아니고, order_totals의 두 간선은 한 이웃이다.
        assert_eq!((counts.dependencies, counts.dependents), (1, 1));
    }
}
