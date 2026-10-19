//! 디렉터리의 SQL 파일을 외부 query routine으로 문서에 붙인다.
//!
//! 이 모듈은 SQL을 파싱하지 않는다. 파일 경로와 원문만 document에 보존하고,
//! 의미 해석과 간선 생성은 parser/engine의 책임으로 남긴다.

use std::collections::BTreeMap;
use std::fs::{self, DirEntry, File};
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use crate::document::{CatalogDocument, RoutineDoc};

const MAX_SQL_FILE_BYTES: u64 = 8 * 1024 * 1024;
const MAX_RELATIVE_PATH_BYTES: usize = 16 * 1024;

const IGNORED_DIRECTORY_NAMES: &[&str] = &[".git", "target", "build", "node_modules"];

/// 경로 사전 검사와 실제 수집이 같은 파일 집합을 사용하게 한다. SQL 원문은 보관하지 않는다.
pub struct SqlFiles {
    files: Vec<(String, PathBuf)>,
}

impl SqlFiles {
    /// 출력이 입력을 덮어쓰는지 검사할 때 수집기의 제외 규칙을 다시 구현하지 않는다.
    pub fn paths(&self) -> impl Iterator<Item = &Path> {
        self.files.iter().map(|(_, path)| path.as_path())
    }
}

/// symlink·빌드 디렉터리 제외와 상대 경로 검사를 한 번 수행해 파일 집합을 고정한다.
pub fn collect(directory: &Path) -> Result<SqlFiles, String> {
    Ok(SqlFiles {
        files: collect_files(directory)?,
    })
}

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
    attach_collected(document, &collect(directory)?, default_schema)
}

/// 출력 사전 검사를 마친 파일 집합에서 SQL 원문을 읽어 document에 붙인다.
pub fn attach_collected(
    document: &mut CatalogDocument,
    files: &SqlFiles,
    default_schema: &str,
) -> Result<usize, String> {
    let schema_index = document
        .schemas
        .iter()
        .position(|schema| schema.name == default_schema)
        .ok_or_else(|| format!("SQL file default schema '{default_schema}' was not collected"))?;

    let routines = read_routines(&files.files)?;
    validate_conflicts(document, schema_index, &routines)?;
    apply_routines(document, schema_index, routines);
    Ok(files.files.len())
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
    // 파일마다 스키마의 routine 전체를 두 번 훑으면 파일 수 × routine 수가 된다. 검사하는 동안
    // document는 바뀌지 않으므로 source·이름 색인을 한 번 만들고, 파일 순서와 파일 안의 세 검사
    // 순서는 그대로 둔다.
    let by_source = routines_by(&schema.routines, |routine| routine.source.as_deref());
    let by_name = routines_by(&schema.routines, |routine| Some(routine.name.as_str()));
    for collected in routines {
        let same_source = by_source
            .get(collected.relative.as_str())
            .map(Vec::as_slice)
            .unwrap_or_default();
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
        let named = by_name
            .get(collected.name.as_str())
            .map(Vec::as_slice)
            .unwrap_or_default();
        if named
            .iter()
            .any(|routine| routine.source.as_deref() != Some(collected.relative.as_str()))
        {
            return Err(format!(
                "SQL query name '{}' conflicts with an unrelated routine",
                collected.name
            ));
        }
    }
    Ok(())
}

/// routine을 key별로 문서 순서대로 모은다. key가 없는 routine은 넣지 않는다.
fn routines_by<'d>(
    routines: &'d [RoutineDoc],
    key: impl Fn(&'d RoutineDoc) -> Option<&'d str>,
) -> BTreeMap<&'d str, Vec<&'d RoutineDoc>> {
    #[cfg(test)]
    work::visited(routines.len());
    let mut index = BTreeMap::<&str, Vec<&RoutineDoc>>::new();
    for routine in routines {
        if let Some(key) = key(routine) {
            index.entry(key).or_default().push(routine);
        }
    }
    index
}

fn apply_routines(
    document: &mut CatalogDocument,
    schema_index: usize,
    routines: Vec<CollectedRoutine>,
) {
    let schema = &mut document.schemas[schema_index];
    // 파일마다 커지는 목록에서 같은 source를 찾으면 파일 수의 제곱이 된다. 새 routine은 모았다가
    // 같은 순서로 끝에 붙이므로 정렬 전 목록과 정렬 결과가 순차 push와 같다.
    let positions = replacement_positions(&schema.routines, &routines);
    let mut added = Vec::new();
    for (collected, position) in routines.into_iter().zip(positions) {
        let routine = RoutineDoc {
            name: collected.name,
            kind: "query".into(),
            language: Some("sql".into()),
            body: Some(collected.body),
            signature: None,
            usage: None,
            member_of: None,
            source: Some(collected.relative),
        };
        match position {
            Some(index) => schema.routines[index] = routine,
            None => added.push(routine),
        }
    }
    schema.routines.extend(added);
    schema.routines.sort_by(|left, right| {
        left.name
            .cmp(&right.name)
            .then(left.signature.cmp(&right.signature))
            .then(left.source.cmp(&right.source))
    });
}

/// 수집한 파일마다 교체할 기존 routine의 위치를 붙이기 전 목록에서 찾는다.
///
/// 교체는 같은 source를 같은 자리에 다시 쓰고 수집 경로는 서로 달라서, 붙이는 도중의 목록에서
/// 차례로 찾던 위치와 같다. 같은 source가 여럿이면 순차 find처럼 앞의 것을 고른다.
fn replacement_positions(
    existing: &[RoutineDoc],
    routines: &[CollectedRoutine],
) -> Vec<Option<usize>> {
    #[cfg(test)]
    work::visited(existing.len());
    let mut positions = BTreeMap::new();
    for (index, routine) in existing.iter().enumerate() {
        if let Some(source) = routine.source.as_deref() {
            positions.entry(source).or_insert(index);
        }
    }
    routines
        .iter()
        .map(|collected| positions.get(collected.relative.as_str()).copied())
        .collect()
}

fn io_error(action: &str, path: &Path, error: io::Error) -> String {
    format!("{action} '{}': {error}", path.display())
}

/// 테스트가 첨부 작업량을 시간 대신 횟수로 확인하게 한다.
///
/// 파일 수 × routine 수의 이차 회귀는 작은 입력에서 시간으로 드러나지 않고, 큰 입력의 시간
/// 단언은 공유 기계에서 불안정하다. 테스트마다 스레드가 따로라 thread_local이면 섞이지 않는다.
#[cfg(test)]
mod work {
    use std::cell::Cell;

    thread_local! {
        static VISITED: Cell<usize> = const { Cell::new(0) };
    }

    /// 충돌 검사와 교체가 들여다본 기존 routine 수를 더한다.
    pub(super) fn visited(count: usize) {
        VISITED.with(|visited| visited.set(visited.get() + count));
    }

    /// 지금까지 센 routine 수를 돌려주고 0으로 되돌린다.
    pub(super) fn take_visited() -> usize {
        VISITED.with(Cell::take)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::document::{CatalogDocument, SchemaDoc};
    use std::fs;
    use tempfile::tempdir;

    fn routine(name: &str, kind: &str, source: Option<&str>) -> RoutineDoc {
        RoutineDoc {
            name: name.into(),
            kind: kind.into(),
            language: Some("sql".into()),
            body: Some("SELECT 0;".into()),
            signature: None,
            usage: None,
            member_of: None,
            source: source.map(str::to_owned),
        }
    }

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
    fn attach_indexes_existing_routines_once_instead_of_per_file() {
        let directory = tempdir().unwrap();
        let (files, functions) = (40, 30);
        for index in 0..files {
            fs::write(
                directory.path().join(format!("q{index:02}.sql")),
                format!("SELECT {index};"),
            )
            .unwrap();
        }
        let mut doc = document();
        doc.schemas[0].routines = (0..functions)
            .map(|index| routine(&format!("report_{index}"), "function", None))
            .collect();
        work::take_visited();
        assert_eq!(attach(&mut doc, directory.path(), "public"), Ok(files));
        let first = work::take_visited();
        let attached = doc.clone();
        assert_eq!(attach(&mut doc, directory.path(), "public"), Ok(files));
        let again = work::take_visited();
        assert_eq!(doc, attached);
        let routines = doc.schemas[0].routines.len();
        assert_eq!(routines, functions + files);
        // 파일마다 routine 전체를 훑거나 커지는 목록에서 찾으면 files × routines가 된다.
        assert!(
            first <= 3 * functions,
            "first attach visited {first} routines for {files} files"
        );
        assert!(
            again <= 3 * routines,
            "re-attach visited {again} routines for {files} files"
        );
    }

    #[test]
    fn indexed_conflict_checks_keep_the_sequential_order_and_messages() {
        let directory = tempdir().unwrap();
        fs::write(directory.path().join("a.sql"), "SELECT 1;").unwrap();
        fs::write(directory.path().join("b.sql"), "SELECT 2;").unwrap();
        let (name_a, name_b) = (query_name("a.sql"), query_name("b.sql"));
        let attach_to = |routines: Vec<RoutineDoc>| {
            let mut doc = document();
            doc.schemas[0].routines = routines;
            let before = doc.clone();
            let result = attach(&mut doc, directory.path(), "public");
            if result.is_err() {
                assert_eq!(doc, before);
            }
            result
        };
        let claimed_twice = vec![
            routine(&name_a, "query", Some("a.sql")),
            routine("other", "query", Some("a.sql")),
        ];
        assert_eq!(
            attach_to(claimed_twice).unwrap_err(),
            "multiple routines already claim SQL source 'a.sql': refusing duplicate"
        );
        assert_eq!(
            attach_to(vec![routine(&name_a, "function", Some("a.sql"))]).unwrap_err(),
            format!(
                "SQL source 'a.sql' conflicts with existing routine '{name_a}': refusing overwrite"
            )
        );
        assert_eq!(
            attach_to(vec![routine(&name_a, "query", Some("elsewhere.sql"))]).unwrap_err(),
            format!("SQL query name '{name_a}' conflicts with an unrelated routine")
        );
        // 파일 순서(a.sql 먼저)대로 검사하므로 b.sql의 이름 충돌보다 a.sql의 source 충돌이 먼저다.
        let both = vec![
            routine(&name_b, "function", None),
            routine("renamed", "query", Some("a.sql")),
        ];
        assert_eq!(
            attach_to(both).unwrap_err(),
            "SQL source 'a.sql' conflicts with existing routine 'renamed': refusing overwrite"
        );
        // 한 파일 안에서는 source 검사를 통과한 뒤 같은 이름의 다른 source를 거절한다.
        let shadowed = vec![
            routine(&name_a, "query", Some("a.sql")),
            routine(&name_a, "query", Some("other.sql")),
        ];
        assert_eq!(
            attach_to(shadowed).unwrap_err(),
            format!("SQL query name '{name_a}' conflicts with an unrelated routine")
        );
        assert_eq!(
            attach_to(vec![routine(&name_a, "query", Some("a.sql"))]),
            Ok(2)
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
