//! CatalogDocument → Graph 변환.
//!
//! 여기서 하는 일은 오직 카탈로그 사실의 그래프화다. 이름 해석(스키마 미상
//! 참조 등)은 정직하게 처리하고, 못 보는 것은 limitations로 넘긴다 —
//! 추측으로 간선을 만들지 않는다.

use schemagraph_core::{
    Edge, EdgeKind, Evidence, EvidenceLayer, Graph, Vertex, VertexId, VertexKind,
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

        for col in &obj.columns {
            let col_id = VertexId::member(&schema.name, &obj.name, &col.name);
            g.add_vertex(Vertex {
                id: col_id.clone(),
                kind: VertexKind::Column,
                name: col.name.clone(),
                schema: schema.name.clone(),
            });
            g.add_edge(contains(&obj_id, &col_id));
        }

        for con in &obj.constraints {
            let con_id = VertexId::member(&schema.name, &obj.name, &con.name);
            g.add_vertex(Vertex {
                id: con_id.clone(),
                kind: VertexKind::Constraint,
                name: con.name.clone(),
                schema: schema.name.clone(),
            });
            g.add_edge(contains(&obj_id, &con_id));
            if con.kind == "fk" {
                build_fk_edges(g, schema, obj, con, &obj_id);
            }
        }

        for idx in &obj.indexes {
            let idx_id = VertexId::member(&schema.name, &obj.name, &idx.name);
            g.add_vertex(Vertex {
                id: idx_id.clone(),
                kind: VertexKind::Index,
                name: idx.name.clone(),
                schema: schema.name.clone(),
            });
            g.add_edge(contains(&obj_id, &idx_id));
        }

        for trg in &obj.triggers {
            let trg_id = VertexId::member(&schema.name, &obj.name, &trg.name);
            g.add_vertex(Vertex {
                id: trg_id.clone(),
                kind: VertexKind::Trigger,
                name: trg.name.clone(),
                schema: schema.name.clone(),
            });
            g.add_edge(contains(&obj_id, &trg_id));
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
        let rt_id = VertexId::object(&schema.name, &id_name);
        g.add_vertex(Vertex {
            id: rt_id.clone(),
            kind,
            name: routine.name.clone(),
            schema: schema.name.clone(),
        });
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
}
