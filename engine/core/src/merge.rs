//! 독립 DB 그래프는 해석을 마친 뒤 명시적인 namespace로 합친다.

use crate::{Graph, SchemaMetadata, VertexId};
use std::collections::BTreeSet;

/// 실제 source id를 보존해 동명 스키마끼리 합쳐지는 일을 막는다.
pub fn namespaced_id(namespace: &str, id: &VertexId) -> VertexId {
    VertexId::from_raw(&format!("{namespace}::{id}"))
}

/// DB별 분석 결과를 섞지 않고 조합한다. DB 간 간선은 호출자가 확인한 근거만 추가한다.
pub fn merge_graphs(mut inputs: Vec<(String, Graph)>) -> Result<Graph, String> {
    inputs.sort_by(|a, b| a.0.cmp(&b.0));
    let mut names = BTreeSet::new();
    for (name, _) in &inputs {
        if name.is_empty()
            || name.len() > 128
            || !name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"_-./".contains(&b))
        {
            return Err(
                "each graph needs a logical namespace using letters, digits, _, -, ., or /".into(),
            );
        }
        if !names.insert(name.clone()) {
            return Err(format!("duplicate source namespace '{name}'"));
        }
    }
    let metadata_complete = inputs
        .iter()
        .all(|(_, graph)| graph.schema_metadata().is_some());
    let mut combined = Graph::new();
    // 병합 결과는 모든 입력이 완전할 때만 완전하다.
    let mut metadata = SchemaMetadata {
        catalog_complete: inputs.iter().all(|(_, graph)| {
            graph
                .schema_metadata()
                .is_some_and(|metadata| metadata.catalog_complete)
        }),
        ..SchemaMetadata::default()
    };
    for (name, graph) in inputs {
        let id = |old: &VertexId| namespaced_id(&name, old);
        for vertex in graph.vertices() {
            let mut vertex = vertex.clone();
            vertex.id = id(&vertex.id);
            combined.add_vertex(vertex);
        }
        for edge in graph.edges() {
            let mut edge = edge.clone();
            edge.from = id(&edge.from);
            edge.to = id(&edge.to);
            combined.add_edge(edge);
        }
        for (old, usage) in graph.usages() {
            combined.set_usage(id(old), usage.clone());
        }
        for (old, analysis) in graph.analysis() {
            combined.set_analysis(id(old), analysis.clone());
        }
        for ((from, to, kind), origins) in graph.origins() {
            for origin in origins {
                combined.add_origin((id(from), id(to), *kind), origin.clone());
            }
        }
        for limitation in graph.limitations() {
            combined.add_limitation(format!("{name}: {limitation}"));
        }
        if let Some(source) = graph.schema_metadata() {
            for (old, column) in &source.columns {
                metadata.columns.insert(id(old), column.clone());
            }
            for (old, index) in &source.indexes {
                let mut index = index.clone();
                index.table = id(&index.table);
                index.columns = index.columns.iter().map(&id).collect();
                metadata.indexes.insert(id(old), index);
            }
            for (old, fk) in &source.foreign_keys {
                let mut fk = fk.clone();
                fk.table = id(&fk.table);
                fk.columns = fk.columns.iter().map(&id).collect();
                fk.target_table = fk.target_table.as_ref().map(&id);
                fk.target_columns = fk.target_columns.iter().map(&id).collect();
                metadata.foreign_keys.insert(id(old), fk);
            }
        }
    }
    if metadata_complete {
        combined.set_schema_metadata(metadata);
    }
    Ok(combined)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Vertex, VertexKind};

    fn fixture() -> Graph {
        let mut graph = Graph::new();
        graph.add_vertex(Vertex {
            id: VertexId::object("s", "t"),
            name: "t".into(),
            schema: "s".into(),
            kind: VertexKind::Table,
        });
        graph
    }

    /// 한 입력이라도 수집 완전성을 선언하지 않았으면 병합 결과도 완전하지 않다.
    #[test]
    fn merged_metadata_is_complete_only_when_every_source_is() {
        let with_metadata = |complete: bool| {
            let mut graph = fixture();
            graph.set_schema_metadata(SchemaMetadata {
                catalog_complete: complete,
                ..SchemaMetadata::default()
            });
            graph
        };
        let complete = |graphs: Vec<(String, Graph)>| {
            merge_graphs(graphs)
                .unwrap()
                .schema_metadata()
                .is_some_and(|metadata| metadata.catalog_complete)
        };
        assert!(complete(vec![
            ("a".into(), with_metadata(true)),
            ("b".into(), with_metadata(true)),
        ]));
        assert!(!complete(vec![
            ("a".into(), with_metadata(true)),
            ("b".into(), with_metadata(false)),
        ]));
    }

    #[test]
    fn sources_are_distinct_and_duplicate_labels_are_rejected() {
        let merged = merge_graphs(vec![("b".into(), fixture()), ("a".into(), fixture())]).unwrap();
        assert_eq!(
            merged.vertices().map(|v| v.id.as_str()).collect::<Vec<_>>(),
            ["a::s.t", "b::s.t"]
        );
        assert!(merge_graphs(vec![("a".into(), fixture()), ("a".into(), fixture())]).is_err());
        assert!(merge_graphs(vec![("postgres://secret".into(), fixture())]).is_err());
    }
}
