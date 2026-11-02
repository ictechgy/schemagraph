//! 합친 그래프의 `openlineage`가 source마다 다른 데이터셋 정체성을 쓰는지 검증한다.
//!
//! 두 source가 같은 이름의 테이블을 가지면, 로컬 스키마·객체 이름만으로 만든 데이터셋
//! 이름이 겹쳐 OpenLineage 소비자가 없는 source 사이 계보를 그린다. 합친 그래프는 source마다
//! (namespace, 데이터베이스) 자리를 따로 쓰고, 그래도 겹치면 내보내지 않는다.

use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;
use std::process::{Command, Output};

const EVENT_TIME: &str = "2026-11-02T00:00:00Z";
const NAMESPACE: &str = "postgres://shared:5432";

/// `public.src`를 `public.dst`로 옮기는 프로시저와 뷰를 가진 PostgreSQL 문서.
fn document(root: &Path, file: &str, source_id: &str, schema: &str) {
    let column = |name: &str, ordinal: u32| {
        json!({"name": name, "data_type": "integer", "nullable": ordinal != 1,
               "ordinal": ordinal, "pk_position": u32::from(ordinal == 1)})
    };
    let table = |name: &str| {
        json!({"name": name, "kind": "table", "constraints": [], "indexes": [],
               "triggers": [], "columns": [column("id", 1), column("val", 2)]})
    };
    let view = json!({"name": "report", "kind": "view", "constraints": [], "indexes": [],
        "triggers": [], "columns": [column("id", 1), column("val", 2)],
        "body": format!("SELECT d.id, s.val FROM {schema}.dst d JOIN {schema}.src s ON s.id = d.id WHERE s.val > 0")});
    let document = json!({
        "version": 1, "reader": "fixture", "dialect": "postgres", "limitations": [],
        "context": {"source_id": source_id, "catalog_complete": true},
        "schemas": [{"name": schema, "objects": [table("src"), table("dst"), view],
            "routines": [{"name": "load", "kind": "procedure", "language": "sql",
                "body": "INSERT INTO dst (id, val) SELECT id, val FROM src"}]}]
    });
    fs::write(root.join(file), serde_json::to_vec(&document).unwrap()).unwrap();
}

fn run(root: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_schemagraph"))
        .current_dir(root)
        .args(args)
        .output()
        .unwrap()
}

fn succeed(root: &Path, args: &[&str]) -> Vec<u8> {
    let output = run(root, args);
    assert_eq!(output.status.code(), Some(0), "{args:?}: {output:?}");
    output.stdout
}

/// 두 source `app`·`wh`를 같은 스키마·테이블 이름으로 만들어 합친다.
fn merged(root: &Path) {
    document(root, "app.json", "app", "public");
    document(root, "wh.json", "wh", "public");
    succeed(
        root,
        &["merge", "app.json", "wh.json", "-o", "merged.graph.json"],
    );
}

fn events(ndjson: &[u8]) -> Vec<Value> {
    String::from_utf8(ndjson.to_vec())
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

fn export(root: &Path, graph: &str, extra: &[&str]) -> Vec<Value> {
    let mut args = vec![
        "openlineage",
        "--graph",
        graph,
        "--namespace",
        NAMESPACE,
        "--event-time",
        EVENT_TIME,
    ];
    args.extend_from_slice(extra);
    events(&succeed(root, &args))
}

/// 이벤트가 가리키는 모든 데이터셋 정체성(namespace, name).
fn dataset_identities(event: &Value) -> BTreeSet<(String, String)> {
    let mut identities = BTreeSet::new();
    let mut add = |dataset: &Value| {
        identities.insert((
            dataset["namespace"].as_str().unwrap().to_owned(),
            dataset["name"].as_str().unwrap().to_owned(),
        ));
    };
    for side in ["inputs", "outputs"] {
        for dataset in event[side].as_array().unwrap() {
            add(dataset);
            let lineage = &dataset["facets"]["columnLineage"];
            for field in lineage["fields"]
                .as_object()
                .into_iter()
                .flat_map(|f| f.values())
            {
                for input in field["inputFields"].as_array().unwrap() {
                    add(input);
                }
            }
            for input in lineage["dataset"].as_array().into_iter().flatten() {
                add(input);
            }
        }
    }
    identities
}

#[test]
fn merged_sources_with_same_named_tables_get_distinct_dataset_identities() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    merged(root);
    let events = export(root, "merged.graph.json", &[]);
    // 작업은 source마다 하나씩이다(합친 그래프의 정점 id로 source를 가른다).
    let mut by_source: BTreeMap<String, BTreeSet<(String, String)>> = BTreeMap::new();
    for event in &events {
        let job = event["job"]["name"].as_str().unwrap();
        let source = if job.starts_with("app") { "app" } else { "wh" };
        by_source
            .entry(source.to_owned())
            .or_default()
            .extend(dataset_identities(event));
    }
    let app = &by_source["app"];
    let wh = &by_source["wh"];
    // 두 source의 src·dst·report는 서로 다른 데이터셋이다 — 겹치면 없는 app→wh 계보가 생긴다.
    assert!(
        app.is_disjoint(wh),
        "shared datasets: {:?}",
        app.intersection(wh).collect::<Vec<_>>()
    );
    assert_eq!(app.len() + wh.len(), 6, "{by_source:?}");
    // 기본값은 source id를 데이터베이스 자리에 둔다 — 작업과 데이터셋이 같은 규칙이다.
    assert_eq!(
        app.iter()
            .map(|(_, name)| name.as_str())
            .collect::<Vec<_>>(),
        ["app.public.dst", "app.public.report", "app.public.src"]
    );
    let jobs: Vec<&str> = events
        .iter()
        .map(|event| event["job"]["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        jobs,
        [
            "app.public.load",
            "app.public.report",
            "wh.public.load",
            "wh.public.report"
        ]
    );
}

#[test]
fn merged_export_with_source_mappings_equals_each_single_source_export() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    merged(root);
    let mappings = [
        ("app", "postgres://app-host:5432", "appdb"),
        ("wh", "postgres://wh-host:5432", "whdb"),
    ];
    let mut flags = Vec::new();
    for (source, uri, database) in mappings {
        flags.push("--source-namespace".to_owned());
        flags.push(format!("{source}={uri}"));
        flags.push("--source-database".to_owned());
        flags.push(format!("{source}={database}"));
    }
    let flags: Vec<&str> = flags.iter().map(String::as_str).collect();
    let merged = export(root, "merged.graph.json", &flags);
    for (source, uri, database) in mappings {
        let graph = format!("{source}.graph.json");
        succeed(
            root,
            &[
                "scan",
                "--document",
                &format!("{source}.json"),
                "-o",
                &graph,
            ],
        );
        let single = events(&succeed(
            root,
            &[
                "openlineage",
                "--graph",
                &graph,
                "--namespace",
                uri,
                "--database",
                database,
                "--event-time",
                EVENT_TIME,
            ],
        ));
        // runId까지 같은 이벤트 — 매핑한 합친 내보내기는 그 source를 따로 내보낸 결과와 같다.
        let from_merged: Vec<&Value> = merged
            .iter()
            .filter(|event| event["job"]["namespace"] == uri)
            .collect();
        assert_eq!(from_merged, single.iter().collect::<Vec<_>>(), "{source}");
    }
    assert_eq!(merged.len(), 4);
}

#[test]
fn database_flag_is_rejected_on_merged_graphs() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    merged(root);
    let output = run(
        root,
        &[
            "openlineage",
            "--graph",
            "merged.graph.json",
            "--namespace",
            NAMESPACE,
            "--database",
            "corpus",
            "--event-time",
            EVENT_TIME,
            "-o",
            "lineage.ndjson",
        ],
    );
    // 사용법 오류는 이 CLI 계약대로 2이고, 출력 파일을 만들지 않는다.
    assert_eq!(output.status.code(), Some(2), "{output:?}");
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(
        stderr.contains("--database names a single source")
            && stderr.contains("--source-database SRC=NAME"),
        "{stderr}"
    );
    assert!(!root.join("lineage.ndjson").exists());
}

#[test]
fn identities_that_would_collide_fail_instead_of_conflating() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    merged(root);
    // 두 source를 같은 namespace·데이터베이스로 매핑하면 같은 이름의 테이블이 한 데이터셋이 된다.
    let output = run(
        root,
        &[
            "openlineage",
            "--graph",
            "merged.graph.json",
            "--namespace",
            NAMESPACE,
            "--source-database",
            "app=corpus",
            "--source-database",
            "wh=corpus",
            "--event-time",
            EVENT_TIME,
            "-o",
            "lineage.ndjson",
        ],
    );
    assert_eq!(output.status.code(), Some(2), "{output:?}");
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(
        stderr.contains(
            "dataset (postgres://shared:5432, corpus.public.dst) would name app::public.dst, wh::public.dst"
        ) && stderr.contains("job (postgres://shared:5432, corpus.public.load) would name app::public.load, wh::public.load"),
        "{stderr}"
    );
    assert!(!root.join("lineage.ndjson").exists());
}

#[test]
fn source_mappings_are_rejected_on_single_graphs_and_for_unknown_sources() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    merged(root);
    succeed(
        root,
        &["scan", "--document", "app.json", "-o", "app.graph.json"],
    );
    for (graph, flag, expected) in [
        (
            "app.graph.json",
            "app=postgres://app-host:5432",
            "--source-namespace applies to merged graphs",
        ),
        (
            "merged.graph.json",
            "nope=postgres://app-host:5432",
            "--source-namespace names source 'nope', which is not in the merged graph (sources: app, wh)",
        ),
    ] {
        let output = run(
            root,
            &[
                "openlineage",
                "--graph",
                graph,
                "--namespace",
                NAMESPACE,
                "--source-namespace",
                flag,
                "--event-time",
                EVENT_TIME,
            ],
        );
        assert_eq!(output.status.code(), Some(2), "{graph}: {output:?}");
        let stderr = String::from_utf8(output.stderr).unwrap();
        assert!(stderr.contains(expected), "{graph}: {stderr}");
    }
}
