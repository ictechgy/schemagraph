//! 정규화한 변경을 이전 그래프에 연결해 삭제된 객체의 의존성을 놓치지 않는다.

use crate::{budget, Neighbor};
use schemagraph_core::{AnalysisState, Graph, VertexId};
use std::sync::atomic::AtomicBool;

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
    review_with_cancellation(
        before,
        after,
        changes,
        comparison_notes,
        max_changes,
        max_impacted,
        budget::Budget::unlimited(),
        None,
    )
}

/// 변경별 impact 탐색에 명시적 정점·간선 예산을 적용한다.
pub fn review_with_budget(
    before: &Graph,
    after: &Graph,
    changes: Vec<Change>,
    comparison_notes: Vec<String>,
    max_changes: usize,
    max_impacted: usize,
    budget: budget::Budget,
) -> ReviewReport {
    review_with_cancellation(
        before,
        after,
        changes,
        comparison_notes,
        max_changes,
        max_impacted,
        budget,
        None,
    )
}

/// 변경별 영향 탐색을 취소 토큰과 예산에 맞춰 수행한다.
pub fn review_with_cancellation(
    before: &Graph,
    after: &Graph,
    mut changes: Vec<Change>,
    mut comparison_notes: Vec<String>,
    max_changes: usize,
    max_impacted: usize,
    budget: budget::Budget,
    cancel: Option<&AtomicBool>,
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
    let incomplete_indexes = [before, after]
        .iter()
        .filter_map(|graph| graph.schema_metadata())
        .flat_map(|metadata| metadata.indexes.values())
        .filter(|index| !index.complete)
        .count();
    let incomplete_foreign_keys = [before, after]
        .iter()
        .filter_map(|graph| graph.schema_metadata())
        .flat_map(|metadata| metadata.foreign_keys.values())
        .filter(|key| !key.complete)
        .count();
    let analysis_partial =
        partial(before) || partial(after) || incomplete_indexes > 0 || incomplete_foreign_keys > 0;
    let mut truncated = total_changes > max_changes;
    let mut visited = 0usize;
    let mut examined_edges = 0usize;
    let mut truncation_reasons = std::collections::BTreeSet::<String>::new();
    let mut complete = total_changes <= max_changes;
    let mut findings = Vec::new();
    for change in changes.into_iter().take(max_changes) {
        if cancelled(cancel) {
            truncation_reasons.insert("cancelled".into());
            truncated = true;
            complete = false;
            break;
        }
        let finding = {
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
                    cancel,
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
        };
        if finding
            .impact_truncation_reasons
            .iter()
            .any(|reason| reason == "cancelled")
        {
            truncation_reasons.insert("cancelled".into());
        }
        findings.push(finding);
        if cancelled(cancel) {
            truncation_reasons.insert("cancelled".into());
            truncated = true;
            complete = false;
            break;
        }
    }
    if cancelled(cancel) {
        truncation_reasons.insert("cancelled".into());
        truncated = true;
        complete = false;
    }
    let mut limitations: Vec<_> = before
        .limitations()
        .iter()
        .map(|n| format!("before: {n}"))
        .chain(after.limitations().iter().map(|n| format!("after: {n}")))
        .collect();
    if analysis_partial {
        limitations.push("one or both snapshots have missing or partial dependency/schema analysis; absence of impact is not proof of safety".into());
    }
    if incomplete_indexes > 0 || incomplete_foreign_keys > 0 {
        limitations.push(format!("across both snapshots, {incomplete_indexes} index definitions and {incomplete_foreign_keys} foreign-key mappings are incomplete"));
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

fn cancelled(cancel: Option<&AtomicBool>) -> bool {
    cancel.is_some_and(|flag| flag.load(std::sync::atomic::Ordering::Relaxed))
}

#[cfg(test)]
mod tests {
    use super::*;
    use schemagraph_core::{Edge, EdgeKind, Vertex, VertexKind};
    use std::sync::atomic::AtomicBool;
    #[test]
    fn incomplete_index_definition_cannot_look_like_complete_change_coverage() {
        let mut graph = Graph::new();
        let mut metadata = schemagraph_core::SchemaMetadata::default();
        metadata.indexes.insert(
            VertexId::member("s", "t", "expression"),
            schemagraph_core::IndexMetadata {
                table: VertexId::object("s", "t"),
                columns: vec![],
                unique: false,
                has_predicate: false,
                complete: false,
            },
        );
        graph.set_schema_metadata(metadata);
        let report = review(&graph, &Graph::new(), vec![], vec![], 10, 10);
        assert!(report.analysis_partial);
        assert!(report
            .limitations
            .iter()
            .any(|note| note.contains("1 index definitions")));
    }
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

    #[test]
    fn cancellation_preserves_counts_and_reason_without_processing_changes() {
        let cancelled = AtomicBool::new(true);
        let absent = Change {
            id: VertexId::object("s", "missing"),
            kind: "object-removed".into(),
            before: None,
            after: None,
        };
        let report = review_with_cancellation(
            &Graph::new(),
            &Graph::new(),
            vec![absent],
            vec!["comparison unavailable".into()],
            10,
            10,
            budget::Budget::unlimited(),
            Some(&cancelled),
        );
        assert_eq!(report.total_changes, 1);
        assert_eq!(report.review_required, 1);
        assert!(report.findings.is_empty());
        assert!(report.truncated);
        assert!(!report.complete);
        assert_eq!(report.truncation_reasons, vec!["cancelled"]);

        let empty = review_with_cancellation(
            &Graph::new(),
            &Graph::new(),
            vec![],
            vec![],
            10,
            10,
            budget::Budget::unlimited(),
            Some(&cancelled),
        );
        assert_eq!(empty.total_changes, 0);
        assert_eq!(empty.review_required, 0);
        assert!(empty.findings.is_empty());
        assert!(empty.truncated);
        assert!(!empty.complete);
        assert_eq!(empty.truncation_reasons, vec!["cancelled"]);
    }
}
