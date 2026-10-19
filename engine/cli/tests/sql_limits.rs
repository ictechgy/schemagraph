//! 아주 긴 연산 사슬이 든 SQL 파일도 scan이 abort 없이 끝나고, 분석 상한을 넘은
//! 몸체는 실제로 센 수를 limitation으로 남기는지 검증한다.

use serde_json::{json, Value};
use std::fs;
use std::path::Path;
use std::process::Command;

fn catalog(root: &Path) {
    let document = json!({
        "version": 1, "reader": "fixture", "dialect": "postgres", "limitations": [],
        "schemas": [{"name": "public", "routines": [], "objects": [{
            "name": "t", "kind": "table", "constraints": [], "indexes": [], "triggers": [],
            "columns": [
                {"name": "id", "data_type": "integer", "nullable": false, "ordinal": 1, "pk_position": 1},
                {"name": "a", "data_type": "text", "nullable": true, "ordinal": 2, "pk_position": 0}
            ]}]}]
    });
    fs::write(
        root.join("catalog.json"),
        serde_json::to_vec(&document).unwrap(),
    )
    .unwrap();
}

/// `id = 0 OR id = 1 …` — 항마다 이항 연산자가 둘이다.
fn or_chain(terms: usize) -> String {
    (0..terms)
        .map(|term| format!("id = {term}"))
        .collect::<Vec<_>>()
        .join(" OR ")
}

fn scan(root: &Path) -> Value {
    let output = Command::new(env!("CARGO_BIN_EXE_schemagraph"))
        .current_dir(root)
        .args([
            "scan",
            "--document",
            "catalog.json",
            "--sql-dir",
            "sql",
            "-o",
            "graph.json",
        ])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    serde_json::from_slice(&fs::read(root.join("graph.json")).unwrap()).unwrap()
}

/// SQL 파일 하나에서 만든 query 정점의 분석 기록.
fn analysis<'a>(graph: &'a Value, source: &str) -> &'a Value {
    graph["analysis"]
        .as_array()
        .unwrap()
        .iter()
        .find(|record| record["source"] == source)
        .unwrap_or_else(|| panic!("analysis for {source}: {graph}"))
}

#[test]
fn scan_survives_long_operator_chains_and_counts_skipped_bodies() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    catalog(root);
    fs::create_dir(root.join("sql")).unwrap();
    // Oracle·구 SQL Server의 다중 행 INSERT 관용구 — 집합 연산자 2,999개.
    let rows: Vec<String> = (0..3_000)
        .map(|row| format!("SELECT {row}, 'v' FROM t"))
        .collect();
    fs::write(
        root.join("sql/seed.sql"),
        format!("INSERT INTO t (id, a) {};\n", rows.join(" UNION ALL ")),
    )
    .unwrap();
    // 상한 안의 OR 사슬. 디버그 빌드에서는 기본 메인 스레드 스택(8MiB)을 넘게 재귀하므로
    // 분석이 전용 스레드에서 돌아야 끝난다.
    fs::write(
        root.join("sql/wide.sql"),
        format!("SELECT a FROM t WHERE {}\n", or_chain(10_000)),
    )
    .unwrap();
    // 이항 연산자 100,001개 — 분석 상한을 넘는다.
    fs::write(
        root.join("sql/huge.sql"),
        format!("SELECT a FROM t WHERE {}\n", or_chain(50_001)),
    )
    .unwrap();

    let graph = scan(root);

    let seed = &analysis(&graph, "seed.sql")["id"];
    assert!(
        graph["edges"]
            .as_array()
            .unwrap()
            .iter()
            .any(|edge| &edge["from"] == seed
                && edge["kind"] == "writes"
                && edge["to"] == "public.t"),
        "{graph}"
    );
    assert_eq!(analysis(&graph, "wide.sql")["state"], "complete");
    assert_eq!(analysis(&graph, "huge.sql")["state"], "partial");
    let limitations = graph["limitations"].as_array().unwrap();
    assert!(
        limitations.iter().any(|limitation| limitation
            .as_str()
            .unwrap()
            .contains("100001 chained infix operators, over the 100000 analysis limit")),
        "{limitations:?}"
    );
}
