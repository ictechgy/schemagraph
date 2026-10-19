//! DB 참조 행을 reader가 만든 실제 정점으로 해석한다. DB의 종류 코드를 추측하지 않는다.

use std::collections::BTreeMap;

use crate::document::{CatalogDocument, CatalogObjectRef};
use schemagraph_core::{
    Edge, EdgeKind, Evidence, EvidenceLayer, Graph, Vertex, VertexId, VertexKind,
};

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
///
/// 부를 때마다 참조의 스키마 자식을 처음부터 훑는다. 같은 그래프에서 참조를 여럿 풀면
/// [`ReferenceIndex`] 하나를 만들어 재사용한다.
pub fn resolve_reference(
    graph: &Graph,
    doc: &CatalogDocument,
    reference: &CatalogObjectRef,
) -> Option<VertexId> {
    ReferenceIndex::new(graph).resolve(doc, reference)
}

/// 이름순으로 정렬한 정점들. 같은 이름은 붙어 있어 이분 탐색으로 그 구간을 찾는다.
///
/// 이름마다 맵 항목과 목록을 따로 만들지 않아, 컬럼을 참조하는 객체가 많아도 색인이 정점
/// 참조 하나씩만 든다.
struct VerticesByName<'g>(Vec<&'g Vertex>);

impl<'g> VerticesByName<'g> {
    /// 같은 이름의 정점은 모두 남겨 모호함을 해석 단계가 판정하게 한다.
    fn new(vertices: impl Iterator<Item = &'g Vertex>) -> Self {
        let mut vertices: Vec<_> = vertices.collect();
        vertices.sort_unstable_by(|a, b| a.name.cmp(&b.name));
        Self(vertices)
    }

    /// 이름이 `name`인 정점들.
    fn named(&self, name: &str) -> &[&'g Vertex] {
        let start = self.0.partition_point(|vertex| vertex.name.as_str() < name);
        let len = self.0[start..].partition_point(|vertex| vertex.name == name);
        &self.0[start..start + len]
    }
}

/// 같은 그래프에서 카탈로그 참조를 여러 번 해석할 때 쓰는 색인.
///
/// 참조마다 스키마의 Contains 자식 전체를 훑으면 의존성 행 수 × 스키마 객체 수가 된다.
/// pg_depend처럼 뷰가 읽는 컬럼마다 행을 내는 카탈로그에서는 scan·review·diff·merge가 모두
/// 이차 시간이 된다. 색인은 처음 묻는 스키마의 직속 자식과 처음 묻는 객체의 컬럼을 이름별로
/// 한 번만 모으고, 그 뒤로는 조회만 한다. 모은 후보에 참조마다 훑던 때와 같은 조건을 걸므로
/// 답이 같다. 그래프를 빌려 쓰므로 색인을 쓰는 동안 그래프가 바뀌지 않는다.
pub struct ReferenceIndex<'g> {
    graph: &'g Graph,
    /// 스키마 id → 그 스키마 직속 정점(이름순).
    objects: BTreeMap<VertexId, VerticesByName<'g>>,
    /// 객체 id → 그 객체가 담은 컬럼 정점(이름순).
    columns: BTreeMap<&'g VertexId, VerticesByName<'g>>,
}

impl<'g> ReferenceIndex<'g> {
    /// 아직 아무것도 훑지 않은 색인. 해석하지 않는 그래프에는 비용이 없다.
    pub fn new(graph: &'g Graph) -> Self {
        Self {
            graph,
            objects: BTreeMap::new(),
            columns: BTreeMap::new(),
        }
    }

    /// 참조를 reader가 만든 실제 정점 id로 푼다.
    ///
    /// 다른 DB의 참조, 조건에 맞는 후보가 없거나 둘 이상인 참조, 멤버 컬럼이 하나로 정해지지
    /// 않는 참조는 None이다 — 추측으로 정점을 고르지 않는다.
    pub fn resolve(
        &mut self,
        doc: &CatalogDocument,
        reference: &CatalogObjectRef,
    ) -> Option<VertexId> {
        if !same_database(doc, reference) {
            return None;
        }
        let object = self.object(reference)?;
        match &reference.member {
            Some(member) => self.column(object, member),
            None => Some(object.id.clone()),
        }
    }

    /// 참조의 schema·name·kind·signature를 만족하는 유일한 스키마 직속 정점.
    fn object(&mut self, reference: &CatalogObjectRef) -> Option<&'g Vertex> {
        let graph = self.graph;
        let named = self
            .objects
            .entry(VertexId::schema(&reference.schema))
            .or_insert_with_key(|schema| schema_children(graph, schema))
            .named(&reference.name);
        let expected = reference.signature.as_ref().map(|signature| {
            VertexId::routine(&reference.schema, None, &reference.name, Some(signature))
        });
        single(named.iter().copied().filter(|vertex| {
            vertex.schema == reference.schema
                && kind_matches(vertex.kind, reference.kind.as_deref())
                && expected
                    .as_ref()
                    .is_none_or(|expected| signature_matches(&vertex.id, expected))
        }))
    }

    /// `object`가 담은 컬럼 중 이름이 `member`인 유일한 정점의 id.
    fn column(&mut self, object: &'g Vertex, member: &str) -> Option<VertexId> {
        let graph = self.graph;
        let named = self
            .columns
            .entry(&object.id)
            .or_insert_with(|| object_columns(graph, &object.id))
            .named(member);
        single(named.iter()).map(|column| column.id.clone())
    }
}

/// 참조가 DB를 밝혔으면 문서가 수집한 DB와 같을 때만 로컬에서 찾는다.
fn same_database(doc: &CatalogDocument, reference: &CatalogObjectRef) -> bool {
    reference.database.as_ref().is_none_or(|database| {
        doc.context.as_ref().and_then(|c| c.database.as_ref()) == Some(database)
    })
}

/// 스키마 직속 정점을 이름별로 모은다.
///
/// 카탈로그 object 이름은 schema 직속이다. 같은 이름의 패키지 멤버나 부모 미수집
/// fallback 정점(`schema.pkg.member`)을 대신 고르지 않도록 id의 부모가 스키마인 정점만 둔다.
fn schema_children<'g>(graph: &'g Graph, schema: &VertexId) -> VerticesByName<'g> {
    VerticesByName::new(
        contained(graph, schema).filter(|vertex| vertex.id.parent().as_ref() == Some(schema)),
    )
}

/// 객체가 담은 컬럼 정점을 이름별로 모은다.
fn object_columns<'g>(graph: &'g Graph, object: &VertexId) -> VerticesByName<'g> {
    VerticesByName::new(contained(graph, object).filter(|vertex| vertex.kind == VertexKind::Column))
}

/// `parent`의 Contains 간선이 가리키는, 그래프에 있는 정점들.
fn contained<'g>(graph: &'g Graph, parent: &VertexId) -> impl Iterator<Item = &'g Vertex> + 'g {
    let edges = graph.outgoing(parent);
    #[cfg(test)]
    work::scanned(edges.len());
    edges
        .iter()
        .filter(|edge| edge.kind == EdgeKind::Contains)
        .filter_map(move |edge| graph.vertex(&edge.to))
}

/// 하나뿐일 때만 고른다. 없거나 둘 이상이면 어느 쪽도 추측하지 않는다.
fn single<T>(mut items: impl Iterator<Item = T>) -> Option<T> {
    let first = items.next()?;
    items.next().is_none().then_some(first)
}

/// signature 참조는 그 오버로드의 정점, 또는 이름 충돌로 `@kind`가 붙은 그 정점만 받는다.
fn signature_matches(id: &VertexId, expected: &VertexId) -> bool {
    id == expected
        || id
            .as_str()
            .strip_prefix(expected.as_str())
            .is_some_and(|rest| rest.starts_with('@') && !rest[1..].contains('.'))
}

/// 카탈로그 증거는 SQL 분석 근거를 덮어쓰지 않고 함께 보존한다.
///
/// 색인이 그래프를 빌려 쓰므로 모든 행을 먼저 해석하고 간선은 그 뒤에 넣는다. DependsOn
/// 간선과 limitation은 해석에 쓰는 정점·Contains 간선을 바꾸지 않아서, 행마다 해석하고 바로
/// 넣던 때와 간선·limitation이 같은 순서로 쌓인다.
pub(crate) fn apply(graph: &mut Graph, doc: &CatalogDocument) {
    let mut dependencies = doc.dependencies.clone();
    dependencies.sort();
    dependencies.dedup();
    let mut references = ReferenceIndex::new(graph);
    let resolved: Vec<_> = dependencies
        .iter()
        .map(|dependency| {
            let from = references.resolve(doc, &dependency.source);
            (from, references.resolve(doc, &dependency.target))
        })
        .collect();
    for (dependency, (from, to)) in dependencies.into_iter().zip(resolved) {
        match (from,to) {
            (Some(from),Some(to))=>graph.add_edge(Edge {from,to,kind:EdgeKind::DependsOn,evidence:vec![Evidence {layer:EvidenceLayer::Catalog,detail:format!("{} dependency type {}",dependency.catalog,dependency.dependency_type)}]}),
            _=>graph.add_limitation(format!("{} dependency {}.{} -> {}.{} could not be resolved to collected vertices; no target inferred",dependency.catalog,dependency.source.schema,dependency.source.name,dependency.target.schema,dependency.target.name)),
        }
    }
}

/// 테스트가 참조 해석 작업량을 시간 대신 횟수로 확인하게 한다.
///
/// 이차 시간 회귀는 작은 입력에서 시간으로 잡히지 않고, 큰 입력의 시간 단언은 공유
/// 기계에서 불안정하다. 테스트마다 스레드가 따로라 thread_local이면 서로 섞이지 않는다.
#[cfg(test)]
pub(crate) mod work {
    use std::cell::Cell;

    thread_local! {
        static SCANNED: Cell<usize> = const { Cell::new(0) };
    }

    /// 참조 해석이 훑은 인접 간선 수를 더한다.
    pub(crate) fn scanned(count: usize) {
        SCANNED.with(|scanned| scanned.set(scanned.get() + count));
    }

    /// 지금까지 센 인접 간선 탐색량을 돌려주고 0으로 되돌린다.
    pub(crate) fn take_scanned() -> usize {
        SCANNED.with(Cell::take)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// 테이블(컬럼 8개)과 몸체 없는 뷰. 뷰마다 컬럼 단위 행 8개와 관계 단위 행 1개를 싣는다 —
    /// pg_depend가 뷰가 읽는 컬럼마다 행을 내는 모양이다.
    fn wide_document(tables: usize, views: usize) -> CatalogDocument {
        let columns: Vec<_> = (0..8)
            .map(|c| json!({"name":format!("c{c}"),"data_type":"integer","nullable":true,"ordinal":c + 1,"pk_position":0}))
            .collect();
        let objects: Vec<_> = (0..tables)
            .map(|t| json!({"name":format!("t{t}"),"kind":"table","columns":columns,"constraints":[],"indexes":[],"triggers":[]}))
            .chain((0..views).map(|v| json!({"name":format!("v{v}"),"kind":"view","columns":[],"constraints":[],"indexes":[],"triggers":[]})))
            .collect();
        let row = |view: usize, table: usize, member: Option<String>| {
            let mut target = json!({"schema":"app","name":format!("t{table}"),"kind":"table"});
            if let Some(member) = member {
                target["member"] = json!(member);
            }
            json!({"source":{"schema":"app","name":format!("v{view}"),"kind":"view"},"target":target,"catalog":"pg_depend","dependency_type":"n"})
        };
        let mut dependencies = Vec::new();
        for view in 0..views {
            for column in 0..8 {
                dependencies.push(row(
                    view,
                    (view * 7 + column) % tables,
                    Some(format!("c{column}")),
                ));
            }
            dependencies.push(row(view, view % tables, None));
        }
        serde_json::from_value(json!({
            "version":1,"reader":"test","dialect":"postgres","limitations":[],
            "context":{"source_id":"app","database":"db","catalog_complete":true},
            "schemas":[{"name":"app","objects":objects,"routines":[]}],
            "dependencies":dependencies
        }))
        .expect("test document is valid")
    }

    /// 색인 도입 전의 해석을 그대로 옮긴 기준 — 참조마다 스키마와 객체의 자식을 훑는다.
    fn scanning_resolve(
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
                    && v.id.parent().as_ref() == Some(&schema_id)
                    && kind_matches(v.kind, reference.kind.as_deref())
            })
            .collect();
        if let Some(signature) = &reference.signature {
            let expected =
                VertexId::routine(&reference.schema, None, &reference.name, Some(signature));
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

    /// 이름 충돌(`@kind`), overload, 패키지 멤버, 부모 없는 멤버, 멤버 이름 충돌, escape가
    /// 필요한 이름, 같은 이름의 다른 스키마를 한 문서에 섞는다.
    fn colliding_document() -> CatalogDocument {
        let column = |name: &str| json!({"name":name,"data_type":"integer","nullable":true,"ordinal":1,"pk_position":0});
        let object = |name: &str, kind: &str, columns: Vec<serde_json::Value>| json!({"name":name,"kind":kind,"columns":columns,"constraints":[],"indexes":[],"triggers":[]});
        let mut indexed = object("u", "table", vec![column("id"), column("c")]);
        indexed["indexes"] = json!([{"name":"id","unique":false,"columns":["id"]}]);
        let routine = |name: &str, kind: &str, signature: Option<&str>, member_of: Option<&str>| {
            let mut routine = json!({"name":name,"kind":kind,"language":"sql"});
            if let Some(signature) = signature {
                routine["signature"] = json!(signature);
            }
            if let Some(member_of) = member_of {
                routine["member_of"] = json!(member_of);
            }
            routine
        };
        serde_json::from_value(json!({
            "version":1,"reader":"test","dialect":"oracle","limitations":[],
            "context":{"source_id":"app","database":"db","catalog_complete":true},
            "schemas":[
                {"name":"s","objects":[
                    object("t", "table", vec![column("c"), column("d")]),
                    object("f", "table", vec![column("c")]),
                    object("v", "view", vec![column("c")]),
                    object("mv", "materialized-view", vec![]),
                    object("seq", "sequence", vec![]),
                    object("ty", "type", vec![]),
                    object("syn", "synonym", vec![]),
                    object("a.b", "table", vec![column("x@y")]),
                    indexed
                ],"routines":[
                    routine("f", "function", Some("integer"), None),
                    routine("f", "function", Some("text"), None),
                    routine("f", "function", None, None),
                    routine("f", "procedure", None, None),
                    routine("pkg", "package", None, None),
                    routine("m", "function", None, Some("pkg")),
                    routine("m", "function", None, None),
                    routine("x", "procedure", None, Some("missing")),
                    routine("t", "function", Some("integer"), None)
                ]},
                {"name":"other","objects":[object("t", "table", vec![column("c")])],"routines":[]},
                {"name":"my.schema","objects":[object("t", "view", vec![column("c")])],"routines":[]}
            ]
        }))
        .expect("test document is valid")
    }

    #[test]
    fn 색인_해석은_참조마다_훑던_해석과_같은_정점을_고른다() {
        let doc = colliding_document();
        let graph = crate::graph::document_to_graph(&doc);
        let mut index = ReferenceIndex::new(&graph);
        let names = [
            "t", "f", "v", "mv", "seq", "ty", "syn", "a.b", "u", "pkg", "m", "x", "zz",
        ];
        let kinds = [
            None,
            Some("table"),
            Some("view"),
            Some("materialized-view"),
            Some("sequence"),
            Some("type"),
            Some("synonym"),
            Some("function"),
            Some("procedure"),
            Some("package"),
            Some("bogus"),
        ];
        let (mut resolved, mut total) = (0, 0);
        for schema in ["s", "other", "my.schema", "absent"] {
            for name in names {
                for kind in kinds {
                    for signature in [None, Some("integer"), Some("text"), Some("bogus")] {
                        for member in [None, Some("c"), Some("id"), Some("x@y"), Some("zz")] {
                            for database in [None, Some("db"), Some("other-db")] {
                                let reference = CatalogObjectRef {
                                    schema: schema.into(),
                                    name: name.into(),
                                    kind: kind.map(str::to_owned),
                                    member: member.map(str::to_owned),
                                    signature: signature.map(str::to_owned),
                                    database: database.map(str::to_owned),
                                };
                                let expected = scanning_resolve(&graph, &doc, &reference);
                                assert_eq!(
                                    index.resolve(&doc, &reference),
                                    expected,
                                    "{reference:?}"
                                );
                                assert_eq!(resolve_reference(&graph, &doc, &reference), expected);
                                total += 1;
                                resolved += usize::from(expected.is_some());
                            }
                        }
                    }
                }
            }
        }
        // 비교가 실패 경로만 지나지 않았는지: 객체·routine·컬럼이 실제로 풀린 경우가 섞여 있다.
        assert!(
            resolved >= 50 && resolved < total / 4,
            "{resolved} of {total}"
        );
        let resolve = |schema: &str,
                       name: &str,
                       kind: &str,
                       signature: Option<&str>,
                       member: Option<&str>| {
            let reference = CatalogObjectRef {
                schema: schema.into(),
                name: name.into(),
                kind: Some(kind.into()),
                member: member.map(str::to_owned),
                signature: signature.map(str::to_owned),
                database: None,
            };
            ReferenceIndex::new(&graph)
                .resolve(&doc, &reference)
                .map(|id| id.as_str().to_owned())
        };
        assert_eq!(
            resolve("s", "f", "function", Some("integer"), None).as_deref(),
            Some("s.f(integer)")
        );
        assert_eq!(
            resolve("s", "f", "procedure", None, None).as_deref(),
            Some("s.f@procedure")
        );
        assert_eq!(
            resolve("s", "f", "function", None, None),
            None,
            "three function candidates"
        );
        assert_eq!(
            resolve("s", "t", "table", None, Some("c")).as_deref(),
            Some("s.t.c")
        );
        assert_eq!(
            resolve("s", "u", "table", None, Some("id")).as_deref(),
            Some("s.u.id")
        );
        assert_eq!(
            resolve("s", "m", "function", None, None).as_deref(),
            Some("s.m")
        );
        assert_eq!(
            resolve("s", "x", "procedure", None, None),
            None,
            "fallback member"
        );
        assert_eq!(
            resolve("my.schema", "t", "view", None, Some("c")).as_deref(),
            Some("my%2Eschema.t.c")
        );
    }

    #[test]
    fn 후보가_둘인_의존성은_간선_대신_limitation으로_남는다() {
        let mut doc = colliding_document();
        let reference = |name: &str, kind: Option<&str>| CatalogObjectRef {
            schema: "s".into(),
            name: name.into(),
            kind: kind.map(str::to_owned),
            member: None,
            signature: None,
            database: None,
        };
        // kind 없는 `t`는 테이블 s.t와 함수 s.t(integer) 두 정점에 맞는다.
        doc.dependencies = vec![
            dependency(reference("v", Some("view")), reference("t", None)),
            dependency(reference("v", Some("view")), reference("mv", None)),
        ];
        let graph = crate::graph::document_to_graph(&doc);
        let depends_on: Vec<_> = graph
            .edges()
            .into_iter()
            .filter(|edge| edge.kind == EdgeKind::DependsOn)
            .map(|edge| (edge.from.as_str().to_owned(), edge.to.as_str().to_owned()))
            .collect();
        assert_eq!(depends_on, [("s.v".to_owned(), "s.mv".to_owned())]);
        assert!(graph.limitations().iter().any(|note| note
            == "pg_depend dependency s.v -> s.t could not be resolved to collected vertices; no target inferred"));
    }

    fn dependency(
        source: CatalogObjectRef,
        target: CatalogObjectRef,
    ) -> crate::document::CatalogDependency {
        crate::document::CatalogDependency {
            source,
            target,
            catalog: "pg_depend".into(),
            dependency_type: "n".into(),
        }
    }

    #[test]
    fn 의존성_행이_많아도_스키마와_객체의_인접_간선은_한_번씩만_훑는다() {
        let doc = wide_document(60, 20);
        work::take_scanned();
        let graph = crate::graph::document_to_graph(&doc);
        let scanned = work::take_scanned();
        let depends_on = graph
            .edges()
            .iter()
            .filter(|edge| edge.kind == EdgeKind::DependsOn)
            .count();
        assert_eq!(depends_on, 20 * 9, "{:?}", graph.limitations());
        // 행마다 스키마 자식 전체를 다시 훑으면 행 수 × 스키마 객체 수가 된다.
        let edges = graph.edges().len();
        assert!(
            scanned <= edges,
            "resolution scanned {scanned} adjacency edges for a graph of {edges} edges"
        );
    }
}
