//! 스냅샷을 끝까지 쓴 뒤 같은 디렉터리 안에서 교체해 이전 산출물을 보존한다.

use anyhow::{bail, Context, Result};
use std::fs::{self, Permissions};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use tempfile::NamedTempFile;

/// stdout 표기를 파일 경로로 오인해 입력 보호 검사를 적용하지 않는다.
pub(crate) fn file_path(output: Option<&str>) -> Option<&Path> {
    output.filter(|path| *path != "-").map(Path::new)
}

/// 입력과 산출물의 별칭 충돌을 쓰기 전에 거절한다. 기존 산출물 갱신은 허용한다.
pub(crate) fn validate_paths(inputs: &[&Path], outputs: &[&Path]) -> Result<()> {
    let inputs = inputs
        .iter()
        .map(|path| {
            fs::canonicalize(path)
                .with_context(|| format!("cannot resolve input {}", path.display()))
        })
        .collect::<Result<Vec<_>>>()?;
    let outputs = outputs
        .iter()
        .map(|path| destination(path).map(|(path, _)| path))
        .collect::<Result<Vec<_>>>()?;
    for (index, output) in outputs.iter().enumerate() {
        for other in inputs.iter().chain(&outputs[..index]) {
            if output == other || same_file(output, other)? {
                bail!("input and output paths must be distinct: {} and {}; choose a different output path", output.display(), other.display());
            }
        }
        for other in &outputs[..index] {
            if output.parent() == other.parent()
                && output
                    .file_name()
                    .map(|name| name.to_string_lossy().to_lowercase())
                    == other
                        .file_name()
                        .map(|name| name.to_string_lossy().to_lowercase())
            {
                bail!("output filenames must differ by more than letter case: {} and {}; choose a different output path", output.display(), other.display());
            }
        }
    }
    Ok(())
}

/// 직렬화·flush·동기화가 성공한 파일만 공개한다. 실패하면 임시 파일만 정리한다.
pub(crate) fn atomic_write(
    path: &Path,
    write: impl FnOnce(&mut dyn Write) -> Result<()>,
) -> Result<()> {
    StagedOutput::new(path, write)?.publish()
}

/// 여러 scan 산출물을 모두 준비한 뒤 공개할 수 있도록 쓰기와 교체를 분리한다.
pub(crate) struct StagedOutput {
    path: PathBuf,
    temporary: NamedTempFile,
}

impl StagedOutput {
    pub(crate) fn new(
        path: &Path,
        write: impl FnOnce(&mut dyn Write) -> Result<()>,
    ) -> Result<Self> {
        let (path, permissions) = destination(path)?;
        // destination은 정규 부모와 파일 이름을 결합하므로 parent는 항상 있다.
        let parent = path
            .parent()
            .context("output path has no parent directory")?;
        let mut temporary = NamedTempFile::new_in(parent).with_context(|| {
            format!(
                "cannot stage output {}; check directory permissions",
                path.display()
            )
        })?;
        {
            let mut writer = BufWriter::new(temporary.as_file_mut());
            write(&mut writer)
                .and_then(|_| writer.flush().map_err(Into::into))
                .with_context(|| {
                    format!(
                        "cannot write output {}; previous output preserved",
                        path.display()
                    )
                })?;
        }
        if let Some(permissions) = permissions {
            temporary
                .as_file()
                .set_permissions(permissions)
                .with_context(|| {
                    format!(
                        "cannot preserve permissions for output {}; previous output preserved",
                        path.display()
                    )
                })?;
        }
        temporary.as_file().sync_all().with_context(|| {
            format!(
                "cannot sync output {}; previous output preserved",
                path.display()
            )
        })?;
        Ok(Self { path, temporary })
    }

    pub(crate) fn publish(self) -> Result<()> {
        self.temporary.persist(&self.path).map_err(|error| {
            anyhow::anyhow!(
                "cannot publish output {}; previous output preserved: {}",
                self.path.display(),
                error.error
            )
        })?;
        Ok(())
    }
}

fn destination(path: &Path) -> Result<(PathBuf, Option<Permissions>)> {
    let name = path
        .file_name()
        .context("output path must have a filename")?;
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let parent = fs::canonicalize(parent).with_context(|| {
        format!(
            "cannot resolve output directory {}; create it and retry",
            parent.display()
        )
    })?;
    let path = parent.join(name);
    let permissions = match fs::symlink_metadata(&path) {
        Ok(metadata) => {
            if !metadata.is_file() || metadata.file_type().is_symlink() {
                bail!("output must be a regular file, not a symlink or special file: {}; choose a different output path", path.display());
            }
            if metadata.permissions().readonly() {
                bail!(
                    "output is read-only: {}; choose a writable output path",
                    path.display()
                );
            }
            Some(metadata.permissions())
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => {
            return Err(error).with_context(|| format!("cannot inspect output {}", path.display()))
        }
    };
    // 대소문자를 구분하지 않는 파일시스템에서도 기존 파일의 실제 경로와 비교한다.
    let path = if permissions.is_some() {
        fs::canonicalize(&path)
            .with_context(|| format!("cannot resolve output {}", path.display()))?
    } else {
        path
    };
    Ok((path, permissions))
}

fn same_file(left: &Path, right: &Path) -> Result<bool> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let metadata = |path| match fs::metadata(path) {
            Ok(metadata) => Ok(Some(metadata)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        };
        Ok(match (metadata(left)?, metadata(right)?) {
            (Some(left), Some(right)) => left.dev() == right.dev() && left.ino() == right.ino(),
            _ => false,
        })
    }
    #[cfg(not(unix))]
    {
        let _ = (left, right);
        Ok(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failed_write_preserves_previous_snapshot_and_cleans_temporary_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("graph.json");
        fs::write(&path, b"previous snapshot").unwrap();
        let result = atomic_write(&path, |writer| {
            writer.write_all(b"partial replacement")?;
            bail!("injected write failure")
        });
        assert!(result.is_err());
        assert_eq!(fs::read(&path).unwrap(), b"previous snapshot");
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn failed_new_output_and_unpublished_stage_leave_no_artifact() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("graph.json");
        assert!(atomic_write(&path, |_| bail!("injected failure")).is_err());
        let staged = StagedOutput::new(&path, |writer| Ok(writer.write_all(b"complete")?)).unwrap();
        assert!(!path.exists());
        drop(staged);
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    #[test]
    fn successful_write_replaces_entire_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("graph.json");
        fs::write(&path, b"long previous snapshot").unwrap();
        atomic_write(&path, |writer| Ok(writer.write_all(b"new")?)).unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"new");
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn publish_failure_preserves_destination_and_cleans_stage() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("graph.json");
        let staged = StagedOutput::new(&path, |writer| Ok(writer.write_all(b"new")?)).unwrap();
        fs::create_dir(&path).unwrap();
        assert!(staged.publish().is_err());
        assert!(path.is_dir());
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn replacement_preserves_private_file_mode() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("graph.json");
        fs::write(&path, b"old").unwrap();
        fs::set_permissions(&path, Permissions::from_mode(0o640)).unwrap();
        atomic_write(&path, |writer| Ok(writer.write_all(b"new")?)).unwrap();
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o640
        );
    }
}
