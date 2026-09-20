//! 캐시는 몸체 해석 결과만 보관한다. 카탈로그·통계는 매번 현재 입력에서 만든다.

use schemagraph_core::{
    Edge, EdgeKind, EvidenceLayer, Graph, ObjectAnalysis, Origin, OriginKey, VertexId, VertexKind,
};

/// 몸체 하나의 재사용 가능한 결과로 DB 수집 사실과 분리한다.
#[derive(Debug, Clone)]
pub struct BodyResult {
    pub edges: Vec<Edge>,
    pub origins: Vec<(OriginKey, Origin)>,
    pub analysis: ObjectAnalysis,
    pub notes: Vec<String>,
    pub enriched: usize,
}

/// 저장 위치·직렬화·오류 처리를 CLI에 남겨 파서의 파일 접근을 막는다.
pub trait BodyCache {
    /// 카탈로그 구조와 파서 버전이 같은 결과만 반환해야 한다.
    fn load(&mut self, owner: &VertexId, body: &str) -> Option<BodyResult>;
    /// 같은 입력을 다시 해석하지 않도록 완성된 몸체 결과를 보관한다.
    fn store(&mut self, owner: &VertexId, body: &str, result: &BodyResult);
}

fn owns(graph: &Graph, owner: &VertexId, id: &VertexId) -> bool {
    id == owner
        || (graph
            .vertex(id)
            .is_some_and(|v| v.kind == VertexKind::Column)
            && graph
                .outgoing(owner)
                .iter()
                .any(|e| e.kind == EdgeKind::Contains && &e.to == id))
}

pub(crate) fn reuse(
    cache: &mut Option<&mut dyn BodyCache>,
    graph: &mut Graph,
    owner: &VertexId,
    body: Option<&str>,
    notes: &mut Vec<String>,
    enriched: &mut usize,
) -> bool {
    let Some(body) = body.filter(|body| !body.trim().is_empty()) else {
        return false;
    };
    cache
        .as_deref_mut()
        .and_then(|cache| cache.load(owner, body))
        .is_some_and(|cached| restore(graph, owner, body, cached, notes, enriched))
}

pub(crate) fn save(
    cache: &mut Option<&mut dyn BodyCache>,
    graph: &Graph,
    owner: &VertexId,
    body: Option<&str>,
    notes: &[String],
    enriched: usize,
) {
    let Some(body) = body.filter(|body| !body.trim().is_empty()) else {
        return;
    };
    if let Some(cache) = cache.as_deref_mut() {
        if let Some(result) = capture(graph, owner, notes, enriched) {
            cache.store(owner, body, &result);
        }
    }
}

pub(crate) fn restore(
    graph: &mut Graph,
    owner: &VertexId,
    body: &str,
    cached: BodyResult,
    notes: &mut Vec<String>,
    enriched: &mut usize,
) -> bool {
    let hash = super::scope::body_hash(body);
    if cached.analysis.body_hash.as_deref() != Some(hash.as_str())
        || cached.enriched > 1
        || cached.edges.iter().any(|e| {
            !owns(graph, owner, &e.from)
                || graph.vertex(&e.to).is_none()
                || !e.kind.is_dependency()
                || e.evidence.is_empty()
                || e.evidence
                    .iter()
                    .any(|v| v.layer != EvidenceLayer::BodyParse)
        })
        || cached.origins.iter().any(|(key, origin)| {
            origin.body_hash != hash
                || !cached
                    .edges
                    .iter()
                    .any(|e| (&e.from, &e.to, e.kind) == (&key.0, &key.1, key.2))
        })
    {
        return false;
    }
    for edge in cached.edges {
        graph.add_edge(edge);
    }
    for (key, origin) in cached.origins {
        graph.add_origin(key, origin);
    }
    graph.set_analysis(owner.clone(), cached.analysis);
    notes.extend(cached.notes);
    *enriched += cached.enriched;
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use schemagraph_source::document::CatalogDocument;
    use std::collections::BTreeMap;

    #[derive(Default)]
    struct MemoryCache(BTreeMap<VertexId, BodyResult>, usize);
    impl BodyCache for MemoryCache {
        fn load(&mut self, owner: &VertexId, _: &str) -> Option<BodyResult> {
            let result = self.0.get(owner).cloned();
            self.1 += usize::from(result.is_some());
            result
        }
        fn store(&mut self, owner: &VertexId, _: &str, result: &BodyResult) {
            self.0.insert(owner.clone(), result.clone());
        }
    }

    fn fixture() -> CatalogDocument {
        serde_json::from_value(serde_json::json!({
            "version":1,"dialect":"sqlite","reader":"test","limitations":[],
            "schemas":[{"name":"main","routines":[],"objects":[
                {"name":"t","kind":"table","columns":[{"name":"id","data_type":"INTEGER","nullable":false,"ordinal":0,"pk_position":1}],"constraints":[],"indexes":[],"triggers":[]},
                {"name":"v","kind":"view","body":"SELECT id FROM t","columns":[{"name":"id","data_type":"INTEGER","nullable":true,"ordinal":0,"pk_position":0}],"constraints":[],"indexes":[],"triggers":[{"name":"write_v","timing":"instead-of","events":["update"],"body":"CREATE TRIGGER write_v INSTEAD OF UPDATE ON v BEGIN UPDATE t SET id=NEW.id; END"}]}
            ]}]
        })).unwrap()
    }

    #[test]
    fn hits_preserve_view_triggers_origins_and_current_usage() {
        let doc = fixture();
        let mut cache = MemoryCache::default();
        let mut cold = schemagraph_source::graph::document_to_graph(&doc);
        let expected = crate::enrich_with_cache(&mut cold, &doc, Some(&mut cache));
        assert_eq!(cache.0.len(), 2);
        let mut warm = schemagraph_source::graph::document_to_graph(&doc);
        let id = VertexId::object("main", "t");
        warm.set_usage(
            id.clone(),
            schemagraph_core::Usage {
                reads: 23,
                ..Default::default()
            },
        );
        let result = crate::enrich_with_cache(&mut warm, &doc, Some(&mut cache));
        assert_eq!(cache.1, 2);
        assert_eq!(result, expected);
        assert_eq!(warm.edges(), cold.edges());
        assert_eq!(warm.analysis(), cold.analysis());
        assert_eq!(warm.origins(), cold.origins());
        assert_eq!(warm.usage(&id).unwrap().reads, 23);
    }

    #[test]
    fn invalid_cached_target_or_body_is_reparsed_without_phantom_edges() {
        let mut doc = fixture();
        let mut cache = MemoryCache::default();
        crate::enrich_with_cache(
            &mut schemagraph_source::graph::document_to_graph(&doc),
            &doc,
            Some(&mut cache),
        );
        let owner = VertexId::object("main", "v");
        cache.0.get_mut(&owner).unwrap().edges[0].to = VertexId::from_raw("missing");
        let mut graph = schemagraph_source::graph::document_to_graph(&doc);
        crate::enrich_with_cache(&mut graph, &doc, Some(&mut cache));
        assert!(graph.edges().iter().all(|e| graph.vertex(&e.to).is_some()));
        doc.schemas[0].objects[1].body = Some("SELECT 1 AS id".into());
        let mut changed = schemagraph_source::graph::document_to_graph(&doc);
        crate::enrich_with_cache(&mut changed, &doc, Some(&mut cache));
        assert!(!changed
            .outgoing(&owner)
            .iter()
            .any(|e| e.kind == EdgeKind::Reads));
    }
}

pub(crate) fn capture(
    graph: &Graph,
    owner: &VertexId,
    notes: &[String],
    enriched: usize,
) -> Option<BodyResult> {
    let mut owners = vec![owner.clone()];
    owners.extend(
        graph
            .outgoing(owner)
            .iter()
            .filter(|e| e.kind == EdgeKind::Contains)
            .filter_map(|e| graph.vertex(&e.to))
            .filter(|v| v.kind == VertexKind::Column)
            .map(|v| v.id.clone()),
    );
    let mut edges = Vec::new();
    let mut origins = Vec::new();
    for id in owners {
        for edge in graph.outgoing(&id) {
            let evidence: Vec<_> = edge
                .evidence
                .iter()
                .filter(|v| v.layer == EvidenceLayer::BodyParse)
                .cloned()
                .collect();
            if evidence.is_empty() {
                continue;
            }
            let key = (edge.from.clone(), edge.to.clone(), edge.kind);
            if let Some(values) = graph.origins().get(&key) {
                origins.extend(values.iter().cloned().map(|origin| (key.clone(), origin)));
            }
            edges.push(Edge {
                evidence,
                ..edge.clone()
            });
        }
    }
    Some(BodyResult {
        edges,
        origins,
        analysis: graph.analysis().get(owner)?.clone(),
        notes: notes.to_vec(),
        enriched,
    })
}
