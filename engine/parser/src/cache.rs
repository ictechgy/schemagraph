//! 캐시는 몸체 해석 결과만 보관한다. 카탈로그·통계는 매번 현재 입력에서 만든다.

use schemagraph_core::{
    Edge, EdgeKind, EvidenceLayer, Graph, ObjectAnalysis, Origin, OriginKey, VertexId, VertexKind,
};

/// 몸체 하나의 재사용 가능한 결과로 DB 수집 사실과 분리한다.
#[derive(Debug, Clone)]
pub struct BodyResult {
    pub owner: VertexId,
    pub edges: Vec<Edge>,
    pub origins: Vec<(OriginKey, Origin)>,
    pub analysis: ObjectAnalysis,
    pub notes: Vec<String>,
    pub enriched: usize,
}

/// 몸체 결과를 재사용할 때 현재 카탈로그에서 다시 확인해야 하는 범위.
///
/// `fingerprint`는 SQL 원문 자체와 별도로, 몸체가 소비한 소유자·관계·컬럼
/// 모양·routine 이름 해석 문맥을 가리킨다. `trusted`가 false면 저장소 구현은
/// 보수적으로 전체 구조 변경과 같은 miss를 선택해야 한다.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CacheContext {
    pub fingerprint: String,
    pub trusted: bool,
}

impl CacheContext {
    /// 구조적 footprint를 확인할 수 없는 몸체를 전역 miss로 취급한다.
    pub fn conservative(fingerprint: impl Into<String>) -> Self {
        Self {
            fingerprint: fingerprint.into(),
            trusted: false,
        }
    }
}

/// 저장 위치·직렬화·오류 처리를 CLI에 남겨 파서의 파일 접근을 막는다.
pub trait BodyCache {
    /// 카탈로그 구조와 파서 버전이 같은 결과만 반환해야 한다.
    fn load(&mut self, owner: &VertexId, body: &str) -> Option<BodyResult>;
    /// 같은 입력을 다시 해석하지 않도록 완성된 몸체 결과를 보관한다.
    fn store(&mut self, owner: &VertexId, body: &str, result: &BodyResult);

    /// 현재 몸체가 소비한 카탈로그 범위가 같은 결과만 재사용한다.
    ///
    /// 기존 외부 구현은 기본 동작으로 전체 namespace를 사용한다. CLI의
    /// 디스크 캐시는 이 메서드를 재정의해 dependency-scoped key를 쓴다.
    fn load_scoped(
        &mut self,
        owner: &VertexId,
        body: &str,
        _context: &CacheContext,
    ) -> Option<BodyResult> {
        self.load(owner, body)
    }

    /// dependency-scoped parser 결과를 저장한다.
    fn store_scoped(
        &mut self,
        owner: &VertexId,
        body: &str,
        result: &BodyResult,
        _context: &CacheContext,
    ) {
        self.store(owner, body, result);
    }

    /// 저장 형식은 읽혔지만 현재 그래프에 안전하게 복원하지 못한 후보를 hit에서
    /// 제외한다. 통계를 제공하지 않는 구현은 기본적으로 아무 일도 하지 않는다.
    fn record_restore_failure(&mut self) {}
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
    context: Option<&CacheContext>,
    notes: &mut Vec<String>,
    enriched: &mut usize,
) -> bool {
    let Some(body) = body.filter(|body| !body.trim().is_empty()) else {
        return false;
    };
    let Some(cache) = cache.as_deref_mut() else {
        return false;
    };
    let cached = context
        .map(|context| cache.load_scoped(owner, body, context))
        .unwrap_or_else(|| cache.load(owner, body));
    let Some(cached) = cached else {
        return false;
    };
    let restored = restore(graph, owner, body, cached, notes, enriched);
    if !restored {
        cache.record_restore_failure();
    }
    restored
}

pub(crate) fn save(
    cache: &mut Option<&mut dyn BodyCache>,
    graph: &Graph,
    owner: &VertexId,
    body: Option<&str>,
    context: Option<&CacheContext>,
    notes: &[String],
    enriched: usize,
) {
    let Some(body) = body.filter(|body| !body.trim().is_empty()) else {
        return;
    };
    if let Some(cache) = cache.as_deref_mut() {
        if let Some(result) = capture(graph, owner, notes, enriched) {
            if let Some(context) = context {
                cache.store_scoped(owner, body, &result, context);
            } else {
                cache.store(owner, body, &result);
            }
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
    if cached.owner != *owner
        || cached.analysis.body_hash.as_deref() != Some(hash.as_str())
        || cached.enriched > 1
        || cached.edges.iter().any(|e| {
            !owns(graph, owner, &e.from) && !dml_effect_owned(graph, &cached, owner, e)
                || graph.vertex(&e.from).is_none()
                || (!owns(graph, owner, &e.from)
                    && graph
                        .vertex(&e.from)
                        .is_none_or(|vertex| vertex.kind != VertexKind::Column))
                || graph.vertex(&e.to).is_none()
                || !e.kind.is_dependency()
                || e.evidence.is_empty()
                || e.evidence
                    .iter()
                    .any(|v| v.layer != EvidenceLayer::BodyParse)
                || !cached.origins.iter().any(|(key, origin)| {
                    (&key.0, &key.1, key.2) == (&e.from, &e.to, e.kind) && origin.body_hash == hash
                })
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

fn dml_effect_owned(graph: &Graph, cached: &BodyResult, owner: &VertexId, edge: &Edge) -> bool {
    if edge.kind != EdgeKind::DerivesFrom {
        return false;
    }
    if ![&edge.from, &edge.to].into_iter().all(|id| {
        graph
            .vertex(id)
            .is_some_and(|vertex| vertex.kind == VertexKind::Column)
    }) {
        return false;
    }
    let matching_write = cached.edges.iter().any(|candidate| {
        candidate.from == *owner && candidate.to == edge.from && candidate.kind == EdgeKind::Writes
    });
    let prefix = format!("dml-owner:{}:", owner.as_str());
    let owned_write_origin = cached.origins.iter().any(|(key, origin)| {
        key.0 == *owner
            && key.1 == edge.from
            && key.2 == EdgeKind::Writes
            && origin.role.starts_with(&prefix)
    });
    if !matching_write || !owned_write_origin {
        return false;
    }
    cached
        .origins
        .iter()
        .filter(|(key, _)| key.0 == edge.from && key.1 == edge.to && key.2 == edge.kind)
        .any(|(_, origin)| origin.role.starts_with(&prefix))
}

#[cfg(test)]
mod tests {
    use super::*;
    use schemagraph_source::document::CatalogDocument;
    use std::collections::BTreeMap;

    #[derive(Default)]
    struct MemoryCache(BTreeMap<VertexId, BodyResult>, usize, usize);
    impl BodyCache for MemoryCache {
        fn load(&mut self, owner: &VertexId, _: &str) -> Option<BodyResult> {
            let result = self.0.get(owner).cloned();
            self.1 += usize::from(result.is_some());
            result
        }
        fn store(&mut self, owner: &VertexId, _: &str, result: &BodyResult) {
            self.0.insert(owner.clone(), result.clone());
        }
        fn record_restore_failure(&mut self) {
            self.2 += 1;
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
        assert_eq!(cache.2, 1);
        doc.schemas[0].objects[1].body = Some("SELECT 1 AS id".into());
        let mut changed = schemagraph_source::graph::document_to_graph(&doc);
        crate::enrich_with_cache(&mut changed, &doc, Some(&mut cache));
        assert!(!changed
            .outgoing(&owner)
            .iter()
            .any(|e| e.kind == EdgeKind::Reads));
    }

    #[test]
    fn same_body_dml_effects_remain_owned_by_their_routine() {
        let doc: CatalogDocument = serde_json::from_value(serde_json::json!({
            "version": 1,
            "dialect": "postgres",
            "reader": "test",
            "limitations": [],
            "schemas": [{
                "name": "main",
                "objects": [
                    {"name":"orders","kind":"table","columns":[{"name":"amount","data_type":"INTEGER","nullable":true,"ordinal":1,"pk_position":0}],"constraints":[],"indexes":[],"triggers":[]},
                    {"name":"audit","kind":"table","columns":[{"name":"amount","data_type":"INTEGER","nullable":true,"ordinal":1,"pk_position":0}],"constraints":[],"indexes":[],"triggers":[]}
                ],
                "routines": [
                    {"name":"first","kind":"procedure","language":"sql","body":"INSERT INTO audit(amount) SELECT amount FROM orders"},
                    {"name":"second","kind":"procedure","language":"sql","body":"INSERT INTO audit(amount) SELECT amount FROM orders"}
                ]
            }]
        }))
        .unwrap();
        let mut cache = MemoryCache::default();
        let mut cold = schemagraph_source::graph::document_to_graph(&doc);
        let expected = crate::enrich_with_cache(&mut cold, &doc, Some(&mut cache));
        let first = VertexId::routine("main", None, "first", None);
        let second = VertexId::routine("main", None, "second", None);
        for owner in [&first, &second] {
            let cached = cache.0.get(owner).expect("routine cache entry");
            assert!(cached.edges.iter().any(|edge| {
                edge.from.as_str() == "main.audit.amount" && edge.kind == EdgeKind::DerivesFrom
            }));
            assert!(cached
                .origins
                .iter()
                .filter(|(key, _)| {
                    key.0.as_str() == "main.audit.amount" && key.2 == EdgeKind::DerivesFrom
                })
                .all(|(_, origin)| origin.role.starts_with(&format!("dml-owner:{owner}:"))));
        }
        let mut warm = schemagraph_source::graph::document_to_graph(&doc);
        let result = crate::enrich_with_cache(&mut warm, &doc, Some(&mut cache));
        assert_eq!(result, expected);
        assert_eq!(warm.edges(), cold.edges());
        assert_eq!(warm.analysis(), cold.analysis());
        assert_eq!(warm.origins(), cold.origins());
    }

    #[test]
    fn scoped_context_tracks_owner_and_referenced_shapes_but_ignores_unrelated_objects() {
        let mut doc: CatalogDocument = serde_json::from_value(serde_json::json!({
            "version": 1,
            "dialect": "postgres",
            "reader": "test",
            "limitations": [],
            "schemas": [{
                "name": "main",
                "objects": [
                    {"name":"orders","kind":"table","columns":[{"name":"amount","data_type":"INTEGER","nullable":true,"ordinal":1,"pk_position":0}],"constraints":[],"indexes":[],"triggers":[]},
                    {"name":"audit","kind":"table","columns":[{"name":"amount","data_type":"INTEGER","nullable":true,"ordinal":1,"pk_position":0}],"constraints":[],"indexes":[],"triggers":[]}
                ],
                "routines": [
                    {"name":"mutate","kind":"procedure","language":"sql","body":"INSERT INTO audit(amount) SELECT amount FROM orders","source":"proc.sql"}
                ]
            }]
        }))
        .unwrap();
        let owner = VertexId::routine("main", None, "mutate", None);
        let body = "INSERT INTO audit(amount) SELECT amount FROM orders";
        let graph = schemagraph_source::graph::document_to_graph(&doc);
        let first = crate::scope::cache_context(&doc, &graph, &owner, body);
        assert!(first.trusted);

        doc.schemas[0].objects.push(serde_json::from_value(serde_json::json!({
            "name":"unrelated","kind":"table","columns":[{"name":"id","data_type":"INTEGER","nullable":true,"ordinal":1,"pk_position":0}],"constraints":[],"indexes":[],"triggers":[]
        })).unwrap());
        let unrelated_graph = schemagraph_source::graph::document_to_graph(&doc);
        let unrelated = crate::scope::cache_context(&doc, &unrelated_graph, &owner, body);
        assert_eq!(first.fingerprint, unrelated.fingerprint);

        doc.schemas[0].routines[0].language = Some("plpgsql".into());
        let language_graph = schemagraph_source::graph::document_to_graph(&doc);
        let language = crate::scope::cache_context(&doc, &language_graph, &owner, body);
        assert_ne!(first.fingerprint, language.fingerprint);

        doc.schemas[0].objects[0].columns[0].ordinal = 2;
        let shape_graph = schemagraph_source::graph::document_to_graph(&doc);
        let shape = crate::scope::cache_context(&doc, &shape_graph, &owner, body);
        assert_ne!(language.fingerprint, shape.fingerprint);
    }

    #[test]
    fn cached_external_lineage_requires_a_matching_owner_write() {
        let doc: CatalogDocument = serde_json::from_value(serde_json::json!({
            "version":1,"dialect":"postgres","reader":"test","limitations":[],
            "schemas":[{"name":"main","objects":[
                {"name":"orders","kind":"table","columns":[{"name":"amount","data_type":"INTEGER","nullable":true,"ordinal":1,"pk_position":0}],"constraints":[],"indexes":[],"triggers":[]},
                {"name":"audit","kind":"table","columns":[{"name":"amount","data_type":"INTEGER","nullable":true,"ordinal":1,"pk_position":0}],"constraints":[],"indexes":[],"triggers":[]}
            ],"routines":[{"name":"mutate","kind":"procedure","language":"sql","body":"INSERT INTO audit(amount) SELECT amount FROM orders"}]}]
        })).unwrap();
        let owner = VertexId::routine("main", None, "mutate", None);
        let body = doc.schemas[0].routines[0].body.as_deref().unwrap();
        let mut cache = MemoryCache::default();
        crate::enrich_with_cache(
            &mut schemagraph_source::graph::document_to_graph(&doc),
            &doc,
            Some(&mut cache),
        );
        let mut cached = cache.0[&owner].clone();
        cached.edges.retain(|edge| {
            !(edge.from == owner
                && edge.to.as_str() == "main.audit.amount"
                && edge.kind == EdgeKind::Writes)
        });
        cached.origins.retain(|(key, _)| {
            !(key.0 == owner && key.1.as_str() == "main.audit.amount" && key.2 == EdgeKind::Writes)
        });
        let mut graph = schemagraph_source::graph::document_to_graph(&doc);
        assert!(!restore(
            &mut graph,
            &owner,
            body,
            cached,
            &mut Vec::new(),
            &mut 0
        ));
    }

    #[test]
    fn cached_external_owner_tag_cannot_authorize_a_non_lineage_edge() {
        let doc: CatalogDocument = serde_json::from_value(serde_json::json!({
            "version":1,"dialect":"postgres","reader":"test","limitations":[],
            "schemas":[{"name":"main","objects":[
                {"name":"orders","kind":"table","columns":[{"name":"amount","data_type":"INTEGER","nullable":true,"ordinal":1,"pk_position":0}],"constraints":[],"indexes":[],"triggers":[]},
                {"name":"audit","kind":"table","columns":[{"name":"amount","data_type":"INTEGER","nullable":true,"ordinal":1,"pk_position":0}],"constraints":[],"indexes":[],"triggers":[]}
            ],"routines":[{"name":"mutate","kind":"procedure","language":"sql","body":"INSERT INTO audit(amount) SELECT amount FROM orders"}]}]
        })).unwrap();
        let owner = VertexId::routine("main", None, "mutate", None);
        let body = doc.schemas[0].routines[0].body.as_deref().unwrap();
        let mut cache = MemoryCache::default();
        crate::enrich_with_cache(
            &mut schemagraph_source::graph::document_to_graph(&doc),
            &doc,
            Some(&mut cache),
        );
        let mut cached = cache.0[&owner].clone();
        let edge = cached
            .edges
            .iter_mut()
            .find(|edge| {
                edge.from.as_str() == "main.audit.amount" && edge.kind == EdgeKind::DerivesFrom
            })
            .unwrap();
        edge.kind = EdgeKind::Reads;
        for (key, _) in &mut cached.origins {
            if key.0.as_str() == "main.audit.amount" && key.2 == EdgeKind::DerivesFrom {
                key.2 = EdgeKind::Reads;
            }
        }
        let mut graph = schemagraph_source::graph::document_to_graph(&doc);
        assert!(!restore(
            &mut graph,
            &owner,
            body,
            cached,
            &mut Vec::new(),
            &mut 0
        ));
    }

    #[test]
    fn scoped_context_uses_binder_case_folding_for_relation_shapes() {
        let mut doc: CatalogDocument = serde_json::from_value(serde_json::json!({
            "version":1,"dialect":"postgres","reader":"test","limitations":[],
            "schemas":[{"name":"main","objects":[
                {"name":"orders","kind":"table","columns":[{"name":"amount","data_type":"INTEGER","nullable":true,"ordinal":1,"pk_position":0}],"constraints":[],"indexes":[],"triggers":[]}
            ],"routines":[{"name":"mutate","kind":"procedure","language":"sql","body":"SELECT * FROM Orders"}]}]
        })).unwrap();
        let owner = VertexId::routine("main", None, "mutate", None);
        let graph = schemagraph_source::graph::document_to_graph(&doc);
        let first = crate::scope::cache_context(&doc, &graph, &owner, "SELECT * FROM Orders");
        doc.schemas[0].objects[0].columns[0].ordinal = 2;
        let graph = schemagraph_source::graph::document_to_graph(&doc);
        let changed = crate::scope::cache_context(&doc, &graph, &owner, "SELECT * FROM Orders");
        assert_ne!(first.fingerprint, changed.fingerprint);
    }

    #[test]
    fn wrapped_routine_body_uses_conservative_cache_context() {
        let doc: CatalogDocument = serde_json::from_value(serde_json::json!({
            "version":1,"dialect":"postgres","reader":"test","limitations":[],
            "schemas":[{"name":"main","objects":[],"routines":[{"name":"mutate","kind":"function","language":"plpgsql","body":"CREATE FUNCTION mutate() RETURNS void AS $$ BEGIN SELECT 1; END $$ LANGUAGE plpgsql"}]}]
        })).unwrap();
        let owner = VertexId::routine("main", None, "mutate", None);
        let graph = schemagraph_source::graph::document_to_graph(&doc);
        let context = crate::scope::cache_context(
            &doc,
            &graph,
            &owner,
            doc.schemas[0].routines[0].body.as_deref().unwrap(),
        );
        assert!(!context.trusted);
    }

    #[test]
    fn scoped_context_tracks_cross_schema_routine_overloads() {
        let mut doc: CatalogDocument = serde_json::from_value(serde_json::json!({
            "version":1,"dialect":"postgres","reader":"test","limitations":[],
            "schemas":[
                {"name":"main","objects":[],"routines":[{"name":"mutate","kind":"function","language":"sql","body":"SELECT other.fn()"}]},
                {"name":"other","objects":[],"routines":[{"name":"fn","kind":"function","language":"sql","body":"SELECT 1","signature":"()"}]}
            ]
        })).unwrap();
        let owner = VertexId::routine("main", None, "mutate", None);
        let graph = schemagraph_source::graph::document_to_graph(&doc);
        let first = crate::scope::cache_context(&doc, &graph, &owner, "SELECT other.fn()");
        doc.schemas[1].routines[0].signature = Some("(integer)".into());
        let graph = schemagraph_source::graph::document_to_graph(&doc);
        let changed = crate::scope::cache_context(&doc, &graph, &owner, "SELECT other.fn()");
        assert_ne!(first.fingerprint, changed.fingerprint);
    }
}

pub(crate) fn capture(
    graph: &Graph,
    owner: &VertexId,
    notes: &[String],
    enriched: usize,
) -> Option<BodyResult> {
    let analysis = graph.analysis().get(owner)?.clone();
    let body_hash = analysis.body_hash.as_deref()?;
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
                origins.extend(
                    values
                        .iter()
                        .filter(|origin| origin.body_hash == body_hash)
                        .cloned()
                        .map(|origin| (key.clone(), origin)),
                );
            }
            edges.push(Edge {
                evidence,
                ..edge.clone()
            });
        }
    }
    // DML 값 계보는 대상 컬럼에서 시작해 routine/view의 소유 간선에 포함되지
    // 않는다. origin의 생산 소유자를 함께 확인해 같은 SQL을 쓰는 두 routine의
    // 캐시 결과가 섞이지 않게 한다.
    let owner_prefix = format!("dml-owner:{}:", owner.as_str());
    for (key, values) in graph.origins() {
        let owned_values: Vec<_> = values
            .iter()
            .filter(|origin| {
                origin.body_hash == body_hash && origin.role.starts_with(&owner_prefix)
            })
            .cloned()
            .collect();
        if owned_values.is_empty() {
            continue;
        }
        let Some(edge) = graph
            .outgoing(&key.0)
            .iter()
            .find(|edge| edge.to == key.1 && edge.kind == key.2)
        else {
            continue;
        };
        if !edges.iter().any(|existing| {
            existing.from == edge.from && existing.to == edge.to && existing.kind == edge.kind
        }) {
            let evidence: Vec<_> = edge
                .evidence
                .iter()
                .filter(|evidence| {
                    evidence.layer == EvidenceLayer::BodyParse
                        && evidence.detail.starts_with(&format!("{} ", owner.as_str()))
                })
                .cloned()
                .collect();
            if evidence.is_empty() {
                continue;
            }
            edges.push(Edge {
                evidence,
                ..edge.clone()
            });
        }
        origins.extend(owned_values.into_iter().map(|origin| (key.clone(), origin)));
    }
    if edges.iter().any(|edge| {
        !origins.iter().any(|(key, origin)| {
            (&key.0, &key.1, key.2) == (&edge.from, &edge.to, edge.kind)
                && origin.body_hash == body_hash
        })
    }) {
        return None;
    }
    Some(BodyResult {
        owner: owner.clone(),
        edges,
        origins,
        analysis,
        notes: notes.to_vec(),
        enriched,
    })
}
