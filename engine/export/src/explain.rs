//! 경로·근거의 공통 출력으로 CLI와 MCP의 결과를 일치시킨다.

use schemagraph_analysis::paths::{Explanation, PathReport};
use schemagraph_core::{Edge, Graph};
use serde_json::{json, Value};

/// 같은 정점 쌍에 여러 종류의 간선이 있으면 각각의 근거를 보존한다.
pub fn edge_value(edge: &Edge) -> Value {
    json!({"from":edge.from.as_str(),"to":edge.to.as_str(),"kind":crate::edge_kind_str(edge.kind),"dependency":edge.kind.is_dependency(),"evidence":edge.evidence.iter().map(|e|json!({"layer":crate::layer_str(e.layer),"detail":e.detail})).collect::<Vec<_>>()})
}

/// 탐색을 완료하지 못했으면 빈 경로가 관계 부재를 뜻하지 않게 표시한다.
pub fn path_value(report: &PathReport) -> Value {
    json!({"from":report.from.as_str(),"to":report.to.as_str(),"direction":if report.reverse{"impact"}else{"dependency"},"paths":report.paths.iter().map(|path|path.iter().map(|id|id.as_str()).collect::<Vec<_>>()).collect::<Vec<_>>(),"edges":report.edges.iter().map(edge_value).collect::<Vec<_>>(),"visited":report.visited,"examinedEdges":report.examined_edges,"truncated":report.truncated,"truncationReasons":report.truncation_reasons,"limitations":report.limitations})
}

/// 기존 간선과 정렬된 근거를 함께 직렬화한다. 원문 SQL은 자동으로 공개하지 않는다.
pub fn explanation_value(report: &Explanation, graph: &Graph) -> Value {
    let edges: Vec<_> = report
        .edges
        .iter()
        .map(|item| {
            let mut value = edge_value(&item.edge);
            if !item.origins.is_empty() {
                value["origins"] = json!(item
                    .origins
                    .iter()
                    .map(|o| {
                        let mut origin = json!({"body_hash":o.body_hash,"role":o.role});
                        if let Some(location) = &o.location {
                            origin["location"] =
                                json!(crate::diagnostics::LocationDoc::from(location));
                        }
                        origin
                    })
                    .collect::<Vec<_>>());
            }
            value
        })
        .collect();
    let mut value = json!({"subject":crate::subject_value(&report.subject),"edges":edges,"totalEdges":report.total_edges,"truncated":report.truncated,"limitations":report.limitations});
    if report.analysis.is_some() {
        value["analysis"] =
            crate::diagnostics::report(graph, Some(&report.subject.id))["objects"][0].clone();
    }
    if let Some(usage) = &report.usage {
        value["usage"] = json!(crate::UsageDoc {
            since: usage.since.clone(),
            reads: usage.reads,
            writes: usage.writes,
            total_ms: usage.total_ms,
            self_ms: usage.self_ms
        });
    }
    value
}
