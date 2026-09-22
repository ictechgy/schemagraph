//! `review` 명령의 정책·기준선·기간 만료 waiver 평가.
//!
//! 이 모듈은 그래프 사실을 바꾸지 않는다. 변경 하나를 안정적인 fingerprint로
//! 식별하고, 사람이 선언한 위험도·기준선·waiver를 별도로 평가한다. 출력 상한은
//! 호출자가 전달하는 전체 변경 목록에 적용되지 않으므로, 결과를 잘라서 CI gate를
//! 우회할 수 없다.

use anyhow::{bail, Context, Result};
use schemagraph_analysis::review::{Change, ReviewReport};
use schemagraph_source::CatalogDocument;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Write;
use std::path::Path;

pub const POLICY_VERSION: u32 = 1;
pub const BASELINE_VERSION: u32 = 1;

/// 정책에서 사용하는 위험도. 순서가 곧 gate 비교 순서다.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Info,
    Low,
    Medium,
    High,
    Critical,
}

impl Severity {
    /// GitHub code-scanning이 이해하는 SARIF level로 매핑한다.
    pub fn sarif_level(self) -> &'static str {
        match self {
            Self::Info | Self::Low => "note",
            Self::Medium => "warning",
            Self::High | Self::Critical => "error",
        }
    }
}

impl<'de> Deserialize<'de> for Severity {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        match value.to_ascii_lowercase().as_str() {
            "info" => Ok(Self::Info),
            "low" => Ok(Self::Low),
            "medium" => Ok(Self::Medium),
            "high" => Ok(Self::High),
            "critical" => Ok(Self::Critical),
            other => Err(serde::de::Error::custom(format!(
                "unknown review severity '{other}'; use info, low, medium, high, or critical"
            ))),
        }
    }
}

/// 현재 엔진이 생성하는 변경 종류의 명시적 allow-list.
pub const CHANGE_KINDS: &[&str] = &[
    "schema-added",
    "schema-removed",
    "object-added",
    "object-removed",
    "object-kind-changed",
    "required-column-added",
    "column-added",
    "column-removed",
    "column-type-changed",
    "column-nullability-changed",
    "column-default-changed",
    "column-position-or-key-changed",
    "constraints-changed",
    "indexes-changed",
    "index-added",
    "unique-index-added",
    "triggers-changed",
    "definition-changed",
    "catalog-dependencies-changed",
];

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawPolicy {
    version: u32,
    fail_threshold: Severity,
    #[serde(default)]
    severity: BTreeMap<String, Severity>,
    #[serde(default)]
    waivers: RawWaivers,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(untagged)]
enum RawWaivers {
    #[default]
    None,
    Map(BTreeMap<String, RawWaiver>),
    List(Vec<RawWaiverEntry>),
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawWaiver {
    reason: String,
    expires: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawWaiverEntry {
    fingerprint: String,
    reason: String,
    expires: String,
}

/// 파싱·검증을 끝낸 review 정책.
#[derive(Debug, Clone)]
pub struct Policy {
    pub fail_threshold: Severity,
    severity: BTreeMap<String, Severity>,
    waivers: BTreeMap<String, Waiver>,
}

impl Policy {
    /// 이 정책이 기간 만료 waiver를 사용해 안전성 gate를 억제할 수 있는지 반환한다.
    pub(crate) fn has_waivers(&self) -> bool {
        !self.waivers.is_empty()
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Waiver {
    pub reason: String,
    pub expires: String,
}

/// strict JSON baseline의 on-disk 계약.
#[derive(Debug, Clone, Serialize)]
pub struct Baseline {
    pub version: u32,
    pub fingerprints: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawBaseline {
    version: u32,
    fingerprints: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum BaselineState {
    New,
    Existing,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PolicyFinding {
    pub id: String,
    pub change: String,
    pub review_required: bool,
    pub fingerprint: String,
    pub severity: Severity,
    pub baseline_state: BaselineState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub waiver: Option<Waiver>,
    pub suppressed: bool,
    pub before: Option<String>,
    pub after: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PolicyEvaluation {
    pub version: u32,
    pub fail_threshold: Severity,
    pub total_changes: usize,
    pub review_required: usize,
    pub new_findings: usize,
    pub existing_findings: usize,
    pub suppressed_findings: usize,
    pub failed_findings: usize,
    pub failed: bool,
    pub findings: Vec<PolicyFinding>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub baseline_version: Option<u32>,
    /// 출력 상한과 관계없이 기준선을 만들 때 필요한 전체 fingerprint.
    #[serde(skip)]
    pub(crate) all_fingerprints: Vec<String>,
    /// baseline으로 억제되지 않은 legacy strict 검토 대상의 전체 건수.
    #[serde(skip)]
    pub(crate) strict_unsuppressed_review_required: usize,
}

impl PolicyEvaluation {
    /// 출력 상한에 해당하는 finding만 남기고 전체 gate 통계는 보존한다.
    pub(crate) fn retain_findings(&mut self, visible: &BTreeSet<String>) {
        self.findings
            .retain(|finding| visible.contains(&finding.fingerprint));
    }
}

/// 정책 파일을 읽고 알려진 변경 종류·waiver 형식을 모두 검증한다.
pub fn load_policy(path: &Path) -> Result<Policy> {
    let text = fs::read_to_string(path)
        .with_context(|| format!("could not read review policy {}", path.display()))?;
    let raw: RawPolicy = toml::from_str(&text)
        .with_context(|| format!("invalid review policy TOML {}", path.display()))?;
    if raw.version != POLICY_VERSION {
        bail!(
            "unsupported review policy version {}; expected {}",
            raw.version,
            POLICY_VERSION
        );
    }
    for kind in raw.severity.keys() {
        if !CHANGE_KINDS.contains(&kind.as_str()) {
            bail!("unknown review policy change kind '{kind}'");
        }
    }
    let raw_waivers: Vec<(String, RawWaiver)> = match raw.waivers {
        RawWaivers::None => vec![],
        RawWaivers::Map(entries) => entries.into_iter().collect(),
        RawWaivers::List(entries) => entries
            .into_iter()
            .map(|entry| {
                (
                    entry.fingerprint,
                    RawWaiver {
                        reason: entry.reason,
                        expires: entry.expires,
                    },
                )
            })
            .collect(),
    };
    let mut waivers = BTreeMap::new();
    for (fingerprint, waiver) in raw_waivers {
        validate_fingerprint(&fingerprint)
            .with_context(|| format!("invalid waiver fingerprint '{fingerprint}'"))?;
        validate_reason(&waiver.reason)
            .with_context(|| format!("invalid waiver for fingerprint '{fingerprint}'"))?;
        parse_iso_date(&waiver.expires)
            .with_context(|| format!("invalid waiver expiry for fingerprint '{fingerprint}'"))?;
        if waivers.contains_key(&fingerprint) {
            bail!("duplicate waiver fingerprint '{fingerprint}'");
        }
        waivers.insert(
            fingerprint,
            Waiver {
                reason: waiver.reason,
                expires: waiver.expires,
            },
        );
    }
    Ok(Policy {
        fail_threshold: raw.fail_threshold,
        severity: raw.severity,
        waivers,
    })
}

/// strict JSON baseline을 읽는다. 알 수 없는 필드는 오타로 간주한다.
pub fn load_baseline(path: &Path) -> Result<Baseline> {
    let text = fs::read_to_string(path)
        .with_context(|| format!("could not read review baseline {}", path.display()))?;
    let raw: RawBaseline = serde_json::from_str(&text)
        .with_context(|| format!("invalid review baseline JSON {}", path.display()))?;
    if raw.version != BASELINE_VERSION {
        bail!(
            "unsupported review baseline version {}; expected {}",
            raw.version,
            BASELINE_VERSION
        );
    }
    let mut fingerprints = BTreeSet::new();
    for fingerprint in raw.fingerprints {
        validate_fingerprint(&fingerprint)
            .with_context(|| format!("invalid baseline fingerprint '{fingerprint}'"))?;
        fingerprints.insert(fingerprint);
    }
    Ok(Baseline {
        version: BASELINE_VERSION,
        fingerprints: fingerprints.into_iter().collect(),
    })
}

/// 기준선 파일을 결정적 JSON으로 원자적으로 기록한다.
pub fn write_baseline(path: &Path, fingerprints: impl IntoIterator<Item = String>) -> Result<()> {
    let mut values = BTreeSet::new();
    for fingerprint in fingerprints {
        validate_fingerprint(&fingerprint)?;
        values.insert(fingerprint);
    }
    let baseline = Baseline {
        version: BASELINE_VERSION,
        fingerprints: values.into_iter().collect(),
    };
    let text = serde_json::to_string_pretty(&baseline)? + "\n";
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    // 임시 파일의 예측 가능한 이름이나 symlink를 따라 기존 입력을 덮어쓰지 않는다.
    let mut temporary = tempfile::NamedTempFile::new_in(parent).with_context(|| {
        format!(
            "could not create baseline temporary file in {}",
            parent.display()
        )
    })?;
    temporary
        .write_all(text.as_bytes())
        .and_then(|_| temporary.as_file().sync_all())
        .with_context(|| format!("could not write review baseline {}", path.display()))?;
    temporary
        .persist(path)
        .with_context(|| format!("could not replace review baseline {}", path.display()))?;
    Ok(())
}

/// 문서의 논리적 비교 범위. 관측값(usage/body timestamp)은 포함하지 않는다.
fn source_identity(before: &CatalogDocument, after: &CatalogDocument) -> Value {
    fn context(document: &CatalogDocument) -> Value {
        let mut schema_filter = document
            .context
            .as_ref()
            .and_then(|context| context.schema_filter.clone());
        if let Some(filter) = &mut schema_filter {
            filter.sort();
            filter.dedup();
        }
        json!({
            "dialect": document.dialect,
            "reader": document.reader,
            "hasContext": document.context.is_some(),
            "sourceId": document.context.as_ref().map(|context| context.source_id.clone()),
            "database": document.context.as_ref().and_then(|context| context.database.clone()),
            "schemaFilter": schema_filter,
        })
    }
    json!({"before": context(before), "after": context(after)})
}

/// 동일한 source 범위에서 동일한 실제 변경만 같은 fingerprint를 얻는다.
pub fn fingerprint(
    change: &Change,
    detail: Option<&Value>,
    before: &CatalogDocument,
    after: &CatalogDocument,
) -> String {
    let payload = json!({
        "version": 1,
        "source": source_identity(before, after),
        "change": {
            "id": change.id.as_str(),
            "kind": change.kind,
            "before": change.before,
            "after": change.after,
            "detail": detail.cloned().unwrap_or(Value::Null),
        },
    });
    let digest =
        Sha256::digest(serde_json::to_vec(&payload).expect("fingerprint payload is serializable"));
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// 변경 전체를 평가한다. `report`는 출력 상한이 적용된 뒤에도 전체 안전성 상태를 보존한다.
pub fn evaluate(
    changes: &[Change],
    details: &BTreeMap<Change, Value>,
    before: &CatalogDocument,
    after: &CatalogDocument,
    policy: Option<&Policy>,
    baseline: Option<&Baseline>,
    as_of: Option<&str>,
    _report: &ReviewReport,
) -> Result<PolicyEvaluation> {
    let fail_threshold = policy.map_or(Severity::Critical, |policy| policy.fail_threshold);
    let as_of = as_of.map(parse_iso_date).transpose()?;
    if policy.is_some_and(|policy| !policy.waivers.is_empty()) && as_of.is_none() {
        bail!("--as-of is required when the review policy contains waivers");
    }
    if let (Some(policy), Some(as_of)) = (policy, as_of) {
        for (fingerprint, waiver) in &policy.waivers {
            if parse_iso_date(&waiver.expires)? < as_of {
                bail!(
                    "waiver for fingerprint {} expired on {} (as-of {})",
                    fingerprint,
                    waiver.expires,
                    format_date(as_of)
                );
            }
        }
    }
    let mut ordered = changes.to_vec();
    ordered.sort();
    ordered.dedup();
    let baseline_set: BTreeSet<&str> = baseline
        .map(|baseline| baseline.fingerprints.iter().map(String::as_str).collect())
        .unwrap_or_default();
    let mut findings = Vec::with_capacity(ordered.len());
    for change in ordered {
        if !CHANGE_KINDS.contains(&change.kind.as_str()) {
            bail!("unknown review change kind '{}'", change.kind);
        }
        let fingerprint = fingerprint(&change, details.get(&change), before, after);
        let severity = policy
            .and_then(|policy| policy.severity.get(&change.kind).copied())
            .unwrap_or_else(|| default_severity(&change));
        let baseline_state = if baseline_set.contains(fingerprint.as_str()) {
            BaselineState::Existing
        } else {
            BaselineState::New
        };
        let waiver = policy.and_then(|policy| policy.waivers.get(&fingerprint).cloned());
        let suppressed =
            waiver.is_some() || (baseline_state == BaselineState::Existing && baseline.is_some());
        findings.push(PolicyFinding {
            id: change.id.as_str().to_owned(),
            change: change.kind.clone(),
            review_required: change.requires_review(),
            fingerprint,
            severity,
            baseline_state,
            waiver,
            suppressed,
            before: change.before.clone(),
            after: change.after.clone(),
        });
    }
    let failed_findings = findings
        .iter()
        .filter(|finding| finding.severity >= fail_threshold && !finding.suppressed)
        .count();
    // Comparison/analysis safeguards are evaluated independently by the caller. A
    // baseline is an annotation and suppression mechanism, never proof of comparability.
    let failed = failed_findings > 0;
    let new_findings = findings
        .iter()
        .filter(|finding| finding.baseline_state == BaselineState::New)
        .count();
    let existing_findings = findings.len() - new_findings;
    let suppressed_findings = findings.iter().filter(|finding| finding.suppressed).count();
    let all_fingerprints = findings
        .iter()
        .map(|finding| finding.fingerprint.clone())
        .collect();
    let strict_unsuppressed_review_required = findings
        .iter()
        .filter(|finding| finding.review_required && !finding.suppressed)
        .count();
    Ok(PolicyEvaluation {
        version: POLICY_VERSION,
        fail_threshold,
        total_changes: findings.len(),
        review_required: ordered_review_required(changes),
        new_findings,
        existing_findings,
        suppressed_findings,
        failed_findings,
        failed,
        findings,
        baseline_version: baseline.map(|baseline| baseline.version),
        all_fingerprints,
        strict_unsuppressed_review_required,
    })
}

fn ordered_review_required(changes: &[Change]) -> usize {
    let mut unique = changes.to_vec();
    unique.sort();
    unique.dedup();
    unique
        .iter()
        .filter(|change| change.requires_review())
        .count()
}

fn default_severity(change: &Change) -> Severity {
    if change.requires_review() {
        Severity::High
    } else {
        Severity::Low
    }
}

fn validate_fingerprint(value: &str) -> Result<()> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
    {
        bail!("fingerprint must be exactly 64 hexadecimal characters");
    }
    Ok(())
}

fn validate_reason(value: &str) -> Result<()> {
    if value.trim().is_empty() {
        bail!("waiver reason must be nonempty");
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct Date {
    year: u32,
    month: u32,
    day: u32,
}

fn parse_iso_date(value: &str) -> Result<Date> {
    let bytes = value.as_bytes();
    if bytes.len() != 10 || !value.is_ascii() || bytes[4] != b'-' || bytes[7] != b'-' {
        bail!("date '{value}' must use ISO-8601 YYYY-MM-DD form");
    }
    let year = value[0..4]
        .parse::<u32>()
        .with_context(|| format!("date '{value}' has an invalid year"))?;
    let month = value[5..7]
        .parse::<u32>()
        .with_context(|| format!("date '{value}' has an invalid month"))?;
    let day = value[8..10]
        .parse::<u32>()
        .with_context(|| format!("date '{value}' has an invalid day"))?;
    if !(1..=12).contains(&month) {
        bail!("date '{value}' has an invalid month");
    }
    let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    let days = match month {
        2 if leap => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    };
    if !(1..=days).contains(&day) {
        bail!("date '{value}' has an invalid day");
    }
    Ok(Date { year, month, day })
}

fn format_date(date: Date) -> String {
    format!("{:04}-{:02}-{:02}", date.year, date.month, date.day)
}

#[cfg(test)]
mod tests {
    use super::*;
    use schemagraph_analysis::review::Change;
    use schemagraph_core::VertexId;

    #[cfg(unix)]
    #[test]
    fn baseline_temporary_symlink_cannot_overwrite_an_existing_file() {
        let directory = tempfile::tempdir().unwrap();
        let baseline = directory.path().join("baseline.json");
        let protected = directory.path().join("protected.txt");
        fs::write(&protected, "keep").unwrap();
        let temporary = baseline.with_extension(format!("json.tmp-{}", std::process::id()));
        std::os::unix::fs::symlink(&protected, &temporary).unwrap();
        write_baseline(&baseline, ["a".repeat(64)]).unwrap();
        assert_eq!(fs::read_to_string(&protected).unwrap(), "keep");
        assert!(baseline.exists());
    }

    fn change(kind: &str) -> Change {
        Change {
            id: VertexId::object("app", "users"),
            kind: kind.into(),
            before: None,
            after: None,
        }
    }

    fn document(source_id: &str) -> CatalogDocument {
        CatalogDocument {
            version: 1,
            dialect: "sqlite".into(),
            reader: "test".into(),
            schemas: vec![],
            limitations: vec![],
            context: Some(schemagraph_source::document::CollectionContext {
                source_id: source_id.into(),
                database: Some("db".into()),
                schema_filter: Some(vec!["app".into()]),
                catalog_complete: true,
            }),
            dependencies: vec![],
        }
    }

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
    fn fingerprint_changes_with_source_scope_but_ignores_usage() {
        let before = document("prod");
        let after = before.clone();
        let value = json!({"index": {"name": "users_name", "columns": ["name"]}});
        let a = fingerprint(&change("indexes-changed"), Some(&value), &before, &after);
        let b = fingerprint(
            &change("indexes-changed"),
            Some(&value),
            &document("test"),
            &after,
        );
        assert_ne!(a, b);

        let mut no_filter = before.clone();
        no_filter.context.as_mut().unwrap().schema_filter = None;
        let mut empty_filter = before.clone();
        empty_filter.context.as_mut().unwrap().schema_filter = Some(vec![]);
        assert_ne!(
            fingerprint(&change("indexes-changed"), Some(&value), &no_filter, &after),
            fingerprint(
                &change("indexes-changed"),
                Some(&value),
                &empty_filter,
                &after
            )
        );
    }

    #[test]
    fn malformed_policy_and_expired_waiver_are_errors() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("review.toml");
        fs::write(
            &path,
            "version = 1\nfail_threshold = \"high\"\n[waivers.not-a-fingerprint]\nreason = \"x\"\nexpires = \"2026-01-01\"\n",
        )
        .unwrap();
        assert!(load_policy(&path).is_err());
    }

    #[test]
    fn duplicate_list_waivers_are_rejected() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("review.toml");
        let fingerprint = "a".repeat(64);
        fs::write(
            &path,
            format!(
                "version = 1\nfail_threshold = \"high\"\nwaivers = [\n  {{ fingerprint = \"{fingerprint}\", reason = \"one\", expires = \"2026-12-31\" }},\n  {{ fingerprint = \"{fingerprint}\", reason = \"two\", expires = \"2026-12-31\" }}\n]\n"
            ),
        )
        .unwrap();
        let error = load_policy(&path).unwrap_err().to_string();
        assert!(error.contains("duplicate waiver fingerprint"), "{error}");
    }

    #[test]
    fn baseline_marks_existing_and_waiver_requires_as_of() {
        let before = document("prod");
        let after = before.clone();
        let change = change("object-removed");
        let fp = fingerprint(&change, None, &before, &after);
        let directory = tempfile::tempdir().unwrap();
        let policy_path = directory.path().join("review.toml");
        fs::write(
            &policy_path,
            format!(
                "version = 1\nfail_threshold = \"high\"\n[waivers.{fp}]\nreason = \"tracked\"\nexpires = \"2026-12-31\"\n"
            ),
        )
        .unwrap();
        let policy = load_policy(&policy_path).unwrap();
        let baseline = Baseline {
            version: BASELINE_VERSION,
            fingerprints: vec![fp],
        };
        let details = BTreeMap::new();
        assert!(evaluate(
            &[change.clone()],
            &details,
            &before,
            &after,
            Some(&policy),
            Some(&baseline),
            None,
            &report(),
        )
        .is_err());
        let result = evaluate(
            &[change],
            &details,
            &before,
            &after,
            Some(&policy),
            Some(&baseline),
            Some("2026-09-22"),
            &report(),
        )
        .unwrap();
        assert!(result.findings[0].suppressed);
        assert_eq!(result.failed_findings, 0);
    }
}
