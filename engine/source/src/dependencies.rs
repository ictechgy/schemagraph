//! DB 참조 행을 reader가 만든 실제 정점으로 해석한다. DB의 종류 코드를 추측하지 않는다.

use crate::document::{CatalogDocument, CatalogObjectRef};
use schemagraph_core::{Edge, EdgeKind, Evidence, EvidenceLayer, Graph, VertexId, VertexKind};

fn kind_matches(kind: VertexKind, expected: Option<&str>) -> bool {
    match expected {
        Some("table") => kind == VertexKind::Table,
        Some("view") => kind == VertexKind::View,
        Some("materialized-view") => kind == VertexKind::MaterializedView,
        Some("function") => kind == VertexKind::Function,
        Some("procedure") => kind == VertexKind::Procedure,
        Some("package") => kind == VertexKind::Package,
        Some("synonym") => kind == VertexKind::Synonym,
        Some("sequence") => kind == VertexKind::Sequence,
        Some("type") => kind == VertexKind::Type,
        Some(_) => false,
        None => kind.level() == schemagraph_core::Level::Object,
    }
}

/// 다른 DB의 동명 객체를 로컬 정점에 연결하지 않는다.
pub fn resolve_reference(
    graph: &Graph,
    doc: &CatalogDocument,
    reference: &CatalogObjectRef,
) -> Option<VertexId> {
    if let Some(database) = &reference.database {
        if doc.context.as_ref().and_then(|c| c.database.as_ref()) != Some(database) {
            return None;
        }
    }
    let schema_id = VertexId::schema(&reference.schema);
    let mut candidates: Vec<_> = graph
        .outgoing(&schema_id)
        .iter()
        .filter(|edge| edge.kind == EdgeKind::Contains)
        .filter_map(|edge| graph.vertex(&edge.to))
        .filter(|v| {
            v.schema == reference.schema
                && v.name == reference.name
                // 카탈로그 object 이름은 schema 직속이다. 같은 이름의 패키지
                // 멤버나 부모 미수집 fallback 정점을 대신 고르지 않는다.
                && v.id.parent().as_ref() == Some(&schema_id)
                && kind_matches(v.kind, reference.kind.as_deref())
        })
        .collect();
    if let Some(signature) = &reference.signature {
        let expected = VertexId::routine(&reference.schema, None, &reference.name, Some(signature));
        candidates.retain(|v| {
            v.id == expected
                || v.id
                    .as_str()
                    .strip_prefix(expected.as_str())
                    .is_some_and(|s| s.starts_with('@') && !s[1..].contains('.'))
        });
    }
    if candidates.len() != 1 {
        return None;
    }
    let object = &candidates[0].id;
    if let Some(member) = &reference.member {
        let hits: Vec<_> = graph
            .outgoing(object)
            .iter()
            .filter(|e| e.kind == EdgeKind::Contains)
            .filter_map(|e| graph.vertex(&e.to))
            .filter(|v| v.kind == VertexKind::Column && v.name == *member)
            .collect();
        (hits.len() == 1).then(|| hits[0].id.clone())
    } else {
        Some(object.clone())
    }
}

/// 카탈로그 증거는 SQL 분석 근거를 덮어쓰지 않고 함께 보존한다.
pub(crate) fn apply(graph: &mut Graph, doc: &CatalogDocument) {
    let mut dependencies = doc.dependencies.clone();
    dependencies.sort();
    dependencies.dedup();
    for dependency in dependencies {
        let from = resolve_reference(graph, doc, &dependency.source);
        let to = resolve_reference(graph, doc, &dependency.target);
        match (from,to) {
            (Some(from),Some(to))=>graph.add_edge(Edge {from,to,kind:EdgeKind::DependsOn,evidence:vec![Evidence {layer:EvidenceLayer::Catalog,detail:format!("{} dependency type {}",dependency.catalog,dependency.dependency_type)}]}),
            _=>graph.add_limitation(format!("{} dependency {}.{} -> {}.{} could not be resolved to collected vertices; no target inferred",dependency.catalog,dependency.source.schema,dependency.source.name,dependency.target.schema,dependency.target.name)),
        }
    }
}
