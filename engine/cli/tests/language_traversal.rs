//! `query`·`impact --format language-traversal`의 CLI 계약 검사.
//!
//! 그래프는 손으로 만든 합성 graph.json이다 — 기대값을 엔진 출력에서 역산하지 않고
//! 간선 목록에서 직접 도출하기 위해서다.

use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::fs;
use std::path::Path;
use std::process::{Command, Output};

const GENERATED_AT: &str = "2026-09-27T00:00:00Z";

/// ```text
/// orders -> customers (references)
/// order_report -> orders, order_report -> products (reads)
/// audit_report -> order_report (reads)
/// catalog -> products (reads)
/// billing.orders는 app.orders와 짧은 이름이 같다(모호성 검사용).
/// ```
fn shop_graph(path: &Path) {
    let vertex = |schema: &str, name: &str, kind: &str| json!({"id": format!("{schema}.{name}"), "kind": kind, "level": "object", "name": name, "schema": schema});
    let edge = |from: &str, to: &str, kind: &str| json!({"from": from, "to": to, "kind": kind});
    let value = json!({
        "version": 2,
        "vertices": [
            vertex("app", "customers", "table"),
            vertex("app", "orders", "table"),
            vertex("app", "products", "table"),
            vertex("app", "order_report", "view"),
            vertex("app", "audit_report", "view"),
            vertex("app", "catalog", "view"),
            vertex("billing", "orders", "table"),
        ],
        "edges": [
            edge("app.orders", "app.customers", "references"),
            edge("app.order_report", "app.orders", "reads"),
            edge("app.order_report", "app.products", "reads"),
            edge("app.audit_report", "app.order_report", "reads"),
            edge("app.catalog", "app.products", "reads"),
        ],
        "limitations": ["synthetic fixture limitation"],
    });
    fs::write(path, serde_json::to_vec_pretty(&value).unwrap()).unwrap();
}

fn run(graph: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_schemagraph"))
        .args(args)
        .arg("--graph")
        .arg(graph)
        .output()
        .expect("run schemagraph")
}

fn traversal(graph: &Path, project: &Path, args: &[&str]) -> Output {
    let mut all = args.to_vec();
    all.extend([
        "--format",
        "language-traversal",
        "--generated-at",
        GENERATED_AT,
        "--project",
    ]);
    all.push(project.to_str().unwrap());
    run(graph, &all)
}

fn json_of(output: &Output) -> Value {
    serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "stdout is not JSON ({error}); stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        )
    })
}

/// 도달 정점을 (usr, depth, via, roots)로 줄인다.
fn rows(document: &Value) -> Vec<(String, u64, String, Vec<u64>)> {
    document["reached"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| {
            (
                item["symbol"]["usr"].as_str().unwrap().to_owned(),
                item["depth"].as_u64().unwrap(),
                item["via"].as_str().unwrap().to_owned(),
                item["roots"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|r| r.as_u64().unwrap())
                    .collect(),
            )
        })
        .collect()
}

fn row(usr: &str, depth: u64, via: &str, roots: &[u64]) -> (String, u64, String, Vec<u64>) {
    (usr.to_owned(), depth, via.to_owned(), roots.to_vec())
}

#[test]
fn impact는_여러_subject를_한_문서로_내고_머리에_그래프_해시를_싣는다() {
    let directory = tempfile::tempdir().unwrap();
    let graph = directory.path().join("shop.graph.json");
    shop_graph(&graph);
    let output = traversal(
        &graph,
        directory.path(),
        &[
            "impact",
            "app.customers",
            "products",
            "--revision",
            "0123abc",
        ],
    );
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let document = json_of(&output);
    let sha = Sha256::digest(fs::read(&graph).unwrap());
    let hex: String = sha.iter().map(|byte| format!("{byte:02x}")).collect();
    let project = fs::canonicalize(directory.path()).unwrap();
    assert_eq!(document["format"], "language-traversal");
    assert_eq!(document["version"], 1);
    assert_eq!(
        document["tool"],
        json!({"name": "schemagraph", "version": env!("CARGO_PKG_VERSION")})
    );
    assert_eq!(document["generatedAt"], GENERATED_AT);
    assert_eq!(document["platform"], "sql");
    assert_eq!(document["project"], project.to_str().unwrap());
    assert_eq!(document["revision"], "0123abc");
    assert_eq!(document["graphRevision"], hex);
    assert_eq!(document["direction"], "dependents");
    assert_eq!(document["truncated"], false);
    assert!(document.get("truncationReasons").is_none());
    assert!(document.get("rootsTruncated").is_none());
    assert_eq!(
        document["limitations"],
        json!(["synthetic fixture limitation"])
    );
    // 짧은 이름으로 준 subject도 roots[].id는 해석된 정점 id다.
    assert_eq!(
        document["roots"],
        json!([
            {"id": "app.customers", "symbol": {"usr": "app.customers", "qualifiedName": "app.customers"}},
            {"id": "app.products", "symbol": {"usr": "app.products", "qualifiedName": "app.products"}},
        ])
    );
    assert_eq!(
        rows(&document),
        [
            row("app.catalog", 1, "app.products", &[1]),
            row("app.order_report", 1, "app.products", &[0, 1]),
            row("app.orders", 1, "app.customers", &[0]),
            row("app.audit_report", 2, "app.order_report", &[0, 1]),
        ]
    );
    assert_eq!(
        document["reached"][2]["relationships"],
        json!(["references"])
    );
}

#[test]
fn 같은_입력은_같은_바이트다() {
    let directory = tempfile::tempdir().unwrap();
    let graph = directory.path().join("shop.graph.json");
    shop_graph(&graph);
    let args = ["impact", "products", "app.customers"];
    let first = traversal(&graph, directory.path(), &args);
    let second = traversal(&graph, directory.path(), &args);
    assert_eq!(first.status.code(), Some(0));
    assert_eq!(first.stdout, second.stdout);
}

#[test]
fn query는_direction으로_정방향을_고르고_depth를_지킨다() {
    let directory = tempfile::tempdir().unwrap();
    let graph = directory.path().join("shop.graph.json");
    shop_graph(&graph);
    let output = traversal(
        &graph,
        directory.path(),
        &[
            "query",
            "audit_report",
            "catalog",
            "--direction",
            "dependencies",
            "--depth",
            "2",
        ],
    );
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let document = json_of(&output);
    assert_eq!(document["direction"], "dependencies");
    // app.customers는 audit_report에서 3걸음이라 depth 2 밖이다.
    assert_eq!(
        rows(&document),
        [
            row("app.order_report", 1, "app.audit_report", &[0]),
            row("app.products", 1, "app.catalog", &[0, 1]),
            row("app.orders", 2, "app.order_report", &[0]),
        ]
    );
}

#[test]
fn 찾지_못한_subject는_자리를_지키고_limitation과_종료코드_1로_알린다() {
    let directory = tempfile::tempdir().unwrap();
    let graph = directory.path().join("shop.graph.json");
    shop_graph(&graph);
    let output = traversal(
        &graph,
        directory.path(),
        &["impact", "orders", "app.products", "missing"],
    );
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    let document = json_of(&output);
    assert_eq!(document["roots"][0], json!({"id": "orders"}));
    assert_eq!(document["roots"][2], json!({"id": "missing"}));
    assert_eq!(document["roots"][1]["id"], "app.products");
    assert_eq!(document["truncated"], true);
    assert_eq!(document["truncationReasons"], json!(["root-not-found"]));
    assert_eq!(
        document["limitations"],
        json!([
            "synthetic fixture limitation",
            "root-not-found: roots[0] 'orders' matches 2 vertices (app.orders, billing.orders); pass a qualified id",
            "root-not-found: roots[2] 'missing' is not a vertex id or unique name in this graph; check the id or rebuild the graph from the same catalog",
        ])
    );
    // 찾은 루트의 탐색과 순번은 입력 위치 그대로다.
    assert_eq!(
        rows(&document),
        [
            row("app.catalog", 1, "app.products", &[1]),
            row("app.order_report", 1, "app.products", &[1]),
            row("app.audit_report", 2, "app.order_report", &[1]),
        ]
    );
}

#[test]
fn 예산_소진과_루트_순번_상한을_보고한다() {
    let directory = tempfile::tempdir().unwrap();
    let graph = directory.path().join("shop.graph.json");
    shop_graph(&graph);
    let limited = traversal(
        &graph,
        directory.path(),
        &["impact", "app.customers", "--max-visited", "2"],
    );
    let document = json_of(&limited);
    assert_eq!(document["truncated"], true);
    assert_eq!(document["truncationReasons"], json!(["visited-limit"]));
    assert_eq!(
        rows(&document),
        [row("app.orders", 1, "app.customers", &[0])]
    );

    // 70개 테이블을 모두 읽는 뷰 하나 — 뷰의 루트 순번은 64개에서 잘린다.
    let wide = directory.path().join("wide.graph.json");
    let tables: Vec<String> = (0..70).map(|n| format!("t{n:02}")).collect();
    let mut vertices: Vec<Value> = tables
        .iter()
        .map(|name| json!({"id": format!("w.{name}"), "kind": "table", "level": "object", "name": name, "schema": "w"}))
        .collect();
    vertices.push(json!({"id": "w.everything", "kind": "view", "level": "object", "name": "everything", "schema": "w"}));
    let edges: Vec<Value> = tables
        .iter()
        .map(|name| json!({"from": "w.everything", "to": format!("w.{name}"), "kind": "reads"}))
        .collect();
    fs::write(
        &wide,
        serde_json::to_vec(&json!({"version": 2, "vertices": vertices, "edges": edges})).unwrap(),
    )
    .unwrap();
    let mut args = vec!["impact"];
    args.extend(tables.iter().map(String::as_str));
    let output = traversal(&wide, directory.path(), &args);
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let document = json_of(&output);
    assert_eq!(document["rootsTruncated"], true);
    assert_eq!(document["truncated"], false);
    assert_eq!(
        rows(&document),
        [row(
            "w.everything",
            1,
            "w.t00",
            &(0..64).collect::<Vec<_>>()
        )]
    );
}

#[test]
fn 기본_json은_그대로이고_새_옵션을_섞으면_거절한다() {
    let directory = tempfile::tempdir().unwrap();
    let graph = directory.path().join("shop.graph.json");
    shop_graph(&graph);
    let default = run(&graph, &["impact", "app.customers"]);
    let explicit = run(&graph, &["impact", "app.customers", "--format", "json"]);
    assert_eq!(default.status.code(), Some(0));
    assert_eq!(default.stdout, explicit.stdout);
    assert_eq!(json_of(&default)["format"], "schemagraph-impact");

    for (args, message) in [
        (
            vec!["impact", "app.customers", "app.products"],
            "impact JSON reports one subject but 2 were given",
        ),
        (
            vec!["query", "app.orders", "--format", "language-traversal"],
            "pass --direction dependencies or --direction dependents",
        ),
        (
            vec!["query", "app.orders", "--direction", "dependencies"],
            "--direction applies only to --format language-traversal",
        ),
        (
            vec!["impact", "app.orders", "--project", "."],
            "--project applies only to --format language-traversal",
        ),
    ] {
        let output = run(&graph, &args);
        assert_eq!(output.status.code(), Some(2), "{args:?}");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains(message), "{args:?}: {stderr}");
    }
}

/// 루트가 다른 루트에 의존하면 그 루트도 reached에 실린다 — FK 의존자가 모두 루트인
/// 실제 사용(테이블 전부를 루트로)에서 결과가 비지 않아야 한다.
#[test]
fn 다른_루트가_닿는_루트도_reached에_실린다() {
    let directory = tempfile::tempdir().unwrap();
    let graph = directory.path().join("shop.graph.json");
    shop_graph(&graph);
    let output = traversal(
        &graph,
        directory.path(),
        &["impact", "app.customers", "app.orders"],
    );
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let document = json_of(&output);
    assert_eq!(
        rows(&document),
        [
            row("app.order_report", 1, "app.orders", &[0, 1]),
            row("app.orders", 1, "app.customers", &[0]),
            row("app.audit_report", 2, "app.order_report", &[0, 1]),
        ]
    );
    assert_eq!(
        document["reached"][1]["relationships"],
        json!(["references"])
    );
}

/// subject가 하나면 language-traversal의 도달 집합은 기본 impact JSON의 impacted와 같다.
/// 단 `--max`로 자르면 남는 이웃이 다르다 — 기본 JSON은 id 순으로, language-traversal은
/// (depth, id) 순으로 정렬한 뒤 자르기 때문이다. 둘 다 자기 정렬의 앞부분이고
/// result-limit로 잘렸다고 알린다는 것을 고정한다(동치 주장은 제한 없는 경우에만 한다).
#[test]
fn subject가_하나면_기본_json과_같은_집합이고_max는_각자의_정렬로_자른다() {
    let directory = tempfile::tempdir().unwrap();
    let graph = directory.path().join("shop.graph.json");
    shop_graph(&graph);
    let default = json_of(&run(&graph, &["impact", "app.customers"]));
    let unlimited = json_of(&traversal(
        &graph,
        directory.path(),
        &["impact", "app.customers"],
    ));
    let by_id: Vec<(String, u64, String)> = default["impacted"]
        .as_array()
        .unwrap()
        .iter()
        .map(|n| {
            (
                n["id"].as_str().unwrap().to_owned(),
                n["distance"].as_u64().unwrap(),
                n["via"].as_str().unwrap().to_owned(),
            )
        })
        .collect();
    let by_depth: Vec<(String, u64, String)> = rows(&unlimited)
        .into_iter()
        .map(|(usr, depth, via, _)| (usr, depth, via))
        .collect();
    let mut sorted = by_depth.clone();
    sorted.sort();
    assert_eq!(sorted, by_id);
    assert_eq!(
        by_depth.iter().map(|r| r.0.as_str()).collect::<Vec<_>>(),
        ["app.orders", "app.order_report", "app.audit_report"]
    );

    let default_cut = json_of(&run(&graph, &["impact", "app.customers", "--max", "1"]));
    let traversal_cut = json_of(&traversal(
        &graph,
        directory.path(),
        &["impact", "app.customers", "--max", "1"],
    ));
    assert_eq!(default_cut["impacted"][0]["id"], json!(by_id[0].0));
    assert_eq!(
        traversal_cut["reached"][0]["symbol"]["usr"],
        json!(by_depth[0].0)
    );
    assert_ne!(
        by_id[0].0, by_depth[0].0,
        "the fixture must separate the two orders"
    );
    for document in [&default_cut, &traversal_cut] {
        assert_eq!(document["truncated"], true);
        assert_eq!(document["truncationReasons"], json!(["result-limit"]));
    }
}
