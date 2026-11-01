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

/// 트리거가 부르는 함수가 시그니처만 다른 오버로드 둘 중 하나를 부르는 문서.
///
/// 모호한 호출은 확정 간선이 없고 진단의 후보로만 남는다. 캐시 hit가 그 후보를
/// 잃으면 warm 스캔의 `dead`가 쓰이는 오버로드를 다시 후보로 낸다.
fn ambiguous_catalog(root: &Path) {
    let document = json!({
        "version": 1, "reader": "fixture", "dialect": "postgres", "limitations": [],
        "schemas": [{"name": "public", "objects": [{
            "name": "orders", "kind": "table", "constraints": [], "indexes": [],
            "triggers": [{"name": "trg_touch",
                "body": "CREATE TRIGGER trg_touch AFTER INSERT ON orders EXECUTE FUNCTION touch()"}],
            "columns": [
                {"name": "id", "data_type": "integer", "nullable": false, "ordinal": 1, "pk_position": 1}
            ]}],
            "routines": [
                {"name": "touch", "kind": "function", "language": "sql", "body": "SELECT over(1)"},
                {"name": "over", "kind": "function", "language": "sql", "signature": "integer", "body": "SELECT 1"},
                {"name": "over", "kind": "function", "language": "sql", "signature": "text", "body": "SELECT 2"}
            ]}]
    });
    fs::write(
        root.join("catalog.json"),
        serde_json::to_vec(&document).unwrap(),
    )
    .unwrap();
}

#[test]
fn warm_cache_scans_keep_ambiguous_call_candidates_for_dead() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    ambiguous_catalog(root);
    let (uncached, _) = scan(root, "uncached.json", None);
    let graph: Value = serde_json::from_slice(&uncached).unwrap();
    let touch = graph["analysis"]
        .as_array()
        .unwrap()
        .iter()
        .find(|analysis| analysis["id"] == "public.touch")
        .unwrap_or_else(|| panic!("{graph}"));
    assert_eq!(
        touch["diagnostics"],
        json!([{
            "candidates": ["public.over(integer)", "public.over(text)"],
            "code": "SG_BODY_PARTIAL",
            "message": "public.touch calls public.over, which has 2 overloads; edge omitted"
        }]),
        "{graph}"
    );

    let (cold, _) = scan(root, "cold.json", Some("cache"));
    let (warm, warm_stderr) = scan(root, "warm.json", Some("cache"));
    assert!(warm_stderr.contains("misses=0"), "{warm_stderr}");
    assert!(cold == uncached, "cold cache scan changed graph.json bytes");
    assert!(warm == uncached, "warm cache scan changed graph.json bytes");

    let dead = Command::new(env!("CARGO_BIN_EXE_schemagraph"))
        .current_dir(root)
        .args(["dead", "-g", "warm.json"])
        .output()
        .unwrap();
    assert_eq!(dead.status.code(), Some(0), "{dead:?}");
    let report: Value = serde_json::from_slice(&dead.stdout).unwrap();
    // 트리거가 부르는 touch가 살아 있으니 그 몸체가 부를 수 있는 오버로드도 후보가 아니다.
    assert_eq!(report["candidates"], json!([]), "{report}");
    assert!(
        report["limitations"]
            .as_array()
            .unwrap()
            .iter()
            .any(|l| l == "routines named by ambiguous overloaded calls: 2; each is kept while a live caller may reach it"),
        "{report}"
    );
}

/// SQL Server 뷰·프로시저가 `orders`를 부르는 문서. `with_variant`면 대소문자만 다른
/// `dbo.Orders`가 생긴다.
fn sqlserver_catalog(root: &Path, with_variant: bool) {
    let column =
        json!({"name": "Id", "data_type": "int", "nullable": true, "ordinal": 1, "pk_position": 0});
    let table = |name: &str| {
        json!({"name": name, "kind": "table", "constraints": [], "indexes": [], "triggers": [],
               "columns": [column.clone()]})
    };
    let mut objects = vec![table("customers")];
    if with_variant {
        objects.push(table("Orders"));
    }
    objects.push(
        json!({"name": "v", "kind": "view", "constraints": [], "indexes": [],
        "triggers": [], "columns": [column.clone()], "body": "SELECT Id FROM dbo.orders"}),
    );
    let document = json!({
        "version": 1, "reader": "fixture", "dialect": "sqlserver", "limitations": [],
        "schemas": [{"name": "dbo", "objects": objects, "routines": [
            {"name": "p", "kind": "procedure", "language": "sql", "body": "SELECT Id FROM orders"}
        ]}]
    });
    fs::write(
        root.join("catalog.json"),
        serde_json::to_vec(&document).unwrap(),
    )
    .unwrap();
}

#[test]
fn warm_cache_follows_sqlserver_case_variants_that_appear_or_disappear() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    // 대소문자 변형이 없을 때 저장한 결과를, 변형이 생긴 뒤와 다시 사라진 뒤에 재사용하면
    // 누락 이유의 대소문자 힌트가 실제 카탈로그와 어긋난다.
    for with_variant in [false, true, false] {
        sqlserver_catalog(root, with_variant);
        let (uncached, _) = scan(root, "uncached.json", None);
        let (warm, _) = scan(root, "warm.json", Some("cache"));
        assert!(
            warm == uncached,
            "warm cache scan changed graph.json bytes (variant: {with_variant})\n{}\n{}",
            String::from_utf8_lossy(&uncached),
            String::from_utf8_lossy(&warm)
        );
        let graph: Value = serde_json::from_slice(&uncached).unwrap();
        let hinted = graph["limitations"].as_array().unwrap().iter().any(|note| {
            note.as_str()
                .unwrap()
                .contains("dbo.Orders differs only by letter case")
        });
        assert_eq!(hinted, with_variant, "{graph}");
    }
}
