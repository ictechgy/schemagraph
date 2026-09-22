//! 같은 변경 검토 결과를 JSON과 사람이 읽는 Markdown으로 표현한다.

use schemagraph_analysis::review::ReviewReport;
use serde_json::{json, Value};

pub use crate::sarif::{
    to_value as to_sarif, to_value_with_policy as to_sarif_with_policy, FindingAnnotation,
    Suppression,
};

/// 분석 불완전과 검토가 필요한 변경을 별도 필드로 표현한다.
pub fn to_value(report: &ReviewReport) -> Value {
    json!({"kind":"review","comparison":if report.comparison_notes.is_empty(){"matched"}else{"unverified"},"comparisonNotes":report.comparison_notes,"analysisPartial":report.analysis_partial,"totalChanges":report.total_changes,"reviewRequiredCount":report.review_required,"truncated":report.truncated,"visited":report.visited,"examinedEdges":report.examined_edges,"truncationReasons":report.truncation_reasons,"complete":report.complete,"limitations":report.limitations,"changes":report.findings.iter().map(|finding| {
        let mut value=json!({"id":finding.change.id.as_str(),"change":finding.change.kind,"impactSnapshot":finding.basis,"classification":if finding.change.requires_review(){"review-required"}else{"additive"},"impacted":crate::neighbors_value(&finding.impacted),"truncated":finding.impact_truncated,"visited":finding.impact_visited,"examinedEdges":finding.impact_examined_edges,"truncationReasons":finding.impact_truncation_reasons,"complete":finding.impact_complete});
        if let Some(before)=&finding.change.before {value["before"]=json!(before);}
        if let Some(after)=&finding.change.after {value["after"]=json!(after);}
        value
    }).collect::<Vec<_>>()})
}

/// 정책·기준선 평가를 기존 review JSON에 선택적으로 붙인다.
///
/// 기본 `to_value`의 바이트 계약을 보존하기 위해 정책을 사용한 호출자만 이
/// 함수를 선택한다. `policy` 값은 CLI가 검증·생성한 JSON이어야 한다.
pub fn to_value_with_policy(report: &ReviewReport, policy: Value) -> Value {
    let mut value = to_value(report);
    value["policy"] = policy;
    value
}

/// Markdown에 정책 gate의 전체 건수와 실패 여부를 덧붙인다.
pub fn to_markdown_with_policy(report: &ReviewReport, policy: &Value) -> String {
    let mut output = to_markdown(report);
    let failed = policy
        .get("failed")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let total = policy
        .get("totalChanges")
        .and_then(Value::as_u64)
        .unwrap_or(report.total_changes as u64);
    let suppressed = policy
        .get("suppressedFindings")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    output.push_str(&format!(
        "\nPolicy gate: {}; {} evaluated changes; {} suppressed.\n",
        if failed { "failed" } else { "passed" },
        total,
        suppressed
    ));
    output
}

fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('|', "&#124;")
        .replace('`', "&#96;")
        .replace('[', "&#91;")
        .replace(']', "&#93;")
        .replace('*', "&#42;")
        .replace(['\n', '\r'], " ")
}

/// 객체명을 링크나 HTML로 해석하지 않는 PR 요약을 만든다.
pub fn to_markdown(report: &ReviewReport) -> String {
    let mut output = format!(
        "# Schema change review\n\n{} changes; {} require review. Collection scope: {}.\n\n",
        report.total_changes,
        report.review_required,
        if report.comparison_notes.is_empty() {
            "matched"
        } else {
            "unverified"
        }
    );
    output.push_str(
        "| Object | Change | Classification | Reported dependents |\n| --- | --- | --- | ---: |\n",
    );
    for finding in &report.findings {
        output.push_str(&format!(
            "| {} | {} | {} | {}{} |\n",
            escape(finding.change.id.as_str()),
            escape(&finding.change.kind),
            if finding.change.requires_review() {
                "Review required"
            } else {
                "Additive"
            },
            finding.impacted.len(),
            if finding.impact_truncated { "+" } else { "" }
        ));
    }
    if report.truncated {
        output.push_str("\nResults are truncated. Use JSON output and higher limits to inspect remaining changes.\n");
    }
    if !report.complete {
        output.push_str("\nImpact traversal is incomplete; an empty or partial list does not establish absence.\n");
    }
    if report.analysis_partial {
        output.push_str("\nSQL analysis is partial or unavailable; an empty impact list does not establish safety.\n");
    }
    for note in report.comparison_notes.iter().chain(&report.limitations) {
        output.push_str(&format!("\n- {}\n", escape(note)));
    }
    output
}
