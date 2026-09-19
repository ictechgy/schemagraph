//! 두 catalog document의 카탈로그 델타 — `diff` 명령의 document 경로.
//!
//! usage는 비교하지 않는다 — 카운트·시각은 관측 부속물이라 항상 달라
//! 모든 객체가 "changed"로 나온다. 델타는 스키마 구조만이다.

use std::collections::BTreeMap;

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
    /// schema.name(signature) — 정점 id와 같은 형태다.
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
fn collect<T: Clone + PartialEq>(
    old: &[T],
    new: &[T],
    name: impl Fn(&T) -> &str,
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
            Some(ov) if *ov != *nv => delta.changed.push(PairChange {
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

/// routine의 정점 id — document_to_graph와 같은 규칙이어야 diff 산출물의
/// id가 그래프 id와 통한다.
fn routine_id(schema: &str, r: &RoutineDoc) -> String {
    match &r.signature {
        Some(sig) if !sig.is_empty() => format!("{schema}.{}({sig})", r.name),
        _ => format!("{schema}.{}", r.name),
    }
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
                .map(move |o| (format!("{}.{}", s.name, o.name), o))
        })
        .collect();
    let new_objects: BTreeMap<String, _> = new
        .schemas
        .iter()
        .flat_map(|s| {
            s.objects
                .iter()
                .map(move |o| (format!("{}.{}", s.name, o.name), o))
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

    let old_routines: BTreeMap<String, _> = old
        .schemas
        .iter()
        .flat_map(|s| s.routines.iter().map(move |r| (routine_id(&s.name, r), r)))
        .collect();
    let new_routines: BTreeMap<String, _> = new
        .schemas
        .iter()
        .flat_map(|s| s.routines.iter().map(move |r| (routine_id(&s.name, r), r)))
        .collect();

    for (id, nr) in &new_routines {
        match old_routines.get(id) {
            Some(or) => {
                let change = RoutineChange {
                    id: id.clone(),
                    kind: (or.kind != nr.kind).then(|| FieldChange {
                        old: or.kind.clone(),
                        new: nr.kind.clone(),
                    }),
                    language: (or.language != nr.language).then(|| FieldChange {
                        old: or.language.clone().unwrap_or_default(),
                        new: nr.language.clone().unwrap_or_default(),
                    }),
                    body_changed: or.body != nr.body,
                };
                if change.kind.is_some() || change.language.is_some() || change.body_changed {
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

    let summary = DiffSummary {
        added: schemas.added.len() + objects.added.len() + routines.added.len(),
        removed: schemas.removed.len() + objects.removed.len() + routines.removed.len(),
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
    let columns = collect(&old.columns, &new.columns, |c| &c.name);
    let constraints = collect(&old.constraints, &new.constraints, |c| &c.name);
    let indexes = collect(&old.indexes, &new.indexes, |i| &i.name);
    let triggers = collect(&old.triggers, &new.triggers, |t| &t.name);
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
    use crate::document::{ObjectDoc, SchemaDoc};

    fn doc(objects: Vec<ObjectDoc>, routines: Vec<RoutineDoc>) -> CatalogDocument {
        CatalogDocument {
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
            name: "f".into(),
            kind: "function".into(),
            language: Some("sql".into()),
            body: Some(body.into()),
            signature: Some("int".into()),
            usage: None,
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
            total_ms: None,
            self_ms: None,
        });
        let d2 = diff_documents(&old, &doc(vec![], vec![with_usage]));
        assert!(d2.routines.changed.is_empty());
    }
}
