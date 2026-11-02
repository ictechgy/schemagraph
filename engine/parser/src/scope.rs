//! 스코프마다 FROM·CTE·출력 컬럼을 분리해 별칭이 다른 질의로 새지 않게 한다.

use std::cell::{OnceCell, RefCell};
use std::collections::{BTreeMap, BTreeSet};
use std::ops::ControlFlow;
use std::rc::Rc;

use schemagraph_core::{
    AnalysisState, Diagnostic, Edge, EdgeKind, Evidence, EvidenceLayer, Graph, ObjectAnalysis,
    Origin, OriginKey, SourceLocation, VertexId, VertexKind,
};
use schemagraph_source::document::{CatalogDocument, ObjectDoc, RoutineDoc};
use sha2::{Digest, Sha256};
use sqlparser::ast::{
    visit_relations, AccessExpr, AssignmentTarget, Expr, GroupByExpr, Ident, JoinConstraint,
    JoinOperator, MergeAction, MergeInsertKind, NamedWindowExpr, ObjectName, ObjectNamePart, Query,
    Select, SelectItem, SelectItemQualifiedWildcardKind, SetExpr, Spanned, Statement, TableAlias,
    TableFactor, TableObject, TableWithJoins, UpdateTableFromKind, Value, Visit, Visitor,
    WildcardAdditionalOptions, WindowType,
};
use sqlparser::dialect::Dialect;

use crate::cache::CacheContext;

type Sources = BTreeSet<VertexId>;
type Ctes = BTreeMap<String, Relation>;
type NamedWindows = BTreeMap<String, NamedWindowExpr>;

#[derive(Clone, Copy)]
enum UsingPreservation {
    Inner,
    Left,
    Right,
    Full,
}

#[derive(Clone, Default)]
struct Column {
    name: String,
    sources: Sources,
    unknown: bool,
    location: Option<SourceLocation>,
}

#[derive(Clone, Default)]
struct Relation {
    aliases: Vec<Vec<String>>,
    columns: Vec<Column>,
    unknown: bool,
}

#[derive(Clone, Default)]
struct Scope {
    relations: Vec<Relation>,
    columns: Vec<Column>,
    outer: Option<Rc<Scope>>,
    named_windows: NamedWindows,
}

impl Scope {
    fn add(&mut self, relation: Relation) {
        self.columns.extend(relation.columns.clone());
        self.relations.push(relation);
    }
}

/// 객체 검색을 몸체마다 전체 카탈로그 순회로 되풀이하지 않는다.
///
/// enrich 한 번이 이 색인 하나를 공유하므로 캐시 키의 문서 단위 값과 캐시 저장이
/// 쓰는 몸체별 DML origin 키도 여기에 둔다.
pub(crate) struct CatalogIndex<'a> {
    tables: BTreeMap<(String, String), Vec<(&'a str, &'a ObjectDoc)>>,
    dialect: &'a str,
    routines: BTreeSet<(String, String)>,
    database: Option<String>,
    doc: &'a CatalogDocument,
    cache_keys: CacheKeyMemo<'a>,
    /// enrich가 다루는 그래프 하나의 이름 해석 색인. 정점 집합이 고정이라
    /// 처음 조회할 때 한 번만 만든다.
    vertices: super::VertexIndex,
    /// DML 분석이 owner별로 origin을 단 간선 키. 캐시 저장이 owner 밖의 컬럼에서
    /// 시작하는 값 계보를 그래프의 모든 origin을 훑지 않고 찾게 한다. 몸체를 저장할
    /// 때 꺼내 비우므로 지금 해석 중인 몸체의 키만 들고 있다.
    dml_origins: RefCell<BTreeMap<VertexId, BTreeSet<OriginKey>>>,
}

impl<'a> CatalogIndex<'a> {
    pub(crate) fn new(doc: &'a CatalogDocument) -> Self {
        let mut tables: BTreeMap<_, Vec<_>> = BTreeMap::new();
        let mut routines = BTreeSet::new();
        for schema in &doc.schemas {
            for routine in &schema.routines {
                let routine_name = match &routine.member_of {
                    Some(pkg) => format!("{pkg}.{}", routine.name),
                    None => routine.name.clone(),
                };
                routines.insert((
                    catalog_key(&doc.dialect, &schema.name),
                    catalog_key(&doc.dialect, &routine_name),
                ));
            }
            for object in &schema.objects {
                tables
                    .entry((
                        catalog_key(&doc.dialect, &schema.name),
                        catalog_key(&doc.dialect, &object.name),
                    ))
                    .or_default()
                    .push((schema.name.as_str(), object));
            }
        }
        Self {
            tables,
            dialect: &doc.dialect,
            routines,
            database: doc
                .context
                .as_ref()
                .and_then(|context| context.database.as_deref())
                .map(|database| database_key(&doc.dialect, database)),
            doc,
            cache_keys: CacheKeyMemo::default(),
            vertices: super::VertexIndex::default(),
            dml_origins: RefCell::default(),
        }
    }

    /// 이 enrich의 그래프에서 이름을 찾는 방식. ci면 대소문자 무시 단일 후보도 본다.
    pub(crate) fn lookup(&self, ci: bool) -> super::Lookup<'_> {
        self.vertices.lookup(ci)
    }

    /// 이 색인이 정규화 규칙으로 쓰는 문서 방언.
    pub(crate) fn dialect(&self) -> &str {
        self.dialect
    }

    /// owner의 DML 효과에 origin을 단 간선 키를 기록한다.
    fn record_dml_origin(&self, owner: &VertexId, key: OriginKey) {
        self.dml_origins
            .borrow_mut()
            .entry(owner.clone())
            .or_default()
            .insert(key);
    }

    /// owner 몸체가 기록한 DML origin 키를 꺼내고 비운다. 기록이 없으면 빈 집합이다.
    pub(crate) fn take_dml_origins(&self, owner: &VertexId) -> BTreeSet<OriginKey> {
        self.dml_origins
            .borrow_mut()
            .remove(owner)
            .unwrap_or_default()
    }

    /// Binder와 캐시 footprint가 같은 규칙으로 관계 키를 만든다.
    ///
    /// `parts`는 `name` 규칙으로 정규화한 이름 부분이다. 한 부분이면 기본 스키마를
    /// 붙이고, 세 부분이면 수집 database와 같을 때만 로컬 관계로 본다. database
    /// 부분도 수집 database와 같은 `database_key`로 접는다 — SQL Server의
    /// `MyDb.dbo.t`를 소문자로 접힌 수집 database와 비교해야 하기 때문이다.
    fn relation_key(&self, parts: &[String], default_schema: &str) -> Option<(String, String)> {
        match parts {
            [table] => Some((catalog_key(self.dialect, default_schema), table.clone())),
            [schema, table] => Some((schema.clone(), table.clone())),
            [database, schema, table]
                if self.database.as_deref()
                    == Some(database_key(self.dialect, database).as_str()) =>
            {
                Some((schema.clone(), table.clone()))
            }
            _ => None,
        }
    }

    pub(crate) fn local_qualified_relation(
        &self,
        name: &ObjectName,
    ) -> Result<(String, String), String> {
        let parts: Vec<_> = name
            .0
            .iter()
            .filter_map(|part| part.as_ident())
            .enumerate()
            .map(|(index, ident)| {
                if index == 0 && name.0.len() == 3 {
                    database_ident_key(self.dialect, ident)
                } else {
                    name_for_dialect(self.dialect, ident)
                }
            })
            .collect();
        let [database, schema, table] = parts.as_slice() else {
            return Err(format!(
                "relation '{}' has an unsupported catalog qualification",
                name
            ));
        };
        if self.database.as_deref() != Some(database.as_str()) {
            return Err(format!(
                "relation '{}' belongs to a different or unknown database/catalog",
                name
            ));
        }
        Ok((schema.clone(), table.clone()))
    }
}

struct Binder<'a, 'd> {
    catalog: &'a CatalogIndex<'d>,
    graph: &'a Graph,
    schema: &'a str,
    /// 한 부분 이름을 `schema` 대신 찾는 순서(routine 정의문의 `SET search_path`).
    search_path: Option<&'a super::SearchPath>,
    reads: BTreeMap<VertexId, BTreeSet<(String, Option<SourceLocation>)>>,
    calls: BTreeMap<VertexId, BTreeSet<Option<SourceLocation>>>,
    diagnostics: Vec<Diagnostic>,
    depth: usize,
    window_expansions: usize,
    scalar_symbols: BTreeSet<String>,
    sqlite_trigger: bool,
    pseudo_relations: BTreeMap<String, Relation>,
    /// 문장열 안에서만 유효한 CTAS/temp 기호. 그래프 정점은 만들지 않고 생성
    /// 질의에서 물려받은 실제 source 컬럼 id만 보관한다.
    local_relations: BTreeMap<Vec<String>, Relation>,
}

/// 입력 원문과 parser 버전으로 근거를 재확인할 때 쓸 해시다.
pub(crate) fn body_hash(body: &str) -> String {
    format!("sha256:{:x}", Sha256::digest(body.as_bytes()))
}

/// 캐시 키 비용을 시간 대신 횟수로 검증하게 하는 테스트 전용 계수다.
#[cfg(test)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct CacheKeyCounters {
    /// 몸체별 cache context 계산 횟수.
    pub(crate) contexts: usize,
    /// cache context가 문서 전체를 직렬화하거나 순회한 횟수.
    pub(crate) document_passes: usize,
}

#[cfg(test)]
thread_local! {
    /// 테스트는 스레드마다 병렬로 돌므로 계수도 스레드마다 따로 둔다.
    static CACHE_KEY_COUNTERS: std::cell::Cell<CacheKeyCounters> =
        const { std::cell::Cell::new(CacheKeyCounters { contexts: 0, document_passes: 0 }) };
}

/// 현재 테스트 스레드의 계수를 돌려주고 0으로 되돌린다.
#[cfg(test)]
pub(crate) fn take_cache_key_counters() -> CacheKeyCounters {
    CACHE_KEY_COUNTERS.with(std::cell::Cell::take)
}

/// 현재 테스트 스레드의 계수를 갱신한다.
#[cfg(test)]
fn count_cache_key(update: impl FnOnce(&mut CacheKeyCounters)) {
    CACHE_KEY_COUNTERS.with(|counters| {
        let mut value = counters.get();
        update(&mut value);
        counters.set(value);
    });
}

/// 캐시 키에 쓰는 문서 단위 값을 enrich 한 번에 처음 필요할 때 한 번만 만든다.
///
/// 몸체마다 문서 전체를 직렬화·순회하면 비용이 몸체 수 × 문서 크기가 된다.
/// 캐시 없는 실행은 아무것도 만들지 않고, 신뢰 지문만 쓰는 실행은 문서 전체
/// 지문을 만들지 않는다.
#[derive(Default)]
struct CacheKeyMemo<'a> {
    /// `global\0{doc:?}`까지 흡수한 해시 상태. owner만 이어 붙여 기존 보수
    /// 지문과 같은 값을 만든다 — 외부 BodyCache가 저장한 키를 그대로 찾는다.
    conservative: OnceCell<Sha256>,
    /// owner 메타데이터를 찾지 못한 몸체가 쓰는 전체 스키마 표현의 해시.
    schemas: OnceCell<String>,
    /// owner·참조 관계 후보와 routine overload 모양.
    footprint: OnceCell<FootprintIndex<'a>>,
}

/// 신뢰 지문이 쓰는 문서 후보를 ASCII 대문자로 접은 (스키마, 이름)으로 찾는다.
///
/// 키를 방언 비교 중 가장 느슨한 ASCII 대소문자 무시로 접어 두고 꺼낼 때 방언
/// 비교로 거르면, 몸체마다 문서를 훑던 비교와 같은 후보를 같은 수만큼 얻는다.
struct FootprintIndex<'a> {
    objects: BTreeMap<(String, String), Vec<(&'a str, &'a ObjectDoc)>>,
    /// 트리거 owner의 부모 id → 그 id를 만든 객체.
    object_ids: BTreeMap<VertexId, Vec<(&'a str, &'a ObjectDoc)>>,
    routines: BTreeMap<(String, String), Vec<(&'a str, &'a RoutineDoc)>>,
    /// 모든 routine overload 모양을 정렬해 한 번만 계산한 해시.
    overloads: String,
}

impl<'a> FootprintIndex<'a> {
    fn new(doc: &'a CatalogDocument) -> Self {
        #[cfg(test)]
        count_cache_key(|counters| counters.document_passes += 1);
        let mut index = Self {
            objects: BTreeMap::new(),
            object_ids: BTreeMap::new(),
            routines: BTreeMap::new(),
            overloads: String::new(),
        };
        let mut overloads = Vec::new();
        for schema in &doc.schemas {
            let schema_name = schema.name.as_str();
            for object in &schema.objects {
                let entry = (schema_name, object);
                let key = folded_key(schema_name, &object.name);
                index.objects.entry(key).or_default().push(entry);
                let id = VertexId::object(schema_name, &object.name);
                index.object_ids.entry(id).or_default().push(entry);
            }
            for routine in &schema.routines {
                let key = folded_key(schema_name, &routine.name);
                index
                    .routines
                    .entry(key)
                    .or_default()
                    .push((schema_name, routine));
                overloads.push(format!(
                    "routine={}.{}:{:?}:{}:{}:{}:{}",
                    schema_name,
                    routine.name,
                    routine.kind,
                    routine.language.as_deref().unwrap_or(""),
                    routine.signature.as_deref().unwrap_or(""),
                    routine.member_of.as_deref().unwrap_or(""),
                    routine.source.as_deref().unwrap_or("")
                ));
            }
        }
        overloads.sort();
        index.overloads = digest_items(&overloads);
        index
    }

    /// 방언 비교로 스키마·이름이 같은 객체 후보.
    fn objects(&self, dialect: &str, schema: &str, name: &str) -> Vec<(&'a str, &'a ObjectDoc)> {
        let candidates = self.objects.get(&folded_key(schema, name));
        candidates
            .into_iter()
            .flatten()
            .filter(|(candidate, object)| {
                schema_name_eq(dialect, candidate, schema) && name_eq(dialect, &object.name, name)
            })
            .copied()
            .collect()
    }

    /// 트리거 정점의 부모 id를 만든 객체. owner 스키마 비교도 그대로 거친다.
    fn trigger_owners(
        &self,
        dialect: &str,
        schema: &str,
        trigger: &VertexId,
    ) -> Vec<(&'a str, &'a ObjectDoc)> {
        let candidates = trigger
            .parent()
            .and_then(|parent| self.object_ids.get(&parent));
        candidates
            .into_iter()
            .flatten()
            .filter(|(candidate, _)| schema_name_eq(dialect, candidate, schema))
            .copied()
            .collect()
    }

    /// 방언 비교로 스키마·이름이 같은 routine 후보.
    fn routines(&self, dialect: &str, schema: &str, name: &str) -> Vec<(&'a str, &'a RoutineDoc)> {
        let candidates = self.routines.get(&folded_key(schema, name));
        candidates
            .into_iter()
            .flatten()
            .filter(|(candidate, routine)| {
                schema_name_eq(dialect, candidate, schema) && name_eq(dialect, &routine.name, name)
            })
            .copied()
            .collect()
    }
}

impl<'a> CatalogIndex<'a> {
    /// 몸체가 실제로 소비하는 카탈로그 모양을 fingerprint한다.
    ///
    /// SQL을 정상적으로 AST로 읽으면 참조한 관계의 후보·컬럼 순서와 routine
    /// overload 집합만 포함해 unrelated 구조 변경은 다른 소유자의 결과를
    /// 무효화하지 않는다. 문장을 읽지 못하면 전체 document 표현을 fingerprint에
    /// 넣어 보수적인 global miss로 돌아간다. 문서 단위 값은 이 색인이 한 번만
    /// 만들므로 몸체 수가 늘어도 몸체마다 문서 전체를 다시 읽지 않는다.
    pub(crate) fn cache_context(
        &self,
        graph: &Graph,
        owner: &VertexId,
        body: &str,
    ) -> CacheContext {
        #[cfg(test)]
        count_cache_key(|counters| counters.contexts += 1);
        // 선언된 search_path는 경로의 모든 스키마가 한 부분 이름의 해석을 바꿀 수 있어
        // 관계 footprint로 좁히지 않는다.
        if wraps_definition_or_dynamic_sql(body)
            || super::routine_search_path(self.dialect, body, |_| true).is_some()
        {
            return self.conservative_context(graph, owner);
        }
        let dialect = super::dialect_for(self.dialect);
        let parsed = super::parse_limits::parse_sql(dialect.as_ref(), body);
        let Ok(statements) = parsed else {
            return self.conservative_context(graph, owner);
        };
        self.scoped_context(graph, owner, &statements)
    }

    /// 문서 전체 표현과 owner 정점으로 전역 miss 지문을 만든다. 문서 부분은
    /// enrich마다 한 번만 해시하고 몸체마다 owner만 이어 붙인다.
    fn conservative_context(&self, graph: &Graph, owner: &VertexId) -> CacheContext {
        let prefix = self.cache_keys.conservative.get_or_init(|| {
            #[cfg(test)]
            count_cache_key(|counters| counters.document_passes += 1);
            let mut hasher = Sha256::new();
            hash_formatted(&mut hasher, format_args!("global\0{:?}", self.doc));
            hasher
        });
        let mut hasher = prefix.clone();
        hash_formatted(&mut hasher, format_args!("\0{:?}", graph.vertex(owner)));
        CacheContext::conservative(format!("sha256:{:x}", hasher.finalize()))
    }

    /// owner·참조 관계의 모양과 routine overload 모양으로 신뢰 지문을 만든다.
    fn scoped_context(
        &self,
        graph: &Graph,
        owner: &VertexId,
        statements: &[Statement],
    ) -> CacheContext {
        let index = self
            .cache_keys
            .footprint
            .get_or_init(|| FootprintIndex::new(self.doc));
        let mut footprint = vec![
            format!("dialect={}", self.doc.dialect),
            format!("owner={}", owner),
            format!("owner-vertex={:?}", graph.vertex(owner)),
            format!("context={:?}", self.doc.context),
        ];
        let owner_metadata_found = self.owner_footprint(index, graph.vertex(owner), &mut footprint);
        if !owner_metadata_found {
            footprint.push(format!("owner-metadata-missing={}", self.schemas_digest()));
        }
        let owner_schema = graph.vertex(owner).map(|vertex| vertex.schema.as_str());
        self.relation_footprint(
            index,
            owner_schema.unwrap_or_default(),
            statements,
            &mut footprint,
        );
        // routine 호출의 한정 방식은 방언마다 다르다. 모든 스키마의 overload
        // 모양을 포함해 불완전한 호출 문맥으로 signature를 고정하지 않는다.
        footprint.push(format!("routines={}", index.overloads));
        footprint.sort();
        CacheContext {
            fingerprint: digest_items(&footprint),
            trusted: owner_metadata_found,
        }
    }

    /// owner 자신의 객체·트리거·routine 모양을 담고, 하나라도 찾았는지 돌려준다.
    fn owner_footprint(
        &self,
        index: &FootprintIndex<'_>,
        owner: Option<&schemagraph_core::Vertex>,
        footprint: &mut Vec<String>,
    ) -> bool {
        let schema = owner
            .map(|vertex| vertex.schema.as_str())
            .unwrap_or_default();
        let name = owner.map(|vertex| vertex.name.as_str()).unwrap_or_default();
        let base_name = name.split_once('@').map(|(name, _)| name).unwrap_or(name);
        let objects = match owner {
            Some(vertex) if owns_catalog_object(vertex.kind) => {
                index.objects(self.dialect, schema, base_name)
            }
            Some(vertex) if vertex.kind == VertexKind::Trigger => {
                index.trigger_owners(self.dialect, schema, &vertex.id)
            }
            _ => Vec::new(),
        };
        let routines = index.routines(self.dialect, schema, base_name);
        let found = !objects.is_empty() || !routines.is_empty();
        for (schema_name, object) in objects {
            footprint.push(format!(
                "owner-object={}.{}:{:?}:{}",
                schema_name,
                object.name,
                object.kind,
                object_shape(object)
            ));
            for trigger in &object.triggers {
                footprint.push(format!(
                    "owner-trigger={}:{}",
                    trigger.name,
                    trigger.body.is_some()
                ));
            }
        }
        for (schema_name, routine) in routines {
            footprint.push(format!(
                "owner-routine={}.{}:{}:{}:{}:{}:{}",
                schema_name,
                routine.name,
                routine.kind,
                routine.language.as_deref().unwrap_or(""),
                routine.signature.as_deref().unwrap_or(""),
                routine.member_of.as_deref().unwrap_or(""),
                routine.source.as_deref().unwrap_or("")
            ));
        }
        found
    }

    /// 몸체가 참조한 관계마다 후보 객체의 모양을, 후보가 없으면 그 사실을 담는다.
    fn relation_footprint(
        &self,
        index: &FootprintIndex<'a>,
        owner_schema: &str,
        statements: &[Statement],
        footprint: &mut Vec<String>,
    ) {
        for parts in relation_names(self.dialect, statements) {
            let Some((ref_schema, ref_name)) = self.relation_key(&parts, owner_schema) else {
                footprint.push(format!("cross-database={}", parts.join(".")));
                continue;
            };
            let candidates = self.relation_candidates(index, &ref_schema, &ref_name);
            if candidates.is_empty() {
                footprint.push(format!("missing-object={ref_schema}.{ref_name}"));
                // SQL Server 누락 메시지는 대소문자만 다른 관계를 적는다(case_only_hint).
                // 그 변형이 생기거나 사라지면 저장한 메시지가 낡으므로 지문에 넣는다.
                if matches!(self.dialect, "sqlserver" | "mssql") {
                    let variants = index.objects.get(&folded_key(&ref_schema, &ref_name));
                    footprint.extend(variants.into_iter().flatten().map(|(schema, object)| {
                        format!("case-variant={}.{}:{:?}", schema, object.name, object.kind)
                    }));
                }
            }
            footprint.extend(candidates.into_iter().map(|(schema, object)| {
                format!(
                    "object={}.{}:{:?}:{}",
                    schema,
                    object.name,
                    object.kind,
                    object_shape(object)
                )
            }));
        }
    }

    /// Binder가 고를 관계 후보. ci 방언은 DML 경로의 vertex_hit가 대소문자 무시
    /// 단일 후보도 고르므로, Binder 후보를 포함하는 접힌 후보 전체를 담는다.
    fn relation_candidates(
        &self,
        index: &FootprintIndex<'a>,
        schema: &str,
        name: &str,
    ) -> Vec<(&'a str, &'a ObjectDoc)> {
        if matches!(self.dialect, "oracle" | "db2") {
            return index.objects(self.dialect, schema, name);
        }
        let key = (schema.to_owned(), name.to_owned());
        self.tables.get(&key).cloned().unwrap_or_default()
    }

    /// owner 메타데이터가 없을 때 쓰는 전체 스키마 표현의 해시를 한 번만 만든다.
    fn schemas_digest(&self) -> &str {
        self.cache_keys.schemas.get_or_init(|| {
            #[cfg(test)]
            count_cache_key(|counters| counters.document_passes += 1);
            let mut hasher = Sha256::new();
            hash_formatted(&mut hasher, format_args!("{:?}", self.doc.schemas));
            format!("sha256:{:x}", hasher.finalize())
        })
    }
}

/// 단일 몸체의 cache context — 테스트가 문서마다 새 색인으로 지문을 비교한다.
#[cfg(test)]
pub(crate) fn cache_context(
    doc: &CatalogDocument,
    graph: &Graph,
    owner: &VertexId,
    body: &str,
) -> CacheContext {
    CatalogIndex::new(doc).cache_context(graph, owner, body)
}

/// 정의 껍질이나 동적 SQL은 몸체만으로 소비하는 이름을 고정할 수 없다.
fn wraps_definition_or_dynamic_sql(body: &str) -> bool {
    // 공백의 종류와 개수는 머리의 뜻을 바꾸지 않는다 — `CREATE\nFUNCTION`도 정의문이고, 이를
    // 놓치면 몸체 관계를 모르는 좁은 지문으로 저장해 관계가 바뀐 뒤에도 재사용한다.
    let uppercase = body
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_uppercase();
    [
        "CREATE FUNCTION",
        "CREATE PROCEDURE",
        "CREATE TRIGGER",
        "CREATE OR REPLACE FUNCTION",
        "CREATE OR REPLACE PROCEDURE",
        "CREATE OR REPLACE TRIGGER",
        "EXECUTE ",
        "EXECUTE(",
        "EXEC ",
        "EXEC(",
    ]
    .iter()
    .any(|keyword| uppercase.contains(keyword))
}

/// 몸체가 참조한 관계 이름을 Binder::parts와 같은 `name` 규칙으로 정규화한다.
/// 같은 규칙이어야 footprint 후보가 Binder가 해석할 후보와 같아진다.
fn relation_names(dialect: &str, statements: &[Statement]) -> BTreeSet<Vec<String>> {
    let mut relation_names = BTreeSet::new();
    for statement in statements {
        let _ = visit_relations(statement, |relation| {
            let parts = relation.0.iter().filter_map(|part| part.as_ident());
            relation_names.insert(parts.map(|ident| name(dialect, ident)).collect::<Vec<_>>());
            ControlFlow::<()>::Continue(())
        });
    }
    relation_names
}

/// 몸체가 catalog 객체 자신(뷰 등)인 owner kind.
fn owns_catalog_object(kind: VertexKind) -> bool {
    matches!(
        kind,
        VertexKind::Table
            | VertexKind::View
            | VertexKind::MaterializedView
            | VertexKind::Synonym
            | VertexKind::Sequence
            | VertexKind::Type
    )
}

/// 방언 비교 중 가장 느슨한 ASCII 대소문자 무시 비교의 키.
fn folded_key(schema: &str, name: &str) -> (String, String) {
    (schema.to_ascii_uppercase(), name.to_ascii_uppercase())
}

/// 정렬된 항목을 0 구분자로 이어 해시한다 — 항목 경계가 섞이지 않게 한다.
fn digest_items(items: &[String]) -> String {
    let mut hasher = Sha256::new();
    for item in items {
        hasher.update(item.as_bytes());
        hasher.update([0]);
    }
    format!("sha256:{:x}", hasher.finalize())
}

/// 서식 결과를 문자열로 모으지 않고 해시에 흘려 문서 크기만 한 임시 할당을
/// 피한다. 흘린 바이트는 같은 서식의 `format!` 결과와 같다.
fn hash_formatted(hasher: &mut Sha256, arguments: std::fmt::Arguments<'_>) {
    // DigestWriter는 오류를 내지 않고 derive(Debug)는 쓰기 오류만 전파하므로
    // 이 쓰기는 실패할 수 없다.
    std::fmt::Write::write_fmt(&mut DigestWriter(hasher), arguments)
        .expect("hashing formatted output cannot fail");
}

/// `fmt::Write`로 받은 조각을 그대로 해시에 넣는다.
struct DigestWriter<'a>(&'a mut Sha256);

impl std::fmt::Write for DigestWriter<'_> {
    fn write_str(&mut self, text: &str) -> std::fmt::Result {
        self.0.update(text.as_bytes());
        Ok(())
    }
}

fn schema_name_eq(dialect: &str, left: &str, right: &str) -> bool {
    if matches!(dialect, "oracle" | "db2") {
        left.eq_ignore_ascii_case(right)
    } else {
        left == right
    }
}

fn name_eq(dialect: &str, left: &str, right: &str) -> bool {
    if matches!(dialect, "sqlite" | "mysql" | "mariadb" | "oracle" | "db2") {
        left.eq_ignore_ascii_case(right)
    } else {
        left == right
    }
}

fn object_shape(object: &ObjectDoc) -> String {
    let mut columns: Vec<_> = object
        .columns
        .iter()
        .map(|column| {
            format!(
                "{}:{}:{}:{}:{}",
                column.name, column.data_type, column.ordinal, column.nullable, column.pk_position
            )
        })
        .collect();
    columns.sort();
    format!("{}|{}", object.body.is_some(), columns.join(","))
}

/// 목적지가 없는 단일 SELECT만 읽기 해석기로 보내고 SELECT INTO는 DML로 남긴다.
pub(crate) fn is_select(dialect: &dyn Dialect, body: &str) -> bool {
    super::parse_limits::parse_sql(dialect, body).is_ok_and(|s| {
        s.len() == 1
            && matches!(s[0], Statement::Query(_))
            && !super::is_column_dml_statement(&s[0])
    })
}

fn catalog_key(dialect: &str, value: &str) -> String {
    if dialect == "sqlite" {
        value.to_lowercase()
    } else {
        value.to_owned()
    }
}

fn database_key(dialect: &str, value: &str) -> String {
    match dialect {
        "sqlite" | "mysql" | "mariadb" | "sqlserver" => value.to_ascii_lowercase(),
        _ => value.to_owned(),
    }
}

fn database_ident_key(dialect: &str, ident: &Ident) -> String {
    if matches!(dialect, "sqlite" | "mysql" | "mariadb" | "sqlserver") {
        ident.value.to_ascii_lowercase()
    } else {
        name_for_dialect(dialect, ident)
    }
}

/// 컬럼 이름은 방언의 열 비교 규칙으로만 정규화한다. 테이블·스키마·테이블
/// 별칭은 같은 규칙을 공유하지 않으므로 `name`을 그대로 사용해야 한다.
fn column_key(dialect: &str, value: &str) -> String {
    match dialect {
        "sqlite" | "mysql" | "mariadb" => value.to_lowercase(),
        _ => value.to_owned(),
    }
}

pub(super) fn name(dialect: &str, ident: &Ident) -> String {
    if dialect == "sqlite" {
        return ident.value.to_lowercase();
    }
    if ident.quote_style.is_some() {
        return ident.value.clone();
    }
    match dialect {
        "postgres" | "postgresql" => ident.value.to_lowercase(),
        "oracle" | "db2" => ident.value.to_uppercase(),
        _ => ident.value.clone(),
    }
}

/// SQL 식별자의 컬럼 위치를 해석한다. 인용 여부에 따른 PostgreSQL·Oracle의
/// 대소문자 의미는 `name`에 맡기고, MySQL/SQLite의 열 비교만 `column_key`로
/// 접는다.
fn column_name(dialect: &str, ident: &Ident) -> String {
    column_key(dialect, &name(dialect, ident))
}

fn location(item: &impl Spanned) -> Option<SourceLocation> {
    let span = item.span();
    (span.start.line > 0 && span.end.line > 0).then_some(SourceLocation {
        line: span.start.line,
        column: span.start.column,
        end_line: span.end.line,
        end_column: span.end.column,
    })
}

fn ident_location(ident: &Ident) -> Option<SourceLocation> {
    let span = ident.span;
    (span.start.line > 0 && span.end.line > 0).then_some(SourceLocation {
        line: span.start.line,
        column: span.start.column,
        end_line: span.end.line,
        end_column: span.end.column,
    })
}

impl Binder<'_, '_> {
    fn call(&mut self, function: &ObjectName, source: Option<SourceLocation>) {
        let parts = self.parts(function);
        let quoted_name = function
            .0
            .last()
            .and_then(|part| part.as_ident())
            .is_some_and(|ident| ident.quote_style.is_some());
        if let ([routine], Some(path)) = (parts.as_slice(), self.search_path) {
            self.call_through_path(function, routine, path, quoted_name, source);
            return;
        }
        let default = catalog_key(self.catalog.dialect, self.schema);
        let (schema, routine) = match parts.as_slice() {
            [routine] => (default.clone(), routine.clone()),
            [schema, routine]
                if self
                    .catalog
                    .routines
                    .contains(&(schema.clone(), routine.clone())) =>
            {
                (schema.clone(), routine.clone())
            }
            [pkg, routine]
                if self
                    .catalog
                    .routines
                    .contains(&(default.clone(), format!("{pkg}.{routine}"))) =>
            {
                (default.clone(), format!("{pkg}.{routine}"))
            }
            [schema, routine] => (schema.clone(), routine.clone()),
            [schema, pkg, routine] => (schema.clone(), format!("{pkg}.{routine}")),
            _ => {
                self.note(
                    "SG_CALL_UNRESOLVED",
                    format!("function '{function}' has an unsupported qualified name"),
                    source,
                );
                return;
            }
        };
        let known = self
            .catalog
            .routines
            .contains(&(schema.clone(), routine.clone()));
        // PostgreSQL의 "substring"은 같은 소문자 내장 함수지만 "SUBSTRING"은
        // 다른 식별자다. 인용 이름을 대소문자 무시로 접어 내장 함수로 숨기지 않는다.
        let builtin_spelling = !quoted_name
            || (matches!(self.catalog.dialect, "postgres" | "postgresql")
                && routine == routine.to_ascii_lowercase());
        if !known
            && builtin_spelling
            && super::is_builtin_function(&routine)
            && (parts.len() == 1
                || matches!(schema.as_str(), "pg_catalog" | "SYS" | "SYSIBM" | "SYSFUN"))
        {
            return;
        }
        let hit = if let Some((pkg, member)) = routine.split_once('.') {
            super::resolve_pkg_member(self.graph, &schema, pkg, member, self.catalog.lookup(false))
        } else if known {
            super::resolve_routine(self.graph, &schema, &routine, self.catalog.lookup(false))
        } else {
            super::RoutineHit::None
        };
        self.record_call(function, hit, source);
    }

    /// routine 정의문이 선언한 search_path로 한 부분 함수 이름을 해석한다. 경로의 모든 수집
    /// 스키마에서 후보를 모으므로 스키마마다 다른 오버로드는 모호함으로 남는다.
    fn call_through_path(
        &mut self,
        function: &ObjectName,
        routine: &str,
        path: &super::SearchPath,
        quoted_name: bool,
        source: Option<SourceLocation>,
    ) {
        let known = path.collected().any(|schema| {
            self.catalog.routines.contains(&(
                catalog_key(self.catalog.dialect, schema),
                routine.to_owned(),
            ))
        });
        let builtin_spelling = !quoted_name || routine == routine.to_ascii_lowercase();
        if !known && builtin_spelling && super::is_builtin_function(routine) {
            return;
        }
        let hit = if known {
            path.routine_hit(self.graph, routine, self.catalog.lookup(false))
        } else {
            path.collected().next().map(|_| super::RoutineHit::None)
        };
        match hit {
            Some(hit) => self.record_call(function, hit, source),
            None => self.note(
                "SG_CALL_UNRESOLVED",
                format!("function '{function}' is not in a collected search_path schema"),
                source,
            ),
        }
    }

    /// 해석 결과를 호출 근거나 진단으로 남긴다.
    fn record_call(
        &mut self,
        function: &ObjectName,
        hit: super::RoutineHit,
        source: Option<SourceLocation>,
    ) {
        match hit {
            super::RoutineHit::One(id) => {
                self.calls.entry(id).or_default().insert(source);
            }
            super::RoutineHit::Ambiguous(candidates) => {
                // 확정 간선 없이 후보만 진단에 싣는다 — dead가 불릴 수 있는 오버로드를
                // 후보로 내지 않게 하는 근거이고, query/impact의 확정 의존은 그대로다.
                let message = format!(
                    "function '{function}' has {} matching overloads; no target guessed",
                    candidates.len()
                );
                self.diagnostics.push(
                    Diagnostic::new("SG_CALL_AMBIGUOUS", message, source)
                        .with_candidates(candidates),
                );
            }
            super::RoutineHit::None => self.note(
                "SG_CALL_UNRESOLVED",
                format!("function '{function}' was not resolved in the collected catalog"),
                source,
            ),
        }
    }
    fn note(&mut self, code: &str, message: String, location: Option<SourceLocation>) {
        self.diagnostics.push(Diagnostic {
            code: code.into(),
            message,
            location,
            candidates: Vec::new(),
        });
    }

    fn read(&mut self, id: VertexId, role: &str, source: Option<SourceLocation>) {
        self.reads
            .entry(id)
            .or_default()
            .insert((role.to_owned(), source));
    }

    fn query(&mut self, query: &Query, inherited: &Ctes, outer: Option<Rc<Scope>>) -> Relation {
        if self.depth >= 64 {
            self.note(
                "SG_SCOPE_DEPTH",
                "query nesting exceeds the 64-scope analysis limit".into(),
                location(query),
            );
            return Relation {
                unknown: true,
                ..Relation::default()
            };
        }
        self.depth += 1;
        let mut ctes = inherited.clone();
        if let Some(with) = &query.with {
            for cte in &with.cte_tables {
                let key = name(self.catalog.dialect, &cte.alias.name);
                if with.recursive {
                    ctes.insert(
                        key.clone(),
                        Relation {
                            unknown: true,
                            ..Relation::default()
                        },
                    );
                }
                let mut relation = self.query(&cte.query, &ctes, outer.clone());
                self.alias(&mut relation, &cte.alias);
                ctes.insert(key, relation);
            }
        }
        let (result, scope) = self.set(&query.body, &ctes, outer);
        if let Some(order) = &query.order_by {
            self.visit(
                order,
                &scope,
                &ctes,
                "ordering",
                &result.columns,
                &scope.named_windows,
            );
        }
        if let Some(limit) = &query.limit_clause {
            self.visit(limit, &scope, &ctes, "limit", &[], &scope.named_windows);
        }
        if let Some(fetch) = &query.fetch {
            self.visit(fetch, &scope, &ctes, "limit", &[], &scope.named_windows);
        }
        if !query.pipe_operators.is_empty() || query.for_clause.is_some() {
            self.note(
                "SG_QUERY_MODIFIER",
                "query result modifiers are not fully interpreted".into(),
                location(query),
            );
        }
        self.depth -= 1;
        result
    }

    fn set(
        &mut self,
        set: &SetExpr,
        ctes: &Ctes,
        outer: Option<Rc<Scope>>,
    ) -> (Relation, Rc<Scope>) {
        match set {
            SetExpr::Select(select) => self.select(select, ctes, outer),
            SetExpr::Query(query) => {
                let result = self.query(query, ctes, outer.clone());
                let scope = Rc::new(Scope {
                    columns: result.columns.clone(),
                    outer,
                    ..Scope::default()
                });
                (result, scope)
            }
            SetExpr::SetOperation { .. } => self.set_operation_chain(set, ctes, outer),
            SetExpr::Values(values) => {
                let scope = Rc::new(Scope {
                    outer,
                    ..Scope::default()
                });
                let mut columns = Vec::new();
                for row in &values.rows {
                    for (i, expr) in row.iter().enumerate() {
                        if i >= columns.len() {
                            columns.push(Column {
                                name: format!("column{}", i + 1),
                                ..Column::default()
                            });
                        }
                        columns[i].sources.extend(self.visit(
                            expr,
                            &scope,
                            ctes,
                            "projection",
                            &[],
                            &BTreeMap::new(),
                        ));
                    }
                }
                (
                    Relation {
                        columns,
                        ..Relation::default()
                    },
                    scope,
                )
            }
            _ => {
                self.note(
                    "SG_SET_UNSUPPORTED",
                    "this query body is not supported for column analysis".into(),
                    None,
                );
                (
                    Relation {
                        unknown: true,
                        ..Relation::default()
                    },
                    Rc::new(Scope {
                        outer,
                        ..Scope::default()
                    }),
                )
            }
        }
    }

    /// 왼쪽으로 깊어지는 집합 연산 사슬을 반복문으로 결합한다.
    ///
    /// sqlparser는 `a UNION b UNION c`를 왼쪽 깊은 트리로 만든다. 왼쪽을 재귀로 내려가면
    /// 사슬 길이만큼 스택을 쓰므로, 왼쪽 등뼈를 모은 뒤 가장 안쪽 연산부터 재귀 때와
    /// 같은 순서(왼쪽 결과 → 오른쪽 결과 → 결합)로 처리한다.
    fn set_operation_chain(
        &mut self,
        set: &SetExpr,
        ctes: &Ctes,
        outer: Option<Rc<Scope>>,
    ) -> (Relation, Rc<Scope>) {
        let mut spine = Vec::new();
        let mut leftmost = set;
        while let SetExpr::SetOperation {
            op, left, right, ..
        } = leftmost
        {
            spine.push((op, right.as_ref()));
            leftmost = left.as_ref();
        }
        let (mut result, _) = self.set(leftmost, ctes, outer.clone());
        for (op, right) in spine.into_iter().rev() {
            let (right, _) = self.set(right, ctes, outer.clone());
            self.combine_set_operation(&mut result, right, op);
        }
        let scope = Rc::new(Scope {
            columns: result.columns.clone(),
            outer,
            ..Scope::default()
        });
        (result, scope)
    }

    /// 집합 연산 한 단계의 오른쪽 결과를 왼쪽 결과에 합친다.
    fn combine_set_operation(
        &mut self,
        left: &mut Relation,
        right: Relation,
        op: &sqlparser::ast::SetOperator,
    ) {
        if left.columns.len() != right.columns.len() {
            self.note(
                "SG_SET_WIDTH",
                "set-operation column counts differ; output lineage is partial".into(),
                None,
            );
            left.unknown = true;
        }
        for (l, r) in left.columns.iter_mut().zip(right.columns) {
            // INTERSECT는 양쪽에 공통인 값을 반환하므로 양쪽 출처를
            // 보존한다. EXCEPT의 오른쪽은 행을 제외하는 읽기 조건이다.
            if matches!(
                op,
                sqlparser::ast::SetOperator::Union | sqlparser::ast::SetOperator::Intersect
            ) {
                l.sources.extend(r.sources);
            }
            l.unknown |= r.unknown;
        }
        left.unknown |= right.unknown;
    }

    fn select(
        &mut self,
        select: &Select,
        ctes: &Ctes,
        outer: Option<Rc<Scope>>,
    ) -> (Relation, Rc<Scope>) {
        let mut scope = Scope {
            outer,
            ..Scope::default()
        };
        for from in &select.from {
            self.join(from, &mut scope, ctes);
        }
        let named_windows = self.named_windows(select);
        scope.named_windows = named_windows.clone();
        let scope = Rc::new(scope);
        self.analyze_named_windows(&named_windows, &scope, ctes);
        let mut columns = Vec::new();
        for item in &select.projection {
            match item {
                SelectItem::UnnamedExpr(expr) | SelectItem::ExprWithAlias { expr, .. } => {
                    let alias = match item {
                        SelectItem::ExprWithAlias { alias, .. } => {
                            column_name(self.catalog.dialect, alias)
                        }
                        _ => match expr {
                            Expr::Identifier(id) => column_name(self.catalog.dialect, id),
                            Expr::CompoundIdentifier(ids) => ids
                                .last()
                                .map(|id| column_name(self.catalog.dialect, id))
                                .unwrap_or_default(),
                            _ => expr.to_string(),
                        },
                    };
                    let before = self.diagnostics.len();
                    let sources = self.visit(expr, &scope, ctes, "projection", &[], &named_windows);
                    columns.push(Column {
                        name: alias,
                        sources,
                        unknown: self.diagnostics.len() > before,
                        location: location(expr),
                    });
                }
                SelectItem::Wildcard(options) => {
                    if scope.relations.is_empty() || scope.relations.iter().any(|r| r.unknown) {
                        self.note(
                            "SG_WILDCARD_UNKNOWN",
                            "wildcard includes an unresolved relation or has no FROM scope".into(),
                            location(item),
                        );
                    }
                    columns.extend(self.wildcard(scope.columns.clone(), options, location(item)));
                }
                SelectItem::QualifiedWildcard(
                    SelectItemQualifiedWildcardKind::ObjectName(qualifier),
                    options,
                ) => {
                    let key = self.parts(qualifier);
                    let matches: Vec<_> = scope
                        .relations
                        .iter()
                        .filter(|r| r.aliases.contains(&key))
                        .collect();
                    if matches.len() == 1 && !matches[0].unknown {
                        columns.extend(self.wildcard(
                            matches[0].columns.clone(),
                            options,
                            location(item),
                        ));
                    } else {
                        self.note(
                            "SG_WILDCARD_UNKNOWN",
                            format!("cannot uniquely expand {qualifier}.*"),
                            location(item),
                        );
                    }
                }
                _ => self.note(
                    "SG_WILDCARD_EXPRESSION",
                    "expression wildcards are not supported".into(),
                    location(item),
                ),
            }
        }
        for predicate in [&select.selection, &select.prewhere].into_iter().flatten() {
            self.visit(predicate, &scope, ctes, "predicate", &[], &named_windows);
        }
        if let Some(having) = &select.having {
            let aliases = if matches!(self.catalog.dialect, "mysql" | "mariadb") {
                columns.as_slice()
            } else {
                &[]
            };
            self.visit(having, &scope, ctes, "predicate", aliases, &named_windows);
        }
        if let Some(qualify) = &select.qualify {
            self.visit(qualify, &scope, ctes, "predicate", &columns, &named_windows);
        }
        if let GroupByExpr::Expressions(exprs, _) = &select.group_by {
            for expr in exprs {
                self.visit(expr, &scope, ctes, "grouping", &columns, &named_windows);
            }
        }
        // PostgreSQL DISTINCT ON의 bare identifier도 ORDER BY와 같은 출력
        // alias 우선 규칙을 따른다. 입력 relation을 먼저 보면 orders.id를
        // 잘못 읽어 amount AS id의 의미가 오염된다.
        self.visit(
            &select.distinct,
            &scope,
            ctes,
            "ordering",
            &columns,
            &named_windows,
        );
        self.visit(
            &select.cluster_by,
            &scope,
            ctes,
            "ordering",
            &[],
            &named_windows,
        );
        self.visit(
            &select.distribute_by,
            &scope,
            ctes,
            "ordering",
            &[],
            &named_windows,
        );
        self.visit(
            &select.sort_by,
            &scope,
            ctes,
            "ordering",
            &[],
            &named_windows,
        );
        if select.connect_by.is_some()
            || !select.lateral_views.is_empty()
            || select.exclude.is_some()
            || select.value_table_mode.is_some()
        {
            self.note(
                "SG_SELECT_MODIFIER",
                "SELECT modifiers are not fully interpreted".into(),
                location(select),
            );
        }
        let unknown = scope.relations.iter().any(|r| r.unknown);
        (
            Relation {
                columns,
                unknown,
                ..Relation::default()
            },
            scope,
        )
    }

    fn named_windows(&mut self, select: &Select) -> NamedWindows {
        let mut windows = NamedWindows::new();
        for definition in &select.named_window {
            let key = name(self.catalog.dialect, &definition.0);
            if windows.insert(key.clone(), definition.1.clone()).is_some() {
                self.note(
                    "SG_WINDOW_DUPLICATE",
                    format!("named window '{key}' is defined more than once"),
                    location(definition),
                );
            }
        }
        windows
    }

    fn analyze_named_windows(&mut self, windows: &NamedWindows, scope: &Rc<Scope>, ctes: &Ctes) {
        for (key, expression) in windows {
            let mut stack = vec![key.clone()];
            self.named_window_expression_sources(expression, windows, scope, ctes, &mut stack);
        }
    }

    fn named_window_expression_sources(
        &mut self,
        expression: &NamedWindowExpr,
        windows: &NamedWindows,
        scope: &Rc<Scope>,
        ctes: &Ctes,
        stack: &mut Vec<String>,
    ) -> Sources {
        match expression {
            NamedWindowExpr::NamedWindow(parent) => {
                self.named_window_sources(parent, windows, scope, ctes, stack)
            }
            NamedWindowExpr::WindowSpec(spec) => {
                let mut sources = Sources::new();
                if let Some(parent) = &spec.window_name {
                    sources.extend(self.named_window_sources(parent, windows, scope, ctes, stack));
                }
                sources.extend(self.visit_with_window_stack(
                    spec,
                    scope,
                    ctes,
                    "window",
                    &[],
                    (windows, stack),
                ));
                sources
            }
        }
    }

    fn named_window_sources(
        &mut self,
        reference: &Ident,
        windows: &NamedWindows,
        scope: &Rc<Scope>,
        ctes: &Ctes,
        stack: &mut Vec<String>,
    ) -> Sources {
        // 깊이만 제한하면 잘못된 정의가 부모 창을 여러 번 참조할 때 지수적으로
        // 재방문할 수 있다. 몸체 전체의 확장 횟수도 제한하고 불완전함을 남긴다.
        if self.window_expansions >= 4096 {
            if self.window_expansions == 4096 {
                self.note(
                    "SG_WINDOW_BUDGET",
                    "named window expansion exceeds the 4096-reference analysis limit".into(),
                    ident_location(reference),
                );
                self.window_expansions += 1;
            }
            return Sources::new();
        }
        self.window_expansions += 1;
        let key = name(self.catalog.dialect, reference);
        if stack.iter().any(|item| item == &key) {
            self.note(
                "SG_WINDOW_CYCLE",
                format!("named window inheritance cycle includes '{key}'"),
                ident_location(reference),
            );
            return Sources::new();
        }
        if stack.len() >= 64 {
            self.note(
                "SG_WINDOW_DEPTH",
                "named window inheritance exceeds the 64-window analysis limit".into(),
                ident_location(reference),
            );
            return Sources::new();
        }
        let Some(expression) = windows.get(&key) else {
            self.note(
                "SG_WINDOW_UNKNOWN",
                format!("named window '{key}' is not defined in this query"),
                ident_location(reference),
            );
            return Sources::new();
        };
        stack.push(key);
        let sources = self.named_window_expression_sources(expression, windows, scope, ctes, stack);
        stack.pop();
        sources
    }

    fn wildcard(
        &mut self,
        mut columns: Vec<Column>,
        options: &WildcardAdditionalOptions,
        source: Option<SourceLocation>,
    ) -> Vec<Column> {
        if options.to_string().trim().is_empty() {
            for column in &mut columns {
                column.location = source.clone();
                for id in &column.sources {
                    self.read(id.clone(), "projection", source.clone());
                }
            }
            return columns;
        }
        self.note(
            "SG_WILDCARD_MODIFIER",
            "wildcard EXCEPT/REPLACE/RENAME modifiers are not yet resolved; output lineage omitted"
                .into(),
            source,
        );
        Vec::new()
    }

    fn join(&mut self, twj: &TableWithJoins, scope: &mut Scope, ctes: &Ctes) {
        let mut group = Scope {
            outer: scope.outer.clone(),
            ..Scope::default()
        };
        self.add_factor(&twj.relation, &mut group, ctes, Rc::new(scope.clone()));
        for join in &twj.joins {
            let mut lateral_scope = scope.clone();
            for r in &group.relations {
                lateral_scope.add(r.clone());
            }
            let mut right = Scope {
                outer: scope.outer.clone(),
                ..Scope::default()
            };
            self.add_factor(&join.relation, &mut right, ctes, Rc::new(lateral_scope));
            let constraint = match &join.join_operator {
                JoinOperator::Join(c)
                | JoinOperator::Inner(c)
                | JoinOperator::Left(c)
                | JoinOperator::Right(c)
                | JoinOperator::LeftOuter(c)
                | JoinOperator::RightOuter(c)
                | JoinOperator::FullOuter(c)
                | JoinOperator::Semi(c)
                | JoinOperator::LeftSemi(c)
                | JoinOperator::RightSemi(c)
                | JoinOperator::Anti(c)
                | JoinOperator::LeftAnti(c)
                | JoinOperator::RightAnti(c)
                | JoinOperator::StraightJoin(c)
                | JoinOperator::AsOf { constraint: c, .. } => Some(c),
                _ => None,
            };
            let keys = match constraint {
                Some(JoinConstraint::Using(names)) => names
                    .iter()
                    .map(|n| self.column_parts(n).join("."))
                    .collect(),
                Some(JoinConstraint::Natural) => group
                    .columns
                    .iter()
                    .filter(|l| right.columns.iter().any(|r| r.name == l.name))
                    .map(|c| c.name.clone())
                    .collect(),
                _ => Vec::new(),
            };
            let preservation = Self::using_preservation(&join.join_operator);
            if keys.is_empty() {
                group.columns.extend(right.columns);
                group.relations.extend(right.relations);
            } else {
                self.using(&mut group, right, &keys, preservation);
            }
            if let Some(JoinConstraint::On(expr)) = constraint {
                let mut on_scope = scope.clone();
                for r in &group.relations {
                    on_scope.add(r.clone());
                }
                self.visit(
                    expr,
                    &Rc::new(on_scope),
                    ctes,
                    "join",
                    &[],
                    &BTreeMap::new(),
                );
            }
            if let JoinOperator::AsOf {
                match_condition, ..
            } = &join.join_operator
            {
                let mut on_scope = scope.clone();
                for r in &group.relations {
                    on_scope.add(r.clone());
                }
                self.visit(
                    match_condition,
                    &Rc::new(on_scope),
                    ctes,
                    "join",
                    &[],
                    &BTreeMap::new(),
                );
            }
            if matches!(
                join.join_operator,
                JoinOperator::Semi(_)
                    | JoinOperator::LeftSemi(_)
                    | JoinOperator::RightSemi(_)
                    | JoinOperator::Anti(_)
                    | JoinOperator::LeftAnti(_)
                    | JoinOperator::RightAnti(_)
                    | JoinOperator::CrossApply
                    | JoinOperator::OuterApply
            ) {
                self.note(
                    "SG_JOIN_PROJECTION",
                    "SEMI/ANTI/APPLY output scope is not fully interpreted; output lineage omitted"
                        .into(),
                    None,
                );
            }
        }
        scope.columns.extend(group.columns);
        scope.relations.extend(group.relations);
    }

    fn add_factor(
        &mut self,
        factor: &TableFactor,
        group: &mut Scope,
        ctes: &Ctes,
        visible: Rc<Scope>,
    ) {
        if let TableFactor::NestedJoin {
            table_with_joins,
            alias: None,
        } = factor
        {
            self.join(table_with_joins, group, ctes);
        } else {
            group.add(self.factor(factor, ctes, visible));
        }
    }

    fn using(
        &mut self,
        scope: &mut Scope,
        right: Scope,
        keys: &[String],
        preservation: UsingPreservation,
    ) {
        let mut merged = Vec::new();
        for key in keys {
            let left: Vec<_> = scope.columns.iter().filter(|c| &c.name == key).collect();
            let rhs: Vec<_> = right.columns.iter().filter(|c| &c.name == key).collect();
            if left.len() == 1 && rhs.len() == 1 {
                let col = match preservation {
                    UsingPreservation::Left => left[0].clone(),
                    UsingPreservation::Right => rhs[0].clone(),
                    UsingPreservation::Inner | UsingPreservation::Full => {
                        let mut col = left[0].clone();
                        col.sources.extend(rhs[0].sources.clone());
                        col.unknown |= rhs[0].unknown;
                        col
                    }
                };
                let mut reads = left[0].sources.clone();
                reads.extend(rhs[0].sources.clone());
                for id in &reads {
                    self.read(id.clone(), "join", None);
                }
                merged.push(col);
            } else {
                self.note(
                    "SG_USING_AMBIGUOUS",
                    format!("JOIN USING column '{key}' is missing or ambiguous"),
                    None,
                );
            }
        }
        if self.catalog.dialect == "sqlite" {
            for col in &mut scope.columns {
                if let Some(merged) = merged.iter().find(|m| m.name == col.name) {
                    *col = merged.clone();
                }
            }
        } else {
            let mut columns = merged;
            columns.extend(
                scope
                    .columns
                    .iter()
                    .filter(|c| !keys.contains(&c.name))
                    .cloned(),
            );
            scope.columns = columns;
        }
        scope.columns.extend(
            right
                .columns
                .iter()
                .filter(|c| !keys.contains(&c.name))
                .cloned(),
        );
        scope.relations.extend(right.relations);
    }

    fn using_preservation(join: &JoinOperator) -> UsingPreservation {
        match join {
            JoinOperator::Left(_) | JoinOperator::LeftOuter(_) => UsingPreservation::Left,
            JoinOperator::Right(_) | JoinOperator::RightOuter(_) => UsingPreservation::Right,
            JoinOperator::FullOuter(_) => UsingPreservation::Full,
            _ => UsingPreservation::Inner,
        }
    }

    fn parts(&self, object: &ObjectName) -> Vec<String> {
        object
            .0
            .iter()
            .filter_map(|p| p.as_ident())
            .map(|id| name(self.catalog.dialect, id))
            .collect()
    }

    fn column_parts(&self, object: &ObjectName) -> Vec<String> {
        let mut parts = self.parts(object);
        if let Some(column) = parts.last_mut() {
            *column = column_key(self.catalog.dialect, column);
        }
        parts
    }

    fn factor(&mut self, factor: &TableFactor, ctes: &Ctes, scope: Rc<Scope>) -> Relation {
        match factor {
            TableFactor::Table {
                name: table,
                alias,
                args,
                ..
            } if args.is_none() => {
                let parts = self.parts(table);
                let mut result = if parts.len() == 1 && ctes.contains_key(&parts[0]) {
                    ctes[&parts[0]].clone()
                } else if self.local_relations.contains_key(&parts) {
                    self.local_relations[&parts].clone()
                } else {
                    self.table(&parts, location(factor))
                };
                if let Some(alias) = alias {
                    self.alias(&mut result, alias);
                } else {
                    result.aliases = vec![parts.clone()];
                    if let Some(last) = parts.last() {
                        result.aliases.push(vec![last.clone()]);
                    }
                }
                result
            }
            TableFactor::Derived {
                lateral,
                subquery,
                alias,
            } => {
                let outer = if *lateral {
                    Some(scope)
                } else {
                    scope.outer.clone()
                };
                let mut result = self.query(subquery, ctes, outer);
                if let Some(alias) = alias {
                    self.alias(&mut result, alias);
                }
                result
            }
            TableFactor::NestedJoin {
                table_with_joins,
                alias,
            } => {
                let mut nested = Scope {
                    outer: scope.outer.clone(),
                    ..Scope::default()
                };
                self.join(table_with_joins, &mut nested, ctes);
                let mut result = Relation {
                    columns: nested.columns,
                    unknown: nested.relations.iter().any(|r| r.unknown),
                    aliases: nested
                        .relations
                        .into_iter()
                        .flat_map(|r| r.aliases)
                        .collect(),
                };
                // 별칭 없는 괄호 JOIN은 factor 하나로 압축하면 a.x와 b.x가
                // 섞인다. join 호출부에서 별도 경로로 펼친다.
                if let Some(alias) = alias {
                    self.alias(&mut result, alias);
                }
                result
            }
            _ => {
                self.note("SG_RELATION_UNSUPPORTED","table function, pivot, or other relation form is not supported for column resolution".into(),location(factor));
                Relation {
                    unknown: true,
                    ..Relation::default()
                }
            }
        }
    }

    fn table(&mut self, parts: &[String], source: Option<SourceLocation>) -> Relation {
        let default_schema = match (parts, self.search_path) {
            ([table], Some(path)) => {
                let schema = path.relation_schema(table, |schema| {
                    self.catalog
                        .tables
                        .contains_key(&(catalog_key(self.catalog.dialect, schema), table.clone()))
                });
                let Some(schema) = schema else {
                    self.note(
                        "SG_RELATION_UNRESOLVED",
                        format!("relation '{table}' is not in a collected search_path schema"),
                        source,
                    );
                    return Relation {
                        unknown: true,
                        ..Relation::default()
                    };
                };
                schema
            }
            _ => self.schema,
        };
        let Some((schema, table)) = self.catalog.relation_key(parts, default_schema) else {
            self.note(
                "SG_CROSS_DATABASE",
                format!(
                    "relation '{}' needs a database/catalog namespace; no local target inferred",
                    parts.join(".")
                ),
                source,
            );
            return Relation {
                unknown: true,
                ..Relation::default()
            };
        };
        let matches = self
            .catalog
            .tables
            .get(&(schema.clone(), table.clone()))
            .cloned()
            .unwrap_or_default();
        if matches.len() != 1 {
            let hint = if matches.is_empty() {
                super::case_only_hint(
                    self.graph,
                    self.catalog.dialect,
                    &VertexId::object(&schema, &table),
                    self.catalog.lookup(false),
                )
                .map(|hint| format!("; {hint}"))
                .unwrap_or_default()
            } else {
                String::new()
            };
            self.note(
                "SG_RELATION_UNRESOLVED",
                format!("relation '{schema}.{table}' is missing or ambiguous in the catalog{hint}"),
                source,
            );
            return Relation {
                unknown: true,
                ..Relation::default()
            };
        }
        let (actual_schema, object) = matches[0];
        let kind = match object.kind.as_str() {
            "view" => VertexKind::View,
            "materialized-view" => VertexKind::MaterializedView,
            "synonym" => VertexKind::Synonym,
            "sequence" => VertexKind::Sequence,
            _ => VertexKind::Table,
        };
        let Some(id) = super::resolve_object(
            self.graph,
            actual_schema,
            &object.name,
            kind,
            &object.kind,
            self.catalog.lookup(false),
        ) else {
            self.note(
                "SG_RELATION_UNRESOLVED",
                format!(
                    "catalog relation '{actual_schema}.{}' has no matching graph vertex",
                    object.name
                ),
                source,
            );
            return Relation {
                unknown: true,
                ..Relation::default()
            };
        };
        self.read(id, "relation", source);
        let mut columns: Vec<_> = object.columns.iter().collect();
        columns.sort_by_key(|c| c.ordinal);
        let columns = columns
            .into_iter()
            .map(|column| {
                let sources = super::resolve_member(
                    self.graph,
                    actual_schema,
                    &object.name,
                    &column.name,
                    VertexKind::Column,
                    "column",
                    self.catalog.lookup(false),
                )
                .into_iter()
                .collect::<Sources>();
                Column {
                    name: column_key(self.catalog.dialect, &column.name),
                    unknown: sources.is_empty(),
                    sources,
                    location: None,
                }
            })
            .collect();
        Relation {
            columns,
            ..Relation::default()
        }
    }

    fn alias(&mut self, relation: &mut Relation, alias: &TableAlias) {
        relation.aliases = vec![vec![name(self.catalog.dialect, &alias.name)]];
        if alias.columns.len() > relation.columns.len() {
            relation.unknown = true;
            self.note(
                "SG_ALIAS_WIDTH",
                "column aliases exceed the known output width".into(),
                None,
            );
        }
        for (column, alias) in relation.columns.iter_mut().zip(&alias.columns) {
            column.name = column_name(self.catalog.dialect, &alias.name);
        }
    }

    fn resolve(
        &mut self,
        ids: &[Ident],
        scope: &Scope,
        source: Option<SourceLocation>,
        aliases: &[Column],
    ) -> Sources {
        let parts: Vec<_> = ids
            .iter()
            .enumerate()
            .map(|(index, id)| {
                if index + 1 == ids.len() {
                    column_name(self.catalog.dialect, id)
                } else {
                    name(self.catalog.dialect, id)
                }
            })
            .collect();
        if parts.len() == 2 {
            let pseudo = procedural_symbol_key(&parts[0]);
            if let Some(relation) = self.pseudo_relations.get(&pseudo) {
                let matches: Vec<_> = relation
                    .columns
                    .iter()
                    .filter(|column| column.name == parts[1])
                    .collect();
                return match matches.as_slice() {
                    [column] if !column.unknown => column.sources.clone(),
                    _ => {
                        self.note(
                            "SG_COLUMN_UNRESOLVED",
                            format!(
                                "transition column '{}.{}' is missing or ambiguous",
                                parts[0], parts[1]
                            ),
                            source,
                        );
                        Sources::new()
                    }
                };
            }
        }
        let Some(column) = parts.last() else {
            return Sources::new();
        };
        let qualifier = &parts[..parts.len() - 1];
        let mut current = Some(scope);
        while let Some(scope) = current {
            let candidates: Vec<_> = if qualifier.is_empty() {
                scope.columns.iter().filter(|c| &c.name == column).collect()
            } else {
                scope
                    .relations
                    .iter()
                    .filter(|r| r.aliases.iter().any(|a| a == qualifier))
                    .flat_map(|r| &r.columns)
                    .filter(|c| &c.name == column)
                    .collect()
            };
            let unknown = if qualifier.is_empty() {
                scope.relations.iter().any(|r| r.unknown)
            } else {
                scope
                    .relations
                    .iter()
                    .any(|r| r.aliases.iter().any(|a| a == qualifier) && r.unknown)
            };
            if candidates.len() == 1 && !unknown && !candidates[0].unknown {
                return candidates[0].sources.clone();
            }
            if !candidates.is_empty() || unknown {
                self.note(
                    "SG_COLUMN_AMBIGUOUS",
                    format!(
                        "column '{}' is ambiguous or has unresolved sources",
                        parts.join(".")
                    ),
                    source,
                );
                return Sources::new();
            }
            // 로컬 별칭이 존재하면 컬럼 누락을 외부 동명 별칭으로 덮지 않는다.
            if !qualifier.is_empty()
                && scope
                    .relations
                    .iter()
                    .any(|r| r.aliases.iter().any(|a| a == qualifier))
            {
                break;
            }
            current = scope.outer.as_deref();
        }
        if qualifier.is_empty() {
            let matched: Vec<_> = aliases.iter().filter(|c| &c.name == column).collect();
            if matched.len() == 1 && !matched[0].unknown {
                return matched[0].sources.clone();
            }
        }
        if ids.len() == 1
            && ids[0].quote_style.is_none()
            && self
                .scalar_symbols
                .contains(&procedural_symbol_key(&ids[0].value))
        {
            return Sources::new();
        }
        self.note(
            "SG_COLUMN_UNRESOLVED",
            format!(
                "column '{}' cannot be resolved in its SQL scope",
                parts.join(".")
            ),
            source,
        );
        Sources::new()
    }

    fn visit<T: Visit>(
        &mut self,
        node: &T,
        scope: &Rc<Scope>,
        ctes: &Ctes,
        role: &str,
        aliases: &[Column],
        windows: &NamedWindows,
    ) -> Sources {
        self.visit_with_window_stack(node, scope, ctes, role, aliases, (windows, &mut Vec::new()))
    }

    fn visit_with_window_stack<T: Visit>(
        &mut self,
        node: &T,
        scope: &Rc<Scope>,
        ctes: &Ctes,
        role: &str,
        aliases: &[Column],
        window_context: (&NamedWindows, &mut Vec<String>),
    ) -> Sources {
        let (windows, window_stack) = window_context;
        let mut visitor = ExprVisitor {
            binder: self,
            scope: scope.clone(),
            ctes,
            role,
            aliases,
            windows,
            window_stack,
            queries: 0,
            expression_depth: 0,
            pseudo_access_roots: 0,
            skip_raise_action: false,
            sources: Sources::new(),
        };
        let _ = node.visit(&mut visitor);
        visitor.sources
    }
}

/// 절차형 DML의 컬럼 효과를 현재 graph의 실제 member 정점에만 투영한다.
///
/// 관계 수준 Reads/Writes는 기존 parser가 유지한다. 이 분석기는 그보다
/// 좁은 대상 컬럼 Writes, RHS/predicate Reads, 값의 DerivesFrom만 추가한다.
/// 해석할 수 없는 target shape·wildcard·control flow는 추측하지 않고
/// `partial` 진단으로 남긴다.
pub(crate) fn apply_dml_column_effects(
    graph: &mut Graph,
    catalog: &CatalogIndex<'_>,
    schema: &str,
    owner: &VertexId,
    statements: &[Statement],
    options: DmlApplyOptions<'_>,
    diagnostics: &mut Vec<Diagnostic>,
) -> bool {
    if statements.is_empty() {
        return false;
    }
    let mut analyzer = DmlAnalyzer {
        binder: Binder {
            catalog,
            graph: &*graph,
            schema,
            search_path: options.search_path,
            reads: BTreeMap::new(),
            calls: BTreeMap::new(),
            diagnostics: Vec::new(),
            depth: 0,
            window_expansions: 0,
            scalar_symbols: options.scalar_symbols.clone(),
            sqlite_trigger: catalog.dialect == "sqlite"
                && graph
                    .vertex(owner)
                    .is_some_and(|vertex| vertex.kind == VertexKind::Trigger),
            pseudo_relations: BTreeMap::new(),
            local_relations: BTreeMap::new(),
        },
        owner,
        diagnostics,
        partial: false,
        effects: Vec::new(),
        allow_local_temps: options.allow_local_temps,
        locations_trusted: options.locations_trusted,
    };
    if let Some((transition_table, transition_names)) = options.transition {
        let relation = analyzer.binder.table(&[transition_table.to_owned()], None);
        for name in transition_names {
            analyzer
                .binder
                .pseudo_relations
                .insert(procedural_symbol_key(name), relation.clone());
            let normalized = match analyzer.binder.catalog.dialect {
                "oracle" | "db2" => name.to_uppercase(),
                "postgres" | "postgresql" | "sqlite" => name.to_lowercase(),
                _ => (*name).to_owned(),
            };
            analyzer
                .binder
                .local_relations
                .insert(vec![normalized], relation.clone());
        }
    }
    for statement in statements {
        analyzer.statement(statement);
    }
    analyzer.flush_reads();
    let effects = std::mem::take(&mut analyzer.effects);
    let binder_diagnostics = std::mem::take(&mut analyzer.binder.diagnostics);
    for mut diagnostic in binder_diagnostics {
        if !options.locations_trusted {
            diagnostic.location = None;
        }
        analyzer.note_diagnostic(diagnostic);
    }
    let partial = analyzer.partial;
    drop(analyzer);
    for effect in effects {
        graph.add_edge(Edge {
            from: effect.from.clone(),
            to: effect.to.clone(),
            kind: effect.kind,
            evidence: vec![Evidence {
                layer: EvidenceLayer::BodyParse,
                detail: effect.detail,
            }],
        });
        let key = (effect.from, effect.to, effect.kind);
        catalog.record_dml_origin(owner, key.clone());
        graph.add_origin(
            key,
            Origin {
                body_hash: options.body_hash.to_owned(),
                role: effect.role,
                location: effect.location,
            },
        );
    }
    partial
}

pub(crate) struct DmlApplyOptions<'a> {
    pub(crate) body_hash: &'a str,
    pub(crate) transition: Option<(&'a str, &'a [&'a str])>,
    pub(crate) allow_local_temps: bool,
    pub(crate) locations_trusted: bool,
    pub(crate) scalar_symbols: &'a BTreeSet<String>,
    /// routine 정의문이 선언한 한 부분 이름의 해석 순서.
    pub(crate) search_path: Option<&'a super::SearchPath>,
}

struct DmlEffect {
    from: VertexId,
    to: VertexId,
    kind: EdgeKind,
    detail: String,
    role: String,
    location: Option<SourceLocation>,
}

struct DmlAnalyzer<'a, 'd> {
    binder: Binder<'a, 'd>,
    owner: &'a VertexId,
    diagnostics: &'a mut Vec<Diagnostic>,
    partial: bool,
    effects: Vec<DmlEffect>,
    allow_local_temps: bool,
    locations_trusted: bool,
}

impl DmlAnalyzer<'_, '_> {
    fn statement(&mut self, statement: &Statement) {
        match statement {
            Statement::Insert(insert) => self.insert(insert),
            Statement::Update {
                table,
                assignments,
                from,
                selection,
                ..
            } => self.update(table, assignments, from.as_ref(), selection.as_ref()),
            Statement::Merge {
                table,
                source,
                on,
                clauses,
                ..
            } => self.merge(table, source, on, clauses),
            Statement::CreateTable(create) => {
                if let Some(query) = &create.query {
                    self.ctas(create, query);
                }
            }
            Statement::Query(query) => self.select_into(query),
            _ => {}
        }
    }

    fn relation_from_name(&mut self, name: &ObjectName, alias: Option<&Ident>) -> Relation {
        let parts = self.binder.parts(name);
        let mut relation = self
            .binder
            .local_relations
            .get(&parts)
            .cloned()
            .unwrap_or_else(|| self.binder.table(&parts, None));
        if let Some(alias) = alias {
            relation.aliases = vec![vec![name_for_dialect(self.binder.catalog.dialect, alias)]];
        } else if relation.aliases.is_empty() {
            relation.aliases = vec![parts.clone()];
            if let Some(last) = parts.last() {
                relation.aliases.push(vec![last.clone()]);
            }
        }
        relation
    }

    fn target_from_insert(&mut self, insert: &sqlparser::ast::Insert) -> Option<Relation> {
        let TableObject::TableName(name) = &insert.table else {
            self.note(
                "SG_DML_TARGET_SHAPE",
                "INSERT target is a table function or generated relation; target columns omitted",
            );
            return None;
        };
        Some(self.relation_from_name(name, insert.table_alias.as_ref()))
    }

    fn insert(&mut self, insert: &sqlparser::ast::Insert) {
        let Some(target) = self.target_from_insert(insert) else {
            return;
        };
        let local_target = match &insert.table {
            TableObject::TableName(name) => {
                let parts = self.binder.parts(name);
                self.binder
                    .local_relations
                    .contains_key(&parts)
                    .then_some(parts)
            }
            TableObject::TableFunction(_) => None,
        }
        .filter(|_| self.allow_local_temps);
        let mut scope = Scope::default();
        scope.add(target.clone());
        let visible = Rc::new(scope.clone());
        if let Some(query) = &insert.source {
            let result = self.binder.query(query, &Ctes::new(), Some(visible));
            if result.unknown || result.columns.iter().any(|column| column.unknown) {
                self.note(
                    "SG_DML_SOURCE_SHAPE",
                    "INSERT SELECT source shape is unresolved; target lineage omitted",
                );
                return;
            }
            if let Some(local_target) = &local_target {
                self.append_local_columns(local_target, &insert.columns, &result.columns);
                return;
            }
            let target_columns =
                self.target_columns(&target, &insert.columns, result.columns.len());
            let Some(target_columns) = target_columns else {
                return;
            };
            for (target_column, output) in target_columns.into_iter().zip(result.columns) {
                self.value_effect(target_column, &output.sources, output.location);
            }
            return;
        }
        if !insert.assignments.is_empty() {
            let scope = Rc::new(scope);
            for assignment in &insert.assignments {
                let sources = self.binder.visit(
                    &assignment.value,
                    &scope,
                    &Ctes::new(),
                    "value",
                    &[],
                    &BTreeMap::new(),
                );
                if let Some(local_target) = &local_target {
                    let Some(position) = self
                        .assignment_positions(&target, &assignment.target)
                        .and_then(|positions| (positions.len() == 1).then_some(positions[0]))
                    else {
                        continue;
                    };
                    self.merge_local_column(local_target, position, &sources, false);
                    continue;
                }
                let Some(target_column) = self.assignment_target(&target, &assignment.target)
                else {
                    continue;
                };
                self.value_effect(target_column, &sources, None);
            }
            return;
        }
        if insert.source.is_none() {
            self.note(
                "SG_DML_SOURCE_SHAPE",
                "INSERT has no explicit value source; generated/default target shape is omitted",
            );
        }
    }

    fn update(
        &mut self,
        table: &TableWithJoins,
        assignments: &[sqlparser::ast::Assignment],
        from: Option<&UpdateTableFromKind>,
        selection: Option<&Expr>,
    ) {
        let from_tables: &[TableWithJoins] = from
            .map(|from| match from {
                UpdateTableFromKind::BeforeSet(tables) | UpdateTableFromKind::AfterSet(tables) => {
                    tables.as_slice()
                }
            })
            .unwrap_or(&[]);
        let promoted = promoted_update_target(&table.relation, from_tables);
        let target_factor = promoted.cloned().unwrap_or_else(|| table.relation.clone());
        let mut scope = Scope::default();
        let target_table = TableWithJoins {
            relation: target_factor.clone(),
            joins: if promoted.is_none() {
                table.joins.clone()
            } else {
                Vec::new()
            },
        };
        self.binder.join(&target_table, &mut scope, &Ctes::new());
        let local_target = match &target_factor {
            TableFactor::Table { name, .. } => {
                let parts = self.binder.parts(name);
                self.binder
                    .local_relations
                    .contains_key(&parts)
                    .then_some(parts)
            }
            _ => None,
        }
        .filter(|_| self.allow_local_temps);
        if from.is_some() {
            for from_table in from_tables {
                if same_update_target(&target_factor, &from_table.relation)
                    && from_table.joins.is_empty()
                {
                    continue;
                }
                if same_update_target(&target_factor, &from_table.relation) {
                    self.add_update_sources(&from_table.joins, &mut scope);
                } else {
                    self.binder.join(from_table, &mut scope, &Ctes::new());
                }
            }
        }
        let target = scope.relations.first().cloned();
        let scope = Rc::new(scope);
        for assignment in assignments {
            let Some(target) = target.as_ref() else {
                self.note(
                    "SG_DML_TARGET_SHAPE",
                    "UPDATE target relation is unresolved",
                );
                break;
            };
            self.update_assignment(target, local_target.as_deref(), assignment, &scope);
        }
        if let Some(selection) = selection {
            self.binder.visit(
                selection,
                &scope,
                &Ctes::new(),
                "predicate",
                &[],
                &BTreeMap::new(),
            );
        }
    }

    fn update_assignment(
        &mut self,
        target: &Relation,
        local_target: Option<&[String]>,
        assignment: &sqlparser::ast::Assignment,
        scope: &Rc<Scope>,
    ) {
        let Some(positions) = self.assignment_positions(target, &assignment.target) else {
            return;
        };
        let outputs = if positions.len() == 1 {
            let diagnostics_before = self.binder.diagnostics.len();
            let sources = self.binder.visit(
                &assignment.value,
                scope,
                &Ctes::new(),
                "value",
                &[],
                &BTreeMap::new(),
            );
            Some(vec![Column {
                sources,
                unknown: self.binder.diagnostics.len() > diagnostics_before,
                location: location(&assignment.value),
                ..Column::default()
            }])
        } else {
            self.tuple_assignment_outputs(&assignment.value, scope)
        };
        let Some(outputs) = outputs else {
            return;
        };
        if outputs.len() != positions.len() {
            self.note(
                "SG_DML_TARGET_SHAPE",
                format!(
                    "tuple assignment width differs (target {}, source {})",
                    positions.len(),
                    outputs.len()
                ),
            );
            return;
        }
        for (position, output) in positions.into_iter().zip(outputs) {
            if let Some(local_target) = local_target {
                self.merge_local_column(local_target, position, &output.sources, output.unknown);
            } else if let Some(target_column) = self.physical_column_at(target, position) {
                self.value_effect(target_column, &output.sources, output.location);
            } else {
                self.note(
                    "SG_DML_TARGET_SHAPE",
                    "assignment target is not a collected physical column",
                );
            }
        }
    }

    fn tuple_assignment_outputs(&mut self, value: &Expr, scope: &Rc<Scope>) -> Option<Vec<Column>> {
        match value {
            Expr::Subquery(query) => {
                let result = self.binder.query(query, &Ctes::new(), Some(scope.clone()));
                (!result.unknown).then_some(result.columns)
            }
            Expr::Tuple(expressions) => Some(
                expressions
                    .iter()
                    .map(|expression| {
                        let diagnostics_before = self.binder.diagnostics.len();
                        let sources = self.binder.visit(
                            expression,
                            scope,
                            &Ctes::new(),
                            "value",
                            &[],
                            &BTreeMap::new(),
                        );
                        Column {
                            sources,
                            unknown: self.binder.diagnostics.len() > diagnostics_before,
                            location: location(expression),
                            ..Column::default()
                        }
                    })
                    .collect(),
            ),
            _ => {
                self.note(
                    "SG_DML_SOURCE_SHAPE",
                    "tuple assignment source is not a positional query or tuple",
                );
                None
            }
        }
    }

    fn add_update_sources(&mut self, joins: &[sqlparser::ast::Join], scope: &mut Scope) {
        for join in joins {
            let right = self
                .binder
                .factor(&join.relation, &Ctes::new(), Rc::new(scope.clone()));
            let mut combined = scope.clone();
            combined.add(right.clone());
            match &join.join_operator {
                sqlparser::ast::JoinOperator::Join(constraint)
                | sqlparser::ast::JoinOperator::Inner(constraint)
                | sqlparser::ast::JoinOperator::Left(constraint)
                | sqlparser::ast::JoinOperator::Right(constraint)
                | sqlparser::ast::JoinOperator::LeftOuter(constraint)
                | sqlparser::ast::JoinOperator::RightOuter(constraint)
                | sqlparser::ast::JoinOperator::FullOuter(constraint)
                | sqlparser::ast::JoinOperator::StraightJoin(constraint) => {
                    self.visit_update_join_constraint(constraint, &combined)
                }
                _ => {}
            }
            scope.add(right);
        }
    }

    fn visit_update_join_constraint(
        &mut self,
        constraint: &sqlparser::ast::JoinConstraint,
        scope: &Scope,
    ) {
        match constraint {
            sqlparser::ast::JoinConstraint::On(expression) => {
                self.binder.visit(
                    expression,
                    &Rc::new(scope.clone()),
                    &Ctes::new(),
                    "join",
                    &[],
                    &BTreeMap::new(),
                );
            }
            sqlparser::ast::JoinConstraint::Using(columns) => {
                for column in columns {
                    let ids: Vec<_> = column
                        .0
                        .iter()
                        .filter_map(|part| part.as_ident().cloned())
                        .collect();
                    let sources = self.binder.resolve(&ids, scope, None, &[]);
                    for source in sources {
                        self.binder.read(source, "join", None);
                    }
                }
            }
            sqlparser::ast::JoinConstraint::Natural => {
                for left in &scope.columns {
                    if scope.relations.iter().any(|relation| {
                        relation.columns.iter().any(|right| right.name == left.name)
                    }) {
                        for source in &left.sources {
                            self.binder.read(source.clone(), "join", None);
                        }
                    }
                }
            }
            sqlparser::ast::JoinConstraint::None => {}
        }
    }

    fn merge(
        &mut self,
        table: &TableFactor,
        source: &TableFactor,
        on: &Expr,
        clauses: &[sqlparser::ast::MergeClause],
    ) {
        let mut scope = Scope::default();
        let visible = Rc::new(scope.clone());
        self.binder
            .add_factor(table, &mut scope, &Ctes::new(), visible);
        let visible = Rc::new(scope.clone());
        self.binder
            .add_factor(source, &mut scope, &Ctes::new(), visible);
        let target = scope.relations.first().cloned();
        let scope = Rc::new(scope);
        self.binder
            .visit(on, &scope, &Ctes::new(), "predicate", &[], &BTreeMap::new());
        for clause in clauses {
            if let Some(predicate) = &clause.predicate {
                self.binder.visit(
                    predicate,
                    &scope,
                    &Ctes::new(),
                    "predicate",
                    &[],
                    &BTreeMap::new(),
                );
            }
            let Some(target) = target.as_ref() else {
                self.note("SG_DML_TARGET_SHAPE", "MERGE target relation is unresolved");
                continue;
            };
            match &clause.action {
                MergeAction::Update { assignments } => {
                    for assignment in assignments {
                        let Some(target_column) =
                            self.assignment_target(target, &assignment.target)
                        else {
                            continue;
                        };
                        let sources = self.binder.visit(
                            &assignment.value,
                            &scope,
                            &Ctes::new(),
                            "value",
                            &[],
                            &BTreeMap::new(),
                        );
                        self.value_effect(target_column, &sources, None);
                    }
                }
                MergeAction::Insert(insert) => match &insert.kind {
                    MergeInsertKind::Values(values) if values.rows.len() == 1 => {
                        let target_columns =
                            self.target_columns(target, &insert.columns, values.rows[0].len());
                        let Some(target_columns) = target_columns else {
                            continue;
                        };
                        for (target_column, value) in
                            target_columns.into_iter().zip(&values.rows[0])
                        {
                            let sources = self.binder.visit(
                                value,
                                &scope,
                                &Ctes::new(),
                                "value",
                                &[],
                                &BTreeMap::new(),
                            );
                            self.value_effect(target_column, &sources, None);
                        }
                    }
                    MergeInsertKind::Values(_) => self.note(
                        "SG_DML_SOURCE_SHAPE",
                        "MERGE INSERT with multiple VALUES rows is not mapped",
                    ),
                    MergeInsertKind::Row => self.note(
                        "SG_DML_TARGET_SHAPE",
                        "MERGE INSERT ROW has an implicit target shape; lineage omitted",
                    ),
                },
                MergeAction::Delete => {}
            }
        }
    }

    fn ctas(&mut self, create: &sqlparser::ast::CreateTable, query: &Query) {
        let result = self.binder.query(query, &Ctes::new(), None);
        if result.unknown || result.columns.iter().any(|column| column.unknown) {
            self.note(
                "SG_DML_SOURCE_SHAPE",
                "CTAS source shape is unresolved; target lineage omitted",
            );
            return;
        }
        let parts = self.binder.parts(&create.name);
        let mut local = result.clone();
        if !create.columns.is_empty() {
            if create.columns.len() != local.columns.len() {
                self.note(
                    "SG_DML_TARGET_SHAPE",
                    "CTAS column list width differs from projection",
                );
                return;
            }
            for (column, definition) in local.columns.iter_mut().zip(&create.columns) {
                column.name = column_name(self.binder.catalog.dialect, &definition.name);
            }
        }
        if create.temporary {
            if !self.allow_local_temps {
                self.note(
                    "SG_DML_TEMP_STATE",
                    "temporary relation crosses unknown control flow or state change; lineage omitted",
                );
                return;
            }
            self.binder.local_relations.insert(parts, local);
            return;
        }
        let target = self.relation_from_name(&create.name, None);
        if target.unknown || target.columns.iter().any(|column| column.unknown) {
            self.binder.local_relations.insert(parts, local);
            self.note(
                "SG_DML_TARGET_SHAPE",
                "CTAS target is not a collected relation; target lineage omitted",
            );
            return;
        }
        let target_names: Vec<_> = create
            .columns
            .iter()
            .map(|column| column.name.clone())
            .collect();
        let target_columns = if target_names.is_empty() {
            self.positional_target_columns(&target, result.columns.len())
        } else {
            self.target_columns(&target, &target_names, result.columns.len())
        };
        let Some(target_columns) = target_columns else {
            return;
        };
        for (target_column, output) in target_columns.into_iter().zip(result.columns) {
            self.value_effect(target_column, &output.sources, output.location);
        }
    }

    fn select_into(&mut self, query: &Query) {
        let Some(into) = super::query_select_into(query) else {
            return;
        };
        let result = self.binder.query(query, &Ctes::new(), None);
        if result.unknown || result.columns.iter().any(|column| column.unknown) {
            self.note(
                "SG_DML_SOURCE_SHAPE",
                "SELECT INTO source shape is unresolved; target lineage omitted",
            );
            return;
        }
        let parts = self.binder.parts(&into.name);
        let local = into.temporary || parts.last().is_some_and(|key| key.starts_with('#'));
        if local {
            if !self.allow_local_temps {
                self.note(
                    "SG_DML_TEMP_STATE",
                    "temporary SELECT INTO crosses an unknown state change; lineage omitted",
                );
                return;
            }
            self.binder.local_relations.insert(parts, result);
            return;
        }
        let target = self.relation_from_name(&into.name, None);
        let Some(target_columns) = self.positional_target_columns(&target, result.columns.len())
        else {
            return;
        };
        for (target_column, output) in target_columns.into_iter().zip(result.columns) {
            self.value_effect(target_column, &output.sources, output.location);
        }
    }

    fn append_local_columns(&mut self, key: &[String], names: &[Ident], outputs: &[Column]) {
        let Some(mut local) = self.binder.local_relations.get(key).cloned() else {
            self.note(
                "SG_DML_TEMP_STATE",
                "temporary INSERT target is not in local state",
            );
            return;
        };
        if !names.is_empty() && names.len() != outputs.len() {
            self.note(
                "SG_DML_TARGET_SHAPE",
                format!(
                    "temporary target column width differs (target {}, source {})",
                    names.len(),
                    outputs.len()
                ),
            );
            return;
        }
        let positions: Vec<usize> = if names.is_empty() {
            (0..local.columns.len()).collect()
        } else {
            let mut positions = Vec::with_capacity(names.len());
            for name in names {
                let wanted = column_name(self.binder.catalog.dialect, name);
                let Some(position) = local
                    .columns
                    .iter()
                    .position(|column| column.name == wanted)
                else {
                    self.note(
                        "SG_DML_TARGET_SHAPE",
                        format!("temporary target column '{}' is missing", name),
                    );
                    return;
                };
                positions.push(position);
            }
            positions
        };
        if positions.len() != outputs.len() {
            self.note(
                "SG_DML_TARGET_SHAPE",
                "temporary target column shape is ambiguous or width differs",
            );
            return;
        }
        for (position, output) in positions.into_iter().zip(outputs) {
            local.columns[position]
                .sources
                .extend(output.sources.clone());
            local.columns[position].unknown |= output.unknown;
        }
        self.binder.local_relations.insert(key.to_vec(), local);
    }

    fn merge_local_column(
        &mut self,
        key: &[String],
        position: usize,
        sources: &Sources,
        unknown: bool,
    ) {
        let Some(mut local) = self.binder.local_relations.get(key).cloned() else {
            self.note(
                "SG_DML_TEMP_STATE",
                "temporary UPDATE target is not in local state",
            );
            return;
        };
        let Some(column) = local.columns.get_mut(position) else {
            self.note(
                "SG_DML_TARGET_SHAPE",
                "temporary target column is no longer addressable",
            );
            return;
        };
        column.sources.extend(sources.clone());
        column.unknown |= unknown;
        self.binder.local_relations.insert(key.to_vec(), local);
    }

    fn target_columns(
        &mut self,
        target: &Relation,
        names: &[Ident],
        width: usize,
    ) -> Option<Vec<VertexId>> {
        if names.is_empty() {
            self.note(
                "SG_DML_TARGET_SHAPE",
                "INSERT target columns are implicit; insertable catalog shape is unknown",
            );
            return None;
        }
        if names.len() != width {
            self.note(
                "SG_DML_TARGET_SHAPE",
                format!(
                    "target column shape is ambiguous or width differs (target {}, source {})",
                    names.len(),
                    width
                ),
            );
            return None;
        }
        let mut columns = Vec::with_capacity(names.len());
        for name in names {
            let assignment_target =
                AssignmentTarget::ColumnName(ObjectName(vec![ObjectNamePart::Identifier(
                    name.clone(),
                )]));
            columns.push(self.assignment_target(target, &assignment_target)?);
        }
        Some(columns)
    }

    fn positional_target_columns(
        &mut self,
        target: &Relation,
        width: usize,
    ) -> Option<Vec<VertexId>> {
        let columns = self.known_columns(target);
        if columns.len() != width {
            self.note(
                "SG_DML_TARGET_SHAPE",
                format!(
                    "target column shape is ambiguous or width differs (target {}, source {})",
                    columns.len(),
                    width
                ),
            );
            return None;
        }
        Some(columns)
    }

    fn known_columns(&self, relation: &Relation) -> Vec<VertexId> {
        relation
            .columns
            .iter()
            .filter(|column| !column.unknown && column.sources.len() == 1)
            .filter_map(|column| column.sources.iter().next().cloned())
            .collect()
    }

    fn assignment_target(
        &mut self,
        relation: &Relation,
        target: &AssignmentTarget,
    ) -> Option<VertexId> {
        let positions = self.assignment_positions(relation, target)?;
        if positions.len() != 1 {
            self.note(
                "SG_DML_TARGET_SHAPE",
                "tuple target requires positional assignment",
            );
            return None;
        }
        self.physical_column_at(relation, positions[0])
    }

    fn assignment_positions(
        &mut self,
        relation: &Relation,
        target: &AssignmentTarget,
    ) -> Option<Vec<usize>> {
        let names: Vec<&ObjectName> = match target {
            AssignmentTarget::ColumnName(name) => vec![name],
            AssignmentTarget::Tuple(names) => names.iter().collect(),
        };
        let mut positions = Vec::with_capacity(names.len());
        for name in names {
            let parts = self.binder.column_parts(name);
            let Some(column_name) = parts.last() else {
                self.note("SG_DML_TARGET_SHAPE", "empty assignment target");
                return None;
            };
            if parts.len() > 1
                && !relation
                    .aliases
                    .iter()
                    .any(|alias| alias == &parts[..parts.len() - 1])
            {
                self.note(
                    "SG_DML_TARGET_SHAPE",
                    format!(
                        "assignment target qualifier '{}' is not the target relation",
                        name
                    ),
                );
                return None;
            }
            let matches: Vec<_> = relation
                .columns
                .iter()
                .enumerate()
                .filter(|(_, column)| &column.name == column_name)
                .map(|(position, _)| position)
                .collect();
            match matches.as_slice() {
                [position] => positions.push(*position),
                _ => {
                    self.note(
                        "SG_DML_TARGET_SHAPE",
                        format!(
                            "assignment target column '{}' is missing or ambiguous",
                            name
                        ),
                    );
                    return None;
                }
            }
        }
        Some(positions)
    }

    fn physical_column_at(&self, relation: &Relation, position: usize) -> Option<VertexId> {
        relation
            .columns
            .get(position)
            .filter(|column| !column.unknown && column.sources.len() == 1)
            .and_then(|column| column.sources.iter().next().cloned())
    }

    fn write_only(&mut self, target: VertexId) {
        self.edge(target, EdgeKind::Writes, "write", None);
    }

    fn value_effect(
        &mut self,
        target: VertexId,
        sources: &Sources,
        location: Option<SourceLocation>,
    ) {
        self.write_only(target.clone());
        for source in sources {
            if source == &target {
                self.note(
                    "SG_TEMPORAL_SELF_LINEAGE",
                    format!("{target} reads its previous value; self DerivesFrom edge omitted"),
                );
                continue;
            }
            self.edge_with_location(
                target.clone(),
                source.clone(),
                EdgeKind::DerivesFrom,
                "value",
                location.clone(),
            );
        }
    }

    fn flush_reads(&mut self) {
        let reads = std::mem::take(&mut self.binder.reads);
        for (source, origins) in reads {
            if self
                .binder
                .graph
                .vertex(&source)
                .is_some_and(|vertex| vertex.kind == VertexKind::Column)
            {
                for (role, location) in origins {
                    self.edge_with_location(
                        self.owner.clone(),
                        source.clone(),
                        EdgeKind::Reads,
                        &role,
                        location,
                    );
                }
            }
        }
    }

    fn edge(
        &mut self,
        target: VertexId,
        kind: EdgeKind,
        role: &str,
        location: Option<SourceLocation>,
    ) {
        self.edge_with_location(self.owner.clone(), target, kind, role, location);
    }

    fn edge_with_location(
        &mut self,
        from: VertexId,
        to: VertexId,
        kind: EdgeKind,
        role: &str,
        location: Option<SourceLocation>,
    ) {
        if self.binder.graph.vertex(&from).is_none() || self.binder.graph.vertex(&to).is_none() {
            self.note(
                "SG_DML_GHOST_VERTEX",
                format!("DML effect endpoint {from}->{to} is absent from the graph"),
            );
            return;
        }
        let detail = format!("{} {kind:?} {} ({role})", self.owner, to);
        self.effects.push(DmlEffect {
            from,
            to,
            kind,
            detail,
            role: Origin::dml_owner_role(self.owner, role),
            location: self.locations_trusted.then_some(location).flatten(),
        });
    }

    fn note(&mut self, code: &str, message: impl Into<String>) {
        self.partial = true;
        self.diagnostics.push(Diagnostic {
            code: code.into(),
            message: message.into(),
            location: None,
            candidates: Vec::new(),
        });
    }

    fn note_diagnostic(&mut self, diagnostic: Diagnostic) {
        self.partial = true;
        self.diagnostics.push(diagnostic);
    }
}

fn name_for_dialect(dialect: &str, ident: &Ident) -> String {
    if ident.quote_style.is_some() {
        ident.value.clone()
    } else if matches!(dialect, "oracle" | "db2") {
        ident.value.to_uppercase()
    } else if matches!(dialect, "postgres" | "postgresql") {
        ident.value.to_lowercase()
    } else {
        ident.value.clone()
    }
}

fn same_update_target(left: &TableFactor, right: &TableFactor) -> bool {
    let (
        TableFactor::Table {
            name: left_name,
            alias: left_alias,
            args: left_args,
            ..
        },
        TableFactor::Table {
            name: right_name,
            alias: right_alias,
            args: right_args,
            ..
        },
    ) = (left, right)
    else {
        return false;
    };
    if left_args.is_some() || right_args.is_some() {
        return false;
    }
    left_name == right_name
        && left_alias.as_ref().map(|alias| &alias.name)
            == right_alias.as_ref().map(|alias| &alias.name)
}

/// SQL Server의 `UPDATE alias ... FROM physical alias`에서 별칭이 가리키는
/// 물리 관계가 하나뿐일 때만 대상 관계로 승격한다.
fn promoted_update_target<'a>(
    target: &TableFactor,
    from: &'a [TableWithJoins],
) -> Option<&'a TableFactor> {
    let TableFactor::Table {
        name,
        alias: None,
        args: None,
        ..
    } = target
    else {
        return None;
    };
    let [target_name] = name.0.as_slice() else {
        return None;
    };
    let target_name = target_name.as_ident()?.value.as_str();
    let matches: Vec<_> = from
        .iter()
        .filter_map(|table| match &table.relation {
            factor @ TableFactor::Table {
                alias: Some(alias),
                args: None,
                ..
            } if alias.name.value.eq_ignore_ascii_case(target_name) => Some(factor),
            _ => None,
        })
        .collect();
    match matches.as_slice() {
        [factor] => Some(*factor),
        _ => None,
    }
}

struct ExprVisitor<'a, 'b, 'c, 'd> {
    binder: &'a mut Binder<'b, 'd>,
    scope: Rc<Scope>,
    ctes: &'c Ctes,
    role: &'c str,
    aliases: &'c [Column],
    windows: &'c NamedWindows,
    window_stack: &'c mut Vec<String>,
    queries: usize,
    expression_depth: usize,
    pseudo_access_roots: usize,
    skip_raise_action: bool,
    sources: Sources,
}

impl Visitor for ExprVisitor<'_, '_, '_, '_> {
    type Break = ();
    fn pre_visit_query(&mut self, query: &Query) -> ControlFlow<()> {
        if self.queries == 0 {
            let result = self
                .binder
                .query(query, self.ctes, Some(self.scope.clone()));
            self.sources
                .extend(result.columns.into_iter().flat_map(|c| c.sources));
        }
        self.queries += 1;
        ControlFlow::Continue(())
    }
    fn post_visit_query(&mut self, _: &Query) -> ControlFlow<()> {
        self.queries -= 1;
        ControlFlow::Continue(())
    }
    fn pre_visit_expr(&mut self, expr: &Expr) -> ControlFlow<()> {
        if self.queries != 0 {
            return ControlFlow::Continue(());
        }
        let root_expression = self.expression_depth == 0;
        self.expression_depth += 1;
        // raise_action은 수식자 없는 첫 Identifier 인자만 허용한다. 바로 다음
        // Expr인 이 키워드만 건너뛰며 이후 메시지 식은 정상적으로 방문한다.
        if std::mem::take(&mut self.skip_raise_action) {
            return ControlFlow::Continue(());
        }
        if let Some((pseudo, field)) = pseudo_field_access(expr) {
            self.pseudo_access_roots += 1;
            let ids = [Ident::new(pseudo), field.clone()];
            let sources = self.binder.resolve(&ids, &self.scope, location(expr), &[]);
            for id in &sources {
                self.binder.read(id.clone(), self.role, location(expr));
            }
            self.sources.extend(sources);
            return ControlFlow::Continue(());
        }
        if let Expr::Function(function) = expr {
            if self.binder.sqlite_trigger && super::sqlite::raise_action(function).is_some() {
                self.skip_raise_action = true;
            } else {
                self.binder.call(&function.name, location(expr));
            }
            if let Some(over) = &function.over {
                let sources = match over {
                    WindowType::NamedWindow(reference) => self.binder.named_window_sources(
                        reference,
                        self.windows,
                        &self.scope,
                        self.ctes,
                        self.window_stack,
                    ),
                    WindowType::WindowSpec(spec) => spec
                        .window_name
                        .as_ref()
                        .map(|reference| {
                            self.binder.named_window_sources(
                                reference,
                                self.windows,
                                &self.scope,
                                self.ctes,
                                self.window_stack,
                            )
                        })
                        .unwrap_or_default(),
                };
                for id in &sources {
                    self.binder.read(id.clone(), self.role, location(expr));
                }
                self.sources.extend(sources);
            }
        }
        let ids = match expr {
            Expr::Identifier(_) if self.pseudo_access_roots > 0 => return ControlFlow::Continue(()),
            Expr::Identifier(id) => std::slice::from_ref(id),
            Expr::CompoundIdentifier(ids) => ids.as_slice(),
            _ => return ControlFlow::Continue(()),
        };
        let aliases = if root_expression || self.role == "predicate" {
            self.aliases
        } else {
            &[]
        };
        let projected: Vec<_> = if root_expression && self.role == "ordering" && ids.len() == 1 {
            aliases
                .iter()
                .filter(|c| {
                    c.name == column_name(self.binder.catalog.dialect, &ids[0]) && !c.unknown
                })
                .collect()
        } else {
            Vec::new()
        };
        let sources = if projected.len() == 1 {
            projected[0].sources.clone()
        } else {
            self.binder
                .resolve(ids, &self.scope, location(expr), aliases)
        };
        for id in &sources {
            self.binder.read(id.clone(), self.role, location(expr));
        }
        self.sources.extend(sources);
        ControlFlow::Continue(())
    }
    fn post_visit_expr(&mut self, expr: &Expr) -> ControlFlow<()> {
        if self.queries == 0 {
            if pseudo_field_access(expr).is_some() {
                self.pseudo_access_roots -= 1;
            }
            self.expression_depth -= 1;
        }
        ControlFlow::Continue(())
    }
}

fn pseudo_field_access(expr: &Expr) -> Option<(&str, &Ident)> {
    let Expr::CompoundFieldAccess { root, access_chain } = expr else {
        return None;
    };
    let Expr::Value(value) = root.as_ref() else {
        return None;
    };
    let Value::Placeholder(pseudo) = &value.value else {
        return None;
    };
    let [AccessExpr::Dot(Expr::Identifier(field))] = access_chain.as_slice() else {
        return None;
    };
    let pseudo = pseudo.trim_start_matches(':');
    (pseudo.eq_ignore_ascii_case("new") || pseudo.eq_ignore_ascii_case("old"))
        .then_some((pseudo, field))
}

fn procedural_symbol_key(value: &str) -> String {
    value
        .trim()
        .trim_start_matches(['@', ':'])
        .to_ascii_lowercase()
}

/// 뷰의 읽기 의존성과 출력 값의 계보를 실제 reader 정점에만 연결한다.
pub(crate) fn enrich_view(
    graph: &mut Graph,
    catalog: &CatalogIndex<'_>,
    schema: &str,
    object: &ObjectDoc,
    owner: &VertexId,
    dialect: &dyn Dialect,
) -> Vec<String> {
    let Some(body) = object.body.as_ref().filter(|s| !s.trim().is_empty()) else {
        graph.set_analysis(
            owner.clone(),
            ObjectAnalysis {
                source: None,
                state: AnalysisState::Unsupported,
                scope: "column-dependencies-and-lineage".into(),
                body_hash: None,
                diagnostics: vec![Diagnostic {
                    code: "SG_BODY_UNAVAILABLE".into(),
                    message: "view definition was not collected".into(),
                    location: None,
                    candidates: Vec::new(),
                }],
            },
        );
        return vec![format!(
            "view {owner}: definition unavailable; dependencies were not analyzed"
        )];
    };
    let hash = body_hash(body);
    let parsed = super::parse_limits::parse_sql(dialect, body);
    let mut binder = Binder {
        catalog,
        graph,
        schema,
        search_path: None,
        reads: BTreeMap::new(),
        calls: BTreeMap::new(),
        diagnostics: Vec::new(),
        depth: 0,
        window_expansions: 0,
        scalar_symbols: BTreeSet::new(),
        sqlite_trigger: false,
        pseudo_relations: BTreeMap::new(),
        local_relations: BTreeMap::new(),
    };
    let mut outputs = Vec::new();
    match parsed {
        Ok(statements) if statements.len() == 1 => {
            for statement in &statements {
                let query = match statement {
                    Statement::Query(q) | Statement::CreateView { query: q, .. } => q,
                    _ => {
                        binder.note(
                            "SG_VIEW_STATEMENT",
                            "view body is not a SELECT query".into(),
                            None,
                        );
                        continue;
                    }
                };
                outputs = binder.query(query, &Ctes::new(), None).columns;
            }
        }
        Ok(_) => binder.note(
            "SG_VIEW_STATEMENT",
            "a view definition must contain exactly one query".into(),
            None,
        ),
        Err(error) => binder.note(
            "SG_PARSE_ERROR",
            format!("view SQL parse failed: {error}"),
            None,
        ),
    }
    let diagnostics = binder.diagnostics;
    let reads = binder.reads;
    let calls = binder.calls;
    for (to, locations) in calls {
        graph.add_edge(Edge {
            from: owner.clone(),
            to: to.clone(),
            kind: EdgeKind::Calls,
            evidence: vec![Evidence {
                layer: EvidenceLayer::BodyParse,
                detail: format!("view {owner} calls {to}"),
            }],
        });
        for location in locations {
            graph.add_origin(
                (owner.clone(), to.clone(), EdgeKind::Calls),
                Origin {
                    body_hash: hash.clone(),
                    role: "call".into(),
                    location,
                },
            );
        }
    }
    for (to, origins) in reads {
        graph.add_edge(Edge {
            from: owner.clone(),
            to: to.clone(),
            kind: EdgeKind::Reads,
            evidence: vec![Evidence {
                layer: EvidenceLayer::BodyParse,
                detail: format!("view {owner} reads {to}"),
            }],
        });
        for (role, location) in origins {
            graph.add_origin(
                (owner.clone(), to.clone(), EdgeKind::Reads),
                Origin {
                    body_hash: hash.clone(),
                    role,
                    location,
                },
            );
        }
    }
    let mut columns: Vec<_> = object.columns.iter().collect();
    columns.sort_by_key(|c| c.ordinal);
    let width_matches = columns.len() == outputs.len();
    let mut diagnostics = diagnostics;
    if !columns.is_empty() && !width_matches {
        diagnostics.push(Diagnostic {
            code: "SG_OUTPUT_WIDTH".into(),
            message: "catalog output width differs from parsed projection; output lineage omitted"
                .into(),
            location: None,
            candidates: Vec::new(),
        });
    }
    let output_shape_known = !diagnostics.iter().any(|d| {
        matches!(
            d.code.as_str(),
            "SG_WILDCARD_UNKNOWN"
                | "SG_WILDCARD_MODIFIER"
                | "SG_WILDCARD_EXPRESSION"
                | "SG_SELECT_MODIFIER"
                | "SG_SET_WIDTH"
                | "SG_ALIAS_WIDTH"
                | "SG_SCOPE_DEPTH"
                | "SG_VIEW_STATEMENT"
                | "SG_JOIN_PROJECTION"
        ) || d.code.starts_with("SG_WINDOW_")
    });
    if width_matches && output_shape_known {
        for (column, output) in columns.into_iter().zip(outputs) {
            let Some(from) = super::resolve_member(
                graph,
                schema,
                &object.name,
                &column.name,
                VertexKind::Column,
                "column",
                catalog.lookup(false),
            ) else {
                continue;
            };
            for to in output.sources {
                graph.add_edge(Edge {
                    from: from.clone(),
                    to: to.clone(),
                    kind: EdgeKind::DerivesFrom,
                    evidence: vec![Evidence {
                        layer: EvidenceLayer::BodyParse,
                        detail: format!("{from} derives its value from {to}"),
                    }],
                });
                graph.add_origin(
                    (from.clone(), to, EdgeKind::DerivesFrom),
                    Origin {
                        body_hash: hash.clone(),
                        role: "value".into(),
                        location: output.location.clone(),
                    },
                );
            }
        }
    }
    let state = if diagnostics.is_empty() {
        AnalysisState::Complete
    } else if diagnostics.iter().any(|d| d.code == "SG_PARSE_ERROR") {
        AnalysisState::Unsupported
    } else {
        AnalysisState::Partial
    };
    let notes = diagnostics
        .iter()
        .map(|d| format!("view {owner}: {}: {}", d.code, d.message))
        .collect();
    graph.set_analysis(
        owner.clone(),
        ObjectAnalysis {
            source: None,
            state,
            scope: if object.columns.is_empty() {
                "column-dependencies"
            } else {
                "column-dependencies-and-lineage"
            }
            .into(),
            body_hash: Some(hash),
            diagnostics,
        },
    );
    notes
}
