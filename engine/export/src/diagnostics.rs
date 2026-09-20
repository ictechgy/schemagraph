//! core에 serde를 넣지 않고 분석 진단·원문 위치의 전송 계약을 정의한다.

use schemagraph_core::{
    AnalysisState, Diagnostic, Graph, ObjectAnalysis, Origin, SourceLocation, VertexId,
};
use serde::{Deserialize, Serialize};

/// 원문 좌표를 1부터 시작하는 행·열로 전달한다.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LocationDoc {
    pub column: u64,
    pub end_column: u64,
    pub end_line: u64,
    pub line: u64,
}

impl From<&SourceLocation> for LocationDoc {
    fn from(s: &SourceLocation) -> Self {
        Self {
            column: s.column,
            end_column: s.end_column,
            end_line: s.end_line,
            line: s.line,
        }
    }
}

impl From<&LocationDoc> for SourceLocation {
    fn from(s: &LocationDoc) -> Self {
        Self {
            column: s.column,
            end_column: s.end_column,
            end_line: s.end_line,
            line: s.line,
        }
    }
}

/// 안정적인 진단 코드와 표시 메시지를 분리한다.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiagnosticDoc {
    pub code: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub location: Option<LocationDoc>,
    pub message: String,
}

/// 분석한 범위에 대한 완료 상태다. 빈 목록은 전체 성공을 뜻하지 않는다.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnalysisDoc {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body_hash: Option<String>,
    pub diagnostics: Vec<DiagnosticDoc>,
    pub id: String,
    pub scope: String,
    pub state: String,
}

/// 원문을 그래프 파일에 복사하지 않고도 근거를 다시 찾을 수 있게 한다.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OriginDoc {
    pub body_hash: String,
    pub from: String,
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub location: Option<LocationDoc>,
    pub role: String,
    pub to: String,
}

/// 문자열 라벨은 JSON·MCP·HTML에서 같은 뜻을 갖는다.
pub fn state_str(state: AnalysisState) -> &'static str {
    match state {
        AnalysisState::Complete => "complete",
        AnalysisState::Partial => "partial",
        AnalysisState::Unsupported => "unsupported",
    }
}

/// 정점 id 순으로 분석 상태를 전송한다.
pub fn analysis_docs(graph: &Graph) -> Vec<AnalysisDoc> {
    graph
        .analysis()
        .iter()
        .map(|(id, a)| AnalysisDoc {
            body_hash: a.body_hash.clone(),
            diagnostics: a
                .diagnostics
                .iter()
                .map(|d| DiagnosticDoc {
                    code: d.code.clone(),
                    location: d.location.as_ref().map(Into::into),
                    message: d.message.clone(),
                })
                .collect(),
            id: id.as_str().to_owned(),
            scope: a.scope.clone(),
            state: state_str(a.state).into(),
        })
        .collect()
}

/// 간선과 근거의 정렬된 키를 그대로 사용해 같은 스냅샷을 재현한다.
pub fn origin_docs(graph: &Graph) -> Vec<OriginDoc> {
    graph
        .origins()
        .iter()
        .flat_map(|((from, to, kind), origins)| {
            origins.iter().map(|o| OriginDoc {
                body_hash: o.body_hash.clone(),
                from: from.as_str().to_owned(),
                kind: crate::edge_kind_str(*kind).into(),
                location: o.location.as_ref().map(Into::into),
                role: o.role.clone(),
                to: to.as_str().to_owned(),
            })
        })
        .collect()
}

/// 미지 상태는 성공으로 읽지 않고 limitation으로 남긴다.
pub(crate) fn restore(graph: &mut Graph, analyses: &[AnalysisDoc], origins: &[OriginDoc]) {
    for a in analyses {
        let state = match a.state.as_str() {
            "complete" => AnalysisState::Complete,
            "partial" => AnalysisState::Partial,
            "unsupported" => AnalysisState::Unsupported,
            _ => {
                graph.add_limitation(format!("unknown analysis state '{}' for {}", a.state, a.id));
                continue;
            }
        };
        graph.set_analysis(
            VertexId::from_raw(&a.id),
            ObjectAnalysis {
                state,
                scope: a.scope.clone(),
                body_hash: a.body_hash.clone(),
                diagnostics: a
                    .diagnostics
                    .iter()
                    .map(|d| Diagnostic {
                        code: d.code.clone(),
                        message: d.message.clone(),
                        location: d.location.as_ref().map(Into::into),
                    })
                    .collect(),
            },
        );
    }
    for o in origins {
        let Some(kind) = crate::edge_kind_parse(&o.kind) else {
            graph.add_limitation(format!("unknown origin edge kind '{}'", o.kind));
            continue;
        };
        graph.add_origin(
            (VertexId::from_raw(&o.from), VertexId::from_raw(&o.to), kind),
            Origin {
                body_hash: o.body_hash.clone(),
                role: o.role.clone(),
                location: o.location.as_ref().map(Into::into),
            },
        );
    }
}

/// 분석 기록이 없는 구버전 스냅샷을 완전 분석으로 오인하지 않도록 요약한다.
pub fn report(graph: &Graph, subject: Option<&VertexId>) -> serde_json::Value {
    let records: Vec<_> = analysis_docs(graph)
        .into_iter()
        .filter(|r| subject.is_none_or(|id| id.as_str() == r.id))
        .collect();
    let count = |state: &str| records.iter().filter(|a| a.state == state).count();
    serde_json::json!({"state":if records.is_empty(){"unavailable"}else{"available"},"summary":{"complete":count("complete"),"partial":count("partial"),"unsupported":count("unsupported"),"total":records.len()},"objects":records,"limitations":graph.limitations()})
}

#[cfg(test)]
mod tests {
    use super::*;
    use schemagraph_core::{Edge, EdgeKind, Vertex, VertexKind};

    #[test]
    fn diagnostics_and_provenance_survive_canonical_stream_roundtrip() {
        let mut graph = Graph::new();
        for name in ["a", "b"] {
            graph.add_vertex(Vertex {
                id: VertexId::object("s", name),
                kind: VertexKind::View,
                name: name.into(),
                schema: "s".into(),
            });
        }
        let a = VertexId::object("s", "a");
        let b = VertexId::object("s", "b");
        graph.add_edge(Edge {
            from: a.clone(),
            to: b.clone(),
            kind: EdgeKind::DerivesFrom,
            evidence: vec![],
        });
        graph.set_analysis(
            a.clone(),
            ObjectAnalysis {
                state: AnalysisState::Partial,
                scope: "column-dependencies-and-lineage".into(),
                body_hash: Some("sha256:test".into()),
                diagnostics: vec![Diagnostic {
                    code: "SG_COLUMN_AMBIGUOUS".into(),
                    message: "ambiguous name".into(),
                    location: Some(SourceLocation {
                        line: 1,
                        column: 8,
                        end_line: 1,
                        end_column: 10,
                    }),
                }],
            },
        );
        graph.add_origin(
            (a, b, EdgeKind::DerivesFrom),
            Origin {
                body_hash: "sha256:test".into(),
                role: "value".into(),
                location: Some(SourceLocation {
                    line: 2,
                    column: 1,
                    end_line: 2,
                    end_column: 8,
                }),
            },
        );
        let mut stream = Vec::new();
        crate::stream::write_graph(&mut stream, &graph).unwrap();
        assert_eq!(
            stream,
            crate::to_pretty_json(&crate::graph_to_doc(&graph))
                .unwrap()
                .as_bytes()
        );
        let doc: crate::GraphDoc = serde_json::from_slice(&stream).unwrap();
        let restored = crate::graph_from_doc(&doc);
        assert_eq!(graph.analysis(), restored.analysis());
        assert_eq!(graph.origins(), restored.origins());
    }

    #[test]
    fn legacy_graph_does_not_claim_successful_analysis() {
        let doc: crate::GraphDoc =
            serde_json::from_str(r#"{"version":1,"vertices":[],"edges":[]}"#).unwrap();
        let graph = crate::graph_from_doc(&doc);
        assert_eq!(report(&graph, None)["state"], "unavailable");
        assert_eq!(report(&graph, None)["summary"]["total"], 0);
    }
}
