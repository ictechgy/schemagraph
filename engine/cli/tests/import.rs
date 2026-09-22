use serde_json::{json, Value};
use std::fs;
use std::path::Path;
use std::process::{Command, Output};

fn write_catalog(path: &Path) {
    fs::write(
        path,
        serde_json::to_vec(&json!({
            "version": 1,
            "dialect": "postgres",
            "reader": "fixture",
            "limitations": [],
            "context": {
                "source_id": "import-check",
                "database": "corpus",
                "schema_filter": ["public"],
                "catalog_complete": true
            },
            "schemas": [{
                "name": "public",
                "objects": [{
                    "name": "orders",
                    "kind": "table",
                    "columns": [{
                        "name": "id",
                        "data_type": "integer",
                        "nullable": false,
                        "ordinal": 1,
                        "pk_position": 1
                    }],
                    "constraints": [],
                    "indexes": [],
                    "triggers": []
                }],
                "routines": []
            }],
            "dependencies": []
        }))
        .unwrap(),
    )
    .unwrap();
}

fn write_manifest(path: &Path, sql_path: &str, sql: &str) {
    fs::write(
        path,
        serde_json::to_vec(&json!({
            "nodes": {
                "model.demo.orders": {
                    "resource_type": "model",
                    "unique_id": "model.demo.orders",
                    "database": "corpus",
                    "schema": "public",
                    "compiled_code": sql,
                    "compiled_path": sql_path,
                    "original_file_path": "models/{{ ref(\"orders\") }}.sql"
                }
            }
        }))
        .unwrap(),
    )
    .unwrap();
}

fn run(cwd: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_schemagraph"))
        .current_dir(cwd)
        .args(args)
        .output()
        .unwrap()
}

#[test]
fn import_outputs_hashes_and_canonical_query_ids() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    write_catalog(&root.join("catalog.json"));
    fs::create_dir(root.join("models")).unwrap();
    let sql = "SELECT id FROM public.orders\n";
    fs::write(root.join("models/orders.sql"), sql).unwrap();
    write_manifest(&root.join("manifest.json"), "models/orders.sql", sql);
    fs::write(root.join(".dbt.json.tmp-preexisting"), "preserve").unwrap();

    let dbt = run(
        root,
        &[
            "import",
            "catalog.json",
            "--format",
            "dbt",
            "--input",
            "manifest.json",
            "--output",
            "dbt.json",
            "--report",
            "dbt.report.json",
            "--project-root",
            ".",
        ],
    );
    assert!(
        dbt.status.success(),
        "{}",
        String::from_utf8_lossy(&dbt.stderr)
    );
    let report: Value =
        serde_json::from_slice(&fs::read(root.join("dbt.report.json")).unwrap()).unwrap();
    for key in ["sql_sha256", "input_sha256", "catalog_sha256"] {
        assert_eq!(report[key].as_str().unwrap().len(), 64);
    }
    assert_eq!(
        report["entries"][0]["routine_id"],
        "public.query_dbt_6d6f64656c2e64656d6f2e6f7264657273"
    );
    assert_eq!(
        fs::read_to_string(root.join(".dbt.json.tmp-preexisting")).unwrap(),
        "preserve"
    );

    let log = root.join("queries.jsonl");
    fs::write(
        &log,
        concat!(
            "{\"version\":1,\"type\":\"query-log\",\"source_id\":\"import-check\",\"database\":\"corpus\",\"window_start\":\"2026-01-01T00:00:00Z\",\"window_end\":\"2026-01-01T01:00:00Z\",\"sampling\":\"all\"}\n",
            "{\"query_id\":\"q:1\",\"schema\":\"public\",\"sql\":\"SELECT id FROM public.orders\",\"observed_at\":\"2026-01-01T00:30:00Z\",\"executions\":1}\n"
        ),
    )
    .unwrap();
    let query = run(
        root,
        &[
            "import",
            "catalog.json",
            "--format",
            "query-log",
            "--input",
            "queries.jsonl",
            "--output",
            "query.json",
            "--report",
            "query.report.json",
        ],
    );
    assert!(
        query.status.success(),
        "{}",
        String::from_utf8_lossy(&query.stderr)
    );
    let report: Value =
        serde_json::from_slice(&fs::read(root.join("query.report.json")).unwrap()).unwrap();
    assert!(report["entries"][0]["routine_id"]
        .as_str()
        .unwrap()
        .contains("%3A"));
    let document: Value =
        serde_json::from_slice(&fs::read(root.join("query.json")).unwrap()).unwrap();
    assert!(document["schemas"][0]["routines"][0].get("usage").is_none());
}

#[test]
fn import_rejects_existing_outputs_and_reimport_without_partial_files() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    write_catalog(&root.join("catalog.json"));
    fs::write(root.join("manifest.json"), "{\"nodes\":{}}").unwrap();
    fs::write(root.join("existing.json"), "keep").unwrap();
    let existing = run(
        root,
        &[
            "import",
            "catalog.json",
            "--format",
            "dbt",
            "--input",
            "manifest.json",
            "--output",
            "existing.json",
            "--report",
            "new.report.json",
        ],
    );
    assert_eq!(existing.status.code(), Some(2));
    assert_eq!(
        fs::read_to_string(root.join("existing.json")).unwrap(),
        "keep"
    );
    assert!(!root.join("new.report.json").exists());

    let sql = "SELECT 1";
    fs::create_dir(root.join("models")).unwrap();
    fs::write(root.join("models/orders.sql"), sql).unwrap();
    write_manifest(&root.join("manifest.json"), "models/orders.sql", sql);
    let first = run(
        root,
        &[
            "import",
            "catalog.json",
            "--format",
            "dbt",
            "--input",
            "manifest.json",
            "--output",
            "augmented.json",
            "--report",
            "augmented.report.json",
            "--project-root",
            ".",
        ],
    );
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    fs::write(
        root.join("queries.jsonl"),
        concat!(
            "{\"version\":1,\"type\":\"query-log\",\"source_id\":\"import-check\",\"database\":\"corpus\",\"window_start\":\"2026-01-01T00:00:00Z\",\"window_end\":\"2026-01-01T01:00:00Z\",\"sampling\":\"all\"}\n",
            "{\"query_id\":\"q-1\",\"schema\":\"public\",\"sql\":\"SELECT 1\",\"observed_at\":\"2026-01-01T00:20:00Z\",\"executions\":1}\n"
        ),
    )
    .unwrap();
    let combined = run(
        root,
        &[
            "import",
            "augmented.json",
            "--format",
            "query-log",
            "--input",
            "queries.jsonl",
            "--output",
            "combined.json",
            "--report",
            "combined.report.json",
        ],
    );
    assert!(
        combined.status.success(),
        "{}",
        String::from_utf8_lossy(&combined.stderr)
    );
    let repeat = run(
        root,
        &[
            "import",
            "augmented.json",
            "--format",
            "dbt",
            "--input",
            "manifest.json",
            "--output",
            "repeat.json",
            "--report",
            "repeat.report.json",
            "--project-root",
            ".",
        ],
    );
    assert_eq!(repeat.status.code(), Some(2));
    assert!(!root.join("repeat.json").exists());
    assert!(!root.join("repeat.report.json").exists());

    fs::write(root.join("empty-manifest.json"), "{\"nodes\":{}}").unwrap();
    let empty_repeat = run(
        root,
        &[
            "import",
            "augmented.json",
            "--format",
            "dbt",
            "--input",
            "empty-manifest.json",
            "--output",
            "empty-repeat.json",
            "--report",
            "empty-repeat.report.json",
        ],
    );
    assert_eq!(empty_repeat.status.code(), Some(2));
    assert!(!root.join("empty-repeat.json").exists());
    assert!(!root.join("empty-repeat.report.json").exists());
}

#[cfg(unix)]
#[test]
fn import_rejects_symlink_and_hardlink_input_aliases() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    write_catalog(&root.join("catalog.json"));
    fs::write(root.join("manifest.json"), "{\"nodes\":{}}").unwrap();
    std::os::unix::fs::symlink(root.join("manifest.json"), root.join("manifest-link.json"))
        .unwrap();
    let symlink = run(
        root,
        &[
            "import",
            "catalog.json",
            "--format",
            "dbt",
            "--input",
            "manifest-link.json",
            "--output",
            "symlink.json",
            "--report",
            "symlink.report.json",
        ],
    );
    assert_eq!(symlink.status.code(), Some(2));
    assert!(!root.join("symlink.json").exists());

    fs::hard_link(root.join("catalog.json"), root.join("catalog-alias.json")).unwrap();
    let alias = run(
        root,
        &[
            "import",
            "catalog.json",
            "--format",
            "dbt",
            "--input",
            "catalog-alias.json",
            "--output",
            "alias.json",
            "--report",
            "alias.report.json",
        ],
    );
    assert_eq!(alias.status.code(), Some(2));
    assert!(!root.join("alias.json").exists());
}
