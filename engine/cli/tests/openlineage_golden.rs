//! 단일 그래프의 `openlineage` 출력을 바이트 단위로 고정한다.
//!
//! 골든은 합친 그래프 데이터셋 이름을 바꾸기 전 main(0.8.1)에서 캡처했다. 입력은
//! `verify-openlineage.py`의 DDL과 같은 모양이다 — 조인·필터 뷰, 단일·다중 출력 DML 작업,
//! 동적 SQL 때문에 분석이 부분적인 작업. runId는 이벤트 내용의 해시라 이벤트 어디가 바뀌어도
//! 함께 바뀐다.

use std::fs;
use std::path::Path;
use std::process::Command;

/// 골든을 캡처한 버전. 이 버전이면 골든 바이트를 그대로 비교한다.
const GOLDEN_VERSION: &str = "0.8.1";

fn golden(name: &str) -> String {
    fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/golden")
            .join(name),
    )
    .unwrap()
}

/// 각 이벤트의 runId를 CLI와 같은 규칙(runId를 뺀 이벤트 JSON의 SHA-256, UUID 버전 8)으로
/// 다시 계산한다.
fn recompute_run_ids(ndjson: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut out = String::new();
    for line in ndjson.lines() {
        let mut event: serde_json::Value = serde_json::from_str(line).unwrap();
        event.as_object_mut().unwrap().remove("run");
        let digest = Sha256::digest(event.to_string().as_bytes());
        let mut bytes = [0u8; 16];
        bytes.copy_from_slice(&digest[..16]);
        bytes[6] = (bytes[6] & 0x0f) | 0x80;
        bytes[8] = (bytes[8] & 0x3f) | 0x80;
        let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
        let run_id = format!(
            "{}-{}-{}-{}-{}",
            &hex[0..8],
            &hex[8..12],
            &hex[12..16],
            &hex[16..20],
            &hex[20..32]
        );
        event["run"] = serde_json::json!({"runId": run_id});
        out.push_str(&event.to_string());
        out.push('\n');
    }
    out
}

/// 버전이 바뀌면 producer URL의 버전만 바꾸고 runId를 다시 계산한다. 그 밖의 바이트는
/// 골든 그대로다. 골든 버전이면 골든을 그대로 돌려준다.
fn for_version(ndjson: &str, version: &str) -> String {
    if version == GOLDEN_VERSION {
        return ndjson.to_owned();
    }
    let renamed = ndjson
        .replace(
            &format!("/tree/v{GOLDEN_VERSION}\""),
            &format!("/tree/v{version}\""),
        )
        .replace(
            &format!("/blob/v{GOLDEN_VERSION}/"),
            &format!("/blob/v{version}/"),
        );
    recompute_run_ids(&renamed)
}

#[test]
fn single_graph_openlineage_bytes_match_the_golden_export() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let catalog =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden/openlineage-single.catalog.json");
    let run = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_schemagraph"))
            .current_dir(root)
            .args(args)
            .output()
            .unwrap()
    };
    let scan = run(&[
        "scan",
        "--document",
        catalog.to_str().unwrap(),
        "-o",
        "graph.json",
    ]);
    assert_eq!(scan.status.code(), Some(0), "{scan:?}");
    let export = run(&[
        "openlineage",
        "--graph",
        "graph.json",
        "--namespace",
        "postgres://verify.invalid:5432",
        "--database",
        "corpus",
        "--event-time",
        "2026-09-25T00:00:00Z",
    ]);
    assert_eq!(export.status.code(), Some(0), "{export:?}");
    let expected = for_version(
        &golden("openlineage-single.ndjson"),
        env!("CARGO_PKG_VERSION"),
    );
    assert_eq!(String::from_utf8(export.stdout).unwrap(), expected);
    assert_eq!(
        String::from_utf8(export.stderr).unwrap(),
        golden("openlineage-single.stderr")
    );
}

/// 다시 계산한 runId가 캡처한 runId와 같다 — 버전 보정이 CLI와 같은 규칙을 쓴다는 뜻이다.
#[test]
fn recomputed_run_ids_reproduce_the_captured_golden() {
    let ndjson = golden("openlineage-single.ndjson");
    assert_eq!(recompute_run_ids(&ndjson), ndjson);
    let other = for_version(&ndjson, "9.9.9");
    assert!(other.contains("/tree/v9.9.9\"") && !other.contains("/tree/v0.8.1\""));
    assert_ne!(other, ndjson);
}
