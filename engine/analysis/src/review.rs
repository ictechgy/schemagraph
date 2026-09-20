//! 정규화한 변경을 이전 그래프에 연결해 삭제된 객체의 의존성을 놓치지 않는다.

use crate::{budget, Neighbor};
use schemagraph_core::{AnalysisState, Graph, VertexId};

/// 변경 탐지 계층이 전달하는 사실이며 실제 DB의 안전성 판정은 아니다.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Change {
    pub id: VertexId,
    pub kind: String,
    pub before: Option<String>,
    pub after: Option<String>,
}

impl Change {
    /// 새 컬럼이라도 NOT NULL이나 제약 추가는 명시적인 검토 대상으로 구분한다.
    pub fn requires_review(&self) -> bool {
        !matches!(
            self.kind.as_str(),
            "object-added" | "column-added" | "index-added"
        )
    }
}

/// 변경 근거와 변경 전 해당 대상에 의존했던 객체를 연결한다.
#[derive(Debug)]
pub struct Finding {
    pub change: Change,
    pub impacted: Vec<Neighbor>,
    pub impact_truncated: bool,
    pub impact_visited: usize,
    pub impact_examined_edges: usize,
    pub impact_truncation_reasons: Vec<String>,
    pub impact_complete: bool,
    pub basis: &'static str,
}

/// 비교 불가·분석 부족·실제 발견 건수를 서로 구분한다.
#[derive(Debug)]
pub struct ReviewReport {
    pub findings: Vec<Finding>,
    pub total_changes: usize,
    pub review_required: usize,
    pub comparison_notes: Vec<String>,
    pub analysis_partial: bool,
    pub truncated: bool,
    pub visited: usize,
    pub examined_edges: usize,
    pub truncation_reasons: Vec<String>,
    pub complete: bool,
    pub limitations: Vec<String>,
}

/// 출력 상한에 가려진 변경이 strict 결과를 바꾸지 않도록 전체 건수를 먼저 센다.
pub fn review(
    before: &Graph,
    after: &Graph,
    changes: Vec<Change>,
    comparison_notes: Vec<String>,
    max_changes: usize,
    max_impacted: usize,
) -> ReviewReport {
    review_with_budget(
        before,
        after,
        changes,
        comparison_notes,
        max_changes,
        max_impacted,
        budget::Budget::unlimited(),
    )
}

/// 변경별 impact 탐색에 명시적 정점·간선 예산을 적용한다.
pub fn review_with_budget(
    before: &Graph,
    after: &Graph,
    mut changes: Vec<Change>,
    mut comparison_notes: Vec<String>,
    max_changes: usize,
    max_impacted: usize,
    budget: budget::Budget,
) -> ReviewReport {
    changes.sort();
    changes.dedup();
    comparison_notes.sort();
    comparison_notes.dedup();
    let total_changes = changes.len();
    let review_required = changes.iter().filter(|c| c.requires_review()).count();
    let partial = |graph: &Graph| {
        graph
            .vertices()
            .filter(|v| {
                matches!(
                    v.kind,
                    schemagraph_core::VertexKind::View
                        | schemagraph_core::VertexKind::MaterializedView
                        | schemagraph_core::VertexKind::Function
                        | schemagraph_core::VertexKind::Procedure
                        | schemagraph_core::VertexKind::Package
                        | schemagraph_core::VertexKind::Trigger
                        | schemagraph_core::VertexKind::Query
                )
            })
            .any(|v| {
                graph
                    .analysis()
                    .get(&v.id)
                    .is_none_or(|a| a.state != AnalysisState::Complete)
            })
    };
    let analysis_partial = partial(before) || partial(after);
    let mut truncated = total_changes > max_changes;
    let mut visited = 0usize;
    let mut examined_edges = 0usize;
    let mut truncation_reasons = std::collections::BTreeSet::new();
    let mut complete = total_changes <= max_changes;
    let findings = changes
        .into_iter()
        .take(max_changes)
        .map(|change| {
            let (graph, basis) = if before.vertex(&change.id).is_some() {
                (before, "before")
            } else {
                (after, "after")
            };
            let (
                impacted,
                impact_truncated,
                impact_visited,
                impact_examined_edges,
                impact_truncation_reasons,
                impact_complete,
            ) = if graph.vertex(&change.id).is_some() {
                let impact = budget::walk(
                    graph,
                    &change.id,
                    u32::MAX,
                    max_impacted,
                    true,
                    budget,
                    None,
                );
                visited += impact.visited;
                examined_edges += impact.examined_edges;
                truncation_reasons.extend(impact.truncation_reasons.iter().cloned());
                complete &= impact.complete;
                (
                    impact.neighbors,
                    !impact.truncation_reasons.is_empty(),
                    impact.visited,
                    impact.examined_edges,
                    impact.truncation_reasons,
                    impact.complete,
                )
            } else {
                (vec![], false, 0, 0, vec![], true)
            };
            truncated |= impact_truncated;
            Finding {
                change,
                impacted,
                impact_truncated,
                impact_visited,
                impact_examined_edges,
                impact_truncation_reasons,
                impact_complete,
                basis,
            }
        })
        .collect();
    let mut limitations: Vec<_> = before
        .limitations()
        .iter()
        .map(|n| format!("before: {n}"))
        .chain(after.limitations().iter().map(|n| format!("after: {n}")))
        .collect();
    if analysis_partial {
        limitations.push("one or both snapshots have missing or partial SQL analysis; absence of impact is not proof of safety".into());
    }
    limitations.sort();
    limitations.dedup();
    ReviewReport {
        findings,
        total_changes,
        review_required,
        comparison_notes,
        analysis_partial,
        truncated,
        visited,
        examined_edges,
        truncation_reasons: truncation_reasons.into_iter().collect(),
        complete,
        limitations,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use schemagraph_core::{Edge, EdgeKind, Vertex, VertexKind};
    #[test]
    fn deleted_target_is_traced_in_the_previous_graph() {
        let mut before = Graph::new();
        let after = Graph::new();
        for name in ["target", "consumer"] {
            before.add_vertex(Vertex {
                id: VertexId::object("s", name),
                kind: VertexKind::View,
                name: name.into(),
                schema: "s".into(),
            });
        }
        before.add_edge(Edge {
            from: VertexId::object("s", "consumer"),
            to: VertexId::object("s", "target"),
            kind: EdgeKind::Reads,
            evidence: vec![],
        });
        let change = Change {
            id: VertexId::object("s", "target"),
            kind: "object-removed".into(),
            before: None,
            after: None,
        };
        let report = review(&before, &after, vec![change.clone()], vec![], 10, 10);
        assert_eq!(
            report.findings[0].impacted[0].vertex.id.as_str(),
            "s.consumer"
        );
        let hidden = review(&before, &after, vec![change.clone()], vec![], 0, 0);
        assert!(hidden.truncated);
        assert_eq!(hidden.review_required, 1);
        assert!(hidden.findings.is_empty());

        let budgeted = review_with_budget(
            &before,
            &after,
            vec![change],
            vec![],
            10,
            10,
            budget::Budget {
                max_visited: 1,
                max_examined_edges: usize::MAX,
            },
        );
        assert!(!budgeted.complete);
        assert_eq!(budgeted.visited, 1);
        assert_eq!(
            budgeted.findings[0].impact_truncation_reasons,
            ["visited-limit"]
        );
    }
}
