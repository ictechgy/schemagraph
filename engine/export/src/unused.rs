//! 미사용 후보 보고서를 에이전트 출력 계약의 JSON으로 만든다.

use crate::UsageDoc;
use schemagraph_analysis::unused::{UnusedCandidate, UnusedReport};

/// 보고서를 JSON 값으로 만든다. 거짓·빈 사실은 키를 생략한다.
///
/// 후보는 "통계 창 안에서 읽힌 기록이 없다"는 관찰일 뿐 삭제 판정이 아니므로,
/// 판정을 흔드는 사실과 수집 한계(`limitations`)를 항상 함께 싣는다.
pub fn to_value(report: &UnusedReport, limitations: &[String]) -> serde_json::Value {
    serde_json::json!({
        "candidates": report.candidates.iter().map(candidate).collect::<Vec<_>>(),
        "limitations": limitations,
        "totalCandidates": report.total,
        "truncated": report.truncated,
        "unobserved": {
            "indexes": report.unobserved.indexes,
            "tables": report.unobserved.tables,
        },
    })
}

/// 후보 한 건 — usage는 since 생략 계약을 지키도록 UsageDoc으로 직렬화한다.
fn candidate(candidate: &UnusedCandidate) -> serde_json::Value {
    let usage = candidate.usage;
    let mut value = serde_json::json!({
        "id": candidate.vertex.id.as_str(),
        "kind": super::vertex_kind_str(candidate.vertex.kind),
        "usage": serde_json::to_value(UsageDoc {
            since: usage.since.clone(),
            reads: usage.reads,
            writes: usage.writes,
            scans: usage.scans,
            total_ms: usage.total_ms,
            self_ms: usage.self_ms,
        })
        .unwrap_or(serde_json::Value::Null),
    });
    let flags = [
        ("enforcesUniqueness", candidate.enforces_uniqueness),
        ("backsConstraint", candidate.backs_constraint),
        ("indexWithoutUsage", candidate.index_without_usage),
        ("windowUnknown", usage.since.is_none()),
    ];
    for (key, present) in flags {
        if present {
            value[key] = serde_json::json!(true);
        }
    }
    if !candidate.covers_foreign_keys.is_empty() {
        value["coversForeignKeys"] = serde_json::json!(candidate
            .covers_foreign_keys
            .iter()
            .map(|id| id.as_str())
            .collect::<Vec<_>>());
    }
    if candidate.body_dependents > 0 {
        value["bodyDependents"] = serde_json::json!(candidate.body_dependents);
    }
    value
}
