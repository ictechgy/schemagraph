//! `rules --level`이 컬럼 간선마다 갈라지던 위반을 투영한 레벨의 간선 하나로 묶는지,
//! 레벨을 주지 않으면 기존 보고와 바이트까지 같은지 검증한다.

use serde_json::{json, Value};
use std::fs;
use std::path::Path;
use std::process::Command;

/// `rpt.v`가 `public.orders`의 컬럼 다섯 개를 읽는 문서와 그 접근을 금지하는 규칙.
fn fixture(root: &Path) {
    let column = |name: &str, ordinal: u32, nullable: bool| {
        json!({"name": name, "data_type": "integer", "nullable": nullable,
               "ordinal": ordinal, "pk_position": u32::from(ordinal == 1 && !nullable)})
    };
    let names = ["a", "b", "c", "d", "e"];
    let table_columns: Vec<Value> = (1..)
        .zip(names)
        .map(|(ordinal, name)| column(name, ordinal, false))
        .collect();
    let view_columns: Vec<Value> = (1..)
        .zip(names)
        .map(|(ordinal, name)| column(name, ordinal, true))
        .collect();
    let document = json!({
        "version": 1, "reader": "fixture", "dialect": "postgres", "limitations": [],
        "schemas": [
            {"name": "public", "routines": [], "objects": [{
                "name": "orders", "kind": "table", "constraints": [], "indexes": [],
                "triggers": [], "columns": table_columns}]},
            {"name": "rpt", "routines": [], "objects": [{
                "name": "v", "kind": "view", "constraints": [], "indexes": [], "triggers": [],
                "columns": view_columns,
                "body": "SELECT a, b, c, d, e FROM public.orders"}]}
        ]
    });
    fs::write(
        root.join("catalog.json"),
        serde_json::to_vec(&document).unwrap(),
    )
    .unwrap();
    fs::write(
        root.join("rules.toml"),
        "[[rule]]\nname = \"rpt does not read public\"\nfrom = \"rpt.*\"\nto = \"public.*\"\n",
    )
    .unwrap();
}

fn run(root: &Path, args: &[&str]) -> (i32, Vec<u8>) {
    let output = Command::new(env!("CARGO_BIN_EXE_schemagraph"))
        .current_dir(root)
        .args(args)
        .output()
        .unwrap();
    (output.status.code().unwrap(), output.stdout)
}

/// 위반 간선을 (from, kind, to)로 줄인다.
fn edges(report: &[u8]) -> Vec<(String, String, String)> {
    let report: Value = serde_json::from_slice(report).unwrap();
    report["violations"]
        .as_array()
        .unwrap()
        .iter()
        .map(|violation| {
            let edge = &violation["edge"];
            let text = |key: &str| edge[key].as_str().unwrap().to_owned();
            (text("from"), text("kind"), text("to"))
        })
        .collect()
}

#[test]
fn object_level_rules_report_one_violation_per_object_edge_kind() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fixture(root);
    let (code, _) = run(
        root,
        &["scan", "--document", "catalog.json", "-o", "graph.json"],
    );
    assert_eq!(code, 0);

    // 기본 보고는 기존처럼 기록된 간선마다 위반을 낸다: 테이블 1 + 컬럼 읽기 5 + 계보 5.
    let rules = ["rules", "-g", "graph.json", "-c", "rules.toml"];
    let (code, member) = run(root, &rules);
    assert_eq!(code, 0);
    assert_eq!(edges(&member).len(), 11);
    let member_report: Value = serde_json::from_slice(&member).unwrap();
    assert!(member_report.get("level").is_none(), "{member_report}");

    let (code, object) = run(
        root,
        &[&rules[..], &["--level", "object", "--strict"]].concat(),
    );
    assert_eq!(code, 1);
    assert_eq!(
        edges(&object),
        [
            ("rpt.v".into(), "reads".into(), "public.orders".into()),
            (
                "rpt.v".into(),
                "derives-from".into(),
                "public.orders".into()
            ),
        ]
    );
    let object_report: Value = serde_json::from_slice(&object).unwrap();
    assert_eq!(object_report["level"], "object");
    assert_eq!(object_report["checked"], 1);

    // 스키마 레벨은 스키마 쌍으로 묶이고, 규칙 글롭은 스키마 정점 id에 대조된다.
    let schema_rules =
        "[[rule]]\nname = \"rpt does not read public\"\nfrom = \"rpt\"\nto = \"public\"\n";
    fs::write(root.join("schema-rules.toml"), schema_rules).unwrap();
    let (code, schema) = run(
        root,
        &[
            "rules",
            "-g",
            "graph.json",
            "-c",
            "schema-rules.toml",
            "--level",
            "schema",
        ],
    );
    assert_eq!(code, 0);
    assert_eq!(
        edges(&schema),
        [
            ("rpt".into(), "reads".into(), "public".into()),
            ("rpt".into(), "derives-from".into(), "public".into()),
        ]
    );
}

#[test]
fn level_rules_report_patterns_that_match_no_vertex_and_reject_member_level() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fixture(root);
    let (code, _) = run(
        root,
        &["scan", "--document", "catalog.json", "-o", "graph.json"],
    );
    assert_eq!(code, 0);
    // 스키마 레벨 정점 id는 `public`이라 `public.*`에 맞지 않는다. 위반 0건을 통과로
    // 읽지 않도록, 아무 정점에도 맞지 않는 패턴을 limitation으로 알린다.
    let (code, report) = run(
        root,
        &[
            "rules",
            "-g",
            "graph.json",
            "-c",
            "rules.toml",
            "--level",
            "schema",
            "--strict",
        ],
    );
    assert_eq!(code, 0);
    let report: Value = serde_json::from_slice(&report).unwrap();
    assert_eq!(report["violations"], json!([]));
    let limitations: Vec<&str> = report["limitations"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(Value::as_str)
        .collect();
    assert!(
        limitations.contains(
            &"rule 'rpt does not read public': from pattern 'rpt.*' matches no schema-level vertex"
        ) && limitations.contains(
            &"rule 'rpt does not read public': to pattern 'public.*' matches no schema-level vertex"
        ),
        "{limitations:?}"
    );
    // 객체 레벨에서는 두 패턴 모두 정점에 맞아 알릴 것이 없다.
    let (_, report) = run(
        root,
        &[
            "rules",
            "-g",
            "graph.json",
            "-c",
            "rules.toml",
            "--level",
            "object",
        ],
    );
    let report: Value = serde_json::from_slice(&report).unwrap();
    assert!(!report["limitations"]
        .as_array()
        .unwrap()
        .iter()
        .any(|note| note.as_str().unwrap().contains("matches no")));
    // 멤버 레벨 투영은 객체 간선을 지우므로 rules에는 받지 않는다.
    for level in ["member", "column"] {
        let (code, _) = run(
            root,
            &[
                "rules",
                "-g",
                "graph.json",
                "-c",
                "rules.toml",
                "--level",
                level,
            ],
        );
        assert_eq!(code, 2, "{level}");
    }
}
