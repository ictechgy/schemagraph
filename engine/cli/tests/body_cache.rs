//! `--cache-dir`를 쓰든 안 쓰든, 캐시가 비었든 찼든 같은 문서는 같은 graph.json
//! 바이트가 되는지 검증한다. graph.json의 sha256을 스냅샷 식별자로 쓰는 소비자가
//! 캐시 상태 때문에 같은 스키마를 다른 스냅샷으로 보면 안 된다.

use serde_json::{json, Value};
use std::fs;
use std::path::Path;
use std::process::Command;

/// 몸체 해석 notes가 한 몸체에서 두 건 이상, 사전순이 아닌 생성 순서로 나오는 문서.
///
/// `load`는 없는 테이블에 쓰고 다른 없는 테이블을 읽는다. parser는 쓰기 대상의
/// 누락을 읽기 대상보다 먼저 남기므로 `zz_missing` 다음에 `aa_missing`이 온다.
fn catalog(root: &Path) {
    let document = json!({
        "version": 1, "reader": "fixture", "dialect": "postgres", "limitations": [],
        "schemas": [{"name": "public", "objects": [{
            "name": "orders", "kind": "table", "constraints": [], "indexes": [], "triggers": [],
            "columns": [
                {"name": "id", "data_type": "integer", "nullable": false, "ordinal": 1, "pk_position": 1}
            ]}],
            "routines": [
                {"name": "load", "kind": "procedure", "language": "sql",
                 "body": "INSERT INTO zz_missing SELECT id FROM aa_missing"},
                {"name": "count_orders", "kind": "function", "language": "sql",
                 "body": "SELECT count(*) FROM orders"}
            ]}]
    });
    fs::write(
        root.join("catalog.json"),
        serde_json::to_vec(&document).unwrap(),
    )
    .unwrap();
}

/// scan을 실행해 graph.json 바이트와 stderr를 돌려준다.
///
/// 캐시 키 파일은 작업 디렉터리 안 설정 위치에 만들어 개발자 홈의 `~/.config`와 환경 키를
/// 쓰지 않는다. cold와 warm이 같은 키 파일을 써야 warm이 hit다.
fn scan(root: &Path, output: &str, cache: Option<&str>) -> (Vec<u8>, String) {
    let mut command = Command::new(env!("CARGO_BIN_EXE_schemagraph"));
    command
        .current_dir(root)
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env_remove("SCHEMAGRAPH_CACHE_KEY")
        .args(["scan", "--document", "catalog.json", "-o", output]);
    if let Some(cache) = cache {
        command.args(["--cache-dir", cache]);
    }
    let result = command.output().unwrap();
    assert_eq!(result.status.code(), Some(0), "{result:?}");
    let stderr = String::from_utf8(result.stderr).unwrap();
    (fs::read(root.join(output)).unwrap(), stderr)
}

#[test]
fn cold_and_warm_cache_scans_write_the_uncached_graph_bytes() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    catalog(root);
    let (uncached, _) = scan(root, "uncached.json", None);
    // 전제: 한 몸체의 limitation이 사전순이 아닌 순서로 나와야 순서를 바꾸는 저장이 드러난다.
    let graph: Value = serde_json::from_slice(&uncached).unwrap();
    let missing: Vec<&str> = graph["limitations"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(Value::as_str)
        .filter(|limitation| limitation.contains("_missing"))
        .collect();
    assert_eq!(missing.len(), 2, "{graph}");
    assert!(missing[0].contains("zz_missing") && missing[1].contains("aa_missing"));

    let (cold, cold_stderr) = scan(root, "cold.json", Some("cache"));
    let (warm, warm_stderr) = scan(root, "warm.json", Some("cache"));
    assert!(
        cold_stderr.contains("hits=0 misses=2 writes=2 warnings=0"),
        "{cold_stderr}"
    );
    assert!(
        warm_stderr.contains("hits=2 misses=0 writes=0 warnings=0"),
        "{warm_stderr}"
    );
    assert!(cold == uncached, "cold cache scan changed graph.json bytes");
    assert!(warm == uncached, "warm cache scan changed graph.json bytes");
}
