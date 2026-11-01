//! 문서화된 다중 DB 흐름(`merge` → `dead`)에서 모호한 오버로드 호출의 후보가
//! namespace를 붙인 정점을 가리키는지 검증한다. 로컬 id로 남으면 `dead`가 근거를 잃어
//! 보존한 호출자가 부를 수 있는 오버로드를 의존자 없는 후보로 낸다.

use serde_json::{json, Value};
use std::fs;
use std::path::Path;
use std::process::Command;

/// `source_id`를 가진 PostgreSQL 문서를 쓴다.
fn document(root: &Path, file: &str, source_id: &str, routines: Value) {
    let document = json!({
        "version": 1, "reader": "fixture", "dialect": "postgres", "limitations": [],
        "context": {"source_id": source_id, "catalog_complete": true},
        "schemas": [{"name": "public", "objects": [{
            "name": "orders", "kind": "table", "constraints": [], "indexes": [], "triggers": [],
            "columns": [
                {"name": "id", "data_type": "integer", "nullable": false, "ordinal": 1, "pk_position": 1}
            ]}],
            "routines": routines}]
    });
    fs::write(root.join(file), serde_json::to_vec(&document).unwrap()).unwrap();
}

fn run(root: &Path, args: &[&str]) -> (i32, Value) {
    let output = Command::new(env!("CARGO_BIN_EXE_schemagraph"))
        .current_dir(root)
        .args(args)
        .output()
        .unwrap();
    let stdout = if output.stdout.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&output.stdout).unwrap()
    };
    (output.status.code().unwrap(), stdout)
}

#[test]
fn merged_graph_keeps_ambiguous_call_candidates_for_dead() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    document(
        root,
        "app.json",
        "app",
        json!([
            {"name": "caller", "kind": "function", "language": "sql", "body": "SELECT over(1)"},
            {"name": "over", "kind": "function", "language": "sql", "signature": "integer", "body": "SELECT 1"},
            {"name": "over", "kind": "function", "language": "sql", "signature": "text", "body": "SELECT 2"}
        ]),
    );
    document(root, "reporting.json", "reporting", json!([]));
    let (code, _) = run(
        root,
        &[
            "merge",
            "app.json",
            "reporting.json",
            "-o",
            "combined.graph.json",
        ],
    );
    assert_eq!(code, 0);

    let graph: Value =
        serde_json::from_slice(&fs::read(root.join("combined.graph.json")).unwrap()).unwrap();
    let caller = graph["analysis"]
        .as_array()
        .unwrap()
        .iter()
        .find(|analysis| analysis["id"] == "app::public.caller")
        .unwrap_or_else(|| panic!("{graph}"));
    assert_eq!(
        caller["diagnostics"][0]["candidates"],
        json!(["app::public.over(integer)", "app::public.over(text)"]),
        "{caller}"
    );

    // 보존한 호출자가 부를 수 있는 두 오버로드는 후보가 아니다.
    let (code, report) = run(
        root,
        &[
            "dead",
            "--graph",
            "combined.graph.json",
            "--retain",
            "app::public.caller",
            "--strict",
        ],
    );
    assert_eq!(code, 0, "{report}");
    assert_eq!(report["candidates"], json!([]), "{report}");
    assert!(
        report["limitations"].as_array().unwrap().iter().any(|note| note
            == "routines named by ambiguous overloaded calls: 2; each is kept while a live caller may reach it"),
        "{report}"
    );
}
