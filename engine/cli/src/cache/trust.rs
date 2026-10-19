//! `--cache-dir`의 신뢰 경계.
//!
//! 캐시 hit는 parser 효과를 다시 해석하지 않고 그래프에 복원한다. 그래서 캐시 디렉터리는
//! 현재 사용자만 쓸 수 있는 진짜 디렉터리여야 한다. 그렇지 못하면 "캐시 오류는 miss"라는
//! 계약대로 경고하고 캐시를 끈다. 분석 결과와 종료 코드는 캐시 없는 실행과 같다.

use std::fs;
use std::io;
use std::path::Path;

/// 캐시 디렉터리를 준비하고, 신뢰할 수 없으면 캐시를 끄는 이유(영어 문장)를 돌려준다.
///
/// 디렉터리가 없으면 0700으로 만든다. umask는 권한 비트를 빼기만 하므로 기본값(0777)으로
/// 만들면 umask 002(사용자 개인 그룹) 환경에서 group 쓰기가 생겨 스스로 캐시를 끄게 된다.
/// 링크는 마지막 경로 성분만 본다. macOS의 `/tmp`·`/var`(그리고 `$TMPDIR`)는 상위 성분이
/// 링크라서 상위까지 거절하면 모든 임시 경로가 막힌다. 끊어진 링크도 따라가 만들지 않는다.
/// 만들기 자체의 I/O 실패(권한 거부, 같은 이름의 파일)는 기존처럼 오류(종료 2)로 둔다.
#[cfg(unix)]
pub(super) fn prepare_directory(directory: &Path) -> io::Result<Option<String>> {
    use std::os::unix::fs::MetadataExt;

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
    Ok(Some(
        "the SQL analysis cache needs Unix ownership checks and no-follow reads; it is disabled on this platform"
            .into(),
    ))
}

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

#[cfg(all(test, unix))]
mod tests {
    use super::*;

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
}
