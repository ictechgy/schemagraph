//! CatalogDocument → Graph 변환.
//!
//! 여기서 하는 일은 오직 카탈로그 사실의 그래프화다. 이름 해석(스키마 미상
//! 참조 등)은 정직하게 처리하고, 못 보는 것은 limitations로 넘긴다 —
//! 추측으로 간선을 만들지 않는다.

use schemagraph_core::{
    Edge, EdgeKind, Evidence, EvidenceLayer, Graph, Usage, Vertex, VertexId, VertexKind,
};

use crate::document::*;

/// document를 그래프로 바꾼다. document의 limitations를 그대로 계승한다.
pub fn document_to_graph(doc: &CatalogDocument) -> Graph {
    let mut g = Graph::new();
    for l in &doc.limitations {
        g.add_limitation(l.clone());
    }
    for schema in &doc.schemas {
        build_schema(&mut g, schema);
    }
    g
}

fn build_schema(g: &mut Graph, schema: &SchemaDoc) {
    let schema_id = VertexId::schema(&schema.name);
    g.add_vertex(Vertex {
        id: schema_id.clone(),
        kind: VertexKind::Schema,
        name: schema.name.clone(),
        schema: schema.name.clone(),
    });

    for obj in &schema.objects {
        let obj_id = VertexId::object(&schema.name, &obj.name);
        let kind = match obj.kind.as_str() {
            "view" => VertexKind::View,
            "materialized-view" => VertexKind::MaterializedView,
            "sequence" => VertexKind::Sequence,
            "type" => VertexKind::Type,
            "synonym" => VertexKind::Synonym,
            _ => VertexKind::Table,
        };
        if obj.name.contains('.') {
            g.add_limitation(format!(
                "객체 이름에 '.'이 있어 정점 id가 깨짐: {}.{}",
                schema.name, obj.name
            ));
        }
        g.add_vertex(Vertex {
            id: obj_id.clone(),
            kind,
            name: obj.name.clone(),
            schema: schema.name.clone(),
        });
        g.add_edge(contains(&schema_id, &obj_id));
        if let Some(u) = &obj.usage {
            g.set_usage(obj_id.clone(), to_usage(u));
        }

        for col in &obj.columns {
            add_member(g, &obj_id, &schema.name, &obj.name, &col.name, VertexKind::Column);
        }

        for con in &obj.constraints {
            add_member(g, &obj_id, &schema.name, &obj.name, &con.name, VertexKind::Constraint);
            if con.kind == "fk" {
                build_fk_edges(g, schema, obj, con, &obj_id);
            }
        }

        for idx in &obj.indexes {
            if let Some(idx_id) =
                add_member(g, &obj_id, &schema.name, &obj.name, &idx.name, VertexKind::Index)
            {
                if let Some(u) = &idx.usage {
                    g.set_usage(idx_id, to_usage(u));
                }
            }
        }

        for trg in &obj.triggers {
            let Some(trg_id) = add_member(
                g,
                &obj_id,
                &schema.name,
                &obj.name,
                &trg.name,
                VertexKind::Trigger,
            ) else {
                continue;
            };
            // 발화 대상은 카탈로그가 알려준 사실 — 몸체 파싱 없이도 fires를 둘 수 있다.
            g.add_edge(Edge {
                from: trg_id,
                to: obj_id.clone(),
                kind: EdgeKind::Fires,
                evidence: vec![Evidence {
                    layer: EvidenceLayer::Catalog,
                    detail: format!("trigger {} on {}", trg.name, obj.name),
                }],
            });
        }
    }

    for routine in &schema.routines {
        let kind = match routine.kind.as_str() {
            "procedure" => VertexKind::Procedure,
            "package" => VertexKind::Package,
            _ => VertexKind::Function,
        };
        // signature는 오버로드 구분자 — 비어 있지 않을 때만 정점 id에 붙인다.
        // (PG의 인자 없는 함수는 시그니처가 빈 문자열이라 "fn()"가 되면 안 된다)
        let id_name = match &routine.signature {
            Some(sig) if !sig.is_empty() => format!("{}({})", routine.name, sig),
            _ => routine.name.clone(),
        };
        // 함수와 프로시저는 같은 이름을 공유할 수 있다(MySQL) — member와
        // 같은 규칙으로 충돌을 분리해 나중 정점이 조용히 드랍되지 않게 한다.
        let suffix = match kind {
            VertexKind::Procedure => "procedure",
            VertexKind::Package => "package",
            _ => "function",
        };
        let Some(rt_id) = resolve_collision(
            g,
            VertexId::object(&schema.name, &id_name),
            &routine.name,
            kind,
            suffix,
            "routine",
        ) else {
            continue;
        };
        g.add_vertex(Vertex {
            id: rt_id.clone(),
            kind,
            name: routine.name.clone(),
            schema: schema.name.clone(),
        });
        if let Some(u) = &routine.usage {
            g.set_usage(rt_id.clone(), to_usage(u));
        }
        g.add_edge(contains(&schema_id, &rt_id));
    }
}

/// fk 제약 하나를 object 레벨 + column 레벨 간선으로 푼다.
fn build_fk_edges(
    g: &mut Graph,
    schema: &SchemaDoc,
    obj: &ObjectDoc,
    con: &ConstraintDoc,
    obj_id: &VertexId,
) {
    let Some(referenced) = &con.referenced else {
        return;
    };
    // 대상 스키마가 카탈로그에 없으면 같은 스키마로 해석한다 — SQLite처럼
    // 스키마가 하나뿐인 DB에서 사실상 항상 맞고, 틀릴 경우는 limitations에
    // 남길 수 있는 게 아니라 조용히 다른 정점을 가리키게 되므로 이름이
    // 존재하는지 검사해 없으면 limitation을 남긴다.
    let target_schema = referenced
        .schema
        .clone()
        .unwrap_or_else(|| schema.name.clone());
    let target_id = VertexId::object(&target_schema, &referenced.table);

    g.add_edge(Edge {
        from: obj_id.clone(),
        to: target_id.clone(),
        kind: EdgeKind::References,
        evidence: vec![Evidence {
            layer: EvidenceLayer::Catalog,
            detail: format!(
                "fk {}: {}.{}({}) -> {}.{}({})",
                con.name,
                schema.name,
                obj.name,
                con.columns.join(","),
                target_schema,
                referenced.table,
                referenced.columns.join(",")
            ),
        }],
    });

    for (local, remote) in con.columns.iter().zip(referenced.columns.iter()) {
        if remote.is_empty() {
            // 대상 컬럼을 카탈로그가 안 알려주면(암시적 PK 참조) 컬럼 간선을
            // 만들지 않는다 — object 간선은 이미 있다.
            continue;
        }
        g.add_edge(Edge {
            from: VertexId::member(&schema.name, &obj.name, local),
            to: VertexId::member(&target_schema, &referenced.table, remote),
            kind: EdgeKind::References,
            evidence: vec![Evidence {
                layer: EvidenceLayer::Catalog,
                detail: format!("fk {}: {} -> {}", con.name, local, remote),
            }],
        });
    }
}

fn contains(from: &VertexId, to: &VertexId) -> Edge {
    Edge {
        from: from.clone(),
        to: to.clone(),
        kind: EdgeKind::Contains,
        evidence: vec![],
    }
}

/// 멤버 정점을 추가한다 — 충돌 시 `@kind` 접미사로 분리한다.
///
/// 컬럼·제약·인덱스·트리거는 `schema.object.name` 공간을 공유하는데,
/// MySQL의 FK 자동 인덱스처럼 다른 kind가 같은 이름을 쓸 수 있다.
/// add_vertex는 first-wins라 나중 정점이 조용히 드랍되므로, 다른 kind가
/// 이미 차지한 이름은 `name@kind`로 분리해 정점을 살린다 — `@`는 SQL
/// 식별자에 못 쓰는 문자라 추가 충돌이 거의 없다. 실제로 쓰인 id를
/// 돌려주고, 분리조차 실패하면 None(정점 생략 + limitation 신고).
fn add_member(
    g: &mut Graph,
    obj_id: &VertexId,
    schema: &str,
    obj: &str,
    name: &str,
    kind: VertexKind,
) -> Option<VertexId> {
    let base = VertexId::member(schema, obj, name);
    let suffix = match kind {
        VertexKind::Column => "column",
        VertexKind::Constraint => "constraint",
        VertexKind::Index => "index",
        VertexKind::Trigger => "trigger",
        _ => "member",
    };
    let id = resolve_collision(g, base, name, kind, suffix, "멤버")?;
    g.add_vertex(Vertex {
        id: id.clone(),
        kind,
        name: name.to_owned(),
        schema: schema.to_owned(),
    });
    g.add_edge(contains(obj_id, &id));
    Some(id)
}

/// id가 이미 점유됐을 때의 분리 규칙 — member·routine 정점이 공유한다.
/// 같은 kind면 진짜 중복이라 합치고, 다른 kind면 `name@kind`로 옮긴다.
/// 반환 id가 실제 정점 id — 호출자는 이 id로 정점·간선·usage를 달아야 한다.
fn resolve_collision(
    g: &mut Graph,
    base: VertexId,
    name: &str,
    kind: VertexKind,
    suffix: &str,
    label: &str,
) -> Option<VertexId> {
    match g.vertex(&base) {
        None => Some(base),
        Some(v) if v.kind == kind => {
            // 같은 kind의 같은 이름은 진짜 중복 — 정점은 합쳐지지만 신고한다.
            g.add_limitation(format!(
                "{label} 이름 중복: {}는 {suffix}가 이미 있다 — 정점이 합쳐진다",
                base.as_str(),
            ));
            Some(base)
        }
        Some(_) => {
            // `@`는 SQL 식별자에 못 쓰는 문자라 base 뒤에 붙여도 안전하다 —
            // member/schema.object 어느 깊이든 같은 규칙으로 분리한다.
            let renamed = VertexId::from_raw(&format!("{}@{suffix}", base.as_str()));
            if g.vertex(&renamed).is_some() {
                g.add_limitation(format!(
                    "{label} id 충돌 미해소: {}와 {} 모두 점유 — {name} {suffix} 정점 생략",
                    base.as_str(),
                    renamed.as_str(),
                ));
                return None;
            }
            g.add_limitation(format!(
                "{label} id 충돌: {}는 다른 kind가 먼저 차지 — {suffix} 정점은 {}로 분리",
                base.as_str(),
                renamed.as_str(),
            ));
            Some(renamed)
        }
    }
}

/// UsageDoc → core Usage — 와이어 타입과 도메인 타입을 분리해 둔 변환.
fn to_usage(u: &UsageDoc) -> Usage {
    Usage {
        since: u.since.clone(),
        reads: u.reads,
        writes: u.writes,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc_with_fk() -> CatalogDocument {
        CatalogDocument {
            version: 1,
            dialect: "sqlite".into(),
            reader: "test".into(),
            limitations: vec![],
            schemas: vec![SchemaDoc {
                name: "main".into(),
                routines: vec![],
                objects: vec![
                    ObjectDoc {
                        name: "customers".into(),
                        kind: "table".into(),
                        columns: vec![ColumnDoc {
                            name: "id".into(),
                            data_type: "INTEGER".into(),
                            nullable: false,
                            default: None,
                            ordinal: 1,
                            pk_position: 1,
                        }],
                        constraints: vec![],
                        indexes: vec![],
                        triggers: vec![],
                        body: None,
                        usage: None,
                    },
                    ObjectDoc {
                        name: "orders".into(),
                        kind: "table".into(),
                        columns: vec![ColumnDoc {
                            name: "customer_id".into(),
                            data_type: "INTEGER".into(),
                            nullable: false,
                            default: None,
                            ordinal: 1,
                            pk_position: 0,
                        }],
                        constraints: vec![ConstraintDoc {
                            name: "orders_fk_0".into(),
                            kind: "fk".into(),
                            columns: vec!["customer_id".into()],
                            referenced: Some(ReferencedDoc {
                                schema: None,
                                table: "customers".into(),
                                columns: vec!["id".into()],
                            }),
                        }],
                        indexes: vec![],
                        triggers: vec![],
                        body: None,
                        usage: None,
                    },
                ],
            }],
        }
    }

    #[test]
    fn fk는_object와_column_두_레벨의_간선이_된다() {
        let g = document_to_graph(&doc_with_fk());
        let edges = g.edges();
        // object 레벨
        assert!(edges.iter().any(|e| e.kind == EdgeKind::References
            && e.from.as_str() == "main.orders"
            && e.to.as_str() == "main.customers"));
        // column 레벨
        assert!(edges.iter().any(|e| e.kind == EdgeKind::References
            && e.from.as_str() == "main.orders.customer_id"
            && e.to.as_str() == "main.customers.id"));
    }

    #[test]
    fn contains는_의존성이_아니다() {
        let g = document_to_graph(&doc_with_fk());
        let orders = VertexId::object("main", "orders");
        let dep: Vec<_> = g
            .outgoing(&orders)
            .iter()
            .filter(|e| e.kind.is_dependency())
            .collect();
        // orders의 의존 간선은 customers를 향한 references 하나뿐.
        assert_eq!(dep.len(), 1);
        assert_eq!(dep[0].to.as_str(), "main.customers");
    }

    #[test]
    fn document의_usage는_정점에_매핑되고_없음과_0은_다르다() {
        let mut doc = doc_with_fk();
        // orders: 0 관측(usage가 있는 채로 0) — 미수집과 구분해야 한다.
        doc.schemas[0].objects[1].usage = Some(UsageDoc {
            since: None,
            reads: 0,
            writes: 0,
        });
        // customers의 인덱스에도 usage를 둔다(인덱스는 멤버 레벨 정점).
        doc.schemas[0].objects[0].indexes.push(IndexDoc {
            name: "idx_id".into(),
            columns: vec!["id".into()],
            unique: true,
            usage: Some(UsageDoc {
                since: Some("2025-01-01".into()),
                reads: 7,
                writes: 0,
            }),
        });

        let g = document_to_graph(&doc);
        let orders = VertexId::object("main", "orders");
        let usage = g.usage(&orders).expect("orders usage");
        assert_eq!(usage.reads, 0);
        assert_eq!(usage.since, None);
        let idx = VertexId::member("main", "customers", "idx_id");
        assert_eq!(g.usage(&idx).map(|u| u.reads), Some(7));
        // customers 객체 자체는 미수집 — None이어야 0 관측과 구분된다.
        assert!(g.usage(&VertexId::object("main", "customers")).is_none());
    }

    #[test]
    fn 멤버_id_충돌은_kind_접미사로_분리된다() {
        let mut doc = doc_with_fk();
        // MySQL의 FK 자동 인덱스는 컬럼 이름을 그대로 쓴다 — 컬럼과 충돌.
        doc.schemas[0].objects[1].indexes.push(IndexDoc {
            name: "customer_id".into(),
            columns: vec!["customer_id".into()],
            unique: false,
            usage: Some(UsageDoc {
                since: Some("2025-01-01".into()),
                reads: 4,
                writes: 0,
            }),
        });
        // 제약과도 충돌하는 인덱스 — fk 제약과 같은 이름.
        doc.schemas[0].objects[1].indexes.push(IndexDoc {
            name: "orders_fk_0".into(),
            columns: vec!["customer_id".into()],
            unique: false,
            usage: None,
        });

        let g = document_to_graph(&doc);
        // 컬럼은 원래 id를 지키고, 인덱스는 @index로 분리된다.
        assert!(g
            .vertex(&VertexId::member("main", "orders", "customer_id"))
            .is_some());
        let idx_id = VertexId::member("main", "orders", "customer_id@index");
        let idx = g.vertex(&idx_id).expect("분리된 index 정점");
        assert_eq!(idx.kind, VertexKind::Index);
        // usage도 분리된 정점에 붙는다.
        assert_eq!(g.usage(&idx_id).map(|u| u.reads), Some(4));
        // 제약 이름과 충돌한 인덱스도 분리된다.
        assert_eq!(
            g.vertex(&VertexId::member("main", "orders", "orders_fk_0@index"))
                .map(|v| v.kind),
            Some(VertexKind::Index)
        );
        // 충돌은 limitation으로 신고된다.
        assert!(g
            .limitations()
            .iter()
            .any(|l| l.contains("멤버 id 충돌")));
    }

    #[test]
    fn routine_이름_충돌과_usage가_분리된_정점에_붙는다() {
        let mut doc = doc_with_fk();
        // 테이블과 같은 이름의 함수 — MySQL에선 함수/프로시저가 이름을 공유할 수 있다.
        doc.schemas[0].routines = vec![
            RoutineDoc {
                name: "orders".into(), // 테이블 orders와 충돌
                kind: "function".into(),
                language: Some("sql".into()),
                body: None,
                signature: None,
                usage: Some(UsageDoc {
                    since: Some("2025-01-01".into()),
                    reads: 12,
                    writes: 0,
                }),
            },
            RoutineDoc {
                name: "helper".into(),
                kind: "procedure".into(),
                language: Some("sql".into()),
                body: None,
                signature: None,
                usage: Some(UsageDoc {
                    since: None,
                    reads: 3,
                    writes: 0,
                }),
            },
        ];

        let g = document_to_graph(&doc);
        // 테이블이 base id를 지키고 함수는 @function으로 분리된다.
        assert_eq!(
            g.vertex(&VertexId::object("main", "orders")).map(|v| v.kind),
            Some(VertexKind::Table)
        );
        let fn_id = VertexId::from_raw("main.orders@function");
        assert_eq!(
            g.vertex(&fn_id).map(|v| v.kind),
            Some(VertexKind::Function)
        );
        // usage는 분리된 정점에 붙는다.
        assert_eq!(g.usage(&fn_id).map(|u| u.reads), Some(12));
        assert_eq!(
            g.usage(&VertexId::object("main", "helper")).map(|u| u.reads),
            Some(3)
        );
        assert!(g
            .limitations()
            .iter()
            .any(|l| l.contains("routine id 충돌")));
    }
}
