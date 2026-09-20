//! 신뢰할 수 있는 로컬 SQL 몸체 분석 캐시.
//!
//! 캐시는 parser 효과만 저장한다. 카탈로그·usage는 매 실행마다 새로 만들고,
//! SQL 원문은 namespace 계산과 entry 저장 어디에도 복사하지 않는다.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use schemagraph_core::{
    AnalysisState, Diagnostic, Edge, EdgeKind, Evidence, EvidenceLayer, ObjectAnalysis, Origin,
    SourceLocation, VertexId,
};
use schemagraph_parser::{BodyCache, BodyResult};
use schemagraph_source::document::{
    CatalogDependency, CatalogDocument, CollectionContext, ColumnDoc, ConstraintDoc, ObjectDoc,
    RoutineDoc, SchemaDoc,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const CACHE_FORMAT_VERSION: u32 = 1;
const MAX_ENTRY_BYTES: u64 = 16 * 1024 * 1024;
const COPY_BUFFER_BYTES: usize = 64 * 1024;
const CACHE_SUFFIX: &str = ".json";

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct CacheStats {
    pub hits: u64,
    pub misses: u64,
    pub writes: u64,
    pub warnings: u64,
}

pub(crate) struct DiskBodyCache {
    directory: PathBuf,
    namespace: String,
    stats: CacheStats,
}

impl DiskBodyCache {
    pub(crate) fn new(directory: &Path, document: &CatalogDocument) -> io::Result<Self> {
        let executable = std::env::current_exe()?;
        let executable_hash = hash_file(&executable)?;
        Self::new_with_fingerprint(directory, document, &executable_hash)
    }

    fn new_with_fingerprint(
        directory: &Path,
        document: &CatalogDocument,
        executable_hash: &str,
    ) -> io::Result<Self> {
        fs::create_dir_all(directory)?;
        let namespace = namespace_hash(document, executable_hash)?;
        Ok(Self {
            directory: directory.to_owned(),
            namespace,
            stats: CacheStats::default(),
        })
    }

    pub(crate) fn stats(&self) -> CacheStats {
        self.stats
    }

    fn load_entry(&mut self, owner: &VertexId, body: &str) -> Option<BodyResult> {
        let key = body_key(&self.namespace, owner, body);
        let path = self.entry_path(&key);
        let bytes = match read_bounded(&path) {
            Ok(Some(bytes)) => bytes,
            Ok(None) => {
                self.stats.misses += 1;
                return None;
            }
            Err(error) => {
                self.warning(format!("cache read failed for {key}: {error}"));
                self.stats.misses += 1;
                return None;
            }
        };
        let entry = match serde_json::from_slice::<CacheFile>(&bytes) {
            Ok(entry) => entry,
            Err(error) => {
                self.warning(format!("ignoring malformed cache entry {key}: {error}"));
                self.stats.misses += 1;
                return None;
            }
        };
        if entry.format != CACHE_FORMAT_VERSION || entry.payload.key != key {
            self.warning(format!(
                "ignoring cache entry with mismatched key or format {key}"
            ));
            self.stats.misses += 1;
            return None;
        }
        let checksum = match serialized_hash(&entry.payload) {
            Ok(checksum) => checksum,
            Err(error) => {
                self.warning(format!(
                    "ignoring cache entry {key}: checksum failed: {error}"
                ));
                self.stats.misses += 1;
                return None;
            }
        };
        if checksum != entry.checksum {
            self.warning(format!("ignoring cache entry with invalid checksum {key}"));
            self.stats.misses += 1;
            return None;
        }
        let expected_body_hash = body_hash(body);
        if entry.payload.owner != owner.as_str() || entry.payload.body_hash != expected_body_hash {
            self.warning(format!(
                "ignoring cache entry with invalid body identity {key}"
            ));
            self.stats.misses += 1;
            return None;
        }
        match entry.payload.result.into_result(owner, &expected_body_hash) {
            Ok(result) => {
                self.stats.hits += 1;
                Some(result)
            }
            Err(error) => {
                self.warning(format!("ignoring invalid parser result {key}: {error}"));
                self.stats.misses += 1;
                None
            }
        }
    }

    fn store_entry(&mut self, owner: &VertexId, body: &str, result: &BodyResult) {
        let key = body_key(&self.namespace, owner, body);
        if result.edges.iter().any(|edge| {
            edge.evidence
                .iter()
                .any(|evidence| evidence.layer != EvidenceLayer::BodyParse)
        }) {
            self.warning(format!(
                "skipping parser cache entry with non-body evidence {key}"
            ));
            return;
        }
        let payload = CachePayload {
            key: key.clone(),
            owner: owner.as_str().to_owned(),
            body_hash: body_hash(body),
            result: BodyResultDoc::from_result(result),
        };
        let checksum = match serialized_hash(&payload) {
            Ok(checksum) => checksum,
            Err(error) => {
                self.warning(format!("cache checksum failed for {key}: {error}"));
                return;
            }
        };
        let entry = CacheFile {
            format: CACHE_FORMAT_VERSION,
            checksum,
            payload,
        };
        let bytes = match bounded_json(&entry) {
            Ok(Some(bytes)) => bytes,
            Ok(None) => {
                self.warning(format!("skipping oversized parser cache entry {key}"));
                return;
            }
            Err(error) => {
                self.warning(format!("cache serialization failed for {key}: {error}"));
                return;
            }
        };
        if let Err(error) = self.atomic_write(&key, &bytes) {
            self.warning(format!("cache write failed for {key}: {error}"));
            return;
        }
        self.stats.writes += 1;
    }

    fn entry_path(&self, key: &str) -> PathBuf {
        self.directory.join(format!("{key}{CACHE_SUFFIX}"))
    }

    fn atomic_write(&self, key: &str, bytes: &[u8]) -> io::Result<()> {
        let destination = self.entry_path(key);
        let counter = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let temporary = self
            .directory
            .join(format!(".{key}.tmp-{}-{counter}", std::process::id()));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        let mut file = options.open(&temporary)?;
        // 캐시는 손상 시 재계산하는 산출물이다. 원자적 교체·체크섬을 유지하되
        // 몸체마다 디스크 flush를 강제해 분석보다 저장이 비싸지게 만들지 않는다.
        let write_result = file.write_all(bytes);
        drop(file);
        if let Err(error) = write_result {
            cleanup_temporary(&temporary);
            return Err(error);
        }
        if let Err(error) = fs::rename(&temporary, &destination) {
            cleanup_temporary(&temporary);
            return Err(error);
        }
        Ok(())
    }

    fn warning(&mut self, message: String) {
        self.stats.warnings += 1;
        eprintln!("schemagraph cache warning: {message}");
    }
}

impl BodyCache for DiskBodyCache {
    fn load(&mut self, owner: &VertexId, body: &str) -> Option<BodyResult> {
        self.load_entry(owner, body)
    }

    fn store(&mut self, owner: &VertexId, body: &str, result: &BodyResult) {
        self.store_entry(owner, body, result)
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct CacheFile {
    format: u32,
    checksum: String,
    payload: CachePayload,
}

#[derive(Debug, Serialize, Deserialize)]
struct CachePayload {
    key: String,
    owner: String,
    body_hash: String,
    result: BodyResultDoc,
}

#[derive(Debug, Serialize, Deserialize)]
struct BodyResultDoc {
    edges: Vec<EdgeDoc>,
    origins: Vec<OriginDoc>,
    analysis: AnalysisDoc,
    notes: Vec<String>,
    enriched: usize,
}

#[derive(Debug, Serialize, Deserialize)]
struct EdgeDoc {
    from: String,
    to: String,
    kind: String,
    evidence: Vec<EvidenceDoc>,
}

#[derive(Debug, Serialize, Deserialize)]
struct EvidenceDoc {
    layer: String,
    detail: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct OriginDoc {
    from: String,
    to: String,
    kind: String,
    body_hash: String,
    role: String,
    location: Option<LocationDoc>,
}

#[derive(Debug, Serialize, Deserialize)]
struct AnalysisDoc {
    state: String,
    scope: String,
    body_hash: Option<String>,
    source: Option<String>,
    diagnostics: Vec<DiagnosticDoc>,
}

#[derive(Debug, Serialize, Deserialize)]
struct DiagnosticDoc {
    code: String,
    message: String,
    location: Option<LocationDoc>,
}

#[derive(Debug, Serialize, Deserialize)]
struct LocationDoc {
    line: u64,
    column: u64,
    end_line: u64,
    end_column: u64,
}

impl BodyResultDoc {
    fn from_result(result: &BodyResult) -> Self {
        let mut edges: Vec<_> = result.edges.iter().map(EdgeDoc::from_edge).collect();
        edges.sort_by(|left, right| {
            (&left.from, &left.to, &left.kind).cmp(&(&right.from, &right.to, &right.kind))
        });
        for edge in &mut edges {
            edge.evidence.sort_by(|left, right| {
                (&left.layer, &left.detail).cmp(&(&right.layer, &right.detail))
            });
        }
        let mut origins: Vec<_> = result
            .origins
            .iter()
            .map(|(key, origin)| OriginDoc::from_origin(key, origin))
            .collect();
        origins.sort_by(|left, right| {
            (
                &left.from,
                &left.to,
                &left.kind,
                &left.body_hash,
                &left.role,
                location_key(left.location.as_ref()),
            )
                .cmp(&(
                    &right.from,
                    &right.to,
                    &right.kind,
                    &right.body_hash,
                    &right.role,
                    location_key(right.location.as_ref()),
                ))
        });
        let mut analysis = AnalysisDoc::from_analysis(&result.analysis);
        analysis.diagnostics.sort_by(|left, right| {
            (
                &left.code,
                &left.message,
                location_key(left.location.as_ref()),
            )
                .cmp(&(
                    &right.code,
                    &right.message,
                    location_key(right.location.as_ref()),
                ))
        });
        let mut notes = result.notes.clone();
        notes.sort();
        Self {
            edges,
            origins,
            analysis,
            notes,
            enriched: result.enriched,
        }
    }

    fn into_result(self, _owner: &VertexId, body_hash: &str) -> Result<BodyResult, String> {
        let edges: Vec<Edge> = self
            .edges
            .into_iter()
            .map(EdgeDoc::into_edge)
            .collect::<Result<_, _>>()?;
        for edge in &edges {
            if edge.from.as_str().is_empty() || edge.to.as_str().is_empty() {
                return Err("edge endpoint is empty".into());
            }
            if edge
                .evidence
                .iter()
                .any(|e| e.layer != EvidenceLayer::BodyParse)
            {
                return Err("cache edge contains non-body evidence".into());
            }
        }
        let origins: Vec<_> = self
            .origins
            .into_iter()
            .map(OriginDoc::into_origin)
            .collect::<Result<_, _>>()?;
        for (key, origin) in &origins {
            if key.0.as_str().is_empty() || key.1.as_str().is_empty() {
                return Err("origin endpoint is empty".into());
            }
            if origin.body_hash != body_hash {
                return Err("origin body hash does not match cache body".into());
            }
            if !edges
                .iter()
                .any(|edge| edge.from == key.0 && edge.to == key.1 && edge.kind == key.2)
            {
                return Err("origin has no matching cached edge".into());
            }
        }
        let analysis = self.analysis.into_analysis(body_hash)?;
        Ok(BodyResult {
            edges,
            origins,
            analysis,
            notes: self.notes,
            enriched: self.enriched,
        })
    }
}

impl EdgeDoc {
    fn from_edge(edge: &Edge) -> Self {
        Self {
            from: edge.from.as_str().to_owned(),
            to: edge.to.as_str().to_owned(),
            kind: edge_kind_str(edge.kind).into(),
            evidence: edge
                .evidence
                .iter()
                .map(|evidence| EvidenceDoc {
                    layer: evidence_layer_str(evidence.layer).into(),
                    detail: evidence.detail.clone(),
                })
                .collect(),
        }
    }

    fn into_edge(self) -> Result<Edge, String> {
        let kind = edge_kind_parse(&self.kind)
            .ok_or_else(|| format!("unknown cached edge kind '{}'; cache miss", self.kind))?;
        let evidence = self
            .evidence
            .into_iter()
            .map(|evidence| {
                let layer = evidence_layer_parse(&evidence.layer).ok_or_else(|| {
                    format!(
                        "unknown cached evidence layer '{}'; cache miss",
                        evidence.layer
                    )
                })?;
                Ok(Evidence {
                    layer,
                    detail: evidence.detail,
                })
            })
            .collect::<Result<_, String>>()?;
        Ok(Edge {
            from: VertexId::from_raw(&self.from),
            to: VertexId::from_raw(&self.to),
            kind,
            evidence,
        })
    }
}

impl OriginDoc {
    fn from_origin(key: &schemagraph_core::OriginKey, origin: &Origin) -> Self {
        Self {
            from: key.0.as_str().to_owned(),
            to: key.1.as_str().to_owned(),
            kind: edge_kind_str(key.2).into(),
            body_hash: origin.body_hash.clone(),
            role: origin.role.clone(),
            location: origin.location.as_ref().map(LocationDoc::from_location),
        }
    }

    fn into_origin(self) -> Result<(schemagraph_core::OriginKey, Origin), String> {
        let kind = edge_kind_parse(&self.kind).ok_or_else(|| {
            format!(
                "unknown cached origin edge kind '{}'; cache miss",
                self.kind
            )
        })?;
        Ok((
            (
                VertexId::from_raw(&self.from),
                VertexId::from_raw(&self.to),
                kind,
            ),
            Origin {
                body_hash: self.body_hash,
                role: self.role,
                location: self.location.map(LocationDoc::into_location),
            },
        ))
    }
}

impl AnalysisDoc {
    fn from_analysis(analysis: &ObjectAnalysis) -> Self {
        Self {
            state: analysis_state_str(analysis.state).into(),
            scope: analysis.scope.clone(),
            body_hash: analysis.body_hash.clone(),
            source: analysis.source.clone(),
            diagnostics: analysis
                .diagnostics
                .iter()
                .map(DiagnosticDoc::from_diagnostic)
                .collect(),
        }
    }

    fn into_analysis(self, body_hash: &str) -> Result<ObjectAnalysis, String> {
        let state = analysis_state_parse(&self.state)?;
        if let Some(hash) = &self.body_hash {
            if hash != body_hash {
                return Err("analysis body hash does not match cache body".into());
            }
        }
        Ok(ObjectAnalysis {
            state,
            scope: self.scope,
            body_hash: self.body_hash,
            diagnostics: self
                .diagnostics
                .into_iter()
                .map(DiagnosticDoc::into_diagnostic)
                .collect(),
            source: self.source,
        })
    }
}

impl DiagnosticDoc {
    fn from_diagnostic(diagnostic: &Diagnostic) -> Self {
        Self {
            code: diagnostic.code.clone(),
            message: diagnostic.message.clone(),
            location: diagnostic.location.as_ref().map(LocationDoc::from_location),
        }
    }

    fn into_diagnostic(self) -> Diagnostic {
        Diagnostic {
            code: self.code,
            message: self.message,
            location: self.location.map(LocationDoc::into_location),
        }
    }
}

impl LocationDoc {
    fn from_location(location: &SourceLocation) -> Self {
        Self {
            line: location.line,
            column: location.column,
            end_line: location.end_line,
            end_column: location.end_column,
        }
    }

    fn into_location(self) -> SourceLocation {
        SourceLocation {
            line: self.line,
            column: self.column,
            end_line: self.end_line,
            end_column: self.end_column,
        }
    }
}

fn analysis_state_str(state: AnalysisState) -> &'static str {
    match state {
        AnalysisState::Complete => "complete",
        AnalysisState::Partial => "partial",
        AnalysisState::Unsupported => "unsupported",
    }
}

fn analysis_state_parse(value: &str) -> Result<AnalysisState, String> {
    match value {
        "complete" => Ok(AnalysisState::Complete),
        "partial" => Ok(AnalysisState::Partial),
        "unsupported" => Ok(AnalysisState::Unsupported),
        _ => Err(format!("unknown cached analysis state '{value}'")),
    }
}

fn edge_kind_str(kind: EdgeKind) -> &'static str {
    match kind {
        EdgeKind::References => "references",
        EdgeKind::Reads => "reads",
        EdgeKind::Writes => "writes",
        EdgeKind::Calls => "calls",
        EdgeKind::Fires => "fires",
        EdgeKind::UsesSequence => "uses-sequence",
        EdgeKind::UsesType => "uses-type",
        EdgeKind::Contains => "contains",
        EdgeKind::Inferred => "inferred",
        EdgeKind::DerivesFrom => "derives-from",
        EdgeKind::DependsOn => "depends-on",
    }
}

fn edge_kind_parse(value: &str) -> Option<EdgeKind> {
    Some(match value {
        "references" => EdgeKind::References,
        "reads" => EdgeKind::Reads,
        "writes" => EdgeKind::Writes,
        "calls" => EdgeKind::Calls,
        "fires" => EdgeKind::Fires,
        "uses-sequence" => EdgeKind::UsesSequence,
        "uses-type" => EdgeKind::UsesType,
        "contains" => EdgeKind::Contains,
        "inferred" => EdgeKind::Inferred,
        "derives-from" => EdgeKind::DerivesFrom,
        "depends-on" => EdgeKind::DependsOn,
        _ => return None,
    })
}

fn evidence_layer_str(layer: EvidenceLayer) -> &'static str {
    match layer {
        EvidenceLayer::Catalog => "catalog",
        EvidenceLayer::BodyParse => "body-parse",
        EvidenceLayer::Stats => "stats",
        EvidenceLayer::Inferred => "inferred",
    }
}

fn evidence_layer_parse(value: &str) -> Option<EvidenceLayer> {
    Some(match value {
        "catalog" => EvidenceLayer::Catalog,
        "body-parse" => EvidenceLayer::BodyParse,
        "stats" => EvidenceLayer::Stats,
        "inferred" => EvidenceLayer::Inferred,
        _ => return None,
    })
}

fn location_key(location: Option<&LocationDoc>) -> (u64, u64, u64, u64) {
    location
        .map(|location| {
            (
                location.line,
                location.column,
                location.end_line,
                location.end_column,
            )
        })
        .unwrap_or((0, 0, 0, 0))
}

fn body_hash(body: &str) -> String {
    format!("sha256:{:x}", Sha256::digest(body.as_bytes()))
}

fn body_key(namespace: &str, owner: &VertexId, body: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(namespace.as_bytes());
    hasher.update([0]);
    hasher.update(owner.as_str().as_bytes());
    hasher.update([0]);
    hasher.update(body.as_bytes());
    format!("{:x}", hasher.finalize())
}

fn namespace_hash(document: &CatalogDocument, executable_hash: &str) -> io::Result<String> {
    serialized_hash(&NamespaceInput {
        cache_format: CACHE_FORMAT_VERSION,
        graph_version: schemagraph_export::GRAPH_VERSION,
        executable_hash,
        document: structural_document(document),
    })
}

#[derive(Serialize)]
struct NamespaceInput<'a> {
    cache_format: u32,
    graph_version: u32,
    executable_hash: &'a str,
    document: StructuralDocument<'a>,
}

#[derive(Serialize)]
struct StructuralDocument<'a> {
    version: u32,
    dialect: &'a str,
    reader: &'a str,
    schemas: Vec<StructuralSchema<'a>>,
    limitations: &'a [String],
    context: Option<&'a CollectionContext>,
    dependencies: Vec<&'a CatalogDependency>,
}

#[derive(Serialize)]
struct StructuralSchema<'a> {
    name: &'a str,
    objects: Vec<StructuralObject<'a>>,
    routines: Vec<StructuralRoutine<'a>>,
}

#[derive(Serialize)]
struct StructuralObject<'a> {
    name: &'a str,
    kind: &'a str,
    columns: Vec<&'a ColumnDoc>,
    constraints: Vec<&'a ConstraintDoc>,
    indexes: Vec<StructuralIndex<'a>>,
    triggers: Vec<StructuralTrigger<'a>>,
}

#[derive(Serialize)]
struct StructuralIndex<'a> {
    name: &'a str,
    unique: bool,
    columns: &'a [String],
    definition_complete: Option<bool>,
    has_predicate: Option<bool>,
    predicate: Option<&'a String>,
}

#[derive(Serialize)]
struct StructuralTrigger<'a> {
    name: &'a str,
}

#[derive(Serialize)]
struct StructuralRoutine<'a> {
    name: &'a str,
    kind: &'a str,
    language: Option<&'a String>,
    signature: Option<&'a String>,
    member_of: Option<&'a String>,
    source: Option<&'a String>,
}

fn structural_document(document: &CatalogDocument) -> StructuralDocument<'_> {
    let mut schemas: Vec<_> = document.schemas.iter().map(structural_schema).collect();
    schemas.sort_by(|left, right| left.name.cmp(right.name));
    let mut dependencies: Vec<_> = document.dependencies.iter().collect();
    dependencies.sort_by(|left, right| left.cmp(right));
    StructuralDocument {
        version: document.version,
        dialect: &document.dialect,
        reader: &document.reader,
        schemas,
        limitations: &document.limitations,
        context: document.context.as_ref(),
        dependencies,
    }
}

fn structural_schema(schema: &SchemaDoc) -> StructuralSchema<'_> {
    let mut objects: Vec<_> = schema.objects.iter().map(structural_object).collect();
    objects.sort_by(|left, right| (&left.name, &left.kind).cmp(&(&right.name, &right.kind)));
    let mut routines: Vec<_> = schema.routines.iter().map(structural_routine).collect();
    routines.sort_by(|left, right| {
        (
            &left.name,
            &left.kind,
            &left.signature,
            &left.member_of,
            &left.source,
        )
            .cmp(&(
                &right.name,
                &right.kind,
                &right.signature,
                &right.member_of,
                &right.source,
            ))
    });
    StructuralSchema {
        name: &schema.name,
        objects,
        routines,
    }
}

fn structural_object(object: &ObjectDoc) -> StructuralObject<'_> {
    let mut columns: Vec<_> = object.columns.iter().collect();
    columns.sort_by(|left, right| (&left.ordinal, &left.name).cmp(&(&right.ordinal, &right.name)));
    let mut constraints: Vec<_> = object.constraints.iter().collect();
    constraints.sort_by(|left, right| (&left.name, &left.kind).cmp(&(&right.name, &right.kind)));
    let mut indexes: Vec<_> = object
        .indexes
        .iter()
        .map(|index| StructuralIndex {
            name: &index.name,
            unique: index.unique,
            columns: &index.columns,
            definition_complete: index.definition_complete,
            has_predicate: index.has_predicate,
            predicate: index.predicate.as_ref(),
        })
        .collect();
    indexes.sort_by(|left, right| left.name.cmp(right.name));
    let mut triggers: Vec<_> = object
        .triggers
        .iter()
        .map(|trigger| StructuralTrigger {
            name: &trigger.name,
        })
        .collect();
    triggers.sort_by(|left, right| left.name.cmp(right.name));
    StructuralObject {
        name: &object.name,
        kind: &object.kind,
        columns,
        constraints,
        indexes,
        triggers,
    }
}

fn structural_routine(routine: &RoutineDoc) -> StructuralRoutine<'_> {
    StructuralRoutine {
        name: &routine.name,
        kind: &routine.kind,
        language: routine.language.as_ref(),
        signature: routine.signature.as_ref(),
        member_of: routine.member_of.as_ref(),
        source: routine.source.as_ref(),
    }
}

fn cleanup_temporary(path: &Path) {
    if let Err(error) = fs::remove_file(path) {
        eprintln!(
            "schemagraph cache warning: temporary cleanup failed for '{}': {error}",
            path.display()
        );
    }
}

fn hash_file(path: &Path) -> io::Result<String> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; COPY_BUFFER_BYTES];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

fn serialized_hash<T: Serialize>(value: &T) -> io::Result<String> {
    let mut writer = HashWriter(Sha256::new());
    serde_json::to_writer(&mut writer, value)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    Ok(format!("{:x}", writer.0.finalize()))
}

struct HashWriter(Sha256);

impl Write for HashWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.update(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn bounded_json<T: Serialize>(value: &T) -> io::Result<Option<Vec<u8>>> {
    let mut writer = LimitedWriter {
        bytes: Vec::new(),
        limit: MAX_ENTRY_BYTES as usize + 1,
    };
    match serde_json::to_writer(&mut writer, value) {
        Ok(()) if writer.bytes.len() as u64 <= MAX_ENTRY_BYTES => Ok(Some(writer.bytes)),
        Ok(()) => Ok(None),
        Err(error) if error.is_io() && writer.bytes.len() as u64 > MAX_ENTRY_BYTES => Ok(None),
        Err(error) => Err(io::Error::new(io::ErrorKind::InvalidData, error)),
    }
}

struct LimitedWriter {
    bytes: Vec<u8>,
    limit: usize,
}

impl Write for LimitedWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let remaining = self.limit.saturating_sub(self.bytes.len());
        if bytes.len() > remaining {
            self.bytes.extend_from_slice(&bytes[..remaining]);
            return Err(io::Error::new(
                io::ErrorKind::Other,
                "cache entry size limit",
            ));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn read_bounded(path: &Path) -> io::Result<Option<Vec<u8>>> {
    let metadata = match fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    if metadata.len() > MAX_ENTRY_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "cache entry exceeds size limit",
        ));
    }
    let file = File::open(path)?;
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.take(MAX_ENTRY_BYTES + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_ENTRY_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "cache entry exceeds size limit",
        ));
    }
    Ok(Some(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;
    use schemagraph_core::{Edge, Evidence};
    use schemagraph_source::document::{CatalogDocument, SchemaDoc};
    use tempfile::tempdir;

    fn document(dialect: &str, table_name: &str) -> CatalogDocument {
        CatalogDocument {
            version: 1,
            dialect: dialect.into(),
            reader: "test".into(),
            schemas: vec![SchemaDoc {
                name: "public".into(),
                objects: vec![ObjectDoc {
                    name: table_name.into(),
                    kind: "table".into(),
                    columns: vec![ColumnDoc {
                        name: "id".into(),
                        data_type: "integer".into(),
                        nullable: false,
                        default: None,
                        ordinal: 1,
                        pk_position: 1,
                    }],
                    constraints: vec![],
                    indexes: vec![],
                    triggers: vec![],
                    body: None,
                    usage: None,
                }],
                routines: vec![],
            }],
            limitations: vec![],
            context: None,
            dependencies: vec![],
        }
    }

    fn result(owner: &VertexId, body: &str, target: &str) -> BodyResult {
        let hash = body_hash(body);
        let target = VertexId::from_raw(target);
        BodyResult {
            edges: vec![Edge {
                from: owner.clone(),
                to: target.clone(),
                kind: EdgeKind::Reads,
                evidence: vec![Evidence {
                    layer: EvidenceLayer::BodyParse,
                    detail: "test body edge".into(),
                }],
            }],
            origins: vec![(
                (owner.clone(), target, EdgeKind::Reads),
                Origin {
                    body_hash: hash.clone(),
                    role: "relation".into(),
                    location: Some(SourceLocation {
                        line: 1,
                        column: 1,
                        end_line: 1,
                        end_column: 8,
                    }),
                },
            )],
            analysis: ObjectAnalysis {
                state: AnalysisState::Complete,
                scope: "object-dependencies".into(),
                body_hash: Some(hash),
                diagnostics: vec![],
                source: Some("queries/view.sql".into()),
            },
            notes: vec!["test note".into()],
            enriched: 1,
        }
    }

    fn cache(directory: &Path, doc: &CatalogDocument, fingerprint: &str) -> DiskBodyCache {
        DiskBodyCache::new_with_fingerprint(directory, doc, fingerprint).unwrap()
    }

    #[test]
    fn unchanged_body_hits_and_roundtrips_parser_effects_without_sql() {
        let directory = tempdir().unwrap();
        let doc = document("postgres", "orders");
        let owner = VertexId::object("public", "view");
        let body = "SELECT id FROM orders /* secret body */";
        let parser_result = result(&owner, body, "public.orders");
        let mut writer = cache(directory.path(), &doc, "exe-a");
        writer.store(&owner, body, &parser_result);
        let entry = fs::read_dir(directory.path())
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        let persisted = fs::read_to_string(entry).unwrap();
        assert!(!persisted.contains(body));

        let mut reader = cache(directory.path(), &doc, "exe-a");
        let loaded = reader.load(&owner, body).unwrap();
        assert_eq!(loaded.edges, parser_result.edges);
        assert_eq!(loaded.origins, parser_result.origins);
        assert_eq!(loaded.analysis, parser_result.analysis);
        assert_eq!(reader.stats().hits, 1);
        assert_eq!(reader.stats().misses, 0);
    }

    #[test]
    fn changed_schema_or_dialect_misses() {
        let directory = tempdir().unwrap();
        let owner = VertexId::object("public", "view");
        let body = "SELECT id FROM orders";
        let mut writer = cache(directory.path(), &document("postgres", "orders"), "exe-a");
        writer.store(&owner, body, &result(&owner, body, "public.orders"));
        assert!(cache(
            directory.path(),
            &document("postgres", "customers"),
            "exe-a"
        )
        .load(&owner, body)
        .is_none());
        assert!(
            cache(directory.path(), &document("mysql", "orders"), "exe-a")
                .load(&owner, body)
                .is_none()
        );
    }

    #[test]
    fn changing_one_body_does_not_invalidate_another() {
        let directory = tempdir().unwrap();
        let doc = document("postgres", "orders");
        let first = VertexId::object("public", "first");
        let second = VertexId::object("public", "second");
        let mut writer = cache(directory.path(), &doc, "exe-a");
        writer.store(
            &first,
            "SELECT 1",
            &result(&first, "SELECT 1", "public.orders"),
        );
        writer.store(
            &second,
            "SELECT 2",
            &result(&second, "SELECT 2", "public.orders"),
        );
        let mut reader = cache(directory.path(), &doc, "exe-a");
        assert!(reader.load(&first, "SELECT 1").is_some());
        assert!(reader.load(&second, "SELECT changed").is_none());
    }

    #[test]
    fn corrupt_entry_is_a_warning_and_miss() {
        let directory = tempdir().unwrap();
        let doc = document("postgres", "orders");
        let owner = VertexId::object("public", "view");
        let body = "SELECT id FROM orders";
        let mut writer = cache(directory.path(), &doc, "exe-a");
        writer.store(&owner, body, &result(&owner, body, "public.orders"));
        let path = fs::read_dir(directory.path())
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        fs::write(path, b"{truncated").unwrap();
        let mut reader = cache(directory.path(), &doc, "exe-a");
        assert!(reader.load(&owner, body).is_none());
        assert_eq!(reader.stats().warnings, 1);
    }

    #[test]
    fn oversized_entry_is_a_warning_and_miss() {
        let directory = tempdir().unwrap();
        let doc = document("postgres", "orders");
        let owner = VertexId::object("public", "view");
        let body = "SELECT id FROM orders";
        let writer = cache(directory.path(), &doc, "exe-a");
        let key = body_key(&writer.namespace, &owner, body);
        fs::write(
            writer.entry_path(&key),
            vec![b'x'; MAX_ENTRY_BYTES as usize + 1],
        )
        .unwrap();
        let mut reader = cache(directory.path(), &doc, "exe-a");
        assert!(reader.load(&owner, body).is_none());
        assert_eq!(reader.stats().warnings, 1);
    }

    #[test]
    fn executable_identity_changes_namespace() {
        let directory = tempdir().unwrap();
        let doc = document("postgres", "orders");
        let owner = VertexId::object("public", "view");
        let body = "SELECT id FROM orders";
        let mut writer = cache(directory.path(), &doc, "exe-a");
        writer.store(&owner, body, &result(&owner, body, "public.orders"));
        assert!(cache(directory.path(), &doc, "exe-b")
            .load(&owner, body)
            .is_none());
    }
}
