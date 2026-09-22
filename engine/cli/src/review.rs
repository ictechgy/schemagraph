//! 카탈로그 차이를 그래프의 실제 id로 해석해 순수 검토 분석으로 전달한다.

use anyhow::{bail, Context, Result};
use schemagraph_analysis::review::Change;
use schemagraph_core::{EdgeKind, Graph, VertexId, VertexKind};
use schemagraph_source::{self as source, diff::DocumentDiff};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::atomic::Ordering;

#[path = "review_policy.rs"]
pub(crate) mod review_policy;

fn root_id(graph: &Graph, id: &str, kind: Option<&str>) -> Option<VertexId> {
    let expected = match kind {
        Some("function") => Some(VertexKind::Function),
        Some("procedure") => Some(VertexKind::Procedure),
        Some("package") => Some(VertexKind::Package),
        _ => None,
    };
    let base = VertexId::from_raw(id);
    if graph
        .vertex(&base)
        .is_some_and(|v| expected.is_none_or(|kind| v.kind == kind))
    {
        return Some(base);
    }
    let suffix = kind?;
    let renamed = VertexId::from_raw(&format!("{id}@{suffix}"));
    graph
        .vertex(&renamed)
        .filter(|v| expected.is_none_or(|kind| v.kind == kind))
        .map(|v| v.id.clone())
}

fn column_id(graph: &Graph, id: &str, name: &str) -> Option<VertexId> {
    let parent = root_id(graph, id, None)?;
    let matches: Vec<_> = graph
        .outgoing(&parent)
        .iter()
        .filter(|e| e.kind == EdgeKind::Contains)
        .filter_map(|e| graph.vertex(&e.to))
        .filter(|v| v.kind == VertexKind::Column && v.name == name)
        .collect();
    (matches.len() == 1).then(|| matches[0].id.clone())
}

struct Changes<'a> {
    before: &'a Graph,
    after: &'a Graph,
    items: Vec<Change>,
    /// `Change`의 before/after가 비어도 구조 변경의 실제 대상을 fingerprint에
    /// 포함하기 위한 diff subset이다. usage·captured_at은 이 값에 넣지 않는다.
    details: BTreeMap<Change, Value>,
    notes: Vec<String>,
}

impl Changes<'_> {
    fn add_detailed(
        &mut self,
        id: &str,
        member: Option<&str>,
        object_kind: Option<&str>,
        kind: &str,
        before: Option<String>,
        after: Option<String>,
        detail: Option<Value>,
    ) {
        let resolve = |graph| match member {
            Some(name) => column_id(graph, id, name),
            None => root_id(graph, id, object_kind),
        };
        let target = resolve(self.before).or_else(|| resolve(self.after));
        match target {
            Some(id) => {
                let change = Change {
                    id,
                    kind: kind.into(),
                    before,
                    after,
                };
                if let Some(detail) = detail {
                    self.details.insert(change.clone(), detail);
                }
                self.items.push(change);
            }
            None => self.notes.push(format!(
                "changed object '{id}' could not be resolved to a graph vertex"
            )),
        }
    }

    fn collect(
        &mut self,
        diff: &DocumentDiff,
        old: &source::CatalogDocument,
        new: &source::CatalogDocument,
    ) -> Result<()> {
        for (schemas, kind) in [
            (&diff.schemas.added, "schema-added"),
            (&diff.schemas.removed, "schema-removed"),
        ] {
            for schema in schemas {
                let id = VertexId::schema(schema);
                self.add_detailed(
                    id.as_str(),
                    None,
                    None,
                    kind,
                    None,
                    None,
                    Some(json!({"schema":schema})),
                );
            }
        }
        for item in diff.objects.added.iter().chain(&diff.routines.added) {
            self.add_detailed(
                &item.id,
                None,
                Some(&item.kind),
                "object-added",
                None,
                Some(item.kind.clone()),
                Some(structure_detail(new, self.after, &item.id)),
            );
        }
        for item in diff.objects.removed.iter().chain(&diff.routines.removed) {
            self.add_detailed(
                &item.id,
                None,
                Some(&item.kind),
                "object-removed",
                Some(item.kind.clone()),
                None,
                Some(structure_detail(old, self.before, &item.id)),
            );
        }
        for object in &diff.objects.changed {
            if let Some(kind) = &object.kind {
                self.add_detailed(
                    &object.id,
                    None,
                    None,
                    "object-kind-changed",
                    Some(kind.old.clone()),
                    Some(kind.new.clone()),
                    Some(serde_json::to_value(kind)?),
                );
            }
            for col in &object.columns.added {
                let kind = if !col.nullable && col.default.is_none() {
                    "required-column-added"
                } else {
                    "column-added"
                };
                self.add_detailed(
                    &object.id,
                    Some(&col.name),
                    None,
                    kind,
                    None,
                    Some(col.data_type.clone()),
                    Some(serde_json::to_value(col)?),
                );
            }
            for col in &object.columns.removed {
                self.add_detailed(
                    &object.id,
                    Some(&col.name),
                    None,
                    "column-removed",
                    Some(col.data_type.clone()),
                    None,
                    Some(serde_json::to_value(col)?),
                );
            }
            for col in &object.columns.changed {
                if col.old.data_type != col.new.data_type {
                    self.add_detailed(
                        &object.id,
                        Some(&col.name),
                        None,
                        "column-type-changed",
                        Some(col.old.data_type.clone()),
                        Some(col.new.data_type.clone()),
                        Some(json!({"old": &col.old, "new": &col.new})),
                    );
                }
                if col.old.nullable != col.new.nullable {
                    self.add_detailed(
                        &object.id,
                        Some(&col.name),
                        None,
                        "column-nullability-changed",
                        Some(col.old.nullable.to_string()),
                        Some(col.new.nullable.to_string()),
                        Some(json!({"old": &col.old, "new": &col.new})),
                    );
                }
                if col.old.default != col.new.default {
                    self.add_detailed(
                        &object.id,
                        Some(&col.name),
                        None,
                        "column-default-changed",
                        col.old.default.clone(),
                        col.new.default.clone(),
                        Some(json!({"old": &col.old, "new": &col.new})),
                    );
                }
                if col.old.pk_position != col.new.pk_position || col.old.ordinal != col.new.ordinal
                {
                    self.add_detailed(
                        &object.id,
                        Some(&col.name),
                        None,
                        "column-position-or-key-changed",
                        None,
                        None,
                        Some(json!({"old": &col.old, "new": &col.new})),
                    );
                }
            }
            if !object.constraints.added.is_empty()
                || !object.constraints.removed.is_empty()
                || !object.constraints.changed.is_empty()
            {
                self.add_detailed(
                    &object.id,
                    None,
                    None,
                    "constraints-changed",
                    None,
                    None,
                    Some(json!({
                        "added": &object.constraints.added,
                        "removed": &object.constraints.removed,
                        "changed": &object.constraints.changed,
                    })),
                );
            }
            if !object.indexes.removed.is_empty() || !object.indexes.changed.is_empty() {
                self.add_detailed(
                    &object.id,
                    None,
                    None,
                    "indexes-changed",
                    None,
                    None,
                    Some(indexes_detail(&object.indexes)),
                );
            } else if !object.indexes.added.is_empty() {
                let kind = if object.indexes.added.iter().any(|index| index.unique) {
                    "unique-index-added"
                } else {
                    "index-added"
                };
                self.add_detailed(
                    &object.id,
                    None,
                    None,
                    kind,
                    None,
                    None,
                    Some(indexes_detail(&object.indexes)),
                );
            }
            if !object.triggers.added.is_empty()
                || !object.triggers.removed.is_empty()
                || !object.triggers.changed.is_empty()
            {
                self.add_detailed(
                    &object.id,
                    None,
                    None,
                    "triggers-changed",
                    None,
                    None,
                    Some(json!({
                        "added": &object.triggers.added,
                        "removed": &object.triggers.removed,
                        "changed": &object.triggers.changed,
                    })),
                );
            }
            if object.body_changed {
                self.definition(&object.id, None);
            }
        }
        for routine in &diff.routines.changed {
            if routine.body_changed || routine.language.is_some() || routine.kind.is_some() {
                self.definition_with_detail(
                    &routine.id,
                    routine.kind.as_ref().map(|k| k.old.as_str()),
                    Some(json!({
                        "kind": &routine.kind,
                        "language": &routine.language,
                        "bodyChanged": routine.body_changed,
                    })),
                );
            }
        }
        let mut dependency_details: BTreeMap<VertexId, (Vec<Value>, Vec<Value>)> = BTreeMap::new();
        for (added, dependencies) in [
            (true, &diff.dependencies.added),
            (false, &diff.dependencies.removed),
        ] {
            for dependency in dependencies {
                let id =
                    source::dependencies::resolve_reference(self.before, old, &dependency.source)
                        .or_else(|| {
                            source::dependencies::resolve_reference(
                                self.after,
                                new,
                                &dependency.source,
                            )
                        });
                if let Some(id) = id {
                    let values = if added {
                        &mut dependency_details.entry(id).or_default().0
                    } else {
                        &mut dependency_details.entry(id).or_default().1
                    };
                    values.push(serde_json::to_value(dependency)?);
                } else {
                    self.notes.push(format!("changed catalog dependency source '{}.{}' could not be resolved to a graph vertex",dependency.source.schema,dependency.source.name));
                }
            }
        }
        for (id, (mut added, mut removed)) in dependency_details {
            sort_json_values(&mut added);
            sort_json_values(&mut removed);
            let change = Change {
                id,
                kind: "catalog-dependencies-changed".into(),
                before: None,
                after: None,
            };
            self.details
                .insert(change.clone(), json!({"added": added, "removed": removed}));
            self.items.push(change);
        }
        Ok(())
    }

    fn definition(&mut self, id: &str, kind: Option<&str>) {
        self.definition_with_detail(id, kind, None);
    }

    fn definition_with_detail(&mut self, id: &str, kind: Option<&str>, detail: Option<Value>) {
        let hash = |graph: &Graph| {
            root_id(graph, id, kind)
                .and_then(|id| graph.analysis().get(&id))
                .and_then(|a| a.body_hash.clone())
        };
        self.add_detailed(
            id,
            None,
            kind,
            "definition-changed",
            hash(self.before),
            hash(self.after),
            detail,
        );
    }
}

fn index_detail(index: &source::document::IndexDoc) -> Value {
    json!({
        "name": index.name,
        "unique": index.unique,
        "columns": index.columns,
        "definitionComplete": index.definition_complete,
        "predicate": index.predicate,
        "hasPredicate": index.has_predicate,
    })
}

fn text_hash(text: Option<&String>) -> Option<String> {
    let text = text?;
    let digest = Sha256::digest(text.as_bytes());
    Some(digest.iter().map(|byte| format!("{byte:02x}")).collect())
}

fn sort_json_values(values: &mut [Value]) {
    values.sort_by_key(|value| {
        serde_json::to_string(value).expect("JSON detail values are serializable")
    });
}

fn trigger_detail(trigger: &source::document::TriggerDoc) -> Value {
    json!({
        "name": trigger.name,
        "bodyHash": text_hash(trigger.body.as_ref()),
    })
}

fn body_hash(graph: &Graph, id: &str, kind: Option<&str>) -> Option<String> {
    root_id(graph, id, kind)
        .and_then(|id| graph.analysis().get(&id))
        .and_then(|analysis| analysis.body_hash.clone())
}

/// 실제 object/routine 구조를 fingerprint에 넣되 usage와 시각적 관측값은 제외한다.
fn structure_detail(document: &source::CatalogDocument, graph: &Graph, id: &str) -> Value {
    for schema in &document.schemas {
        for object in &schema.objects {
            if VertexId::object(&schema.name, &object.name).as_str() == id {
                return json!({
                    "id": id,
                    "kind": object.kind,
                    "bodyHash": body_hash(graph, id, None),
                    "columns": &object.columns,
                    "constraints": &object.constraints,
                    "indexes": object.indexes.iter().map(index_detail).collect::<Vec<_>>(),
                    "triggers": object.triggers.iter().map(trigger_detail).collect::<Vec<_>>(),
                });
            }
        }
        for routine in &schema.routines {
            let base = VertexId::routine(
                &schema.name,
                routine.member_of.as_deref(),
                &routine.name,
                routine.signature.as_deref(),
            );
            let candidate = base.as_str();
            let renamed = format!("{candidate}@{}", routine.kind);
            if id != candidate && id != renamed {
                continue;
            }
            return json!({
                "id": id,
                "kind": routine.kind,
                "language": routine.language,
                "signature": routine.signature,
                "memberOf": routine.member_of,
                "source": routine.source,
                "bodyHash": body_hash(graph, id, Some(&routine.kind)),
            });
        }
    }
    json!({"id": id})
}

fn indexes_detail(indexes: &source::diff::CollectionDelta<source::document::IndexDoc>) -> Value {
    json!({
        "added": indexes.added.iter().map(index_detail).collect::<Vec<_>>(),
        "removed": indexes.removed.iter().map(index_detail).collect::<Vec<_>>(),
        "changed": indexes.changed.iter().map(|change| json!({
            "name": change.name,
            "old": index_detail(&change.old),
            "new": index_detail(&change.new),
        })).collect::<Vec<_>>(),
    })
}

/// CLI와 CI는 같은 보고서의 건수와 불완전 상태로 종료 코드를 정한다.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReviewOutputFormat {
    Json,
    Markdown,
    Sarif,
}

pub(crate) struct ReviewOptions<'a> {
    pub before: &'a Path,
    pub after: &'a Path,
    pub strict: bool,
    pub require_complete: bool,
    pub max_changes: usize,
    pub max_impacted: usize,
    pub budget: schemagraph_analysis::budget::Budget,
    pub format: ReviewOutputFormat,
    pub policy: Option<&'a Path>,
    pub baseline: Option<&'a Path>,
    pub as_of: Option<&'a str>,
    pub write_baseline: Option<&'a Path>,
}

/// 정책·기준선·SARIF를 포함한 review 진입점.
pub(crate) fn run_with_options(options: ReviewOptions<'_>) -> Result<i32> {
    let ReviewOptions {
        before: before_path,
        after: after_path,
        strict,
        require_complete,
        max_changes,
        max_impacted,
        budget,
        format,
        policy: policy_path,
        baseline: baseline_path,
        as_of,
        write_baseline: write_baseline_path,
    } = options;
    let cancel = super::cancellation::install()?;
    if let Some(output) = write_baseline_path {
        if let Ok(output) = std::fs::canonicalize(output) {
            for input in [
                Some(before_path),
                Some(after_path),
                policy_path,
                baseline_path,
            ]
            .into_iter()
            .flatten()
            {
                if std::fs::canonicalize(input)? == output {
                    bail!("baseline output must not overwrite a review input; choose a separate output path");
                }
            }
        }
    }
    let old =
        super::load_document(before_path).context("review requires a before catalog document")?;
    if cancel.load(Ordering::Relaxed) {
        return Ok(130);
    }
    let new =
        super::load_document(after_path).context("review requires an after catalog document")?;
    if cancel.load(Ordering::Relaxed) {
        return Ok(130);
    }
    let before = super::analyze_document(&old, false);
    if cancel.load(Ordering::Relaxed) {
        return Ok(130);
    }
    let after = super::analyze_document(&new, false);
    if cancel.load(Ordering::Relaxed) {
        return Ok(130);
    }
    let diff = source::diff::diff_documents(&old, &new);
    let mut notes = source::context::comparison_notes(&old, &new);
    for (label, document, graph) in [("before", &old, &before), ("after", &new, &after)] {
        let missing = document
            .dependencies
            .iter()
            .filter(|dependency| {
                source::dependencies::resolve_reference(graph, document, &dependency.source)
                    .is_none()
                    || source::dependencies::resolve_reference(graph, document, &dependency.target)
                        .is_none()
            })
            .count();
        if missing > 0 {
            notes.push(format!("{label} snapshot has {missing} catalog dependency records with uncollected or ambiguous endpoints"));
        }
    }
    let mut changes = Changes {
        before: &before,
        after: &after,
        items: vec![],
        details: BTreeMap::new(),
        notes,
    };
    changes.collect(&diff, &old, &new)?;
    let all_changes = changes.items.clone();
    let details = changes.details.clone();
    let comparison_notes = changes.notes.clone();
    let report = schemagraph_analysis::review::review_with_cancellation(
        &before,
        &after,
        all_changes.clone(),
        comparison_notes,
        max_changes,
        max_impacted,
        budget,
        Some(cancel),
    );
    if cancel.load(Ordering::Relaxed) {
        return Ok(130);
    }
    let policy = policy_path.map(review_policy::load_policy).transpose()?;
    let baseline = baseline_path
        .map(review_policy::load_baseline)
        .transpose()?;
    let needs_evaluation = policy.is_some()
        || baseline.is_some()
        || write_baseline_path.is_some()
        || format == ReviewOutputFormat::Sarif;
    let mut evaluation = if needs_evaluation {
        Some(review_policy::evaluate(
            &all_changes,
            &details,
            &old,
            &new,
            policy.as_ref(),
            baseline.as_ref(),
            as_of,
            &report,
        )?)
    } else {
        None
    };
    if let Some(evaluation) = &mut evaluation {
        let visible = report
            .findings
            .iter()
            .map(|finding| {
                review_policy::fingerprint(
                    &finding.change,
                    details.get(&finding.change),
                    &old,
                    &new,
                )
            })
            .collect();
        evaluation.retain_findings(&visible);
    }
    if let Some(path) = write_baseline_path {
        if report_has_safety_gap(&report) {
            bail!("cannot write a review baseline for an incomplete or incomparable report");
        }
        let evaluation = evaluation
            .as_ref()
            .context("writing a baseline requires review evaluation")?;
        review_policy::write_baseline(path, evaluation.all_fingerprints.iter().cloned())?;
    }
    match format {
        ReviewOutputFormat::Markdown => match &evaluation {
            Some(evaluation) => print!(
                "{}",
                schemagraph_export::review::to_markdown_with_policy(
                    &report,
                    &serde_json::to_value(evaluation)?,
                )
            ),
            None => print!("{}", schemagraph_export::review::to_markdown(&report)),
        },
        ReviewOutputFormat::Json => {
            let value = match &evaluation {
                Some(evaluation) => schemagraph_export::review::to_value_with_policy(
                    &report,
                    serde_json::to_value(evaluation)?,
                ),
                None => schemagraph_export::review::to_value(&report),
            };
            println!("{}", serde_json::to_string_pretty(&value)?);
        }
        ReviewOutputFormat::Sarif => {
            let evaluation = evaluation
                .as_ref()
                .context("SARIF output requires review evaluation")?;
            let annotations = evaluation
                .findings
                .iter()
                .map(|finding| schemagraph_export::sarif::FindingAnnotation {
                    id: finding.id.clone(),
                    kind: finding.change.clone(),
                    fingerprint: finding.fingerprint.clone(),
                    level: finding.severity.sarif_level().into(),
                    baseline_state: match finding.baseline_state {
                        review_policy::BaselineState::New => "new".into(),
                        review_policy::BaselineState::Existing => "unchanged".into(),
                    },
                    before: finding.before.clone(),
                    after: finding.after.clone(),
                    suppression: finding.waiver.as_ref().map(|waiver| {
                        schemagraph_export::sarif::Suppression {
                            kind: "external".into(),
                            justification: format!(
                                "{} (expires {})",
                                waiver.reason, waiver.expires
                            ),
                        }
                    }),
                })
                .collect::<Vec<_>>();
            let policy = serde_json::to_value(evaluation)?;
            let value =
                schemagraph_export::sarif::to_value_with_policy(&report, &annotations, &policy);
            println!("{}", serde_json::to_string_pretty(&value)?);
        }
    }

    let suppression_safety_gap = (baseline.is_some()
        || policy
            .as_ref()
            .is_some_and(review_policy::Policy::has_waivers))
        && report_has_safety_gap(&report);
    let review_required = evaluation
        .as_ref()
        .map(|evaluation| evaluation.strict_unsuppressed_review_required)
        .unwrap_or(report.review_required);
    if suppression_safety_gap
        || (strict && !report.comparison_notes.is_empty())
        || (require_complete && (report.analysis_partial || report.truncated || !report.complete))
    {
        Ok(2)
    } else if (strict
        && policy_path.is_some()
        && evaluation
            .as_ref()
            .is_some_and(|evaluation| evaluation.failed))
        || (strict && policy_path.is_none() && review_required > 0)
    {
        Ok(1)
    } else {
        Ok(0)
    }
}

fn report_has_safety_gap(report: &schemagraph_analysis::review::ReviewReport) -> bool {
    !report.comparison_notes.is_empty()
        || report.analysis_partial
        || report.truncated
        || !report.complete
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn catalog_dependency_changes_preserve_overloaded_source_identity() {
        let old:source::CatalogDocument=serde_json::from_value(serde_json::json!({"version":1,"reader":"test","dialect":"postgres","limitations":[],"schemas":[{"name":"app","objects":[{"name":"t","kind":"table","columns":[],"constraints":[],"indexes":[],"triggers":[]}],"routines":[{"name":"f","kind":"function","signature":"integer","language":"sql","body":"SELECT 1"}]}]})).unwrap();
        let mut new = old.clone();
        new.dependencies.push(serde_json::from_value(serde_json::json!({"source":{"schema":"app","name":"f","signature":"integer","kind":"function"},"target":{"schema":"app","name":"t","kind":"table"},"catalog":"pg_depend","dependency_type":"n"})).unwrap());
        let before = super::super::analyze_document(&old, false);
        let after = super::super::analyze_document(&new, false);
        let mut changes = Changes {
            before: &before,
            after: &after,
            items: vec![],
            details: BTreeMap::new(),
            notes: vec![],
        };
        changes
            .collect(&source::diff::diff_documents(&old, &new), &old, &new)
            .unwrap();
        assert!(changes.notes.is_empty(), "{:?}", changes.notes);
        assert!(changes
            .items
            .iter()
            .any(|change| change.id.as_str() == "app.f(integer)"
                && change.kind == "catalog-dependencies-changed"));
    }

    #[test]
    fn structural_change_details_make_none_before_after_fingerprints_specific() {
        let old: source::CatalogDocument = serde_json::from_value(serde_json::json!({
            "version": 1,
            "reader": "test",
            "dialect": "sqlite",
            "limitations": [],
            "schemas": [{"name":"app","objects":[{"name":"users","kind":"table","columns":[{"name":"id","data_type":"integer","nullable":false,"ordinal":1,"pk_position":1},{"name":"name","data_type":"text","nullable":true,"ordinal":2,"pk_position":0}],"constraints":[{"name":"users_unique","kind":"unique","columns":["id"]}],"indexes":[{"name":"users_idx","unique":false,"columns":["id"],"usage":{"since":"2026-01-01","reads":1,"writes":2}}],"triggers":[{"name":"users_trigger","body":"BEGIN SELECT 1; END"}]}],"routines":[]}]
        }))
        .unwrap();
        let new: source::CatalogDocument = serde_json::from_value(serde_json::json!({
            "version": 1,
            "reader": "test",
            "dialect": "sqlite",
            "limitations": [],
            "schemas": [{"name":"app","objects":[{"name":"users","kind":"table","columns":[{"name":"id","data_type":"integer","nullable":false,"ordinal":1,"pk_position":1},{"name":"name","data_type":"text","nullable":true,"ordinal":2,"pk_position":0}],"constraints":[{"name":"users_unique","kind":"unique","columns":["name"]}],"indexes":[{"name":"users_idx","unique":false,"columns":["name"],"usage":{"since":"2026-02-01","reads":999,"writes":888}}],"triggers":[{"name":"users_trigger","body":"BEGIN SELECT 2; END"}]}],"routines":[]}]
        }))
        .unwrap();
        let before = super::super::analyze_document(&old, false);
        let after = super::super::analyze_document(&new, false);
        let mut changes = Changes {
            before: &before,
            after: &after,
            items: vec![],
            details: BTreeMap::new(),
            notes: vec![],
        };
        changes
            .collect(&source::diff::diff_documents(&old, &new), &old, &new)
            .unwrap();
        let change = changes
            .items
            .iter()
            .find(|change| change.kind == "indexes-changed")
            .unwrap();
        assert!(change.before.is_none() && change.after.is_none());
        let detail = changes.details.get(change).unwrap();
        assert!(detail["changed"][0]["old"].get("usage").is_none());
        assert!(detail["changed"][0]["new"].get("usage").is_none());
        let first = review_policy::fingerprint(change, Some(detail), &old, &new);
        let mut changed_detail = detail.clone();
        changed_detail["changed"][0]["new"]["columns"] = serde_json::json!(["email"]);
        let second = review_policy::fingerprint(change, Some(&changed_detail), &old, &new);
        assert_ne!(first, second);
        for kind in ["constraints-changed", "triggers-changed"] {
            let structural = changes.items.iter().find(|item| item.kind == kind).unwrap();
            let structural_detail = changes.details.get(structural).unwrap();
            assert!(structural_detail.get("usage").is_none());
        }
    }

    #[test]
    fn dependency_fingerprint_keeps_all_sources_and_change_directions() {
        let mut old: source::CatalogDocument = serde_json::from_value(serde_json::json!({
            "version": 1,
            "reader": "test",
            "dialect": "postgres",
            "limitations": [],
            "schemas": [{"name":"app","objects":[
                {"name":"t","kind":"table","columns":[],"constraints":[],"indexes":[],"triggers":[]},
                {"name":"u","kind":"table","columns":[],"constraints":[],"indexes":[],"triggers":[]},
                {"name":"v","kind":"table","columns":[],"constraints":[],"indexes":[],"triggers":[]}
            ],"routines":[{"name":"f","kind":"function","signature":"integer","language":"sql","body":"SELECT 1"}]}]
        }))
        .unwrap();
        let dependency = |target: &str| {
            serde_json::from_value(serde_json::json!({
                "source":{"schema":"app","name":"f","signature":"integer","kind":"function"},
                "target":{"schema":"app","name":target,"kind":"table"},
                "catalog":"pg_depend","dependency_type":"n"
            }))
            .unwrap()
        };
        old.dependencies = vec![dependency("t"), dependency("u")];
        let mut new = old.clone();
        new.dependencies = vec![dependency("t"), dependency("v")];
        let before = super::super::analyze_document(&old, false);
        let after = super::super::analyze_document(&new, false);
        let mut changes = Changes {
            before: &before,
            after: &after,
            items: vec![],
            details: BTreeMap::new(),
            notes: vec![],
        };
        changes
            .collect(&source::diff::diff_documents(&old, &new), &old, &new)
            .unwrap();
        let change = changes
            .items
            .iter()
            .find(|change| change.kind == "catalog-dependencies-changed")
            .unwrap();
        let detail = changes.details.get(change).unwrap();
        assert_eq!(detail["added"].as_array().unwrap().len(), 1);
        assert_eq!(detail["removed"].as_array().unwrap().len(), 1);
        let forward = review_policy::fingerprint(change, Some(detail), &old, &new);
        let reverse = json!({
            "added": detail["removed"].clone(),
            "removed": detail["added"].clone(),
        });
        assert_ne!(
            forward,
            review_policy::fingerprint(change, Some(&reverse), &old, &new)
        );
    }

    #[test]
    fn added_objects_and_routines_use_structure_details_without_usage() {
        let old: source::CatalogDocument = serde_json::from_value(serde_json::json!({
            "version": 1,
            "reader": "test",
            "dialect": "sqlite",
            "limitations": [],
            "schemas": [{"name":"app","objects":[],"routines":[]}]
        }))
        .unwrap();
        let new: source::CatalogDocument = serde_json::from_value(serde_json::json!({
            "version": 1,
            "reader": "test",
            "dialect": "sqlite",
            "limitations": [],
            "schemas": [{"name":"app","objects":[{"name":"users","kind":"table","columns":[{"name":"id","data_type":"integer","nullable":false,"ordinal":1,"pk_position":1}],"constraints":[],"indexes":[{"name":"users_idx","unique":false,"columns":["id"],"usage":{"since":"2026-01-01","reads":4,"writes":5}}],"triggers":[]}],"routines":[{"name":"f","kind":"function","signature":"integer","language":"sql","body":"SELECT 1"}]}]
        }))
        .unwrap();
        let before = super::super::analyze_document(&old, false);
        let after = super::super::analyze_document(&new, false);
        let mut changes = Changes {
            before: &before,
            after: &after,
            items: vec![],
            details: BTreeMap::new(),
            notes: vec![],
        };
        changes
            .collect(&source::diff::diff_documents(&old, &new), &old, &new)
            .unwrap();
        let object = changes
            .items
            .iter()
            .find(|change| change.id.as_str() == "app.users")
            .unwrap();
        let object_detail = changes.details.get(object).unwrap();
        assert_eq!(object_detail["columns"].as_array().unwrap().len(), 1);
        assert!(object_detail["indexes"][0].get("usage").is_none());
        let routine = changes
            .items
            .iter()
            .find(|change| change.id.as_str() == "app.f(integer)")
            .unwrap();
        assert!(changes.details.get(routine).unwrap()["bodyHash"].is_string());
    }

    #[test]
    fn routine_definition_detail_preserves_kind_and_language_changes() {
        let old: source::CatalogDocument = serde_json::from_value(serde_json::json!({
            "version": 1,
            "reader": "test",
            "dialect": "sqlite",
            "limitations": [],
            "schemas": [{"name":"app","objects":[],"routines":[{"name":"f","kind":"function","signature":"integer","language":"sql","body":"SELECT 1"}]}]
        }))
        .unwrap();
        let new: source::CatalogDocument = serde_json::from_value(serde_json::json!({
            "version": 1,
            "reader": "test",
            "dialect": "sqlite",
            "limitations": [],
            "schemas": [{"name":"app","objects":[],"routines":[{"name":"f","kind":"function","signature":"integer","language":"plpgsql","body":"SELECT 1"}]}]
        }))
        .unwrap();
        let before = super::super::analyze_document(&old, false);
        let after = super::super::analyze_document(&new, false);
        let mut changes = Changes {
            before: &before,
            after: &after,
            items: vec![],
            details: BTreeMap::new(),
            notes: vec![],
        };
        changes
            .collect(&source::diff::diff_documents(&old, &new), &old, &new)
            .unwrap();
        let change = changes
            .items
            .iter()
            .find(|change| change.kind == "definition-changed")
            .unwrap();
        let detail = changes.details.get(change).unwrap();
        assert_eq!(detail["language"]["old"], "sql");
        assert_eq!(detail["language"]["new"], "plpgsql");
    }
}
