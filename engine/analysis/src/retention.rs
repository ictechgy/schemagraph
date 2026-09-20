//! 애플리케이션 진입점과 명시적 예외를 DB 내부 후보 판정에서 구분한다.

use schemagraph_core::{Graph, VertexId};
use std::collections::{BTreeMap, VecDeque};

/// 재현 가능한 기준일을 받아 예외 만료가 현재 시계에 숨겨서 의존하지 않게 한다.
#[derive(Debug, Clone, Default)]
pub struct RetentionPolicy {
    pub retain: Vec<String>,
    pub suppressions: Vec<Suppression>,
    pub as_of: Option<String>,
}

/// 예외는 후보를 숨기는 대신 보고된 후보에 이유를 붙인다.
#[derive(Debug, Clone)]
pub struct Suppression {
    pub pattern: String,
    pub reason: String,
    pub until: Option<String>,
}

/// 명시적 루트에서 실제 의존 간선만 따라간다. contains·inferred는 사용 근거가 아니다.
pub(crate) fn retained(graph: &Graph, policy: &RetentionPolicy) -> BTreeMap<VertexId, String> {
    let mut protected = BTreeMap::new();
    let mut queue = VecDeque::new();
    for v in graph.vertices() {
        if policy
            .retain
            .iter()
            .any(|p| crate::glob_match(p, v.id.as_str()))
        {
            protected.insert(v.id.clone(), "explicit-root".into());
            queue.push_back(v.id.clone());
        }
    }
    while let Some(id) = queue.pop_front() {
        for e in graph
            .outgoing(&id)
            .iter()
            .filter(|e| e.kind.is_dependency())
        {
            if graph.vertex(&e.to).is_some() && !protected.contains_key(&e.to) {
                protected.insert(e.to.clone(), "root-dependency".into());
                queue.push_back(e.to.clone());
            }
        }
    }
    protected
}

/// 일치하지 않는 설정과 만료된 예외가 조용히 적용된 것처럼 보이지 않게 한다.
pub(crate) fn policy_notes(graph: &Graph, policy: &RetentionPolicy) -> Vec<String> {
    let mut notes = Vec::new();
    for pattern in &policy.retain {
        if !graph
            .vertices()
            .any(|v| crate::glob_match(pattern, v.id.as_str()))
        {
            notes.push(format!("retain pattern '{pattern}' matches no vertices"));
        }
    }
    for suppression in &policy.suppressions {
        if suppression
            .until
            .as_ref()
            .is_some_and(|until| policy.as_of.as_ref().is_none_or(|date| date > until))
        {
            notes.push(format!(
                "suppression '{}' is expired or has no as-of date; not applied",
                suppression.pattern
            ));
        }
        if !graph
            .vertices()
            .any(|v| crate::glob_match(&suppression.pattern, v.id.as_str()))
        {
            notes.push(format!(
                "suppression pattern '{}' matches no vertices",
                suppression.pattern
            ));
        }
    }
    notes.sort();
    notes.dedup();
    notes
}

/// 실제 후보에만 예외를 적용해 존재하지 않는 위반의 억제를 보고하지 않는다.
pub(crate) fn suppression(id: &VertexId, policy: &RetentionPolicy) -> Option<String> {
    let reasons: std::collections::BTreeSet<_> = policy
        .suppressions
        .iter()
        .filter(|s| {
            !s.reason.trim().is_empty()
                && crate::glob_match(&s.pattern, id.as_str())
                && s.until
                    .as_ref()
                    .is_none_or(|until| policy.as_of.as_ref().is_some_and(|date| date <= until))
        })
        .map(|s| s.reason.as_str())
        .collect();
    (!reasons.is_empty()).then(|| reasons.into_iter().collect::<Vec<_>>().join("; "))
}

#[cfg(test)]
mod tests {
    use super::*;
    use schemagraph_core::{Edge, EdgeKind, Usage, Vertex, VertexKind};

    fn fixture() -> Graph {
        let mut g = Graph::new();
        for name in ["entry", "helper", "unused"] {
            g.add_vertex(Vertex {
                id: VertexId::object("s", name),
                kind: VertexKind::Function,
                name: name.into(),
                schema: "s".into(),
            });
        }
        g.add_edge(Edge {
            from: VertexId::object("s", "entry"),
            to: VertexId::object("s", "helper"),
            kind: EdgeKind::Calls,
            evidence: vec![],
        });
        g
    }

    #[test]
    fn explicit_entrypoint_protects_its_dependency_closure() {
        let g = fixture();
        let report = crate::dead_with_policy(
            &g,
            100,
            &RetentionPolicy {
                retain: vec!["s.entry".into()],
                ..Default::default()
            },
        );
        assert_eq!(report.candidates.len(), 1);
        assert_eq!(report.candidates[0].vertex.id.as_str(), "s.unused");
        assert_eq!(report.retained.len(), 2);
        assert_eq!(report.retained[1].1, "root-dependency");
    }

    #[test]
    fn suppressions_are_reported_only_on_real_candidates_and_expire() {
        let g = fixture();
        let policy = RetentionPolicy {
            retain: vec!["s.entry".into()],
            suppressions: vec![
                Suppression {
                    pattern: "s.*".into(),
                    reason: "external scheduled job".into(),
                    until: Some("2026-09-21".into()),
                },
                Suppression {
                    pattern: "s.missing".into(),
                    reason: "old exception".into(),
                    until: None,
                },
            ],
            as_of: Some("2026-09-20".into()),
        };
        let report = crate::dead_with_policy(&g, 100, &policy);
        assert_eq!(report.candidates.len(), 1);
        assert_eq!(
            report.candidates[0].suppression.as_deref(),
            Some("external scheduled job")
        );
        assert_eq!(report.unsuppressed_count, 0);
        assert!(report
            .limitations
            .iter()
            .any(|l| l.contains("matches no vertices")));
        let expired = RetentionPolicy {
            as_of: Some("2026-09-22".into()),
            ..policy
        };
        let report = crate::dead_with_policy(&g, 100, &expired);
        assert_eq!(report.unsuppressed_count, 1);
        assert!(report.candidates[0].suppression.is_none());
        assert!(report.limitations.iter().any(|l| l.contains("expired")));
    }

    #[test]
    fn truncation_does_not_turn_a_strict_failure_into_success() {
        let g = fixture();
        let report = crate::dead_with_policy(&g, 0, &RetentionPolicy::default());
        assert!(report.candidates.is_empty());
        assert!(report.truncated);
        assert_eq!(report.total_candidates, 3);
        assert_eq!(report.unsuppressed_count, 3);
    }

    #[test]
    fn usage_absence_zero_and_positive_values_remain_distinct_evidence() {
        let mut g = fixture();
        g.set_usage(
            VertexId::object("s", "helper"),
            Usage {
                reads: 0,
                since: Some("2026-09-20".into()),
                ..Usage::default()
            },
        );
        g.set_usage(
            VertexId::object("s", "unused"),
            Usage {
                reads: 7,
                since: Some("2026-09-20".into()),
                ..Usage::default()
            },
        );
        let report = crate::dead(&g, 100);
        assert!(report.candidates[0].usage.is_none());
        assert_eq!(report.candidates[1].usage.as_ref().unwrap().reads, 0);
        assert_eq!(report.candidates[2].usage.as_ref().unwrap().reads, 7);
    }
}
