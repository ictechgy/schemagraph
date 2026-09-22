//! 오프라인 외부 SQL import의 CLI 경계.
//!
//! source 어댑터가 반환한 document와 ImportReport를 각각 결정적 JSON으로
//! 원자적으로 기록한다. 이 모듈도 SQL을 파싱하거나 그래프를 만들지 않는다.

use anyhow::{bail, Context, Result};
use schemagraph_source::imports::{self, ImportKind, ImportLimits};
use schemagraph_source::{self as source};
use sha2::{Digest, Sha256};
use std::fs::{self, File};
use std::io::{Cursor, Write};
use std::path::{Path, PathBuf};
use tempfile::NamedTempFile;

/// catalog에 offline 입력을 붙이고 document/report 두 산출물을 기록한다.
pub(crate) fn run(
    catalog: &Path,
    input: &Path,
    output_document: &Path,
    output_report: &Path,
    kind: ImportKind,
    project_root: Option<&Path>,
    document_version: u32,
    limits: ImportLimits,
) -> Result<i32> {
    let catalog_path = validate_input(catalog, "catalog", limits.max_input_bytes)?;
    let input_path = validate_input(input, "import input", limits.max_input_bytes)?;
    if catalog_path == input_path || same_file(&catalog_path, &input_path)? {
        bail!("catalog and import input must be different files");
    }
    let document_output = validate_new_output(output_document, "document output")?;
    let report_output = validate_new_output(output_report, "report output")?;
    if document_output == report_output
        || document_output == catalog_path
        || document_output == input_path
        || report_output == catalog_path
        || report_output == input_path
    {
        bail!("import document and report outputs must be different paths");
    }
    let catalog_bytes =
        source::imports::read_bounded_file(&catalog_path, limits.max_input_bytes, "catalog")
            .map_err(|error| anyhow::anyhow!("could not read catalog: {error}"))?;
    let document = source::codec::document_from_reader(Cursor::new(&catalog_bytes))
        .map_err(|error| anyhow::anyhow!("invalid catalog document: {error}"))?;
    let mut result = match kind {
        ImportKind::DbtManifest => {
            imports::import_dbt_manifest(&document, &input_path, project_root, limits)
        }
        ImportKind::QueryLogJsonl => {
            if project_root.is_some() {
                bail!("--project-root is only valid for dbt manifest import");
            }
            imports::import_query_log(&document, &input_path, limits)
        }
    }
    .map_err(|error| anyhow::anyhow!("offline import failed: {error}"))?;
    result.report.catalog_sha256 = sha256_hex(&catalog_bytes);
    let document_value = source::codec::document_to_value(&result.document, document_version)
        .map_err(|error| anyhow::anyhow!("could not encode imported catalog: {error}"))?;
    let mut document_bytes = serde_json::to_vec_pretty(&document_value)?;
    document_bytes.push(b'\n');
    let mut report_bytes = serde_json::to_vec_pretty(&serde_json::to_value(&result.report)?)?;
    report_bytes.push(b'\n');
    atomic_write_pair(
        &document_output,
        &document_bytes,
        &report_output,
        &report_bytes,
    )?;
    Ok(0)
}

fn parent_dir(path: &Path) -> &Path {
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
}

fn validate_input(path: &Path, label: &str, limit: u64) -> Result<PathBuf> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("could not inspect {label} {}", path.display()))?;
    if metadata.file_type().is_symlink() {
        bail!("{label} must not be a symlink: {}", path.display());
    }
    if !metadata.is_file() {
        bail!("{label} is not a regular file: {}", path.display());
    }
    if metadata.len() > limit {
        bail!("{label} exceeds {limit} bytes: {}", path.display());
    }
    Ok(fs::canonicalize(path)
        .with_context(|| format!("could not canonicalize {label} {}", path.display()))?)
}

fn same_file(left: &Path, right: &Path) -> Result<bool> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let left = fs::metadata(left)?;
        let right = fs::metadata(right)?;
        Ok(left.dev() == right.dev() && left.ino() == right.ino())
    }
    #[cfg(not(unix))]
    {
        let _ = (left, right);
        Ok(false)
    }
}

fn output_key(path: &Path) -> Result<PathBuf> {
    let parent = parent_dir(path);
    let parent = fs::canonicalize(parent).with_context(|| {
        format!(
            "could not resolve import output directory {}",
            parent.display()
        )
    })?;
    let name = path
        .file_name()
        .context("import output path must have a filename")?;
    Ok(parent.join(name))
}

fn validate_new_output(path: &Path, label: &str) -> Result<PathBuf> {
    let key = output_key(path)?;
    if fs::symlink_metadata(&key).is_ok() {
        bail!(
            "{label} already exists; import only publishes new files: {}",
            path.display()
        );
    }
    Ok(key)
}

fn atomic_write_pair(
    document_path: &Path,
    document_bytes: &[u8],
    report_path: &Path,
    report_bytes: &[u8],
) -> Result<()> {
    let mut document_temp =
        NamedTempFile::new_in(parent_dir(document_path)).with_context(|| {
            format!(
                "could not create temporary import output in {}",
                parent_dir(document_path).display()
            )
        })?;
    let mut report_temp = NamedTempFile::new_in(parent_dir(report_path)).with_context(|| {
        format!(
            "could not create temporary import output in {}",
            parent_dir(report_path).display()
        )
    })?;
    write_temporary(&mut document_temp, document_bytes, document_path)?;
    write_temporary(&mut report_temp, report_bytes, report_path)?;
    let document_file = document_temp
        .persist_noclobber(document_path)
        .map_err(|error| {
            anyhow::anyhow!(
                "could not publish new import document {}: {}",
                document_path.display(),
                error.error
            )
        })?;
    let _report_file = match report_temp.persist_noclobber(report_path) {
        Ok(file) => file,
        Err(error) => {
            let rollback = remove_owned_output(&document_file, document_path);
            return match rollback {
                Ok(()) => Err(anyhow::anyhow!(
                    "could not publish new import report {}: {}; document output rolled back",
                    report_path.display(),
                    error.error
                )),
                Err(rollback_error) => Err(anyhow::anyhow!(
                    "could not publish new import report {}: {}; document rollback failed: {}",
                    report_path.display(),
                    error.error,
                    rollback_error
                )),
            };
        }
    };
    Ok(())
}

fn remove_owned_output(file: &File, path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let current = match fs::symlink_metadata(path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error.into()),
        };
        let owned = file.metadata()?;
        if current.dev() != owned.dev() || current.ino() != owned.ino() {
            bail!("output was replaced by another writer; replacement preserved");
        }
        fs::remove_file(path)?;
        Ok(())
    }
    #[cfg(not(unix))]
    {
        let _ = (file, path);
        bail!("output ownership could not be verified; remove the partial output manually");
    }
}

fn write_temporary(file: &mut NamedTempFile, bytes: &[u8], path: &Path) -> Result<()> {
    file.as_file_mut()
        .write_all(bytes)
        .and_then(|_| file.as_file().sync_all())
        .with_context(|| format!("could not write import output {}", path.display()))?;
    Ok(())
}

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn rollback_preserves_replacement_and_removes_only_the_owned_file() {
        let directory = tempfile::tempdir().unwrap();
        let output = directory.path().join("document.json");
        let owned = File::create(&output).unwrap();
        let replacement = directory.path().join("replacement.json");
        fs::write(&replacement, b"keep another writer").unwrap();
        fs::rename(replacement, &output).unwrap();
        assert!(remove_owned_output(&owned, &output).is_err());
        assert_eq!(fs::read(&output).unwrap(), b"keep another writer");
        let actual = File::open(&output).unwrap();
        remove_owned_output(&actual, &output).unwrap();
        assert!(!output.exists());
    }
}
