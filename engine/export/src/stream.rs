//! 그래프를 빌린 채 출력해 GraphDoc·Value·문자열의 전체 복제를 피한다.
//! 필드 선언 순서는 기존 Value 기반 출력의 사전식 키 순서와 같다.

use std::io::Write;

use schemagraph_core::{Edge, Evidence, Graph, Usage, Vertex};
use serde::ser::SerializeSeq;
use serde::{Serialize, Serializer};

use crate::{edge_kind_str, layer_str, level_str, vertex_kind_str, GRAPH_VERSION};

/// 이미 완성된 그래프를 복제하지 않고 기존 canonical JSON 바이트 그대로 내보낸다.
/// 간선 정렬에는 참조 벡터만 필요하며 본문·정점·근거 문자열은 빌려 쓴다.
pub fn write_graph(writer: impl Write, graph: &Graph) -> serde_json::Result<()> {
    let edges = graph.edges();
    let view = GraphView {
        analysis: crate::diagnostics::analysis_docs(graph),
        edges: Edges(&edges),
        limitations: graph.limitations(),
        origins: crate::diagnostics::origin_docs(graph),
        schema_metadata: graph.schema_metadata().map(crate::schema_metadata::to_doc),
        version: GRAPH_VERSION,
        vertices: Vertices(graph),
    };
    serde_json::to_writer_pretty(writer, &view)
}

#[derive(Serialize)]
struct GraphView<'a> {
    #[serde(skip_serializing_if = "Vec::is_empty")]
    analysis: Vec<crate::diagnostics::AnalysisDoc>,
    edges: Edges<'a>,
    #[serde(skip_serializing_if = "<[String]>::is_empty")]
    limitations: &'a [String],
    #[serde(skip_serializing_if = "Vec::is_empty")]
    origins: Vec<crate::diagnostics::OriginDoc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    schema_metadata: Option<crate::schema_metadata::SchemaMetadataDoc>,
    version: u32,
    vertices: Vertices<'a>,
}

struct Edges<'a>(&'a [&'a Edge]);

impl Serialize for Edges<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut sequence = serializer.serialize_seq(Some(self.0.len()))?;
        for edge in self.0 {
            sequence.serialize_element(&EdgeView {
                evidence: EvidenceList(&edge.evidence),
                from: edge.from.as_str(),
                kind: edge_kind_str(edge.kind),
                to: edge.to.as_str(),
            })?;
        }
        sequence.end()
    }
}

#[derive(Serialize)]
struct EdgeView<'a> {
    #[serde(skip_serializing_if = "EvidenceList::is_empty")]
    evidence: EvidenceList<'a>,
    from: &'a str,
    kind: &'static str,
    to: &'a str,
}

struct EvidenceList<'a>(&'a [Evidence]);

impl EvidenceList<'_> {
    fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl Serialize for EvidenceList<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut sequence = serializer.serialize_seq(Some(self.0.len()))?;
        for evidence in self.0 {
            sequence.serialize_element(&EvidenceView {
                detail: &evidence.detail,
                layer: layer_str(evidence.layer),
            })?;
        }
        sequence.end()
    }
}

#[derive(Serialize)]
struct EvidenceView<'a> {
    detail: &'a str,
    layer: &'static str,
}

struct Vertices<'a>(&'a Graph);

impl Serialize for Vertices<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut sequence = serializer.serialize_seq(None)?;
        for vertex in self.0.vertices() {
            sequence.serialize_element(&VertexView::new(vertex, self.0.usage(&vertex.id)))?;
        }
        sequence.end()
    }
}

#[derive(Serialize)]
struct VertexView<'a> {
    id: &'a str,
    kind: &'static str,
    level: &'static str,
    name: &'a str,
    schema: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    usage: Option<UsageView<'a>>,
}

impl<'a> VertexView<'a> {
    fn new(vertex: &'a Vertex, usage: Option<&'a Usage>) -> Self {
        Self {
            id: vertex.id.as_str(),
            kind: vertex_kind_str(vertex.kind),
            level: level_str(vertex.kind.level()),
            name: &vertex.name,
            schema: &vertex.schema,
            usage: usage.map(|usage| UsageView {
                reads: usage.reads,
                scans: usage.scans,
                self_ms: usage.self_ms,
                since: usage.since.as_deref(),
                total_ms: usage.total_ms,
                writes: usage.writes,
            }),
        }
    }
}

#[derive(Serialize)]
struct UsageView<'a> {
    reads: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    scans: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    self_ms: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    since: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    total_ms: Option<f64>,
    writes: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use schemagraph_core::{EdgeKind, EvidenceLayer, VertexId, VertexKind};

    #[test]
    fn streaming_bytes_match_legacy_with_optional_evidence_and_usage() {
        let mut graph = Graph::new();
        let mut bytes = Vec::new();
        write_graph(&mut bytes, &graph).unwrap();
        assert_eq!(
            bytes,
            crate::to_pretty_json(&crate::graph_to_doc(&graph))
                .unwrap()
                .as_bytes()
        );
        for name in ["z\\\"", "a한글"] {
            let id = VertexId::object("s", name);
            graph.add_vertex(Vertex {
                id: id.clone(),
                kind: VertexKind::Table,
                name: name.into(),
                schema: "s".into(),
            });
            graph.add_edge(Edge {
                from: id.clone(),
                to: id.clone(),
                kind: EdgeKind::Reads,
                evidence: vec![Evidence {
                    layer: EvidenceLayer::BodyParse,
                    detail: "first\nsecond".into(),
                }],
            });
            graph.set_usage(
                id,
                Usage {
                    since: Some("2026-01-01T00:00:00Z".into()),
                    reads: u64::MAX,
                    writes: 0,
                    scans: None,
                    total_ms: Some(12.5),
                    self_ms: None,
                },
            );
        }
        graph.add_limitation("one limitation");
        bytes.clear();
        write_graph(&mut bytes, &graph).unwrap();
        assert_eq!(
            bytes,
            crate::to_pretty_json(&crate::graph_to_doc(&graph))
                .unwrap()
                .as_bytes()
        );
    }

    #[test]
    fn streaming_propagates_output_failure() {
        struct Broken;
        impl Write for Broken {
            fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
                Err(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "closed output",
                ))
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        assert!(write_graph(Broken, &Graph::new()).unwrap_err().is_io());
    }
}
