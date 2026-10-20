//! `--cache-dir`의 신뢰 경계를 실제 CLI로 검증한다.
//!
//! 캐시 hit는 parser 효과를 그대로 그래프에 복원한다. 그래서 링크로 다른 곳을 가리키는
//! 디렉터리·항목, 다른 사용자가 쓸 수 있는 디렉터리, FIFO 같은 특수 파일은 hit가 되면
//! 안 되고 분석을 멈추게 해서도 안 된다. 항목은 사용자별 비밀 키의 HMAC-SHA256 태그로
//! 인증하므로, 공개 정보만으로 다시 계산한 위조 항목과 다른 키로 쓴 항목도 miss다.
//! 어느 경우든 캐시 오류는 경고와 miss이고, 종료 코드와 graph.json 바이트는 캐시 없는
//! 실행과 같다.
#![cfg(unix)]

use serde_json::json;
use sha2::{Digest, Sha256};
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
    // POSIX는 끝이 `/`나 `/.`인 경로의 마지막 링크를 따라간다(셸 탭 완성이 `/`를 붙인다).
    // 그런 표기도 링크 자체로 보고 거절해야 한다. macOS의 `mkdir("dangling/")`은 대상을 만든다.
    for cache in [
        "linked",
        "linked/",
        "linked/.",
        "linked//",
        "dangling",
        "dangling/",
        "dangling/.",
    ] {
        let scan = run_scan(
            scan_command(root, "graph.json", Some(cache)),
            root,
            "graph.json",
        );
        assert!(
            scan.stderr.contains("symbolic link"),
            "{cache}: {}",
            scan.stderr
        );
        assert_eq!(
            stats(&scan.stderr),
            [0, 1, 0, 1],
            "{cache}: {}",
            scan.stderr
        );
        assert!(
            scan.graph == expected,
            "{cache}: disabled cache changed graph.json"
        );
        assert_eq!(
            fs::read_dir(root.join("real")).unwrap().count(),
            0,
            "{cache}"
        );
        assert!(
            fs::symlink_metadata(root.join("missing")).is_err(),
            "{cache}"
        );
    }
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

/// 테스트 전용 가짜 키(32바이트 이상). 실제 비밀값이 아니다.
const FIRST_KEY: &str = "PLACEHOLDER-cache-key-for-tests-first";
const SECOND_KEY: &str = "PLACEHOLDER-cache-key-for-tests-second";

fn key_file(root: &Path) -> PathBuf {
    root.join("config/schemagraph/cache.key")
}

fn mode(path: &Path) -> u32 {
    fs::symlink_metadata(path).unwrap().permissions().mode() & 0o777
}

/// 공개 정보만으로 만든 위조: payload의 `core.salaries`를 `public.harmless`로 바꾸고 두 번째
/// 필드(형식 3의 `checksum`, 형식 4의 `tag`)를 serde 압축 직렬화의 SHA-256으로 다시 쓴다.
/// 같은 릴리스 바이너리와 문서를 가진 사람은 이 값을 계산할 수 있지만 사용자별 키는 모른다.
fn forge_with_recomputed_checksum(entry: &Path) {
    let raw = fs::read(entry).unwrap();
    let marker = b",\"payload\":";
    let index = raw
        .windows(marker.len())
        .position(|window| window == marker)
        .unwrap();
    let payload = String::from_utf8(raw[index + marker.len()..raw.len() - 1].to_vec()).unwrap();
    let payload = payload.replace("core.salaries", "public.harmless");
    let checksum = format!("{:x}", Sha256::digest(payload.as_bytes()));
    // 머리는 `{"format":N,"<필드>":"<64 hex>"`다. 따옴표로 감싼 hex 66바이트를 바꾼다.
    let header = String::from_utf8(raw[..index - 66].to_vec()).unwrap();
    fs::write(
        entry,
        format!("{header}\"{checksum}\",\"payload\":{payload}}}"),
    )
    .unwrap();
}

#[test]
fn forged_entry_with_a_recomputed_checksum_is_recomputed_and_replaced() {
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
    forge_with_recomputed_checksum(&entry);

    let forged = run_scan(
        scan_command(root, "graph.json", Some("cache")),
        root,
        "graph.json",
    );
    assert_eq!(stats(&forged.stderr), [0, 1, 1, 1], "{}", forged.stderr);
    assert!(
        forged.stderr.contains("invalid authentication tag"),
        "{}",
        forged.stderr
    );
    assert!(forged.graph == expected, "forged entry changed graph.json");
    // 다시 계산한 결과가 위조 항목을 덮어써 다음 실행은 hit다.
    let warm = run_scan(
        scan_command(root, "warm.json", Some("cache")),
        root,
        "warm.json",
    );
    assert_eq!(stats(&warm.stderr), [1, 0, 0, 0], "{}", warm.stderr);
    assert!(warm.graph == expected);
}

#[test]
fn environment_key_is_reused_and_entries_from_another_key_miss() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    catalog(root);
    let expected = uncached(root);
    let scan_with_key = |output: &str, key: &str| {
        let mut command = scan_command(root, output, Some("cache"));
        command.env("SCHEMAGRAPH_CACHE_KEY", key);
        run_scan(command, root, output)
    };
    let cold = scan_with_key("cold.json", FIRST_KEY);
    assert_eq!(stats(&cold.stderr), [0, 1, 1, 0], "{}", cold.stderr);
    let warm = scan_with_key("warm.json", FIRST_KEY);
    assert_eq!(stats(&warm.stderr), [1, 0, 0, 0], "{}", warm.stderr);

    let other = scan_with_key("other.json", SECOND_KEY);
    assert_eq!(stats(&other.stderr), [0, 1, 1, 1], "{}", other.stderr);
    assert!(
        other.stderr.contains("invalid authentication tag"),
        "{}",
        other.stderr
    );
    assert!(
        other.graph == expected,
        "foreign-key entry changed graph.json"
    );
    assert!(!other.stderr.contains(FIRST_KEY) && !other.stderr.contains(SECOND_KEY));
    // 환경 키를 쓰면 키 파일을 만들지 않는다.
    assert!(fs::symlink_metadata(root.join("config")).is_err());
}

#[test]
fn missing_key_file_is_created_private_and_reused() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    catalog(root);
    let cold = run_scan(
        scan_command(root, "cold.json", Some("cache")),
        root,
        "cold.json",
    );
    assert_eq!(stats(&cold.stderr), [0, 1, 1, 0], "{}", cold.stderr);
    let key = key_file(root);
    assert!(fs::symlink_metadata(&key).unwrap().file_type().is_file());
    assert_eq!((mode(&key), fs::metadata(&key).unwrap().len()), (0o600, 32));
    assert_eq!(mode(key.parent().unwrap()), 0o700);
    let bytes = fs::read(&key).unwrap();

    let warm = run_scan(
        scan_command(root, "warm.json", Some("cache")),
        root,
        "warm.json",
    );
    assert_eq!(stats(&warm.stderr), [1, 0, 0, 0], "{}", warm.stderr);
    assert_eq!(fs::read(&key).unwrap(), bytes, "the key file was replaced");
    // 공개에 쓴 임시 파일이 남지 않는다.
    assert_eq!(fs::read_dir(key.parent().unwrap()).unwrap().count(), 1);
}

#[test]
fn unsafe_key_files_disable_the_cache_and_are_left_alone() {
    let readable: fn(&Path) = |key| {
        fs::write(key, [7u8; 32]).unwrap();
        fs::set_permissions(key, fs::Permissions::from_mode(0o644)).unwrap();
    };
    let linked: fn(&Path) = |key| {
        let target = key.with_file_name("real.key");
        fs::write(&target, [7u8; 32]).unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o600)).unwrap();
        symlink(&target, key).unwrap();
    };
    let short: fn(&Path) = |key| {
        fs::write(key, [7u8; 16]).unwrap();
        fs::set_permissions(key, fs::Permissions::from_mode(0o600)).unwrap();
    };
    // 상한(4096바이트)을 넘는 정규 파일은 링크·특수 파일이 아니라 길이 문제로 안내한다.
    let oversized: fn(&Path) = |key| {
        fs::write(key, [7u8; 5000]).unwrap();
        fs::set_permissions(key, fs::Permissions::from_mode(0o600)).unwrap();
    };
    for (prepare, reason) in [
        (readable, "accessible by group or other users"),
        (linked, "symbolic link"),
        (short, "fewer than 32 bytes"),
        (
            oversized,
            "holds more than 4096 bytes; use a key of 32 to 4096 bytes",
        ),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        catalog(root);
        let expected = uncached(root);
        let key = key_file(root);
        fs::create_dir_all(key.parent().unwrap()).unwrap();
        prepare(&key);
        let before = fs::read(&key).unwrap();

        let scan = run_scan(
            scan_command(root, "graph.json", Some("cache")),
            root,
            "graph.json",
        );
        assert!(scan.stderr.contains(reason), "{reason}: {}", scan.stderr);
        assert_eq!(stats(&scan.stderr), [0, 1, 0, 1], "{}", scan.stderr);
        assert!(scan.graph == expected, "disabled cache changed graph.json");
        assert_eq!(
            fs::read(&key).unwrap(),
            before,
            "{reason}: key file changed"
        );
        assert_eq!(fs::read_dir(root.join("cache")).unwrap().count(), 0);
    }
}

#[test]
fn short_environment_key_or_missing_home_disables_the_cache() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    catalog(root);
    let expected = uncached(root);
    let mut short = scan_command(root, "short.json", Some("cache"));
    short.env("SCHEMAGRAPH_CACHE_KEY", "PLACEHOLDER-short");
    let mut homeless = scan_command(root, "homeless.json", Some("cache"));
    homeless.env_remove("HOME").env_remove("XDG_CONFIG_HOME");
    for (command, output, reason) in [
        (short, "short.json", "shorter than 32 bytes"),
        (homeless, "homeless.json", "SCHEMAGRAPH_CACHE_KEY"),
    ] {
        let scan = run_scan(command, root, output);
        assert!(scan.stderr.contains(reason), "{reason}: {}", scan.stderr);
        assert_eq!(stats(&scan.stderr), [0, 1, 0, 1], "{}", scan.stderr);
        assert!(scan.graph == expected, "disabled cache changed graph.json");
    }
    assert!(fs::symlink_metadata(root.join("config")).is_err());
}

#[test]
fn empty_environment_key_and_relative_config_home_fall_back_to_the_home_key_file() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    catalog(root);
    // Actions는 없는 secret을 빈 문자열로 넘긴다. 상대 XDG_CONFIG_HOME은 XDG 규약대로 무시한다.
    let mut command = scan_command(root, "graph.json", Some("cache"));
    command
        .env("SCHEMAGRAPH_CACHE_KEY", "")
        .env("XDG_CONFIG_HOME", "relative-config")
        .env("HOME", root.join("home"));
    let scan = run_scan(command, root, "graph.json");
    assert_eq!(stats(&scan.stderr), [0, 1, 1, 0], "{}", scan.stderr);
    let key = root.join("home/.config/schemagraph/cache.key");
    assert_eq!(mode(&key), 0o600);
    assert!(fs::symlink_metadata(root.join("relative-config")).is_err());
}
