//! 이름 패턴으로 정점을 찾는다 — 에이전트가 정확한 id를 모를 때 쓰는 첫 단계다.
//!
//! 전체 그래프를 한 번에 내보내지 않고 이름·요약 단계로 나눠 공개하므로,
//! 큰 스키마에서도 호출자가 필요한 만큼만 받는다. 판정은 하지 않고 일치한
//! 정점과 의존 이웃 수만 보고한다.

use crate::glob_match;
use schemagraph_core::{Graph, Vertex, VertexId, VertexKind};
use std::collections::BTreeSet;

/// 검색 결과 한 건 — 정점과 의존 이웃 수다.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchHit<'a> {
    pub vertex: &'a Vertex,
    /// 이 정점이 의존하는 서로 다른 정점 수(자기 자신 제외).
    pub dependencies: usize,
    /// 이 정점에 의존하는 서로 다른 정점 수(자기 자신 제외).
    pub dependents: usize,
}

/// 검색 결과 — `total`은 상한과 무관한 전체 일치 수다.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchReport<'a> {
    pub matches: Vec<SearchHit<'a>>,
    pub total: usize,
    pub truncated: bool,
}

/// 패턴에 맞는 정점을 id 순으로 최대 `max`개 돌려준다.
///
/// `*`·`?`가 있으면 id 전체에 대한 글롭, 없으면 id 또는 말단 이름의 부분
/// 문자열 일치다. 둘 다 대소문자를 무시한다 — SQL 방언마다 식별자 대소문자
/// 규칙이 달라 호출자가 저장된 표기를 알기 어렵기 때문이다.
pub fn search<'a>(
    graph: &'a Graph,
    pattern: &str,
    kind: Option<VertexKind>,
    max: usize,
) -> SearchReport<'a> {
    let needle = pattern.to_lowercase();
    let is_glob = needle.contains(['*', '?']);
    let mut matches = Vec::new();
    let mut total = 0usize;
    for vertex in graph.vertices() {
        if kind.is_some_and(|kind| vertex.kind != kind) || !matches_vertex(vertex, &needle, is_glob)
        {
            continue;
        }
        total += 1;
        if matches.len() < max {
            matches.push(hit(graph, vertex));
        }
    }
    SearchReport {
        truncated: total > matches.len(),
        matches,
        total,
    }
}

/// 한 정점이 소문자로 접은 패턴과 맞는지 판정한다.
fn matches_vertex(vertex: &Vertex, needle: &str, is_glob: bool) -> bool {
    let id = vertex.id.as_str().to_lowercase();
    if is_glob {
        return glob_match(needle, &id);
    }
    id.contains(needle) || vertex.name.to_lowercase().contains(needle)
}

/// 의존 간선(`contains`·`inferred` 제외)의 서로 다른 이웃 수를 센다.
fn hit<'a>(graph: &'a Graph, vertex: &'a Vertex) -> SearchHit<'a> {
    let distinct = |ids: Vec<&VertexId>| {
        ids.into_iter()
            .filter(|id| **id != vertex.id)
            .collect::<BTreeSet<_>>()
            .len()
    };
    let outgoing = graph.outgoing(&vertex.id).iter();
    let incoming = graph.incoming(&vertex.id).iter();
    SearchHit {
        vertex,
        dependencies: distinct(
            outgoing
                .filter(|e| e.kind.is_dependency())
                .map(|e| &e.to)
                .collect(),
        ),
        dependents: distinct(
            incoming
                .filter(|e| e.kind.is_dependency())
                .map(|e| &e.from)
                .collect(),
        ),
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
        let report = search(&graph, "ORDER", None, 10);
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
            ids(&search(&graph, "public.orders.*", None, 10)),
            ["public.Orders.id"]
        );
        assert_eq!(
            ids(&search(&graph, "*s", None, 10)),
            ["public.Orders", "public.customers", "public.order_totals"]
        );
    }

    #[test]
    fn kind_filter_and_truncation_keep_the_full_total() {
        let graph = graph();
        let tables = search(&graph, "public", Some(VertexKind::Table), 1);
        assert_eq!(ids(&tables), ["public.Orders"]);
        assert_eq!((tables.total, tables.truncated), (2, true));
    }

    #[test]
    fn neighbor_counts_skip_contains_and_merge_parallel_edges() {
        let graph = graph();
        let report = search(&graph, "public.Orders", Some(VertexKind::Table), 10);
        let hit = &report.matches[0];
        // contains(→ id 컬럼)는 의존이 아니고, order_totals의 두 간선은 한 이웃이다.
        assert_eq!((hit.dependencies, hit.dependents), (1, 1));
    }
}
