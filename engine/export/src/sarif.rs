//! `review` 결과를 SARIF 2.1.0으로 내보낸다.
//!
//! 위치를 알 수 없는 데이터베이스 객체에 SQL 파일 행 번호를 꾸며내지 않고
//! logical location만 사용한다. 정책 평가가 제공한 fingerprint·baseline·waiver
//! 메타데이터는 결과에만 부착하고, 그래프의 사실과 위험도 권고를 섞지 않는다.

use schemagraph_analysis::review::ReviewReport;
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;

/// CLI 정책 평가와 export 계층 사이의 작은 SARIF 계약.
#[derive(Debug, Clone)]
pub struct FindingAnnotation {
    pub id: String,
    pub kind: String,
    pub fingerprint: String,
    pub level: String,
    /// SARIF baselineState: `new` 또는 `unchanged`.
    pub baseline_state: String,
    pub before: Option<String>,
    pub after: Option<String>,
    pub suppression: Option<Suppression>,
}

#[derive(Debug, Clone)]
pub struct Suppression {
    /// waiver는 external, 저장된 기준선은 suppression을 만들지 않는다.
    pub kind: String,
    pub justification: String,
}

/// 정책 평가 결과를 SARIF 2.1.0의 결정적인 JSON 값으로 만든다.
pub fn to_value(report: &ReviewReport, annotations: &[FindingAnnotation]) -> Value {
    let mut annotations = annotations.to_vec();
    annotations.sort_by(|a, b| {
        a.fingerprint
            .cmp(&b.fingerprint)
            .then_with(|| a.id.cmp(&b.id))
            .then_with(|| a.kind.cmp(&b.kind))
    });

    let mut rules = BTreeMap::<String, Value>::new();
    let results = annotations
        .iter()
        .map(|finding| {
            rules.entry(finding.kind.clone()).or_insert_with(|| {
                json!({
                    "id": finding.kind,
                    "shortDescription": {"text": format!("Schema change: {}", finding.kind)},
                    "defaultConfiguration": {"level": finding.level},
                })
            });
            let mut result = Map::new();
            result.insert("ruleId".into(), json!(finding.kind));
            result.insert("level".into(), json!(finding.level));
            result.insert("message".into(), json!({"text": message(finding)}));
            result.insert("baselineState".into(), json!(finding.baseline_state));
            result.insert(
                "fingerprints".into(),
                json!({"schemagraph/v1": finding.fingerprint}),
            );
            result.insert(
                "locations".into(),
                json!([{"logicalLocations":[{"kind":"database-object","name":finding.id}]}]),
            );
            if let Some(suppression) = &finding.suppression {
                result.insert(
                    "suppressions".into(),
                    json!([{
                        "kind": suppression.kind,
                        "justification": suppression.justification,
                    }]),
                );
            }
            Value::Object(result)
        })
        .collect::<Vec<_>>();
    let rules = rules.into_values().collect::<Vec<_>>();
    json!({
        "$schema": "https://json.schemastore.org/sarif-2.1.0.json",
        "version": "2.1.0",
        "runs": [{
            "tool": {
                "driver": {
                    "name": "schemagraph",
                    "informationUri": "https://github.com/ictechgy/schemagraph",
                    "rules": rules,
                }
            },
            "results": results,
            "properties": {
                "comparison": if report.comparison_notes.is_empty() {"matched"} else {"unverified"},
                "analysisPartial": report.analysis_partial,
                "totalChanges": report.total_changes,
                "reviewRequiredCount": report.review_required,
                "truncated": report.truncated,
                "complete": report.complete,
                "limitations": report.limitations,
                "comparisonNotes": report.comparison_notes,
            }
        }]
    })
}

/// SARIF properties에 정책의 전체 gate 통계를 덧붙인다.
///
/// `policy`는 출력 상한이 적용된 finding 목록을 가질 수 있지만
/// `failedFindings`·`totalChanges` 같은 집계는 전체 변경을 가리킨다.
pub fn to_value_with_policy(
    report: &ReviewReport,
    annotations: &[FindingAnnotation],
    policy: &Value,
) -> Value {
    let mut value = to_value(report, annotations);
    value["runs"][0]["properties"]["policy"] = policy.clone();
    value
}

fn message(finding: &FindingAnnotation) -> String {
    let mut message = format!("{} ({})", finding.id, finding.kind);
    if let Some(before) = &finding.before {
        message.push_str(&format!("; before={before}"));
    }
    if let Some(after) = &finding.after {
        message.push_str(&format!("; after={after}"));
    }
    message
}

#[cfg(test)]
mod tests {
    use super::*;
    use schemagraph_analysis::review::ReviewReport;

    fn report() -> ReviewReport {
        ReviewReport {
            findings: vec![],
            total_changes: 1,
            review_required: 1,
            comparison_notes: vec![],
            analysis_partial: false,
            truncated: false,
            visited: 0,
            examined_edges: 0,
            truncation_reasons: vec![],
            complete: true,
            limitations: vec![],
        }
    }

    #[test]
    fn sarif_has_deterministic_rules_and_logical_locations() {
        let finding = FindingAnnotation {
            id: "app.users".into(),
            kind: "object-removed".into(),
            fingerprint: "a".repeat(64),
            level: "error".into(),
            baseline_state: "new".into(),
            before: Some("table".into()),
            after: None,
            suppression: None,
        };
        let value = to_value(&report(), &[finding]);
        assert_eq!(value["version"], "2.1.0");
        assert_eq!(value["runs"][0]["results"][0]["baselineState"], "new");
        assert_eq!(
            value["runs"][0]["results"][0]["locations"][0]["logicalLocations"][0]["kind"],
            "database-object"
        );
        assert!(value["runs"][0]["results"][0]
            .get("physicalLocation")
            .is_none());
    }

    #[test]
    fn policy_summary_keeps_hidden_failure_counts_in_properties() {
        let value = to_value_with_policy(
            &report(),
            &[],
            &json!({"totalChanges": 4, "failedFindings": 2, "truncated": true}),
        );
        assert_eq!(value["runs"][0]["results"].as_array().unwrap().len(), 0);
        assert_eq!(
            value["runs"][0]["properties"]["policy"]["failedFindings"],
            2
        );
        assert_eq!(value["runs"][0]["properties"]["truncated"], false);
    }
}
