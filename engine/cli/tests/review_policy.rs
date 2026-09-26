use serde_json::{json, Value};
use std::fs;
use std::path::Path;
use std::process::{Command, Output};

fn document(path: &Path, source_id: &str, removed_name: bool) {
    let mut columns = vec![json!({
        "name": "id",
        "data_type": "integer",
        "nullable": false,
        "ordinal": 1,
        "pk_position": 1
    })];
    if !removed_name {
        columns.push(json!({
            "name": "name",
            "data_type": "text",
            "nullable": true,
            "ordinal": 2,
            "pk_position": 0
        }));
    }
    let value = json!({
        "version": 1,
        "dialect": "sqlite",
        "reader": "review-policy-test",
        "limitations": [],
        "context": {
            "source_id": source_id,
            "database": "review",
            "schema_filter": ["app"],
            "catalog_complete": true
        },
        "schemas": [{
            "name": "app",
            "objects": [{
                "name": "users",
                "kind": "table",
                "columns": columns,
                "constraints": [],
                "indexes": [],
                "triggers": []
            }],
            "routines": []
        }],
        "dependencies": []
    });
    fs::write(path, serde_json::to_vec(&value).unwrap()).unwrap();
}

fn mark_incomplete(path: &Path) {
    let mut value: Value = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
    value["limitations"] = json!(["catalog fixture intentionally incomplete"]);
    value["context"]["catalog_complete"] = json!(false);
    fs::write(path, serde_json::to_vec(&value).unwrap()).unwrap();
}

fn policy(path: &Path) {
    fs::write(
        path,
        "version = 1\nfail_threshold = \"high\"\n[severity]\ncolumn-removed = \"high\"\n",
    )
    .unwrap();
}

fn low_severity_policy(path: &Path) {
    fs::write(
        path,
        "version = 1\nfail_threshold = \"high\"\n[severity]\ncolumn-removed = \"low\"\n",
    )
    .unwrap();
}

fn run(before: &Path, after: &Path, values: &[&str]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_schemagraph"));
    command.arg("review").arg(before).arg(after).args(values);
    command.output().unwrap()
}

fn json_output(output: &Output) -> Value {
    serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "stdout was not JSON: {error}; stderr={}",
            String::from_utf8_lossy(&output.stderr)
        )
    })
}

fn edit_document(path: &Path, edit: impl FnOnce(&mut Value)) {
    let mut value: Value = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
    edit(&mut value);
    fs::write(path, serde_json::to_vec(&value).unwrap()).unwrap();
}

fn index(predicate: &str, unique: bool) -> Value {
    json!({"name":"users_idx", "unique":unique, "columns":["id"],
           "definition_complete":true, "predicate":predicate, "has_predicate":true})
}

#[test]
fn changed_index_predicate_and_unique_index_addition_require_review() {
    let directory = tempfile::tempdir().unwrap();
    let before = directory.path().join("before.json");
    let after = directory.path().join("after.json");
    document(&before, "prod", false);
    document(&after, "prod", false);
    edit_document(&before, |doc| {
        doc["schemas"][0]["objects"][0]["indexes"] = json!([index("id > 0", true)])
    });
    edit_document(&after, |doc| {
        doc["schemas"][0]["objects"][0]["indexes"] = json!([index("id > 1", true)])
    });
    let changed = run(&before, &after, &["--strict"]);
    assert_eq!(changed.status.code(), Some(1));
    assert_eq!(
        json_output(&changed)["changes"][0]["change"],
        "indexes-changed"
    );
    document(&before, "prod", false);
    let added = run(&before, &after, &["--strict"]);
    assert_eq!(added.status.code(), Some(1));
    assert_eq!(
        json_output(&added)["changes"][0]["change"],
        "unique-index-added"
    );
}

#[test]
fn empty_schema_removal_is_a_reviewable_change() {
    let directory = tempfile::tempdir().unwrap();
    let before = directory.path().join("before.json");
    let after = directory.path().join("after.json");
    document(&before, "prod", false);
    document(&after, "prod", false);
    edit_document(&before, |doc| {
        doc["schemas"]
            .as_array_mut()
            .unwrap()
            .push(json!({"name":"empty.schema", "objects":[], "routines":[]}))
    });
    for path in [&before, &after] {
        edit_document(path, |doc| {
            doc["context"]
                .as_object_mut()
                .unwrap()
                .remove("schema_filter");
        });
    }
    let removed = run(&before, &after, &["--strict"]);
    assert_eq!(removed.status.code(), Some(1));
    let report = json_output(&removed);
    assert_eq!(report["totalChanges"], 1);
    assert_eq!(report["changes"][0]["change"], "schema-removed");
    assert_eq!(report["changes"][0]["id"], "empty%2Eschema");
}

#[test]
fn baseline_cannot_overwrite_its_catalog_input() {
    let directory = tempfile::tempdir().unwrap();
    let before = directory.path().join("before.json");
    let after = directory.path().join("after.json");
    document(&before, "prod", false);
    document(&after, "prod", true);
    let original = fs::read(&before).unwrap();
    let output = run(
        &before,
        &after,
        &["--write-baseline", before.to_str().unwrap()],
    );
    assert_eq!(output.status.code(), Some(2));
    assert_eq!(fs::read(&before).unwrap(), original);
}

#[test]
fn absent_database_identity_does_not_allow_baseline_suppression() {
    let directory = tempfile::tempdir().unwrap();
    let before = directory.path().join("before.json");
    let after = directory.path().join("after.json");
    document(&before, "prod", false);
    document(&after, "prod", true);
    for path in [&before, &after] {
        edit_document(path, |doc| {
            doc["context"].as_object_mut().unwrap().remove("database");
        });
    }
    let baseline = directory.path().join("baseline.json");
    let output = run(
        &before,
        &after,
        &["--write-baseline", baseline.to_str().unwrap()],
    );
    assert_eq!(output.status.code(), Some(2));
    assert!(!baseline.exists());
}

#[test]
fn policy_gate_counts_hidden_changes_when_max_changes_is_zero() {
    let directory = tempfile::tempdir().unwrap();
    let before = directory.path().join("before.json");
    let after = directory.path().join("after.json");
    let policy_path = directory.path().join("review.toml");
    document(&before, "prod", false);
    document(&after, "prod", true);
    policy(&policy_path);

    let output = run(
        &before,
        &after,
        &[
            "--strict",
            "--policy",
            policy_path.to_str().unwrap(),
            "--max-changes",
            "0",
        ],
    );
    assert_eq!(
        output.status.code(),
        Some(1),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value = json_output(&output);
    assert_eq!(value["totalChanges"], 1);
    assert_eq!(value["changes"].as_array().unwrap().len(), 0);
    assert_eq!(value["policy"]["failed"], true);
    assert_eq!(value["policy"]["findings"].as_array().unwrap().len(), 0);

    let sarif = run(
        &before,
        &after,
        &[
            "--strict",
            "--policy",
            policy_path.to_str().unwrap(),
            "--format",
            "sarif",
            "--max-changes",
            "0",
        ],
    );
    assert_eq!(sarif.status.code(), Some(1));
    let sarif_value = json_output(&sarif);
    assert_eq!(
        sarif_value["runs"][0]["results"].as_array().unwrap().len(),
        0
    );
    assert_eq!(
        sarif_value["runs"][0]["properties"]["policy"]["failedFindings"],
        1
    );
}

#[test]
fn strict_with_policy_uses_policy_threshold_instead_of_legacy_review_gate() {
    let directory = tempfile::tempdir().unwrap();
    let before = directory.path().join("before.json");
    let after = directory.path().join("after.json");
    let policy_path = directory.path().join("review.toml");
    document(&before, "prod", false);
    document(&after, "prod", true);
    low_severity_policy(&policy_path);

    let output = run(
        &before,
        &after,
        &["--strict", "--policy", policy_path.to_str().unwrap()],
    );
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value = json_output(&output);
    assert_eq!(value["policy"]["failed"], false);

    let report_only = directory.path().join("report-only.toml");
    policy(&report_only);
    let report_only_output = run(
        &before,
        &after,
        &["--policy", report_only.to_str().unwrap()],
    );
    assert_eq!(report_only_output.status.code(), Some(0));
    assert_eq!(json_output(&report_only_output)["policy"]["failed"], true);
}

#[test]
fn baseline_is_existing_only_for_the_same_logical_source() {
    let directory = tempfile::tempdir().unwrap();
    let before = directory.path().join("before.json");
    let after = directory.path().join("after.json");
    let baseline = directory.path().join("review-baseline.json");
    let policy_path = directory.path().join("review.toml");
    document(&before, "prod", false);
    document(&after, "prod", true);
    policy(&policy_path);

    let write = run(
        &before,
        &after,
        &["--write-baseline", baseline.to_str().unwrap()],
    );
    assert_eq!(
        write.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&write.stderr)
    );
    let existing = run(
        &before,
        &after,
        &[
            "--policy",
            policy_path.to_str().unwrap(),
            "--baseline",
            baseline.to_str().unwrap(),
            "--strict",
        ],
    );
    assert_eq!(
        existing.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&existing.stderr)
    );
    let value = json_output(&existing);
    assert_eq!(value["policy"]["findings"][0]["baselineState"], "existing");
    assert_eq!(value["policy"]["findings"][0]["suppressed"], true);

    document(&before, "staging", false);
    document(&after, "staging", true);
    let different_source = run(
        &before,
        &after,
        &[
            "--policy",
            policy_path.to_str().unwrap(),
            "--baseline",
            baseline.to_str().unwrap(),
            "--strict",
        ],
    );
    assert_eq!(different_source.status.code(), Some(1));
    let value = json_output(&different_source);
    assert_eq!(value["policy"]["findings"][0]["baselineState"], "new");
    assert_eq!(value["policy"]["findings"][0]["suppressed"], false);
}

#[test]
fn baseline_and_waiver_cannot_suppress_an_incomplete_comparison() {
    let directory = tempfile::tempdir().unwrap();
    let before = directory.path().join("before.json");
    let after = directory.path().join("after.json");
    let baseline = directory.path().join("review-baseline.json");
    let waiver = directory.path().join("review-waiver.toml");
    document(&before, "prod", false);
    document(&after, "prod", true);
    let write = run(
        &before,
        &after,
        &["--write-baseline", baseline.to_str().unwrap()],
    );
    assert_eq!(write.status.code(), Some(0));
    let baseline_value: Value = serde_json::from_slice(&fs::read(&baseline).unwrap()).unwrap();
    let fingerprint = baseline_value["fingerprints"][0].as_str().unwrap();
    fs::write(
        &waiver,
        format!(
            "version = 1\nfail_threshold = \"high\"\nwaivers = [{{ fingerprint = \"{fingerprint}\", reason = \"tracked\", expires = \"2026-12-31\" }}]\n"
        ),
    )
    .unwrap();
    mark_incomplete(&after);

    let baseline_output = run(&before, &after, &["--baseline", baseline.to_str().unwrap()]);
    assert_eq!(baseline_output.status.code(), Some(2));
    let waiver_output = run(
        &before,
        &after,
        &[
            "--policy",
            waiver.to_str().unwrap(),
            "--as-of",
            "2026-09-22",
        ],
    );
    assert_eq!(waiver_output.status.code(), Some(2));
}

#[test]
fn malformed_policy_is_a_cli_error_and_sarif_has_logical_locations() {
    let directory = tempfile::tempdir().unwrap();
    let before = directory.path().join("before.json");
    let after = directory.path().join("after.json");
    let invalid = directory.path().join("invalid.toml");
    document(&before, "prod", false);
    document(&after, "prod", true);
    fs::write(
        &invalid,
        "version = 1\nfail_threshold = \"high\"\n[severity]\nmade-up-rule = \"high\"\n",
    )
    .unwrap();
    let malformed = run(&before, &after, &["--policy", invalid.to_str().unwrap()]);
    assert_eq!(malformed.status.code(), Some(2));
    assert!(
        String::from_utf8_lossy(&malformed.stderr).contains("unknown review policy change kind")
    );

    let sarif = run(&before, &after, &["--format", "sarif"]);
    assert_eq!(sarif.status.code(), Some(0));
    let value = json_output(&sarif);
    assert_eq!(value["version"], "2.1.0");
    let result = &value["runs"][0]["results"][0];
    assert!(result["locations"][0]["logicalLocations"].is_array());
    assert!(result.get("physicalLocation").is_none());
}

/// review finding의 impacted도 query·impact와 같은 `via`를 싣는다.
///
/// RELEASE-NOTES와 ANALYSIS가 이 계약을 약속하므로, review 직렬화가 공용 이웃
/// 직렬화를 거치지 않게 바뀌면 여기서 드러나야 한다.
#[test]
fn review_findings_carry_via_on_impacted_objects() {
    let directory = tempfile::tempdir().unwrap();
    let before = directory.path().join("before.json");
    let after = directory.path().join("after.json");
    document(&before, "prod", false);
    document(&after, "prod", true);
    for path in [&before, &after] {
        edit_document(path, |doc| {
            doc["schemas"][0]["objects"]
                .as_array_mut()
                .unwrap()
                .push(json!({
                    "name": "user_names",
                    "kind": "view",
                    "body": "CREATE VIEW user_names AS SELECT name FROM users",
                    "columns": [{"name": "name", "data_type": "text", "nullable": true,
                                 "ordinal": 1, "pk_position": 0}],
                    "constraints": [],
                    "indexes": [],
                    "triggers": []
                }))
        });
    }
    let output = run(&before, &after, &[]);
    let report = json_output(&output);
    let changes = report["changes"].as_array().unwrap();
    let with_impacted: Vec<&Value> = changes
        .iter()
        .filter(|change| {
            change["impacted"]
                .as_array()
                .is_some_and(|impacted| !impacted.is_empty())
        })
        .collect();
    assert!(
        !with_impacted.is_empty(),
        "fixture must produce a finding with impacted objects: {report}"
    );
    for change in with_impacted {
        for neighbor in change["impacted"].as_array().unwrap() {
            let via = neighbor["via"]
                .as_str()
                .unwrap_or_else(|| panic!("impacted entry without via: {neighbor}"));
            assert!(!via.is_empty());
            if neighbor["distance"] == 1 {
                assert_eq!(via, change["id"].as_str().unwrap(), "{change}");
            }
        }
    }
}
