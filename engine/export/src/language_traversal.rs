//! isthmus `language-traversal` v1 — 다중 루트 탐색 결과를 언어 공통 형식으로 옮긴다.
//!
//! isthmus `trace`는 여러 생산자의 탐색 결과를 생산자 id 정확 일치로만 잇는다. 그래서
//! 이 문서의 모든 id는 schemagraph 정점 id이고, `facts`가 relation-decl의
//! `symbol.usr`에 싣는 값과 같다. 계약의 정본은 isthmus 문서이며, 이 모듈은 탐색
//! 결과를 그 모양으로 옮기기만 한다 — 조인·gap 판정은 isthmus가 한다.

use schemagraph_analysis::traversal::{MultiRootReport, ReachedVertex};
use schemagraph_core::VertexId;
use serde_json::{json, Value};

use crate::edge_kind_str;

/// 문서의 `format` 값.
pub const FORMAT: &str = "language-traversal";

/// 문서의 와이어 버전.
pub const VERSION: u32 = 1;

/// schemagraph가 내는 문서의 `platform` 값 — `facts`의 bridge-facts와 같다.
pub const PLATFORM: &str = "sql";

/// 탐색 방향. 정방향은 루트가 의존하는 대상, 역방향은 루트에 의존하는 정점이다.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    /// 루트가 의존하는 대상(나가는 간선) — `query`의 dependencies와 같다.
    Dependencies,
    /// 루트에 의존하는 정점(들어오는 간선) — `impact`와 같다.
    Dependents,
}

impl Direction {
    /// 와이어 라벨.
    pub fn label(self) -> &'static str {
        match self {
            Self::Dependencies => "dependencies",
            Self::Dependents => "dependents",
        }
    }

    /// 탐색이 들어오는 간선을 따라가는가.
    pub fn is_reverse(self) -> bool {
        matches!(self, Self::Dependents)
    }
}

/// 요청된 루트 하나 — 사용자가 준 문자열과 해석된 정점 id.
#[derive(Debug, Clone)]
pub struct RootInput<'a> {
    /// 명령행에 준 원문(이름 또는 정규 id).
    pub requested: &'a str,
    /// 해석된 정점 id. 해석하지 못했으면 None이다.
    pub resolved: Option<&'a VertexId>,
}

/// 문서 머리 — 탐색 결과가 아니라 호출자가 관측한 값들이다.
#[derive(Debug, Clone)]
pub struct Header<'a> {
    /// 이 바이너리의 버전.
    pub tool_version: &'a str,
    /// RFC 3339 생성 시각.
    pub generated_at: &'a str,
    /// 다른 생산자 문서와 공유하는 project 루트(realpath).
    pub project: &'a str,
    /// 호출자가 준 소스 리비전(git 커밋 등). 없으면 키를 뺀다.
    pub revision: Option<&'a str>,
    /// 입력 graph.json 바이트의 SHA-256(소문자 16진수).
    pub graph_revision: &'a str,
    /// 탐색 방향.
    pub direction: Direction,
}

/// 다중 루트 탐색 결과를 `language-traversal` v1 문서로 만든다.
///
/// 해석된 루트는 `symbol`을 싣고, 해석하지 못한 루트는 요청 원문을 `id`로 두고
/// `symbol`을 뺀다 — 루트 순번이 입력 위치와 어긋나지 않게 자리를 지킨다. 선택 필드
/// (`revision`, `rootsTruncated`, `truncationReasons`)는 값이 없으면 키를 뺀다.
pub fn to_value(
    header: &Header,
    roots: &[RootInput],
    report: &MultiRootReport,
    limitations: &[String],
) -> Value {
    let mut value = json!({
        "format": FORMAT,
        "version": VERSION,
        "tool": { "name": "schemagraph", "version": header.tool_version },
        "generatedAt": header.generated_at,
        "platform": PLATFORM,
        "project": header.project,
        "graphRevision": header.graph_revision,
        "direction": header.direction.label(),
        "roots": roots.iter().map(root_value).collect::<Vec<_>>(),
        "reached": report.reached.iter().map(reached_value).collect::<Vec<_>>(),
        "truncated": !report.truncation_reasons.is_empty(),
        "limitations": limitations,
    });
    if let Some(revision) = header.revision {
        value["revision"] = json!(revision);
    }
    if report.roots_truncated {
        value["rootsTruncated"] = json!(true);
    }
    if !report.truncation_reasons.is_empty() {
        value["truncationReasons"] = json!(report.truncation_reasons);
    }
    value
}

/// 정점 id의 symbol — `facts` relation-decl과 같은 두 키에 같은 값을 싣는다.
fn symbol_value(id: &VertexId) -> Value {
    json!({ "usr": id.as_str(), "qualifiedName": id.as_str() })
}

fn root_value(root: &RootInput) -> Value {
    match root.resolved {
        Some(id) => json!({ "id": id.as_str(), "symbol": symbol_value(id) }),
        None => json!({ "id": root.requested }),
    }
}

fn reached_value(vertex: &ReachedVertex) -> Value {
    let neighbor = &vertex.neighbor;
    json!({
        "symbol": symbol_value(&neighbor.vertex.id),
        "via": neighbor.via.as_str(),
        "depth": neighbor.distance,
        "roots": vertex.roots,
        "relationships": neighbor.edges.iter().map(|kind| edge_kind_str(*kind)).collect::<Vec<_>>(),
    })
}

/// 해석하지 못한 루트의 limitation 문구다.
///
/// 원인(이름이 없음 / 짧은 이름이 여러 정점에 맞음)과 해결 방향을 함께 싣는다. 접두사
/// `root-not-found:`는 `truncationReasons`의 같은 이름과 짝을 이룬다.
pub fn root_not_found_limitation(index: usize, requested: &str, candidates: &[VertexId]) -> String {
    if candidates.is_empty() {
        return format!(
            "root-not-found: roots[{index}] '{requested}' is not a vertex id or unique name in this graph; check the id or rebuild the graph from the same catalog"
        );
    }
    let names: Vec<&str> = candidates.iter().map(VertexId::as_str).collect();
    format!(
        "root-not-found: roots[{index}] '{requested}' matches {} vertices ({}); pass a qualified id",
        names.len(),
        names.join(", ")
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use schemagraph_analysis::budget::Budget;
    use schemagraph_analysis::traversal::walk_roots;
    use schemagraph_core::{Edge, EdgeKind, Graph, Vertex, VertexKind};

    fn id(name: &str) -> VertexId {
        VertexId::object("app", name)
    }

    /// orders -> customers, report -> orders.
    fn chain() -> Graph {
        let mut graph = Graph::new();
        for name in ["customers", "orders", "report"] {
            graph.add_vertex(Vertex {
                id: id(name),
                kind: VertexKind::Table,
                name: name.into(),
                schema: "app".into(),
            });
        }
        for (from, to, kind) in [
            ("orders", "customers", EdgeKind::References),
            ("report", "orders", EdgeKind::Reads),
        ] {
            graph.add_edge(Edge {
                from: id(from),
                to: id(to),
                kind,
                evidence: vec![],
            });
        }
        graph
    }

    fn header(revision: Option<&str>) -> Header<'_> {
        Header {
            tool_version: "0.0.0-test",
            generated_at: "2026-09-27T00:00:00Z",
            project: "/work/app",
            revision,
            graph_revision: "ab12",
            direction: Direction::Dependents,
        }
    }

    fn keys(value: &Value) -> Vec<&str> {
        value
            .as_object()
            .expect("object")
            .keys()
            .map(String::as_str)
            .collect()
    }

    #[test]
    fn 문서는_계약의_키와_symbol을_싣는다() {
        let graph = chain();
        let root = id("customers");
        let report = walk_roots(
            &graph,
            &[Some(root.clone())],
            u32::MAX,
            usize::MAX,
            true,
            Budget::unlimited(),
            None,
        );
        let roots = [RootInput {
            requested: "customers",
            resolved: Some(&root),
        }];
        let value = to_value(&header(Some("abc123")), &roots, &report, &[]);
        assert_eq!(
            keys(&value),
            [
                "direction",
                "format",
                "generatedAt",
                "graphRevision",
                "limitations",
                "platform",
                "project",
                "reached",
                "revision",
                "roots",
                "tool",
                "truncated",
                "version",
            ]
        );
        assert_eq!(
            value["roots"],
            json!([{"id": "app.customers", "symbol": {"usr": "app.customers", "qualifiedName": "app.customers"}}])
        );
        assert_eq!(
            value["reached"],
            json!([
                {"symbol": {"usr": "app.orders", "qualifiedName": "app.orders"},
                 "via": "app.customers", "depth": 1, "roots": [0], "relationships": ["references"]},
                {"symbol": {"usr": "app.report", "qualifiedName": "app.report"},
                 "via": "app.orders", "depth": 2, "roots": [0], "relationships": ["reads"]},
            ])
        );
        assert_eq!(
            (&value["format"], &value["version"], &value["platform"]),
            (&json!("language-traversal"), &json!(1), &json!("sql"))
        );
        assert_eq!(
            value["tool"],
            json!({"name": "schemagraph", "version": "0.0.0-test"})
        );
        assert_eq!(value["truncated"], json!(false));
    }

    #[test]
    fn 해석하지_못한_루트는_symbol_없이_자리를_지키고_선택_필드가_실린다() {
        let graph = chain();
        let root = id("customers");
        let report = walk_roots(
            &graph,
            &[None, Some(root.clone())],
            u32::MAX,
            usize::MAX,
            true,
            Budget::unlimited(),
            None,
        );
        let roots = [
            RootInput {
                requested: "nope",
                resolved: None,
            },
            RootInput {
                requested: "app.customers",
                resolved: Some(&root),
            },
        ];
        let note = root_not_found_limitation(0, "nope", &[]);
        let value = to_value(&header(None), &roots, &report, std::slice::from_ref(&note));
        assert_eq!(value["roots"][0], json!({"id": "nope"}));
        assert_eq!(value["reached"][0]["roots"], json!([1]));
        assert_eq!(value["truncated"], json!(true));
        assert_eq!(value["truncationReasons"], json!(["root-not-found"]));
        assert_eq!(value["limitations"], json!([note]));
        assert!(value.get("revision").is_none());
        assert!(value.get("rootsTruncated").is_none());
    }

    #[test]
    fn 모호한_이름의_limitation은_후보와_해결_방향을_싣는다() {
        let candidates = [
            VertexId::object("billing", "orders"),
            VertexId::object("shop", "orders"),
        ];
        let note = root_not_found_limitation(2, "orders", &candidates);
        assert_eq!(
            note,
            "root-not-found: roots[2] 'orders' matches 2 vertices (billing.orders, shop.orders); pass a qualified id"
        );
    }

    /// 루트 번호가 잘린 보고서만 rootsTruncated 키를 싣는다.
    #[test]
    fn roots_truncated는_잘렸을_때만_실린다() {
        let report = MultiRootReport {
            reached: Vec::new(),
            visited: 0,
            examined_edges: 0,
            truncation_reasons: Vec::new(),
            complete: true,
            roots_truncated: true,
        };
        let value = to_value(&header(None), &[], &report, &[]);
        assert_eq!(value["rootsTruncated"], json!(true));
        assert!(value.get("truncationReasons").is_none());
    }
}
