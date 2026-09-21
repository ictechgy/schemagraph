//! 카탈로그 차이를 그래프의 실제 id로 해석해 순수 검토 분석으로 전달한다.

use anyhow::{Context, Result};
use schemagraph_analysis::review::Change;
use schemagraph_core::{EdgeKind, Graph, VertexId, VertexKind};
use schemagraph_source::{self as source, diff::DocumentDiff};
use std::path::Path;
use std::sync::atomic::Ordering;

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
    notes: Vec<String>,
}

impl Changes<'_> {
    fn add(
        &mut self,
        id: &str,
        member: Option<&str>,
        object_kind: Option<&str>,
        kind: &str,
        before: Option<String>,
        after: Option<String>,
    ) {
        let resolve = |graph| match member {
            Some(name) => column_id(graph, id, name),
            None => root_id(graph, id, object_kind),
        };
        let target = resolve(self.before).or_else(|| resolve(self.after));
        match target {
            Some(id) => self.items.push(Change {
                id,
                kind: kind.into(),
                before,
                after,
            }),
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
        for item in diff.objects.added.iter().chain(&diff.routines.added) {
            self.add(
                &item.id,
                None,
                Some(&item.kind),
                "object-added",
                None,
                Some(item.kind.clone()),
            );
        }
        for item in diff.objects.removed.iter().chain(&diff.routines.removed) {
            self.add(
                &item.id,
                None,
                Some(&item.kind),
                "object-removed",
                Some(item.kind.clone()),
                None,
            );
        }
        for object in &diff.objects.changed {
            if let Some(kind) = &object.kind {
                self.add(
                    &object.id,
                    None,
                    None,
                    "object-kind-changed",
                    Some(kind.old.clone()),
                    Some(kind.new.clone()),
                );
            }
            for col in &object.columns.added {
                let kind = if !col.nullable && col.default.is_none() {
                    "required-column-added"
                } else {
                    "column-added"
                };
                self.add(
                    &object.id,
                    Some(&col.name),
                    None,
                    kind,
                    None,
                    Some(col.data_type.clone()),
                );
            }
            for col in &object.columns.removed {
                self.add(
                    &object.id,
                    Some(&col.name),
                    None,
                    "column-removed",
                    Some(col.data_type.clone()),
                    None,
                );
            }
            for col in &object.columns.changed {
                if col.old.data_type != col.new.data_type {
                    self.add(
                        &object.id,
                        Some(&col.name),
                        None,
                        "column-type-changed",
                        Some(col.old.data_type.clone()),
                        Some(col.new.data_type.clone()),
                    );
                }
                if col.old.nullable != col.new.nullable {
                    self.add(
                        &object.id,
                        Some(&col.name),
                        None,
                        "column-nullability-changed",
                        Some(col.old.nullable.to_string()),
                        Some(col.new.nullable.to_string()),
                    );
                }
                if col.old.default != col.new.default {
                    self.add(
                        &object.id,
                        Some(&col.name),
                        None,
                        "column-default-changed",
                        col.old.default.clone(),
                        col.new.default.clone(),
                    );
                }
                if col.old.pk_position != col.new.pk_position || col.old.ordinal != col.new.ordinal
                {
                    self.add(
                        &object.id,
                        Some(&col.name),
                        None,
                        "column-position-or-key-changed",
                        None,
                        None,
                    );
                }
            }
            if !object.constraints.added.is_empty()
                || !object.constraints.removed.is_empty()
                || !object.constraints.changed.is_empty()
            {
                self.add(&object.id, None, None, "constraints-changed", None, None);
            }
            if !object.indexes.removed.is_empty() || !object.indexes.changed.is_empty() {
                self.add(&object.id, None, None, "indexes-changed", None, None);
            } else if !object.indexes.added.is_empty() {
                self.add(&object.id, None, None, "index-added", None, None);
            }
            if !object.triggers.added.is_empty()
                || !object.triggers.removed.is_empty()
                || !object.triggers.changed.is_empty()
            {
                self.add(&object.id, None, None, "triggers-changed", None, None);
            }
            if object.body_changed {
                self.definition(&object.id, None);
            }
        }
        for routine in &diff.routines.changed {
            if routine.body_changed || routine.language.is_some() || routine.kind.is_some() {
                self.definition(&routine.id, routine.kind.as_ref().map(|k| k.old.as_str()));
            }
        }
        for dependency in diff
            .dependencies
            .added
            .iter()
            .chain(&diff.dependencies.removed)
        {
            let id = source::dependencies::resolve_reference(self.before, old, &dependency.source)
                .or_else(|| {
                    source::dependencies::resolve_reference(self.after, new, &dependency.source)
                });
            if let Some(id) = id {
                self.items.push(Change {
                    id,
                    kind: "catalog-dependencies-changed".into(),
                    before: None,
                    after: None,
                });
            } else {
                self.notes.push(format!("changed catalog dependency source '{}.{}' could not be resolved to a graph vertex",dependency.source.schema,dependency.source.name));
            }
        }
        Ok(())
    }

    fn definition(&mut self, id: &str, kind: Option<&str>) {
        let hash = |graph: &Graph| {
            root_id(graph, id, kind)
                .and_then(|id| graph.analysis().get(&id))
                .and_then(|a| a.body_hash.clone())
        };
        self.add(
            id,
            None,
            kind,
            "definition-changed",
            hash(self.before),
            hash(self.after),
        );
    }
}

/// CLI와 CI는 같은 보고서의 건수와 불완전 상태로 종료 코드를 정한다.
pub(crate) fn run(
    before: &Path,
    after: &Path,
    strict: bool,
    require_complete: bool,
    max_changes: usize,
    max_impacted: usize,
    budget: schemagraph_analysis::budget::Budget,
    markdown: bool,
) -> Result<i32> {
    let cancel = super::cancellation::install()?;
    let old = super::load_document(before).context("review requires a before catalog document")?;
    if cancel.load(Ordering::Relaxed) {
        return Ok(130);
    }
    let new = super::load_document(after).context("review requires an after catalog document")?;
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
        notes,
    };
    changes.collect(&diff, &old, &new)?;
    let report = schemagraph_analysis::review::review_with_cancellation(
        &before,
        &after,
        changes.items,
        changes.notes,
        max_changes,
        max_impacted,
        budget,
        Some(cancel),
    );
    if markdown {
        print!("{}", schemagraph_export::review::to_markdown(&report));
    } else {
        println!(
            "{}",
            serde_json::to_string_pretty(&schemagraph_export::review::to_value(&report))?
        );
    }
    if (strict && !report.comparison_notes.is_empty())
        || (require_complete && (report.analysis_partial || report.truncated || !report.complete))
    {
        Ok(2)
    } else if strict && report.review_required > 0 {
        Ok(1)
    } else {
        Ok(0)
    }
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
}
