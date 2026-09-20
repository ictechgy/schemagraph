//! 디렉터리의 SQL 파일을 외부 query routine으로 문서에 붙인다.
//!
//! 이 모듈은 SQL을 파싱하지 않는다. 파일 경로와 원문만 document에 보존하고,
//! 의미 해석과 간선 생성은 parser/engine의 책임으로 남긴다.

use std::fs::{self, DirEntry, File};
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use crate::document::{CatalogDocument, RoutineDoc};

const MAX_SQL_FILE_BYTES: u64 = 8 * 1024 * 1024;
const MAX_RELATIVE_PATH_BYTES: usize = 16 * 1024;

const IGNORED_DIRECTORY_NAMES: &[&str] = &[".git", "target", "build", "node_modules"];

/// SQL 파일을 수집해 `default_schema`의 외부 query routine으로 추가한다.
///
/// 모든 파일을 먼저 읽고 충돌을 검증한 뒤 document를 바꾸므로, 실패한
/// 호출은 입력 document를 부분적으로 바꾸지 않는다. 같은 source의 query는
/// 반복 실행 시 교체하고, 다른 routine과의 이름 충돌은 오류로 보고한다.
pub fn attach(
    document: &mut CatalogDocument,
    directory: &Path,
    default_schema: &str,
) -> Result<usize, String> {
    let schema_index = document
        .schemas
        .iter()
        .position(|schema| schema.name == default_schema)
        .ok_or_else(|| format!("SQL file default schema '{default_schema}' was not collected"))?;

    let files = collect_files(directory)?;
    let routines = read_routines(&files)?;
    validate_conflicts(document, schema_index, &routines)?;
    apply_routines(document, schema_index, routines);
    Ok(files.len())
}

struct CollectedRoutine {
    relative: String,
    name: String,
    body: String,
}

fn collect_files(root: &Path) -> Result<Vec<(String, PathBuf)>, String> {
    let root_type =
        fs::symlink_metadata(root).map_err(|error| io_error("read SQL directory", root, error))?;
    if root_type.file_type().is_symlink() {
        return Err(format!(
            "SQL directory must not be a symlink: {}",
            root.display()
        ));
    }
    if !root_type.is_dir() {
        return Err(format!("SQL path is not a directory: {}", root.display()));
    }

    let mut files = Vec::new();
    visit_directory(root, root, &mut files)?;
    files.sort_by(|left, right| left.0.cmp(&right.0));
    if files.windows(2).any(|pair| pair[0].0 == pair[1].0) {
        return Err("SQL files have duplicate normalized relative paths".into());
    }
    Ok(files)
}

fn visit_directory(
    root: &Path,
    directory: &Path,
    files: &mut Vec<(String, PathBuf)>,
) -> Result<(), String> {
    let mut entries: Vec<DirEntry> = fs::read_dir(directory)
        .map_err(|error| io_error("read SQL directory", directory, error))?
        .collect::<Result<_, _>>()
        .map_err(|error| io_error("read SQL directory entry", directory, error))?;
    entries.sort_by_key(DirEntry::file_name);

    for entry in entries {
        let path = entry.path();
        let file_type = entry
            .file_type()
            .map_err(|error| io_error("inspect SQL directory entry", &path, error))?;
        // symlink_metadata/file_type를 사용해 링크를 따라가지 않는다. 선택한
        // root 밖의 파일을 SQL 입력으로 끌어오지 않는 것이 이 수집기의 경계다.
        if file_type.is_symlink() {
            continue;
        }
        if file_type.is_dir() {
            if is_ignored_directory(&entry) {
                continue;
            }
            visit_directory(root, &path, files)?;
            continue;
        }
        if !file_type.is_file() || !is_sql_file(&path) {
            continue;
        }
        let relative = relative_path(root, &path)?;
        files.push((relative, path));
    }
    Ok(())
}

fn is_ignored_directory(entry: &DirEntry) -> bool {
    entry
        .file_name()
        .to_str()
        .is_some_and(|name| IGNORED_DIRECTORY_NAMES.contains(&name))
}

fn is_sql_file(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("sql"))
}

fn relative_path(root: &Path, path: &Path) -> Result<String, String> {
    let relative = path
        .strip_prefix(root)
        .map_err(|_| format!("SQL file escaped selected directory: {}", path.display()))?;
    let relative = relative
        .to_str()
        .ok_or_else(|| format!("SQL file path is not valid UTF-8: {}", path.display()))?
        .replace('\\', "/");
    if relative.len() > MAX_RELATIVE_PATH_BYTES {
        return Err(format!(
            "SQL file relative path exceeds {MAX_RELATIVE_PATH_BYTES} bytes: {relative}"
        ));
    }
    Ok(relative)
}

fn read_routines(files: &[(String, PathBuf)]) -> Result<Vec<CollectedRoutine>, String> {
    files
        .iter()
        .map(|(relative, path)| {
            let metadata = fs::symlink_metadata(path)
                .map_err(|error| io_error("inspect SQL file", path, error))?;
            if metadata.file_type().is_symlink() {
                return Err(format!(
                    "SQL file became a symlink while collecting: {relative}"
                ));
            }
            if metadata.len() > MAX_SQL_FILE_BYTES {
                return Err(format!(
                    "SQL file exceeds {MAX_SQL_FILE_BYTES} bytes: {relative}"
                ));
            }
            let mut bytes = Vec::with_capacity(metadata.len().min(MAX_SQL_FILE_BYTES) as usize);
            let file = File::open(path).map_err(|error| io_error("open SQL file", path, error))?;
            file.take(MAX_SQL_FILE_BYTES + 1)
                .read_to_end(&mut bytes)
                .map_err(|error| io_error("read SQL file", path, error))?;
            if bytes.len() as u64 > MAX_SQL_FILE_BYTES {
                return Err(format!(
                    "SQL file exceeds {MAX_SQL_FILE_BYTES} bytes: {relative}"
                ));
            }
            let body = String::from_utf8(bytes)
                .map_err(|_| format!("SQL file is not valid UTF-8: {relative}"))?;
            Ok(CollectedRoutine {
                name: query_name(relative),
                relative: relative.clone(),
                body,
            })
        })
        .collect()
}

fn query_name(relative: &str) -> String {
    let mut name = String::from("query_");
    for byte in relative.as_bytes() {
        name.push_str(&format!("{byte:02x}"));
    }
    name
}

fn validate_conflicts(
    document: &CatalogDocument,
    schema_index: usize,
    routines: &[CollectedRoutine],
) -> Result<(), String> {
    let schema = &document.schemas[schema_index];
    for collected in routines {
        let same_source: Vec<&RoutineDoc> = schema
            .routines
            .iter()
            .filter(|routine| routine.source.as_deref() == Some(collected.relative.as_str()))
            .collect();
        if same_source.len() > 1 {
            return Err(format!(
                "multiple routines already claim SQL source '{}': refusing duplicate",
                collected.relative
            ));
        }
        if let Some(existing) = same_source.first() {
            if existing.kind != "query" || existing.name != collected.name {
                return Err(format!(
                    "SQL source '{}' conflicts with existing routine '{}': refusing overwrite",
                    collected.relative, existing.name
                ));
            }
        }
        if schema.routines.iter().any(|routine| {
            routine.name == collected.name
                && routine.source.as_deref() != Some(collected.relative.as_str())
        }) {
            return Err(format!(
                "SQL query name '{}' conflicts with an unrelated routine",
                collected.name
            ));
        }
    }
    Ok(())
}

fn apply_routines(
    document: &mut CatalogDocument,
    schema_index: usize,
    routines: Vec<CollectedRoutine>,
) {
    let schema = &mut document.schemas[schema_index];
    for collected in routines {
        let routine = RoutineDoc {
            name: collected.name,
            kind: "query".into(),
            language: Some("sql".into()),
            body: Some(collected.body),
            signature: None,
            usage: None,
            member_of: None,
            source: Some(collected.relative.clone()),
        };
        if let Some(existing) = schema
            .routines
            .iter_mut()
            .find(|existing| existing.source.as_deref() == Some(collected.relative.as_str()))
        {
            *existing = routine;
        } else {
            schema.routines.push(routine);
        }
    }
    schema.routines.sort_by(|left, right| {
        left.name
            .cmp(&right.name)
            .then(left.signature.cmp(&right.signature))
            .then(left.source.cmp(&right.source))
    });
}

fn io_error(action: &str, path: &Path, error: io::Error) -> String {
    format!("{action} '{}': {error}", path.display())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::document::{CatalogDocument, SchemaDoc};
    use std::fs;
    use tempfile::tempdir;

    fn document() -> CatalogDocument {
        CatalogDocument {
            version: 1,
            dialect: "external".into(),
            reader: "test".into(),
            schemas: vec![SchemaDoc {
                name: "public".into(),
                objects: vec![],
                routines: vec![],
            }],
            limitations: vec![],
            context: None,
            dependencies: vec![],
        }
    }

    #[test]
    fn files_are_sorted_named_by_full_relative_path_and_keep_raw_bodies() {
        let directory = tempdir().unwrap();
        fs::create_dir(directory.path().join("nested")).unwrap();
        fs::write(directory.path().join("z.sql"), "SELECT 3;\n").unwrap();
        fs::write(directory.path().join("a.sql"), "SELECT 1;\n").unwrap();
        fs::write(directory.path().join("nested/a.sql"), "SELECT 2;\n").unwrap();
        let mut doc = document();

        assert_eq!(attach(&mut doc, directory.path(), "public"), Ok(3));
        let routines = &doc.schemas[0].routines;
        assert_eq!(routines.len(), 3);
        let sources: Vec<_> = routines
            .iter()
            .map(|routine| routine.source.as_deref().unwrap())
            .collect();
        assert_eq!(sources, vec!["a.sql", "nested/a.sql", "z.sql"]);
        assert_eq!(routines[0].kind, "query");
        assert_eq!(routines[0].language.as_deref(), Some("sql"));
        assert_eq!(routines[0].body.as_deref(), Some("SELECT 1;\n"));
        assert_ne!(routines[0].name, routines[1].name);
        assert!(routines[0].name.starts_with("query_"));
        assert!(routines[0].signature.is_none());
        assert!(routines[0].usage.is_none());
        assert!(routines[0].member_of.is_none());
    }

    #[test]
    fn repeated_attach_replaces_exact_source_without_duplication() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("query.sql");
        fs::write(&path, "SELECT 1;").unwrap();
        let mut doc = document();
        assert_eq!(attach(&mut doc, directory.path(), "public"), Ok(1));
        fs::write(&path, "SELECT 2;").unwrap();
        assert_eq!(attach(&mut doc, directory.path(), "public"), Ok(1));
        assert_eq!(doc.schemas[0].routines.len(), 1);
        assert_eq!(
            doc.schemas[0].routines[0].body.as_deref(),
            Some("SELECT 2;")
        );
    }

    #[test]
    fn symlinks_and_ignored_directories_are_not_collected() {
        let directory = tempdir().unwrap();
        fs::create_dir(directory.path().join("target")).unwrap();
        fs::write(directory.path().join("target/ignored.sql"), "SELECT 0;").unwrap();
        fs::write(directory.path().join("kept.sql"), "SELECT 1;").unwrap();
        #[cfg(unix)]
        {
            let outside = tempdir().unwrap();
            let outside_file = outside.path().join("outside.sql");
            fs::write(&outside_file, "SELECT outside;").unwrap();
            std::os::unix::fs::symlink(&outside_file, directory.path().join("link.sql")).unwrap();
        }
        let mut doc = document();
        assert_eq!(attach(&mut doc, directory.path(), "public"), Ok(1));
        assert_eq!(
            doc.schemas[0].routines[0].source.as_deref(),
            Some("kept.sql")
        );
    }

    #[test]
    fn unknown_schema_and_bad_file_leave_document_unchanged() {
        let directory = tempdir().unwrap();
        fs::write(directory.path().join("ok.sql"), "SELECT 1;").unwrap();
        let mut doc = document();
        let before = doc.clone();
        assert!(attach(&mut doc, directory.path(), "missing").is_err());
        assert_eq!(doc, before);

        let directory = tempdir().unwrap();
        fs::write(directory.path().join("ok.sql"), "SELECT 1;").unwrap();
        fs::write(directory.path().join("bad.sql"), [0xff, 0xfe]).unwrap();
        let before = doc.clone();
        let error = attach(&mut doc, directory.path(), "public").unwrap_err();
        assert!(error.contains("UTF-8"));
        assert_eq!(doc, before);
    }

    #[test]
    fn oversized_file_is_rejected_without_partial_mutation() {
        let directory = tempdir().unwrap();
        fs::write(directory.path().join("ok.sql"), "SELECT 1;").unwrap();
        let oversized = directory.path().join("large.sql");
        File::create(&oversized)
            .unwrap()
            .set_len(MAX_SQL_FILE_BYTES + 1)
            .unwrap();
        let mut doc = document();
        let before = doc.clone();
        let error = attach(&mut doc, directory.path(), "public").unwrap_err();
        assert!(error.contains("exceeds"));
        assert_eq!(doc, before);
    }
}
