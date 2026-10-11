//! 실패한 scan과 경로 별칭이 입력·기존 스냅샷을 손상하지 않는지 검증한다.

use serde_json::{json, Value};
use std::fs;
use std::path::Path;
use std::process::{Command, Output};

fn catalog(root: &Path, name: &str, source: &str) -> Vec<u8> {
    let bytes = serde_json::to_vec(&json!({
        "version": 1, "reader": "fixture", "dialect": "sqlite", "limitations": [],
        "context": {"source_id": source, "catalog_complete": true},
        "schemas": [{"name": "main", "objects": [{"name": "orders", "kind": "table",
            "columns": [], "indexes": [], "constraints": [], "triggers": []}], "routines": []}]
    }))
    .unwrap();
    fs::write(root.join(name), &bytes).unwrap();
    bytes
}

fn run(root: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_schemagraph"))
        .current_dir(root)
        .args(args)
        .output()
        .unwrap()
}

fn assert_error(output: Output, message: &str) {
    assert_eq!(output.status.code(), Some(2), "{output:?}");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains(message),
        "{output:?}"
    );
}

#[test]
fn scan_rejects_catalog_overwrite_and_identical_output_paths() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let original = catalog(root, "catalog.json", "app");
    for args in [
        vec!["scan", "--document", "catalog.json", "-o", "./catalog.json"],
        vec![
            "scan",
            "--document",
            "catalog.json",
            "--emit-document",
            "catalog.json",
            "-o",
            "graph.json",
        ],
        vec![
            "scan",
            "--document",
            "catalog.json",
            "--emit-document",
            "./graph.json",
            "-o",
            "graph.json",
        ],
    ] {
        assert_error(run(root, &args), "must be distinct");
        assert_eq!(fs::read(root.join("catalog.json")).unwrap(), original);
        assert!(!root.join("graph.json").exists());
    }
}

#[test]
fn scan_protects_local_database_before_opening_it() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fs::write(root.join("my db.sqlite"), b"preserve the database").unwrap();
    for url in [
        "my db.sqlite",
        "sqlite:my%20db.sqlite",
        "sqlite://my%20db.sqlite?mode=ro",
        "file:my%20db.sqlite?mode=ro",
        "sqlite:file:my%20db.sqlite?mode=rw",
        "sqlite://file:my%2520db.sqlite?mode=rwc",
    ] {
        assert_error(
            run(root, &["scan", url, "-o", "my db.sqlite"]),
            "must be distinct",
        );
        assert_error(
            run(
                root,
                &[
                    "scan",
                    url,
                    "-o",
                    "graph.json",
                    "--emit-document",
                    "my db.sqlite",
                ],
            ),
            "must be distinct",
        );
        assert_eq!(
            fs::read(root.join("my db.sqlite")).unwrap(),
            b"preserve the database"
        );
        assert!(!root.join("graph.json").exists());
    }
}

#[test]
fn scan_preflights_all_outputs_before_publishing_catalog() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    catalog(root, "catalog.json", "app");
    fs::write(root.join("emitted.json"), b"previous catalog").unwrap();
    assert_error(
        run(
            root,
            &[
                "scan",
                "--document",
                "catalog.json",
                "--emit-document",
                "emitted.json",
                "-o",
                "missing/graph.json",
            ],
        ),
        "output directory",
    );
    assert_eq!(
        fs::read(root.join("emitted.json")).unwrap(),
        b"previous catalog"
    );
    assert_eq!(fs::read_dir(root).unwrap().count(), 2);
}

#[test]
fn scan_rejects_output_names_that_only_differ_by_case() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    catalog(root, "catalog.json", "app");
    assert_error(
        run(
            root,
            &[
                "scan",
                "--document",
                "catalog.json",
                "-o",
                "graph.json",
                "--emit-document",
                "GRAPH.json",
            ],
        ),
        "letter case",
    );
    assert!(!root.join("graph.json").exists());
    assert!(!root.join("GRAPH.json").exists());
}

#[test]
fn scan_protects_attached_sql_inputs_and_case_insensitive_extensions() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    catalog(root, "catalog.json", "app");
    fs::create_dir_all(root.join("queries/nested")).unwrap();
    let input = "queries/nested/orders.SQL";
    fs::write(root.join(input), b"SELECT * FROM main.orders").unwrap();
    for args in [
        vec![
            "scan",
            "--document",
            "catalog.json",
            "--sql-dir",
            "queries",
            "-o",
            input,
        ],
        vec![
            "scan",
            "--document",
            "catalog.json",
            "--sql-dir",
            "queries",
            "--emit-document",
            input,
            "-o",
            "graph.json",
        ],
    ] {
        assert_error(run(root, &args), "must be distinct");
        assert_eq!(
            fs::read(root.join(input)).unwrap(),
            b"SELECT * FROM main.orders"
        );
        assert!(!root.join("graph.json").exists());
    }
}

#[cfg(unix)]
#[test]
fn scan_protects_hardlinks_to_sql_inputs_outside_the_sql_directory() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    catalog(root, "catalog.json", "app");
    fs::create_dir(root.join("queries")).unwrap();
    fs::write(
        root.join("queries/orders.sql"),
        b"SELECT * FROM main.orders",
    )
    .unwrap();
    fs::hard_link(root.join("queries/orders.sql"), root.join("graph.json")).unwrap();
    assert_error(
        run(
            root,
            &[
                "scan",
                "--document",
                "catalog.json",
                "--sql-dir",
                "queries",
                "-o",
                "graph.json",
            ],
        ),
        "must be distinct",
    );
    assert_eq!(
        fs::read(root.join("queries/orders.sql")).unwrap(),
        b"SELECT * FROM main.orders"
    );
}

#[test]
fn scan_failure_after_catalog_serialization_preserves_both_previous_outputs() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    catalog(root, "catalog.json", "app");
    fs::write(root.join("emitted.json"), b"previous catalog").unwrap();
    fs::write(root.join("graph.json"), b"previous graph").unwrap();
    fs::write(root.join("cache-is-file"), b"keep").unwrap();
    assert_error(
        run(
            root,
            &[
                "scan",
                "--document",
                "catalog.json",
                "--emit-document",
                "emitted.json",
                "-o",
                "graph.json",
                "--cache-dir",
                "cache-is-file",
            ],
        ),
        "analysis cache",
    );
    assert_eq!(
        fs::read(root.join("emitted.json")).unwrap(),
        b"previous catalog"
    );
    assert_eq!(
        fs::read(root.join("graph.json")).unwrap(),
        b"previous graph"
    );
    assert_eq!(fs::read_dir(root).unwrap().count(), 4);
}

#[test]
fn scan_replaces_outputs_with_identical_stdout_and_file_bytes() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    catalog(root, "catalog.json", "app");
    let stdout = run(root, &["scan", "--document", "catalog.json", "-o", "-"]);
    assert!(stdout.status.success(), "{stdout:?}");
    fs::write(root.join("graph.json"), b"previous graph").unwrap();
    let output = run(
        root,
        &[
            "scan",
            "--document",
            "catalog.json",
            "-o",
            "graph.json",
            "--emit-document",
            "emitted.json",
        ],
    );
    assert!(output.status.success(), "{output:?}");
    assert_eq!(fs::read(root.join("graph.json")).unwrap(), stdout.stdout);
    let emitted: Value =
        serde_json::from_slice(&fs::read(root.join("emitted.json")).unwrap()).unwrap();
    assert_eq!(emitted["schemas"][0]["objects"][0]["name"], "orders");
    assert_eq!(fs::read_dir(root).unwrap().count(), 3);
}

#[test]
fn merge_and_facts_reject_input_overwrite() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let original = catalog(root, "a.json", "app");
    catalog(root, "b.json", "reporting");
    for args in [
        vec!["merge", "a.json", "b.json", "-o", "a.json"],
        vec![
            "facts",
            "--document",
            "a.json",
            "--project",
            ".",
            "-o",
            "a.json",
        ],
    ] {
        assert_error(run(root, &args), "must be distinct");
        assert_eq!(fs::read(root.join("a.json")).unwrap(), original);
    }
}

#[test]
fn openlineage_rejects_graph_overwrite() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let bytes = br#"{"version":2,"vertices":[],"edges":[],"limitations":[]}"#;
    fs::write(root.join("graph.json"), bytes).unwrap();
    assert_error(
        run(
            root,
            &[
                "openlineage",
                "--graph",
                "graph.json",
                "--namespace",
                "urn:test",
                "-o",
                "graph.json",
            ],
        ),
        "must be distinct",
    );
    assert_eq!(fs::read(root.join("graph.json")).unwrap(), bytes);
}

#[cfg(unix)]
#[test]
fn scan_rejects_symlinks_hardlinks_and_parent_directory_aliases() {
    use std::os::unix::fs::symlink;
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let original = catalog(root, "catalog.json", "app");
    symlink("catalog.json", root.join("symlink.json")).unwrap();
    fs::hard_link(root.join("catalog.json"), root.join("hardlink.json")).unwrap();
    symlink(root, root.join("alias")).unwrap();
    for (path, message) in [
        ("symlink.json", "regular file"),
        ("hardlink.json", "must be distinct"),
        ("alias/catalog.json", "must be distinct"),
    ] {
        assert_error(
            run(root, &["scan", "--document", "catalog.json", "-o", path]),
            message,
        );
        assert_eq!(fs::read(root.join("catalog.json")).unwrap(), original);
    }
    assert!(fs::symlink_metadata(root.join("symlink.json"))
        .unwrap()
        .file_type()
        .is_symlink());
}

#[cfg(unix)]
#[test]
fn read_only_and_special_outputs_are_preserved() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    catalog(root, "catalog.json", "app");
    fs::write(root.join("graph.json"), b"private snapshot").unwrap();
    fs::set_permissions(root.join("graph.json"), fs::Permissions::from_mode(0o400)).unwrap();
    assert_error(
        run(
            root,
            &["scan", "--document", "catalog.json", "-o", "graph.json"],
        ),
        "read-only",
    );
    assert_eq!(
        fs::read(root.join("graph.json")).unwrap(),
        b"private snapshot"
    );
    assert_error(
        run(
            root,
            &["scan", "--document", "catalog.json", "-o", "/dev/null"],
        ),
        "regular file",
    );
}
