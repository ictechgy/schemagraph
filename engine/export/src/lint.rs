//! schema lint 결과를 결정적인 에이전트용 JSON으로 직렬화한다.

use schemagraph_analysis::schema_lint::{LintFinding, LintReport, LintStatus};
use serde_json::{json, Value};

fn status_value(status: LintStatus) -> &'static str {
    status.as_str()
}

fn finding_value(finding: &LintFinding) -> Value {
    let mut value = json!({
        "message": finding.message,
        "rule": finding.rule,
        "status": status_value(finding.status),
        "subject": finding.subject.as_str(),
    });
    // 참고용 규칙만 표시한다 — 없는 선택 필드는 키를 생략하는 계약이다.
    if finding.advisory {
        value["advisory"] = json!(true);
    }
    if let Some(table) = &finding.table {
        value["table"] = json!(table.as_str());
    }
    if !finding.columns.is_empty() {
        value["columns"] = json!(finding
            .columns
            .iter()
            .map(|column| column.as_str())
            .collect::<Vec<_>>());
    }
    if let Some(code) = &finding.code {
        value["code"] = json!(code);
    }
    if let Some(location) = &finding.location {
        value["location"] = json!({
            "column": location.column,
            "endColumn": location.end_column,
            "endLine": location.end_line,
            "line": location.line,
        });
    }
    value
}

/// schema lint 보고서를 안정적인 JSON 값으로 변환한다.
pub fn to_value(report: &LintReport) -> Value {
    json!({
        "kind": "lint",
        "blockingCount": report.blocking_count,
        "complete": report.complete,
        "confirmedCount": report.confirmed_count,
        "findings": report.findings.iter().map(finding_value).collect::<Vec<_>>(),
        "limitations": report.limitations,
        "totalFindings": report.total_findings,
        "truncated": report.truncated,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use schemagraph_core::VertexId;

    fn finding(rule: &str, subject: &str, status: LintStatus) -> LintFinding {
        LintFinding {
            rule: rule.into(),
            status,
            advisory: false,
            subject: VertexId::from_raw(subject),
            table: None,
            columns: vec![],
            code: None,
            message: "fact".into(),
            location: None,
        }
    }

    #[test]
    fn lint_json_has_counts_and_status_without_drop_advice() {
        let report = LintReport {
            findings: vec![finding(
                "fk-index-prefix",
                "app.orders.fk",
                LintStatus::Confirmed,
            )],
            total_findings: 2,
            confirmed_count: 1,
            blocking_count: 1,
            truncated: true,
            limitations: vec!["metadata incomplete".into()],
            complete: false,
        };
        let value = to_value(&report);
        assert_eq!(value["kind"], "lint");
        assert_eq!(value["totalFindings"], 2);
        assert_eq!(value["confirmedCount"], 1);
        assert_eq!(value["truncated"], true);
        assert!(value.to_string().find("drop").is_none());
    }

    #[test]
    fn lint_json_omits_empty_optional_fields_and_keeps_location_shape() {
        let mut item = finding("ambiguous-reference", "app.view", LintStatus::Unverified);
        item.code = Some("SG_COLUMN_AMBIGUOUS".into());
        item.location = Some(schemagraph_core::SourceLocation {
            line: 2,
            column: 3,
            end_line: 2,
            end_column: 5,
        });
        let report = LintReport {
            findings: vec![item],
            total_findings: 1,
            confirmed_count: 0,
            blocking_count: 0,
            truncated: false,
            limitations: vec![],
            complete: true,
        };
        let value = to_value(&report);
        let item = &value["findings"][0];
        assert_eq!(item["code"], "SG_COLUMN_AMBIGUOUS");
        assert_eq!(item["location"]["endColumn"], 5);
        assert!(item.get("columns").is_none());
        assert!(item.get("table").is_none());
    }
}
