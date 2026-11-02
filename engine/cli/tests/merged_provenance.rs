//! 합친 그래프(`merge`)에서 구조화된 정점 id는 namespace를 따르고, 원문 메시지는 각 source를
//! 분석할 때의 로컬 id를 그대로 둔다는 계약을 검증한다.
//!
//! DML origin 역할 `dml-owner:<작업 id>:<역할>`의 작업 id는 구조화된 id라 namespace를 따라야
//! `openlineage`가 계보를 만든 작업을 찾는다. 진단 메시지와 간선 근거 문구는 자유 문장이라
//! 바꾸지 않는다(ANALYSIS.md의 merge 절).

use serde_json::{json, Value};
use std::fs;
use std::path::Path;
use std::process::Command;

const EVENT_TIME: &str = "2026-11-02T00:00:00Z";

/// `source_id`를 가진 PostgreSQL 문서를 쓴다. 테이블마다 `id`(PK)와 `val` 컬럼이 있다.
fn document(root: &Path, file: &str, source_id: &str, tables: &[&str], routines: Value) {
    let column = |name: &str, ordinal: u32| {
        json!({"name": name, "data_type": "integer", "nullable": ordinal != 1,
               "ordinal": ordinal, "pk_position": u32::from(ordinal == 1)})
    };
    let objects: Vec<Value> = tables
        .iter()
        .map(|name| {
            json!({"name": name, "kind": "table", "constraints": [], "indexes": [],
                   "triggers": [], "columns": [column("id", 1), column("val", 2)]})
        })
        .collect();
    let document = json!({
        "version": 1, "reader": "fixture", "dialect": "postgres", "limitations": [],
        "context": {"source_id": source_id, "catalog_complete": true},
        "schemas": [{"name": "public", "objects": objects, "routines": routines}]
    });
    fs::write(root.join(file), serde_json::to_vec(&document).unwrap()).unwrap();
}

/// 프로시저 `load`가 `src`를 `dst`로 옮기고, `caller`가 오버로드 둘 중 하나를 부르는 `app` 문서와
/// 같은 이름의 테이블이 없는 `reporting` 문서.
fn sources(root: &Path) {
    document(
        root,
        "app.json",
        "app",
        &["src", "dst"],
        json!([
            {"name": "load", "kind": "procedure", "language": "sql",
             "body": "INSERT INTO dst (id, val) SELECT id, val FROM src"},
            {"name": "caller", "kind": "function", "language": "sql", "body": "SELECT over(1)"},
            {"name": "over", "kind": "function", "language": "sql", "signature": "integer", "body": "SELECT 1"},
            {"name": "over", "kind": "function", "language": "sql", "signature": "text", "body": "SELECT 2"}
        ]),
    );
    document(root, "reporting.json", "reporting", &["facts"], json!([]));
}

fn run(root: &Path, args: &[&str]) -> Vec<u8> {
    let output = Command::new(env!("CARGO_BIN_EXE_schemagraph"))
        .current_dir(root)
        .args(args)
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(0), "{args:?}: {output:?}");
    output.stdout
}

/// OpenLineage NDJSON에서 작업 이름으로 첫 출력 데이터셋의 컬럼 계보를 찾는다.
fn column_lineage(ndjson: &[u8], job: &str) -> Value {
    String::from_utf8(ndjson.to_vec())
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .find(|event| event["job"]["name"] == job)
        .unwrap_or_else(|| panic!("no event for job {job}"))["outputs"][0]["facets"]
        ["columnLineage"]["fields"]
        .clone()
}

#[test]
fn merged_graph_keeps_dml_job_column_lineage_in_openlineage() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    sources(root);
    run(
        root,
        &["scan", "--document", "app.json", "-o", "app.graph.json"],
    );
    run(
        root,
        &[
            "merge",
            "app.json",
            "reporting.json",
            "-o",
            "combined.graph.json",
        ],
    );
    let lineage = |graph: &str| {
        run(
            root,
            &[
                "openlineage",
                "--graph",
                graph,
                "--namespace",
                "postgres://db",
                "--event-time",
                EVENT_TIME,
            ],
        )
    };
    let single = column_lineage(&lineage("app.graph.json"), "public.load");
    assert_eq!(
        single["id"]["inputFields"][0]["field"], "id",
        "the single-source export is the reference: {single}"
    );
    // 합친 그래프의 같은 작업도 같은 컬럼 계보를 낸다.
    let merged = column_lineage(&lineage("combined.graph.json"), "app::public.load");
    assert_eq!(merged, single);
}

#[test]
fn merged_graph_namespaces_structured_ids_and_keeps_local_message_text() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    sources(root);
    run(
        root,
        &[
            "merge",
            "app.json",
            "reporting.json",
            "-o",
            "combined.graph.json",
        ],
    );
    let diagnostics: Value = serde_json::from_slice(&run(
        root,
        &[
            "diagnostics",
            "--graph",
            "combined.graph.json",
            "app::public.caller",
        ],
    ))
    .unwrap();
    // 소유자 id와 후보는 namespace를 따르고, 메시지와 limitation은 그 source를 분석할 때의 문장이다.
    // limitation에는 merge가 source 접두를 붙인다.
    assert_eq!(
        diagnostics,
        json!({
            "limitations": [
                "app: public.caller calls public.over, which has 2 overloads; edge omitted"
            ],
            "objects": [{
                "body_hash": "sha256:1f7aa46efc43422fd660667c5895bc575a6dfe6cccec640b4403512b9dcfda76",
                "diagnostics": [{
                    "candidates": ["app::public.over(integer)", "app::public.over(text)"],
                    "code": "SG_BODY_PARTIAL",
                    "message": "public.caller calls public.over, which has 2 overloads; edge omitted"
                }],
                "id": "app::public.caller",
                "scope": "object-dependencies",
                "state": "partial"
            }],
            "state": "available",
            "summary": {"complete": 0, "partial": 1, "total": 1, "unsupported": 0}
        })
    );

    let explain: Value = serde_json::from_slice(&run(
        root,
        &[
            "explain",
            "--graph",
            "combined.graph.json",
            "app::public.load",
        ],
    ))
    .unwrap();
    // 간선 끝과 DML origin의 작업 id는 namespace를 따르고, 근거 문구는 로컬 문장이다.
    let writes: Vec<Value> = explain["edges"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|edge| edge["kind"] == "writes" && edge["to"] == "app::public.dst.id")
        .map(|edge| {
            json!({
                "from": edge["from"],
                "to": edge["to"],
                "evidence": edge["evidence"],
                "roles": edge["origins"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|origin| origin["role"].clone())
                    .collect::<Vec<_>>(),
            })
        })
        .collect();
    assert_eq!(
        writes,
        [json!({
            "from": "app::public.load",
            "to": "app::public.dst.id",
            "evidence": [{"detail": "public.load Writes public.dst.id (write)", "layer": "body-parse"}],
            "roles": ["body-dependency", "dml-owner:app::public.load:write"]
        })]
    );
}
