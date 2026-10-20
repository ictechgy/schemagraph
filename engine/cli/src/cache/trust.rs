//! `--cache-dir`의 신뢰 경계: 디렉터리 검사와 항목 인증 키.
//!
//! 캐시 hit는 parser 효과를 다시 해석하지 않고 그래프에 복원한다. 그래서 캐시 디렉터리는
//! 현재 사용자만 쓸 수 있는 진짜 디렉터리여야 하고, 항목은 캐시 밖에 둔 사용자별 비밀
//! 키로 HMAC-SHA256 태그를 단다. 실행 파일 해시·문서처럼 공개된 재료만으로는 유효한
//! 항목을 만들 수 없게 하기 위해서다. 둘 중 하나라도 세우지 못하면 "캐시 오류는 miss"라는
//! 계약대로 경고하고 캐시를 끈다. 분석 결과와 종료 코드는 캐시 없는 실행과 같다.

use std::ffi::OsString;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use hmac::{Hmac, Mac};
use sha2::Sha256;

/// 항목 태그 계산기. 키를 넣은 상태를 항목마다 복제해 쓴다.
pub(super) type HmacSha256 = Hmac<Sha256>;

/// 항목 태그 키를 넣은 HMAC-SHA256 상태. 키 바이트는 이 상태를 만든 뒤 남기지 않는다.
///
/// `Debug`를 구현하지 않아 키가 로그나 오류 문구에 실리지 않는다.
pub(super) struct CacheKey(HmacSha256);

impl CacheKey {
    /// HMAC은 어떤 길이의 키도 받으므로 실패는 라이브러리 계약 위반뿐이다. 그래도 unwrap
    /// 대신 캐시를 끄는 이유로 돌려준다.
    #[cfg_attr(not(unix), allow(dead_code))]
    pub(super) fn new(bytes: &[u8]) -> Result<Self, String> {
        HmacSha256::new_from_slice(bytes)
            .map(Self)
            .map_err(|error| format!("the cache key was rejected by HMAC-SHA256: {error}"))
    }

    /// 항목 하나의 태그를 계산하거나 확인할 새 상태.
    pub(super) fn mac(&self) -> HmacSha256 {
        self.0.clone()
    }
}

/// 키를 고르는 데 쓰는 환경 값. 테스트가 프로세스 환경을 바꾸지 않고 경우를 고정한다.
#[cfg_attr(not(unix), allow(dead_code))]
pub(super) struct KeyEnvironment {
    cache_key: Option<OsString>,
    config_home: Option<OsString>,
    home: Option<OsString>,
}

impl KeyEnvironment {
    pub(super) fn from_process() -> Self {
        Self {
            cache_key: std::env::var_os("SCHEMAGRAPH_CACHE_KEY"),
            config_home: std::env::var_os("XDG_CONFIG_HOME"),
            home: std::env::var_os("HOME"),
        }
    }
}

/// `--cache-dir` 경로를 성분 단위로 다시 모은다. 끝의 `/`, `.` 성분, 겹친 `/`가 빠진다.
///
/// POSIX는 `link/`·`link/.`처럼 끝이 `/`나 `.`인 경로를 마지막 성분의 링크까지 따라가
/// 해석한다. 그대로 검사하면 `lstat`이 링크 대신 대상 디렉터리를 보고, macOS의 `mkdir`은
/// 끊어진 링크의 대상 디렉터리까지 만든다. 셸 탭 완성이 끝 `/`를 붙이므로 흔한 표기다. 다시
/// 모으면 마지막 성분이 사용자가 이름 붙인 항목 자체가 되어 링크 검사가 그 항목을 본다. 앞의
/// `.`와 `..`는 지우면 경로 뜻이 바뀌므로 `Path::components`가 그대로 둔다.
pub(super) fn normalized_directory(directory: &Path) -> PathBuf {
    directory.components().collect()
}

/// 캐시 디렉터리를 준비하고, 신뢰할 수 없으면 캐시를 끄는 이유(영어 문장)를 돌려준다.
///
/// 검사는 [`normalized_directory`]로 끝 `/`·`.`를 지운 경로에 한다. 호출자도 같은 경로 아래에
/// 항목을 둔다. 디렉터리가 없으면 0700으로 만든다. umask는 권한 비트를 빼기만 하므로
/// 기본값(0777)으로 만들면 umask 002(사용자 개인 그룹) 환경에서 group 쓰기가 생겨 스스로
/// 캐시를 끄게 된다. 링크는 마지막 경로 성분만 본다. macOS의 `/tmp`·`/var`(그리고 `$TMPDIR`)는
/// 상위 성분이 링크라서 상위까지 거절하면 모든 임시 경로가 막힌다. 끊어진 링크도 따라가
/// 만들지 않는다. 만들기 자체의 I/O 실패(권한 거부, 같은 이름의 파일)는 기존처럼 오류(종료
/// 2)로 둔다.
#[cfg(unix)]
pub(super) fn prepare_directory(directory: &Path) -> io::Result<Option<String>> {
    use std::os::unix::fs::MetadataExt;

    let directory = &normalized_directory(directory);
    if is_symlink(directory)? {
        return Ok(Some(untrusted_directory(directory, SYMLINK_DIRECTORY)));
    }
    private_directory(directory)?;
    // 만든 뒤 다시 본다. 그 사이 링크로 바뀐 경로도 신뢰하지 않는다.
    let metadata = fs::symlink_metadata(directory)?;
    let is_link = metadata.file_type().is_symlink();
    let problem = directory_problem(is_link, metadata.uid(), metadata.mode(), current_uid());
    Ok(problem.map(|problem| untrusted_directory(directory, problem)))
}

/// 소유자와 권한 비트로 신뢰 여부를 판정할 수 없는 빌드에서는 캐시를 쓰지 않는다.
#[cfg(not(unix))]
pub(super) fn prepare_directory(directory: &Path) -> io::Result<Option<String>> {
    fs::create_dir_all(directory)?;
    Ok(Some(UNSUPPORTED_PLATFORM.into()))
}

/// 항목 태그 키를 정한다. Err는 캐시를 끄는 이유(영어 문장)이며 키 값은 담지 않는다.
///
/// `SCHEMAGRAPH_CACHE_KEY`가 있으면 그 원시 바이트를 쓴다. CI에서 actions/cache로 옮긴
/// 캐시를 재사용하는 경로다. 없으면 `${XDG_CONFIG_HOME:-$HOME/.config}/schemagraph/cache.key`를
/// 쓰고, 그 파일이 없으면 새 난수 키로 만든다. 일회성 러너에서는 실행마다 새 키가 생겨
/// 옮겨 온 캐시를 믿지 않는다(안전한 기본). 짧은 키, 위치를 정할 수 없는 키 파일(HOME 없음),
/// 다른 사용자가 읽을 수 있거나 링크인 키 파일은 경고하고 캐시를 끈다. 사용 오류(원칙 4)로
/// 끝내지 않는 이유는 결정 문서가 "키를 얻지 못하면 경고하고 캐시를 끈다"이고, 캐시 오류는
/// 분석을 바꾸지 않는 miss라는 계약이 있기 때문이다.
#[cfg(unix)]
pub(super) fn resolve_key(environment: &KeyEnvironment) -> Result<CacheKey, String> {
    // Actions는 저장소 밖 PR처럼 받을 수 없는 secret을 빈 문자열로 넘긴다. 빈 값은 설정하지
    // 않은 것으로 보고 키 파일로 넘어간다. 위조 방어는 같고 재사용만 하지 않는다.
    match environment
        .cache_key
        .as_deref()
        .filter(|value| !value.is_empty())
    {
        Some(value) => environment_key(value),
        None => file_key(&key_directory(environment)?),
    }
}

/// 디렉터리를 믿을 수 없는 빌드에서는 키를 찾기 전에 이미 캐시가 꺼진다.
#[cfg(not(unix))]
pub(super) fn resolve_key(_environment: &KeyEnvironment) -> Result<CacheKey, String> {
    Err(UNSUPPORTED_PLATFORM.into())
}

#[cfg(not(unix))]
const UNSUPPORTED_PLATFORM: &str =
    "the SQL analysis cache needs Unix ownership checks and no-follow reads; it is disabled on this platform";

/// 키 최소 길이. HMAC-SHA256 출력(32바이트)보다 짧은 키는 태그보다 추측 공간이 좁다.
#[cfg(unix)]
const MIN_KEY_BYTES: usize = 32;

/// 키 파일 읽기 상한. 만드는 키는 32바이트다. 사용자가 직접 둔 더 긴 키도 받되, 실수로
/// 가리킨 큰 파일을 메모리로 읽지 않는다. 넘으면 이 상한을 담은 경고로 캐시를 끈다
/// (ANALYSIS.md에 32~4096바이트로 적혀 있다).
#[cfg(unix)]
const MAX_KEY_FILE_BYTES: u64 = 4096;

#[cfg(unix)]
const KEY_FILE_NAME: &str = "cache.key";

#[cfg(unix)]
const SYMLINK_DIRECTORY: &str =
    "is a symbolic link; pass a real directory owned by the current user, outside checked-out code";

#[cfg(unix)]
fn untrusted_directory(directory: &Path, problem: &str) -> String {
    format!("cache directory '{}' {problem}", directory.display())
}

#[cfg(unix)]
fn is_symlink(path: &Path) -> io::Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => Ok(metadata.file_type().is_symlink()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

/// 없는 상위 디렉터리까지 0700으로 만든다. 이미 있으면 권한을 바꾸지 않는다.
#[cfg(unix)]
fn private_directory(directory: &Path) -> io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;

    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(directory)
}

#[cfg(unix)]
fn current_uid() -> u32 {
    rustix::process::geteuid().as_raw()
}

/// 링크, 다른 소유자, group·other 쓰기 비트를 거절한다. 읽기 비트는 막지 않는다(항목에는
/// SQL 원문이 없고, 막아야 하는 것은 열람이 아니라 변조다). 순수 함수로 두어 테스트가
/// 루트 권한 없이 다른 소유자 경우도 고정한다.
#[cfg(unix)]
fn directory_problem(is_link: bool, owner: u32, mode: u32, current: u32) -> Option<&'static str> {
    if is_link {
        return Some(SYMLINK_DIRECTORY);
    }
    if owner != current {
        return Some("is owned by another user; use a private directory owned by the current user");
    }
    if mode & 0o022 != 0 {
        return Some(
            "is writable by group or other users; run `chmod go-w` on it or use a private directory",
        );
    }
    None
}

#[cfg(unix)]
fn environment_key(value: &std::ffi::OsStr) -> Result<CacheKey, String> {
    use std::os::unix::ffi::OsStrExt;

    if value.as_bytes().len() < MIN_KEY_BYTES {
        return Err(format!(
            "SCHEMAGRAPH_CACHE_KEY is shorter than {MIN_KEY_BYTES} bytes; set it to a random value such as the output of `openssl rand -hex 32`"
        ));
    }
    CacheKey::new(value.as_bytes())
}

/// `${XDG_CONFIG_HOME:-$HOME/.config}/schemagraph`. 비었거나 상대 경로인 값은 XDG 규약대로
/// 없는 것으로 본다. 상대 경로를 따르면 작업 디렉터리(체크아웃) 안 파일이 키가 될 수 있다.
#[cfg(unix)]
fn key_directory(environment: &KeyEnvironment) -> Result<std::path::PathBuf, String> {
    let absolute = |value: &Option<OsString>| {
        value
            .as_deref()
            .map(std::path::PathBuf::from)
            .filter(|path| path.is_absolute())
    };
    absolute(&environment.config_home)
        .or_else(|| absolute(&environment.home).map(|home| home.join(".config")))
        .map(|config| config.join("schemagraph"))
        .ok_or_else(|| {
            "no cache key: set SCHEMAGRAPH_CACHE_KEY, or set HOME so that a private key file can be created".into()
        })
}

/// 키 파일을 읽고, 없으면 새 난수 키로 만든다.
#[cfg(unix)]
fn file_key(directory: &Path) -> Result<CacheKey, String> {
    let path = directory.join(KEY_FILE_NAME);
    match read_key_file(&path)? {
        Some(key) => Ok(key),
        None => create_key_file(directory, &path),
    }
}

/// 키 파일을 링크를 따르지 않고 읽고, 같은 fd의 fstat으로 현재 사용자 전용인지 본다.
/// `Ok(None)`은 파일이 없다는 뜻이다. 상위 디렉터리(`~/.config`)는 dotfiles 관리로 링크인
/// 경우가 흔해 결정대로 파일 자체만 본다.
#[cfg(unix)]
fn read_key_file(path: &Path) -> Result<Option<CacheKey>, String> {
    use std::os::unix::fs::MetadataExt;

    let read = schemagraph_source::imports::read_bounded_regular_file(path, MAX_KEY_FILE_BYTES);
    let (bytes, metadata) = match read {
        Ok(read) => read,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(unreadable_key_file(path, &error)),
    };
    match key_file_problem(metadata.uid(), metadata.mode(), bytes.len(), current_uid()) {
        Some(problem) => Err(format!("cache key file '{}' {problem}", path.display())),
        None => CacheKey::new(&bytes).map(Some),
    }
}

/// 키 파일을 열거나 읽지 못한 이유를 해결 방향과 함께 만든다.
///
/// 상한 초과는 `read_bounded_regular_file`이 `InvalidData`로 알린다. 링크·특수 파일과는
/// 해결 방향(키를 줄이거나 파일을 지운다)이 달라 따로 안내한다. 링크·특수 파일이라고 하면
/// 사용자가 직접 둔 긴 키가 왜 거절되는지 알 수 없다.
#[cfg(unix)]
fn unreadable_key_file(path: &Path, error: &io::Error) -> String {
    let path = path.display();
    if error.kind() == io::ErrorKind::InvalidData {
        return format!(
            "cache key file '{path}' holds more than {MAX_KEY_FILE_BYTES} bytes; use a key of {MIN_KEY_BYTES} to {MAX_KEY_FILE_BYTES} bytes, or remove the file so that a new key is created"
        );
    }
    format!(
        "cannot use cache key file '{path}'; it must be a regular file, not a symbolic link: {error}"
    )
}

/// 같은 fd에서 본 키 파일이 현재 사용자 전용이고 충분히 긴지 판정한다. 순수 함수로 두어
/// 다른 소유자 경우도 테스트한다.
#[cfg(unix)]
fn key_file_problem(owner: u32, mode: u32, length: usize, current: u32) -> Option<&'static str> {
    if owner != current {
        return Some("is owned by another user; remove it so that a new key is created");
    }
    if mode & 0o077 != 0 {
        return Some("is accessible by group or other users; run `chmod 600` on it");
    }
    if length < MIN_KEY_BYTES {
        return Some("holds fewer than 32 bytes; remove it so that a new key is created");
    }
    None
}

/// 32바이트 난수 키를 같은 디렉터리의 임시 파일(0600)에 다 쓴 뒤 hard link로 공개한다.
///
/// hard link는 이미 있는 이름을 덮어쓰지 않는다. 동시에 시작한 두 실행이 서로의 반쯤 쓴
/// 키를 읽거나 서로의 키를 바꿔치지 않고, 진 쪽은 이긴 쪽의 키를 읽는다.
#[cfg(unix)]
fn create_key_file(directory: &Path, path: &Path) -> Result<CacheKey, String> {
    let failed = |error: &dyn std::fmt::Display| {
        format!(
            "could not create cache key file '{}': {error}",
            path.display()
        )
    };
    let mut key = [0u8; MIN_KEY_BYTES];
    getrandom::getrandom(&mut key).map_err(|error| failed(&error))?;
    private_directory(directory).map_err(|error| failed(&error))?;
    let temporary = write_temporary_key(directory, &key).map_err(|error| failed(&error))?;
    let published = fs::hard_link(&temporary, path);
    super::cleanup_temporary(&temporary);
    match published {
        Ok(()) => CacheKey::new(&key),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => read_key_file(path)?
            .ok_or_else(|| failed(&"another process created the key file and it disappeared")),
        Err(error) => Err(failed(&error)),
    }
}

#[cfg(unix)]
fn write_temporary_key(directory: &Path, key: &[u8]) -> io::Result<std::path::PathBuf> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    use std::sync::atomic::Ordering;

    let counter = super::TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let temporary = directory.join(format!(
        ".{KEY_FILE_NAME}.tmp-{}-{counter}",
        std::process::id()
    ));
    let mut options = fs::OpenOptions::new();
    let mut file = options
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temporary)?;
    // 공개한 키 파일이 비어 있으면 이후 실행마다 캐시가 꺼지므로 링크하기 전에 디스크에 내린다.
    let written = file.write_all(key).and_then(|()| file.sync_all());
    drop(file);
    if let Err(error) = written {
        super::cleanup_temporary(&temporary);
        return Err(error);
    }
    Ok(temporary)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn environment(config_home: Option<&str>, home: Option<&str>) -> KeyEnvironment {
        KeyEnvironment {
            cache_key: None,
            config_home: config_home.map(OsString::from),
            home: home.map(OsString::from),
        }
    }

    #[test]
    fn directory_problem_rejects_links_other_owners_and_shared_write_bits() {
        assert_eq!(directory_problem(false, 501, 0o40700, 501), None);
        assert_eq!(directory_problem(false, 501, 0o40755, 501), None);
        assert_eq!(
            directory_problem(true, 501, 0o120777, 501),
            Some(SYMLINK_DIRECTORY)
        );
        let foreign = directory_problem(false, 0, 0o40700, 501).unwrap();
        assert!(foreign.contains("another user"), "{foreign}");
        for mode in [0o40770, 0o40707, 0o41777] {
            let shared = directory_problem(false, 501, mode, 501).unwrap();
            assert!(shared.contains("writable by group or other"), "{mode:o}");
        }
    }

    /// 끝 `/`·`.`와 겹친 `/`만 지운다. 앞의 `.`·`..`와 절대 경로 뿌리는 그대로다.
    #[test]
    fn normalized_directory_drops_trailing_separators_and_dot_components() {
        for (given, expected) in [
            ("linked/", "linked"),
            ("linked/.", "linked"),
            ("linked//", "linked"),
            ("linked/./", "linked"),
            ("a/./b/", "a/b"),
            ("./cache/", "./cache"),
            ("../cache/.", "../cache"),
            ("/tmp/cache/", "/tmp/cache"),
            ("linked", "linked"),
        ] {
            assert_eq!(
                normalized_directory(Path::new(given)),
                Path::new(expected),
                "{given}"
            );
        }
    }

    #[test]
    fn key_file_problem_requires_an_owner_only_file_of_at_least_32_bytes() {
        assert_eq!(key_file_problem(501, 0o100600, 32, 501), None);
        assert_eq!(key_file_problem(501, 0o100400, 64, 501), None);
        let foreign = key_file_problem(0, 0o100600, 32, 501).unwrap();
        assert!(foreign.contains("another user"), "{foreign}");
        for mode in [0o100640, 0o100604, 0o100610] {
            let shared = key_file_problem(501, mode, 32, 501).unwrap();
            assert!(shared.contains("group or other"), "{mode:o}");
        }
        let short = key_file_problem(501, 0o100600, 31, 501).unwrap();
        assert!(short.contains("fewer than 32 bytes"), "{short}");
    }

    #[test]
    fn key_directory_follows_xdg_and_ignores_empty_or_relative_values() {
        let path = |config: Option<&str>, home: Option<&str>| {
            key_directory(&environment(config, home)).map(|path| path.display().to_string())
        };
        assert_eq!(
            path(Some("/xdg"), Some("/home/u")),
            Ok("/xdg/schemagraph".into())
        );
        for config in [None, Some(""), Some("relative/config")] {
            assert_eq!(
                path(config, Some("/home/u")),
                Ok("/home/u/.config/schemagraph".into())
            );
        }
        for home in [None, Some(""), Some("relative-home")] {
            let missing = path(None, home).unwrap_err();
            assert!(missing.contains("SCHEMAGRAPH_CACHE_KEY"), "{missing}");
        }
    }
}
