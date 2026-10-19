//! `--cache-dir`의 신뢰 경계를 실제 CLI로 검증한다.
//!
//! 캐시 hit는 parser 효과를 그대로 그래프에 복원한다. 그래서 링크로 다른 곳을 가리키는
//! 디렉터리·항목, 다른 사용자가 쓸 수 있는 디렉터리, FIFO 같은 특수 파일은 hit가 되면
//! 안 되고 분석을 멈추게 해서도 안 된다. 어느 경우든 캐시 오류는 경고와 miss이고,
//! 종료 코드와 graph.json 바이트는 캐시 없는 실행과 같다.
#![cfg(unix)]

use serde_json::json;
use std::fs;
use std::os::unix::fs::{symlink, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// 몸체 하나(`public.report`)가 `core.salaries`를 읽는 문서. 항목이 하나만 생긴다.
fn catalog(root: &Path) {
    let table = |name: &str| {
        json!({
            "name": name, "kind": "table", "constraints": [], "indexes": [], "triggers": [],
            "columns": [
                {"name": "id", "data_type": "integer", "nullable": false, "ordinal": 1, "pk_position": 1},
                {"name": "amount", "data_type": "integer", "nullable": false, "ordinal": 2, "pk_position": 0}
            ]
        })
    };
    let document = json!({
        "version": 1, "reader": "fixture", "dialect": "postgres", "limitations": [],
        "schemas": [
            {"name": "core", "objects": [table("salaries")], "routines": []},
            {"name": "public", "objects": [table("harmless")], "routines": [
                {"name": "report", "kind": "function", "language": "sql",
                 "body": "SELECT id, amount FROM core.salaries"}
            ]}
        ]
    });
    fs::write(
        root.join("catalog.json"),
        serde_json::to_vec(&document).unwrap(),
    )
    .unwrap();
}

/// 개발자 홈의 키 파일과 환경 키를 쓰지 않도록 설정 위치를 작업 디렉터리 안으로 돌린다.
fn isolated(command: &mut Command, root: &Path) {
    command
        .current_dir(root)
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env_remove("SCHEMAGRAPH_CACHE_KEY");
}

fn scan_command(root: &Path, output: &str, cache: Option<&str>) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_schemagraph"));
    isolated(&mut command, root);
    command.args(["scan", "--document", "catalog.json", "-o", output]);
    if let Some(cache) = cache {
        command.args(["--cache-dir", cache]);
    }
    command
}

struct Scan {
    graph: Vec<u8>,
    stderr: String,
}

/// 종료 0을 확인하고 graph.json 바이트와 stderr를 돌려준다.
fn run_scan(mut command: Command, root: &Path, output: &str) -> Scan {
    let result = command.output().unwrap();
    assert_eq!(result.status.code(), Some(0), "{result:?}");
    Scan {
        graph: fs::read(root.join(output)).unwrap(),
        stderr: String::from_utf8(result.stderr).unwrap(),
    }
}

fn uncached(root: &Path) -> Vec<u8> {
    run_scan(
        scan_command(root, "uncached.json", None),
        root,
        "uncached.json",
    )
    .graph
}

/// stderr 요약 줄 `schemagraph cache: hits=… misses=… writes=… warnings=…`의 네 값.
fn stats(stderr: &str) -> [u64; 4] {
    let line = stderr
        .lines()
        .find_map(|line| line.strip_prefix("schemagraph cache: "))
        .unwrap_or_else(|| panic!("no cache summary line: {stderr}"));
    let values: Vec<u64> = line
        .split(' ')
        .map(|pair| pair.split_once('=').unwrap().1.parse().unwrap())
        .collect();
    values.try_into().unwrap()
}

/// 캐시 디렉터리의 유일한 항목 경로(숨은 임시 파일은 세지 않는다).
fn only_entry(cache: &Path) -> PathBuf {
    let entries: Vec<PathBuf> = fs::read_dir(cache)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| {
            let name = path.file_name().unwrap().to_string_lossy();
            name.ends_with(".json") && !name.starts_with('.')
        })
        .collect();
    assert_eq!(entries.len(), 1, "{entries:?}");
    entries.into_iter().next().unwrap()
}

/// 막힌 scan이 테스트를 멈추지 않도록 기한 안에 끝나는지 보며 실행한다.
fn run_scan_within(mut command: Command, root: &Path, output: &str, limit: Duration) -> Scan {
    let stderr_path = root.join("scan.stderr");
    let mut child = command
        .stdout(Stdio::null())
        .stderr(fs::File::create(&stderr_path).unwrap())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + limit;
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if Instant::now() > deadline {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("scan did not finish within {limit:?}; it blocked on a cache entry");
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    assert_eq!(status.code(), Some(0));
    Scan {
        graph: fs::read(root.join(output)).unwrap(),
        stderr: fs::read_to_string(&stderr_path).unwrap(),
    }
}

#[test]
fn symlinked_cache_directory_disables_the_cache() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    catalog(root);
    let expected = uncached(root);
    fs::create_dir(root.join("real")).unwrap();
    symlink(root.join("real"), root.join("linked")).unwrap();
    // 끊어진 링크도 따라가 대상 디렉터리를 만들지 않고 같은 이유로 끈다.
    symlink(root.join("missing"), root.join("dangling")).unwrap();
    for cache in ["linked", "dangling"] {
        let scan = run_scan(
            scan_command(root, "graph.json", Some(cache)),
            root,
            "graph.json",
        );
        assert!(scan.stderr.contains("symbolic link"), "{}", scan.stderr);
        assert_eq!(stats(&scan.stderr), [0, 1, 0, 1], "{}", scan.stderr);
        assert!(scan.graph == expected, "disabled cache changed graph.json");
    }
    assert_eq!(fs::read_dir(root.join("real")).unwrap().count(), 0);
    assert!(fs::symlink_metadata(root.join("missing")).is_err());
}

#[test]
fn cache_directory_writable_by_other_users_disables_the_cache() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    catalog(root);
    let expected = uncached(root);
    for (name, mode) in [("group-writable", 0o770), ("other-writable", 0o707)] {
        fs::create_dir(root.join(name)).unwrap();
        fs::set_permissions(root.join(name), fs::Permissions::from_mode(mode)).unwrap();
        let scan = run_scan(
            scan_command(root, "graph.json", Some(name)),
            root,
            "graph.json",
        );
        assert!(
            scan.stderr.contains("writable by group or other users"),
            "{}",
            scan.stderr
        );
        assert_eq!(stats(&scan.stderr), [0, 1, 0, 1], "{}", scan.stderr);
        assert_eq!(fs::read_dir(root.join(name)).unwrap().count(), 0);
        assert!(scan.graph == expected, "disabled cache changed graph.json");
    }
}

#[test]
fn created_cache_directory_is_private_under_a_group_writable_umask() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    catalog(root);
    let scan_under_umask = || {
        let mut command = Command::new("/bin/sh");
        isolated(&mut command, root);
        command.args([
            "-c",
            "umask 002 && exec \"$0\" \"$@\"",
            env!("CARGO_BIN_EXE_schemagraph"),
            "scan",
            "--document",
            "catalog.json",
            "-o",
            "graph.json",
            "--cache-dir",
            "fresh/cache",
        ]);
        run_scan(command, root, "graph.json")
    };
    let cold = scan_under_umask();
    assert_eq!(stats(&cold.stderr), [0, 1, 1, 0], "{}", cold.stderr);
    for directory in ["fresh", "fresh/cache"] {
        let mode = fs::metadata(root.join(directory))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o700, "{directory}");
    }
    // umask 002(사용자 개인 그룹) 환경에서도 스스로 만든 디렉터리 때문에 꺼지지 않는다.
    let warm = scan_under_umask();
    assert_eq!(stats(&warm.stderr), [1, 0, 0, 0], "{}", warm.stderr);
}

#[test]
fn symlinked_entry_is_a_warning_and_miss_and_is_replaced() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    catalog(root);
    let expected = uncached(root);
    run_scan(
        scan_command(root, "cold.json", Some("cache")),
        root,
        "cold.json",
    );
    let entry = only_entry(&root.join("cache"));
    fs::create_dir(root.join("outside")).unwrap();
    let outside = root.join("outside/entry.json");
    fs::rename(&entry, &outside).unwrap();
    symlink(&outside, &entry).unwrap();
    let outside_bytes = fs::read(&outside).unwrap();

    let scan = run_scan(
        scan_command(root, "graph.json", Some("cache")),
        root,
        "graph.json",
    );
    assert_eq!(stats(&scan.stderr), [0, 1, 1, 1], "{}", scan.stderr);
    assert!(scan.graph == expected, "symlinked entry changed graph.json");
    // 다시 계산한 결과는 링크를 따라 쓰지 않고 링크 자리를 정규 파일로 바꾼다.
    assert!(fs::symlink_metadata(&entry).unwrap().file_type().is_file());
    assert_eq!(fs::read(&outside).unwrap(), outside_bytes);
}

#[test]
fn fifo_entry_does_not_block_the_scan() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    catalog(root);
    let expected = uncached(root);
    run_scan(
        scan_command(root, "cold.json", Some("cache")),
        root,
        "cold.json",
    );
    let entry = only_entry(&root.join("cache"));
    fs::remove_file(&entry).unwrap();
    assert!(Command::new("mkfifo")
        .arg(&entry)
        .status()
        .unwrap()
        .success());

    let command = scan_command(root, "graph.json", Some("cache"));
    let scan = run_scan_within(command, root, "graph.json", Duration::from_secs(60));
    assert_eq!(stats(&scan.stderr), [0, 1, 1, 1], "{}", scan.stderr);
    assert!(scan.graph == expected, "FIFO entry changed graph.json");
    assert!(fs::symlink_metadata(&entry).unwrap().file_type().is_file());
}
