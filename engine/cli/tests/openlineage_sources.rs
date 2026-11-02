//! 합친 그래프의 `openlineage`가 source마다 다른 데이터셋 정체성을 쓰는지 검증한다.
//!
//! 두 source가 같은 이름의 테이블을 가지면, 로컬 스키마·객체 이름만으로 만든 데이터셋
//! 이름이 겹쳐 OpenLineage 소비자가 없는 source 사이 계보를 그린다. 합친 그래프는 source마다
//! 데이터셋의 데이터베이스 자리(기본은 source id)를 따로 쓰고, 그래도 겹치면 내보내지 않는다.
//! 작업 이름은 바꾸지 않는다.

use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;
use std::process::{Command, Output};

const EVENT_TIME: &str = "2026-11-02T00:00:00Z";
const NAMESPACE: &str = "postgres://shared:5432";

fn column(name: &str, ordinal: u32) -> Value {
    json!({"name": name, "data_type": "integer", "nullable": ordinal != 1,
           "ordinal": ordinal, "pk_position": u32::from(ordinal == 1)})
}

fn table(name: &str) -> Value {
    json!({"name": name, "kind": "table", "constraints": [], "indexes": [],
           "triggers": [], "columns": [column("id", 1), column("val", 2)]})
}

/// `src`를 `dst`로 옮기는 프로시저와, 둘을 조인해 거르는 뷰를 가진 PostgreSQL 문서.
fn document(root: &Path, file: &str, source_id: &str) {
    let view = json!({"name": "report", "kind": "view", "constraints": [], "indexes": [],
        "triggers": [], "columns": [column("id", 1), column("val", 2)],
        "body": "SELECT d.id, s.val FROM public.dst d JOIN public.src s ON s.id = d.id WHERE s.val > 0"});
    let document = json!({
        "version": 1, "reader": "fixture", "dialect": "postgres", "limitations": [],
        "context": {"source_id": source_id, "catalog_complete": true},
        "schemas": [{"name": "public", "objects": [table("src"), table("dst"), view],
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
    document(root, "app.json", "app");
    document(root, "wh.json", "wh");
    succeed(
        root,
        &["merge", "app.json", "wh.json", "-o", "merged.graph.json"],
    );
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
    String::from_utf8(succeed(root, &args))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

/// 이벤트가 가리키는 모든 데이터셋 정체성(namespace, name) — 입력, 출력, inputFields.
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

/// 작업 이름의 source(합친 정점 id의 접두)별 데이터셋 정체성.
fn identities_by_source(events: &[Value]) -> BTreeMap<String, BTreeSet<(String, String)>> {
    let mut by_source: BTreeMap<String, BTreeSet<(String, String)>> = BTreeMap::new();
    for event in events {
        let job = event["job"]["name"].as_str().unwrap();
        let source = job.split("::").next().unwrap().to_owned();
        by_source
            .entry(source)
            .or_default()
            .extend(dataset_identities(event));
    }
    by_source
}

/// T2: 기본값으로 같은 이름의 테이블이 source마다 다른 데이터셋이 되고, 작업은 그대로다.
#[test]
fn merged_sources_with_same_named_tables_get_distinct_dataset_identities() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    merged(root);
    let events = export(root, "merged.graph.json", &[]);
    let by_source = identities_by_source(&events);
    let (app, wh) = (&by_source["app"], &by_source["wh"]);
    // 겹치면 소비자가 없는 app→wh 계보를 그린다.
    assert!(
        app.is_disjoint(wh),
        "shared datasets: {:?}",
        app.intersection(wh).collect::<Vec<_>>()
    );
    let names = |set: &BTreeSet<(String, String)>| {
        set.iter()
            .map(|(namespace, name)| format!("{namespace} {name}"))
            .collect::<Vec<_>>()
    };
    assert_eq!(
        names(app),
        [
            "postgres://shared:5432 app.public.dst",
            "postgres://shared:5432 app.public.report",
            "postgres://shared:5432 app.public.src"
        ]
    );
    assert_eq!(
        names(wh),
        [
            "postgres://shared:5432 wh.public.dst",
            "postgres://shared:5432 wh.public.report",
            "postgres://shared:5432 wh.public.src"
        ]
    );
    // 작업 정체성은 바뀌지 않는다 — 합친 정점 id와 전역 namespace.
    let jobs: Vec<(&str, &str)> = events
        .iter()
        .map(|event| {
            (
                event["job"]["namespace"].as_str().unwrap(),
                event["job"]["name"].as_str().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        jobs,
        [
            (NAMESPACE, "app::public.load"),
            (NAMESPACE, "app::public.report"),
            (NAMESPACE, "wh::public.load"),
            (NAMESPACE, "wh::public.report"),
        ]
    );
}

/// T3: 매핑한 source는 그 namespace·데이터베이스를, 매핑하지 않은 source는 기본값을 쓴다.
#[test]
fn mapped_sources_use_their_namespace_and_database_and_others_keep_the_default() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    merged(root);
    let events = export(
        root,
        "merged.graph.json",
        &[
            "--source-namespace",
            "app=postgres://a:5432",
            "--source-database",
            "app=appdb",
        ],
    );
    let by_source = identities_by_source(&events);
    assert_eq!(
        by_source["app"].iter().cloned().collect::<Vec<_>>(),
        [
            ("postgres://a:5432".into(), "appdb.public.dst".into()),
            ("postgres://a:5432".into(), "appdb.public.report".into()),
            ("postgres://a:5432".into(), "appdb.public.src".into()),
        ]
    );
    assert!(by_source["wh"].contains(&(NAMESPACE.to_owned(), "wh.public.dst".to_owned())));
    let load = events
        .iter()
        .find(|event| event["job"]["name"] == "app::public.load")
        .unwrap();
    assert_eq!(load["job"]["namespace"], NAMESPACE);
    assert_eq!(
        load["outputs"][0]["facets"]["columnLineage"]["fields"]["val"]["inputFields"][0],
        json!({"namespace": "postgres://a:5432", "name": "appdb.public.src", "field": "val",
               "transformations": [{"type": "DIRECT"}]})
    );
}

/// T4: 두 source를 같은 namespace·데이터베이스로 매핑하면 내보내지 않고 두 정점을 알린다.
#[test]
fn identities_that_would_collide_fail_instead_of_conflating() {
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
        ),
        "{stderr}"
    );
    assert!(!root.join("lineage.ndjson").exists());
}

/// T5: 합친 그래프의 `--database`, 단일 그래프의 매핑, 모르는 source는 사용법 오류다.
#[test]
fn naming_options_that_do_not_fit_the_graph_are_usage_errors() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    merged(root);
    succeed(
        root,
        &["scan", "--document", "app.json", "-o", "app.graph.json"],
    );
    for (graph, extra, expected) in [
        (
            "merged.graph.json",
            ["--database", "corpus"],
            "--database names a single source, but this is a merged graph",
        ),
        (
            "app.graph.json",
            ["--source-namespace", "app=postgres://a:5432"],
            "--source-namespace applies to merged graphs",
        ),
        (
            "app.graph.json",
            ["--source-database", "app=appdb"],
            "--source-database applies to merged graphs",
        ),
        (
            "merged.graph.json",
            ["--source-namespace", "nope=postgres://a:5432"],
            "--source-namespace names source 'nope', which is not in the merged graph (sources: app, wh)",
        ),
    ] {
        let mut args = vec![
            "openlineage",
            "--graph",
            graph,
            "--namespace",
            NAMESPACE,
            "--event-time",
            EVENT_TIME,
            "-o",
            "lineage.ndjson",
        ];
        args.extend_from_slice(&extra);
        let output = run(root, &args);
        // 이 CLI의 사용법 오류는 2이고 출력 파일을 만들지 않는다(64는 문서를 쓴 부분 성공 전용).
        assert_eq!(output.status.code(), Some(2), "{extra:?}: {output:?}");
        let stderr = String::from_utf8(output.stderr).unwrap();
        assert!(stderr.contains(expected), "{extra:?}: {stderr}");
        assert!(!root.join("lineage.ndjson").exists(), "{extra:?}");
    }
}

/// 단일 그래프에서 이름 안의 `.`로 두 데이터셋 이름이 겹치면 출력과 종료 코드는 그대로 두고
/// stderr로만 알린다.
#[test]
fn single_graph_name_overlaps_are_reported_on_stderr_only() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    // 스키마 `a`의 테이블 `b.c`와 스키마 `a.b`의 테이블 `c`는 둘 다 `a.b.c`가 된다.
    let view = json!({"name": "both", "kind": "view", "constraints": [], "indexes": [],
        "triggers": [], "columns": [column("id", 1)],
        "body": "SELECT x.id FROM a.\"b.c\" x JOIN \"a.b\".c y ON y.id = x.id"});
    let document = json!({
        "version": 1, "reader": "fixture", "dialect": "postgres", "limitations": [],
        "schemas": [
            {"name": "a", "objects": [table("b.c"), view], "routines": []},
            {"name": "a.b", "objects": [table("c")], "routines": []}
        ]
    });
    fs::write(
        root.join("dotted.json"),
        serde_json::to_vec(&document).unwrap(),
    )
    .unwrap();
    succeed(
        root,
        &[
            "scan",
            "--document",
            "dotted.json",
            "-o",
            "dotted.graph.json",
        ],
    );
    let output = run(
        root,
        &[
            "openlineage",
            "--graph",
            "dotted.graph.json",
            "--namespace",
            NAMESPACE,
            "--event-time",
            EVENT_TIME,
        ],
    );
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let events: Vec<Value> = String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0]["job"]["name"], "a.both");
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(
        stderr.contains("dataset (postgres://shared:5432, a.b.c) would name a%2Eb.c, a.b%2Ec"),
        "{stderr}"
    );
}
