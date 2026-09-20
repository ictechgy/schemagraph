//! DB별 SQL 해석 후 수집된 DB 참조만 명시적인 source namespace 사이에 잇는다.

use anyhow::{anyhow, bail, Result};
use schemagraph_core::{
    merge_graphs, namespaced_id, Edge, EdgeKind, Evidence, EvidenceLayer, Graph,
};
use schemagraph_source::{self as source, CatalogDocument};
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

fn source_id(doc: &CatalogDocument) -> Result<&str> {
    let id = doc
        .context
        .as_ref()
        .map(|context| context.source_id.as_str())
        .unwrap_or("");
    source::context::validate_source_id(id).map_err(|error| anyhow!(error))?;
    Ok(id)
}

pub(crate) fn run(paths: &[PathBuf]) -> Result<Graph> {
    let documents = paths
        .iter()
        .map(|path| super::load_document(path))
        .collect::<Result<Vec<_>>>()?;
    combine(documents)
}

fn combine(mut documents: Vec<CatalogDocument>) -> Result<Graph> {
    let mut names = BTreeSet::new();
    for document in &documents {
        if !names.insert(source_id(document)?.to_owned()) {
            bail!("merge requires distinct source-id labels");
        }
    }
    documents.sort_by(|a, b| {
        a.context
            .as_ref()
            .map(|c| &c.source_id)
            .cmp(&b.context.as_ref().map(|c| &c.source_id))
    });
    let mut databases: BTreeMap<String, Vec<usize>> = BTreeMap::new();
    let mut local = Vec::new();
    for (index, document) in documents.iter().enumerate() {
        if let Some(database) = document
            .context
            .as_ref()
            .and_then(|context| context.database.as_ref())
        {
            databases.entry(database.clone()).or_default().push(index);
        }
        let mut doc = document.clone();
        let own_database = doc
            .context
            .as_ref()
            .and_then(|context| context.database.as_ref());
        // 미수집 외부 참조는 통합 단계에서 판정하므로 로컬 분석의 실패 수에 중복 집계하지 않는다.
        doc.dependencies.retain(|dependency| {
            dependency
                .target
                .database
                .as_ref()
                .is_none_or(|database| Some(database) == own_database)
        });
        local.push(super::analyze_document(&doc, false));
    }
    let mut cross_edges = Vec::new();
    let mut limitations = Vec::new();
    for (index, document) in documents.iter().enumerate() {
        let own_database = document
            .context
            .as_ref()
            .and_then(|context| context.database.as_ref());
        for dependency in &document.dependencies {
            let Some(database) = &dependency.target.database else {
                continue;
            };
            if Some(database) == own_database {
                continue;
            }
            let candidates = databases.get(database).map(Vec::as_slice).unwrap_or(&[]);
            let target_index = if candidates.len() == 1 {
                Some(candidates[0])
            } else {
                None
            };
            let from = source::dependencies::resolve_reference(
                &local[index],
                document,
                &dependency.source,
            );
            let to = target_index.and_then(|target| {
                source::dependencies::resolve_reference(
                    &local[target],
                    &documents[target],
                    &dependency.target,
                )
                .map(|id| (target, id))
            });
            match (from,to) {
                (Some(from),Some((target,to))) => cross_edges.push(Edge {
                    from:namespaced_id(source_id(document)?, &from),
                    to:namespaced_id(source_id(&documents[target])?, &to),
                    kind:EdgeKind::DependsOn,
                    evidence:vec![Evidence {layer:EvidenceLayer::Catalog,detail:format!("{} dependency type {}",dependency.catalog,dependency.dependency_type)}],
                }),
                _ => limitations.push(format!("{}: cross-database dependency to {database}.{}.{} is missing or ambiguous among collected sources; no target inferred",source_id(document)?,dependency.target.schema,dependency.target.name)),
            }
        }
    }
    let inputs = documents
        .iter()
        .zip(local)
        .map(|(doc, graph)| source_id(doc).map(|id| (id.to_owned(), graph)))
        .collect::<Result<Vec<_>>>()?;
    let mut merged = merge_graphs(inputs).map_err(|error| anyhow!(error))?;
    cross_edges.sort_by(|a, b| (&a.from, &a.to, a.kind).cmp(&(&b.from, &b.to, b.kind)));
    for edge in cross_edges {
        merged.add_edge(edge);
    }
    limitations.sort();
    limitations.dedup();
    for limitation in limitations {
        merged.add_limitation(limitation);
    }
    Ok(merged)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    fn doc(source: &str, database: &str) -> CatalogDocument {
        serde_json::from_value(json!({"version":1,"reader":"test","dialect":"sqlserver","limitations":[],"context":{"source_id":source,"database":database,"catalog_complete":true},"schemas":[{"name":"dbo","objects":[{"name":"t","kind":"table","columns":[],"constraints":[],"indexes":[],"triggers":[]}],"routines":[]}]})).unwrap()
    }
    #[test]
    fn only_unique_collected_database_targets_form_edges() {
        let mut a = doc("app", "a");
        a.dependencies.push(serde_json::from_value(json!({"source":{"schema":"dbo","name":"t","kind":"table"},"target":{"schema":"dbo","name":"t","kind":"table","database":"b"},"catalog":"sys.sql_expression_dependencies","dependency_type":"by-name"})).unwrap());
        let joined = combine(vec![a.clone(), doc("warehouse", "b")]).unwrap();
        assert!(joined
            .edges()
            .iter()
            .any(|edge| edge.from.as_str() == "app::dbo.t"
                && edge.to.as_str() == "warehouse::dbo.t"
                && edge.kind == EdgeKind::DependsOn));
        let ambiguous = combine(vec![a, doc("warehouse", "b"), doc("replica", "b")]).unwrap();
        assert!(!ambiguous
            .edges()
            .iter()
            .any(|edge| edge.kind == EdgeKind::DependsOn));
        assert!(ambiguous
            .limitations()
            .iter()
            .any(|note| note.contains("ambiguous")));
    }
}
