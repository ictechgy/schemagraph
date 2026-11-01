//! 캐시는 몸체 해석 결과만 보관한다. 카탈로그·통계는 매번 현재 입력에서 만든다.

use std::collections::BTreeSet;

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

/// 몸체 하나를 다시 해석한 결과 중 그래프 밖에 있어 저장할 때 함께 넘기는 값.
pub(crate) struct BodyOutcome<'a> {
    /// 이 몸체가 남긴 limitation 메시지. 생성 순서 그대로다.
    pub(crate) notes: &'a [String],
    /// 이 몸체로 간선을 보강한 객체 수(0 또는 1).
    pub(crate) enriched: usize,
    /// 이 몸체의 DML 분석이 origin을 단 간선 키(`CatalogIndex::take_dml_origins`).
    pub(crate) dml_origins: BTreeSet<OriginKey>,
}

pub(crate) fn save(
    cache: &mut Option<&mut dyn BodyCache>,
    graph: &Graph,
    owner: &VertexId,
    body: Option<&str>,
    context: Option<&CacheContext>,
    outcome: BodyOutcome<'_>,
) {
    let Some(body) = body.filter(|body| !body.trim().is_empty()) else {
        return;
    };
    if let Some(cache) = cache.as_deref_mut() {
        if let Some(result) = capture(graph, owner, outcome) {
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
        || !candidates_exist(graph, &cached.analysis)
        || !CachedKeys::new(graph, owner, &hash, &cached).accept(graph, &cached)
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

/// 진단이 후보로 가리키는 정점이 모두 현재 그래프에 있는가. 후보는 `dead`가 의존자로
/// 세는 근거라, 사라진 정점을 가리키는 캐시 결과를 복원하면 유령 후보가 된다.
fn candidates_exist(graph: &Graph, analysis: &ObjectAnalysis) -> bool {
    analysis
        .diagnostics
        .iter()
        .flat_map(|diagnostic| &diagnostic.candidates)
        .all(|candidate| graph.vertex(candidate).is_some())
}

/// 빌린 (from, to, kind) 간선 키.
type KeyRef<'a> = (&'a VertexId, &'a VertexId, EdgeKind);

/// hit를 복원하기 전에 확인하는 캐시 결과의 키 집합.
///
/// 간선마다 origin 목록을, origin마다 간선 목록을 훑으면 넓은 뷰·대형 프로시저의
/// hit가 다시 파싱하는 것보다 느려진다. 집합을 한 번 만들고 같은 거부 조건을
/// 조회로 확인한다.
struct CachedKeys<'a> {
    owner: &'a VertexId,
    hash: &'a str,
    /// owner가 Contains로 담은 정점. 그중 컬럼은 owner 소유 간선의 출발점이다.
    contained: BTreeSet<&'a VertexId>,
    /// 캐시 간선의 키.
    edges: BTreeSet<KeyRef<'a>>,
    /// 이 몸체 해시를 가진 캐시 origin의 키.
    hashed_origins: BTreeSet<KeyRef<'a>>,
    /// owner의 DML 분석이 남긴(`dml-owner:{owner}:`) 캐시 origin의 키.
    owned_origins: BTreeSet<KeyRef<'a>>,
}

impl<'a> CachedKeys<'a> {
    fn new(graph: &'a Graph, owner: &'a VertexId, hash: &'a str, cached: &'a BodyResult) -> Self {
        let mut keys = Self {
            owner,
            hash,
            contained: BTreeSet::new(),
            edges: BTreeSet::new(),
            hashed_origins: BTreeSet::new(),
            owned_origins: BTreeSet::new(),
        };
        for edge in graph.outgoing(owner) {
            #[cfg(test)]
            count_restore_visit();
            if edge.kind == EdgeKind::Contains {
                keys.contained.insert(&edge.to);
            }
        }
        for edge in &cached.edges {
            #[cfg(test)]
            count_restore_visit();
            keys.edges.insert((&edge.from, &edge.to, edge.kind));
        }
        keys.add_origins(cached);
        keys
    }

    /// origin 키를 몸체 해시가 맞는 것과 owner DML이 남긴 것으로 나눠 담는다.
    fn add_origins(&mut self, cached: &'a BodyResult) {
        let prefix = format!("dml-owner:{}:", self.owner.as_str());
        for (key, origin) in &cached.origins {
            #[cfg(test)]
            count_restore_visit();
            if origin.body_hash == self.hash {
                self.hashed_origins.insert((&key.0, &key.1, key.2));
            }
            if origin.role.starts_with(&prefix) {
                self.owned_origins.insert((&key.0, &key.1, key.2));
            }
        }
    }

    /// 캐시 간선과 origin을 모두 현재 그래프에 안전하게 복원할 수 있는지 본다.
    fn accept(&self, graph: &Graph, cached: &'a BodyResult) -> bool {
        cached
            .edges
            .iter()
            .all(|edge| self.accept_edge(graph, edge))
            && cached.origins.iter().all(|(key, origin)| {
                #[cfg(test)]
                count_restore_visit();
                origin.body_hash == self.hash && self.edges.contains(&(&key.0, &key.1, key.2))
            })
    }

    /// owner가 소유했거나 owner 쓰기에 딸린 DML 계보이고, 양 끝이 그래프에 있으며,
    /// 몸체 근거만 있고 이 몸체 해시의 origin이 있는 의존 간선만 받는다.
    fn accept_edge(&self, graph: &Graph, edge: &'a Edge) -> bool {
        #[cfg(test)]
        count_restore_visit();
        let owned = self.owns(graph, &edge.from);
        (owned || self.dml_effect_owned(graph, edge))
            && graph
                .vertex(&edge.from)
                .is_some_and(|vertex| owned || vertex.kind == VertexKind::Column)
            && graph.vertex(&edge.to).is_some()
            && edge.kind.is_dependency()
            && !edge.evidence.is_empty()
            && edge
                .evidence
                .iter()
                .all(|evidence| evidence.layer == EvidenceLayer::BodyParse)
            && self
                .hashed_origins
                .contains(&(&edge.from, &edge.to, edge.kind))
    }

    /// owner 자신이거나 owner가 Contains로 담은 컬럼이다.
    fn owns(&self, graph: &Graph, id: &VertexId) -> bool {
        id == self.owner
            || (graph
                .vertex(id)
                .is_some_and(|vertex| vertex.kind == VertexKind::Column)
                && self.contained.contains(id))
    }

    /// 컬럼 사이 DerivesFrom이고, owner가 그 대상 컬럼에 쓰는 캐시 간선과 owner DML
    /// origin이 있으며, 계보 자체에도 owner DML origin이 있다.
    fn dml_effect_owned(&self, graph: &Graph, edge: &'a Edge) -> bool {
        let column = |id: &VertexId| {
            graph
                .vertex(id)
                .is_some_and(|vertex| vertex.kind == VertexKind::Column)
        };
        let write = (self.owner, &edge.from, EdgeKind::Writes);
        edge.kind == EdgeKind::DerivesFrom
            && column(&edge.from)
            && column(&edge.to)
            && self.edges.contains(&write)
            && self.owned_origins.contains(&write)
            && self
                .owned_origins
                .contains(&(&edge.from, &edge.to, edge.kind))
    }
}

#[cfg(test)]
thread_local! {
    /// capture가 DML 값 계보를 찾으려고 들여다본 origin 키 수. 이차 시간 회귀는 작은
    /// 입력에서 시간으로 드러나지 않으므로 작업량을 횟수로 센다. 테스트는 스레드마다
    /// 병렬로 돌므로 계수도 스레드마다 따로 둔다.
    static CAPTURED_KEYS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    /// restore가 hit를 검증하며 들여다본 캐시 간선·origin과 owner 인접 간선 수.
    static RESTORE_VISITS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// 현재 테스트 스레드의 capture 계수를 돌려주고 0으로 되돌린다.
#[cfg(test)]
fn take_captured_keys() -> usize {
    CAPTURED_KEYS.with(|count| count.replace(0))
}

/// restore가 항목 하나를 들여다봤음을 센다.
#[cfg(test)]
fn count_restore_visit() {
    RESTORE_VISITS.with(|count| count.set(count.get() + 1));
}

/// 현재 테스트 스레드의 restore 계수를 돌려주고 0으로 되돌린다.
#[cfg(test)]
fn take_restore_visits() -> usize {
    RESTORE_VISITS.with(|count| count.replace(0))
}

pub(crate) fn capture(
    graph: &Graph,
    owner: &VertexId,
    outcome: BodyOutcome<'_>,
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
    // 캐시 결과가 섞이지 않게 한다. 후보는 DML 분석이 이 owner에 기록한 키만 본다
    // — 몸체마다 그래프의 모든 origin을 훑으면 저장 비용이 몸체 수의 제곱이 된다.
    // 키를 정렬 순서로 돌므로 모으는 순서는 전체 origin을 훑을 때와 같다.
    let mut captured: BTreeSet<OriginKey> = edges.iter().map(edge_key).collect();
    let owner_prefix = format!("dml-owner:{}:", owner.as_str());
    let evidence_prefix = format!("{} ", owner.as_str());
    for key in &outcome.dml_origins {
        #[cfg(test)]
        CAPTURED_KEYS.with(|count| count.set(count.get() + 1));
        let Some(values) = graph.origins().get(key) else {
            continue;
        };
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
        // 이미 모은 간선은 그래프에 있는 owner 간선이라 다시 찾지 않는다.
        if !captured.contains(key) {
            let Some(edge) = graph
                .outgoing(&key.0)
                .iter()
                .find(|edge| edge.to == key.1 && edge.kind == key.2)
            else {
                continue;
            };
            let evidence: Vec<_> = edge
                .evidence
                .iter()
                .filter(|evidence| {
                    evidence.layer == EvidenceLayer::BodyParse
                        && evidence.detail.starts_with(&evidence_prefix)
                })
                .cloned()
                .collect();
            if evidence.is_empty() {
                continue;
            }
            captured.insert(key.clone());
            edges.push(Edge {
                evidence,
                ..edge.clone()
            });
        }
        origins.extend(owned_values.into_iter().map(|origin| (key.clone(), origin)));
    }
    // 모은 origin은 모두 이 몸체의 해시다. 간선마다 origin 목록을 훑지 않고 키로 확인한다.
    let origin_keys: BTreeSet<&OriginKey> = origins.iter().map(|(key, _)| key).collect();
    if captured.iter().any(|key| !origin_keys.contains(key)) {
        return None;
    }
    Some(BodyResult {
        owner: owner.clone(),
        edges,
        origins,
        analysis,
        notes: outcome.notes.to_vec(),
        enriched: outcome.enriched,
    })
}

/// 간선을 origin과 같은 (from, to, kind) 키로 나타낸다.
fn edge_key(edge: &Edge) -> OriginKey {
    (edge.from.clone(), edge.to.clone(), edge.kind)
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
    fn cached_ambiguous_call_candidates_must_exist_in_the_current_graph() {
        let doc: CatalogDocument = serde_json::from_value(serde_json::json!({
            "version":1,"dialect":"postgres","reader":"test","limitations":[],
            "schemas":[{"name":"public","objects":[],"routines":[
                {"name":"caller","kind":"function","language":"sql","body":"SELECT over(1)"},
                {"name":"over","kind":"function","language":"sql","signature":"integer","body":"SELECT 1"},
                {"name":"over","kind":"function","language":"sql","signature":"text","body":"SELECT 2"}
            ]}]
        }))
        .unwrap();
        let mut cache = MemoryCache::default();
        let mut cold = schemagraph_source::graph::document_to_graph(&doc);
        crate::enrich_with_cache(&mut cold, &doc, Some(&mut cache));
        let owner = VertexId::object("public", "caller");
        let candidates = |graph: &Graph| -> Vec<String> {
            graph.analysis()[&owner]
                .diagnostics
                .iter()
                .flat_map(|diagnostic| diagnostic.candidates.iter().map(|id| id.to_string()))
                .collect()
        };
        assert_eq!(
            candidates(&cold),
            ["public.over(integer)", "public.over(text)"]
        );
        // 캐시 결과의 후보 하나를 그래프에 없는 정점으로 바꾸면 복원하지 않고 다시 해석한다.
        cache.0.get_mut(&owner).unwrap().analysis.diagnostics[0].candidates[0] =
            VertexId::from_raw("public.over(ghost)");
        let mut warm = schemagraph_source::graph::document_to_graph(&doc);
        crate::enrich_with_cache(&mut warm, &doc, Some(&mut cache));
        assert_eq!(cache.2, 1);
        assert_eq!(warm.analysis(), cold.analysis());
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

    /// pg_get_functiondef 꼴의 보수 경로 몸체와 AST로 읽히는 신뢰 경로 몸체를
    /// 섞어 캐시 키 계산이 몸체 수에 따라 어떻게 늘어나는지 보게 한다.
    fn mixed_body_document(bodies: usize) -> CatalogDocument {
        let column = serde_json::json!([{"name":"amount","data_type":"INTEGER","nullable":true,"ordinal":1,"pk_position":0}]);
        let mut objects = vec![
            serde_json::json!({"name":"orders","kind":"table","columns":column,"constraints":[],"indexes":[],"triggers":[
                {"name":"touch_orders","body":"CREATE TRIGGER touch_orders AFTER INSERT ON public.orders FOR EACH ROW EXECUTE FUNCTION public.touch_0()"}
            ]}),
            serde_json::json!({"name":"audit","kind":"table","columns":column,"constraints":[],"indexes":[],"triggers":[]}),
        ];
        let mut routines = Vec::new();
        for index in 0..bodies {
            objects.push(serde_json::json!({"name":format!("v_{index}"),"kind":"view","body":"SELECT amount FROM orders","columns":column,"constraints":[],"indexes":[],"triggers":[]}));
            routines.push(serde_json::json!({"name":format!("touch_{index}"),"kind":"function","language":"plpgsql","body":format!(
                "CREATE OR REPLACE FUNCTION public.touch_{index}()\n RETURNS trigger\n LANGUAGE plpgsql\nAS $function$ BEGIN UPDATE audit SET amount = 1; RETURN NEW; END $function$\n"
            )}));
            routines.push(serde_json::json!({"name":format!("copy_{index}"),"kind":"procedure","language":"sql","body":"INSERT INTO audit(amount) SELECT amount FROM orders"}));
        }
        serde_json::from_value(serde_json::json!({
            "version":1,"dialect":"postgres","reader":"test","limitations":[],
            "schemas":[{"name":"public","objects":objects,"routines":routines}]
        }))
        .unwrap()
    }

    #[test]
    fn enrich_without_a_cache_does_not_build_cache_contexts() {
        let doc = mixed_body_document(2);
        crate::scope::take_cache_key_counters();
        let mut plain = schemagraph_source::graph::document_to_graph(&doc);
        let expected = crate::enrich_from_document(&mut plain, &doc);
        assert_eq!(crate::scope::take_cache_key_counters().contexts, 0);

        let mut cache = MemoryCache::default();
        let mut cached = schemagraph_source::graph::document_to_graph(&doc);
        let result = crate::enrich_with_cache(&mut cached, &doc, Some(&mut cache));
        // 뷰 2개, trigger 1개, routine 4개가 각각 context를 한 번씩 만든다.
        assert_eq!(crate::scope::take_cache_key_counters().contexts, 7);
        assert_eq!(result, expected);
        assert_eq!(cached.edges(), plain.edges());
        assert_eq!(cached.analysis(), plain.analysis());
        assert_eq!(cached.origins(), plain.origins());
    }

    #[test]
    fn cache_contexts_walk_the_document_a_bounded_number_of_times() {
        for bodies in [4, 16] {
            let doc = mixed_body_document(bodies);
            let mut cache = MemoryCache::default();
            crate::scope::take_cache_key_counters();
            crate::enrich_with_cache(
                &mut schemagraph_source::graph::document_to_graph(&doc),
                &doc,
                Some(&mut cache),
            );
            let counters = crate::scope::take_cache_key_counters();
            assert_eq!(counters.contexts, 3 * bodies + 1);
            // 문서 지문·후보 색인·routine 모양은 몸체 수와 무관하게 enrich마다
            // 한 번만 만든다.
            assert!(
                counters.document_passes <= 3,
                "{bodies} bodies: {counters:?}"
            );
        }
    }

    /// context 지문별로 저장하는 캐시 — CLI 디스크 캐시의 scoped 키처럼 동작한다.
    #[derive(Default)]
    struct ScopedMemoryCache(BTreeMap<(VertexId, String), BodyResult>);
    impl BodyCache for ScopedMemoryCache {
        fn load(&mut self, _: &VertexId, _: &str) -> Option<BodyResult> {
            None
        }
        fn store(&mut self, _: &VertexId, _: &str, _: &BodyResult) {}
        fn load_scoped(
            &mut self,
            owner: &VertexId,
            _: &str,
            context: &CacheContext,
        ) -> Option<BodyResult> {
            self.0
                .get(&(owner.clone(), context.fingerprint.clone()))
                .cloned()
        }
        fn store_scoped(
            &mut self,
            owner: &VertexId,
            _: &str,
            result: &BodyResult,
            context: &CacheContext,
        ) {
            self.0
                .insert((owner.clone(), context.fingerprint.clone()), result.clone());
        }
    }

    #[test]
    fn sqlite_schema_qualifiers_scope_cached_results_like_the_binder() {
        let mut doc: CatalogDocument = serde_json::from_value(serde_json::json!({
            "version":1,"dialect":"sqlite","reader":"test","limitations":[],
            "schemas":[{"name":"main","routines":[],"objects":[
                {"name":"t","kind":"table","columns":[{"name":"id","data_type":"INTEGER","nullable":true,"ordinal":1,"pk_position":0}],"constraints":[],"indexes":[],"triggers":[]},
                {"name":"u","kind":"table","columns":[{"name":"other","data_type":"INTEGER","nullable":true,"ordinal":1,"pk_position":0}],"constraints":[],"indexes":[],"triggers":[]},
                {"name":"v","kind":"view","body":"SELECT id FROM MAIN.t, MAIN.u","columns":[{"name":"id","data_type":"INTEGER","nullable":true,"ordinal":1,"pk_position":0}],"constraints":[],"indexes":[],"triggers":[]}
            ]}]
        }))
        .unwrap();
        let owner = VertexId::object("main", "v");
        let body = "SELECT id FROM MAIN.t, MAIN.u";
        let graph = schemagraph_source::graph::document_to_graph(&doc);
        let first = crate::scope::cache_context(&doc, &graph, &owner, body);
        let mut cache = ScopedMemoryCache::default();
        crate::enrich_with_cache(
            &mut schemagraph_source::graph::document_to_graph(&doc),
            &doc,
            Some(&mut cache),
        );

        // MAIN.u에 id가 생기면 뷰의 id는 모호해진다. Binder가 해석한 main.u의
        // 모양이 지문에 들어 있어야 낡은 complete 결과를 복원하지 않는다.
        doc.schemas[0].objects[1].columns.push(
            serde_json::from_value(serde_json::json!({"name":"id","data_type":"INTEGER","nullable":true,"ordinal":2,"pk_position":0}))
                .unwrap(),
        );
        let graph = schemagraph_source::graph::document_to_graph(&doc);
        let changed = crate::scope::cache_context(&doc, &graph, &owner, body);
        assert!(first.trusted && changed.trusted);
        assert_ne!(first.fingerprint, changed.fingerprint);
        let mut expected = schemagraph_source::graph::document_to_graph(&doc);
        crate::enrich_from_document(&mut expected, &doc);
        let mut warm = schemagraph_source::graph::document_to_graph(&doc);
        crate::enrich_with_cache(&mut warm, &doc, Some(&mut cache));
        assert!(!expected
            .edges()
            .iter()
            .any(|edge| edge.kind == EdgeKind::DerivesFrom));
        assert_eq!(warm.edges(), expected.edges());
        assert_eq!(warm.analysis(), expected.analysis());
    }

    #[test]
    fn conservative_context_keeps_its_fingerprint_and_tracks_unrelated_changes() {
        use sha2::{Digest, Sha256};

        let mut doc = mixed_body_document(1);
        let owner = VertexId::routine("public", None, "touch_0", None);
        let body = doc.schemas[0].routines[0].body.clone().unwrap();
        let graph = schemagraph_source::graph::document_to_graph(&doc);
        let context = crate::scope::cache_context(&doc, &graph, &owner, &body);
        assert!(!context.trusted);
        // 외부 BodyCache에 저장된 보수 지문을 다시 찾을 수 있게 값 정의를 유지한다.
        let legacy =
            Sha256::digest(format!("global\0{:?}\0{:?}", doc, graph.vertex(&owner)).as_bytes());
        assert_eq!(context.fingerprint, format!("sha256:{legacy:x}"));

        doc.schemas[0].objects.push(
            serde_json::from_value(serde_json::json!({
                "name":"unrelated","kind":"table","columns":[],"constraints":[],"indexes":[],"triggers":[]
            }))
            .unwrap(),
        );
        let graph = schemagraph_source::graph::document_to_graph(&doc);
        let changed = crate::scope::cache_context(&doc, &graph, &owner, &body);
        assert_ne!(context.fingerprint, changed.fingerprint);
    }

    /// 같은 대상 컬럼에 값 계보를 남기는 DML routine이 `routines`개인 문서.
    ///
    /// 몸체는 서로 달라 원문 해시가 다르지만 `audit.amount ← orders.amount` 계보 키는
    /// 모든 routine이 공유한다.
    fn dml_routine_document(routines: usize) -> CatalogDocument {
        let column = serde_json::json!([
            {"name":"id","data_type":"INTEGER","nullable":false,"ordinal":1,"pk_position":1},
            {"name":"amount","data_type":"INTEGER","nullable":true,"ordinal":2,"pk_position":0},
            {"name":"note","data_type":"TEXT","nullable":true,"ordinal":3,"pk_position":0}
        ]);
        let objects: Vec<_> = ["orders", "audit", "archive"]
            .into_iter()
            .map(|name| serde_json::json!({"name":name,"kind":"table","columns":column,"constraints":[],"indexes":[],"triggers":[]}))
            .collect();
        let mut bodies = Vec::new();
        for index in 0..routines {
            bodies.push(serde_json::json!({"name":format!("copy_{index}"),"kind":"procedure","language":"sql","body":format!(
                "INSERT INTO audit(amount, note) SELECT amount, note FROM orders WHERE id > {index}"
            )}));
            bodies.push(serde_json::json!({"name":format!("move_{index}"),"kind":"function","language":"plpgsql","body":format!(
                "BEGIN UPDATE archive SET amount = o.amount FROM orders o WHERE archive.id = o.id AND o.id > {index}; \
                 INSERT INTO audit(amount) SELECT amount FROM archive WHERE note = 'x'; END"
            )}));
        }
        serde_json::from_value(serde_json::json!({
            "version":1,"dialect":"postgres","reader":"test","limitations":[],
            "schemas":[{"name":"public","objects":objects,"routines":bodies}]
        }))
        .unwrap()
    }

    /// owner의 DML 분석이 origin을 단 간선 키 수.
    fn dml_origin_keys(graph: &Graph, owner: &VertexId) -> usize {
        let prefix = format!("dml-owner:{owner}:");
        graph
            .origins()
            .values()
            .filter(|values| values.iter().any(|origin| origin.role.starts_with(&prefix)))
            .count()
    }

    #[test]
    fn capture_examines_only_the_saved_bodys_dml_origin_keys() {
        for routines in [4, 16] {
            let doc = dml_routine_document(routines);
            let mut cache = MemoryCache::default();
            let mut graph = schemagraph_source::graph::document_to_graph(&doc);
            take_captured_keys();
            crate::enrich_with_cache(&mut graph, &doc, Some(&mut cache));
            let examined = take_captured_keys();
            // 몸체를 저장할 때마다 그래프의 모든 origin을 훑으면 몸체 수의 제곱으로 는다.
            // 저장하는 몸체가 만든 DML origin 키만 보면 몸체 수에 비례한다.
            let expected: usize = cache
                .0
                .keys()
                .map(|owner| dml_origin_keys(&graph, owner))
                .sum();
            assert_eq!(cache.0.len(), 2 * routines);
            assert!(expected >= 4 * routines, "{routines}: {expected}");
            assert_eq!(examined, expected, "{routines} routines");
        }
    }

    #[test]
    fn capture_matches_the_whole_graph_origin_scan_it_replaced() {
        for doc in [dml_routine_document(3), fixture()] {
            let mut cache = MemoryCache::default();
            let mut graph = schemagraph_source::graph::document_to_graph(&doc);
            crate::enrich_with_cache(&mut graph, &doc, Some(&mut cache));
            assert!(!cache.0.is_empty());
            // 이 문서들은 뒤 몸체가 앞 owner의 간선을 바꾸지 않으므로, 끝난 그래프에서
            // 옛 방식으로 다시 모은 결과가 저장 때 모은 결과와 같아야 한다.
            for (owner, saved) in &cache.0 {
                let legacy = legacy_capture(&graph, owner, &saved.notes, saved.enriched)
                    .expect("legacy capture of a saved body");
                assert_eq!(saved.edges, legacy.edges, "{owner}");
                assert_eq!(saved.origins, legacy.origins, "{owner}");
                assert_eq!(saved.analysis, legacy.analysis, "{owner}");
            }
        }
    }

    /// 컬럼 `columns`개를 모두 투영하는 넓은 뷰 하나와 그 원본 테이블.
    fn wide_view_document(columns: usize) -> CatalogDocument {
        let column = |index: usize| serde_json::json!({"name":format!("c{index}"),"data_type":"INTEGER","nullable":true,"ordinal":index + 1,"pk_position":0});
        let shape: Vec<_> = (0..columns).map(column).collect();
        let projection: Vec<_> = (0..columns).map(|index| format!("c{index}")).collect();
        serde_json::from_value(serde_json::json!({
            "version":1,"dialect":"postgres","reader":"test","limitations":[],
            "schemas":[{"name":"public","routines":[],"objects":[
                {"name":"t","kind":"table","columns":shape,"constraints":[],"indexes":[],"triggers":[]},
                {"name":"v","kind":"view","body":format!("SELECT {} FROM t", projection.join(", ")),"columns":shape,"constraints":[],"indexes":[],"triggers":[]}
            ]}]
        }))
        .unwrap()
    }

    #[test]
    fn restore_checks_each_cached_item_a_bounded_number_of_times() {
        for doc in [wide_view_document(64), dml_routine_document(4)] {
            let mut cache = MemoryCache::default();
            let mut cold = schemagraph_source::graph::document_to_graph(&doc);
            let expected = crate::enrich_with_cache(&mut cold, &doc, Some(&mut cache));
            let items: usize = cache
                .0
                .values()
                .map(|result| result.edges.len() + result.origins.len())
                .sum();
            take_restore_visits();
            let mut warm = schemagraph_source::graph::document_to_graph(&doc);
            let result = crate::enrich_with_cache(&mut warm, &doc, Some(&mut cache));
            let visits = take_restore_visits();
            // 모든 몸체가 hit로 복원돼야 이 계수가 restore의 작업량이다.
            assert_eq!((cache.1, cache.2), (cache.0.len(), 0));
            assert_eq!(result, expected);
            assert_eq!(warm.edges(), cold.edges());
            assert_eq!(warm.origins(), cold.origins());
            // 간선마다 origin 목록을, origin마다 간선 목록을 훑으면 넓은 몸체에서 항목
            // 수의 제곱이 된다. 집합을 한 번 만들어 조회하면 항목마다 몇 번만 본다.
            assert!(
                visits <= 3 * items,
                "{visits} visits for {items} cached items"
            );
        }
    }

    #[test]
    fn restore_accepts_exactly_what_the_pairwise_checks_accepted() {
        for doc in [dml_routine_document(2), fixture(), wide_view_document(3)] {
            let mut cache = MemoryCache::default();
            crate::enrich_with_cache(
                &mut schemagraph_source::graph::document_to_graph(&doc),
                &doc,
                Some(&mut cache),
            );
            let graph = schemagraph_source::graph::document_to_graph(&doc);
            for (owner, saved) in &cache.0 {
                let hash = saved.analysis.body_hash.clone().unwrap();
                let verdicts: Vec<bool> = mutations(saved)
                    .iter()
                    .map(|cached| {
                        let accepted =
                            CachedKeys::new(&graph, owner, &hash, cached).accept(&graph, cached);
                        let legacy = legacy_accepts(&graph, owner, &hash, cached);
                        assert_eq!(accepted, legacy, "{owner}: {cached:?}");
                        accepted
                    })
                    .collect();
                // 원래 결과는 받고 망가뜨린 변형 일부는 거부해야 비교가 의미 있다.
                assert!(verdicts[0], "{owner}");
                assert!(verdicts.iter().any(|accepted| !accepted), "{owner}");
            }
        }
    }

    /// 저장된 결과와, 간선·origin을 하나씩 망가뜨린 변형들.
    fn mutations(saved: &BodyResult) -> Vec<BodyResult> {
        let mut variants = vec![saved.clone()];
        let edge_changes: [fn(&mut Edge); 6] = [
            |edge| edge.to = VertexId::from_raw("missing"),
            |edge| std::mem::swap(&mut edge.from, &mut edge.to),
            |edge| edge.kind = EdgeKind::Contains,
            |edge| edge.kind = EdgeKind::DerivesFrom,
            |edge| edge.evidence.clear(),
            |edge| edge.evidence[0].layer = EvidenceLayer::Catalog,
        ];
        for index in 0..saved.edges.len() {
            for change in edge_changes {
                let mut variant = saved.clone();
                change(&mut variant.edges[index]);
                variants.push(variant);
            }
            let mut variant = saved.clone();
            variant.edges.remove(index);
            variants.push(variant);
        }
        let origin_changes: [fn(&mut (OriginKey, Origin)); 3] = [
            |(_, origin)| origin.body_hash = "sha256:other".into(),
            |(_, origin)| origin.role = "relation".into(),
            |(key, _)| key.2 = EdgeKind::Reads,
        ];
        for index in 0..saved.origins.len() {
            for change in origin_changes {
                let mut variant = saved.clone();
                change(&mut variant.origins[index]);
                variants.push(variant);
            }
            let mut variant = saved.clone();
            variant.origins.remove(index);
            variants.push(variant);
        }
        variants
    }

    /// 간선마다 origin 목록을, origin마다 간선 목록을 훑던 restore의 거부 조건.
    /// 집합 조회로 바꾼 조건이 같은 결과를 받아들이는지 비교하려고 남긴다.
    fn legacy_accepts(graph: &Graph, owner: &VertexId, hash: &str, cached: &BodyResult) -> bool {
        let owns = |id: &VertexId| {
            id == owner
                || (graph
                    .vertex(id)
                    .is_some_and(|v| v.kind == VertexKind::Column)
                    && graph
                        .outgoing(owner)
                        .iter()
                        .any(|e| e.kind == EdgeKind::Contains && &e.to == id))
        };
        let rejected = cached.edges.iter().any(|e| {
            !owns(&e.from) && !legacy_dml_effect_owned(graph, cached, owner, e)
                || graph.vertex(&e.from).is_none()
                || (!owns(&e.from)
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
        }) || cached.origins.iter().any(|(key, origin)| {
            origin.body_hash != hash
                || !cached
                    .edges
                    .iter()
                    .any(|e| (&e.from, &e.to, e.kind) == (&key.0, &key.1, key.2))
        });
        !rejected
    }

    fn legacy_dml_effect_owned(
        graph: &Graph,
        cached: &BodyResult,
        owner: &VertexId,
        edge: &Edge,
    ) -> bool {
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
            candidate.from == *owner
                && candidate.to == edge.from
                && candidate.kind == EdgeKind::Writes
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

    /// 몸체마다 그래프의 모든 origin을 훑던 capture. 결과가 같은지 비교하려고 남긴다.
    fn legacy_capture(
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
}
