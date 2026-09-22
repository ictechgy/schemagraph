//! 오프라인 dbt manifest와 bounded JSONL query-log를 catalog document에 붙인다.
//!
//! 이 모듈은 SQL을 해석하거나 간선을 만들지 않는다. 입력 SQL은 RoutineDoc
//! 원문으로만 옮기고, 의미 해석은 이후 parser/analysis 단계에 남긴다. 모든
//! 검증은 clone에 적용하기 전에 끝내므로 실패한 import는 호출자의 document를
//! 바꾸지 않는다.

use crate::document::{CatalogDocument, RoutineDoc};
use schemagraph_core::VertexId;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self};
use std::io;
use std::path::{Component, Path, PathBuf};

pub const IMPORT_REPORT_VERSION: u32 = 1;
/// dbt adapter가 예약한 routine 이름 namespace.
pub const DBT_ROUTINE_PREFIX: &str = "query_dbt_";
/// query-log adapter가 예약한 routine 이름 namespace.
pub const QUERY_LOG_ROUTINE_PREFIX: &str = "query_log_";
const DEFAULT_MAX_INPUT_BYTES: u64 = 64 * 1024 * 1024;
const DEFAULT_MAX_ROWS: usize = 100_000;
const DEFAULT_MAX_SQL_BYTES: u64 = 8 * 1024 * 1024;
const DEFAULT_MAX_TOTAL_SQL_BYTES: u64 = 64 * 1024 * 1024;
const DEFAULT_MAX_PATH_BYTES: usize = 16 * 1024;
const MAX_IDENTIFIER_BYTES: usize = 1024;

/// 가져올 offline 입력의 종류.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImportKind {
    DbtManifest,
    QueryLogJsonl,
}

impl ImportKind {
    /// report와 CLI 오류에 사용하는 안정적인 이름.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::DbtManifest => "dbt-manifest",
            Self::QueryLogJsonl => "query-log-jsonl",
        }
    }
}

/// import가 사용할 입력·행·SQL 예산.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImportLimits {
    pub max_input_bytes: u64,
    pub max_rows: usize,
    pub max_sql_bytes: u64,
    pub max_total_sql_bytes: u64,
    pub max_path_bytes: usize,
}

impl Default for ImportLimits {
    fn default() -> Self {
        Self {
            max_input_bytes: DEFAULT_MAX_INPUT_BYTES,
            max_rows: DEFAULT_MAX_ROWS,
            max_sql_bytes: DEFAULT_MAX_SQL_BYTES,
            max_total_sql_bytes: DEFAULT_MAX_TOTAL_SQL_BYTES,
            max_path_bytes: DEFAULT_MAX_PATH_BYTES,
        }
    }
}

/// 변환된 document와 별도 관측 report.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ImportResult {
    pub document: CatalogDocument,
    pub report: ImportReport,
}

/// import 결과의 결정적 JSON 계약.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ImportReport {
    pub version: u32,
    pub kind: String,
    pub source_id: String,
    pub database: String,
    pub sql_sha256: String,
    pub input_sha256: String,
    pub catalog_sha256: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub window_start: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub window_end: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sampling: Option<String>,
    pub imported: usize,
    pub entries: Vec<ImportEntry>,
    pub limitations: Vec<String>,
}

/// 하나의 안정적인 외부 query identity와 관측 부속물.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ImportEntry {
    pub stable_id: String,
    pub schema: String,
    pub routine_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub original_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub query_id: Option<String>,
    pub sql_bytes: u64,
    pub sql_sha256: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub observed_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub executions: Option<u64>,
}

#[derive(Debug, Clone)]
struct PendingRoutine {
    stable_id: String,
    schema: String,
    routine: RoutineDoc,
    entry: ImportEntry,
    sql: String,
}

/// dbt manifest의 model compiled SQL을 query routine으로 붙인다.
pub fn import_dbt_manifest(
    document: &CatalogDocument,
    manifest_path: &Path,
    project_root: Option<&Path>,
    limits: ImportLimits,
) -> Result<ImportResult, String> {
    let (source_id, database) = comparison_scope(document)?;
    let bytes = read_bounded(manifest_path, limits.max_input_bytes, "dbt manifest")?;
    let input_sha256 = sha256_hex(&bytes);
    let catalog_sha256 = canonical_catalog_sha256(document)?;
    let value: Value = serde_json::from_slice(&bytes)
        .map_err(|error| format!("invalid dbt manifest JSON: {error}"))?;
    let nodes = value
        .get("nodes")
        .and_then(Value::as_object)
        .ok_or("dbt manifest requires an object-valued 'nodes' field")?;
    let root = project_root.map(canonical_project_root).transpose()?;
    let mut ids = BTreeSet::new();
    let mut source_paths = BTreeMap::<String, String>::new();
    let mut pending = Vec::new();
    let mut total_sql_bytes = 0u64;
    let mut keys: Vec<_> = nodes.keys().cloned().collect();
    keys.sort();
    for key in keys {
        let node = nodes
            .get(&key)
            .ok_or_else(|| format!("dbt manifest node '{key}' disappeared during import"))?;
        let object = node
            .as_object()
            .ok_or_else(|| format!("dbt manifest node '{key}' must be an object"))?;
        let resource_type = required_string(object, "resource_type", &format!("node '{key}'"))?;
        if resource_type != "model" {
            continue;
        }
        let unique_id = required_string(object, "unique_id", &format!("node '{key}'"))?;
        if unique_id != key {
            return Err(format!(
                "dbt model node key '{key}' does not match unique_id '{unique_id}'"
            ));
        }
        if !ids.insert(unique_id.clone()) {
            return Err(format!("duplicate dbt model unique_id '{unique_id}'"));
        }
        if ids.len() > limits.max_rows {
            return Err(format!(
                "dbt manifest exceeds {} model nodes",
                limits.max_rows
            ));
        }
        let schema = required_string(object, "schema", &format!("dbt model '{unique_id}'"))?;
        validate_schema_scope(document, &schema)?;
        let node_database =
            required_string(object, "database", &format!("dbt model '{unique_id}'"))?;
        if node_database != database {
            return Err(format!(
                "dbt model '{unique_id}' database '{node_database}' does not match catalog database '{database}'"
            ));
        }
        let compiled_code = optional_string(object, "compiled_code", &format!("node '{key}'"))?;
        let compiled_path = optional_string(object, "compiled_path", &format!("node '{key}'"))?;
        let original_path =
            optional_string(object, "original_file_path", &format!("node '{key}'"))?;
        let source_candidate = compiled_path.clone();
        let (body, source) = match compiled_code {
            Some(body) => {
                if body.trim().is_empty() {
                    return Err(format!("dbt model '{unique_id}' compiled_code is empty"));
                }
                let source = read_compiled_sql(
                    root.as_deref(),
                    source_candidate.as_deref(),
                    limits.max_path_bytes,
                    limits.max_sql_bytes,
                    Some(&body),
                    &unique_id,
                )?
                .1;
                (body, source)
            }
            None => read_compiled_sql(
                root.as_deref(),
                source_candidate.as_deref(),
                limits.max_path_bytes,
                limits.max_sql_bytes,
                None,
                &unique_id,
            )?,
        };
        let sql_bytes = body.len() as u64;
        let sql = body.clone();
        add_sql_budget(
            &mut total_sql_bytes,
            sql_bytes,
            limits.max_sql_bytes,
            limits.max_total_sql_bytes,
            &unique_id,
        )?;
        let name = dbt_routine_name(&unique_id);
        let sql_sha256 = sha256_hex(body.as_bytes());
        if let Some(path) = &source {
            if let Some(previous) = source_paths.insert(path.clone(), unique_id.clone()) {
                if previous != unique_id {
                    return Err(format!(
                        "dbt models '{previous}' and '{unique_id}' share compiled SQL path '{path}'"
                    ));
                }
            }
        }
        let routine_id = routine_id(&schema, &name);
        pending.push(PendingRoutine {
            stable_id: unique_id.clone(),
            schema: schema.clone(),
            routine: RoutineDoc {
                name,
                kind: "query".into(),
                language: Some("sql".into()),
                body: Some(body),
                signature: None,
                usage: None,
                member_of: None,
                source: source.clone(),
            },
            sql,
            entry: ImportEntry {
                stable_id: unique_id,
                schema,
                routine_id,
                source,
                original_path,
                query_id: None,
                sql_bytes,
                sql_sha256,
                observed_at: None,
                executions: None,
            },
        });
    }
    let imported = ids.len();
    apply_pending(
        document,
        pending,
        DBT_ROUTINE_PREFIX,
        format!(
            "dbt manifest import added {imported} compiled SQL queries; compiled SQL is static and has no runtime usage statistics"
        ),
        ImportReport {
            version: IMPORT_REPORT_VERSION,
            kind: ImportKind::DbtManifest.as_str().into(),
            source_id,
            database,
            window_start: None,
            window_end: None,
            sampling: None,
            imported,
            entries: Vec::new(),
            limitations: Vec::new(),
            sql_sha256: String::new(),
            input_sha256,
            catalog_sha256,
        },
    )
}

/// bounded JSONL query-log를 query routine으로 붙인다.
pub fn import_query_log(
    document: &CatalogDocument,
    jsonl_path: &Path,
    limits: ImportLimits,
) -> Result<ImportResult, String> {
    let (catalog_source_id, catalog_database) = comparison_scope(document)?;
    let bytes = read_bounded(jsonl_path, limits.max_input_bytes, "query log")?;
    let input_sha256 = sha256_hex(&bytes);
    let catalog_sha256 = canonical_catalog_sha256(document)?;
    let text = String::from_utf8(bytes).map_err(|_| "query log is not valid UTF-8".to_owned())?;
    let mut lines = text.lines();
    let header_line = lines.next().ok_or("query log is empty")?;
    if header_line.trim().is_empty() {
        return Err("query log header is empty".into());
    }
    let header: QueryLogHeader = serde_json::from_str(header_line)
        .map_err(|error| format!("invalid query log header: {error}"))?;
    if header.version != 1 || header.record_type != "query-log" {
        return Err("query log header must declare type 'query-log' and version 1".into());
    }
    if header.source_id != catalog_source_id {
        return Err(format!(
            "query log source_id '{}' does not match catalog source_id '{}'",
            header.source_id, catalog_source_id
        ));
    }
    validate_identifier(&header.source_id, "query log source_id")?;
    validate_identifier(&header.database, "query log database")?;
    if header.database != catalog_database {
        return Err(format!(
            "query log database '{}' does not match catalog database '{}'",
            header.database, catalog_database
        ));
    }
    if header.sampling.trim().is_empty() || header.sampling.chars().any(char::is_control) {
        return Err("query log sampling must be nonempty".into());
    }
    let window_start = Timestamp::parse(&header.window_start)
        .map_err(|error| format!("invalid query log window_start: {error}"))?;
    let window_end = Timestamp::parse(&header.window_end)
        .map_err(|error| format!("invalid query log window_end: {error}"))?;
    if window_start > window_end {
        return Err("query log window_start must not be after window_end".into());
    }
    let mut pending = Vec::new();
    let mut ids = BTreeSet::new();
    let mut total_sql_bytes = 0u64;
    for (index, line) in lines.enumerate() {
        let row_number = index + 2;
        if index >= limits.max_rows {
            return Err(format!("query log exceeds {} rows", limits.max_rows));
        }
        if line.trim().is_empty() {
            return Err(format!("query log row {row_number} is empty"));
        }
        let row: QueryLogRow = serde_json::from_str(line)
            .map_err(|error| format!("invalid query log row {row_number}: {error}"))?;
        if row.query_id.trim().is_empty()
            || row.schema.trim().is_empty()
            || row.query_id.chars().any(char::is_control)
            || row.schema.chars().any(char::is_control)
        {
            return Err(format!(
                "query log row {row_number} has an empty query_id or schema"
            ));
        }
        validate_identifier(&row.query_id, "query log query_id")?;
        validate_schema_scope(document, &row.schema)?;
        let observed_at = Timestamp::parse(&row.observed_at).map_err(|error| {
            format!("invalid query log observed_at on row {row_number}: {error}")
        })?;
        if observed_at < window_start || observed_at > window_end {
            return Err(format!(
                "query log observed_at on row {row_number} is outside the declared window"
            ));
        }
        if row.sql.trim().is_empty() {
            return Err(format!("query log row {row_number} SQL is empty"));
        }
        let sql_bytes = row.sql.len() as u64;
        add_sql_budget(
            &mut total_sql_bytes,
            sql_bytes,
            limits.max_sql_bytes,
            limits.max_total_sql_bytes,
            &row.query_id,
        )?;
        let stable_id = query_log_stable_id(&header.source_id, &row.query_id);
        if !ids.insert(stable_id.clone()) {
            return Err(format!(
                "duplicate query-log identity for query_id '{}'",
                row.query_id
            ));
        }
        let name = query_log_routine_name(&header.source_id, &row.query_id);
        let routine_id = routine_id(&row.schema, &name);
        let sql = row.sql.clone();
        let sql_sha256 = sha256_hex(sql.as_bytes());
        pending.push(PendingRoutine {
            stable_id: stable_id.clone(),
            schema: row.schema.clone(),
            routine: RoutineDoc {
                name,
                kind: "query".into(),
                language: Some("sql".into()),
                body: Some(row.sql),
                signature: None,
                usage: None,
                member_of: None,
                source: None,
            },
            sql,
            entry: ImportEntry {
                stable_id,
                schema: row.schema,
                routine_id,
                source: None,
                original_path: None,
                query_id: Some(row.query_id),
                sql_bytes,
                sql_sha256,
                observed_at: Some(row.observed_at),
                executions: Some(row.executions),
            },
        });
    }
    let imported = pending.len();
    let limitation = format!(
        "query-log import observed {imported} queries in window {}..{}; sampling={}; execution counts remain in ImportReport and are not routine usage",
        header.window_start, header.window_end, header.sampling
    );
    apply_pending(
        document,
        pending,
        QUERY_LOG_ROUTINE_PREFIX,
        limitation,
        ImportReport {
            version: IMPORT_REPORT_VERSION,
            kind: ImportKind::QueryLogJsonl.as_str().into(),
            source_id: header.source_id,
            database: header.database,
            window_start: Some(header.window_start),
            window_end: Some(header.window_end),
            sampling: Some(header.sampling),
            imported,
            entries: Vec::new(),
            limitations: Vec::new(),
            sql_sha256: String::new(),
            input_sha256,
            catalog_sha256,
        },
    )
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct QueryLogHeader {
    version: u32,
    #[serde(rename = "type")]
    record_type: String,
    source_id: String,
    database: String,
    window_start: String,
    window_end: String,
    sampling: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct QueryLogRow {
    query_id: String,
    schema: String,
    sql: String,
    observed_at: String,
    executions: u64,
}

fn apply_pending(
    document: &CatalogDocument,
    pending: Vec<PendingRoutine>,
    reserved_prefix: &'static str,
    limitation: String,
    mut report: ImportReport,
) -> Result<ImportResult, String> {
    validate_conflicts(document, &pending, reserved_prefix)?;
    let mut output = document.clone();
    for item in &pending {
        let schema = output
            .schemas
            .iter_mut()
            .find(|schema| schema.name == item.schema)
            .ok_or_else(|| format!("import target schema '{}' disappeared", item.schema))?;
        schema.routines.push(item.routine.clone());
    }
    for schema in &mut output.schemas {
        schema.routines.sort_by(|left, right| {
            left.name
                .cmp(&right.name)
                .then(left.signature.cmp(&right.signature))
                .then(left.source.cmp(&right.source))
        });
    }
    output.limitations.push(limitation.clone());
    output.limitations.sort();
    output.limitations.dedup();
    let sql_digest = sql_sha256(&pending);
    report.entries = pending.into_iter().map(|item| item.entry).collect();
    report.entries.sort_by(|left, right| {
        left.stable_id
            .cmp(&right.stable_id)
            .then(left.schema.cmp(&right.schema))
    });
    report.limitations = vec![limitation];
    report.sql_sha256 = sql_digest;
    Ok(ImportResult {
        document: output,
        report,
    })
}

fn validate_conflicts(
    document: &CatalogDocument,
    pending: &[PendingRoutine],
    reserved_prefix: &'static str,
) -> Result<(), String> {
    let mut pending_ids = BTreeSet::new();
    for schema in &document.schemas {
        for routine in &schema.routines {
            if routine.name.starts_with(reserved_prefix) {
                return Err(format!(
                    "catalog already contains reserved imported routine '{}'; start from the original catalog for a new import",
                    routine.name
                ));
            }
        }
    }
    for item in pending {
        if !pending_ids.insert(item.stable_id.clone()) {
            return Err(format!(
                "duplicate imported stable identity '{}'",
                item.stable_id
            ));
        }
        let canonical_routine = VertexId::routine(
            &item.schema,
            None,
            &item.routine.name,
            item.routine.signature.as_deref(),
        );
        for schema in &document.schemas {
            if schema.name != item.schema {
                continue;
            }
            if schema
                .objects
                .iter()
                .any(|object| VertexId::object(&schema.name, &object.name) == canonical_routine)
            {
                return Err(format!(
                    "imported routine '{}' conflicts with an existing object id",
                    item.routine.name
                ));
            }
        }
        let mut matches = Vec::new();
        for schema in &document.schemas {
            for routine in &schema.routines {
                if routine.name == item.routine.name {
                    matches.push((schema.name.as_str(), routine));
                }
            }
        }
        if matches.len() > 1 {
            return Err(format!(
                "imported routine '{}' has duplicate existing identities",
                item.routine.name
            ));
        }
        for (schema_name, existing) in matches {
            if schema_name != item.schema {
                return Err(format!(
                    "imported routine '{}' already exists in schema '{schema_name}'",
                    item.routine.name
                ));
            }
            if existing.kind != "query" {
                return Err(format!(
                    "imported routine '{}' conflicts with existing {}",
                    item.routine.name, existing.kind
                ));
            }
            if existing.source != item.routine.source {
                return Err(format!(
                    "imported routine '{}' conflicts with an existing query source",
                    item.routine.name
                ));
            }
        }
    }
    Ok(())
}

fn comparison_scope(document: &CatalogDocument) -> Result<(String, String), String> {
    let context = document
        .context
        .as_ref()
        .ok_or("offline import requires catalog context with explicit source_id and database")?;
    if context.source_id.trim().is_empty() {
        return Err("offline import requires a nonempty catalog context source_id".into());
    }
    validate_identifier(&context.source_id, "catalog source_id")?;
    let database = context
        .database
        .as_deref()
        .filter(|database| !database.trim().is_empty())
        .ok_or("offline import requires an explicit catalog context database")?;
    validate_identifier(database, "catalog database")?;
    Ok((context.source_id.clone(), database.to_owned()))
}

fn validate_schema_scope(document: &CatalogDocument, schema: &str) -> Result<(), String> {
    validate_identifier(schema, "import schema")?;
    if !document.schemas.iter().any(|item| item.name == schema) {
        return Err(format!(
            "offline import schema '{schema}' is not in the catalog"
        ));
    }
    if let Some(filter) = document
        .context
        .as_ref()
        .and_then(|context| context.schema_filter.as_ref())
    {
        if !filter.iter().any(|item| item == schema) {
            return Err(format!(
                "offline import schema '{schema}' is outside the catalog schema scope"
            ));
        }
    }
    Ok(())
}

fn required_string(
    object: &serde_json::Map<String, Value>,
    key: &str,
    owner: &str,
) -> Result<String, String> {
    let value = object
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("{owner} requires string field '{key}'"))?;
    if value.trim().is_empty() || value.chars().any(char::is_control) {
        return Err(format!(
            "{owner} field '{key}' must be nonempty and free of control characters"
        ));
    }
    validate_identifier(value, owner)?;
    Ok(value.to_owned())
}

fn validate_identifier(value: &str, label: &str) -> Result<(), String> {
    if value.len() > MAX_IDENTIFIER_BYTES {
        return Err(format!("{label} exceeds {MAX_IDENTIFIER_BYTES} bytes"));
    }
    Ok(())
}

fn optional_string(
    object: &serde_json::Map<String, Value>,
    key: &str,
    owner: &str,
) -> Result<Option<String>, String> {
    match object.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) => Ok(Some(value.clone())),
        Some(_) => Err(format!("{owner} field '{key}' must be a string or null")),
    }
}

/// Read an authorized offline input through one no-follow file descriptor.
pub fn read_bounded_file(path: &Path, limit: u64, label: &str) -> Result<Vec<u8>, String> {
    read_bounded(path, limit, label)
}

#[cfg(unix)]
fn read_bounded(path: &Path, limit: u64, label: &str) -> Result<Vec<u8>, String> {
    use rustix::fs::{open, Mode, OFlags};

    let file = open(
        path,
        OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
        Mode::empty(),
    )
    .map_err(|error| io_error(&format!("securely open {label}"), path, error.into()))?;
    read_open_fd(&file, limit, label, path)
}

#[cfg(not(unix))]
fn read_bounded(_path: &Path, _limit: u64, _label: &str) -> Result<Vec<u8>, String> {
    Err(
        "offline import inputs require Unix no-symlink file-descriptor support on this build"
            .into(),
    )
}

#[cfg(unix)]
fn read_open_fd(
    file: &rustix::fd::OwnedFd,
    limit: u64,
    label: &str,
    path: &Path,
) -> Result<Vec<u8>, String> {
    use rustix::fs::FileType;

    let stat = rustix::fs::fstat(file)
        .map_err(|error| format!("could not inspect {label} '{}': {error}", path.display()))?;
    if !FileType::from_raw_mode(stat.st_mode).is_file() {
        return Err(format!("{label} is not a regular file: {}", path.display()));
    }
    let mut bytes = Vec::new();
    let mut buffer = [0u8; 8192];
    loop {
        let count = rustix::io::read(file, &mut buffer)
            .map_err(|error| format!("could not read {label} '{}': {error}", path.display()))?;
        if count == 0 {
            break;
        }
        bytes.extend_from_slice(&buffer[..count]);
        if bytes.len() as u64 > limit {
            return Err(format!("{label} exceeds {limit} bytes: {}", path.display()));
        }
    }
    Ok(bytes)
}

fn canonical_project_root(path: &Path) -> Result<PathBuf, String> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| io_error("inspect dbt project root", path, error))?;
    if metadata.file_type().is_symlink() {
        return Err(format!(
            "dbt project root must not be a symlink: {}",
            path.display()
        ));
    }
    if !metadata.is_dir() {
        return Err(format!(
            "dbt project root is not a directory: {}",
            path.display()
        ));
    }
    fs::canonicalize(path).map_err(|error| io_error("canonicalize dbt project root", path, error))
}

fn validate_relative_sql_path(raw: &str, max_path_bytes: usize) -> Result<String, String> {
    if raw.is_empty() || raw.len() > max_path_bytes || raw.chars().any(char::is_control) {
        return Err("compiled SQL path is empty, too long, or contains control characters".into());
    }
    let normalized = raw.replace('\\', "/");
    let path = Path::new(&normalized);
    if path.is_absolute()
        || path.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
    {
        return Err(format!("compiled SQL path escapes the project root: {raw}"));
    }
    let relative = path
        .components()
        .filter_map(|component| match component {
            Component::Normal(value) => value.to_str(),
            Component::CurDir => None,
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("/");
    if relative.is_empty()
        || !Path::new(&relative)
            .extension()
            .and_then(|extension| extension.to_str())
            .is_some_and(|extension| extension.eq_ignore_ascii_case("sql"))
    {
        return Err(format!(
            "compiled SQL path must name a relative .sql file: {raw}"
        ));
    }
    Ok(relative)
}

fn read_compiled_sql(
    root: Option<&Path>,
    raw: Option<&str>,
    max_path_bytes: usize,
    max_sql_bytes: u64,
    expected: Option<&str>,
    identity: &str,
) -> Result<(String, Option<String>), String> {
    let Some(raw) = raw else {
        return expected.map(|body| (body.to_owned(), None)).ok_or_else(|| {
            format!("dbt model '{identity}' has no compiled_code and no compiled_path")
        });
    };
    let relative = validate_relative_sql_path(raw, max_path_bytes)?;
    let Some(root) = root else {
        return expected.map(|body| (body.to_owned(), None)).ok_or_else(|| {
            format!("dbt model '{identity}' needs --project-root to read compiled SQL")
        });
    };
    let bytes = match secure_read_relative(root, &relative, max_sql_bytes, identity)? {
        Some(bytes) => bytes,
        None => {
            if let Some(expected) = expected {
                return Ok((expected.to_owned(), None));
            }
            return Err(format!("compiled SQL path does not exist: {relative}"));
        }
    };
    let body = String::from_utf8(bytes)
        .map_err(|_| format!("dbt compiled SQL is not valid UTF-8: {relative}"))?;
    if let Some(expected) = expected {
        if body.as_bytes() == expected.as_bytes() {
            Ok((expected.to_owned(), Some(relative)))
        } else {
            Ok((expected.to_owned(), None))
        }
    } else {
        Ok((body, Some(relative)))
    }
}

#[cfg(unix)]
fn secure_read_relative(
    root: &Path,
    relative: &str,
    limit: u64,
    identity: &str,
) -> Result<Option<Vec<u8>>, String> {
    use rustix::fs::{open, openat, Mode, OFlags};

    let mut directory = open(
        root,
        OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
        Mode::empty(),
    )
    .map_err(|error| {
        format!("could not securely open dbt project root for '{identity}': {error}")
    })?;
    let components: Vec<_> = Path::new(relative)
        .components()
        .filter_map(|component| match component {
            Component::Normal(value) => Some(value),
            _ => None,
        })
        .collect();
    let (filename, directories) = components
        .split_last()
        .ok_or_else(|| format!("compiled SQL path is empty for '{identity}'"))?;
    for component in directories {
        directory = openat(
            &directory,
            *component,
            OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
            Mode::empty(),
        )
        .map_err(|error| {
            format!("could not securely open compiled SQL directory for '{identity}': {error}")
        })?;
    }
    let file = match openat(
        &directory,
        *filename,
        OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
        Mode::empty(),
    ) {
        Ok(file) => file,
        Err(error) if error == rustix::io::Errno::NOENT => return Ok(None),
        Err(error) => {
            return Err(format!(
                "could not securely open compiled SQL for '{identity}': {error}"
            ))
        }
    };
    read_open_fd(&file, limit, "compiled SQL", Path::new(relative)).map(Some)
}

#[cfg(not(unix))]
fn secure_read_relative(
    _root: &Path,
    _relative: &str,
    _limit: u64,
    _identity: &str,
) -> Result<Option<Vec<u8>>, String> {
    Err("dbt compiled_path import requires a Unix no-symlink file-descriptor API on this build; use compiled_code without a path".into())
}

fn add_sql_budget(
    total: &mut u64,
    bytes: u64,
    per_query_limit: u64,
    total_limit: u64,
    identity: &str,
) -> Result<(), String> {
    if bytes > per_query_limit {
        return Err(format!(
            "SQL for '{identity}' exceeds {per_query_limit} bytes"
        ));
    }
    *total = total
        .checked_add(bytes)
        .ok_or("import SQL byte budget overflow")?;
    if *total > total_limit {
        return Err(format!("import SQL exceeds {total_limit} total bytes"));
    }
    Ok(())
}

fn dbt_routine_name(unique_id: &str) -> String {
    format!("{}{}", DBT_ROUTINE_PREFIX, hex_encode(unique_id.as_bytes()))
}

fn query_log_stable_id(source_id: &str, query_id: &str) -> String {
    format!(
        "{}:{}",
        hex_encode(source_id.as_bytes()),
        hex_encode(query_id.as_bytes())
    )
}

fn query_log_routine_name(source_id: &str, query_id: &str) -> String {
    format!(
        "{}{}",
        QUERY_LOG_ROUTINE_PREFIX,
        query_log_stable_id(source_id, query_id)
    )
}

fn routine_id(schema: &str, name: &str) -> String {
    VertexId::routine(schema, None, name, None)
        .as_str()
        .to_owned()
}

fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    hex_encode(&digest)
}

fn canonical_catalog_sha256(document: &CatalogDocument) -> Result<String, String> {
    let bytes = serde_json::to_vec(document)
        .map_err(|error| format!("could not hash catalog document: {error}"))?;
    Ok(sha256_hex(&bytes))
}

fn sql_sha256(pending: &[PendingRoutine]) -> String {
    let mut items: Vec<_> = pending.iter().collect();
    items.sort_by(|left, right| left.stable_id.cmp(&right.stable_id));
    let mut hasher = Sha256::new();
    for item in items {
        hasher.update((item.stable_id.len() as u64).to_be_bytes());
        hasher.update(item.stable_id.as_bytes());
        hasher.update((item.sql.len() as u64).to_be_bytes());
        hasher.update(item.sql.as_bytes());
    }
    hex_encode(&hasher.finalize())
}

fn io_error(action: &str, path: &Path, error: io::Error) -> String {
    format!("{action} '{}': {error}", path.display())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct Timestamp {
    year: u32,
    month: u32,
    day: u32,
    hour: u32,
    minute: u32,
    second: u32,
    nanos: u32,
}

impl Timestamp {
    fn parse(value: &str) -> Result<Self, String> {
        if !value.is_ascii() || !value.ends_with('Z') {
            return Err("timestamp must be UTC ISO-8601 with a trailing Z".into());
        }
        let body = &value[..value.len() - 1];
        if body.len() < 19
            || body.as_bytes()[4] != b'-'
            || body.as_bytes()[7] != b'-'
            || body.as_bytes()[10] != b'T'
            || body.as_bytes()[13] != b':'
            || body.as_bytes()[16] != b':'
        {
            return Err("timestamp must use YYYY-MM-DDTHH:MM:SS[.fraction]Z".into());
        }
        let year = parse_digits(&body[0..4])?;
        let month = parse_digits(&body[5..7])?;
        let day = parse_digits(&body[8..10])?;
        let hour = parse_digits(&body[11..13])?;
        let minute = parse_digits(&body[14..16])?;
        let second = parse_digits(&body[17..19])?;
        let nanos = if body.len() == 19 {
            0
        } else {
            let fraction = body
                .get(20..)
                .ok_or("timestamp fraction must start with '.'")?;
            if body.as_bytes().get(19).is_none_or(|value| *value != b'.')
                || fraction.is_empty()
                || fraction.len() > 9
                || !fraction.bytes().all(|byte| byte.is_ascii_digit())
            {
                return Err("timestamp fraction must contain 1-9 digits".into());
            }
            let mut value = fraction
                .parse::<u32>()
                .map_err(|_| "invalid timestamp fraction")?;
            for _ in fraction.len()..9 {
                value *= 10;
            }
            value
        };
        let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
        let days = match month {
            1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
            4 | 6 | 9 | 11 => 30,
            2 if leap => 29,
            2 => 28,
            _ => return Err("timestamp month is invalid".into()),
        };
        if day == 0 || day > days || hour > 23 || minute > 59 || second > 59 {
            return Err("timestamp date or time is invalid".into());
        }
        Ok(Self {
            year,
            month,
            day,
            hour,
            minute,
            second,
            nanos,
        })
    }
}

fn parse_digits(value: &str) -> Result<u32, String> {
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err("timestamp contains a non-numeric field".into());
    }
    value
        .parse()
        .map_err(|_| "timestamp numeric field is invalid".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::document::{CatalogDocument, CollectionContext, SchemaDoc};
    use std::fs;
    use tempfile::tempdir;

    fn document() -> CatalogDocument {
        CatalogDocument {
            version: 1,
            dialect: "external".into(),
            reader: "fixture".into(),
            schemas: vec![SchemaDoc {
                name: "analytics".into(),
                objects: vec![],
                routines: vec![],
            }],
            limitations: vec![],
            context: Some(CollectionContext {
                source_id: "warehouse-prod".into(),
                database: Some("warehouse".into()),
                schema_filter: Some(vec!["analytics".into()]),
                catalog_complete: true,
            }),
            dependencies: vec![],
        }
    }

    fn write_manifest(path: &Path, node: Value) {
        fs::write(
            path,
            serde_json::to_vec(&serde_json::json!({"nodes": {"model.demo.orders": node}})).unwrap(),
        )
        .unwrap();
    }

    fn limits() -> ImportLimits {
        ImportLimits {
            max_input_bytes: 1024 * 1024,
            max_rows: 10,
            max_sql_bytes: 1024,
            max_total_sql_bytes: 4096,
            max_path_bytes: 256,
        }
    }

    #[test]
    fn dbt_import_keeps_compiled_sql_raw_and_report_sorted() {
        let directory = tempdir().unwrap();
        let manifest = directory.path().join("manifest.json");
        write_manifest(
            &manifest,
            serde_json::json!({
                "resource_type":"model",
                "unique_id":"model.demo.orders",
                "database":"warehouse",
                "schema":"analytics",
                "compiled_code":"SELECT id FROM raw.orders",
                "compiled_path":"models/orders.sql"
            }),
        );
        fs::create_dir(directory.path().join("models")).unwrap();
        fs::write(
            directory.path().join("models/orders.sql"),
            "SELECT id FROM raw.orders",
        )
        .unwrap();
        let result =
            import_dbt_manifest(&document(), &manifest, Some(directory.path()), limits()).unwrap();
        let routine = &result.document.schemas[0].routines[0];
        assert_eq!(routine.kind, "query");
        assert_eq!(routine.body.as_deref(), Some("SELECT id FROM raw.orders"));
        assert_eq!(routine.source.as_deref(), Some("models/orders.sql"));
        assert_eq!(result.report.entries[0].stable_id, "model.demo.orders");
        assert_eq!(result.report.entries[0].sql_sha256.len(), 64);
        assert_eq!(result.report.sql_sha256.len(), 64);
        assert!(result.report.limitations[0].contains("static"));
    }

    #[test]
    fn dbt_path_escape_and_context_mismatch_leave_document_unchanged() {
        let directory = tempdir().unwrap();
        let manifest = directory.path().join("manifest.json");
        write_manifest(
            &manifest,
            serde_json::json!({
                "resource_type":"model",
                "unique_id":"model.demo.orders",
                "database":"other",
                "schema":"analytics",
                "compiled_code":"SELECT 1",
                "compiled_path":"../orders.sql"
            }),
        );
        let original = document();
        assert!(
            import_dbt_manifest(&original, &manifest, Some(directory.path()), limits()).is_err()
        );
        assert_eq!(original, document());
    }

    #[test]
    fn query_log_validates_window_scope_and_never_synthesizes_usage() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("queries.jsonl");
        fs::write(
            &path,
            concat!(
                "{\"version\":1,\"type\":\"query-log\",\"source_id\":\"warehouse-prod\",\"database\":\"warehouse\",\"window_start\":\"2026-01-01T00:00:00Z\",\"window_end\":\"2026-01-01T01:00:00Z\",\"sampling\":\"1/10\"}\n",
                "{\"query_id\":\"q-2\",\"schema\":\"analytics\",\"sql\":\"SELECT 2\",\"observed_at\":\"2026-01-01T00:30:00.500Z\",\"executions\":7}\n",
                "{\"query_id\":\"q-1\",\"schema\":\"analytics\",\"sql\":\"SELECT 1\",\"observed_at\":\"2026-01-01T00:20:00Z\",\"executions\":3}\n",
            ),
        )
        .unwrap();
        let result = import_query_log(&document(), &path, limits()).unwrap();
        assert_eq!(result.report.entries[0].query_id.as_deref(), Some("q-1"));
        assert_eq!(result.report.entries[1].query_id.as_deref(), Some("q-2"));
        assert_eq!(
            routine_id(
                "analytics",
                &query_log_routine_name("warehouse-prod", "q:1")
            ),
            "analytics.query_log_77617265686f7573652d70726f64%3A713a31"
        );
        assert_eq!(result.report.entries[0].sql_sha256.len(), 64);
        assert!(result.document.schemas[0]
            .routines
            .iter()
            .all(|routine| routine.usage.is_none()));
        assert!(result.report.limitations[0].contains("sampling=1/10"));
    }

    #[test]
    fn query_log_rejects_duplicate_and_unknown_scope_without_mutation() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("queries.jsonl");
        let header = "{\"version\":1,\"type\":\"query-log\",\"source_id\":\"warehouse-prod\",\"database\":\"warehouse\",\"window_start\":\"2026-01-01T00:00:00Z\",\"window_end\":\"2026-01-01T01:00:00Z\",\"sampling\":\"all\"}\n";
        let row = "{\"query_id\":\"q-1\",\"schema\":\"analytics\",\"sql\":\"SELECT 1\",\"observed_at\":\"2026-01-01T00:20:00Z\",\"executions\":1}\n";
        fs::write(&path, format!("{header}{row}{row}")).unwrap();
        let original = document();
        assert!(import_query_log(&original, &path, limits()).is_err());
        assert_eq!(original, document());
    }

    #[test]
    fn timestamp_validation_rejects_invalid_window() {
        assert!(Timestamp::parse("2026-02-30T00:00:00Z").is_err());
        assert!(Timestamp::parse("2026-01-01T00:00:00+09:00").is_err());
        assert!(Timestamp::parse("2026-01-01T00:00:00.123Z").is_ok());
    }

    #[test]
    fn query_log_limits_fail_closed_without_mutating_the_catalog() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("queries.jsonl");
        fs::write(
            &path,
            concat!(
                "{\"version\":1,\"type\":\"query-log\",\"source_id\":\"warehouse-prod\",\"database\":\"warehouse\",\"window_start\":\"2026-01-01T00:00:00Z\",\"window_end\":\"2026-01-01T01:00:00Z\",\"sampling\":\"all\"}\n",
                "{\"query_id\":\"q-1\",\"schema\":\"analytics\",\"sql\":\"SELECT 123\",\"observed_at\":\"2026-01-01T00:20:00Z\",\"executions\":1}\n",
                "{\"query_id\":\"q-2\",\"schema\":\"analytics\",\"sql\":\"SELECT 2\",\"observed_at\":\"2026-01-01T00:21:00Z\",\"executions\":1}\n",
            ),
        )
        .unwrap();
        let original = document();
        let mut small = limits();
        small.max_rows = 1;
        assert!(import_query_log(&original, &path, small).is_err());
        assert_eq!(original, document());
        small = limits();
        small.max_sql_bytes = 4;
        assert!(import_query_log(&original, &path, small).is_err());
        assert_eq!(original, document());
    }

    #[test]
    fn dbt_without_compiled_code_reads_a_bounded_project_file() {
        let directory = tempdir().unwrap();
        let manifest = directory.path().join("manifest.json");
        fs::create_dir(directory.path().join("target")).unwrap();
        fs::write(directory.path().join("target/orders.sql"), "SELECT 1").unwrap();
        write_manifest(
            &manifest,
            serde_json::json!({
                "resource_type":"model",
                "unique_id":"model.demo.orders",
                "database":"warehouse",
                "schema":"analytics",
                "compiled_path":"target/orders.sql"
            }),
        );
        let result =
            import_dbt_manifest(&document(), &manifest, Some(directory.path()), limits()).unwrap();
        assert_eq!(
            result.document.schemas[0].routines[0].body.as_deref(),
            Some("SELECT 1")
        );
        assert_eq!(
            result.document.schemas[0].routines[0].source.as_deref(),
            Some("target/orders.sql")
        );
    }

    #[test]
    fn compiled_code_does_not_read_original_path_and_mismatched_compiled_path_has_no_source() {
        let directory = tempdir().unwrap();
        let manifest = directory.path().join("manifest.json");
        fs::create_dir(directory.path().join("models")).unwrap();
        fs::write(directory.path().join("models/orders.sql"), "SELECT 2").unwrap();
        write_manifest(
            &manifest,
            serde_json::json!({
                "resource_type":"model",
                "unique_id":"model.demo.orders",
                "database":"warehouse",
                "schema":"analytics",
                "compiled_code":"SELECT 1",
                "compiled_path":"models/orders.sql",
                "original_file_path":"models/{{ ref('orders') }}.sql"
            }),
        );
        let result =
            import_dbt_manifest(&document(), &manifest, Some(directory.path()), limits()).unwrap();
        assert!(result.document.schemas[0].routines[0].source.is_none());
        assert_eq!(
            result.report.entries[0].original_path.as_deref(),
            Some("models/{{ ref('orders') }}.sql")
        );

        let manifest = directory.path().join("manifest-no-compiled-path.json");
        write_manifest(
            &manifest,
            serde_json::json!({
                "resource_type":"model",
                "unique_id":"model.demo.orders",
                "database":"warehouse",
                "schema":"analytics",
                "compiled_code":"SELECT 1",
                "original_file_path":"models/missing.sql"
            }),
        );
        let result =
            import_dbt_manifest(&document(), &manifest, Some(directory.path()), limits()).unwrap();
        assert!(result.document.schemas[0].routines[0].source.is_none());
    }

    #[test]
    fn reserved_namespace_forbids_reimport_into_augmented_catalog() {
        let directory = tempdir().unwrap();
        let manifest = directory.path().join("manifest.json");
        write_manifest(
            &manifest,
            serde_json::json!({
                "resource_type":"model",
                "unique_id":"model.demo.orders",
                "database":"warehouse",
                "schema":"analytics",
                "compiled_code":"SELECT 1"
            }),
        );
        let first = import_dbt_manifest(&document(), &manifest, None, limits()).unwrap();
        let error = import_dbt_manifest(&first.document, &manifest, None, limits())
            .unwrap_err()
            .to_string();
        assert!(error.contains("reserved imported routine"), "{error}");
        let empty_manifest = directory.path().join("empty-manifest.json");
        fs::write(&empty_manifest, "{\"nodes\":{}}").unwrap();
        assert!(
            import_dbt_manifest(&first.document, &empty_manifest, None, limits())
                .unwrap_err()
                .contains("reserved imported routine")
        );

        let log = directory.path().join("queries.jsonl");
        fs::write(
            &log,
            concat!(
                "{\"version\":1,\"type\":\"query-log\",\"source_id\":\"warehouse-prod\",\"database\":\"warehouse\",\"window_start\":\"2026-01-01T00:00:00Z\",\"window_end\":\"2026-01-01T01:00:00Z\",\"sampling\":\"all\"}\n",
                "{\"query_id\":\"q-1\",\"schema\":\"analytics\",\"sql\":\"SELECT 1\",\"observed_at\":\"2026-01-01T00:20:00Z\",\"executions\":1}\n"
            ),
        )
        .unwrap();
        let dbt_then_log = import_dbt_manifest(&document(), &manifest, None, limits()).unwrap();
        assert!(import_query_log(&dbt_then_log.document, &log, limits()).is_ok());
        let first = import_query_log(&document(), &log, limits()).unwrap();
        assert!(import_query_log(&first.document, &log, limits())
            .unwrap_err()
            .contains("reserved imported routine"));
        assert!(import_query_log(&first.document, &log, limits()).is_err());
        let empty_log = directory.path().join("empty-queries.jsonl");
        fs::write(
            &empty_log,
            "{\"version\":1,\"type\":\"query-log\",\"source_id\":\"warehouse-prod\",\"database\":\"warehouse\",\"window_start\":\"2026-01-01T00:00:00Z\",\"window_end\":\"2026-01-01T01:00:00Z\",\"sampling\":\"all\"}\n",
        )
        .unwrap();
        assert!(import_query_log(&first.document, &empty_log, limits())
            .unwrap_err()
            .contains("reserved imported routine"));

        let mut collision = document();
        collision.schemas[0].objects.push(
            serde_json::from_value(serde_json::json!({
                "name": query_log_routine_name("warehouse-prod", "q-1"),
                "kind": "table",
                "columns": [],
                "constraints": [],
                "indexes": [],
                "triggers": []
            }))
            .unwrap(),
        );
        assert!(import_query_log(&collision, &log, limits())
            .unwrap_err()
            .contains("existing object id"));
    }

    #[cfg(unix)]
    #[test]
    fn dbt_compiled_path_symlink_is_rejected() {
        let directory = tempdir().unwrap();
        let outside = tempdir().unwrap();
        let outside_sql = outside.path().join("outside.sql");
        fs::write(&outside_sql, "SELECT outside").unwrap();
        std::os::unix::fs::symlink(&outside_sql, directory.path().join("orders.sql")).unwrap();
        let manifest = directory.path().join("manifest.json");
        write_manifest(
            &manifest,
            serde_json::json!({
                "resource_type":"model",
                "unique_id":"model.demo.orders",
                "database":"warehouse",
                "schema":"analytics",
                "compiled_path":"orders.sql"
            }),
        );
        assert!(
            import_dbt_manifest(&document(), &manifest, Some(directory.path()), limits()).is_err()
        );
    }

    #[cfg(unix)]
    #[test]
    fn dbt_parent_component_symlink_is_rejected() {
        let directory = tempdir().unwrap();
        let outside = tempdir().unwrap();
        fs::write(outside.path().join("orders.sql"), "SELECT outside").unwrap();
        std::os::unix::fs::symlink(outside.path(), directory.path().join("models")).unwrap();
        let manifest = directory.path().join("manifest.json");
        write_manifest(
            &manifest,
            serde_json::json!({
                "resource_type":"model",
                "unique_id":"model.demo.orders",
                "database":"warehouse",
                "schema":"analytics",
                "compiled_path":"models/orders.sql"
            }),
        );
        assert!(
            import_dbt_manifest(&document(), &manifest, Some(directory.path()), limits()).is_err()
        );
    }
}
