//! 신뢰할 수 있는 로컬 SQL 몸체 분석 캐시.
//!
//! 캐시는 parser 효과만 저장한다. 카탈로그·usage는 매 실행마다 새로 만들고,
//! SQL 원문은 namespace 계산과 entry 저장 어디에도 복사하지 않는다.
//! 캐시 디렉터리는 신뢰 입력이고, 항목 payload는 사용자별 비밀 키의 HMAC-SHA256
//! 태그로 인증한다. 신뢰를 세우지 못하면 캐시를 끈다([`trust`]).

mod trust;

use std::collections::BTreeSet;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use hmac::Mac;
use schemagraph_core::{
    AnalysisState, Diagnostic, Edge, EdgeKind, Evidence, EvidenceLayer, ObjectAnalysis, Origin,
    SourceLocation, VertexId,
};
use schemagraph_parser::{BodyCache, BodyResult, CacheContext};
use schemagraph_source::document::{
    CatalogDependency, CatalogDocument, CollectionContext, ColumnDoc, ConstraintDoc, ObjectDoc,
    RoutineDoc, SchemaDoc,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// 4: payload를 키 없는 SHA-256 `checksum` 대신 사용자별 비밀 키의 HMAC-SHA256 `tag`로
/// 인증한다. 공개된 재료(실행 파일·문서)만으로 다시 계산한 위조 항목이 hit가 되지 않게
/// 하기 위해서다. 3 이하의 항목은 namespace가 달라 읽지 않으므로 한 번 miss가 나고 다시 쓴다.
/// (3: notes를 정렬하지 않고 parser 순서로 저장한다.)
const CACHE_FORMAT_VERSION: u32 = 4;
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
    scoped_namespace: String,
    /// 항목 태그 키. None이면 신뢰 경계를 세우지 못해 꺼진 캐시다. load는 항상 miss,
    /// store는 쓰지 않는다.
    key: Option<trust::CacheKey>,
    stats: CacheStats,
}

impl DiskBodyCache {
    pub(crate) fn new(directory: &Path, document: &CatalogDocument) -> io::Result<Self> {
        let executable = std::env::current_exe()?;
        let executable_hash = hash_file(&executable)?;
        Self::new_with_fingerprint(directory, document, &executable_hash, || {
            trust::resolve_key(&trust::KeyEnvironment::from_process())
        })
    }

    /// `key`는 디렉터리를 믿을 수 있을 때만 부른다(키 파일을 만들 수 있다).
    fn new_with_fingerprint(
        directory: &Path,
        document: &CatalogDocument,
        executable_hash: &str,
        key: impl FnOnce() -> Result<trust::CacheKey, String>,
    ) -> io::Result<Self> {
        let untrusted = trust::prepare_directory(directory)?;
        let namespace = namespace_hash(document, executable_hash)?;
        let scoped_namespace = scoped_namespace_hash(document, executable_hash)?;
        let mut cache = Self {
            // 신뢰 검사가 본 경로(끝 `/`·`.`를 지운 경로) 아래에 항목을 둔다.
            directory: trust::normalized_directory(directory),
            namespace,
            scoped_namespace,
            key: None,
            stats: CacheStats::default(),
        };
        let key = match untrusted {
            Some(reason) => Err(reason),
            None => key(),
        };
        match key {
            Ok(key) => cache.key = Some(key),
            Err(reason) => cache.warning(format!("SQL analysis cache disabled: {reason}")),
        }
        Ok(cache)
    }

    pub(crate) fn stats(&self) -> CacheStats {
        self.stats
    }

    fn load_entry(
        &mut self,
        owner: &VertexId,
        body: &str,
        context: Option<&CacheContext>,
    ) -> Option<BodyResult> {
        if self.key.is_none() {
            self.stats.misses += 1;
            return None;
        }
        let key = context
            .map(|context| scoped_body_key(&self.scoped_namespace, owner, body, context))
            .unwrap_or_else(|| body_key(&self.namespace, owner, body));
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
        match self.authenticate(&entry) {
            Ok(true) => {}
            Ok(false) => {
                self.warning(format!(
                    "ignoring cache entry with invalid authentication tag {key}; it was forged, corrupted, or written with another cache key"
                ));
                self.stats.misses += 1;
                return None;
            }
            Err(error) => {
                self.warning(format!(
                    "ignoring cache entry {key}: authentication failed: {error}"
                ));
                self.stats.misses += 1;
                return None;
            }
        }
        let expected_body_hash = body_hash(body);
        if entry.payload.owner != owner.as_str() || entry.payload.body_hash != expected_body_hash {
            self.warning(format!(
                "ignoring cache entry with invalid body identity {key}"
            ));
            self.stats.misses += 1;
            return None;
        }
        if let Some(context) = context {
            if entry.payload.footprint.as_deref() != Some(context.fingerprint.as_str()) {
                self.stats.misses += 1;
                return None;
            }
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

    fn store_entry(
        &mut self,
        owner: &VertexId,
        body: &str,
        result: &BodyResult,
        context: Option<&CacheContext>,
    ) {
        if self.key.is_none() {
            return;
        }
        let key = context
            .map(|context| scoped_body_key(&self.scoped_namespace, owner, body, context))
            .unwrap_or_else(|| body_key(&self.namespace, owner, body));
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
            footprint: context.map(|context| context.fingerprint.clone()),
            result: BodyResultDoc::from_result(result),
        };
        let tag = match self
            .key
            .as_ref()
            .map(|cache_key| payload_tag(cache_key, &payload))
        {
            Some(Ok(tag)) => tag,
            Some(Err(error)) => {
                self.warning(format!(
                    "cache authentication tag failed for {key}: {error}"
                ));
                return;
            }
            None => return,
        };
        let entry = CacheFile {
            format: CACHE_FORMAT_VERSION,
            tag,
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

    /// payload 태그를 상수 시간으로 확인한다. 꺼진 캐시는 어떤 항목도 믿지 않는다.
    fn authenticate(&self, entry: &CacheFile) -> io::Result<bool> {
        let (Some(cache_key), Some(tag)) = (&self.key, decode_hex(&entry.tag)) else {
            return Ok(false);
        };
        Ok(payload_mac(cache_key, &entry.payload)?
            .verify_slice(&tag)
            .is_ok())
    }
}

impl BodyCache for DiskBodyCache {
    fn load(&mut self, owner: &VertexId, body: &str) -> Option<BodyResult> {
        self.load_entry(owner, body, None)
    }

    fn store(&mut self, owner: &VertexId, body: &str, result: &BodyResult) {
        self.store_entry(owner, body, result, None)
    }

    fn load_scoped(
        &mut self,
        owner: &VertexId,
        body: &str,
        context: &CacheContext,
    ) -> Option<BodyResult> {
        if !context.trusted {
            return self.load_entry(owner, body, None);
        }
        self.load_entry(owner, body, Some(context))
    }

    fn store_scoped(
        &mut self,
        owner: &VertexId,
        body: &str,
        result: &BodyResult,
        context: &CacheContext,
    ) {
        if !context.trusted {
            self.store_entry(owner, body, result, None);
            return;
        }
        self.store_entry(owner, body, result, Some(context))
    }

    fn record_restore_failure(&mut self) {
        self.stats.hits = self.stats.hits.saturating_sub(1);
        self.stats.misses += 1;
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct CacheFile {
    format: u32,
    /// payload 직렬화 바이트의 HMAC-SHA256(소문자 hex). 키 없이는 만들 수 없다.
    tag: String,
    payload: CachePayload,
}

#[derive(Debug, Serialize, Deserialize)]
struct CachePayload {
    key: String,
    owner: String,
    body_hash: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    footprint: Option<String>,
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
        // notes는 hit 때 그대로 graph.json limitations에 이어 붙는다. 캐시 없는 실행과
        // 같은 바이트가 되도록 정렬하지 않고 parser가 낸 순서를 저장한다.
        Self {
            edges,
            origins,
            analysis,
            notes: result.notes.clone(),
            enriched: result.enriched,
        }
    }

    fn into_result(self, owner: &VertexId, body_hash: &str) -> Result<BodyResult, String> {
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
        // origin마다 간선 목록을 훑으면 넓은 몸체의 hit가 다시 파싱하는 것보다 느려진다.
        let edge_keys: BTreeSet<_> = edges
            .iter()
            .map(|edge| (&edge.from, &edge.to, edge.kind))
            .collect();
        for (key, origin) in &origins {
            if key.0.as_str().is_empty() || key.1.as_str().is_empty() {
                return Err("origin endpoint is empty".into());
            }
            if origin.body_hash != body_hash {
                return Err("origin body hash does not match cache body".into());
            }
            if !edge_keys.contains(&(&key.0, &key.1, key.2)) {
                return Err("origin has no matching cached edge".into());
            }
        }
        let analysis = self.analysis.into_analysis(body_hash)?;
        Ok(BodyResult {
            owner: owner.clone(),
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

fn scoped_body_key(
    namespace: &str,
    owner: &VertexId,
    body: &str,
    context: &CacheContext,
) -> String {
    let mut hasher = Sha256::new();
    hasher.update(namespace.as_bytes());
    hasher.update([0]);
    hasher.update(owner.as_str().as_bytes());
    hasher.update([0]);
    hasher.update(body.as_bytes());
    hasher.update([0]);
    hasher.update(context.fingerprint.as_bytes());
    hasher.update([0]);
    hasher.update([u8::from(context.trusted)]);
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

fn scoped_namespace_hash(document: &CatalogDocument, executable_hash: &str) -> io::Result<String> {
    serialized_hash(&ScopedNamespaceInput {
        cache_format: CACHE_FORMAT_VERSION,
        graph_version: schemagraph_export::GRAPH_VERSION,
        executable_hash,
        dialect: &document.dialect,
        reader: &document.reader,
        context: document.context.as_ref(),
    })
}

#[derive(Serialize)]
struct ScopedNamespaceInput<'a> {
    cache_format: u32,
    graph_version: u32,
    executable_hash: &'a str,
    dialect: &'a str,
    reader: &'a str,
    context: Option<&'a CollectionContext>,
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

/// payload를 serde 압축 직렬화 그대로 HMAC에 흘려 넣는다. 저장과 확인이 같은 구조체를 같은
/// 방식으로 직렬화하므로 바이트가 같다. 모르는 필드는 역직렬화에서 버려져 태그 재료에도
/// 들지 않는다.
fn payload_mac(
    cache_key: &trust::CacheKey,
    payload: &CachePayload,
) -> io::Result<trust::HmacSha256> {
    let mut writer = MacWriter(cache_key.mac());
    serde_json::to_writer(&mut writer, payload)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    Ok(writer.0)
}

fn payload_tag(cache_key: &trust::CacheKey, payload: &CachePayload) -> io::Result<String> {
    Ok(format!(
        "{:x}",
        payload_mac(cache_key, payload)?.finalize().into_bytes()
    ))
}

/// hex 숫자만 받는다. `u8::from_str_radix`는 `+` 부호를 받아 같은 태그의 다른 표기를 허용한다.
fn decode_hex(text: &str) -> Option<Vec<u8>> {
    if !text.len().is_multiple_of(2) || !text.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    text.as_bytes()
        .chunks(2)
        .map(|pair| {
            std::str::from_utf8(pair)
                .ok()
                .and_then(|pair| u8::from_str_radix(pair, 16).ok())
        })
        .collect()
}

struct MacWriter(trust::HmacSha256);

impl Write for MacWriter {
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

/// 항목을 링크를 따르지 않고(O_NOFOLLOW), FIFO에서 멈추지 않고(O_NONBLOCK), 같은 fd의
/// fstat으로 정규 파일·크기 상한을 확인해 읽는다. 없는 항목만 조용한 miss(`Ok(None)`)이고
/// 링크·특수 파일·상한 초과는 오류라 호출자가 경고와 함께 miss로 센다.
fn read_bounded(path: &Path) -> io::Result<Option<Vec<u8>>> {
    match schemagraph_source::imports::read_bounded_regular_file(path, MAX_ENTRY_BYTES) {
        Ok((bytes, _)) => Ok(Some(bytes)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
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
            owner: owner.clone(),
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

    /// 테스트 전용 가짜 키(32바이트 이상). 실제 비밀값이 아니다.
    const TEST_KEY: &[u8] = b"PLACEHOLDER-unit-test-cache-key-first";

    fn cache(directory: &Path, doc: &CatalogDocument, fingerprint: &str) -> DiskBodyCache {
        cache_with_key(directory, doc, fingerprint, TEST_KEY)
    }

    fn cache_with_key(
        directory: &Path,
        doc: &CatalogDocument,
        fingerprint: &str,
        key: &[u8],
    ) -> DiskBodyCache {
        DiskBodyCache::new_with_fingerprint(directory, doc, fingerprint, || {
            trust::CacheKey::new(key)
        })
        .unwrap()
    }

    fn only_entry(directory: &Path) -> PathBuf {
        let mut entries = fs::read_dir(directory).unwrap();
        let entry = entries.next().unwrap().unwrap().path();
        assert!(entries.next().is_none());
        entry
    }

    #[test]
    fn entry_written_with_another_key_is_a_warning_and_miss() {
        let directory = tempdir().unwrap();
        let doc = document("postgres", "orders");
        let owner = VertexId::object("public", "view");
        let body = "SELECT id FROM orders";
        cache(directory.path(), &doc, "exe-a").store(
            &owner,
            body,
            &result(&owner, body, "public.orders"),
        );
        let second_key = b"PLACEHOLDER-unit-test-cache-key-second";
        let mut reader = cache_with_key(directory.path(), &doc, "exe-a", second_key);
        assert!(reader.load(&owner, body).is_none());
        assert_eq!(
            (
                reader.stats().hits,
                reader.stats().misses,
                reader.stats().warnings
            ),
            (0, 1, 1)
        );
        // 같은 키로는 그대로 hit다.
        assert!(cache(directory.path(), &doc, "exe-a")
            .load(&owner, body)
            .is_some());
    }

    #[test]
    fn tampered_payload_or_missing_tag_is_a_warning_and_miss() {
        let owner = VertexId::object("public", "view");
        let body = "SELECT id FROM orders";
        let doc = document("postgres", "orders");
        let edits: [fn(String) -> String; 3] = [
            // 태그는 그대로 두고 간선 대상만 바꾼다.
            |entry| entry.replace("\"to\":\"public.orders\"", "\"to\":\"public.view\""),
            // 태그 필드를 지운다(형식 4는 태그가 필수다).
            |entry| {
                let start = entry.find(",\"tag\":").unwrap();
                let end = entry.find(",\"payload\":").unwrap();
                format!("{}{}", &entry[..start], &entry[end..])
            },
            // hex가 아닌 태그(부호 붙은 hex 포함)는 비교하지 않고 거절한다.
            |entry| {
                let start = entry.find("\"tag\":\"").unwrap() + 7;
                format!("{}+{}", &entry[..start], &entry[start + 1..])
            },
        ];
        for edit in edits {
            let directory = tempdir().unwrap();
            cache(directory.path(), &doc, "exe-a").store(
                &owner,
                body,
                &result(&owner, body, "public.orders"),
            );
            let path = only_entry(directory.path());
            fs::write(&path, edit(fs::read_to_string(&path).unwrap())).unwrap();
            let mut reader = cache(directory.path(), &doc, "exe-a");
            assert!(reader.load(&owner, body).is_none());
            assert_eq!((reader.stats().misses, reader.stats().warnings), (1, 1));
        }
    }

    #[test]
    fn disabled_cache_counts_misses_without_reading_or_writing() {
        let directory = tempdir().unwrap();
        let doc = document("postgres", "orders");
        let owner = VertexId::object("public", "view");
        let body = "SELECT id FROM orders";
        let mut disabled =
            DiskBodyCache::new_with_fingerprint(directory.path(), &doc, "exe-a", || {
                Err("no cache key in this test".into())
            })
            .unwrap();
        assert_eq!(disabled.stats().warnings, 1);
        disabled.store(&owner, body, &result(&owner, body, "public.orders"));
        assert!(disabled.load(&owner, body).is_none());
        assert_eq!(
            (
                disabled.stats().hits,
                disabled.stats().misses,
                disabled.stats().writes
            ),
            (0, 1, 0)
        );
        assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 0);
    }

    #[test]
    fn decode_hex_accepts_only_pairs_of_hex_digits() {
        assert_eq!(decode_hex("00ff7A"), Some(vec![0x00, 0xff, 0x7a]));
        for invalid in ["0", "+f", "-1", "zz", "0x", " 0", "é0"] {
            assert_eq!(decode_hex(invalid), None, "{invalid}");
        }
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
    fn notes_keep_their_parser_order_through_the_disk_cache() {
        let directory = tempdir().unwrap();
        let doc = document("postgres", "orders");
        let owner = VertexId::object("public", "view");
        let body = "SELECT id FROM orders";
        let mut parser_result = result(&owner, body, "public.orders");
        // notes는 graph.json limitations에 이 순서 그대로 실린다. 캐시 없는 실행과
        // 같은 바이트가 되려면 hit도 생성 순서를 돌려줘야 한다.
        parser_result.notes = vec!["z: written first".into(), "a: read second".into()];
        cache(directory.path(), &doc, "exe-a").store(&owner, body, &parser_result);
        let loaded = cache(directory.path(), &doc, "exe-a")
            .load(&owner, body)
            .unwrap();
        assert_eq!(loaded.notes, parser_result.notes);
    }

    #[test]
    fn cached_origin_without_its_edge_is_a_warning_and_miss() {
        let directory = tempdir().unwrap();
        let doc = document("postgres", "orders");
        let owner = VertexId::object("public", "view");
        let body = "SELECT id FROM orders";
        let mut parser_result = result(&owner, body, "public.orders");
        // origin마다 같은 키의 캐시 간선이 있어야 복원한 근거가 유령 간선을 가리키지 않는다.
        let (key, _) = &mut parser_result.origins[0];
        key.2 = EdgeKind::Writes;
        cache(directory.path(), &doc, "exe-a").store(&owner, body, &parser_result);
        let mut reader = cache(directory.path(), &doc, "exe-a");
        assert!(reader.load(&owner, body).is_none());
        assert_eq!(
            (
                reader.stats().hits,
                reader.stats().misses,
                reader.stats().warnings
            ),
            (0, 1, 1)
        );
    }

    #[test]
    fn scoped_footprint_survives_unrelated_document_shape_change() {
        let directory = tempdir().unwrap();
        let owner = VertexId::object("public", "view");
        let body = "SELECT id FROM orders";
        let context = CacheContext {
            fingerprint: "orders-shape-v1".into(),
            trusted: true,
        };
        let mut writer = cache(directory.path(), &document("postgres", "orders"), "exe-a");
        writer.store_scoped(
            &owner,
            body,
            &result(&owner, body, "public.orders"),
            &context,
        );
        // scoped namespace에는 전역 수집 문맥만 두고, 관계 모양은 몸체별
        // footprint가 확인하므로 무관한 테이블 추가는 hit를 유지한다.
        let mut reader = cache(
            directory.path(),
            &document("postgres", "customers"),
            "exe-a",
        );
        assert!(reader.load_scoped(&owner, body, &context).is_some());
        assert_eq!(reader.stats().hits, 1);
        assert_eq!(reader.stats().misses, 0);
    }

    #[test]
    fn scoped_footprint_change_is_a_real_miss() {
        let directory = tempdir().unwrap();
        let doc = document("postgres", "orders");
        let owner = VertexId::object("public", "view");
        let body = "SELECT id FROM orders";
        let first = CacheContext {
            fingerprint: "orders-shape-v1".into(),
            trusted: true,
        };
        let changed = CacheContext {
            fingerprint: "orders-shape-v2".into(),
            trusted: true,
        };
        let mut writer = cache(directory.path(), &doc, "exe-a");
        writer.store_scoped(&owner, body, &result(&owner, body, "public.orders"), &first);
        let mut reader = cache(directory.path(), &doc, "exe-a");
        assert!(reader.load_scoped(&owner, body, &changed).is_none());
        assert_eq!(reader.stats().hits, 0);
        assert_eq!(reader.stats().misses, 1);
    }

    #[test]
    fn untrusted_scoped_context_uses_the_full_document_namespace() {
        let directory = tempdir().unwrap();
        let doc = document("postgres", "orders");
        let owner = VertexId::object("public", "view");
        let body = "SELECT id FROM orders";
        let context = CacheContext::conservative("fallback");
        let mut body_cache = cache(directory.path(), &doc, "exe-a");
        body_cache.store_scoped(
            &owner,
            body,
            &result(&owner, body, "public.orders"),
            &context,
        );
        assert!(body_cache.load_scoped(&owner, body, &context).is_some());
        assert_eq!(body_cache.stats().hits, 1);
        assert_eq!(body_cache.stats().misses, 0);

        let changed = document("postgres", "customers");
        let mut changed_cache = cache(directory.path(), &changed, "exe-a");
        assert!(changed_cache.load_scoped(&owner, body, &context).is_none());
        assert_eq!(changed_cache.stats().misses, 1);
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
