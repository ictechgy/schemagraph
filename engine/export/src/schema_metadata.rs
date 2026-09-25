//! GraphDoc 경계에서 schema metadata를 결정적으로 보존하고 검증한다.

use schemagraph_core::{
    ColumnMetadata, ForeignKeyMetadata, Graph, IndexMetadata, SchemaMetadata, VertexId, VertexKind,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// 기본값 SQL을 복제하지 않고 타입·키 순서를 그래프 파일에 보존한다.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ColumnMetadataDoc {
    pub data_type: String,
    pub nullable: bool,
    pub ordinal: u32,
    #[serde(default)]
    pub pk_position: u32,
}

/// 부분·식 인덱스를 완전한 key prefix로 오독하지 않게 한다.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexMetadataDoc {
    pub table: String,
    pub columns: Vec<String>,
    pub unique: bool,
    pub has_predicate: bool,
    pub complete: bool,
}

/// FK의 컬럼 순서와 미수집 대상의 차이를 전송한다.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ForeignKeyMetadataDoc {
    pub table: String,
    pub columns: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_table: Option<String>,
    pub target_columns: Vec<String>,
    pub complete: bool,
}

/// DB 재접속 없이 lint를 재현할 수 있는 카탈로그 사실이다.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SchemaMetadataDoc {
    pub columns: BTreeMap<String, ColumnMetadataDoc>,
    pub indexes: BTreeMap<String, IndexMetadataDoc>,
    pub foreign_keys: BTreeMap<String, ForeignKeyMetadataDoc>,
    /// 참일 때만 싣는다 — 없으면 옛 그래프와 같이 "완전성 미선언"으로 읽는다.
    #[serde(default, skip_serializing_if = "is_false")]
    pub catalog_complete: bool,
}

/// serde가 거짓 값의 선택 필드를 생략하도록 쓰는 판정이다.
fn is_false(value: &bool) -> bool {
    !*value
}

/// Core metadata를 GraphDoc wire 값으로 변환한다.
pub fn to_doc(metadata: &SchemaMetadata) -> SchemaMetadataDoc {
    SchemaMetadataDoc {
        columns: metadata
            .columns
            .iter()
            .map(|(id, value)| {
                (
                    id.as_str().into(),
                    ColumnMetadataDoc {
                        data_type: value.data_type.clone(),
                        nullable: value.nullable,
                        ordinal: value.ordinal,
                        pk_position: value.pk_position,
                    },
                )
            })
            .collect(),
        indexes: metadata
            .indexes
            .iter()
            .map(|(id, value)| {
                (
                    id.as_str().into(),
                    IndexMetadataDoc {
                        table: value.table.as_str().into(),
                        columns: value
                            .columns
                            .iter()
                            .map(|column| column.as_str().into())
                            .collect(),
                        unique: value.unique,
                        has_predicate: value.has_predicate,
                        complete: value.complete,
                    },
                )
            })
            .collect(),
        foreign_keys: metadata
            .foreign_keys
            .iter()
            .map(|(id, value)| {
                (
                    id.as_str().into(),
                    ForeignKeyMetadataDoc {
                        table: value.table.as_str().into(),
                        columns: value
                            .columns
                            .iter()
                            .map(|column| column.as_str().into())
                            .collect(),
                        target_table: value
                            .target_table
                            .as_ref()
                            .map(|table| table.as_str().into()),
                        target_columns: value
                            .target_columns
                            .iter()
                            .map(|column| column.as_str().into())
                            .collect(),
                        complete: value.complete,
                    },
                )
            })
            .collect(),
        catalog_complete: metadata.catalog_complete,
    }
}

/// GraphDoc metadata를 실제 그래프 정점에만 연결한다.
pub fn from_doc(doc: &SchemaMetadataDoc, graph: &Graph) -> Result<SchemaMetadata, String> {
    let mut metadata = SchemaMetadata {
        catalog_complete: doc.catalog_complete,
        ..SchemaMetadata::default()
    };
    for (raw, value) in &doc.columns {
        let id = checked_vertex(graph, raw, VertexKind::Column, "column")?;
        metadata.columns.insert(
            id,
            ColumnMetadata {
                data_type: value.data_type.clone(),
                nullable: value.nullable,
                ordinal: value.ordinal,
                pk_position: value.pk_position,
            },
        );
    }
    for (raw, value) in &doc.indexes {
        let id = checked_vertex(graph, raw, VertexKind::Index, "index")?;
        let table = checked_object(graph, &value.table, "index table")?;
        if !belongs_to(graph, &id, &table) {
            return Err(format!("index {raw} is not owned by its declared table"));
        }
        let columns = value
            .columns
            .iter()
            .map(|column| checked_vertex(graph, column, VertexKind::Column, "index column"))
            .collect::<Result<Vec<_>, _>>()?;
        if columns
            .iter()
            .any(|column| !belongs_to(graph, column, &table))
        {
            return Err(format!("index {raw} contains a column outside its table"));
        }
        if value.complete && columns.is_empty() {
            return Err(format!(
                "index {raw} has no key columns but claims a complete definition"
            ));
        }
        metadata.indexes.insert(
            id,
            IndexMetadata {
                table,
                columns,
                unique: value.unique,
                has_predicate: value.has_predicate,
                complete: value.complete,
            },
        );
    }
    for (raw, value) in &doc.foreign_keys {
        let id = checked_vertex(graph, raw, VertexKind::Constraint, "foreign key")?;
        let table = checked_object(graph, &value.table, "foreign-key table")?;
        if !belongs_to(graph, &id, &table) {
            return Err(format!(
                "foreign key {raw} is not owned by its declared table"
            ));
        }
        let columns = value
            .columns
            .iter()
            .map(|column| checked_vertex(graph, column, VertexKind::Column, "foreign-key column"))
            .collect::<Result<Vec<_>, _>>()?;
        if columns
            .iter()
            .any(|column| !belongs_to(graph, column, &table))
        {
            return Err(format!(
                "foreign key {raw} contains a column outside its table"
            ));
        }
        let target_table = value
            .target_table
            .as_deref()
            .map(|target| checked_object(graph, target, "foreign-key target table"))
            .transpose()?;
        let target_columns = value
            .target_columns
            .iter()
            .map(|column| {
                checked_vertex(
                    graph,
                    column,
                    VertexKind::Column,
                    "foreign-key target column",
                )
            })
            .collect::<Result<Vec<_>, _>>()?;
        if let Some(target) = &target_table {
            if target_columns
                .iter()
                .any(|column| !belongs_to(graph, column, target))
            {
                return Err(format!(
                    "foreign key {raw} contains a target column outside its target table"
                ));
            }
        }
        if (target_table.is_none() && !target_columns.is_empty())
            || (value.complete
                && (columns.is_empty()
                    || target_table.is_none()
                    || target_columns.len() != columns.len()))
        {
            return Err(format!(
                "foreign key {raw} has inconsistent column correspondence or completeness"
            ));
        }
        metadata.foreign_keys.insert(
            id,
            ForeignKeyMetadata {
                table,
                columns,
                target_table,
                target_columns,
                complete: value.complete,
            },
        );
    }
    Ok(metadata)
}

fn belongs_to(graph: &Graph, child: &VertexId, parent: &VertexId) -> bool {
    let owners: std::collections::BTreeSet<_> = graph
        .incoming(child)
        .iter()
        .filter(|edge| edge.kind == schemagraph_core::EdgeKind::Contains)
        .map(|edge| &edge.from)
        .collect();
    if owners.is_empty() {
        child.parent().as_ref() == Some(parent)
    } else {
        owners.len() == 1 && owners.contains(parent)
    }
}

fn checked_vertex(
    graph: &Graph,
    raw: &str,
    kind: VertexKind,
    label: &str,
) -> Result<VertexId, String> {
    let id = VertexId::from_raw(raw);
    match graph.vertex(&id) {
        Some(vertex) if vertex.kind == kind => Ok(id),
        Some(vertex) => Err(format!(
            "{label} {raw} has kind {:?}, expected {kind:?}",
            vertex.kind
        )),
        None => Err(format!("{label} {raw} is not present in graph")),
    }
}

fn checked_object(graph: &Graph, raw: &str, label: &str) -> Result<VertexId, String> {
    let id = VertexId::from_raw(raw);
    match graph.vertex(&id) {
        Some(vertex)
            if matches!(
                vertex.kind,
                VertexKind::Table | VertexKind::View | VertexKind::MaterializedView
            ) =>
        {
            Ok(id)
        }
        Some(vertex) => Err(format!(
            "{label} {raw} has non-object kind {:?}",
            vertex.kind
        )),
        None => Err(format!("{label} {raw} is not present in graph")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use schemagraph_core::{Graph, Vertex, VertexKind};

    fn graph() -> Graph {
        let mut graph = Graph::new();
        for (id, kind, name) in [
            ("app", VertexKind::Schema, "app"),
            ("app.orders", VertexKind::Table, "orders"),
            ("app.orders.id", VertexKind::Column, "id"),
            ("app.orders.idx", VertexKind::Index, "idx"),
            ("app.orders.fk", VertexKind::Constraint, "fk"),
        ] {
            graph.add_vertex(Vertex {
                id: VertexId::from_raw(id),
                kind,
                name: name.into(),
                schema: "app".into(),
            });
        }
        graph
    }

    #[test]
    fn metadata_round_trip_keeps_actual_ids_and_optional_fields() {
        let mut metadata = SchemaMetadata::default();
        metadata.columns.insert(
            VertexId::from_raw("app.orders.id"),
            ColumnMetadata {
                data_type: "INTEGER".into(),
                nullable: false,
                ordinal: 1,
                pk_position: 1,
            },
        );
        metadata.indexes.insert(
            VertexId::from_raw("app.orders.idx"),
            IndexMetadata {
                table: VertexId::from_raw("app.orders"),
                columns: vec![VertexId::from_raw("app.orders.id")],
                unique: true,
                has_predicate: false,
                complete: true,
            },
        );
        metadata.foreign_keys.insert(
            VertexId::from_raw("app.orders.fk"),
            ForeignKeyMetadata {
                table: VertexId::from_raw("app.orders"),
                columns: vec![VertexId::from_raw("app.orders.id")],
                target_table: None,
                target_columns: vec![],
                complete: false,
            },
        );
        let wire = to_doc(&metadata);
        assert_eq!(from_doc(&wire, &graph()).unwrap(), metadata);
        // 완전성을 선언하지 않은 메타데이터는 키를 싣지 않아 옛 그래프와 같은 모양이다.
        assert!(serde_json::to_value(&wire)
            .unwrap()
            .get("catalog_complete")
            .is_none());
        metadata.catalog_complete = true;
        let wire = to_doc(&metadata);
        assert_eq!(
            serde_json::to_value(&wire).unwrap()["catalog_complete"],
            true
        );
        assert_eq!(from_doc(&wire, &graph()).unwrap(), metadata);
    }

    #[test]
    fn metadata_reader_rejects_unknown_vertex_endpoints() {
        let mut wire = SchemaMetadataDoc {
            columns: BTreeMap::new(),
            indexes: BTreeMap::new(),
            foreign_keys: BTreeMap::new(),
            catalog_complete: false,
        };
        wire.indexes.insert(
            "app.orders.idx".into(),
            IndexMetadataDoc {
                table: "app.orders".into(),
                columns: vec!["app.orders.missing".into()],
                unique: false,
                has_predicate: false,
                complete: true,
            },
        );
        assert!(from_doc(&wire, &graph())
            .unwrap_err()
            .contains("not present"));
    }
}
