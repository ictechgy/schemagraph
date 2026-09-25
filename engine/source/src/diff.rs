//! 두 catalog document의 카탈로그 델타 — `diff` 명령의 document 경로.
//!
//! usage는 비교하지 않는다 — 카운트·시각은 관측 부속물이라 항상 달라
//! 모든 객체가 "changed"로 나온다. 델타는 스키마 구조만이다.

use std::collections::BTreeMap;

use schemagraph_core::VertexId;
use serde::Serialize;

use crate::document::{
    CatalogDocument, ColumnDoc, ConstraintDoc, IndexDoc, RoutineDoc, TriggerDoc,
};

/// document diff 보고. `kind`는 "document" — graph diff와 구분하는 태그다.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DocumentDiff {
    pub kind: &'static str,
    /// 방언이 다르면 같은 이름 비교가 어긋날 수 있어 둘 다 실는다.
    pub dialects: [String; 2],
    pub summary: DiffSummary,
    pub schemas: NameDelta,
    pub objects: ObjectDelta,
    pub routines: RoutineDelta,
    /// 델타 자체의 한계 + 방언 불일치 경고. 입력 문서의 limitations는
    /// 스캔 시점의 관측 한계라 델타로 섞지 않는다.
    pub limitations: Vec<String>,
    #[serde(skip_serializing_if = "DependencyDelta::is_empty")]
    pub dependencies: DependencyDelta,
}

/// 새 카탈로그 증거의 추가·제거도 스냅샷 diff에서 누락하지 않는다.
#[derive(Debug, Serialize)]
pub struct DependencyDelta {
    pub added: Vec<crate::document::CatalogDependency>,
    pub removed: Vec<crate::document::CatalogDependency>,
}

impl DependencyDelta {
    fn is_empty(&self) -> bool {
        self.added.is_empty() && self.removed.is_empty()
    }
}

#[derive(Debug, Serialize)]
pub struct DiffSummary {
    pub added: usize,
    pub removed: usize,
    pub changed: usize,
}

#[derive(Debug, Serialize)]
pub struct NameDelta {
    pub added: Vec<String>,
    pub removed: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct ObjectDelta {
    pub added: Vec<IdKind>,
    pub removed: Vec<IdKind>,
    pub changed: Vec<ObjectChange>,
}

#[derive(Debug, Serialize)]
pub struct IdKind {
    /// schema.name — 정점 id와 같은 한정 형태다.
    pub id: String,
    pub kind: String,
}

#[derive(Debug, Serialize)]
pub struct FieldChange {
    pub old: String,
    pub new: String,
}

/// 이름으로 대응되는 컬렉션의 델타 — 같은 이름인데 정의가 다르면
/// old/new 전체를 실어 소비자가 필드 비교를 다시 안 해도 되게 한다.
#[derive(Debug, Serialize)]
pub struct CollectionDelta<T> {
    pub added: Vec<T>,
    pub removed: Vec<T>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub changed: Vec<PairChange<T>>,
}

impl<T> CollectionDelta<T> {
    fn is_empty(&self) -> bool {
        self.added.is_empty() && self.removed.is_empty() && self.changed.is_empty()
    }
}

#[derive(Debug, Serialize)]
pub struct PairChange<T> {
    pub name: String,
    pub old: T,
    pub new: T,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ObjectChange {
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<FieldChange>,
    #[serde(skip_serializing_if = "CollectionDelta::is_empty")]
    pub columns: CollectionDelta<ColumnDoc>,
    #[serde(skip_serializing_if = "CollectionDelta::is_empty")]
    pub constraints: CollectionDelta<ConstraintDoc>,
    #[serde(skip_serializing_if = "CollectionDelta::is_empty")]
    pub indexes: CollectionDelta<IndexDoc>,
    #[serde(skip_serializing_if = "CollectionDelta::is_empty")]
    pub triggers: CollectionDelta<TriggerDoc>,
    /// 몸체는 길 수 있어 바뀐 사실만 싣는다 — 원문은 문서에 있다.
    #[serde(skip_serializing_if = "is_false")]
    pub body_changed: bool,
}

#[derive(Debug, Serialize)]
pub struct RoutineDelta {
    pub added: Vec<IdKind>,
    pub removed: Vec<IdKind>,
    pub changed: Vec<RoutineChange>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RoutineChange {
    /// 새 문서에서의 graph routine id.
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<FieldChange>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub language: Option<FieldChange>,
    #[serde(skip_serializing_if = "is_false")]
    pub body_changed: bool,
}

fn is_false(b: &bool) -> bool {
    !*b
}

/// 이름으로 대응되는 컬렉션을 비교한다.
fn collect<T: Clone>(
    old: &[T],
    new: &[T],
    name: impl Fn(&T) -> &str,
    same: impl Fn(&T, &T) -> bool,
) -> CollectionDelta<T> {
    let old_by: BTreeMap<&str, &T> = old.iter().map(|x| (name(x), x)).collect();
    let new_by: BTreeMap<&str, &T> = new.iter().map(|x| (name(x), x)).collect();
    let mut delta = CollectionDelta {
        added: Vec::new(),
        removed: Vec::new(),
        changed: Vec::new(),
    };
    for (n, nv) in &new_by {
        match old_by.get(n) {
            Some(ov) if !same(ov, nv) => delta.changed.push(PairChange {
                name: (*n).to_owned(),
                old: (*ov).clone(),
                new: (*nv).clone(),
            }),
            Some(_) => {}
            None => delta.added.push((*nv).clone()),
        }
    }
    for (n, ov) in &old_by {
        if !new_by.contains_key(n) {
            delta.removed.push((*ov).clone());
        }
    }
    delta
}

/// routine의 graph 정규 id — document_to_graph와 같은 규칙이어야 diff
/// 산출물의 id가 그래프 id와 통한다.
fn routine_id(schema: &str, r: &RoutineDoc) -> String {
    VertexId::routine(
        schema,
        r.member_of.as_deref(),
        &r.name,
        r.signature.as_deref(),
    )
    .as_str()
    .to_owned()
}

/// 같은 이름의 테이블·함수·프로시저를 reader가 실제로 분리한 id로 비교한다.
fn routine_map(doc: &CatalogDocument) -> BTreeMap<String, &RoutineDoc> {
    use schemagraph_core::{VertexId, VertexKind};
    let graph = crate::graph::document_to_graph(doc);
    let mut result = BTreeMap::new();
    for schema in &doc.schemas {
        for routine in &schema.routines {
            let base = routine_id(&schema.name, routine);
            let (kind, suffix) = match routine.kind.as_str() {
                "procedure" => (VertexKind::Procedure, "procedure"),
                "package" => (VertexKind::Package, "package"),
                "query" => (VertexKind::Query, "query"),
                _ => (VertexKind::Function, "function"),
            };
            let renamed = format!("{base}@{suffix}");
            let id = [&base, &renamed].into_iter().find(|id| {
                graph
                    .vertex(&VertexId::from_raw(id))
                    .is_some_and(|v| v.kind == kind)
            });
            // 중복·충돌로 reader가 만들지 못한 정점은 입력 id를 유지한다. 분석 단계가 미해결로 보고한다.
            result.insert(id.unwrap_or(&base).clone(), routine);
        }
    }
    result
}

fn routine_change(new_id: &str, old: &RoutineDoc, new: &RoutineDoc) -> Option<RoutineChange> {
    let change = RoutineChange {
        id: new_id.to_owned(),
        kind: (old.kind != new.kind).then(|| FieldChange {
            old: old.kind.clone(),
            new: new.kind.clone(),
        }),
        language: (old.language != new.language).then(|| FieldChange {
            old: old.language.clone().unwrap_or_default(),
            new: new.language.clone().unwrap_or_default(),
        }),
        body_changed: old.body != new.body,
    };
    (change.kind.is_some() || change.language.is_some() || change.body_changed).then_some(change)
}

/// 두 catalog document를 스키마·객체·routine 기준으로 비교한다.
/// usage는 관측 부속물이라 비교하지 않는다(모듈 문서 참조).
pub fn diff_documents(old: &CatalogDocument, new: &CatalogDocument) -> DocumentDiff {
    let mut limitations = Vec::new();
    if old.dialect != new.dialect {
        limitations.push(format!(
            "dialect가 다르다({} → {}) — 같은 이름도 의미가 다를 수 있다",
            old.dialect, new.dialect
        ));
    }

    let old_schemas: BTreeMap<&str, _> = old.schemas.iter().map(|s| (s.name.as_str(), s)).collect();
    let new_schemas: BTreeMap<&str, _> = new.schemas.iter().map(|s| (s.name.as_str(), s)).collect();

    let schemas = NameDelta {
        added: new_schemas
            .keys()
            .filter(|n| !old_schemas.contains_key(*n))
            .map(|n| (*n).to_owned())
            .collect(),
        removed: old_schemas
            .keys()
            .filter(|n| !new_schemas.contains_key(*n))
            .map(|n| (*n).to_owned())
            .collect(),
    };

    // 객체·routine은 스키마를 넘나들어 옮길 수 있으므로 전체를 id로 평평하게
    // 비교한다 — 스키마 간 이동은 removed+added로 자연스럽게 나온다.
    let mut objects = ObjectDelta {
        added: Vec::new(),
        removed: Vec::new(),
        changed: Vec::new(),
    };
    let mut routines = RoutineDelta {
        added: Vec::new(),
        removed: Vec::new(),
        changed: Vec::new(),
    };

    let old_objects: BTreeMap<String, _> = old
        .schemas
        .iter()
        .flat_map(|s| {
            s.objects
                .iter()
                .map(move |o| (VertexId::object(&s.name, &o.name).as_str().to_owned(), o))
        })
        .collect();
    let new_objects: BTreeMap<String, _> = new
        .schemas
        .iter()
        .flat_map(|s| {
            s.objects
                .iter()
                .map(move |o| (VertexId::object(&s.name, &o.name).as_str().to_owned(), o))
        })
        .collect();

    for (id, no) in &new_objects {
        match old_objects.get(id) {
            Some(oo) => {
                let change = object_change(id, oo, no);
                if change.is_some() {
                    objects.changed.push(change.unwrap());
                }
            }
            None => objects.added.push(IdKind {
                id: id.clone(),
                kind: no.kind.clone(),
            }),
        }
    }
    for (id, oo) in &old_objects {
        if !new_objects.contains_key(id) {
            objects.removed.push(IdKind {
                id: id.clone(),
                kind: oo.kind.clone(),
            });
        }
    }

    let old_routines = routine_map(old);
    let new_routines = routine_map(new);

    for (id, nr) in &new_routines {
        match old_routines.get(id) {
            Some(or) => {
                if let Some(change) = routine_change(id, or, nr) {
                    routines.changed.push(change);
                }
            }
            None => routines.added.push(IdKind {
                id: id.clone(),
                kind: nr.kind.clone(),
            }),
        }
    }
    for (id, or) in &old_routines {
        if !new_routines.contains_key(id) {
            routines.removed.push(IdKind {
                id: id.clone(),
                kind: or.kind.clone(),
            });
        }
    }

    let old_dependencies: std::collections::BTreeSet<_> =
        old.dependencies.iter().cloned().collect();
    let new_dependencies: std::collections::BTreeSet<_> =
        new.dependencies.iter().cloned().collect();
    let dependencies = DependencyDelta {
        added: new_dependencies
            .difference(&old_dependencies)
            .cloned()
            .collect(),
        removed: old_dependencies
            .difference(&new_dependencies)
            .cloned()
            .collect(),
    };
    let summary = DiffSummary {
        added: schemas.added.len()
            + objects.added.len()
            + routines.added.len()
            + dependencies.added.len(),
        removed: schemas.removed.len()
            + objects.removed.len()
            + routines.removed.len()
            + dependencies.removed.len(),
        changed: objects.changed.len() + routines.changed.len(),
    };

    DocumentDiff {
        kind: "document",
        dialects: [old.dialect.clone(), new.dialect.clone()],
        summary,
        schemas,
        objects,
        routines,
        limitations,
        dependencies,
    }
}

/// 같은 id의 객체가 구조적으로 다른가 — 다르면 변경 내용을 만든다.
fn object_change(
    id: &str,
    old: &crate::document::ObjectDoc,
    new: &crate::document::ObjectDoc,
) -> Option<ObjectChange> {
    let kind = (old.kind != new.kind).then(|| FieldChange {
        old: old.kind.clone(),
        new: new.kind.clone(),
    });
    let columns = collect(&old.columns, &new.columns, |c| &c.name, |a, b| a == b);
    let constraints = collect(
        &old.constraints,
        &new.constraints,
        |c| &c.name,
        |a, b| a == b,
    );
    let indexes = collect(
        &old.indexes,
        &new.indexes,
        |i| &i.name,
        |a, b| {
            a.name == b.name
                && a.unique == b.unique
                && a.columns == b.columns
                && a.definition_complete == b.definition_complete
                && a.predicate == b.predicate
                && a.has_predicate == b.has_predicate
        },
    );
    let triggers = collect(&old.triggers, &new.triggers, |t| &t.name, |a, b| a == b);
    let body_changed = old.body != new.body;

    if kind.is_none()
        && columns.is_empty()
        && constraints.is_empty()
        && indexes.is_empty()
        && triggers.is_empty()
        && !body_changed
    {
        return None;
    }
    Some(ObjectChange {
        id: id.to_owned(),
        kind,
        columns,
        constraints,
        indexes,
        triggers,
        body_changed,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::document::{IndexDoc, ObjectDoc, SchemaDoc, UsageDoc};

    fn doc(objects: Vec<ObjectDoc>, routines: Vec<RoutineDoc>) -> CatalogDocument {
        CatalogDocument {
            context: None,
            dependencies: Vec::new(),
            version: 1,
            dialect: "sqlite".into(),
            reader: "test".into(),
            limitations: vec![],
            schemas: vec![SchemaDoc {
                name: "main".into(),
                objects,
                routines,
            }],
        }
    }

    fn table(name: &str, columns: Vec<ColumnDoc>) -> ObjectDoc {
        ObjectDoc {
            name: name.into(),
            kind: "table".into(),
            columns,
            constraints: vec![],
            indexes: vec![],
            triggers: vec![],
            body: None,
            usage: None,
        }
    }

    fn col(name: &str, data_type: &str) -> ColumnDoc {
        ColumnDoc {
            name: name.into(),
            data_type: data_type.into(),
            nullable: true,
            default: None,
            ordinal: 1,
            pk_position: 0,
        }
    }

    #[test]
    fn 컬럼_추가와_타입_변경을_잡는다() {
        let old = doc(
            vec![table("t", vec![col("a", "INTEGER"), col("b", "TEXT")])],
            vec![],
        );
        let new = doc(
            vec![table(
                "t",
                vec![
                    col("a", "INTEGER"),
                    col("b", "BLOB"), // 타입 변경
                    col("c", "REAL"), // 추가
                ],
            )],
            vec![],
        );
        let d = diff_documents(&old, &new);
        assert_eq!(d.objects.changed.len(), 1);
        let delta = &d.objects.changed[0].columns;
        assert_eq!(delta.added.len(), 1);
        assert_eq!(delta.changed.len(), 1);
        assert_eq!(delta.changed[0].name, "b");
        assert_eq!(delta.changed[0].old.data_type, "TEXT");
        assert_eq!(delta.changed[0].new.data_type, "BLOB");
    }

    #[test]
    fn routine은_시그니처_id로_대응한다() {
        let r = |body: &str| RoutineDoc {
            source: None,
            name: "f".into(),
            kind: "function".into(),
            language: Some("sql".into()),
            body: Some(body.into()),
            signature: Some("int".into()),
            usage: None,
            member_of: None,
        };
        let old = doc(vec![], vec![r("select 1")]);
        let new = doc(vec![], vec![r("select 2")]);
        let d = diff_documents(&old, &new);
        assert_eq!(d.routines.changed.len(), 1);
        assert_eq!(d.routines.changed[0].id, "main.f(int)");
        assert!(d.routines.changed[0].body_changed);
        // usage만 다르면 changed가 아니다 — 관측 부속물은 델타가 아니다.
        let mut with_usage = r("select 1");
        with_usage.usage = Some(crate::document::UsageDoc {
            since: None,
            reads: 5,
            writes: 0,
            scans: None,
            total_ms: None,
            self_ms: None,
        });
        let d2 = diff_documents(&old, &doc(vec![], vec![with_usage]));
        assert!(d2.routines.changed.is_empty());
    }

    #[test]
    fn index_usage만_바뀌면_document_diff에서_제외한다() {
        let mut old_table = table("t", vec![]);
        old_table.indexes = vec![IndexDoc {
            has_predicate: None,
            definition_complete: None,
            predicate: None,
            name: "t_idx".into(),
            unique: true,
            columns: vec!["id".into()],
            usage: None,
        }];
        let mut new_table = old_table.clone();
        new_table.indexes[0].usage = Some(UsageDoc {
            since: None,
            reads: 4,
            writes: 0,
            scans: None,
            total_ms: None,
            self_ms: None,
        });
        let d = diff_documents(
            &doc(vec![old_table.clone()], vec![]),
            &doc(vec![new_table.clone()], vec![]),
        );
        assert_eq!(d.summary.changed, 0);
        assert!(d.objects.changed.is_empty());

        let mut structurally_changed = new_table;
        structurally_changed.indexes[0].unique = false;
        structurally_changed.indexes[0].columns = vec!["other_id".into()];
        let d = diff_documents(
            &doc(vec![old_table], vec![]),
            &doc(vec![structurally_changed], vec![]),
        );
        assert_eq!(d.objects.changed.len(), 1);
        assert_eq!(d.objects.changed[0].indexes.changed.len(), 1);
    }

    #[test]
    fn routine_member_of_이동은_graph_id_add_remove로_보고한다() {
        let routine = |member_of: &str| RoutineDoc {
            source: None,
            name: "touch".into(),
            kind: "procedure".into(),
            language: Some("plsql".into()),
            body: Some("BEGIN NULL; END;".into()),
            signature: None,
            usage: None,
            member_of: Some(member_of.into()),
        };
        let old = doc(vec![], vec![routine("old_ops")]);
        let new = doc(vec![], vec![routine("new_ops")]);
        let d = diff_documents(&old, &new);
        assert_eq!(d.routines.changed.len(), 0);
        assert_eq!(d.routines.added[0].id, "main.new_ops.touch");
        assert_eq!(d.routines.removed[0].id, "main.old_ops.touch");
    }

    #[test]
    fn 같은_이름의_다른_패키지_멤버는_동일_문서에서_충돌하지_않는다() {
        let routine = |parent: &str, name: &str| RoutineDoc {
            source: None,
            name: name.into(),
            kind: "procedure".into(),
            language: Some("plsql".into()),
            body: Some("BEGIN NULL; END;".into()),
            signature: None,
            usage: None,
            member_of: Some(parent.into()),
        };
        let old = doc(
            vec![],
            vec![routine("ops_a", "touch"), routine("ops_b", "touch")],
        );
        let d = diff_documents(&old, &old.clone());
        assert_eq!(d.summary.added, 0);
        assert_eq!(d.summary.removed, 0);
        assert_eq!(d.summary.changed, 0);
    }
}
