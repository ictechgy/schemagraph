//! schemagraph-parser — SQL 몸체를 파싱해 의존 간선을 만든다.
//!
//! reader는 원문만 옮기고, 몸체의 의미는 여기서 해석한다 — 그래프 의미론의
//! 권위는 엔진 한 곳에 있다(DESIGN.md "파싱은 단일 소스"). 파싱 실패는
//! 숨기지 않고 limitation으로 실측해 돌려준다.
//!
//! 뷰는 스키마·스코프별로 컬럼을 해석하고, 절차형 몸체는 지원하는 SQL 문장을
//! 추출한다. 값의 계보와 조건절의 읽기 의존성을 구분하고 미해석은 진단한다.

use std::cell::OnceCell;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::ops::ControlFlow;

use schemagraph_core::{Edge, EdgeKind, Evidence, EvidenceLayer, Graph, VertexId, VertexKind};
use schemagraph_source::document::{CatalogDocument, ObjectDoc, RoutineDoc};
use sqlparser::ast::{
    visit_expressions, visit_statements, Expr, ObjectName, Statement, TableFactor, TableObject,
    TableWithJoins, UpdateTableFromKind,
};
use sqlparser::dialect::{
    Dialect, GenericDialect, MsSqlDialect, MySqlDialect, PostgreSqlDialect, SQLiteDialect,
};

mod body_relations;
mod cache;
mod constant_sql;
mod defaults;
mod parse_limits;
pub use cache::{BodyCache, BodyResult, CacheContext};
mod scope;
#[cfg(test)]
mod scope_tests;
mod sqlite;

/// document의 몸체들을 파싱해 그래프에 간선을 보강한다.
///
/// 반환값은 (간선을 만든 객체 수, 실패·주의로 limitation에 실을 메시지들).
/// 호출자가 limitations를 그래프에 싣는다 — 여기서 직접 싣지 않는 이유는
/// reader가 이미 싣은 한계와 순서를 섞지 않기 위해서다.
///
/// 문장마다 연산 사슬 길이에 상한을 두고 넘는 문장은 파싱하지 않지만, 상한 안의
/// 사슬도 분석 재귀가 release(aarch64) 기준 최대 166MiB 스택을 쓸 수 있다. 신뢰할 수
/// 없는 SQL을 다루는 호출자는 그만한 스택의 스레드에서 부른다(CLI는 512MiB).
pub fn enrich_from_document(g: &mut Graph, doc: &CatalogDocument) -> (usize, Vec<String>) {
    enrich_with_cache(g, doc, None)
}

/// 현재 카탈로그에 유효한 몸체 결과만 재사용해 구조·통계 갱신을 보존한다.
pub fn enrich_with_cache(
    g: &mut Graph,
    doc: &CatalogDocument,
    mut cache: Option<&mut dyn BodyCache>,
) -> (usize, Vec<String>) {
    let dialect = dialect_for(&doc.dialect);
    // Oracle의 미인용 식별자는 대문자로 접힌다 — 카탈로그는 ORDERS인데 몸체는
    // orders라 정확 일치가 없다. 대소문자 구분 방언(PG 등)에 켜면 다른
    // 객체를 잘못 가리키므로 접힘 의미론이 확실한 방언에만 켠다.
    let ci = matches!(doc.dialect.as_str(), "oracle" | "db2");
    let format_shadowed = matches!(doc.dialect.as_str(), "postgres" | "postgresql")
        && doc
            .schemas
            .iter()
            .flat_map(|schema| &schema.routines)
            .any(|routine| routine.name.eq_ignore_ascii_case("format"));
    let catalog_index = scope::CatalogIndex::new(doc);
    let mut enriched = 0usize;
    let mut unavailable = 0usize;
    let mut notes: Vec<String> = Vec::new();

    // 컬럼 기본값은 routine/view 몸체 캐시의 소유가 아니다. 현재 카탈로그에서 매번 읽는다.
    defaults::enrich(g, doc, &catalog_index, dialect.as_deref(), ci, &mut notes);

    for schema in &doc.schemas {
        for obj in &schema.objects {
            if matches!(obj.kind.as_str(), "view" | "materialized-view") {
                let kind = if obj.kind == "view" {
                    VertexKind::View
                } else {
                    VertexKind::MaterializedView
                };
                if let Some(owner) = resolve_object(
                    g,
                    &schema.name,
                    &obj.name,
                    kind,
                    &obj.kind,
                    catalog_index.lookup(false),
                ) {
                    // 캐시 키는 재사용·저장에만 쓰인다. 캐시가 없으면 만들지 않는다.
                    let context = obj
                        .body
                        .as_deref()
                        .filter(|_| cache.is_some())
                        .map(|body| catalog_index.cache_context(g, &owner, body));
                    if !cache::reuse(
                        &mut cache,
                        g,
                        &owner,
                        obj.body.as_deref(),
                        context.as_ref(),
                        &mut notes,
                        &mut enriched,
                    ) {
                        let note_start = notes.len();
                        let enriched_start = enriched;
                        notes.extend(scope::enrich_view(
                            g,
                            &catalog_index,
                            &schema.name,
                            obj,
                            &owner,
                            dialect.as_deref(),
                        ));
                        enriched += usize::from(obj.body.is_some());
                        cache::save(
                            &mut cache,
                            g,
                            &owner,
                            obj.body.as_deref(),
                            context.as_ref(),
                            cache::BodyOutcome {
                                notes: &notes[note_start..],
                                enriched: enriched - enriched_start,
                                dml_origins: catalog_index.take_dml_origins(&owner),
                            },
                        );
                    }
                }
            }
            for trg in &obj.triggers {
                // document_to_graph가 멤버 이름 충돌을 `name@kind`로 분리한다 —
                // 같은 규칙으로 실제 정점 id를 찾고, 못 찾으면 유령 간선 대신
                // notes로 남긴다.
                let Some(trigger_id) = resolve_member(
                    g,
                    &schema.name,
                    &obj.name,
                    &trg.name,
                    VertexKind::Trigger,
                    "trigger",
                    catalog_index.lookup(ci),
                ) else {
                    notes.push(format!(
                        "trigger {}.{}.{}: 정점을 못 찾음(이름 충돌?) — 몸체 간선 생략",
                        schema.name, obj.name, trg.name
                    ));
                    continue;
                };
                let Some(body) = trg.body.as_ref().filter(|b| !b.trim().is_empty()) else {
                    unavailable += 1;
                    record_body(
                        g,
                        &trigger_id,
                        None,
                        schemagraph_core::AnalysisState::Unsupported,
                        "SG_BODY_UNAVAILABLE",
                        &["trigger definition was not collected".into()],
                        &[],
                    );
                    continue;
                };
                let context = cache
                    .is_some()
                    .then(|| catalog_index.cache_context(g, &trigger_id, body));
                if cache::reuse(
                    &mut cache,
                    g,
                    &trigger_id,
                    Some(body),
                    context.as_ref(),
                    &mut notes,
                    &mut enriched,
                ) {
                    continue;
                }
                let note_start = notes.len();
                let enriched_start = enriched;
                let mut state = schemagraph_core::AnalysisState::Complete;
                let mut record_body_diagnostics = Vec::new();
                let parsed_body = match doc.dialect.as_str() {
                    "db2" => parse_db2_trigger_body(dialect.as_deref(), body),
                    "informix" => parse_informix_trigger_body(dialect.as_deref(), body),
                    _ => parse_trigger_body(
                        dialect.as_deref(),
                        body,
                        doc.dialect == "sqlserver" || doc.dialect == "mssql",
                        doc.dialect == "sqlite",
                    ),
                };
                match parsed_body {
                    Ok(mut parsed) => {
                        apply_trigger(
                            g,
                            BodyContext {
                                catalog: &catalog_index,
                                schema: &schema.name,
                                ci,
                                body_hash: scope::body_hash(body),
                                sqlserver_trigger: doc.dialect == "sqlserver"
                                    || doc.dialect == "mssql",
                            },
                            &obj.name,
                            &trigger_id,
                            &mut parsed,
                            &mut notes,
                        );
                        if parsed.partial {
                            state = schemagraph_core::AnalysisState::Partial;
                        }
                        let diagnostics = parsed.diagnostics.clone();
                        enriched += 1;
                        record_body_diagnostics = diagnostics;
                    }
                    Err(msg) => {
                        state = schemagraph_core::AnalysisState::Unsupported;
                        notes.push(format!("trigger {trigger_id} body parse failed: {msg}"));
                    }
                }
                record_body(
                    g,
                    &trigger_id,
                    Some(body),
                    state,
                    "SG_BODY_PARTIAL",
                    &notes[note_start..],
                    &record_body_diagnostics,
                );
                cache::save(
                    &mut cache,
                    g,
                    &trigger_id,
                    Some(body),
                    context.as_ref(),
                    cache::BodyOutcome {
                        notes: &notes[note_start..],
                        enriched: enriched - enriched_start,
                        dml_origins: catalog_index.take_dml_origins(&trigger_id),
                    },
                );
            }
        }
        for routine in &schema.routines {
            let (kind, suffix) = match routine.kind.as_str() {
                "procedure" => (VertexKind::Procedure, "procedure"),
                "package" => (VertexKind::Package, "package"),
                "query" => (VertexKind::Query, "query"),
                _ => (VertexKind::Function, "function"),
            };
            // 패키지 멤버는 schema.pkg.member 정점 — schema 직속이 아니다.
            let owner = resolve_routine_vertex(
                g,
                &schema.name,
                routine,
                kind,
                suffix,
                catalog_index.lookup(ci),
            );
            let Some(owner) = owner else {
                notes.push(format!(
                    "routine {}.{}: 정점을 못 찾음 — 몸체 간선 생략",
                    schema.name, routine.name
                ));
                continue;
            };
            let Some(body) = routine
                .body
                .as_ref()
                .filter(|b| routine.kind == "query" || !b.trim().is_empty())
            else {
                unavailable += 1;
                record_body(
                    g,
                    &owner,
                    None,
                    schemagraph_core::AnalysisState::Unsupported,
                    "SG_BODY_UNAVAILABLE",
                    &["routine definition was not collected".into()],
                    &[],
                );
                continue;
            };
            let context = cache
                .is_some()
                .then(|| catalog_index.cache_context(g, &owner, body));
            if cache::reuse(
                &mut cache,
                g,
                &owner,
                Some(body),
                context.as_ref(),
                &mut notes,
                &mut enriched,
            ) {
                continue;
            }
            let note_start = notes.len();
            let enriched_start = enriched;
            if routine.kind == "query" && scope::is_select(dialect.as_deref(), body) {
                let object = ObjectDoc {
                    name: routine.name.clone(),
                    kind: "query".into(),
                    columns: vec![],
                    constraints: vec![],
                    indexes: vec![],
                    triggers: vec![],
                    body: Some(body.clone()),
                    usage: None,
                };
                notes.extend(scope::enrich_view(
                    g,
                    &catalog_index,
                    &schema.name,
                    &object,
                    &owner,
                    dialect.as_deref(),
                ));
                enriched += 1;
                cache::save(
                    &mut cache,
                    g,
                    &owner,
                    Some(body),
                    context.as_ref(),
                    cache::BodyOutcome {
                        notes: &notes[note_start..],
                        enriched: enriched - enriched_start,
                        dml_origins: catalog_index.take_dml_origins(&owner),
                    },
                );
                continue;
            }
            let mut state = schemagraph_core::AnalysisState::Complete;
            let mut record_body_diagnostics = Vec::new();
            (|| {
                if matches!(doc.dialect.as_str(), "db2" | "informix")
                    && catalog_routine_language_allowed(&doc.dialect, routine.language.as_deref())
                {
                    let (mut parsed, unextracted) = parse_catalog_routine_body(&doc.dialect, body);
                    let partial = apply_routine(
                        g,
                        BodyContext {
                            catalog: &catalog_index,
                            schema: &schema.name,
                            ci,
                            body_hash: scope::body_hash(body),
                            sqlserver_trigger: false,
                        },
                        &owner,
                        &mut parsed,
                        &mut notes,
                    );
                    if partial {
                        state = schemagraph_core::AnalysisState::Partial;
                    }
                    record_body_diagnostics = parsed.diagnostics.clone();
                    if unextracted > 0 {
                        notes.push(format!(
                        "routine {owner}: body statement(s) {unextracted} could not be extracted"
                    ));
                    }
                    enriched += 1;
                    return;
                }
                if matches!(doc.dialect.as_str(), "db2" | "informix") {
                    state = schemagraph_core::AnalysisState::Unsupported;
                    let language = routine.language.as_deref().unwrap_or("unknown");
                    notes.push(format!(
                    "routine {owner}: language {language} is unsupported for {0} catalog parsing — body edges omitted",
                    doc.dialect
                ));
                    return;
                }
                match routine.language.as_deref() {
                    // SQL 언어 함수는 몸체가 그대로 SQL이라 파싱 가능 — 나머지
                    // 언어는 몸체 문법이 SQL이 아니라 미지원으로 보고한다.
                    Some("sql") | None => match parse_routine_body(dialect.as_deref(), body) {
                        Ok(mut parsed) => {
                            let partial = apply_routine(
                                g,
                                BodyContext {
                                    catalog: &catalog_index,
                                    schema: &schema.name,
                                    ci,
                                    body_hash: scope::body_hash(body),
                                    sqlserver_trigger: false,
                                },
                                &owner,
                                &mut parsed,
                                &mut notes,
                            );
                            if partial {
                                state = schemagraph_core::AnalysisState::Partial;
                            }
                            record_body_diagnostics = parsed.diagnostics.clone();
                            enriched += 1;
                        }
                        Err(msg) => {
                            // 도매 파싱이 실패한 몸체 — T-SQL의 DECLARE/IF/TRY
                            // 같은 절차형 구문이 섞이면 sqlparser가 문장 목록째
                            // 거부한다. 문장 추출로 부분 복구해 파싱된 문장의
                            // 간선만 취하고, 미추출은 limitation으로 센다.
                            let (mut parsed, unextracted) = parse_procedural_body(
                                dialect.as_deref(),
                                body,
                                constant_sql::dialect(&doc.dialect),
                                format_shadowed,
                            );
                            if parsed.has_edges() || unextracted > 0 {
                                if parsed.has_edges() {
                                    let partial = apply_routine(
                                        g,
                                        BodyContext {
                                            catalog: &catalog_index,
                                            schema: &schema.name,
                                            ci,
                                            body_hash: scope::body_hash(body),
                                            sqlserver_trigger: false,
                                        },
                                        &owner,
                                        &mut parsed,
                                        &mut notes,
                                    );
                                    if partial {
                                        state = schemagraph_core::AnalysisState::Partial;
                                    }
                                }
                                record_body_diagnostics = parsed.diagnostics.clone();
                                // 미추출 0건은 완전 복구 — 한계가 아니므로 조용히 둔다.
                                if unextracted > 0 {
                                    notes.push(format!(
                                        "routine {owner}: 몸체 문장 {unextracted}건 미추출\
                                     (절차형 구문) — 몸체 간선 불완전 ({msg})"
                                    ));
                                }
                                enriched += 1;
                            } else {
                                state = schemagraph_core::AnalysisState::Unsupported;
                                notes.push(format!("routine {owner} body parse failed: {msg}"));
                            }
                        }
                    },
                    // plpgsql·plsql은 문장 단위 추출기로 SQL 문장만 꺼내 파싱한다.
                    // 꺼내지 못한 문장(동적 SQL 등)은 수를 세어 limitation으로 남긴다.
                    Some("plpgsql" | "plsql" | "pl/sql") => {
                        let (mut parsed, unextracted) = parse_procedural_body(
                            dialect.as_deref(),
                            body,
                            constant_sql::dialect(&doc.dialect),
                            format_shadowed,
                        );
                        let partial = apply_routine(
                            g,
                            BodyContext {
                                catalog: &catalog_index,
                                schema: &schema.name,
                                ci,
                                body_hash: scope::body_hash(body),
                                sqlserver_trigger: false,
                            },
                            &owner,
                            &mut parsed,
                            &mut notes,
                        );
                        if partial {
                            state = schemagraph_core::AnalysisState::Partial;
                        }
                        record_body_diagnostics = parsed.diagnostics.clone();
                        // 멤버가 member_of로 나오는 문서에서 패키지 몸체는 스펙이라
                        // 실행 간선이 거의 없다 — 패키지 정점에 간선이 붙는다는 건
                        // 멤버 몸체가 통째로 실린 옛 형식이라는 뜻이라 그때만 알린다.
                        if routine.kind == "package" && parsed.has_edges() {
                            notes.push(format!(
                                "routine {owner}: 패키지 몸체 간선은 패키지 정점에 귀속 \
                             — 멤버 구분은 문서의 member_of를 쓴다"
                            ));
                        }
                        if unextracted > 0 {
                            notes.push(format!(
                                "routine {owner}: 몸체 문장 {unextracted}건 미추출\
                             (동적 SQL·비SQL 구문) — 몸체 간선 불완전"
                            ));
                        }
                        enriched += 1;
                    }
                    Some(lang) => {
                        state = schemagraph_core::AnalysisState::Unsupported;
                        notes.push(format!("routine {owner}: language {lang} is unsupported; body dependencies omitted"));
                    }
                }
            })();
            record_body(
                g,
                &owner,
                Some(body),
                state,
                "SG_BODY_PARTIAL",
                &notes[note_start..],
                &record_body_diagnostics,
            );
            cache::save(
                &mut cache,
                g,
                &owner,
                Some(body),
                context.as_ref(),
                cache::BodyOutcome {
                    notes: &notes[note_start..],
                    enriched: enriched - enriched_start,
                    dml_origins: catalog_index.take_dml_origins(&owner),
                },
            );
        }
    }
    for schema in &doc.schemas {
        for routine in schema
            .routines
            .iter()
            .filter(|r| r.kind == "query" && r.source.is_some())
        {
            if let Some(id) = resolve_routine_vertex(
                g,
                &schema.name,
                routine,
                VertexKind::Query,
                "query",
                catalog_index.lookup(false),
            ) {
                if let Some(mut record) = g.analysis().get(&id).cloned() {
                    record.source = routine.source.clone();
                    g.set_analysis(id, record);
                }
            }
        }
    }
    if unavailable > 0 {
        g.add_limitation(format!("{unavailable} routine/trigger definitions were not collected; see structured analysis diagnostics"));
    }
    (enriched, notes)
}

/// 절차형 추출기의 범위를 명시하고, 재작성 전 원문 해시만 근거로 보존한다.
fn record_body(
    g: &mut Graph,
    owner: &VertexId,
    body: Option<&str>,
    mut state: schemagraph_core::AnalysisState,
    code: &str,
    notes: &[String],
    structured_diagnostics: &[schemagraph_core::Diagnostic],
) {
    use schemagraph_core::{AnalysisState, Diagnostic, ObjectAnalysis, Origin};
    if state == AnalysisState::Complete && !notes.is_empty() {
        state = AnalysisState::Partial;
    }
    let code = if state == AnalysisState::Unsupported && code == "SG_BODY_PARTIAL" {
        "SG_BODY_UNSUPPORTED"
    } else {
        code
    };
    let hash = body.map(scope::body_hash);
    let keys: Vec<_> = g
        .outgoing(owner)
        .iter()
        .filter(|e| {
            e.evidence
                .iter()
                .any(|ev| ev.layer == EvidenceLayer::BodyParse)
        })
        .map(|e| (e.from.clone(), e.to.clone(), e.kind))
        .collect();
    if let Some(hash) = &hash {
        for key in keys {
            g.add_origin(
                key,
                Origin {
                    body_hash: hash.clone(),
                    role: "body-dependency".into(),
                    location: None,
                },
            );
        }
    }
    let mut diagnostics: Vec<Diagnostic> = structured_diagnostics.to_vec();
    // 구조화 진단이 이미 같은 문구를 싣고 있으면(후보를 실은 모호 호출 등) note로
    // 같은 진단을 한 번 더 만들지 않는다.
    let structured: BTreeSet<&str> = structured_diagnostics
        .iter()
        .map(|diagnostic| diagnostic.message.as_str())
        .collect();
    diagnostics.extend(
        notes
            .iter()
            .filter(|message| !structured.contains(message.as_str()))
            .map(|message| Diagnostic::new(code, message.clone(), None)),
    );
    g.set_analysis(
        owner.clone(),
        ObjectAnalysis {
            source: None,
            state,
            scope: "object-dependencies".into(),
            body_hash: hash,
            diagnostics,
        },
    );
}

/// `--inferred` opt-in 이름 규칙 추정 — 선언된 FK가 없는 `xxx_id` 컬럼을
/// 같은 스키마의 `xxx`/`xxxs`/`xxxes`/`xxies` 테이블의 `id` 컬럼으로 추정해
/// `inferred` 간선을 만든다. 추정은 판정 근거가 아니라 탐색 보조다 —
/// EdgeKind::Inferred는 is_dependency()가 걸러 의존성 질의에 섞이지 않고,
/// 후보가 둘 이상이면 추측하지 않고 수만 센다.
pub fn enrich_inferred(g: &mut Graph, doc: &CatalogDocument) -> (usize, Vec<String>) {
    let mut made = 0usize;
    let mut notes = Vec::new();
    let mut ambiguous = 0usize;
    for schema in &doc.schemas {
        // 스키마 안 테이블명 인덱스 — 추정이라 카탈로그 접힘(Oracle 대문자
        // 등)을 흡수하려고 대소문자를 무시한다.
        let tables: BTreeMap<String, &ObjectDoc> = schema
            .objects
            .iter()
            .filter(|o| o.kind == "table")
            .map(|o| (o.name.to_ascii_lowercase(), o))
            .collect();
        for obj in schema.objects.iter().filter(|o| o.kind == "table") {
            // 선언된 FK가 덮는 컬럼은 추정 대상이 아니다 — 추정은 선언이
            // 없는 틈만 메운다.
            let declared: BTreeSet<&str> = obj
                .constraints
                .iter()
                .filter(|c| c.kind == "fk")
                .flat_map(|c| c.columns.iter().map(|s| s.as_str()))
                .collect();
            for col in &obj.columns {
                let lower = col.name.to_ascii_lowercase();
                let Some(stem) = lower.strip_suffix("_id") else {
                    continue;
                };
                if stem.is_empty() || declared.contains(col.name.as_str()) {
                    continue;
                }
                // 후보 테이블명: 단수·복수·-es·-ies 변형.
                let mut names = vec![stem.to_owned(), format!("{stem}s"), format!("{stem}es")];
                if let Some(y) = stem.strip_suffix('y') {
                    names.push(format!("{y}ies"));
                }
                // 대상 테이블이 id 컬럼을 가질 때만 추정 근거가 성립한다.
                let candidates: Vec<&ObjectDoc> = names
                    .iter()
                    .filter_map(|n| tables.get(n.as_str()).copied())
                    .filter(|t| t.columns.iter().any(|c| c.name.eq_ignore_ascii_case("id")))
                    .collect();
                match candidates.as_slice() {
                    [target] => {
                        let from = VertexId::object(&schema.name, &obj.name);
                        let to = VertexId::object(&schema.name, &target.name);
                        if g.vertex(&from).is_none() || g.vertex(&to).is_none() {
                            continue;
                        }
                        g.add_edge(Edge {
                            from: from.clone(),
                            to: to.clone(),
                            kind: EdgeKind::Inferred,
                            evidence: vec![Evidence {
                                layer: EvidenceLayer::Inferred,
                                detail: format!(
                                    "{}.{}.{} ~ {to}.id 이름 규칙 추정(선언된 FK 없음)",
                                    schema.name, obj.name, col.name
                                ),
                            }],
                        });
                        made += 1;
                    }
                    [] => {}
                    _ => ambiguous += 1,
                }
            }
        }
    }
    if ambiguous > 0 {
        notes.push(format!(
            "inferred: 컬럼 {ambiguous}건이 후보 테이블 모호로 생략됨"
        ));
    }
    (made, notes)
}

/// doc.dialect 문자열 → sqlparser 방언. 모르는 방언은 Generic으로 돌린다 —
/// 파싱을 아예 포기하는 것보다 Generic이 낫다(못 읽으면 실패로 센다).
fn dialect_for(dialect: &str) -> Option<Box<dyn Dialect>> {
    Some(match dialect {
        "sqlite" => Box::new(SQLiteDialect {}),
        // 이름 정규화(scope)·상수 SQL이 같은 방언으로 보는 별칭도 같은 파서로 읽는다.
        // JDBC 프로브는 jdbc:mariadb: URL을 "mariadb"로 싣는다.
        "postgres" | "postgresql" => Box::new(PostgreSqlDialect {}),
        "mysql" | "mariadb" => Box::new(MySqlDialect {}),
        // MsSqlDialect는 [bracket] 식별자·TOP 같은 T-SQL 표면을 이해한다 —
        // Generic으로 파싱하면 문장째 실패하던 것이 풀린다.
        "sqlserver" | "mssql" => Box::new(MsSqlDialect {}),
        _ => Box::new(GenericDialect {}),
    })
}

/// ObjectName → (스키마?, 테이블). 부분이 셋 이상이면 마지막 둘을 쓰고
/// 나머지는 버린다 — 카탈로그를 넘나드는 참조는 DB마다 의미가 다르다.
fn object_name_parts(name: &ObjectName) -> (Option<String>, String) {
    let parts: Vec<String> = name
        .0
        .iter()
        .filter_map(|p| p.as_ident().map(|i| i.value.clone()))
        .collect();
    match parts.as_slice() {
        [] => (None, String::new()),
        [t] => (None, t.clone()),
        [s, t] => (Some(s.clone()), t.clone()),
        rest => {
            let t = rest.last().unwrap().clone();
            let s = rest.get(rest.len() - 2).cloned();
            (s, t)
        }
    }
}

/// 테이블 관계는 database qualifier를 보존할 catalog 필드가 없으므로
/// 두 부분(schema.table)까지만 안전하게 귀속한다. routine/package 이름은
/// object_name_parts를 계속 사용해 3부분 이름을 별도로 지원한다.
fn relation_name_parts(name: &ObjectName) -> Option<(Option<String>, String)> {
    let parts: Vec<String> = name
        .0
        .iter()
        .filter_map(|part| part.as_ident().map(|ident| ident.value.clone()))
        .collect();
    match parts.as_slice() {
        [] => None,
        [table] => Some((None, table.clone())),
        [schema, table] => Some((Some(schema.clone()), table.clone())),
        _ => None,
    }
}

fn relation_name_key(name: &ObjectName) -> String {
    name.0
        .iter()
        .filter_map(|part| part.as_ident().map(|ident| ident.value.clone()))
        .collect::<Vec<_>>()
        .join(".")
}

/// trigger 본문 파싱 결과.
struct ParsedTrigger {
    /// INSERT/UPDATE/DELETE의 쓰기 대상 (스키마?, 테이블).
    writes: Vec<(Option<String>, String)>,
    /// 쓰기 대상을 제외한 관계 — 조인·서브쿼리의 읽기 대상.
    reads: Vec<(Option<String>, String)>,
    /// NEW./OLD.로 읽은 발사 테이블의 컬럼.
    fired_columns: Vec<String>,
    /// EXECUTE FUNCTION/PROCEDURE로 부르는 routine (스키마?, 이름).
    calls: Vec<(Option<String>, String)>,
    /// database qualifier가 있어 local catalog로 귀속하지 않은 관계.
    ignored_relations: BTreeSet<String>,
    /// database.schema.table 3부분 관계. 적용 시 수집 context.database와
    /// 일치하는 경우에만 로컬 객체로 해석한다.
    qualified_reads: Vec<ObjectName>,
    qualified_writes: Vec<ObjectName>,
    /// AST로 확인한 DML 문장. 컬럼 효과는 카탈로그 정점이 있는 적용 단계에서
    /// 해석한다 — 파싱 단계에서 임시·미수집 정점을 만들지 않는다.
    dml_statements: Vec<Statement>,
    /// 문장 해시 → dml_statements 위치. 같은 문장인지 해시가 같은 후보와만 비교한다.
    dml_buckets: BTreeMap<u64, Vec<usize>>,
    /// 직선 구간에서 만든 임시 relation 이름. graph 정점으로 만들지 않고
    /// source column으로 접어야 하므로 객체 수준 missing note에서도 제외한다.
    local_relations: BTreeSet<(Option<String>, String)>,
    /// DML 컬럼 계보가 보수적으로 부분 결과임을 알리는 구조화 진단.
    diagnostics: Vec<schemagraph_core::Diagnostic>,
    /// routine header나 선언부에서 실제로 선언된 scalar symbol만 보관한다.
    scalar_symbols: BTreeSet<String>,
    partial: bool,
    /// 임시 relation 상태가 모든 문장을 순서대로 관찰했다는 보수적 증명.
    temp_state_safe: bool,
    /// AST span이 변환된 조각이 아니라 원문 몸체 좌표를 직접 가리키는가.
    locations_trusted: bool,
}

impl ParsedTrigger {
    /// 간선이 될 수확이 하나라도 있나 — 부분 복구 폴백이 "못 읽은 몸체"와
    /// "읽을 게 없던 몸체"를 구분하는 기준이다.
    fn has_edges(&self) -> bool {
        !(self.writes.is_empty()
            && self.reads.is_empty()
            && self.fired_columns.is_empty()
            && self.calls.is_empty()
            && self.ignored_relations.is_empty()
            && self.qualified_reads.is_empty()
            && self.qualified_writes.is_empty()
            && self.dml_statements.is_empty())
    }

    /// 컬럼 효과를 해석할 DML 문장을 처음 나온 순서대로 한 번씩만 보관한다.
    ///
    /// 위치만 다른 같은 문장은 효과와 근거가 같아 하나로 합친다. sqlparser의 비교와
    /// 해시는 둘 다 위치(span)를 보지 않으므로 해시가 다르면 다른 문장이다. 앞서 모은
    /// 문장 전체와 깊게 비교하면 문장 수의 제곱이 되므로, 해시가 같은 후보와만 비교한다.
    /// 문장은 복제하지 않고 옮긴다 — 깊은 집합 연산 사슬의 복제는 사슬 길이만큼 스택을 쓴다.
    fn retain_dml_statement(&mut self, statement: Statement) {
        let bucket = self
            .dml_buckets
            .entry(statement_hash(&statement))
            .or_default();
        let duplicate = bucket.iter().any(|&index| {
            #[cfg(test)]
            work::compared_statement();
            self.dml_statements[index] == statement
        });
        if !duplicate {
            bucket.push(self.dml_statements.len());
            self.dml_statements.push(statement);
        }
    }
}

/// 한 실행 안에서만 쓰는 문장 해시. 후보를 고르는 데만 쓰고 출력 순서에는 쓰지 않는다.
fn statement_hash(statement: &Statement) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    statement.hash(&mut hasher);
    hasher.finish()
}

/// `CREATE TRIGGER ... BEGIN <문장들> END`를 파싱한다. sqlparser는
/// CREATE TRIGGER 자체를 못 파는 방언이 많아 껍질(BEGIN..END)은 직접
/// 벗기고 안쪽 문장만 파서에 넘긴다. 껍질이 없는 몸체(reader가 내부
/// 문장만 저장한 경우)는 통째로 파싱한다.
fn parse_trigger_body(
    dialect: Option<&dyn Dialect>,
    body: &str,
    sqlserver_trigger: bool,
    sqlite_trigger: bool,
) -> Result<ParsedTrigger, String> {
    let default = GenericDialect {};
    let dialect = dialect.unwrap_or(&default);
    let calls = extract_execute_targets(body);
    let inner = extract_trigger_inner(body).unwrap_or(body);
    let locations_trusted =
        std::ptr::eq(inner.as_ptr(), body.as_ptr()) && inner.len() == body.len();
    let statements = parse_limits::parse_sql(dialect, inner.trim());
    let mut parsed = ParsedTrigger {
        writes: Vec::new(),
        reads: Vec::new(),
        fired_columns: Vec::new(),
        calls,
        ignored_relations: BTreeSet::new(),
        qualified_reads: Vec::new(),
        qualified_writes: Vec::new(),
        dml_statements: Vec::new(),
        dml_buckets: BTreeMap::new(),
        local_relations: BTreeSet::new(),
        diagnostics: Vec::new(),
        scalar_symbols: BTreeSet::new(),
        partial: false,
        temp_state_safe: true,
        locations_trusted,
    };
    match statements {
        Ok(stmts) if !stmts.is_empty() => {
            // 일부 T-SQL 파서는 DECLARE/SET/EXEC 조각을 성공으로 분류하지만
            // 변수와 동적 SQL의 의미는 AST에 남기지 않는다. 그런 몸체는
            // 절차형 추출기로 다시 읽어야 한다. trigger의 EXECUTE FUNCTION은
            // 위에서 calls를 이미 채취했으므로 예외로 둔다.
            if parsed.calls.is_empty() && needs_procedural_recovery(body) {
                return Err(
                    "procedural variables or dynamic SQL require statement extraction".to_owned(),
                );
            }
            for mut stmt in stmts {
                if dialect.is::<PostgreSqlDialect>() {
                    body_relations::normalize_postgres(&mut stmt);
                }
                collect_trigger_stmt(stmt, &mut parsed, sqlserver_trigger, sqlite_trigger);
            }
            invalidate_temp_state_if_unsafe(&mut parsed, body, 0);
            Ok(parsed)
        }
        // EXECUTE FUNCTION 꼴 trigger는 본문 자체는 못 파지만 호출 대상은
        // 채취됐다 — 문장 파싱 실패를 호출 간선 포기 이유로 쓰지 않는다.
        _ if !parsed.calls.is_empty() => Ok(parsed),
        Ok(_) => Err("본문에서 문장을 찾지 못함".to_owned()),
        Err(e) => Err(e.to_string()),
    }
}

/// AST 도매 파싱이 성공해도 변수·동적 실행을 잃는 표면 구문인지 확인한다.
fn needs_procedural_recovery(body: &str) -> bool {
    if ["DECLARE", "EXECUTE", "EXEC"]
        .iter()
        .any(|keyword| !find_all_keywords(body, keyword).is_empty())
    {
        return true;
    }
    find_all_keywords(body, "SET")
        .into_iter()
        .any(|position| body[position + "SET".len()..].trim_start().starts_with('@'))
}

/// routine 몸체 파싱 — CREATE FUNCTION 전문이 들어오면 AS 뒤의 몸체만
/// 꺼내고, 나머지는 trigger와 같은 문장 수집 골격으로 파싱한다.
fn parse_routine_body(dialect: Option<&dyn Dialect>, body: &str) -> Result<ParsedTrigger, String> {
    let inner = extract_as_body(body).unwrap_or(std::borrow::Cow::Borrowed(body));
    let transformed =
        inner.as_ref().len() != body.len() || !std::ptr::eq(inner.as_ref().as_ptr(), body.as_ptr());
    let mut parsed = parse_trigger_body(dialect, &inner, false, false)?;
    parsed
        .scalar_symbols
        .extend(routine_parameter_symbols(body));
    if transformed {
        parsed.locations_trusted = false;
    }
    Ok(parsed)
}

/// Db2 LUW와 Informix SPL의 catalog text는 `AS` 문자열이 아니라 CREATE
/// routine 전문이다. 알려진 header와 block 껍질만 벗기고 나머지는 기존
/// 문장 추출기에 넘긴다 — 동적 SQL의 값을 추측하지 않는다.
fn parse_catalog_routine_body(dialect_name: &str, body: &str) -> (ParsedTrigger, usize) {
    let Some(inner) = strip_catalog_routine_header(dialect_name, body) else {
        return (empty_parsed_trigger(), 1);
    };
    let dialect = dialect_for(dialect_name);
    parse_procedural_body(
        dialect.as_deref(),
        inner,
        constant_sql::dialect(dialect_name),
        false,
    )
}

fn catalog_routine_language_allowed(dialect: &str, language: Option<&str>) -> bool {
    let Some(language) = language else {
        return true;
    };
    match dialect {
        "db2" => language.eq_ignore_ascii_case("sql"),
        "informix" => language.eq_ignore_ascii_case("spl") || language.eq_ignore_ascii_case("sql"),
        _ => false,
    }
}

fn empty_parsed_trigger() -> ParsedTrigger {
    ParsedTrigger {
        writes: Vec::new(),
        reads: Vec::new(),
        fired_columns: Vec::new(),
        calls: Vec::new(),
        ignored_relations: BTreeSet::new(),
        qualified_reads: Vec::new(),
        qualified_writes: Vec::new(),
        dml_statements: Vec::new(),
        dml_buckets: BTreeMap::new(),
        local_relations: BTreeSet::new(),
        diagnostics: Vec::new(),
        scalar_symbols: BTreeSet::new(),
        partial: false,
        temp_state_safe: true,
        locations_trusted: false,
    }
}

/// Db2 `RETURN`/DML과 Informix `RETURNING;` 뒤를 routine body로 찾는다.
/// 키워드 검색기는 문자열·주석을 건너뛰므로 quoted fake SQL을 body marker로
/// 오인하지 않는다.
fn strip_catalog_routine_header<'a>(dialect: &str, body: &'a str) -> Option<&'a str> {
    let create = find_all_keywords(body, "CREATE").first().copied()?;
    let tail = &body[create + "CREATE".len()..];
    let routine = [
        find_all_keywords(tail, "FUNCTION").first().copied(),
        find_all_keywords(tail, "PROCEDURE").first().copied(),
    ]
    .into_iter()
    .flatten()
    .min()?;
    let after_routine = &tail[routine + head_word(&tail[routine..]).1..];
    if dialect == "db2" {
        return strip_db2_routine_body(after_routine);
    }
    strip_informix_routine_body(after_routine)
}

fn strip_db2_routine_body(tail: &str) -> Option<&str> {
    let markers = [
        "BEGIN", "RETURN", "SELECT", "INSERT", "UPDATE", "DELETE", "MERGE", "CALL", "VALUES",
    ];
    let marker = markers
        .iter()
        .flat_map(|keyword| find_all_keywords(tail, keyword))
        .min()?;
    let body = &tail[marker..];
    if starts_with_keyword(body, "BEGIN") {
        return extract_trigger_inner(body).or(Some(body));
    }
    Some(body)
}

fn strip_informix_routine_body(tail: &str) -> Option<&str> {
    if let Some(returning) = find_all_keywords(tail, "RETURNING").first().copied() {
        let after_returning = &tail[returning + "RETURNING".len()..];
        let end = find_top_level_semicolon(after_returning)?;
        return Some(after_returning[end..].trim_start());
    }
    if let Some(end) = find_parameter_list_end(tail) {
        return Some(tail[end..].trim_start());
    }
    [
        "DEFINE", "LET", "SELECT", "INSERT", "UPDATE", "DELETE", "MERGE", "RETURN",
    ]
    .iter()
    .flat_map(|keyword| find_all_keywords(tail, keyword).into_iter())
    .min()
    .map(|position| tail[position..].trim_start())
}

fn find_parameter_list_end(text: &str) -> Option<usize> {
    let quotes = QuoteScanner::new(text);
    let bytes = text.as_bytes();
    let mut open = None;
    let mut scan = 0;
    while scan < bytes.len() {
        if let Some(end) = quotes.end(scan) {
            scan = end;
            continue;
        }
        if bytes[scan] == b'(' {
            open = Some(scan);
            break;
        }
        scan += 1;
    }
    let open = open?;
    let mut depth = 0usize;
    let mut index = open;
    while index < bytes.len() {
        if let Some(end) = quotes.end(index) {
            index = end;
            continue;
        }
        match bytes[index] {
            b'(' => depth += 1,
            b')' => {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    return Some(index + 1);
                }
            }
            _ => {}
        }
        index += 1;
    }
    None
}

fn find_top_level_semicolon(text: &str) -> Option<usize> {
    let quotes = QuoteScanner::new(text);
    let bytes = text.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if let Some(end) = quotes.end(index) {
            index = end;
            continue;
        }
        if bytes[index] == b';' {
            return Some(index + 1);
        }
        index += 1;
    }
    None
}

fn parse_informix_trigger_body(
    dialect: Option<&dyn Dialect>,
    body: &str,
) -> Result<ParsedTrigger, String> {
    let inner = strip_catalog_trigger_header("informix", body)
        .and_then(strip_outer_parentheses)
        .or_else(|| strip_catalog_trigger_header("informix", body))
        .or_else(|| strip_outer_parentheses(body))
        .unwrap_or(body);
    if let Some(alias) = informix_new_alias(body) {
        let normalized = replace_trigger_alias(inner, &alias);
        return parse_trigger_body(dialect, &normalized, false, false);
    }
    parse_trigger_body(dialect, inner, false, false)
}

fn informix_new_alias(body: &str) -> Option<String> {
    let referencing = find_all_keywords(body, "REFERENCING").first().copied()?;
    let rest = &body[referencing + "REFERENCING".len()..];
    let new_position = find_all_keywords(rest, "NEW").first().copied()?;
    let after_new = rest[new_position + "NEW".len()..].trim_start();
    if !starts_with_keyword(after_new, "AS") {
        return None;
    }
    let after_as = after_new["AS".len()..].trim_start();
    let (alias, length) = head_word(after_as);
    (length > 0).then(|| alias.to_owned())
}

fn replace_trigger_alias(body: &str, alias: &str) -> String {
    let quotes = QuoteScanner::new(body);
    let bytes = body.as_bytes();
    let mut output = String::with_capacity(body.len());
    let mut index = 0;
    while index < bytes.len() {
        if let Some(end) = quotes.end(index) {
            output.push_str(&body[index..end]);
            index = end;
            continue;
        }
        let remaining = &body[index..];
        if remaining
            .get(..alias.len())
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case(alias))
            && (index == 0 || !is_identifier_byte(bytes[index - 1]))
            && bytes.get(index + alias.len()) == Some(&b'.')
        {
            output.push_str("NEW.");
            index += alias.len() + 1;
            continue;
        }
        let width = body[index..]
            .chars()
            .next()
            .map(char::len_utf8)
            .unwrap_or(1);
        output.push_str(&body[index..index + width]);
        index += width;
    }
    output
}

fn is_identifier_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'$' | b'#' | b'.') || byte >= 128
}

fn parse_db2_trigger_body(
    dialect: Option<&dyn Dialect>,
    body: &str,
) -> Result<ParsedTrigger, String> {
    let inner = strip_catalog_trigger_header("db2", body).unwrap_or(body);
    parse_trigger_body(dialect, inner, false, false)
}

fn strip_catalog_trigger_header<'a>(dialect: &str, body: &'a str) -> Option<&'a str> {
    let for_each_row = find_all_keywords(body, "FOR")
        .into_iter()
        .filter_map(|position| {
            let rest = body[position + "FOR".len()..].trim_start();
            if !starts_with_keyword(rest, "EACH") {
                return None;
            }
            let rest = rest["EACH".len()..].trim_start();
            starts_with_keyword(rest, "ROW").then_some(rest["ROW".len()..].trim_start())
        })
        .next()?;
    if dialect == "db2" {
        let mut rest = for_each_row;
        if starts_with_keyword(rest, "MODE") {
            let after_mode = rest["MODE".len()..].trim_start();
            let (_, length) = head_word(after_mode);
            rest = after_mode[length..].trim_start();
        }
        return Some(rest);
    }
    Some(for_each_row)
}

fn strip_outer_parentheses(text: &str) -> Option<&str> {
    let trimmed = text.trim();
    if !trimmed.starts_with('(') {
        return None;
    }
    let end = trimmed.trim_end_matches(';').trim_end().len();
    if end == 0 || trimmed.as_bytes().get(end - 1) != Some(&b')') {
        return None;
    }
    let bytes = trimmed.as_bytes();
    let quotes = QuoteScanner::new(trimmed);
    let mut depth = 0usize;
    let mut index = 0;
    while index < end {
        if let Some(end) = quotes.end(index) {
            index = end;
            continue;
        }
        match bytes[index] {
            b'(' => depth += 1,
            b')' => {
                depth = depth.checked_sub(1)?;
                if depth == 0 && index + 1 != end {
                    return None;
                }
            }
            _ => {}
        }
        index += 1;
    }
    (depth == 0).then(|| trimmed[1..end - 1].trim())
}

/// `AS $$...$$`·`AS $tag$...$tag$`·`AS '...'` 안의 몸체를 꺼낸다 —
/// pg_get_functiondef·SHOW CREATE FUNCTION류 정의문은 몸체를 AS 뒤에
/// 문자열로 담으므로, 그 껍질을 벗겨야 SQL 문장이 나온다.
fn extract_as_body(body: &str) -> Option<std::borrow::Cow<'_, str>> {
    for i in find_all_keywords(body, "AS") {
        let rest = body[i + "AS".len()..].trim_start();
        if rest.starts_with('\'') {
            if let Some(inner) = unquote_sql_string(rest) {
                return Some(inner);
            }
        } else if rest.starts_with('$') {
            if let Some(inner) = undollar_quote(rest) {
                return Some(inner);
            }
        }
    }
    None
}

/// '...' 리터럴의 내용 — '' 이스케이프는 따옴표 하나로 되돌린다.
fn unquote_sql_string(rest: &str) -> Option<std::borrow::Cow<'_, str>> {
    let bytes = rest.as_bytes();
    let mut j = 1;
    while j < bytes.len() {
        if bytes[j] == b'\'' {
            if j + 1 < bytes.len() && bytes[j + 1] == b'\'' {
                j += 2;
                continue;
            }
            let inner = &rest[1..j];
            return Some(if inner.contains("''") {
                std::borrow::Cow::Owned(inner.replace("''", "'"))
            } else {
                std::borrow::Cow::Borrowed(inner)
            });
        }
        j += 1;
    }
    None
}

/// $tag$body$tag$ dollar-quote의 내용 — 태그는 $ 사이의 식별자다.
fn undollar_quote(rest: &str) -> Option<std::borrow::Cow<'_, str>> {
    let tag_end = rest[1..].find('$').map(|k| k + 1)?;
    let tag = &rest[..=tag_end]; // "$tag$"
    let inner_start = tag.len();
    let close = rest[inner_start..].find(tag).map(|k| inner_start + k)?;
    Some(std::borrow::Cow::Borrowed(rest[inner_start..close].trim()))
}

/// `EXECUTE FUNCTION f(...)`·`EXECUTE PROCEDURE p(...)` 꼴의 호출 대상을
/// 껍질에서 직접 채취한다 — PG·Oracle 계열은 trigger가 내부 문장 대신
/// routine을 호출하고, 그 이름은 파서가 못 읽는 CREATE TRIGGER 안에 있다.
fn extract_execute_targets(body: &str) -> Vec<(Option<String>, String)> {
    let mut out = Vec::new();
    for i in find_all_keywords(body, "EXECUTE") {
        let rest = body[i + "EXECUTE".len()..].trim_start();
        let name_part = if starts_with_keyword(rest, "FUNCTION") {
            rest["FUNCTION".len()..].trim_start()
        } else if starts_with_keyword(rest, "PROCEDURE") {
            rest["PROCEDURE".len()..].trim_start()
        } else {
            continue;
        };
        let name: String = name_part
            .chars()
            .take_while(|c| c.is_alphanumeric() || matches!(c, '_' | '.' | '"' | '$'))
            .collect();
        let name = name.trim_matches('"').replace("\".\"", ".");
        if !name.is_empty() {
            let (schema, table) = split_qualified(&name);
            if !table.is_empty() {
                out.push((schema, table));
            }
        }
    }
    out
}

/// "a.b.c" 꼴 이름을 (스키마?, 말단)으로 나눈다 — object_name_parts와 같은
/// 규칙: 셋 이상이면 마지막 둘. 따옴표는 이미 벗겨졌다고 가정한다.
fn split_qualified(name: &str) -> (Option<String>, String) {
    let parts: Vec<&str> = name.split('.').filter(|p| !p.is_empty()).collect();
    match parts.as_slice() {
        [] => (None, String::new()),
        [t] => (None, (*t).to_owned()),
        rest => (
            Some(rest[rest.len() - 2].to_string()),
            rest[rest.len() - 1].to_string(),
        ),
    }
}

/// 대소문자 무관 접두 + 단어 경계 검사.
fn starts_with_keyword(s: &str, kw: &str) -> bool {
    s.len() >= kw.len()
        && s.as_bytes()[..kw.len()].eq_ignore_ascii_case(kw.as_bytes())
        && (s.len() == kw.len() || {
            let b = s.as_bytes()[kw.len()];
            !b.is_ascii_alphanumeric() && b != b'_'
        })
}

/// BEGIN..END 안쪽을 꺼낸다. 인용·주석 안의 키워드는 블록 경계가 아니다.
fn extract_trigger_inner(body: &str) -> Option<&str> {
    // 대문자 사본을 만들지 않는다 — to_uppercase는 비ASCII에서 바이트
    // 오프셋을 바꿔 슬라이스를 틀어뜨린다. 원문에서 case-insensitive로 찾는다.
    let begin = find_all_keywords(body, "BEGIN").first().copied()? + "BEGIN".len();
    let end = find_all_keywords(body, "END").last().copied()?;
    let mut inner = body.get(begin..end)?.trim();
    // SQL/PSM의 BEGIN ATOMIC — ATOMIC은 문장이 아니라 벗겨야 파싱된다.
    if starts_with_keyword(inner, "ATOMIC") {
        inner = inner["ATOMIC".len()..].trim_start();
    }
    (!inner.is_empty()).then_some(inner)
}

/// 인용·주석 바깥의 키워드만 찾는다 — 문자열에 든 SQL은 실행 문맥에서만 푼다.
fn find_all_keywords(s: &str, kw: &str) -> Vec<usize> {
    let quotes = QuoteScanner::new(s);
    let bytes = s.as_bytes();
    let mut found = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if let Some(end) = quotes.end(i) {
            i = end;
            continue;
        }
        if bytes
            .get(i..i + kw.len())
            .is_some_and(|word| word.eq_ignore_ascii_case(kw.as_bytes()))
            && word_boundary(s, i, kw.len())
        {
            found.push(i);
        }
        i += 1;
    }
    found
}

/// 위치 i부터 len 바이트가 독립 단어인지 — 식별자 문자로 붙어 있으면
/// 키워드가 아니다.
fn word_boundary(s: &str, i: usize, len: usize) -> bool {
    let ident = |b: u8| b.is_ascii_alphanumeric() || b == b'_';
    let before_ok = i == 0 || !ident(s.as_bytes()[i - 1]);
    let after_ok = i + len >= s.len() || !ident(s.as_bytes()[i + len]);
    before_ok && after_ok
}

/// 절차형 몸체에서 안전하게 재사용할 수 있는 텍스트 변수 상태.
///
/// 외부 함수 호출·분기·루프·중첩 블록을 만나면 전체 평가를 끄고 변수 값을
/// 비운다. 경로가 여러 개일 때 한 경로의 값으로 다른 경로의 SQL을 추측하지
/// 않기 위해서다.
struct ExtractionState {
    dialect: constant_sql::TextDialect,
    variables: BTreeMap<String, String>,
    capacities: BTreeMap<String, constant_sql::TextCapacity>,
    unicode_capable: BTreeSet<String>,
    format_shadowed: bool,
    evaluation_enabled: bool,
    outer_block_seen: bool,
    declared_symbols: BTreeSet<String>,
    /// 지금 몸체를 꺼내 내려간 중첩 DO 블록 수.
    do_depth: usize,
    /// 추적 중인 변수 이름과 값의 바이트 합.
    tracked_bytes: usize,
    /// 추적 상한을 처음 넘은 사유. 그 뒤 동적 SQL을 실제로 접지 못했을 때만 진단에 싣는다.
    tracking_overflow: Option<String>,
    /// 추적 상한이 평가를 끈 뒤, 분기 같은 다른 이유로 꺼지기 전에 변수 값이 필요한 동적
    /// SQL을 접지 못했는가.
    overflow_blocked_folding: bool,
}

impl ExtractionState {
    fn new(dialect: constant_sql::TextDialect, format_shadowed: bool) -> Self {
        Self {
            dialect,
            variables: BTreeMap::new(),
            capacities: BTreeMap::new(),
            unicode_capable: BTreeSet::new(),
            format_shadowed,
            evaluation_enabled: true,
            outer_block_seen: false,
            declared_symbols: BTreeSet::new(),
            do_depth: 0,
            tracked_bytes: 0,
            tracking_overflow: None,
            overflow_blocked_folding: false,
        }
    }

    fn invalidate(&mut self) {
        self.evaluation_enabled = false;
        self.invalidate_all_variables();
    }

    fn observe_begin(&mut self) {
        if self.outer_block_seen {
            self.invalidate_control_flow();
        } else {
            self.outer_block_seen = true;
        }
    }

    /// 분기·루프·중첩 블록 뒤로는 추적 상한과 무관하게 평가가 꺼진다. 그 전까지 상한 때문에
    /// 접지 못한 동적 SQL이 없었다면 상한은 결과를 바꾸지 않으므로 사유를 버린다.
    fn invalidate_control_flow(&mut self) {
        if !self.overflow_blocked_folding {
            self.tracking_overflow = None;
        }
        self.invalidate();
    }

    /// 동적 SQL 식을 접지 못했다. 추적 상한이 평가를 끈 뒤라면 그 탓일 수 있으므로 상한
    /// 사유를 몸체 진단으로 남기게 표시한다.
    fn record_unfolded_dynamic_sql(&mut self) {
        if self.tracking_overflow.is_some() {
            self.overflow_blocked_folding = true;
        }
    }

    /// 몸체 진단에 실을 추적 상한 사유. 상한 뒤 동적 SQL을 실제로 접지 못했을 때만 돌려준다
    /// — 그런 동적 SQL이 없으면 변수 값을 버린 것이 결과를 바꾸지 않았다.
    fn take_tracking_limit(&mut self) -> Option<String> {
        let blocked = self.overflow_blocked_folding;
        self.tracking_overflow.take().filter(|_| blocked)
    }

    /// 대입 하나를 지금 변수 환경으로 평가해 값을 싣거나, 불명확하면 지운다.
    fn assign(&mut self, name: &str, expression: &str) {
        let key = normalize_variable_name(name);
        if key.is_empty() {
            return;
        }
        let value = self.assigned_value(&key, expression);
        self.set_variable(key, value);
    }

    /// 대입식의 확정 텍스트 값. 추적하지 않는 타입·용량 초과·평가 실패는 None이다.
    ///
    /// 변수 환경은 빌려서 평가한다 — 대입마다 환경 전체(값당 최대 64KiB)를 복제하면
    /// 대입 수의 제곱만큼 복사하게 된다.
    fn assigned_value(&self, key: &str, expression: &str) -> Option<String> {
        if !self.evaluation_enabled {
            return None;
        }
        let capacity = self.capacities.get(key).copied()?;
        let value = constant_sql::evaluate_with_format(
            expression,
            self.dialect,
            &self.variables,
            !self.format_shadowed,
        )?;
        let unicode_safe = self.dialect != constant_sql::TextDialect::MsSql
            || self.unicode_capable.contains(key)
            || value.is_ascii();
        (unicode_safe && constant_sql::fits_capacity(&value, capacity)).then_some(value)
    }

    /// 확정 값을 싣는다. 값이 없거나 평가가 꺼졌으면 지우고, 추적 상한을 넘으면
    /// 평가를 끄고 사유를 남긴다 — 참조 대입(`b := a`)은 값을 통째로 복사하므로 변수
    /// 수와 총량을 묶지 않으면 작은 몸체 하나가 GB 단위 메모리를 잡는다.
    fn set_variable(&mut self, key: String, value: Option<String>) {
        let previous = self
            .variables
            .get(&key)
            .map_or(0, |old| key.len() + old.len());
        let Some(value) = value.filter(|_| self.evaluation_enabled) else {
            self.remove_variable(&key);
            return;
        };
        let bytes = self.tracked_bytes - previous + key.len() + value.len();
        let count = self.variables.len() + usize::from(previous == 0);
        if let Some(reason) = constant_sql::tracking_overflow(count, bytes) {
            self.tracking_overflow.get_or_insert(reason);
            self.invalidate();
            return;
        }
        self.tracked_bytes = bytes;
        self.variables.insert(key, value);
    }

    fn remove_variable(&mut self, key: &str) {
        if let Some(old) = self.variables.remove(key) {
            self.tracked_bytes -= key.len() + old.len();
        }
    }

    fn declare(&mut self, name: &str, type_text: &str, expression: Option<&str>) {
        let key = normalize_variable_name(name);
        self.declared_symbols.insert(key.clone());
        self.remove_variable(&key);
        self.capacities.remove(&key);
        self.unicode_capable.remove(&key);
        let Some(capacity) = constant_sql::declared_text_capacity(type_text) else {
            return;
        };
        self.capacities.insert(key.clone(), capacity);
        if constant_sql::is_unicode_text_type(type_text) {
            self.unicode_capable.insert(key);
        }
        if let Some(expression) = expression {
            self.assign(name, expression);
        }
    }

    fn invalidate_all_variables(&mut self) {
        self.variables.clear();
        self.tracked_bytes = 0;
    }

    fn invalidate_variable(&mut self, name: &str) {
        self.remove_variable(&normalize_variable_name(name));
    }

    fn evaluate(&self, expression: &str) -> Option<String> {
        let allow_format = !self.format_shadowed;
        if self.evaluation_enabled {
            constant_sql::evaluate_with_format(
                expression,
                self.dialect,
                &self.variables,
                allow_format,
            )
        } else {
            // 분기 이후에도 경로와 무관한 리터럴·리터럴 format은 복구한다.
            // 변수는 이미 비웠으므로 이 호출이 값을 추측하지 않는다.
            constant_sql::evaluate_with_format(
                expression,
                self.dialect,
                &BTreeMap::new(),
                allow_format,
            )
        }
    }
}

/// plpgsql/plsql 몸체 — 문장 추출기가 꺼낸 SQL을 문장별로 파싱한다.
/// 한 문장이 이상해도 나머지가 살도록 개별 파싱하고, 실패는 미추출 수에
/// 합산한다. 반환의 usize는 추출·파싱에 실패한 문장 수다.
fn parse_procedural_body(
    dialect: Option<&dyn Dialect>,
    body: &str,
    text_dialect: constant_sql::TextDialect,
    format_shadowed: bool,
) -> (ParsedTrigger, usize) {
    let extracted = extract_as_body(body);
    let starts_in_declare = extracted
        .as_ref()
        .is_some_and(|inner| !starts_with_keyword(skip_ws_comments(inner), "BEGIN"));
    let inner = extracted.unwrap_or(std::borrow::Cow::Borrowed(body));
    let mut state = ExtractionState::new(text_dialect, format_shadowed);
    state
        .declared_symbols
        .extend(routine_parameter_symbols(body));
    let (stmts, mut unextracted) =
        extract_procedural_statements(&inner, &mut state, starts_in_declare);
    let default = GenericDialect {};
    let dialect = dialect.unwrap_or(&default);
    let mut parsed = ParsedTrigger {
        writes: Vec::new(),
        reads: Vec::new(),
        fired_columns: Vec::new(),
        calls: Vec::new(),
        ignored_relations: BTreeSet::new(),
        qualified_reads: Vec::new(),
        qualified_writes: Vec::new(),
        dml_statements: Vec::new(),
        dml_buckets: BTreeMap::new(),
        local_relations: BTreeSet::new(),
        diagnostics: Vec::new(),
        scalar_symbols: state.declared_symbols.clone(),
        partial: false,
        temp_state_safe: true,
        locations_trusted: false,
    };
    if let Some(reason) = state.take_tracking_limit() {
        // 상한 뒤의 변수 값은 버렸으므로 그 뒤 동적 SQL을 상수로 접지 못했다.
        parsed.partial = true;
        parsed.diagnostics.push(schemagraph_core::Diagnostic {
            code: "SG_VARIABLE_STATE_LIMIT".into(),
            message: format!(
                "{reason}; dynamic SQL after the limit was not folded from variable values"
            ),
            location: None,
            candidates: Vec::new(),
        });
    }
    for stmt in &stmts {
        match parse_limits::parse_sql(dialect, stmt) {
            Ok(list) => {
                for mut s in list {
                    if dialect.is::<PostgreSqlDialect>() {
                        body_relations::normalize_postgres(&mut s);
                    }
                    collect_trigger_stmt(s, &mut parsed, false, false);
                }
            }
            // 추출기가 SQL이 아닌 조각을 문장으로 오인한 경우 — 하나의
            // 실패가 전체를 쓰러뜨리지 않게 수만 센다.
            Err(_) => unextracted += 1,
        }
    }
    if ["IF", "ELSIF", "ELSEIF", "WHILE", "FOR", "CASE", "LOOP"]
        .iter()
        .any(|keyword| !find_all_keywords(body, keyword).is_empty())
        && parsed.dml_statements.iter().any(is_column_dml_statement)
    {
        parsed.partial = true;
        parsed.diagnostics.push(schemagraph_core::Diagnostic {
            code: "SG_DML_CONTROL_FLOW".into(),
            message: "DML extracted from a control-flow body is analyzed conservatively".into(),
            location: None,
            candidates: Vec::new(),
        });
    }
    invalidate_temp_state_if_unsafe(&mut parsed, body, unextracted);
    (parsed, unextracted)
}

/// 동적·미추출·DROP 뒤의 임시 relation은 실행 순서를 완전히 복원하지 못하면
/// 이전 바인딩을 재사용하지 않는다.
fn invalidate_temp_state_if_unsafe(parsed: &mut ParsedTrigger, body: &str, unextracted: usize) {
    if parsed.local_relations.is_empty() {
        return;
    }
    let unsafe_state = unextracted > 0
        || !find_all_keywords(body, "DROP").is_empty()
        || !find_all_keywords(body, "EXECUTE").is_empty()
        || !find_all_keywords(body, "EXEC").is_empty();
    if !unsafe_state {
        return;
    }
    parsed.temp_state_safe = false;
    parsed.partial = true;
    parsed.diagnostics.push(schemagraph_core::Diagnostic {
        code: "SG_DML_TEMP_STATE".into(),
        message: "temporary relation state crosses dynamic, dropped, or unextracted SQL; local lineage omitted".into(),
        location: None,
        candidates: Vec::new(),
    });
}

/// 절차형 몸체(plpgsql·plsql)에서 SQL 문장을 문장 단위로 추출한다.
///
/// 전체 문법을 파싱하는 대신 `;`로 나눈 조각에서 제어 구조 키워드를 벗겨
/// SQL 문장만 복원한다 — 조건식은 서브쿼리를 담을 수 있어 `(`가 있을 때
/// `SELECT`로 싸고, `FOR .. IN <질의>`·`PERFORM`·`EXECUTE '리터럴'`·
/// `:=` 대입·PL/SQL bare 호출은 그에 맞는 SQL로 재작성한다. 꺼내지 못한
/// 문장(동적 SQL 식, 인식 못 한 구문)은 수를 돌려 limitation으로 남긴다 —
/// 추측으로 채우지 않는다.
fn extract_procedural_statements(
    body: &str,
    state: &mut ExtractionState,
    starts_in_declare: bool,
) -> (Vec<String>, usize) {
    let mut stmts: Vec<String> = Vec::new();
    let mut unextracted = 0usize;
    // DECLARE/IS 섹션 안의 조각은 선언문 — 의존 대상이 아니라 미추출로 세지
    // 않되, DEFAULT/:= 안의 서브쿼리는 추출한다.
    let mut in_declare = starts_in_declare;
    for chunk in split_top_level(body) {
        let mut rest = chunk;
        loop {
            // 주석이 선두를 가리면 문장 머리 판별이 깨져 조각째 버려진다 —
            // 프로브 몸체는 `--` 주석을 그대로 담고 오므로 매 반복 건너뛴다.
            rest = skip_ws_comments(rest);
            if rest.is_empty() {
                break;
            }
            if rest.starts_with("<<") {
                // <<label>> 형태의 레이블은 소비하고 계속.
                match rest.find(">>") {
                    Some(end) => rest = &rest[end + 2..],
                    None => break,
                }
                continue;
            }
            let (word, wlen) = head_word(rest);
            match word.to_ascii_uppercase().as_str() {
                "BEGIN" => {
                    state.observe_begin();
                    in_declare = false;
                    rest = &rest[wlen..];
                }
                "DECLARE" | "DEFINE" | "IS" | "AS" => {
                    in_declare = true;
                    rest = &rest[wlen..];
                }
                "END" => {
                    rest = &rest[wlen..];
                    // END IF/LOOP/CASE 또는 END <label> — 꼬리 한 단어를 소비.
                    let (_tail_word, tail_len) = head_word(rest.trim_start());
                    if tail_len > 0 {
                        rest = &rest.trim_start()[tail_len..];
                    }
                }
                // T-SQL의 TRY/CATCH·TRAN/TRANSACTION은 BEGIN/END의 꼬리로만
                // 오는 한정자라 소비한다 — 독립 문장 머리가 아니다.
                "LOOP" | "ELSE" | "EXCEPTION" | "REPEAT" | "TRY" | "CATCH" | "TRAN"
                | "TRANSACTION" => {
                    state.invalidate_control_flow();
                    rest = &rest[wlen..];
                }
                // T-SQL은 조건 뒤가 THEN/LOOP가 아니라 BEGIN이라 둘 다 본다.
                "IF" | "ELSIF" | "ELSEIF" | "WHEN" => {
                    state.invalidate_control_flow();
                    rest = strip_condition(
                        rest,
                        wlen,
                        &["THEN", "BEGIN"],
                        &mut stmts,
                        &mut unextracted,
                    );
                }
                "WHILE" => {
                    state.invalidate_control_flow();
                    rest = strip_condition(
                        rest,
                        wlen,
                        &["LOOP", "BEGIN"],
                        &mut stmts,
                        &mut unextracted,
                    );
                }
                "UNTIL" => {
                    state.invalidate_control_flow();
                    rest = strip_condition(rest, wlen, &["END"], &mut stmts, &mut unextracted);
                }
                "FOR" => {
                    state.invalidate_control_flow();
                    rest = strip_for(rest, wlen, &mut stmts, &mut unextracted, state)
                }
                "CASE" => {
                    state.invalidate_control_flow();
                    rest = strip_case_operand(rest, wlen, &mut unextracted)
                }
                "RETURN" => rest = strip_return(rest, wlen, &mut stmts, &mut unextracted, state),
                "PERFORM" => {
                    // PERFORM은 결과를 버리는 SELECT — 몸체로는 SELECT와 같다.
                    let target = rest[wlen..].trim();
                    if !target.is_empty() {
                        stmts.push(format!("SELECT {target}"));
                    }
                    break;
                }
                "OPEN" => {
                    strip_open(rest, wlen, &mut stmts, &mut unextracted, state);
                    break;
                }
                "EXECUTE" => {
                    strip_execute(rest, wlen, &mut stmts, &mut unextracted, state);
                    break;
                }
                "EXEC" => {
                    // T-SQL `EXEC proc <args>`는 CALL의 별칭 — EXECUTE와
                    // 달리 동적 SQL이 아니라 routine 호출이 기본이다.
                    strip_exec_call(rest, wlen, &mut stmts, &mut unextracted, state);
                    break;
                }
                "SET" => {
                    // T-SQL `SET @v = <expr>`은 변수 대입 — 우변에 질의가
                    // 있으면 살린다. `SET NOCOUNT ON` 같은 환경 설정은
                    // 문장째 파서에 넘겨 성공하면 간선 없음, 실패하면
                    // 미추출로 세게 둔다.
                    let tail = rest[wlen..].trim_start();
                    if tail.starts_with('@') {
                        if let Some(eq) = find_top_level(tail, "=", false) {
                            let rhs = tail[eq + 1..].trim();
                            let name = variable_name(&tail[..eq]);
                            if let Some(name) = name {
                                state.assign(name, rhs);
                            }
                            if rhs.contains('(') && !is_constant_text(rhs, state) {
                                stmts.push(format!("SELECT {rhs}"));
                            }
                        }
                    } else {
                        stmts.push(rest.trim().to_owned());
                    }
                    break;
                }
                "EXIT" | "CONTINUE" | "ASSERT" => {
                    strip_tail_condition(rest, wlen, &mut stmts);
                    break;
                }
                "DO" => {
                    state.invalidate_control_flow();
                    rest = strip_do(rest, wlen, &mut stmts, &mut unextracted, state)
                }
                "PROCEDURE" | "FUNCTION" | "PACKAGE" => {
                    rest = strip_routine_header(rest, wlen, &mut unextracted);
                    in_declare = true;
                }
                "CREATE" => {
                    if create_is_routine_header(rest) {
                        rest = strip_routine_header(rest, wlen, &mut unextracted);
                        in_declare = true;
                    } else {
                        stmts.push(rest.trim().to_owned());
                        break;
                    }
                }
                "GOTO" | "GET" | "RAISE" | "SIGNAL" | "RESIGNAL" | "NULL" | "PRAGMA" | "CLOSE"
                | "FETCH" | "MOVE" | "LEAVE" | "ITERATE" | "PRINT" | "THROW" | "RAISERROR"
                | "WAITFOR" | "DEALLOCATE" | "BREAK" => {
                    if word.eq_ignore_ascii_case("FETCH") {
                        let tail = &rest[wlen..];
                        if let Some(into) = find_top_level(tail, "INTO", true) {
                            invalidate_variable_list(&tail[into + "INTO".len()..], state);
                        }
                    }
                    break;
                }
                _ => {
                    // 문장 머리가 오면 선언 섹션은 끝났다 — T-SQL은 DECLARE가
                    // BEGIN..END 안의 문장이라 뒤따르는 UPDATE 등이 선언으로
                    // 오인되지 않게 실제 SQL 문장을 우선 본다.
                    if is_statement_head(word) {
                        in_declare = false;
                        record_variable_writes(rest, word, state);
                        // SELECT .. INTO <변수>·RETURNING .. INTO <변수>는
                        // 변수 귀속 절 — 벗기지 않으면 변수가 테이블로 오인된다.
                        let stmt = match word.to_ascii_uppercase().as_str() {
                            "SELECT" | "WITH" => strip_select_into(rest),
                            "INSERT" | "UPDATE" | "DELETE" => {
                                match find_top_level(rest, "RETURNING", true) {
                                    Some(r) => strip_returning_into(rest, r),
                                    None => rest.trim().to_owned(),
                                }
                            }
                            _ => rest.trim().to_owned(),
                        };
                        if !stmt.is_empty() {
                            stmts.push(stmt);
                        }
                    } else if in_declare {
                        extract_decl_default(rest, &mut stmts, state);
                    } else if let Some(p) = find_top_level(rest, ":=", false) {
                        // 대입문 — 우변의 서브쿼리·함수 호출만 SELECT로 살린다.
                        let rhs = rest[p + 2..].trim();
                        if let Some(name) = variable_name(&rest[..p]) {
                            state.assign(name, rhs);
                        }
                        if rhs.contains('(') && !is_constant_text(rhs, state) {
                            stmts.push(format!("SELECT {rhs}"));
                        }
                    } else if !bare_call(rest, &mut stmts, state) {
                        unextracted += 1;
                    }
                    break;
                }
            }
        }
    }
    (stmts, unextracted)
}

/// `;`로 끝나는 최상위 조각으로 나눈다 — 문자열·식별자 인용·주석·
/// dollar-quote·Oracle q-quote 안의 `;`는 문장 경계가 아니다.
fn split_top_level(s: &str) -> Vec<&str> {
    let quotes = QuoteScanner::new(s);
    let b = s.as_bytes();
    let n = b.len();
    let mut parts = Vec::new();
    let mut start = 0usize;
    let mut i = 0usize;
    while i < n {
        if let Some(end) = quotes.end(i) {
            i = end;
            continue;
        }
        i = match b[i] {
            b';' => {
                parts.push(&s[start..i]);
                i += 1;
                start = i;
                i
            }
            _ => i + 1,
        };
    }
    if start < n {
        parts.push(&s[start..]);
    }
    parts
}

/// 인용·주석·괄호 안이 아닌 최상위에서 needle의 첫 위치를 찾는다.
/// word_boundary가 true면 식별자 문자와 붙은 위치는 건너뛴다.
fn find_top_level(s: &str, needle: &str, word_boundary: bool) -> Option<usize> {
    let quotes = QuoteScanner::new(s);
    let b = s.as_bytes();
    let n = b.len();
    let mut depth = 0i32;
    let mut i = 0usize;
    while i < n {
        if let Some(end) = quotes.end(i) {
            i = end;
            continue;
        }
        i = match b[i] {
            b'(' => {
                depth += 1;
                i + 1
            }
            b')' => {
                depth = (depth - 1).max(0);
                i + 1
            }
            _ => {
                if depth == 0
                    && i + needle.len() <= n
                    && b[i..i + needle.len()].eq_ignore_ascii_case(needle.as_bytes())
                    && (!word_boundary || self::word_boundary(s, i, needle.len()))
                {
                    return Some(i);
                }
                i + 1
            }
        };
    }
    None
}

/// 한 문자열을 훑는 동안 인용·주석 경계를 찾는다.
///
/// 문장 분할과 키워드 탐색이 같은 인용 규칙을 써야 경계 해석이 어긋나지 않는다.
/// 닫히지 않은 `$tag$`·`q'x` 여는 표식은 닫는 표식을 문자열 끝까지 찾고, 없으면
/// 호출자가 1바이트만 전진한다. 서로 다른 닫히지 않은 표식이 n개면 스캔 한 번이
/// O(n × 길이)가 되므로, 처음 실패한 검색 뒤에 표식마다 마지막 등장 위치를 한 번 모아
/// 두고 그 위치가 찾을 범위보다 앞이면 찾지 않는다. 찾는 결과는 그대로다.
struct QuoteScanner<'a> {
    text: &'a str,
    /// 다음 `$`까지의 `$…$` 조각마다 마지막으로 시작한 위치. dollar-quote의 닫는
    /// 태그는 이 꼴로만 나타난다.
    dollar_tags: OnceCell<HashMap<&'a str, usize>>,
    /// 문자마다 `문자'`가 마지막으로 시작한 위치. q-quote의 닫는 표식이 이 꼴이다.
    q_closers: OnceCell<HashMap<char, usize>>,
}

impl<'a> QuoteScanner<'a> {
    fn new(text: &'a str) -> Self {
        Self {
            text,
            dollar_tags: OnceCell::new(),
            q_closers: OnceCell::new(),
        }
    }

    /// i에서 시작하는 인용·주석의 끝 다음 위치. 인용·주석이 아니면 None이다.
    fn end(&self, i: usize) -> Option<usize> {
        let (s, bytes) = (self.text, self.text.as_bytes());
        match bytes[i] {
            b'\'' => Some(skip_string(s, i)),
            b'"' | b'`' | b'[' => Some(skip_quoted_ident(s, i)),
            b'-' if bytes.get(i + 1) == Some(&b'-') => Some(skip_line_comment(s, i)),
            b'/' if bytes.get(i + 1) == Some(&b'*') => Some(skip_block_comment(s, i)),
            b'$' => self.dollar_quote_end(i),
            b'e' | b'E' if quote_prefix_opens(s, i) => Some(skip_escape_string(s, i + 1)),
            b'q' | b'Q' if quote_prefix_opens(s, i) => self.q_quote_end(i),
            _ => None,
        }
    }

    /// i의 `$tag$`가 여는 인용이면 닫는 태그 다음 위치. 위치표가 i 뒤에 같은 태그가
    /// 없다고 하면 찾지 않는다.
    fn dollar_quote_end(&self, i: usize) -> Option<usize> {
        let tag = dollar_tag(self.text, i)?;
        let after = i + tag.len();
        if known_absent(&self.dollar_tags, &tag, after) {
            return None;
        }
        let end = closing_delimiter_end(self.text, after, tag);
        if end.is_none() {
            self.dollar_tags
                .get_or_init(|| dollar_tag_positions(self.text));
        }
        end
    }

    /// i의 `q'x`가 여는 인용이면 닫는 `x'` 다음 위치. 위치표가 몸체 시작 뒤에 닫는
    /// 표식이 없다고 하면 찾지 않는다.
    fn q_quote_end(&self, i: usize) -> Option<usize> {
        let (close, start) = q_quote_delimiter(self.text, i)?;
        if known_absent(&self.q_closers, &close, start) {
            return None;
        }
        let end = closing_delimiter_end(self.text, start, &format!("{close}'"));
        if end.is_none() {
            self.q_closers.get_or_init(|| q_closer_positions(self.text));
        }
        end
    }
}

/// 마지막 등장 위치를 모아 뒀고 그 위치가 `from`보다 앞이면(또는 한 번도 없으면)
/// `from` 뒤에는 그 표식이 없다.
fn known_absent<K: std::hash::Hash + Eq>(
    positions: &OnceCell<HashMap<K, usize>>,
    key: &K,
    from: usize,
) -> bool {
    positions
        .get()
        .is_some_and(|positions| positions.get(key).is_none_or(|&last| last < from))
}

/// `from` 뒤 첫 `delimiter`의 끝 다음 위치.
fn closing_delimiter_end(s: &str, from: usize, delimiter: &str) -> Option<usize> {
    let found = s[from..].find(delimiter);
    #[cfg(test)]
    work::searched_bytes(found.map_or(s.len() - from, |offset| offset + delimiter.len()));
    found.map(|offset| from + offset + delimiter.len())
}

/// 연속한 `$` 두 개 사이 조각(`$…$`)마다 마지막으로 시작한 위치.
fn dollar_tag_positions(text: &str) -> HashMap<&str, usize> {
    let dollars: Vec<usize> = text
        .bytes()
        .enumerate()
        .filter_map(|(index, byte)| (byte == b'$').then_some(index))
        .collect();
    dollars
        .windows(2)
        .map(|pair| (&text[pair[0]..=pair[1]], pair[0]))
        .collect()
}

/// 작은따옴표 앞 문자마다 `문자'`가 마지막으로 시작한 위치.
fn q_closer_positions(text: &str) -> HashMap<char, usize> {
    text.char_indices()
        .filter(|&(_, character)| character == '\'')
        .filter_map(|(quote, _)| {
            let before = text[..quote].chars().next_back()?;
            Some((before, quote - before.len_utf8()))
        })
        .collect()
}

/// E-string·q-quote 접두 문자 — 식별자 중간의 `e'`·`q'`는 여는 인용이 아니다.
fn quote_prefix_opens(s: &str, i: usize) -> bool {
    let bytes = s.as_bytes();
    bytes.get(i + 1) == Some(&b'\'') && (i == 0 || !is_ident_char(bytes[i - 1]))
}

/// i의 여는 ' 다음에 오는 닫는 ' 다음 위치 — ''는 이스케이프다.
fn skip_string(s: &str, i: usize) -> usize {
    let b = s.as_bytes();
    let mut j = i + 1;
    while j < b.len() {
        if b[j] == b'\'' {
            if b.get(j + 1) == Some(&b'\'') {
                j += 2;
            } else {
                return j + 1;
            }
        } else {
            j += 1;
        }
    }
    b.len()
}

/// PG E-string의 역슬래시는 다음 문자를 이스케이프하므로 닫는 따옴표와 구분한다.
fn skip_escape_string(s: &str, i: usize) -> usize {
    let bytes = s.as_bytes();
    let mut j = i + 1;
    while j < bytes.len() {
        match bytes[j] {
            b'\\' => j = (j + 2).min(bytes.len()),
            b'\'' if bytes.get(j + 1) == Some(&b'\'') => j += 2,
            b'\'' => return j + 1,
            _ => j += 1,
        }
    }
    bytes.len()
}

/// 인용 식별자의 끝 다음 위치 — ""·``·]]는 이스케이프다.
fn skip_quoted_ident(s: &str, i: usize) -> usize {
    let b = s.as_bytes();
    let q = if b[i] == b'[' { b']' } else { b[i] };
    let mut j = i + 1;
    while j < b.len() {
        if b[j] == q {
            if b.get(j + 1) == Some(&q) {
                j += 2;
            } else {
                return j + 1;
            }
        } else {
            j += 1;
        }
    }
    b.len()
}

/// `--` 줄 주석의 끝 위치.
fn skip_line_comment(s: &str, i: usize) -> usize {
    s[i..].find('\n').map(|k| i + k + 1).unwrap_or(s.len())
}

/// `/* */` 블록 주석의 끝 위치 — pg는 중첩을 허용해 깊이로 추적한다.
fn skip_block_comment(s: &str, i: usize) -> usize {
    let b = s.as_bytes();
    let mut j = i + 2;
    let mut depth = 1;
    while j < b.len() && depth > 0 {
        if b[j] == b'/' && b.get(j + 1) == Some(&b'*') {
            depth += 1;
            j += 2;
        } else if b[j] == b'*' && b.get(j + 1) == Some(&b'/') {
            depth -= 1;
            j += 2;
        } else {
            j += 1;
        }
    }
    j
}

/// i의 `$`가 `$tag$` 여는 인용이면 닫는 태그 다음 위치 — 태그는 식별자
/// 규칙이라 `$1` 같은 위치 매개변수와 구분된다(숫자로 시작 못 함).
fn dollar_quote_end(s: &str, i: usize) -> Option<usize> {
    let tag = dollar_tag(s, i)?;
    closing_delimiter_end(s, i + tag.len(), tag)
}

/// i에서 시작하는 `$tag$` 여는 태그. 태그 안에는 `$`가 없으므로 다음 `$`까지다.
fn dollar_tag(s: &str, i: usize) -> Option<&str> {
    let b = s.as_bytes();
    let mut j = i + 1;
    if j < b.len() && (b[j].is_ascii_alphabetic() || b[j] == b'_') {
        while j < b.len() && (b[j].is_ascii_alphanumeric() || b[j] == b'_') {
            j += 1;
        }
    }
    (j < b.len() && b[j] == b'$').then(|| &s[i..=j])
}

/// Oracle `q'구분자...구분자'`의 끝 다음 위치 — 괄호 구분자는 대칭,
/// 나머지는 같은 문자가 닫는다.
fn q_quote_end(s: &str, i: usize) -> Option<usize> {
    let (close, start) = q_quote_delimiter(s, i)?;
    closing_delimiter_end(s, start, &format!("{close}'"))
}

/// q-quote의 닫는 구분 문자와 몸체 시작 위치.
fn q_quote_delimiter(s: &str, i: usize) -> Option<(char, usize)> {
    let open = s.get(i + 2..)?.chars().next()?;
    let close = match open {
        '[' => ']',
        '(' => ')',
        '{' => '}',
        '<' => '>',
        c if !c.is_whitespace() && c != '\'' => c,
        _ => return None,
    };
    Some((close, i + 2 + open.len_utf8()))
}

/// 공백과 주석(`--…`, `/*…*/`)을 건너뛴다 — 조각 선두·중간의 주석이
/// 문장 머리 판별을 가리지 않게 추출 루프에서 매 반복 적용한다.
fn skip_ws_comments(mut s: &str) -> &str {
    loop {
        s = s.trim_start();
        if let Some(rest) = s.strip_prefix("--") {
            match rest.find('\n') {
                Some(p) => s = &rest[p + 1..],
                None => return "",
            }
        } else if let Some(rest) = s.strip_prefix("/*") {
            match rest.find("*/") {
                Some(p) => s = &rest[p + 2..],
                None => return "",
            }
        } else {
            return s;
        }
    }
}

/// 조각 선두의 식별자 단어 — (단어, 바이트 길이). 선두가 식별자 문자가
/// 아니면 ("", 0)이다. Oracle은 `$`·`#`도 식별자 문자다.
fn head_word(s: &str) -> (&str, usize) {
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() && is_ident_char(b[i]) {
        i += 1;
    }
    (&s[..i], i)
}

/// 식별자 문자 — `$`는 pg 위치 매개변수·Oracle 식별자, `#`는 Oracle 식별자.
fn is_ident_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'_' | b'$' | b'#')
}

/// `식별자[.식별자]*`의 바이트 길이.
fn dotted_name_len(s: &str) -> usize {
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() && is_ident_char(b[i]) {
        i += 1;
    }
    while b.get(i) == Some(&b'.') && b.get(i + 1).is_some_and(|c| is_ident_char(*c)) {
        i += 1;
        while i < b.len() && is_ident_char(b[i]) {
            i += 1;
        }
    }
    i
}

/// `IF/ELSIF/WHEN <cond> THEN`·`WHILE <cond> LOOP`의 조건식을 추출한다.
/// 조건 안의 EXISTS/IN 서브쿼리가 테이블을 읽을 수 있어 `(`가 있으면
/// SELECT로 싼다 — 없으면 스칼라 조건이라 간선이 생기지 않는다.
/// 종결자는 가장 먼저 오는 것을 쓴다 — T-SQL은 THEN/LOOP 대신 BEGIN이
/// 몸체를 여는 언어라 호출부에서 후보를 둘 다 넘긴다.
fn strip_condition<'a>(
    rest: &'a str,
    kwlen: usize,
    terms: &[&str],
    stmts: &mut Vec<String>,
    unextracted: &mut usize,
) -> &'a str {
    let tail = &rest[kwlen..];
    let hit = terms
        .iter()
        .filter_map(|t| find_top_level(tail, t, true).map(|p| (p, t.len())))
        .min_by_key(|(p, _)| *p);
    match hit {
        Some((p, tlen)) => {
            let cond = tail[..p].trim();
            if cond.contains('(') {
                stmts.push(format!("SELECT {cond}"));
            }
            &tail[p + tlen..]
        }
        None => {
            *unextracted += 1;
            ""
        }
    }
}

/// T-SQL `EXEC <routine> <args>` — EXECUTE와 달리 기본이 routine 호출이다.
/// 문자열·변수·상수 연결식으로 확정되는 괄호 실행은 동적 SQL로 복구한다.
fn strip_exec_call(
    rest: &str,
    kwlen: usize,
    stmts: &mut Vec<String>,
    unextracted: &mut usize,
    state: &mut ExtractionState,
) {
    let tail = skip_ws_comments(&rest[kwlen..]);
    if tail.starts_with('(')
        || tail.starts_with('\'')
        || tail.starts_with("N'")
        || tail.starts_with("n'")
        || tail.starts_with('@')
        || sql_string_literal(tail).is_some()
    {
        push_dynamic_sql(tail, false, stmts, unextracted, state);
        return;
    }
    let len = dotted_name_len(tail);
    if len > 0 && !tail[..len].starts_with('(') {
        invalidate_call_arguments(&tail[len..], state);
        stmts.push(format!("CALL {}()", &tail[..len]));
    } else {
        *unextracted += 1;
    }
}

/// `FOR <var> IN <질의|범위|EXECUTE> LOOP` — IN 뒤가 질의면 문장으로 살리고,
/// EXECUTE도 문자열이 확정되면 복구한다. 숫자 범위는 의존이 없어 넘긴다.
fn strip_for<'a>(
    rest: &'a str,
    kwlen: usize,
    stmts: &mut Vec<String>,
    unextracted: &mut usize,
    state: &mut ExtractionState,
) -> &'a str {
    let tail = &rest[kwlen..];
    let Some(in_pos) = find_top_level(tail, "IN", true) else {
        *unextracted += 1;
        return "";
    };
    let after_in = &tail[in_pos + "IN".len()..];
    let Some(loop_pos) = find_top_level(after_in, "LOOP", true) else {
        *unextracted += 1;
        return "";
    };
    let inner = after_in[..loop_pos].trim();
    if starts_with_keyword(inner, "EXECUTE") {
        strip_execute(inner, "EXECUTE".len(), stmts, unextracted, state);
    } else if is_query_text(inner) {
        stmts.push(inner.to_owned());
    }
    &after_in[loop_pos + "LOOP".len()..]
}

/// `CASE <피연산자> WHEN` — 피연산자를 건너뛰고 첫 WHEN부터 다시 처리한다.
fn strip_case_operand<'a>(rest: &'a str, kwlen: usize, unextracted: &mut usize) -> &'a str {
    let tail = &rest[kwlen..];
    match find_top_level(tail, "WHEN", true) {
        Some(p) => &tail[p..],
        None => {
            *unextracted += 1;
            ""
        }
    }
}

/// `RETURN QUERY <q>`·`RETURN NEXT <e>`·`RETURN <e>` — QUERY 뒤 질의는
/// 그대로 살리고, 스칼라 식은 호출·서브쿼리(`(`)가 있을 때만 SELECT로 싼다.
fn strip_return<'a>(
    rest: &'a str,
    kwlen: usize,
    stmts: &mut Vec<String>,
    unextracted: &mut usize,
    state: &mut ExtractionState,
) -> &'a str {
    let mut tail = rest[kwlen..].trim_start();
    if starts_with_keyword(tail, "QUERY") {
        tail = tail["QUERY".len()..].trim_start();
        if starts_with_keyword(tail, "EXECUTE") {
            strip_execute(tail, "EXECUTE".len(), stmts, unextracted, state);
        } else if !tail.is_empty() {
            stmts.push(tail.to_owned());
        }
        return "";
    }
    if starts_with_keyword(tail, "NEXT") {
        tail = tail["NEXT".len()..].trim_start();
    }
    if tail.contains('(') {
        stmts.push(format!("SELECT {tail}"));
    }
    ""
}

/// `OPEN <cursor> FOR <질의>` — FOR 뒤의 질의를 살린다.
fn strip_open<'a>(
    rest: &'a str,
    kwlen: usize,
    stmts: &mut Vec<String>,
    unextracted: &mut usize,
    state: &mut ExtractionState,
) -> &'a str {
    let tail = &rest[kwlen..];
    if let Some(p) = find_top_level(tail, "FOR", true) {
        let inner = tail[p + "FOR".len()..].trim();
        if starts_with_keyword(inner, "EXECUTE") {
            strip_execute(inner, "EXECUTE".len(), stmts, unextracted, state);
        } else if is_query_text(inner) {
            stmts.push(inner.to_owned());
        } else {
            // Oracle OPEN .. FOR는 EXECUTE 없이 문자열을 직접 받는다.
            push_dynamic_sql(inner, true, stmts, unextracted, state);
        }
    }
    ""
}

/// `EXECUTE` 뒤의 제한된 상수 텍스트 식만 SQL로 복구한다. PostgreSQL
/// `format()`도 evaluator가 지원하는 형식과 알려진 인자일 때만 통과한다.
/// `EXECUTE FUNCTION/PROCEDURE f()`는 CALL로 재작성한다 — 인자는 이름
/// 해석에 필요 없어 버린다.
fn strip_execute(
    rest: &str,
    kwlen: usize,
    stmts: &mut Vec<String>,
    unextracted: &mut usize,
    state: &mut ExtractionState,
) {
    let mut tail = skip_ws_comments(&rest[kwlen..]);
    if starts_with_keyword(tail, "IMMEDIATE") {
        tail = skip_ws_comments(&tail["IMMEDIATE".len()..]);
    }
    for kw in ["FUNCTION", "PROCEDURE"] {
        if starts_with_keyword(tail, kw) {
            let name = tail[kw.len()..].trim_start();
            let len = dotted_name_len(name);
            if len > 0 {
                invalidate_call_arguments(&name[len..], state);
                stmts.push(format!("CALL {}()", &name[..len]));
                return;
            }
        }
    }
    push_dynamic_sql(tail, true, stmts, unextracted, state);
}

/// 리터럴 한 개와 소비하지 않은 꼬리를 함께 돌려 연결식을 놓치지 않는다.
fn sql_string_literal(rest: &str) -> Option<(std::borrow::Cow<'_, str>, &str)> {
    if rest.starts_with('\'') {
        return Some((unquote_sql_string(rest)?, &rest[skip_string(rest, 0)..]));
    }
    if rest.starts_with("N'") || rest.starts_with("n'") {
        return Some((
            unquote_sql_string(&rest[1..])?,
            &rest[skip_string(rest, 1)..],
        ));
    }
    if rest.starts_with('$') {
        return Some((undollar_quote(rest)?, &rest[dollar_quote_end(rest, 0)?..]));
    }
    if rest.starts_with("q'") || rest.starts_with("Q'") {
        let end = q_quote_end(rest, 0)?;
        let delimiter_len = rest.get(2..)?.chars().next()?.len_utf8();
        let inner = rest.get(2 + delimiter_len..end - delimiter_len - 1)?;
        return Some((std::borrow::Cow::Borrowed(inner), &rest[end..]));
    }
    None
}

/// SQL 본문이 끝났는지 확인한 뒤에만 간선 후보로 넘긴다. AT 같은 원격 실행
/// 꼬리는 로컬 객체로 오귀속하지 않고 미추출로 남긴다.
fn push_dynamic_sql(
    tail: &str,
    allow_bindings: bool,
    stmts: &mut Vec<String>,
    unextracted: &mut usize,
    state: &mut ExtractionState,
) {
    let tail = skip_ws_comments(tail);
    let Some((expression, remaining)) = split_dynamic_expression(tail) else {
        *unextracted += 1;
        return;
    };
    let Some(sql) = state.evaluate(expression) else {
        state.record_unfolded_dynamic_sql();
        *unextracted += 1;
        return;
    };
    if !(remaining.is_empty() || allow_bindings && dynamic_binding_head(remaining).is_some()) {
        *unextracted += 1;
        return;
    }
    stmts.push(sql);
    collect_dynamic_bindings(remaining, stmts, unextracted);
}

/// 동적 SQL 식과 `INTO`·`USING` 바인딩 꼬리를 나눈다. 인용·괄호 안의
/// 키워드는 식의 데이터이므로 최상위 위치에서만 경계를 찾는다.
fn split_dynamic_expression(rest: &str) -> Option<(&str, &str)> {
    let boundary = ["RETURNING", "INTO", "USING", "BULK"]
        .iter()
        .filter_map(|keyword| find_top_level(rest, keyword, true))
        .min();
    let (expression, remaining) = match boundary {
        Some(position) => (&rest[..position], &rest[position..]),
        None => (rest, ""),
    };
    let expression = expression.trim();
    (!expression.is_empty()).then_some((expression, remaining.trim_start()))
}

/// INTO 대상·USING 인자는 SQL 문자열 밖의 식이다. 함수 호출이 있으면
/// 의존성이 생기므로 버리지 않고 별도 SELECT로 파싱한다.
fn dynamic_binding_head(mut rest: &str) -> Option<&str> {
    if starts_with_keyword(rest, "RETURNING") {
        rest = skip_ws_comments(&rest["RETURNING".len()..]);
    }
    if starts_with_keyword(rest, "BULK") {
        rest = skip_ws_comments(&rest["BULK".len()..]);
        if !starts_with_keyword(rest, "COLLECT") {
            return None;
        }
        rest = skip_ws_comments(&rest["COLLECT".len()..]);
    }
    for keyword in ["INTO", "USING"] {
        if starts_with_keyword(rest, keyword) {
            let mut tail = skip_ws_comments(&rest[keyword.len()..]);
            if keyword == "INTO" && starts_with_keyword(tail, "STRICT") {
                tail = skip_ws_comments(&tail["STRICT".len()..]);
            }
            return Some(tail);
        }
    }
    None
}

/// 바인딩 절의 복잡한 식을 SQL 파서로 넘겨, 해석 실패도 기존 미추출 집계에 싣는다.
fn collect_dynamic_bindings(mut rest: &str, stmts: &mut Vec<String>, unextracted: &mut usize) {
    while !rest.is_empty() {
        let Some(expressions) = dynamic_binding_head(rest) else {
            *unextracted += 1;
            return;
        };
        let end = ["INTO", "USING", "RETURNING", "BULK"]
            .iter()
            .filter_map(|keyword| find_top_level(expressions, keyword, true))
            .min()
            .unwrap_or(expressions.len());
        stmts.push(format!("SELECT {}", expressions[..end].trim()));
        rest = skip_ws_comments(&expressions[end..]);
    }
}

/// `EXIT/CONTINUE WHEN <cond>`·`ASSERT <cond>`의 꼬리 조건 — `(`가
/// 있으면 서브쿼리·호출이 있을 수 있어 SELECT로 싼다.
fn strip_tail_condition(rest: &str, kwlen: usize, stmts: &mut Vec<String>) {
    let tail = &rest[kwlen..];
    let cond = match find_top_level(tail, "WHEN", true) {
        Some(p) => tail[p + "WHEN".len()..].trim(),
        None => tail.trim(),
    };
    if cond.contains('(') {
        stmts.push(format!("SELECT {cond}"));
    }
}

/// 몸체를 꺼내 따라 내려갈 중첩 DO 블록의 최대 깊이.
///
/// 실제 몸체의 DO 중첩은 한두 단계다. 상한이 없으면 태그를 바꿔 가며 중첩한 몸체
/// 하나로 추출 재귀가 스택을 넘고, 단계마다 남은 몸체를 복사해 메모리가 GB 단위가 됐다.
const MAX_DO_NESTING: usize = 32;

/// `DO $$..$$`·`DO '..'` 중첩 익명 블록 — 몸체를 꺼내 재귀 추출한다.
/// 깊이 상한을 넘거나 몸체를 꺼내지 못한 블록은 미추출로 센다.
fn strip_do<'a>(
    rest: &'a str,
    kwlen: usize,
    stmts: &mut Vec<String>,
    unextracted: &mut usize,
    state: &mut ExtractionState,
) -> &'a str {
    let inner = (state.do_depth < MAX_DO_NESTING)
        .then(|| do_block_body(&rest[kwlen..]))
        .flatten();
    let Some(body) = inner else {
        *unextracted += 1;
        return "";
    };
    state.do_depth += 1;
    let (mut nested, nested_unextracted) = extract_procedural_statements(&body, state, false);
    state.do_depth -= 1;
    stmts.append(&mut nested);
    *unextracted += nested_unextracted;
    ""
}

/// DO 뒤 인용 몸체. dollar-quote는 원문을 빌려 단계마다 남은 몸체를 복사하지 않는다.
fn do_block_body(tail: &str) -> Option<std::borrow::Cow<'_, str>> {
    let tail = tail.trim_start();
    if tail.starts_with('$') {
        undollar_quote(tail)
    } else if tail.starts_with('\'') {
        unquote_sql_string(tail)
    } else {
        None
    }
}

/// `PROCEDURE p(a int)`·`FUNCTION f RETURN t IS`·`PACKAGE BODY x IS` 같은
/// 정의 헤더를 IS/AS까지 건너뛴다 — 인자 목록은 괄호 안이라 최상위
/// 키워드 검색이 건너뛴다. 헤더 뒤는 선언부다.
fn strip_routine_header<'a>(rest: &'a str, kwlen: usize, unextracted: &mut usize) -> &'a str {
    let tail = &rest[kwlen..];
    let pos = find_top_level(tail, "IS", true)
        .into_iter()
        .chain(find_top_level(tail, "AS", true))
        .min();
    match pos {
        Some(p) => &tail[p + 2..],
        None => {
            *unextracted += 1;
            ""
        }
    }
}

/// `CREATE [OR REPLACE] PROCEDURE/FUNCTION/PACKAGE` 꼴이면 routine 헤더다 —
/// CREATE TABLE 같은 진짜 DDL과 구분해 헤더만 건너뛰게 한다.
fn create_is_routine_header(rest: &str) -> bool {
    let mut tail = rest["CREATE".len()..].trim_start();
    for _ in 0..6 {
        if let Some(t) = tail.strip_prefix('=') {
            // DEFINER=user 같은 절의 값 부분을 건너뛴다.
            let t = t.trim_start();
            let (_, l) = head_word(t);
            tail = t[l..].trim_start();
        }
        let (w, l) = head_word(tail);
        match w.to_ascii_uppercase().as_str() {
            "OR" | "REPLACE" | "NONEDITIONABLE" | "EDITIONABLE" | "GLOBAL" | "TEMPORARY"
            | "DEFINER" | "ALGORITHM" | "SQL" | "SECURITY" | "INVOKER" => {
                tail = tail[l..].trim_start();
            }
            "PROCEDURE" | "FUNCTION" | "PACKAGE" => return true,
            _ => return false,
        }
    }
    false
}

/// SQL 문장이 변수에 결과를 쓰면 그 값을 상수로 유지하지 않는다. 한 행인지
/// 여러 행인지, 실행 시 오류가 나는지까지 절차형 추출기에서 판정할 수 없기
/// 때문에 이후 동적 SQL은 unknown으로 남겨야 한다.
fn record_variable_writes(stmt: &str, head: &str, state: &mut ExtractionState) {
    let upper = head.to_ascii_uppercase();
    if upper == "CALL" {
        invalidate_call_arguments(&stmt[head.len()..], state);
        return;
    }
    if upper == "SELECT" || upper == "WITH" {
        if upper == "WITH" {
            // CTE 내부의 T-SQL 변수 대입 범위를 문장 추출기만으로
            // 구분하지 못하므로, 기존 값을 보존하지 않는다.
            state.invalidate_all_variables();
        }
        if let Some(position) =
            find_top_level(stmt, "INTO", true).or_else(|| find_top_level(stmt, "BULK", true))
        {
            let after = &stmt[position..];
            let target_start = if starts_with_keyword(after, "BULK") {
                let after_bulk = skip_ws_comments(&after["BULK".len()..]);
                if starts_with_keyword(after_bulk, "COLLECT") {
                    skip_ws_comments(&after_bulk["COLLECT".len()..])
                } else {
                    after_bulk
                }
            } else {
                skip_ws_comments(&after["INTO".len()..])
            };
            let target_start = if starts_with_keyword(target_start, "STRICT") {
                skip_ws_comments(&target_start["STRICT".len()..])
            } else {
                target_start
            };
            let end = find_top_level(target_start, "FROM", true).unwrap_or(target_start.len());
            invalidate_variable_list(&target_start[..end], state);
        }
        record_select_assignments(stmt, head, state);
        return;
    }
    if matches!(upper.as_str(), "INSERT" | "UPDATE" | "DELETE") {
        let Some(returning) = find_top_level(stmt, "RETURNING", true) else {
            return;
        };
        let after = &stmt[returning + "RETURNING".len()..];
        if let Some(into) = find_top_level(after, "INTO", true) {
            let targets = skip_ws_comments(&after[into + "INTO".len()..]);
            let targets = if starts_with_keyword(targets, "STRICT") {
                &targets["STRICT".len()..]
            } else {
                targets
            };
            invalidate_variable_list(targets, state);
        }
    }
}

/// T-SQL `SELECT @a = ..., @b = ...`의 모든 대입을 처리한다. 일부 조각을
/// 해석하지 못하면 전체 환경을 비워 stale 변수로 SQL을 추측하지 않는다.
fn record_select_assignments(stmt: &str, head: &str, state: &mut ExtractionState) {
    let after_head = skip_ws_comments(&stmt[head.len()..]);
    let end = ["FROM", "INTO"]
        .iter()
        .filter_map(|keyword| find_top_level(after_head, keyword, true))
        .min()
        .unwrap_or(after_head.len());
    let mut assignments = Vec::new();
    let mut saw_assignment = false;
    let mut unmodeled = false;
    for item in split_top_level_commas(&after_head[..end]) {
        let item = item.trim();
        let Some(eq) = find_top_level(item, "=", false) else {
            if saw_assignment && !item.is_empty() {
                unmodeled = true;
            }
            continue;
        };
        let lhs = item[..eq].trim();
        let Some(name) = variable_name(lhs).filter(|name| lhs[name.len()..].trim().is_empty())
        else {
            if saw_assignment {
                unmodeled = true;
            }
            continue;
        };
        saw_assignment = true;
        assignments.push((name.to_owned(), item[eq + 1..].trim().to_owned()));
    }
    if unmodeled {
        state.invalidate_all_variables();
        return;
    }
    if assignments.is_empty() {
        return;
    }
    let targets: BTreeSet<String> = assignments
        .iter()
        .map(|(name, _)| normalize_variable_name(name))
        .collect();
    // 오른쪽 식을 모두 대입 전 환경으로 먼저 평가한 뒤 적용한다 — 동시 대입이라 앞선
    // 대입 결과를 보지 않는다. 같은 문장의 대상을 참조하는 식이나 타입·용량 제약으로
    // 불명확해진 값은 이전 값을 되살리지 않고 지운다.
    let values: Vec<(String, Option<String>)> = assignments
        .iter()
        .map(|(name, expression)| {
            let key = normalize_variable_name(name);
            let value = (!constant_sql::references_variables(expression, state.dialect, &targets))
                .then(|| state.assigned_value(&key, expression))
                .flatten();
            (key, value)
        })
        .collect();
    for (key, value) in values.into_iter().filter(|(key, _)| !key.is_empty()) {
        state.set_variable(key, value);
    }
}

fn invalidate_variable_list(list: &str, state: &mut ExtractionState) {
    let end = ["FROM", "RETURNING", "USING"]
        .iter()
        .filter_map(|keyword| find_top_level(list, keyword, true))
        .min()
        .unwrap_or(list.len());
    for target in split_top_level_commas(&list[..end]) {
        if let Some(name) = variable_name(target.trim()) {
            state.invalidate_variable(name);
        }
    }
}

fn split_top_level_commas(s: &str) -> Vec<&str> {
    let quotes = QuoteScanner::new(s);
    let bytes = s.as_bytes();
    let mut parts = Vec::new();
    let mut start = 0;
    let mut depth = 0i32;
    let mut position = 0;
    while position < bytes.len() {
        if let Some(end) = quotes.end(position) {
            position = end;
            continue;
        }
        match bytes[position] {
            b'(' => depth += 1,
            b')' => depth = (depth - 1).max(0),
            b',' if depth == 0 => {
                parts.push(&s[start..position]);
                start = position + 1;
            }
            _ => {}
        }
        position += 1;
    }
    parts.push(&s[start..]);
    parts
}

/// 호출 인자 중 변수는 routine의 OUT/INOUT 또는 드라이버별 output 표기일
/// 수 있으므로 호출 뒤의 확정값으로 재사용하지 않는다.
fn invalidate_call_arguments(rest: &str, state: &mut ExtractionState) {
    let arguments = parenthesized_arguments(rest).unwrap_or(rest);
    for argument in split_top_level_commas(arguments) {
        let argument = strip_call_mode(argument.trim());
        if let Some(name) = variable_name(argument) {
            state.invalidate_variable(name);
        }
    }
}

fn parenthesized_arguments(rest: &str) -> Option<&str> {
    let quotes = QuoteScanner::new(rest);
    let bytes = rest.as_bytes();
    let mut position = 0;
    let mut open = None;
    let mut depth = 0i32;
    while position < bytes.len() {
        if let Some(end) = quotes.end(position) {
            position = end;
            continue;
        }
        match bytes[position] {
            b'(' if open.is_none() => {
                open = Some(position + 1);
                depth = 1;
            }
            b'(' if open.is_some() => depth += 1,
            b')' if open.is_some() => {
                depth -= 1;
                if depth == 0 {
                    let start = open?;
                    return Some(&rest[start..position]);
                }
            }
            _ => {}
        }
        position += 1;
    }
    None
}

fn strip_call_mode(mut argument: &str) -> &str {
    for mode in ["IN OUT", "INOUT", "OUTPUT", "OUT", "IN"] {
        if argument.len() >= mode.len()
            && argument.as_bytes()[..mode.len()].eq_ignore_ascii_case(mode.as_bytes())
            && (argument.len() == mode.len()
                || argument.as_bytes()[mode.len()].is_ascii_whitespace())
        {
            argument = skip_ws_comments(&argument[mode.len()..]);
            break;
        }
    }
    argument
}

/// `SELECT .. [BULK COLLECT] INTO <변수> [FROM ..]` — plpgsql/plsql의 변수
/// 귀속 절을 벗긴다. 벗기지 않으면 변수명이 FROM 없는 테이블로 오인된다.
fn strip_select_into(stmt: &str) -> String {
    let cut = find_top_level(stmt, "BULK", true).or_else(|| find_top_level(stmt, "INTO", true));
    let Some(cut) = cut else {
        return stmt.trim().to_owned();
    };
    match find_top_level(&stmt[cut..], "FROM", true).map(|p| cut + p) {
        Some(f) => format!("{} {}", stmt[..cut].trim_end(), &stmt[f..]),
        None => stmt[..cut].trim_end().to_owned(),
    }
}

/// `INSERT/UPDATE/DELETE .. RETURNING <cols> INTO <변수>` — RETURNING의
/// INTO 뒤는 변수 귀속이라 잘라낸다(변수는 항상 절의 마지막이다).
fn strip_returning_into(stmt: &str, returning_pos: usize) -> String {
    let after = &stmt[returning_pos + "RETURNING".len()..];
    match find_top_level(after, "INTO", true) {
        Some(p) => stmt[..returning_pos + "RETURNING".len() + p]
            .trim_end()
            .to_owned(),
        None => stmt.trim().to_owned(),
    }
}

/// 방언 표기(`@name`)와 관계없이 변수 환경에서 사용할 키를 만든다.
fn normalize_variable_name(name: &str) -> String {
    name.trim().trim_start_matches('@').to_ascii_lowercase()
}

/// routine header의 parameter 이름만 수집한다. 몸체에 우연히 등장한 identifier는
/// symbol로 승격하지 않아 SQL 컬럼 오타를 숨기지 않는다.
fn routine_parameter_symbols(body: &str) -> BTreeSet<String> {
    let routine = ["PROCEDURE", "FUNCTION"]
        .iter()
        .flat_map(|keyword| {
            find_all_keywords(body, keyword)
                .into_iter()
                .map(move |position| (position, *keyword))
        })
        .min_by_key(|(position, _)| *position);
    let Some((position, keyword)) = routine else {
        return BTreeSet::new();
    };
    let mut tail = skip_ws_comments(&body[position + keyword.len()..]);
    let name_len = dotted_name_len(tail);
    if name_len == 0 {
        return BTreeSet::new();
    }
    tail = skip_ws_comments(&tail[name_len..]);
    if tail.starts_with('(') {
        let Some(end) = find_parameter_list_end(tail) else {
            return BTreeSet::new();
        };
        let parameters = &tail[1..end - 1];
        return split_top_level_commas(parameters)
            .into_iter()
            .filter_map(variable_name)
            .map(normalize_variable_name)
            .filter(|name| !name.is_empty())
            .collect();
    }

    let header_end = find_top_level(tail, "AS", true)
        .into_iter()
        .chain(find_top_level(tail, "IS", true))
        .min()
        .unwrap_or(tail.len());
    declared_at_names(&tail[..header_end])
}

fn declared_at_names(text: &str) -> BTreeSet<String> {
    let quotes = QuoteScanner::new(text);
    let mut names = BTreeSet::new();
    let mut index = 0usize;
    while index < text.len() {
        if let Some(end) = quotes.end(index) {
            index = end;
            continue;
        }
        if text.as_bytes()[index] == b'@'
            && text.as_bytes().get(index + 1) != Some(&b'@')
            && (index == 0 || text.as_bytes()[index - 1] != b'@')
        {
            if let Some(name) = variable_name(&text[index..]) {
                names.insert(normalize_variable_name(name));
                index += name.len();
                continue;
            }
        }
        index += 1;
    }
    names
}

/// 대입 왼쪽 또는 선언 시작에서 단순 변수 이름을 꺼낸다. 선언의 나머지
/// 타입 정보는 호출자가 따로 해석하지 않으므로 첫 이름 뒤는 무시한다.
fn variable_name(fragment: &str) -> Option<&str> {
    let fragment = skip_ws_comments(fragment);
    let bytes = fragment.as_bytes();
    let mut end = 0;
    if bytes.get(end) == Some(&b'@') {
        end += 1;
    }
    while bytes
        .get(end)
        .is_some_and(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'$' | b'#'))
    {
        end += 1;
    }
    if end == 0 || (end == 1 && bytes[0] == b'@') {
        None
    } else {
        Some(&fragment[..end])
    }
}

fn is_constant_text(expression: &str, state: &ExtractionState) -> bool {
    state.evaluate(expression).is_some()
}

fn declaration_type<'a>(chunk: &'a str, name: &str) -> &'a str {
    let start = skip_ws_comments(chunk);
    let after_name = &start[name.len()..];
    let end = find_top_level(after_name, ":=", false)
        .or_else(|| find_top_level(after_name, "DEFAULT", true))
        .or_else(|| find_top_level(after_name, "=", false))
        .unwrap_or(after_name.len());
    after_name[..end].trim()
}

/// 선언 조각에서 의존이 될 수 있는 질의만 꺼낸다 — `:=`·`DEFAULT`의
/// 서브쿼리와 `CURSOR FOR <질의>`(plpgsql·T-SQL 공통). 선언 그 자체는
/// 미추출로 세지 않는다 — 의존이 아니라 변수 선언일 뿐이다.
fn extract_decl_default(chunk: &str, stmts: &mut Vec<String>, state: &mut ExtractionState) {
    let rhs = find_top_level(chunk, ":=", false)
        .map(|p| &chunk[p + 2..])
        .or_else(|| find_top_level(chunk, "DEFAULT", true).map(|p| &chunk[p + "DEFAULT".len()..]))
        .or_else(|| find_top_level(chunk, "=", false).map(|p| &chunk[p + 1..]));
    if let Some(name) = variable_name(chunk) {
        let type_text = declaration_type(chunk, name);
        if let Some(rhs) = rhs.map(str::trim).filter(|rhs| !rhs.is_empty()) {
            state.declare(name, type_text, Some(rhs));
            if rhs.contains('(') && !is_constant_text(rhs, state) {
                stmts.push(format!("SELECT {rhs}"));
                return;
            }
        } else {
            state.declare(name, type_text, None);
        }
    }
    if let Some(rhs) = rhs.map(str::trim).filter(|r| r.contains('(')) {
        if !is_constant_text(rhs, state) {
            stmts.push(format!("SELECT {rhs}"));
        }
        return;
    }
    if let Some(p) = find_top_level(chunk, "FOR", true) {
        let tail = chunk[p + "FOR".len()..].trim();
        if is_query_text(tail) {
            stmts.push(tail.to_owned());
        }
    }
}

/// `f(a)`·`pkg.f(a)` 꼴의 bare 호출을 `CALL f(a)`로 재작성한다 —
/// PL/SQL은 CALL 없이 프로시저를 부른다. 대상이 routine이 아니면
/// resolve 단계에서 내장 함수·미수집으로 걸러진다.
fn bare_call(rest: &str, stmts: &mut Vec<String>, state: &mut ExtractionState) -> bool {
    let n = dotted_name_len(rest);
    if n > 0 && rest[n..].trim_start().starts_with('(') {
        invalidate_call_arguments(&rest[n..], state);
        stmts.push(format!("CALL {}", rest.trim()));
        true
    } else {
        false
    }
}

/// 조각 선두가 이 단어면 그대로 SQL 문장으로 넘긴다 — CREATE·EXECUTE·DO는
/// 별도 처리가 필요해 여기 두지 않는다.
fn is_statement_head(word: &str) -> bool {
    matches!(
        word.to_ascii_uppercase().as_str(),
        "SELECT"
            | "INSERT"
            | "UPDATE"
            | "DELETE"
            | "MERGE"
            | "WITH"
            | "VALUES"
            | "TABLE"
            | "CALL"
            | "SET"
            | "SHOW"
            | "EXPLAIN"
            | "TRUNCATE"
            | "COMMENT"
            | "ANALYZE"
            | "VACUUM"
            | "GRANT"
            | "REVOKE"
            | "DENY"
            | "RENAME"
            | "LOCK"
            | "UNLOCK"
            | "COMMIT"
            | "ROLLBACK"
            | "SAVEPOINT"
            | "RELEASE"
            | "START"
            | "DESCRIBE"
            | "DESC"
            | "USE"
            | "DISCARD"
            | "LISTEN"
            | "NOTIFY"
            | "COPY"
            | "ALTER"
            | "DROP"
            | "REINDEX"
            | "CHECKPOINT"
            | "CLUSTER"
            | "IMPORT"
    )
}

/// 텍스트가 질의문인지 — FOR .. IN이나 OPEN .. FOR 뒤를 걸러내는 데 쓴다.
fn is_query_text(s: &str) -> bool {
    let (w, _) = head_word(s.trim_start());
    w.eq_ignore_ascii_case("select")
        || w.eq_ignore_ascii_case("with")
        || w.eq_ignore_ascii_case("values")
        || w.eq_ignore_ascii_case("table")
        || s.trim_start().starts_with('(')
}

/// 소유한 문장 하나의 쓰기/읽기/발사-컬럼 참조를 모은다.
///
/// 컬럼 DML은 복제하지 않고 옮겨 보관하고, 절차형 블록(IF/WHILE/CASE)은 안쪽 문장을
/// 꺼내 같은 규칙으로 모은다. 컬럼 DML에는 안쪽 문장이 없으므로 처음 나온 순서가
/// 참조로 훑던 때와 같다.
fn collect_trigger_stmt(
    stmt: Statement,
    parsed: &mut ParsedTrigger,
    sqlserver_trigger: bool,
    sqlite_trigger: bool,
) {
    collect_trigger_facts(&stmt, parsed, sqlserver_trigger, sqlite_trigger);
    if is_column_dml_statement(&stmt) {
        parsed.retain_dml_statement(stmt);
        return;
    }
    for nested in into_nested_statements(stmt) {
        collect_trigger_stmt(nested, parsed, sqlserver_trigger, sqlite_trigger);
    }
}

/// 문장 하나의 관계·식·호출 참조를 모은다. 안쪽 문장은 호출자가 따로 넘긴다.
fn collect_trigger_facts(
    stmt: &Statement,
    parsed: &mut ParsedTrigger,
    sqlserver_trigger: bool,
    sqlite_trigger: bool,
) {
    if let Statement::CreateTable(create) = stmt {
        let local = create.temporary
            || create
                .name
                .0
                .last()
                .and_then(|part| part.as_ident())
                .is_some_and(|ident| ident.value.starts_with('#'));
        if local {
            if let Some(target) = relation_name_parts(&create.name) {
                parsed.local_relations.insert(target);
            }
        }
    }
    if let Some(into) = select_into(stmt) {
        let local = into.temporary
            || into
                .name
                .0
                .last()
                .and_then(|part| part.as_ident())
                .is_some_and(|ident| ident.value.starts_with('#'));
        if local {
            if let Some(target) = relation_name_parts(&into.name) {
                parsed.local_relations.insert(target);
            }
        }
    }
    if matches!(
        stmt,
        Statement::If(_) | Statement::While(_) | Statement::Case(_)
    ) && nested_statements(stmt)
        .iter()
        .any(|nested| is_column_dml_statement(nested))
    {
        parsed.partial = true;
        parsed.diagnostics.push(schemagraph_core::Diagnostic {
            code: "SG_DML_CONTROL_FLOW".into(),
            message: "DML inside conditional or loop control flow is analyzed conservatively"
                .into(),
            location: None,
            candidates: Vec::new(),
        });
    }
    // write_targets는 절차형 블록까지 재귀로 본다 — 블록 안의 UPDATE 대상이
    // 부모 문장의 visit_relations에서 reads로 오인되지 않게 미리 다 알아야 한다.
    let write_targets = write_targets(stmt);
    let qualified_writes = qualified_write_targets(stmt);
    for target in &qualified_writes {
        if !parsed.qualified_writes.contains(target) {
            parsed.qualified_writes.push(target.clone());
        }
    }
    for t in &write_targets {
        if parsed.local_relations.contains(t) {
            continue;
        }
        if !parsed.writes.contains(t) {
            parsed.writes.push(t.clone());
        }
    }
    body_relations::collect_reads(stmt, parsed, sqlserver_trigger);
    let _ = visit_expressions(stmt, |e| {
        match e {
            Expr::CompoundIdentifier(parts) => {
                if parts.len() == 2 {
                    let q = parts[0].value.to_ascii_lowercase();
                    let col = parts[1].value.clone();
                    if (q == "new" || q == "old") && !parsed.fired_columns.contains(&col) {
                        parsed.fired_columns.push(col);
                    }
                }
            }
            Expr::CompoundFieldAccess { root, access_chain } => {
                if let (
                    Expr::Value(value),
                    [sqlparser::ast::AccessExpr::Dot(Expr::Identifier(column))],
                ) = (root.as_ref(), access_chain.as_slice())
                {
                    if let sqlparser::ast::Value::Placeholder(pseudo) = &value.value {
                        let pseudo = pseudo.trim_start_matches(':');
                        if (pseudo.eq_ignore_ascii_case("new")
                            || pseudo.eq_ignore_ascii_case("old"))
                            && !parsed.fired_columns.contains(&column.value)
                        {
                            parsed.fired_columns.push(column.value.clone());
                        }
                    }
                }
            }
            // 몸체 안의 함수 호출 — 내장 함수는 routine이 아니라 걸러야
            // count() 같은 호출이 "카탈로그에 없음" 노이즈를 만들지 않는다.
            Expr::Function(f) => {
                // SQLite RAISE는 trigger 전용 구문이다. visitor는 계속 내려가므로
                // 오류 메시지의 서브쿼리·사용자 함수·NEW/OLD 읽기는 그대로 남는다.
                if sqlite_trigger && sqlite::raise_action(f).is_some() {
                    return ControlFlow::<()>::Continue(());
                }
                let t = object_name_parts(&f.name);
                let quoted_name = f
                    .name
                    .0
                    .last()
                    .and_then(|part| part.as_ident())
                    .is_some_and(|ident| ident.quote_style.is_some());
                if !t.1.is_empty()
                    && (quoted_name || !is_builtin_function(&t.1))
                    && !parsed.calls.contains(&t)
                {
                    parsed.calls.push(t);
                }
            }
            _ => {}
        }
        ControlFlow::<()>::Continue(())
    });
    // CALL proc(...) 문장 — sqlparser가 파싱하는 방언이면 여기 잡힌다.
    if let Statement::Call(call) = stmt {
        let t = object_name_parts(&call.name);
        if !t.1.is_empty() && !parsed.calls.contains(&t) {
            parsed.calls.push(t);
        }
    }
}

/// 절차형 블록(IF/WHILE/CASE) 안의 문장을 nested_statements와 같은 순서로 옮겨 꺼낸다.
///
/// 도매 파싱이 성공해도 DML·CALL이 블록 안에 중첩돼 있어, 내려가지 않으면 writes가
/// reads로 오분류되고 CALL이 조용히 빠진다. 관계·식 수확은 visitor가 이미 트리 전체를
/// 보지만, 쓰기 대상과 DML 효과는 문장마다 모아야 한다.
fn into_nested_statements(stmt: Statement) -> Vec<Statement> {
    use sqlparser::ast::ConditionalStatements;
    let blocks: Vec<sqlparser::ast::ConditionalStatementBlock> = match stmt {
        Statement::If(s) => std::iter::once(s.if_block)
            .chain(s.elseif_blocks)
            .chain(s.else_block)
            .collect(),
        Statement::While(s) => vec![s.while_block],
        Statement::Case(s) => s.when_blocks.into_iter().chain(s.else_block).collect(),
        _ => return Vec::new(),
    };
    blocks
        .into_iter()
        .flat_map(|block| match block.conditional_statements {
            ConditionalStatements::Sequence { statements } => statements,
            ConditionalStatements::BeginEnd(block) => block.statements,
        })
        .collect()
}

fn is_column_dml_statement(stmt: &Statement) -> bool {
    matches!(
        stmt,
        Statement::Insert(_)
            | Statement::Update { .. }
            | Statement::Merge { .. }
            | Statement::CreateTable(sqlparser::ast::CreateTable { query: Some(_), .. })
    ) || select_into(stmt).is_some()
}

fn select_into(stmt: &Statement) -> Option<&sqlparser::ast::SelectInto> {
    let Statement::Query(query) = stmt else {
        return None;
    };
    query_select_into(query)
}

/// 집합 질의의 목적지는 첫 SELECT에 선언되며 값의 원천은 전체 질의에서 구한다.
fn query_select_into(query: &sqlparser::ast::Query) -> Option<&sqlparser::ast::SelectInto> {
    let mut body = query.body.as_ref();
    loop {
        match body {
            sqlparser::ast::SetExpr::Select(select) => return select.into.as_ref(),
            sqlparser::ast::SetExpr::SetOperation { left, .. } => body = left.as_ref(),
            sqlparser::ast::SetExpr::Query(query) => body = query.body.as_ref(),
            _ => return None,
        }
    }
}

fn is_sqlserver_transition_relation(relation: &(Option<String>, String)) -> bool {
    relation.0.is_none()
        && (relation.1.eq_ignore_ascii_case("inserted")
            || relation.1.eq_ignore_ascii_case("deleted"))
}

/// 내장 함수 이름 — 이 목록에 있으면 routine 해석·한계 보고 모두 건너뛴다.
/// 빠진 내장 함수는 "카탈로그에 없음" notes로 가는데 그것이 정직하다:
/// 실제로 그래프에 없는 호출이므로, 소비자가 내장 함수로 판별할 수 있다.
fn is_builtin_function(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        // 집계·수학
        "count" | "sum" | "avg" | "min" | "max" | "abs" | "ceil" | "ceiling" | "floor"
            | "round" | "truncate" | "mod" | "power" | "pow" | "sqrt" | "exp" | "ln" | "log"
            | "log2" | "log10" | "sign" | "pi" | "rand" | "random" | "greatest" | "least"
            | "stddev" | "stddev_pop" | "stddev_samp" | "var_pop" | "var_samp" | "variance"
            // 문자열
            | "concat" | "concat_ws" | "substring" | "substr" | "position" | "length"
            | "char_length" | "character_length" | "octet_length" | "upper" | "lower" | "ucase"
            | "lcase" | "trim" | "ltrim" | "rtrim" | "btrim" | "replace" | "reverse" | "repeat"
            | "left" | "right" | "lpad" | "rpad" | "instr" | "locate" | "ascii" | "chr" | "char"
            | "ord" | "soundex" | "format" | "quote_ident" | "quote_literal" | "translate"
            | "initcap" | "split_part" | "strpos" | "to_hex" | "md5" | "string_agg"
            | "group_concat" | "array_agg" | "listagg"
            // 널·조건
            | "coalesce" | "nullif" | "ifnull" | "isnull" | "nvl" | "nvl2" | "if" | "iif"
            | "decode" | "case" | "exists"
            // 날짜·시각
            | "now" | "curdate" | "curtime" | "current_date" | "current_time"
            | "current_timestamp" | "localtime" | "localtimestamp" | "date_add" | "date_sub"
            | "datediff" | "timestampdiff" | "timestampadd" | "date_format" | "str_to_date"
            | "extract" | "year" | "month" | "day" | "hour" | "minute" | "second" | "week"
            | "quarter" | "dayofyear" | "dayofmonth" | "dayofweek" | "weekday" | "last_day"
            | "adddate" | "subdate" | "makedate" | "maketime" | "unix_timestamp"
            | "from_unixtime" | "sec_to_time" | "time_to_sec" | "timediff" | "date_trunc"
            | "age" | "make_date" | "make_time" | "make_timestamp" | "to_date"
            | "to_timestamp" | "to_char" | "to_number" | "to_interval"
            // 형변환·정체성·컨텍스트
            | "cast" | "convert" | "current_user" | "session_user" | "system_user" | "user"
            | "database" | "schema" | "version" | "uuid" | "typeof" | "pg_typeof"
            | "current_schema" | "current_schemas" | "current_catalog" | "current_setting"
            | "set_config" | "inet_client_addr" | "inet_server_addr" | "pg_backend_pid"
            | "pg_postmaster_start_time" | "pg_conf_load_time" | "pg_is_in_recovery"
            // JSON
            | "json_extract" | "json_set" | "json_insert" | "json_replace" | "json_remove"
            | "json_contains" | "json_valid" | "json_length" | "json_keys" | "json_array"
            | "json_object" | "json_quote" | "json_unquote" | "json_search" | "json_value"
            | "json_query" | "jsonb_build_object" | "jsonb_build_array" | "to_json"
            | "to_jsonb" | "row_to_json" | "json_agg" | "jsonb_agg" | "json_extract_path"
            | "json_extract_path_text" | "jsonb_extract_path" | "jsonb_extract_path_text"
            // 윈도우·순위
            | "row_number" | "rank" | "dense_rank" | "ntile" | "lag" | "lead" | "first_value"
            | "last_value" | "nth_value" | "cume_dist" | "percent_rank"
            // 기타 흔한 것
            | "count_big" | "checksum_agg" | "grouping" | "grouping_id" | "sha1" | "sha2"
            | "compress" | "uncompress" | "crc32" | "inet_aton" | "inet_ntoa" | "is_ipv4"
            | "is_ipv6" | "name_const" | "sleep" | "get_lock" | "release_lock" | "pg_sleep"
            | "generate_series" | "unnest" | "array_length" | "cardinality" | "string_to_array"
            | "array_to_string" | "regexp_replace" | "regexp_matches" | "pg_get_userbyid"
            | "pg_table_is_visible" | "pg_function_is_visible" | "has_table_privilege"
            | "has_schema_privilege" | "pg_get_expr" | "format_type" | "obj_description"
            | "col_description" | "shobj_description" | "txid_current" | "pg_current_xact_id"
    )
}

/// DML 문장의 쓰기 대상 — 절차형 블록 안의 문장까지 재귀로 본다.
/// SELECT-only 문장은 빈 벡터다.
fn write_targets(stmt: &Statement) -> Vec<(Option<String>, String)> {
    let mut targets = Vec::new();
    let _ = visit_statements(stmt, |statement| {
        targets.extend(write_targets_shallow(statement));
        ControlFlow::<()>::Continue(())
    });
    targets
}

/// 절차형 블록(IF/ELSEIF/ELSE, WHILE, CASE) 안에 중첩된 문장들.
/// T-SQL 몸체는 도매 파싱이 성공해도 DML이 블록 안에 들어간다.
fn nested_statements(stmt: &Statement) -> Vec<&Statement> {
    match stmt {
        Statement::If(s) => std::iter::once(&s.if_block)
            .chain(s.elseif_blocks.iter())
            .chain(s.else_block.iter())
            .flat_map(|b| b.statements().iter())
            .collect(),
        Statement::While(s) => s.while_block.statements().iter().collect(),
        Statement::Case(s) => s
            .when_blocks
            .iter()
            .chain(s.else_block.iter())
            .flat_map(|b| b.statements().iter())
            .collect(),
        _ => vec![],
    }
}

/// 한 문장의 표면 쓰기 대상 — 중첩 블록은 write_targets가 본다.
fn write_targets_shallow(stmt: &Statement) -> Vec<(Option<String>, String)> {
    match stmt {
        Statement::Truncate { table_names, .. } => table_names
            .iter()
            .filter_map(|target| relation_name_parts(&target.name))
            .collect(),
        Statement::Update { table, from, .. } => {
            let tables: &[TableWithJoins] = from
                .as_ref()
                .map(|from| match from {
                    UpdateTableFromKind::BeforeSet(tables)
                    | UpdateTableFromKind::AfterSet(tables) => tables.as_slice(),
                })
                .unwrap_or(&[]);
            promoted_update_target(&table.relation, tables)
                .or_else(|| table_factor_name(&table.relation))
                .into_iter()
                .collect()
        }
        Statement::Merge { table, .. } => table_factor_name(table).into_iter().collect(),
        Statement::Insert(insert) => match &insert.table {
            TableObject::TableName(name) => relation_name_parts(name).into_iter().collect(),
            _ => vec![],
        },
        Statement::CreateTable(create) if create.query.is_some() => {
            relation_name_parts(&create.name).into_iter().collect()
        }
        Statement::Query(_) => select_into(stmt)
            .and_then(|into| relation_name_parts(&into.name))
            .into_iter()
            .collect(),
        Statement::Delete(delete) => {
            if !delete.tables.is_empty() {
                delete
                    .tables
                    .iter()
                    .filter_map(relation_name_parts)
                    .collect()
            } else {
                // DELETE FROM t — from 절이 쓰기 대상이다.
                let tables: &[sqlparser::ast::TableWithJoins] = match &delete.from {
                    sqlparser::ast::FromTable::WithFromKeyword(t)
                    | sqlparser::ast::FromTable::WithoutKeyword(t) => t,
                };
                tables
                    .iter()
                    .filter_map(|twj| table_factor_name(&twj.relation))
                    .collect()
            }
        }
        _ => vec![],
    }
}

fn read_exclusions(
    stmt: &Statement,
    write_targets: &[(Option<String>, String)],
) -> Vec<(Option<String>, String)> {
    let mut exclusions = write_targets.to_vec();
    if let Statement::Update { table, .. } = stmt {
        if let Some(target) = table_factor_name(&table.relation) {
            if !exclusions.contains(&target) {
                exclusions.push(target);
            }
        }
    }
    exclusions
}

fn promoted_update_target(
    target: &TableFactor,
    from: &[TableWithJoins],
) -> Option<(Option<String>, String)> {
    let TableFactor::Table {
        name,
        alias: None,
        args: None,
        ..
    } = target
    else {
        return None;
    };
    let [target_name] = name.0.as_slice() else {
        return None;
    };
    let target_name = target_name.as_ident()?.value.as_str();
    let matches: Vec<_> = from
        .iter()
        .filter_map(|table| match &table.relation {
            factor @ TableFactor::Table {
                alias: Some(alias),
                args: None,
                ..
            } if alias.name.value.eq_ignore_ascii_case(target_name) => table_factor_name(factor),
            _ => None,
        })
        .collect();
    match matches.as_slice() {
        [target] => Some(target.clone()),
        _ => None,
    }
}

fn qualified_write_targets(stmt: &Statement) -> Vec<ObjectName> {
    let mut names = Vec::new();
    match stmt {
        Statement::Update { table, from, .. } => {
            let tables: &[TableWithJoins] = from
                .as_ref()
                .map(|from| match from {
                    UpdateTableFromKind::BeforeSet(tables)
                    | UpdateTableFromKind::AfterSet(tables) => tables.as_slice(),
                })
                .unwrap_or(&[]);
            if let Some(target) = promoted_update_factor(&table.relation, tables) {
                if let TableFactor::Table { name, .. } = target {
                    names.push(name.clone());
                }
            } else if let TableFactor::Table { name, .. } = &table.relation {
                names.push(name.clone());
            }
        }
        Statement::Merge {
            table: TableFactor::Table { name, .. },
            ..
        } => names.push(name.clone()),
        Statement::Insert(insert) => {
            if let TableObject::TableName(name) = &insert.table {
                names.push(name.clone());
            }
        }
        Statement::CreateTable(create) if create.query.is_some() => names.push(create.name.clone()),
        Statement::Query(_) => {
            if let Some(into) = select_into(stmt) {
                names.push(into.name.clone());
            }
        }
        _ => {}
    }
    names
        .into_iter()
        .filter(|name| name.0.iter().filter_map(|part| part.as_ident()).count() == 3)
        .collect()
}

fn promoted_update_factor<'a>(
    target: &TableFactor,
    from: &'a [TableWithJoins],
) -> Option<&'a TableFactor> {
    let TableFactor::Table {
        name,
        alias: None,
        args: None,
        ..
    } = target
    else {
        return None;
    };
    let [target_name] = name.0.as_slice() else {
        return None;
    };
    let target_name = target_name.as_ident()?.value.as_str();
    let matches: Vec<_> = from
        .iter()
        .filter_map(|table| match &table.relation {
            factor @ TableFactor::Table {
                alias: Some(alias),
                args: None,
                ..
            } if alias.name.value.eq_ignore_ascii_case(target_name) => Some(factor),
            _ => None,
        })
        .collect();
    match matches.as_slice() {
        [target] => Some(*target),
        _ => None,
    }
}

/// TableFactor::Table이면 (스키마?, 테이블)을 돌린다.
fn table_factor_name(factor: &TableFactor) -> Option<(Option<String>, String)> {
    if let TableFactor::Table { name, .. } = factor {
        relation_name_parts(name)
    } else {
        None
    }
}

/// routine 이름 해석 결과 — 오버로드는 호출 정확도상 구분 못 하므로
/// 모호함을 숨기지 않고 별도로 보고한다. 모호하면 실제 후보 정점을 함께 돌려
/// 진단이 "어느 것 중 하나"를 잃지 않게 한다.
enum RoutineHit {
    One(VertexId),
    None,
    Ambiguous(Vec<VertexId>),
}

#[cfg(test)]
thread_local! {
    /// 이름 해석이 그래프 정점 전체를 훑은 횟수. 테스트 스레드마다 따로 센다.
    static VERTEX_SCANS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// 이름 해석의 정점 전체 순회를 한 번 센다.
#[cfg(test)]
fn count_vertex_scan() {
    VERTEX_SCANS.with(|scans| scans.set(scans.get() + 1));
}

/// 현재 테스트 스레드의 정점 순회 횟수를 돌려주고 0으로 되돌린다.
#[cfg(test)]
fn take_vertex_scans() -> usize {
    VERTEX_SCANS.with(std::cell::Cell::take)
}

/// 테스트가 몸체 분석 작업량을 시간 대신 횟수로 확인하게 한다.
///
/// 이차 시간 회귀는 작은 입력에서 시간으로 잡히지 않고, 큰 입력의 시간 단언은 공유
/// 기계에서 불안정하다. 테스트마다 스레드가 따로라 thread_local이면 서로 섞이지 않는다.
#[cfg(test)]
mod work {
    use std::cell::Cell;

    thread_local! {
        static COMPARED_STATEMENTS: Cell<usize> = const { Cell::new(0) };
        static SEARCHED_BYTES: Cell<usize> = const { Cell::new(0) };
    }

    /// DML 문장 중복 확인에서 문장 전체를 비교한 횟수를 하나 더한다.
    pub(crate) fn compared_statement() {
        COMPARED_STATEMENTS.with(|count| count.set(count.get() + 1));
    }

    /// 현재 테스트 스레드의 문장 비교 횟수를 돌려주고 0으로 되돌린다.
    pub(crate) fn take_compared_statements() -> usize {
        COMPARED_STATEMENTS.with(Cell::take)
    }

    /// 닫는 인용 표식을 찾으며 훑은 바이트 수를 더한다.
    pub(crate) fn searched_bytes(bytes: usize) {
        SEARCHED_BYTES.with(|count| count.set(count.get() + bytes));
    }

    /// 현재 테스트 스레드에서 닫는 표식을 찾으며 훑은 바이트 수를 돌려주고 0으로 되돌린다.
    pub(crate) fn take_searched_bytes() -> usize {
        SEARCHED_BYTES.with(Cell::take)
    }
}

/// enrich 동안 고정된 정점 집합에서 이름 해석 후보를 찾는 색인.
///
/// parser는 정점을 추가하지 않고 간선만 더하므로 처음 조회할 때 만든 색인이
/// enrich 끝까지 유효하다. 참조마다 전체 정점을 훑던 해석을 키 조회로 바꾸되,
/// 후보 0개·1개·2개 이상을 None·One·Ambiguous로 가르는 규칙은 그대로 둔다.
/// 그래서 한 색인은 처음 조회한 그래프 하나에만 써야 한다.
#[derive(Default)]
pub(crate) struct VertexIndex {
    exact: OnceCell<RoutineIndex>,
    folded: OnceCell<RoutineIndex>,
    /// ASCII 대문자로 접은 id → 정점. ci 방언의 단일 후보 판정에 쓴다.
    folded_ids: OnceCell<BTreeMap<String, Vec<VertexId>>>,
}

impl VertexIndex {
    /// 정확 일치만 볼지, 대소문자 무시 단일 후보까지 볼지 정한 조회 방식.
    pub(crate) fn lookup(&self, ci: bool) -> Lookup<'_> {
        Lookup { index: self, ci }
    }

    fn routines(&self, g: &Graph, ci: bool) -> &RoutineIndex {
        if ci {
            self.folded.get_or_init(|| RoutineIndex::new(g, fold_case))
        } else {
            self.exact
                .get_or_init(|| RoutineIndex::new(g, str::to_owned))
        }
    }

    fn folded_ids(&self, g: &Graph) -> &BTreeMap<String, Vec<VertexId>> {
        self.folded_ids.get_or_init(|| {
            #[cfg(test)]
            count_vertex_scan();
            let mut ids: BTreeMap<String, Vec<VertexId>> = BTreeMap::new();
            for vertex in g.vertices() {
                let key = fold_case(vertex.id.as_str());
                ids.entry(key).or_default().push(vertex.id.clone());
            }
            ids
        })
    }
}

/// routine 이름 해석 후보. 이름 키는 만들 때 준 규칙으로 접은 값이다.
#[derive(Default)]
struct RoutineIndex {
    /// (스키마, 표시 이름) → 함수·프로시저·패키지 정점.
    routines: BTreeMap<(String, String), Vec<VertexId>>,
    /// (패키지 id, 멤버 이름) → 함수·프로시저 정점. 패키지 id는 접지 않는다.
    members: BTreeMap<(VertexId, String), Vec<VertexId>>,
}

impl RoutineIndex {
    fn new(g: &Graph, fold: fn(&str) -> String) -> Self {
        #[cfg(test)]
        count_vertex_scan();
        let mut index = Self::default();
        for vertex in g.vertices() {
            if matches!(
                vertex.kind,
                VertexKind::Function | VertexKind::Procedure | VertexKind::Package
            ) {
                let key = (fold(&vertex.schema), fold(&vertex.name));
                index
                    .routines
                    .entry(key)
                    .or_default()
                    .push(vertex.id.clone());
            }
            if matches!(vertex.kind, VertexKind::Function | VertexKind::Procedure) {
                if let Some(package) = vertex.id.parent() {
                    let key = (package, fold(&vertex.name));
                    index
                        .members
                        .entry(key)
                        .or_default()
                        .push(vertex.id.clone());
                }
            }
        }
        index
    }
}

/// 이름 해석 방식과 그 색인. ci면 정확 일치가 없을 때 대소문자 무시 단일
/// 후보를 본다.
#[derive(Clone, Copy)]
pub(crate) struct Lookup<'a> {
    index: &'a VertexIndex,
    ci: bool,
}

impl Lookup<'_> {
    /// 이 조회 방식의 비교 규칙으로 이름 키를 만든다.
    fn key(&self, value: &str) -> String {
        if self.ci {
            fold_case(value)
        } else {
            value.to_owned()
        }
    }
}

/// ci 비교 `a == b || a.eq_ignore_ascii_case(b)`와 같은 동치류를 만드는 키.
fn fold_case(value: &str) -> String {
    value.to_ascii_uppercase()
}

/// 후보 수로 routine 해석 결과를 정한다 — 둘 이상이면 추측하지 않는다.
fn routine_hit(candidates: Option<&Vec<VertexId>>) -> RoutineHit {
    match candidates.map(Vec::as_slice).unwrap_or_default() {
        [] => RoutineHit::None,
        [only] => RoutineHit::One(only.clone()),
        many => RoutineHit::Ambiguous(many.to_vec()),
    }
}

/// 멤버 정점 id를 찾는다 — document_to_graph가 이름 충돌을 `name@kind`로
/// 분리하므로, 평범한 id에 우리 kind가 없으면 접미사 id를 시도한다.
/// 못 찾으면 None — 유령 id로 간선을 만들지 않는다.
fn resolve_member(
    g: &Graph,
    schema: &str,
    obj: &str,
    name: &str,
    kind: VertexKind,
    suffix: &str,
    lookup: Lookup<'_>,
) -> Option<VertexId> {
    resolve_renamed(g, VertexId::member(schema, obj, name), kind, suffix, lookup)
}

fn resolve_routine_vertex(
    g: &Graph,
    schema: &str,
    routine: &RoutineDoc,
    kind: VertexKind,
    suffix: &str,
    lookup: Lookup<'_>,
) -> Option<VertexId> {
    resolve_renamed(
        g,
        VertexId::routine(
            schema,
            routine.member_of.as_deref(),
            &routine.name,
            routine.signature.as_deref(),
        ),
        kind,
        suffix,
        lookup,
    )
}

/// 객체 레벨 정점(routine 등)을 같은 규칙으로 찾는다.
fn resolve_object(
    g: &Graph,
    schema: &str,
    name: &str,
    kind: VertexKind,
    suffix: &str,
    lookup: Lookup<'_>,
) -> Option<VertexId> {
    resolve_renamed(g, VertexId::object(schema, name), kind, suffix, lookup)
}

/// base id가 다른 kind에게 점유됐으면 `base@suffix`를 시도한다 —
/// graph.rs의 resolve_collision과 같은 명명 규칙을 공유해야 한다.
fn resolve_renamed(
    g: &Graph,
    base: VertexId,
    kind: VertexKind,
    suffix: &str,
    lookup: Lookup<'_>,
) -> Option<VertexId> {
    let renamed = VertexId::from_raw(&format!("{}@{suffix}", base.as_str()));
    [base, renamed]
        .into_iter()
        .filter_map(|id| vertex_hit(g, &id, lookup))
        .find(|id| g.vertex(id).map(|v| v.kind == kind).unwrap_or(false))
}

/// 정점 조회 — 정확 일치가 없을 때 ci가 켜져 있으면 대소문자 무시 단일 후보를
/// 찾는다. Oracle의 미인용 식별자는 대문자로 접혀 몸체의 `orders`와 카탈로그의
/// `ORDERS`가 같은 대상이지만, 대소문자만 다른 두 객체가 있으면 어느 쪽인지
/// 추측할 수 없어 None을 돌려 유령 간선을 막는다.
fn vertex_hit(g: &Graph, id: &VertexId, lookup: Lookup<'_>) -> Option<VertexId> {
    if g.vertex(id).is_some() {
        return Some(id.clone());
    }
    if !lookup.ci {
        return None;
    }
    match lookup.index.folded_ids(g).get(&fold_case(id.as_str())) {
        Some(hits) if hits.len() == 1 => Some(hits[0].clone()),
        _ => None,
    }
}

/// 스키마 안에서 routine 정점을 이름으로 찾는다. 정확한 id(name 그대로)가
/// 먼저이고, 없으면 routine kind 정점의 표시 이름과 비교한다 — 호출자는
/// 시그니처를 모르므로 이름이 유일할 때만 받아들인다(resolve()와 같은 철학).
fn resolve_routine(g: &Graph, schema: &str, name: &str, lookup: Lookup<'_>) -> RoutineHit {
    // 정확한 base id가 있어도 같은 표시 이름의 시그니처 정점이 있으면
    // 호출 인자 수를 모르는 이 경로에서 무인자 overload를 임의로 고르면 안
    // 된다. 모든 routine 후보를 먼저 모아 하나일 때만 선택한다.
    let routines = &lookup.index.routines(g, lookup.ci).routines;
    routine_hit(routines.get(&(lookup.key(schema), lookup.key(name))))
}

/// 한 몸체를 같은 카탈로그·방언·소스 해시로 적용하는 문맥이다。
struct BodyContext<'a, 'doc> {
    catalog: &'a scope::CatalogIndex<'doc>,
    schema: &'a str,
    ci: bool,
    body_hash: String,
    sqlserver_trigger: bool,
}

/// trigger 파싱 결과를 그래프 간선으로 반영한다 — DML + calls + fired-columns.
fn apply_trigger(
    g: &mut Graph,
    context: BodyContext<'_, '_>,
    owner_table: &str,
    trigger_id: &VertexId,
    parsed: &mut ParsedTrigger,
    notes: &mut Vec<String>,
) -> bool {
    let BodyContext {
        catalog,
        schema,
        ci,
        body_hash,
        sqlserver_trigger,
    } = context;
    if g.vertex(trigger_id).is_none() {
        notes.push(format!(
            "trigger {trigger_id}의 정점이 카탈로그에 없음 — 간선 생략"
        ));
        return false;
    }
    note_ignored_relations(trigger_id, &parsed.ignored_relations, notes);
    apply_dml_edges(g, catalog, schema, trigger_id, parsed, notes, ci);
    let transition = if sqlserver_trigger {
        Some((owner_table, &["inserted", "deleted"][..]))
    } else if !parsed.fired_columns.is_empty() {
        Some((owner_table, &["new", "old"][..]))
    } else {
        None
    };
    parsed.partial |= scope::apply_dml_column_effects(
        g,
        catalog,
        schema,
        trigger_id,
        &parsed.dml_statements,
        scope::DmlApplyOptions {
            body_hash: &body_hash,
            transition,
            allow_local_temps: !parsed
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code == "SG_DML_CONTROL_FLOW")
                && parsed.temp_state_safe,
            locations_trusted: parsed.locations_trusted,
            scalar_symbols: &parsed.scalar_symbols,
        },
        &mut parsed.diagnostics,
    );
    apply_call_edges(
        g,
        schema,
        trigger_id,
        &parsed.calls,
        CallNotes {
            notes,
            diagnostics: &mut parsed.diagnostics,
        },
        catalog.lookup(ci),
    );
    // NEW.x / OLD.x는 발사 테이블의 컬럼을 읽는다는 뜻이다. 컬럼이 id 충돌로
    // `@column`으로 분리됐을 수 있어 kind-aware 해석자를 쓴다.
    let mut made = std::collections::BTreeSet::new();
    for column in &parsed.fired_columns {
        let Some(target) = resolve_member(
            g,
            schema,
            owner_table,
            column,
            VertexKind::Column,
            "column",
            catalog.lookup(ci),
        ) else {
            continue;
        };
        if !made.insert(target.clone()) {
            continue;
        }
        g.add_edge(Edge {
            from: trigger_id.clone(),
            to: target.clone(),
            kind: EdgeKind::Reads,
            evidence: vec![Evidence {
                layer: EvidenceLayer::BodyParse,
                detail: format!("trigger {trigger_id} reads NEW/OLD.{column} on {owner_table}"),
            }],
        });
    }
    parsed.partial
}

/// routine 파싱 결과를 그래프 간선으로 반영한다 — trigger와 같은 DML/calls
/// 규칙이지만 발사 테이블(NEW/OLD)은 없다.
fn apply_routine(
    g: &mut Graph,
    context: BodyContext<'_, '_>,
    owner: &VertexId,
    parsed: &mut ParsedTrigger,
    notes: &mut Vec<String>,
) -> bool {
    let BodyContext {
        catalog,
        schema,
        ci,
        body_hash,
        ..
    } = context;
    if g.vertex(owner).is_none() {
        notes.push(format!(
            "routine {owner}의 정점이 카탈로그에 없음 — 간선 생략"
        ));
        return false;
    }
    note_ignored_relations(owner, &parsed.ignored_relations, notes);
    apply_dml_edges(g, catalog, schema, owner, parsed, notes, ci);
    parsed.partial |= scope::apply_dml_column_effects(
        g,
        catalog,
        schema,
        owner,
        &parsed.dml_statements,
        scope::DmlApplyOptions {
            body_hash: &body_hash,
            transition: None,
            allow_local_temps: !parsed
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code == "SG_DML_CONTROL_FLOW")
                && parsed.temp_state_safe,
            locations_trusted: parsed.locations_trusted,
            scalar_symbols: &parsed.scalar_symbols,
        },
        &mut parsed.diagnostics,
    );
    apply_call_edges(
        g,
        schema,
        owner,
        &parsed.calls,
        CallNotes {
            notes,
            diagnostics: &mut parsed.diagnostics,
        },
        catalog.lookup(ci),
    );
    parsed.partial
}

fn note_ignored_relations(from: &VertexId, relations: &BTreeSet<String>, notes: &mut Vec<String>) {
    if relations.is_empty() {
        return;
    }
    notes.push(format!(
        "{from}: skipped {} database-qualified relations without catalog identities ({})",
        relations.len(),
        relations.iter().cloned().collect::<Vec<_>>().join(", ")
    ));
}

/// 몸체 문장의 writes/reads 대상을 간선으로 만든다 — trigger와 routine이 공유.
fn apply_dml_edges(
    g: &mut Graph,
    catalog: &scope::CatalogIndex<'_>,
    schema: &str,
    from: &VertexId,
    parsed: &mut ParsedTrigger,
    notes: &mut Vec<String>,
    ci: bool,
) {
    for (kind, targets) in [
        (EdgeKind::Writes, &parsed.writes),
        (EdgeKind::Reads, &parsed.reads),
    ] {
        for (ref_schema, table) in targets {
            if parsed
                .local_relations
                .contains(&(ref_schema.clone(), table.clone()))
            {
                continue;
            }
            let target_schema = ref_schema.clone().unwrap_or_else(|| schema.to_owned());
            let target_id = VertexId::object(&target_schema, table);
            let Some(target) = vertex_hit(g, &target_id, catalog.lookup(ci)) else {
                notes.push(format!(
                    "{from}이(가) 참조하는 {target_schema}.{table}이 카탈로그에 없음"
                ));
                continue;
            };
            g.add_edge(Edge {
                from: from.clone(),
                to: target.clone(),
                kind,
                evidence: vec![Evidence {
                    layer: EvidenceLayer::BodyParse,
                    detail: format!("{from} {:?} {target}", kind),
                }],
            });
        }
    }
    for (kind, targets) in [
        (EdgeKind::Writes, parsed.qualified_writes.clone()),
        (EdgeKind::Reads, parsed.qualified_reads.clone()),
    ] {
        for name in targets {
            let (target_schema, table) = match catalog.local_qualified_relation(&name) {
                Ok(target) => target,
                Err(message) => {
                    parsed.partial = true;
                    notes.push(format!("{from}: {message}"));
                    parsed.diagnostics.push(schemagraph_core::Diagnostic {
                        code: "SG_CROSS_DATABASE".into(),
                        message,
                        location: None,
                        candidates: Vec::new(),
                    });
                    continue;
                }
            };
            let target_id = VertexId::object(&target_schema, &table);
            let Some(target) = vertex_hit(g, &target_id, catalog.lookup(ci)) else {
                notes.push(format!(
                    "{from}이(가) 참조하는 {target_schema}.{table}이 카탈로그에 없음"
                ));
                continue;
            };
            g.add_edge(Edge {
                from: from.clone(),
                to: target.clone(),
                kind,
                evidence: vec![Evidence {
                    layer: EvidenceLayer::BodyParse,
                    detail: format!("{from} {:?} {target}", kind),
                }],
            });
        }
    }
}

/// 호출 해석이 남기는 limitation note와 몸체의 구조화 진단.
///
/// 모호한 오버로드 호출은 note 문구를 그대로 두고, 같은 문구의 진단에 후보 정점을
/// 싣는다 — `dead`가 불릴 수 있는 오버로드를 후보로 내지 않게 하는 근거다.
struct CallNotes<'a> {
    notes: &'a mut Vec<String>,
    diagnostics: &'a mut Vec<schemagraph_core::Diagnostic>,
}

/// EXECUTE FUNCTION/PROCEDURE 호출 대상을 간선으로 — routine 정점 id는
/// 시그니처를 포함할 수 있어 이름으로 해석한다.
fn apply_call_edges(
    g: &mut Graph,
    schema: &str,
    from: &VertexId,
    calls: &[(Option<String>, String)],
    out: CallNotes<'_>,
    lookup: Lookup<'_>,
) {
    let CallNotes { notes, diagnostics } = out;
    for (ref_schema, name) in calls {
        let target_schema = ref_schema.clone().unwrap_or_else(|| schema.to_owned());
        let hit = match resolve_routine(g, &target_schema, name, lookup) {
            RoutineHit::None => {
                // `pkg.member()` 꼴 — qualifier가 스키마가 아니라 패키지면
                // 멤버 정점 schema.pkg.member를 찾는다.
                ref_schema
                    .as_ref()
                    .map(|pkg| resolve_pkg_member(g, schema, pkg, name, lookup))
                    .unwrap_or(RoutineHit::None)
            }
            hit => hit,
        };
        match hit {
            RoutineHit::One(target) => {
                g.add_edge(Edge {
                    from: from.clone(),
                    to: target.clone(),
                    kind: EdgeKind::Calls,
                    evidence: vec![Evidence {
                        layer: EvidenceLayer::BodyParse,
                        detail: format!("{from} calls {target}"),
                    }],
                });
            }
            RoutineHit::None => notes.push(format!(
                "{from}이(가) 부르는 {target_schema}.{name}이 카탈로그에 없음 \
                 (내장 함수이거나 미수집)"
            )),
            RoutineHit::Ambiguous(candidates) => {
                let note = format!(
                    "{from}이(가) 부르는 {target_schema}.{name}에 {}개 오버로드가 있어 간선 생략",
                    candidates.len()
                );
                // 확정 간선은 만들지 않는다. 진단의 후보는 note와 같은 문구라
                // record_body가 note 진단을 따로 만들지 않는다.
                diagnostics.push(
                    schemagraph_core::Diagnostic::new("SG_BODY_PARTIAL", note.clone(), None)
                        .with_candidates(candidates),
                );
                notes.push(note);
            }
        }
    }
}

/// `schema.pkg.member` 정점을 찾는다 — 멤버 kind는 procedure/function 둘
/// 다 될 수 있어 둘을 시도하고, 시그니처가 붙은 id(`pkg.m(int)`)는
/// `schema.pkg.` 접두 안의 이름 스캔으로 찾는다. 오버로드가 여럿이면
/// Ambiguous — resolve_routine과 같이 추측하지 않는다.
fn resolve_pkg_member(
    g: &Graph,
    schema: &str,
    pkg: &str,
    name: &str,
    lookup: Lookup<'_>,
) -> RoutineHit {
    let base = VertexId::routine(schema, Some(pkg), name, None);
    if let Some(id) = resolve_renamed(g, base.clone(), VertexKind::Procedure, "procedure", lookup)
        .or_else(|| resolve_renamed(g, base, VertexKind::Function, "function", lookup))
    {
        return RoutineHit::One(id);
    }
    let package_id = VertexId::object(schema, pkg);
    let members = &lookup.index.routines(g, lookup.ci).members;
    routine_hit(members.get(&(package_id, lookup.key(name))))
}

#[cfg(test)]
mod tests {
    use super::*;
    use schemagraph_source::document::*;

    fn col(name: &str, ordinal: u32) -> ColumnDoc {
        ColumnDoc {
            name: name.into(),
            data_type: "INTEGER".into(),
            nullable: false,
            default: None,
            ordinal,
            pk_position: 0,
        }
    }

    fn table(name: &str, columns: Vec<ColumnDoc>) -> ObjectDoc {
        ObjectDoc {
            name: name.into(),
            kind: "table".into(),
            columns,
            constraints: vec![],
            indexes: vec![],
            triggers: vec![],
            body: None,
            usage: None,
        }
    }

    fn doc_with_view(body: &str) -> CatalogDocument {
        CatalogDocument {
            context: None,
            dependencies: Vec::new(),
            version: 1,
            dialect: "sqlite".into(),
            reader: "test".into(),
            limitations: vec![],
            schemas: vec![SchemaDoc {
                name: "main".into(),
                routines: vec![],
                objects: vec![
                    table("orders", vec![col("id", 1), col("customer_id", 2)]),
                    table("customers", vec![col("id", 1), col("name", 2)]),
                    ObjectDoc {
                        name: "order_totals".into(),
                        kind: "view".into(),
                        columns: vec![],
                        constraints: vec![],
                        indexes: vec![],
                        triggers: vec![],
                        body: Some(body.to_owned()),
                        usage: None,
                    },
                ],
            }],
        }
    }

    /// trigger 하나가 달린 orders 테이블만 있는 document — view 없이 만든다
    /// (view 몸체 파싱 실패 notes가 섞이지 않도록).
    fn doc_with_trigger(body: &str) -> CatalogDocument {
        CatalogDocument {
            context: None,
            dependencies: Vec::new(),
            version: 1,
            dialect: "sqlite".into(),
            reader: "test".into(),
            limitations: vec![],
            schemas: vec![SchemaDoc {
                name: "main".into(),
                routines: vec![],
                objects: vec![
                    {
                        let mut t = table("orders", vec![col("id", 1), col("customer_id", 2)]);
                        t.triggers.push(TriggerDoc {
                            name: "trg_touch".into(),
                            body: Some(body.to_owned()),
                        });
                        t
                    },
                    table("customers", vec![col("id", 1), col("name", 2)]),
                ],
            }],
        }
    }

    fn sqlserver_trigger_doc(body: &str) -> CatalogDocument {
        let mut doc = doc_with_trigger(body);
        doc.dialect = "sqlserver".into();
        doc.schemas[0].name = "dbo".into();
        doc.schemas[0].objects[0].columns.push(col("name", 3));
        doc
    }

    fn build(doc: &CatalogDocument) -> (Graph, Vec<String>) {
        let mut g = schemagraph_source::graph::document_to_graph(doc);
        let (_n, notes) = enrich_from_document(&mut g, doc);
        (g, notes)
    }

    #[test]
    fn view의_테이블_참조가_reads_간선이_된다() {
        let doc = doc_with_view(
            "CREATE VIEW order_totals AS SELECT o.id, c.name FROM orders o JOIN customers c ON c.id = o.customer_id",
        );
        let (g, _) = build(&doc);
        let edges = g.edges();
        assert!(edges.iter().any(|e| e.kind == EdgeKind::Reads
            && e.from.as_str() == "main.order_totals"
            && e.to.as_str() == "main.orders"));
        assert!(edges.iter().any(|e| e.kind == EdgeKind::Reads
            && e.from.as_str() == "main.order_totals"
            && e.to.as_str() == "main.customers"));
    }

    #[test]
    fn 별칭이_있는_컬럼_참조는_member_간선이_된다() {
        let doc = doc_with_view(
            "CREATE VIEW order_totals AS SELECT o.id, c.name FROM orders o JOIN customers c ON c.id = o.customer_id",
        );
        let (g, _) = build(&doc);
        assert!(g.edges().iter().any(|e| e.kind == EdgeKind::Reads
            && e.from.as_str() == "main.order_totals"
            && e.to.as_str() == "main.customers.name"));
    }

    #[test]
    fn 없는_테이블은_유령을_만들지_않고_notes를_남긴다() {
        let doc = doc_with_view("CREATE VIEW order_totals AS SELECT * FROM ghost_table");
        let (g, notes) = build(&doc);
        assert!(!g.vertices().any(|v| v.id.as_str() == "main.ghost_table"));
        assert!(notes.iter().any(|n| n.contains("ghost_table")));
    }

    #[test]
    fn 파싱_실패는_notes로_보고된다() {
        let doc = doc_with_view("CREATE VIEW v AS SELECT ((( broken syntax");
        let mut g = schemagraph_source::graph::document_to_graph(&doc);
        let (_n, notes) = enrich_from_document(&mut g, &doc);
        assert_eq!(notes.len(), 1);
        assert!(notes[0].contains("parse failed"));
    }

    #[test]
    fn 서브쿼리의_비한정_컬럼을_해당_스코프에서_해석한다() {
        let doc = doc_with_view(
            "CREATE VIEW order_totals AS SELECT o.id FROM orders o WHERE o.customer_id IN (SELECT id FROM customers)",
        );
        let (g, notes) = build(&doc);
        assert!(notes.is_empty(), "{notes:?}");
        assert!(g
            .edges()
            .iter()
            .any(|e| e.kind == EdgeKind::Reads && e.to.as_str() == "main.customers.id"));
    }

    /// Oracle은 카탈로그가 대문자(미인용 식별자 접힘)인데 몸체는 소문자로
    /// 쓰이는 게 보통 — ci 해석이 소문자 참조를 대문자 정점으로 연결해야 한다.
    #[test]
    fn oracle의_소문자_몸체가_대문자_정점으로_해석된다() {
        let doc = CatalogDocument {
            context: None,
            dependencies: Vec::new(),
            version: 1,
            dialect: "oracle".into(),
            reader: "test".into(),
            limitations: vec![],
            schemas: vec![SchemaDoc {
                name: "SGFIX".into(),
                routines: vec![],
                objects: vec![
                    table("ORDERS", vec![col("ID", 1), col("CUSTOMER_ID", 2)]),
                    table("CUSTOMERS", vec![col("ID", 1), col("NAME", 2)]),
                    ObjectDoc {
                        name: "ORDER_TOTALS".into(),
                        kind: "view".into(),
                        columns: vec![],
                        constraints: vec![],
                        indexes: vec![],
                        triggers: vec![],
                        body: Some(
                            "CREATE VIEW order_totals AS SELECT o.id, c.name \
                             FROM orders o JOIN customers c ON c.id = o.customer_id"
                                .to_owned(),
                        ),
                        usage: None,
                    },
                ],
            }],
        };
        let (g, notes) = build(&doc);
        assert!(notes.is_empty(), "notes: {notes:?}");
        assert!(g.edges().iter().any(|e| e.kind == EdgeKind::Reads
            && e.from.as_str() == "SGFIX.ORDER_TOTALS"
            && e.to.as_str() == "SGFIX.ORDERS"));
        assert!(g.edges().iter().any(|e| e.kind == EdgeKind::Reads
            && e.from.as_str() == "SGFIX.ORDER_TOTALS"
            && e.to.as_str() == "SGFIX.CUSTOMERS.NAME"));
    }

    /// 대소문자만 다른 두 객체가 공존하고 몸체가 둘 중 어느 철자도 정확히
    /// 쓰지 않으면 ci는 어느 쪽인지 모른다 — 추측 간선 대신 miss를 남긴다.
    #[test]
    fn oracle의_미인용_식별자는_대문자_객체에만_붙는다() {
        let doc = CatalogDocument {
            context: None,
            dependencies: Vec::new(),
            version: 1,
            dialect: "oracle".into(),
            reader: "test".into(),
            limitations: vec![],
            schemas: vec![SchemaDoc {
                name: "SGFIX".into(),
                routines: vec![],
                objects: vec![
                    table("ORDERS", vec![col("ID", 1)]),
                    table("orders", vec![col("id", 1)]),
                    ObjectDoc {
                        name: "V".into(),
                        kind: "view".into(),
                        columns: vec![],
                        constraints: vec![],
                        indexes: vec![],
                        triggers: vec![],
                        body: Some("CREATE VIEW v AS SELECT id FROM Orders".to_owned()),
                        usage: None,
                    },
                ],
            }],
        };
        let (g, notes) = build(&doc);
        assert!(notes.is_empty(), "{notes:?}");
        assert!(g.edges().iter().any(|e| e.kind == EdgeKind::Reads
            && e.from.as_str() == "SGFIX.V"
            && e.to.as_str() == "SGFIX.ORDERS"));
        assert!(!g
            .edges()
            .iter()
            .any(|e| e.kind == EdgeKind::Reads && e.to.as_str() == "SGFIX.orders"));
    }

    #[test]
    fn trigger의_update_대상이_writes_간선이_된다() {
        let doc = doc_with_trigger(
            "CREATE TRIGGER trg_touch AFTER INSERT ON orders \
             BEGIN UPDATE customers SET name = name WHERE id = NEW.customer_id; END",
        );
        let (g, notes) = build(&doc);
        assert!(notes.is_empty(), "notes: {notes:?}");
        assert!(g.edges().iter().any(|e| e.kind == EdgeKind::Writes
            && e.from.as_str() == "main.orders.trg_touch"
            && e.to.as_str() == "main.customers"));
    }

    #[test]
    fn sqlserver_trigger의_inserted_deleted는_transition_relation으로_취급한다() {
        let doc = sqlserver_trigger_doc(
            "CREATE TRIGGER trg_touch AFTER UPDATE ON orders \
             AS BEGIN SET NOCOUNT ON; INSERT INTO customers (id, name) \
             SELECT n.id, o.name FROM inserted n JOIN deleted o ON o.id = n.id; END",
        );
        let (g, notes) = build(&doc);
        assert!(notes.is_empty(), "notes: {notes:?}");
        assert_eq!(
            g.analysis()[&VertexId::from_raw("dbo.orders.trg_touch")].state,
            schemagraph_core::AnalysisState::Complete,
            "{notes:?}"
        );
        assert!(g.edges().iter().any(|edge| {
            edge.kind == EdgeKind::Writes
                && edge.from.as_str() == "dbo.orders.trg_touch"
                && edge.to.as_str() == "dbo.customers"
        }));
        assert!(!g.edges().iter().any(|edge| {
            edge.from.as_str() == "dbo.orders.trg_touch" && edge.to.as_str().ends_with(".inserted")
        }));
        assert!(!g.edges().iter().any(|edge| {
            edge.from.as_str() == "dbo.orders.trg_touch" && edge.to.as_str().ends_with(".deleted")
        }));
    }

    #[test]
    fn sqlserver_trigger의_qualified_transition_relation은_물리참조로_남긴다() {
        let doc = sqlserver_trigger_doc(
            "CREATE TRIGGER trg_touch AFTER INSERT ON orders \
             AS BEGIN INSERT INTO customers (name) \
             SELECT name FROM dbo.inserted; END",
        );
        let (g, notes) = build(&doc);
        assert!(notes.iter().any(|note| note.contains("dbo.inserted")));
        assert_eq!(
            g.analysis()[&VertexId::from_raw("dbo.orders.trg_touch")].state,
            schemagraph_core::AnalysisState::Partial
        );
        assert!(!g.edges().iter().any(|edge| {
            edge.from.as_str() == "dbo.orders.trg_touch" && edge.to.as_str() == "dbo.inserted"
        }));
    }

    #[test]
    fn sqlserver_trigger의_미지관계는_진단한다() {
        let doc = sqlserver_trigger_doc(
            "CREATE TRIGGER trg_touch AFTER INSERT ON orders \
             AS BEGIN INSERT INTO customers (name) \
             SELECT name FROM ghost_table; END",
        );
        let (g, notes) = build(&doc);
        assert!(notes.iter().any(|note| note.contains("ghost_table")));
        assert_eq!(
            g.analysis()[&VertexId::from_raw("dbo.orders.trg_touch")].state,
            schemagraph_core::AnalysisState::Partial
        );
    }

    #[test]
    fn sqlserver_routine의_inserted는_transition_relation이_아니다() {
        let mut doc = doc_with_routine(
            vec![routine(
                "read_inserted",
                Some("sql"),
                "SELECT id FROM inserted",
            )],
            "",
        );
        doc.dialect = "sqlserver".into();
        let (g, notes) = build(&doc);
        assert!(notes.iter().any(|note| note.contains("inserted")));
        assert_eq!(
            g.analysis()[&VertexId::from_raw("public.read_inserted")].state,
            schemagraph_core::AnalysisState::Partial
        );
    }

    #[test]
    fn 다른_방언의_trigger_inserted는_transition_relation이_아니다() {
        let mut doc = doc_with_trigger(
            "CREATE TRIGGER trg_touch AFTER INSERT ON orders \
             BEGIN INSERT INTO customers (name) SELECT name FROM inserted; END",
        );
        doc.dialect = "postgres".into();
        let (g, notes) = build(&doc);
        assert!(notes.iter().any(|note| note.contains("inserted")));
        assert_eq!(
            g.analysis()[&VertexId::from_raw("main.orders.trg_touch")].state,
            schemagraph_core::AnalysisState::Partial
        );
    }

    #[test]
    fn trigger의_new_컬럼은_발사테이블_member_reads가_된다() {
        let doc = doc_with_trigger(
            "CREATE TRIGGER trg_touch AFTER INSERT ON orders \
             BEGIN UPDATE customers SET name = name WHERE id = NEW.customer_id; END",
        );
        let (g, _) = build(&doc);
        assert!(g.edges().iter().any(|e| e.kind == EdgeKind::Reads
            && e.from.as_str() == "main.orders.trg_touch"
            && e.to.as_str() == "main.orders.customer_id"));
    }

    #[test]
    fn trigger_본문의_조인테이블은_reads가_된다() {
        let doc = doc_with_trigger(
            "CREATE TRIGGER trg_touch AFTER INSERT ON orders \
             BEGIN UPDATE customers SET name = name \
             WHERE id IN (SELECT customer_id FROM orders); END",
        );
        let (g, _) = build(&doc);
        // 쓰기 대상 customers는 writes, 서브쿼리의 orders는 reads.
        assert!(g.edges().iter().any(|e| e.kind == EdgeKind::Writes
            && e.from.as_str() == "main.orders.trg_touch"
            && e.to.as_str() == "main.customers"));
        assert!(g.edges().iter().any(|e| e.kind == EdgeKind::Reads
            && e.from.as_str() == "main.orders.trg_touch"
            && e.to.as_str() == "main.orders"));
    }

    #[test]
    fn trigger_껍질이_없으면_통째로_파싱한다() {
        // reader가 내부 문장만 저장한 경우를 흉내낸다.
        let doc = doc_with_trigger("DELETE FROM customers WHERE id = 1");
        let (g, notes) = build(&doc);
        assert!(notes.is_empty(), "notes: {notes:?}");
        assert!(g.edges().iter().any(|e| e.kind == EdgeKind::Writes
            && e.from.as_str() == "main.orders.trg_touch"
            && e.to.as_str() == "main.customers"));
    }

    #[test]
    fn trigger_파싱_실패는_notes로_보고된다() {
        let doc = doc_with_trigger("CREATE TRIGGER t BEGIN ((( 깨짐; END");
        let (_g, notes) = build(&doc);
        assert!(notes.iter().any(|n| n.contains("parse failed")));
    }

    /// routine이 달린 document — PG 방언의 EXECUTE FUNCTION trigger도 실험한다.
    fn doc_with_routine(routines: Vec<RoutineDoc>, trigger_body: &str) -> CatalogDocument {
        CatalogDocument {
            context: None,
            dependencies: Vec::new(),
            version: 1,
            dialect: "postgres".into(),
            reader: "test".into(),
            limitations: vec![],
            schemas: vec![SchemaDoc {
                name: "public".into(),
                routines,
                objects: vec![
                    {
                        let mut t = table("orders", vec![col("id", 1), col("customer_id", 2)]);
                        if !trigger_body.is_empty() {
                            t.triggers.push(TriggerDoc {
                                name: "trg_touch".into(),
                                body: Some(trigger_body.to_owned()),
                            });
                        }
                        t
                    },
                    table("customers", vec![col("id", 1), col("name", 2)]),
                ],
            }],
        }
    }

    fn routine(name: &str, language: Option<&str>, body: &str) -> RoutineDoc {
        RoutineDoc {
            source: None,
            name: name.into(),
            kind: "function".into(),
            language: language.map(|l| l.to_owned()),
            body: Some(body.to_owned()),
            signature: None,
            usage: None,
            member_of: None,
        }
    }

    #[test]
    fn sql_routine의_질의가_reads_간선이_된다() {
        let doc = doc_with_routine(
            vec![routine(
                "order_count",
                Some("sql"),
                "SELECT count(*) FROM orders",
            )],
            "",
        );
        let (g, notes) = build(&doc);
        assert!(notes.is_empty(), "notes: {notes:?}");
        assert!(g.edges().iter().any(|e| e.kind == EdgeKind::Reads
            && e.from.as_str() == "public.order_count"
            && e.to.as_str() == "public.orders"));
    }

    #[test]
    fn create_function_껍질의_as_몸체를_벗겨_파싱한다() {
        let doc = doc_with_routine(
            vec![routine(
                "touch_customer",
                Some("sql"),
                "CREATE FUNCTION touch_customer() RETURNS int AS $$ UPDATE customers SET name = name WHERE id = 1; $$ LANGUAGE sql",
            )],
            "",
        );
        let (g, _) = build(&doc);
        assert!(g.edges().iter().any(|e| e.kind == EdgeKind::Writes
            && e.from.as_str() == "public.touch_customer"
            && e.to.as_str() == "public.customers"));
    }

    #[test]
    fn 이름_충돌한_routine의_몸체_간선이_분리된_정점에_붙는다() {
        // 테이블 orders와 같은 이름의 함수 — document_to_graph가
        // public.orders@function으로 분리한다. 파서가 옛 id로 유령 간선을
        // 만들면 이 테스트가 잡는다.
        let doc = doc_with_routine(
            vec![routine(
                "orders",
                Some("sql"),
                "SELECT count(*) FROM customers",
            )],
            "",
        );
        let (g, notes) = build(&doc);
        assert!(
            g.edges().iter().any(|e| e.kind == EdgeKind::Reads
                && e.from.as_str() == "public.orders@function"
                && e.to.as_str() == "public.customers"),
            "notes: {notes:?}"
        );
        // 테이블 정점을 소스로 하는 몸체 간선은 없어야 한다.
        assert!(!g
            .edges()
            .iter()
            .any(|e| e.from.as_str() == "public.orders" && e.kind == EdgeKind::Reads));
    }

    #[test]
    fn 호출_해석은_정확한_id가_routine일_때만_받는다() {
        // 함수 orders는 테이블과 충돌해 @function으로 분리됐다. 호출자가
        // orders()를 부르면 정확한 id(public.orders)는 테이블이라 건너뛰고
        // 이름 스캔으로 분리된 함수 정점을 찾아야 한다.
        let doc = doc_with_routine(
            vec![
                routine("orders", Some("sql"), "SELECT 1"),
                routine("caller", Some("sql"), "SELECT orders()"),
            ],
            "",
        );
        let (g, notes) = build(&doc);
        assert!(
            g.edges().iter().any(|e| e.kind == EdgeKind::Calls
                && e.from.as_str() == "public.caller"
                && e.to.as_str() == "public.orders@function"),
            "notes: {notes:?}"
        );
        assert!(!g
            .edges()
            .iter()
            .any(|e| e.kind == EdgeKind::Calls && e.to.as_str() == "public.orders"));
    }

    #[test]
    fn tsql의_try_if_exec_declare가_전부_추출된다() {
        // T-SQL 절차형 구문 — DECLARE/SET @v=/IF..BEGIN/TRY..CATCH/EXEC가
        // 모두 도매 파싱을 깨뜨리지만 문장 추출로 전부 간선이 된다.
        let mut doc = doc_with_routine(
            vec![
                routine("audit_orders", Some("sql"), "BEGIN END"),
                routine(
                    "sync_orders",
                    Some("sql"),
                    "BEGIN \
                     SET NOCOUNT ON; \
                     DECLARE @n INT; \
                     SET @n = (SELECT COUNT(*) FROM customers); \
                     IF @n > 0 BEGIN \
                       UPDATE orders SET customer_id = 1 WHERE id = @n; \
                     END \
                     BEGIN TRY \
                       EXEC audit_orders @n; \
                     END TRY \
                     BEGIN CATCH \
                       INSERT INTO customers (name) VALUES ('err'); \
                     END CATCH \
                     END",
                ),
            ],
            "",
        );
        doc.dialect = "sqlserver".into();
        let (g, notes) = build(&doc);
        assert!(g.edges().iter().any(|e| e.kind == EdgeKind::Reads
            && e.from.as_str() == "public.sync_orders"
            && e.to.as_str() == "public.customers"));
        assert!(g.edges().iter().any(|e| e.kind == EdgeKind::Writes
            && e.from.as_str() == "public.sync_orders"
            && e.to.as_str() == "public.orders"));
        assert!(g.edges().iter().any(|e| e.kind == EdgeKind::Calls
            && e.from.as_str() == "public.sync_orders"
            && e.to.as_str() == "public.audit_orders"));
        assert!(g.edges().iter().any(|e| e.kind == EdgeKind::Writes
            && e.from.as_str() == "public.sync_orders"
            && e.to.as_str() == "public.customers"));
        assert!(
            !notes
                .iter()
                .any(|n| n.contains("sync_orders") && n.contains("parse failed")),
            "notes: {notes:?}"
        );
    }

    #[test]
    fn tsql의_while_begin과_cursor_for_select가_추출된다() {
        let mut doc = doc_with_routine(
            vec![routine(
                "drain",
                Some("sql"),
                "BEGIN \
                 DECLARE c CURSOR FOR SELECT id FROM customers; \
                 DECLARE @i INT = 0; \
                 WHILE @i < 10 BEGIN \
                   UPDATE orders SET customer_id = @i WHERE id = @i; \
                   SET @i = @i + 1; \
                 END \
                 END",
            )],
            "",
        );
        doc.dialect = "sqlserver".into();
        let (g, _) = build(&doc);
        assert!(g.edges().iter().any(|e| e.kind == EdgeKind::Reads
            && e.from.as_str() == "public.drain"
            && e.to.as_str() == "public.customers"));
        assert!(g.edges().iter().any(|e| e.kind == EdgeKind::Writes
            && e.from.as_str() == "public.drain"
            && e.to.as_str() == "public.orders"));
    }

    #[test]
    fn tsql_부분복구는_간선과_불완전_한계를_함께_남긴다() {
        // EXEC('..'+@t)는 동적 실행이라 못 읽지만 UPDATE는 살아야 한다 —
        // 도매 실패가 몸체 전체를 버리는 일은 없어야 한다.
        let mut doc = doc_with_routine(
            vec![routine(
                "wipe",
                Some("sql"),
                "BEGIN UPDATE customers SET name = 'x' WHERE id = 1; \
                 EXEC('DELETE FROM ' + @t); END",
            )],
            "",
        );
        doc.dialect = "sqlserver".into();
        let (g, notes) = build(&doc);
        assert!(g.edges().iter().any(|e| e.kind == EdgeKind::Writes
            && e.from.as_str() == "public.wipe"
            && e.to.as_str() == "public.customers"));
        assert!(
            notes
                .iter()
                .any(|n| n.contains("wipe") && n.contains("미추출")),
            "notes: {notes:?}"
        );
    }

    #[test]
    fn 몸체_선두의_주석이_첫_문장을_삼키지_않는다() {
        // 프로브는 CREATE 앞의 -- 주석을 몸체에 그대로 싣는다 — 주석이
        // 문장 머리 판별을 가리면 첫 조각째 버려져 간선이 빠진다.
        let mut doc = doc_with_routine(
            vec![routine(
                "drain",
                Some("sql"),
                "-- cursor가 읽는 대상\n\
                 CREATE PROCEDURE drain AS \
                 BEGIN DECLARE c CURSOR FOR SELECT id FROM customers; END",
            )],
            "",
        );
        doc.dialect = "sqlserver".into();
        let (g, _) = build(&doc);
        assert!(g.edges().iter().any(|e| e.kind == EdgeKind::Reads
            && e.from.as_str() == "public.drain"
            && e.to.as_str() == "public.customers"));
    }

    #[test]
    fn tsql_복구불가_몸체는_파싱실패로_정직하게_보고한다() {
        // 간선이 하나도 안 나오면 부분 복구 성공으로 꾸미지 않고
        // 원래 파싱 실패를 그대로 보고한다.
        let mut doc = doc_with_routine(
            vec![routine(
                "noop",
                Some("sql"),
                "BEGIN PRINT 'x'; THROW 50001, 'e', 1; END",
            )],
            "",
        );
        doc.dialect = "sqlserver".into();
        let (g, notes) = build(&doc);
        assert!(!g.edges().iter().any(|e| e.from.as_str() == "public.noop"));
        assert!(
            notes
                .iter()
                .any(|n| n.contains("noop") && n.contains("parse failed")),
            "notes: {notes:?}"
        );
    }

    #[test]
    fn 미지원_언어_routine은_파싱대신_한계를_보고한다() {
        let doc = doc_with_routine(
            vec![routine(
                "touch_customer",
                Some("plpython3u"),
                "UPDATE customers SET name = name WHERE id = 1",
            )],
            "",
        );
        let (g, notes) = build(&doc);
        assert!(notes.iter().any(|n| n.contains("plpython3u")));
        assert!(!g
            .edges()
            .iter()
            .any(|e| e.from.as_str() == "public.touch_customer"));
    }

    #[test]
    fn plpgsql_몸체의_dml이_간선이_된다() {
        let doc = doc_with_routine(
            vec![routine(
                "touch_customer",
                Some("plpgsql"),
                "BEGIN UPDATE customers SET name = name WHERE id = 1; END",
            )],
            "",
        );
        let (g, notes) = build(&doc);
        assert!(notes.is_empty(), "notes: {notes:?}");
        assert!(g.edges().iter().any(|e| e.kind == EdgeKind::Writes
            && e.from.as_str() == "public.touch_customer"
            && e.to.as_str() == "public.customers"));
    }

    #[test]
    fn plpgsql의_선언부_조건_루프가_전부_추출된다() {
        // 선언 기본값의 서브쿼리, IF의 EXISTS, FOR .. IN 질의 — 어느 것도
        // 몸체 밖 참조가 아니라 간선이어야 한다.
        let doc = doc_with_routine(
            vec![routine(
                "sync_orders",
                Some("plpgsql"),
                "DECLARE max_id int := (SELECT max(id) FROM orders); \
                 BEGIN \
                 IF EXISTS (SELECT 1 FROM customers WHERE id = 1) THEN \
                   UPDATE orders SET customer_id = 1 WHERE id = max_id; \
                 END IF; \
                 FOR r IN (SELECT id FROM customers) LOOP \
                   UPDATE orders SET customer_id = r.id WHERE id = r.id; \
                 END LOOP; \
                 END",
            )],
            "",
        );
        let (g, notes) = build(&doc);
        assert!(notes.is_empty(), "notes: {notes:?}");
        assert!(g.edges().iter().any(|e| e.kind == EdgeKind::Reads
            && e.from.as_str() == "public.sync_orders"
            && e.to.as_str() == "public.customers"));
        assert!(g.edges().iter().any(|e| e.kind == EdgeKind::Writes
            && e.from.as_str() == "public.sync_orders"
            && e.to.as_str() == "public.orders"));
    }

    #[test]
    fn plpgsql의_리터럴과_format_동적sql은_파싱한다() {
        // PostgreSQL format은 제한된 %I/%L/%s 형식과 상수 인자를 평가한다.
        let doc = doc_with_routine(
            vec![routine(
                "wipe",
                Some("plpgsql"),
                "BEGIN EXECUTE 'DELETE FROM customers'; \
                 EXECUTE format('DELETE FROM %I', 'orders'); END",
            )],
            "",
        );
        let (g, notes) = build(&doc);
        assert!(g.edges().iter().any(|e| e.kind == EdgeKind::Writes
            && e.from.as_str() == "public.wipe"
            && e.to.as_str() == "public.customers"));
        assert!(g.edges().iter().any(|e| e.kind == EdgeKind::Writes
            && e.from.as_str() == "public.wipe"
            && e.to.as_str() == "public.orders"));
        assert!(notes.is_empty(), "notes: {notes:?}");
    }

    #[test]
    fn dynamic_sql_does_not_treat_a_literal_prefix_as_the_complete_command() {
        for command in [
            "EXECUTE 'DELETE FROM customers' || suffix",
            "EXECUTE ('DELETE FROM customers' || suffix)",
        ] {
            let doc = doc_with_routine(
                vec![routine(
                    "dynamic_cleanup",
                    Some("plpgsql"),
                    &format!("BEGIN {command}; UPDATE orders SET customer_id = 1; END"),
                )],
                "",
            );
            let (graph, notes) = build(&doc);
            let targets: BTreeSet<_> = graph
                .edges()
                .iter()
                .filter(|edge| edge.from.as_str() == "public.dynamic_cleanup")
                .map(|edge| (edge.kind, edge.to.as_str()))
                .collect();
            assert_eq!(
                targets,
                BTreeSet::from([
                    (EdgeKind::Writes, "public.orders"),
                    (EdgeKind::Writes, "public.orders.customer_id"),
                ])
            );
            assert!(
                notes.iter().any(|note| note.contains("1건 미추출")),
                "{notes:?}"
            );
        }
    }

    #[test]
    fn dynamic_sql_evaluates_a_whole_constant_concatenation() {
        for (dialect, language, command) in [
            (
                "postgres",
                "plpgsql",
                "EXECUTE ('DELETE FROM ' || 'customers')",
            ),
            (
                "oracle",
                "plsql",
                "EXECUTE IMMEDIATE ('DELETE FROM ' || 'customers')",
            ),
            ("sqlserver", "sql", "EXEC(N'DELETE FROM ' + N'customers')"),
        ] {
            let mut doc = doc_with_routine(
                vec![routine(
                    "dynamic_cleanup",
                    Some(language),
                    &format!("BEGIN {command}; END"),
                )],
                "",
            );
            doc.dialect = dialect.into();
            let (graph, notes) = build(&doc);
            assert!(notes.is_empty(), "{dialect}: {notes:?}");
            assert!(graph
                .edges()
                .iter()
                .any(|edge| edge.kind == EdgeKind::Writes
                    && edge.from.as_str() == "public.dynamic_cleanup"
                    && edge.to.as_str() == "public.customers"));
        }
    }

    #[test]
    fn dynamic_sql_tracks_straight_line_variables_and_reassignment() {
        let doc = doc_with_routine(
            vec![routine(
                "dynamic_cleanup",
                Some("plpgsql"),
                "DECLARE sql_text text := 'DELETE FROM ' || 'customers'; \
                 BEGIN sql_text := 'DELETE FROM orders'; EXECUTE sql_text; END",
            )],
            "",
        );
        let (graph, notes) = build(&doc);
        assert!(notes.is_empty(), "{notes:?}");
        assert!(graph
            .edges()
            .iter()
            .any(|edge| edge.kind == EdgeKind::Writes
                && edge.from.as_str() == "public.dynamic_cleanup"
                && edge.to.as_str() == "public.orders"));
        assert!(!graph
            .edges()
            .iter()
            .any(|edge| edge.kind == EdgeKind::Writes
                && edge.from.as_str() == "public.dynamic_cleanup"
                && edge.to.as_str() == "public.customers"));
    }

    #[test]
    fn dynamic_sql_tracks_tsql_set_variables() {
        let mut doc = doc_with_routine(
            vec![routine(
                "dynamic_cleanup",
                Some("sql"),
                "BEGIN DECLARE @sql_text nvarchar(max) = N'DELETE FROM customers'; \
                 SET @sql_text = N'DELETE FROM orders'; EXEC @sql_text; END",
            )],
            "",
        );
        doc.dialect = "sqlserver".into();
        let (graph, notes) = build(&doc);
        assert!(notes.is_empty(), "{notes:?}");
        assert!(graph
            .edges()
            .iter()
            .any(|edge| edge.kind == EdgeKind::Writes
                && edge.from.as_str() == "public.dynamic_cleanup"
                && edge.to.as_str() == "public.orders"));
    }

    #[test]
    fn dynamic_sql_tracks_all_tsql_select_assignments() {
        let mut doc = doc_with_routine(
            vec![routine(
                "dynamic_cleanup",
                Some("sql"),
                "BEGIN DECLARE @a nvarchar(max) = N'DELETE FROM customers'; \
                 DECLARE @b nvarchar(max) = N'DELETE FROM customers'; \
                 SELECT @a = N'x', @b = N'DELETE FROM orders'; EXEC(@b); END",
            )],
            "",
        );
        doc.dialect = "sqlserver".into();
        let (graph, notes) = build(&doc);
        assert!(notes.is_empty(), "{notes:?}");
        assert!(graph
            .edges()
            .iter()
            .any(|edge| edge.kind == EdgeKind::Writes
                && edge.from.as_str() == "public.dynamic_cleanup"
                && edge.to.as_str() == "public.orders"));
        assert!(!graph
            .edges()
            .iter()
            .any(|edge| edge.kind == EdgeKind::Writes
                && edge.from.as_str() == "public.dynamic_cleanup"
                && edge.to.as_str() == "public.customers"));
    }

    #[test]
    fn dynamic_sql_does_not_assume_tsql_select_assignment_order() {
        let mut doc = doc_with_routine(
            vec![routine(
                "dynamic_cleanup",
                Some("sql"),
                "BEGIN DECLARE @a nvarchar(max) = N'DELETE FROM customers'; \
                 DECLARE @b nvarchar(max) = N'DELETE FROM customers'; \
                 SELECT @a = @b, @b = N'DELETE FROM orders'; \
                 EXEC(@a); EXEC(@b); END",
            )],
            "",
        );
        doc.dialect = "sqlserver".into();
        let (graph, notes) = build(&doc);
        assert!(graph
            .edges()
            .iter()
            .any(|edge| edge.kind == EdgeKind::Writes
                && edge.from.as_str() == "public.dynamic_cleanup"
                && edge.to.as_str() == "public.orders"));
        assert!(!graph
            .edges()
            .iter()
            .any(|edge| edge.kind == EdgeKind::Writes
                && edge.from.as_str() == "public.dynamic_cleanup"
                && edge.to.as_str() == "public.customers"));
        assert!(
            notes.iter().any(|note| note.contains("미추출")),
            "{notes:?}"
        );
    }

    #[test]
    fn dynamic_sql_requires_tsql_unicode_literals_and_targets() {
        for (name, declaration, expect_orders) in [
            (
                "plain_unicode",
                "DECLARE @sql nvarchar(max) = 'DELETE FROM orders -- 한글';",
                false,
            ),
            (
                "n_unicode",
                "DECLARE @sql nvarchar(max) = N'DELETE FROM orders -- 한글';",
                true,
            ),
            (
                "n_to_varchar",
                "DECLARE @sql varchar(max) = N'DELETE FROM orders -- 한글';",
                false,
            ),
        ] {
            let mut doc = doc_with_routine(
                vec![routine(
                    name,
                    Some("sql"),
                    &format!("BEGIN {declaration} EXEC(@sql); END"),
                )],
                "",
            );
            doc.dialect = "sqlserver".into();
            let (graph, notes) = build(&doc);
            let has_orders = graph.edges().iter().any(|edge| {
                edge.kind == EdgeKind::Writes
                    && edge.from.as_str() == format!("public.{name}")
                    && edge.to.as_str() == "public.orders"
            });
            assert_eq!(has_orders, expect_orders, "{name}: {notes:?}");
            if !expect_orders {
                assert!(notes
                    .iter()
                    .any(|note| note.contains(name) && note.contains("미추출")));
            } else {
                assert!(notes.is_empty(), "{name}: {notes:?}");
            }
        }
    }

    #[test]
    fn dynamic_sql_rejects_truncated_text_declarations() {
        let doc = doc_with_routine(
            vec![routine(
                "dynamic_cleanup",
                Some("plpgsql"),
                "DECLARE sql_text varchar(8) := 'DELETE FROM customers'; \
                 BEGIN EXECUTE sql_text; END",
            )],
            "",
        );
        let (graph, notes) = build(&doc);
        assert!(!graph
            .edges()
            .iter()
            .any(|edge| edge.from.as_str() == "public.dynamic_cleanup"));
        assert!(
            notes.iter().any(|note| note.contains("미추출")),
            "{notes:?}"
        );
    }

    #[test]
    fn dynamic_sql_invalidates_exec_output_arguments_and_fetch_targets() {
        let mut doc = doc_with_routine(
            vec![
                routine("mutate", Some("sql"), "SELECT 1"),
                routine(
                    "exec_caller",
                    Some("sql"),
                    "BEGIN DECLARE @sql nvarchar(max) = N'DELETE FROM customers'; \
                     EXEC mutate @sql OUTPUT; EXEC(@sql); END",
                ),
                routine(
                    "fetch_caller",
                    Some("plpgsql"),
                    "DECLARE sql_text text := 'DELETE FROM customers'; \
                     DECLARE c CURSOR FOR SELECT id FROM customers; \
                     BEGIN FETCH c INTO sql_text; EXECUTE sql_text; END",
                ),
            ],
            "",
        );
        doc.dialect = "sqlserver".into();
        let (graph, notes) = build(&doc);
        assert!(!graph.edges().iter().any(|edge| {
            (edge.from.as_str() == "public.exec_caller"
                || edge.from.as_str() == "public.fetch_caller")
                && edge.kind == EdgeKind::Writes
        }));
        assert!(notes
            .iter()
            .any(|note| note.contains("exec_caller") && note.contains("미추출")));
        assert!(notes
            .iter()
            .any(|note| note.contains("fetch_caller") && note.contains("미추출")));

        let mut callee = routine("mutate", Some("plpgsql"), "SELECT 1");
        callee.kind = "procedure".into();
        let call_doc = doc_with_routine(
            vec![
                callee,
                routine(
                    "call_caller",
                    Some("plpgsql"),
                    "DECLARE sql_text text := 'DELETE FROM customers'; \
                     BEGIN CALL mutate(sql_text); EXECUTE sql_text; END",
                ),
            ],
            "",
        );
        let (call_graph, call_notes) = build(&call_doc);
        assert!(!call_graph.edges().iter().any(|edge| {
            edge.from.as_str() == "public.call_caller" && edge.kind == EdgeKind::Writes
        }));
        assert!(
            call_notes
                .iter()
                .any(|note| note.contains("call_caller") && note.contains("미추출")),
            "{call_notes:?}"
        );
    }

    #[test]
    fn dynamic_sql_does_not_fold_shadowed_postgres_format() {
        let doc = doc_with_routine(
            vec![
                routine("format", Some("sql"), "SELECT 1"),
                routine(
                    "dynamic_cleanup",
                    Some("plpgsql"),
                    "BEGIN EXECUTE format('DELETE FROM %I', 'customers'); END",
                ),
            ],
            "",
        );
        let (graph, notes) = build(&doc);
        assert!(!graph
            .edges()
            .iter()
            .any(|edge| edge.from.as_str() == "public.dynamic_cleanup"));
        assert!(
            notes
                .iter()
                .any(|note| note.contains("dynamic_cleanup") && note.contains("미추출")),
            "{notes:?}"
        );
    }

    #[test]
    fn dynamic_sql_invalidates_variables_after_a_branch() {
        let doc = doc_with_routine(
            vec![routine(
                "dynamic_cleanup",
                Some("plpgsql"),
                "DECLARE sql_text text := 'DELETE FROM customers'; \
                 BEGIN IF true THEN sql_text := 'DELETE FROM orders'; END IF; \
                 EXECUTE sql_text; END",
            )],
            "",
        );
        let (graph, notes) = build(&doc);
        assert!(!graph
            .edges()
            .iter()
            .any(|edge| edge.from.as_str() == "public.dynamic_cleanup"
                && edge.kind == EdgeKind::Writes));
        assert!(
            notes.iter().any(|note| note.contains("1건 미추출")),
            "{notes:?}"
        );
    }

    #[test]
    fn dynamic_sql_unknown_assignment_and_nested_scope_do_not_guess() {
        for body in [
            "DECLARE sql_text text := 'DELETE FROM customers'; BEGIN sql_text := make_sql(); EXECUTE sql_text; END",
            "DECLARE sql_text text := 'DELETE FROM customers'; BEGIN BEGIN sql_text := 'DELETE FROM orders'; END; EXECUTE sql_text; END",
        ] {
            let doc = doc_with_routine(
                vec![routine("dynamic_cleanup", Some("plpgsql"), body)],
                "",
            );
            let (graph, notes) = build(&doc);
            assert!(!graph
                .edges()
                .iter()
                .any(|edge| edge.from.as_str() == "public.dynamic_cleanup"));
            assert!(notes.iter().any(|note| note.contains("미추출")), "{notes:?}");
        }
    }

    #[test]
    fn dynamic_sql_invalidates_values_written_by_select_into() {
        let doc = doc_with_routine(
            vec![routine(
                "dynamic_cleanup",
                Some("plpgsql"),
                "DECLARE sql_text text := 'DELETE FROM customers'; \
                 BEGIN SELECT body INTO sql_text FROM command_text; EXECUTE sql_text; END",
            )],
            "",
        );
        let (graph, notes) = build(&doc);
        assert!(!graph
            .edges()
            .iter()
            .any(|edge| edge.from.as_str() == "public.dynamic_cleanup"));
        assert!(
            notes.iter().any(|note| note.contains("미추출")),
            "{notes:?}"
        );
    }

    #[test]
    fn dynamic_sql_rejects_format_outside_postgres() {
        let mut doc = doc_with_routine(
            vec![routine(
                "dynamic_cleanup",
                Some("plsql"),
                "BEGIN EXECUTE IMMEDIATE format('DELETE FROM %I', 'customers'); END",
            )],
            "",
        );
        doc.dialect = "oracle".into();
        let (graph, notes) = build(&doc);
        assert!(!graph
            .edges()
            .iter()
            .any(|edge| edge.from.as_str() == "public.dynamic_cleanup"));
        assert!(
            notes.iter().any(|note| note.contains("미추출")),
            "{notes:?}"
        );
    }

    #[test]
    fn dynamic_sql_recovers_complete_quoted_commands_across_dialects() {
        for (dialect, language, command) in [
            (
                "postgres",
                "plpgsql",
                "EXECUTE $sql$UPDATE customers SET name = 'BEGIN; END'$sql$",
            ),
            (
                "postgres",
                "plpgsql",
                "EXECUTE (('UPDATE customers SET name = ''한글'''))",
            ),
            (
                "oracle",
                "plsql",
                "EXECUTE IMMEDIATE q'[UPDATE customers SET name = 'BEGIN; END']'",
            ),
            (
                "oracle",
                "plsql",
                "EXECUTE IMMEDIATE q'!UPDATE customers SET name = '한글'!'",
            ),
            (
                "oracle",
                "plsql",
                "EXECUTE IMMEDIATE q'한UPDATE customers SET name = 'BEGIN; END'한'",
            ),
            (
                "sqlserver",
                "sql",
                "EXEC(N'UPDATE customers SET name = ''BEGIN; END''')",
            ),
            (
                "sqlserver",
                "sql",
                "EXECUTE(N'UPDATE customers SET name = ''한글''')",
            ),
        ] {
            let mut doc = doc_with_routine(
                vec![routine(
                    "dynamic_touch",
                    Some(language),
                    &format!("BEGIN {command}; END"),
                )],
                "",
            );
            doc.dialect = dialect.into();
            let (graph, notes) = build(&doc);
            assert!(notes.is_empty(), "{dialect}: {command}: {notes:?}");
            assert!(
                graph
                    .edges()
                    .iter()
                    .any(|edge| edge.kind == EdgeKind::Writes
                        && edge.from.as_str() == "public.dynamic_touch"
                        && edge.to.as_str() == "public.customers"),
                "{dialect}: {command}"
            );
        }
    }

    #[test]
    fn dynamic_sql_recovers_cursor_loop_and_return_queries_with_binding_calls() {
        for command in [
            "RETURN QUERY EXECUTE $q$SELECT id FROM customers WHERE id = $1$q$ USING customer_key()",
            "FOR row IN EXECUTE 'SELECT id FROM customers WHERE id = $1' USING customer_key() LOOP NULL; END LOOP",
            "OPEN cur FOR EXECUTE 'SELECT id FROM customers WHERE id = $1' USING customer_key()",
            "EXECUTE 'SELECT id FROM customers WHERE id = $1' INTO STRICT found_id USING customer_key()",
        ] {
            let doc = doc_with_routine(
                vec![
                    routine("customer_key", Some("sql"), "SELECT 1"),
                    routine("dynamic_read", Some("plpgsql"), &format!("BEGIN {command}; END")),
                ],
                "",
            );
            let (graph, notes) = build(&doc);
            assert!(notes.is_empty(), "{command}: {notes:?}");
            for (kind, target) in [(EdgeKind::Reads, "public.customers"), (EdgeKind::Calls, "public.customer_key")] {
                assert!(graph.edges().iter().any(|edge| edge.kind == kind
                    && edge.from.as_str() == "public.dynamic_read"
                    && edge.to.as_str() == target), "{command}: missing {target}");
            }
        }
    }

    #[test]
    fn dynamic_sql_does_not_resolve_remote_execution_as_local_dependencies() {
        let mut doc = doc_with_routine(
            vec![routine("remote_read", Some("sql"),
                "BEGIN EXEC(N'SELECT id FROM customers') AT linked_db; UPDATE orders SET customer_id = 1; END")],
            "",
        );
        doc.dialect = "sqlserver".into();
        let (graph, notes) = build(&doc);
        assert!(
            notes.iter().any(|note| note.contains("미추출")),
            "{notes:?}"
        );
        assert!(!graph
            .edges()
            .iter()
            .any(|edge| edge.from.as_str() == "public.remote_read"
                && edge.to.as_str() == "public.customers"));
        assert!(graph
            .edges()
            .iter()
            .any(|edge| edge.kind == EdgeKind::Writes
                && edge.from.as_str() == "public.remote_read"
                && edge.to.as_str() == "public.orders"));
    }

    #[test]
    fn dynamic_sql_does_not_drop_database_qualifiers_for_local_edges() {
        let mut doc = doc_with_routine(
            vec![routine(
                "remote_read",
                Some("sql"),
                "BEGIN EXEC(N'SELECT id FROM otherdb.dbo.customers'); \
                 UPDATE orders SET customer_id = 1; END",
            )],
            "",
        );
        doc.dialect = "sqlserver".into();
        let (graph, notes) = build(&doc);
        assert!(!graph.edges().iter().any(|edge| {
            edge.from.as_str() == "public.remote_read" && edge.to.as_str() == "public.customers"
        }));
        assert!(graph.edges().iter().any(|edge| {
            edge.kind == EdgeKind::Writes
                && edge.from.as_str() == "public.remote_read"
                && edge.to.as_str() == "public.orders"
        }));
        assert!(
            notes.iter().any(|note| {
                note.contains("otherdb.dbo.customers") && note.contains("database")
            }),
            "{notes:?}"
        );
    }

    #[test]
    fn dynamic_sql_keywords_inside_data_or_comments_are_not_executed() {
        let doc = doc_with_routine(
            vec![
                routine("touch_customer", Some("sql"), "SELECT 1"),
                routine(
                    "message",
                    Some("sql"),
                    "SELECT 'EXECUTE FUNCTION touch_customer(); BEGIN' AS message /* END */",
                ),
            ],
            "",
        );
        let (graph, notes) = build(&doc);
        assert!(notes.is_empty(), "{notes:?}");
        assert!(!graph
            .edges()
            .iter()
            .any(|edge| edge.from.as_str() == "public.message"));
    }

    #[test]
    fn dynamic_sql_handles_short_and_empty_bodies_without_panicking() {
        for body in ["x", ";", " "] {
            let doc = doc_with_routine(vec![routine("short_body", Some("plpgsql"), body)], "");
            let (graph, notes) = build(&doc);
            if body == "x" {
                assert!(
                    notes.iter().any(|note| note.contains("1건 미추출")),
                    "{notes:?}"
                );
            }
            assert!(!graph
                .edges()
                .iter()
                .any(|edge| edge.from.as_str() == "public.short_body"));
        }
    }

    #[test]
    fn dynamic_sql_quote_boundaries_preserve_data_and_quoted_identifiers() {
        for (dialect, body) in [
            (
                "postgres",
                "SELECT E'escaped \\' EXECUTE FUNCTION touch_customer(); BEGIN'",
            ),
            (
                "postgres",
                "SELECT $q$EXECUTE FUNCTION touch_customer(); BEGIN$q$",
            ),
            ("sqlserver", "SELECT id AS [BEGIN] FROM orders"),
        ] {
            let mut doc = doc_with_routine(
                vec![
                    routine("touch_customer", Some("sql"), "SELECT 1"),
                    routine("message", Some("sql"), body),
                ],
                "",
            );
            doc.dialect = dialect.into();
            let (graph, notes) = build(&doc);
            assert!(notes.is_empty(), "{body}: {notes:?}");
            assert!(
                !graph
                    .edges()
                    .iter()
                    .any(|edge| edge.from.as_str() == "public.message"
                        && edge.kind == EdgeKind::Calls)
            );
            if dialect == "sqlserver" {
                assert!(graph
                    .edges()
                    .iter()
                    .any(|edge| edge.from.as_str() == "public.message"
                        && edge.to.as_str() == "public.orders"));
            }
        }
    }

    #[test]
    fn dynamic_sql_incomplete_literals_do_not_become_routine_calls() {
        for command in [
            "EXEC(N'SELECT id FROM customers'",
            "EXEC N'SELECT id FROM customers",
        ] {
            let mut doc = doc_with_routine(
                vec![
                    routine("N", Some("sql"), "SELECT 1"),
                    routine("broken", Some("sql"), command),
                ],
                "",
            );
            doc.dialect = "sqlserver".into();
            let (graph, notes) = build(&doc);
            assert!(!notes.is_empty(), "{command}");
            assert!(!graph
                .edges()
                .iter()
                .any(|edge| edge.from.as_str() == "public.broken"));
        }
    }

    #[test]
    fn plsql의_bare_호출이_calls_간선이_된다() {
        // PL/SQL은 CALL 없이 프로시저를 부른다 — f(...) 꼴을 CALL로 재작성.
        let mut callee = routine(
            "touch_customer",
            Some("plsql"),
            "BEGIN UPDATE customers SET name = name WHERE id = 1; END",
        );
        callee.kind = "procedure".into();
        let doc = doc_with_routine(
            vec![
                callee,
                routine("nightly", Some("plsql"), "BEGIN touch_customer(1); END"),
            ],
            "",
        );
        let (g, notes) = build(&doc);
        assert!(notes.is_empty(), "notes: {notes:?}");
        assert!(g.edges().iter().any(|e| e.kind == EdgeKind::Calls
            && e.from.as_str() == "public.nightly"
            && e.to.as_str() == "public.touch_customer"));
    }

    #[test]
    fn plsql의_is_헤더와_선언부를_건너뛴다() {
        // Oracle ALL_SOURCE 원문은 PROCEDURE p IS 헤더를 포함한다.
        let doc = doc_with_routine(
            vec![routine(
                "touch_customer",
                Some("plsql"),
                "PROCEDURE touch_customer IS cnt NUMBER; \
                 BEGIN UPDATE customers SET name = name WHERE id = 1; END;",
            )],
            "",
        );
        let (g, notes) = build(&doc);
        assert!(notes.is_empty(), "notes: {notes:?}");
        assert!(g.edges().iter().any(|e| e.kind == EdgeKind::Writes
            && e.from.as_str() == "public.touch_customer"
            && e.to.as_str() == "public.customers"));
    }

    #[test]
    fn plpgsql의_perform과_return_query가_reads가_된다() {
        let doc = doc_with_routine(
            vec![routine(
                "report",
                Some("plpgsql"),
                "BEGIN PERFORM id FROM customers; \
                 RETURN QUERY SELECT id FROM orders; END",
            )],
            "",
        );
        let (g, notes) = build(&doc);
        assert!(notes.is_empty(), "notes: {notes:?}");
        assert!(g.edges().iter().any(|e| e.kind == EdgeKind::Reads
            && e.from.as_str() == "public.report"
            && e.to.as_str() == "public.customers"));
        assert!(g.edges().iter().any(|e| e.kind == EdgeKind::Reads
            && e.from.as_str() == "public.report"
            && e.to.as_str() == "public.orders"));
    }

    #[test]
    fn plpgsql_몸체의_세미콜론_문자열은_문장을_나누지_않는다() {
        // 'a;b' 안의 ;가 조각을 갈라 뒷 문장이 뭉개지면 UPDATE가 사라진다.
        let doc = doc_with_routine(
            vec![routine(
                "mark",
                Some("plpgsql"),
                "BEGIN UPDATE customers SET name = 'a;b' WHERE id = 1; \
                 UPDATE orders SET customer_id = 2 WHERE id = 1; END",
            )],
            "",
        );
        let (g, notes) = build(&doc);
        assert!(notes.is_empty(), "notes: {notes:?}");
        assert!(g.edges().iter().any(|e| e.kind == EdgeKind::Writes
            && e.from.as_str() == "public.mark"
            && e.to.as_str() == "public.customers"));
        assert!(g.edges().iter().any(|e| e.kind == EdgeKind::Writes
            && e.from.as_str() == "public.mark"
            && e.to.as_str() == "public.orders"));
    }

    #[test]
    fn package_body의_멤버_문장이_패키지_정점에_귀속된다() {
        // 멤버별 정점 분리는 미지원 — 대신 멤버 몸체의 간선이 패키지 정점에
        // 붙고, 귀속이 부분적이라는 한계가 보고돼야 한다.
        let mut pkg = routine(
            "order_ops",
            Some("plsql"),
            "PACKAGE BODY order_ops IS \
             PROCEDURE touch(cid IN NUMBER) IS \
             BEGIN UPDATE customers SET name = name WHERE id = cid; END touch; \
             FUNCTION count_all RETURN NUMBER IS n NUMBER; \
             BEGIN SELECT count(*) INTO n FROM orders; RETURN n; END count_all; \
             END order_ops;",
        );
        pkg.kind = "package".into();
        let doc = doc_with_routine(vec![pkg], "");
        let (g, notes) = build(&doc);
        assert!(g.edges().iter().any(|e| e.kind == EdgeKind::Writes
            && e.from.as_str() == "public.order_ops"
            && e.to.as_str() == "public.customers"));
        assert!(g.edges().iter().any(|e| e.kind == EdgeKind::Reads
            && e.from.as_str() == "public.order_ops"
            && e.to.as_str() == "public.orders"));
        assert!(
            notes.iter().any(|n| n.contains("패키지 정점에 귀속")),
            "notes: {notes:?}"
        );
    }

    #[test]
    fn member_of_멤버는_패키지_아래_정점과_contains_간선이_된다() {
        // member_of가 실린 멤버는 schema.pkg.member 정점이 되고 몸체
        // 간선은 멤버에 귀속된다 — 패키지 정점에는 스펙만 남는다.
        let mut pkg = routine(
            "order_ops",
            Some("plsql"),
            "PACKAGE order_ops IS END order_ops;",
        );
        pkg.kind = "package".into();
        let mut touch = routine(
            "touch",
            Some("plsql"),
            "PROCEDURE touch(cid IN NUMBER) IS \
             BEGIN UPDATE customers SET name = name WHERE id = cid; END touch;",
        );
        touch.kind = "procedure".into();
        touch.member_of = Some("order_ops".into());
        let mut count_all = routine(
            "count_all",
            Some("plsql"),
            "FUNCTION count_all RETURN NUMBER IS n NUMBER; \
             BEGIN SELECT count(*) INTO n FROM orders; RETURN n; END count_all;",
        );
        count_all.member_of = Some("order_ops".into());
        let doc = doc_with_routine(vec![pkg, touch, count_all], "");
        let (g, _) = build(&doc);
        // 멤버 정점 + 패키지→멤버 contains.
        assert!(g
            .vertex(&VertexId::from_raw("public.order_ops.touch"))
            .is_some());
        assert!(g.edges().iter().any(|e| e.kind == EdgeKind::Contains
            && e.from.as_str() == "public.order_ops"
            && e.to.as_str() == "public.order_ops.touch"));
        // 멤버 몸체 간선은 멤버에 귀속 — 패키지 정점에는 붙지 않는다.
        assert!(g.edges().iter().any(|e| e.kind == EdgeKind::Writes
            && e.from.as_str() == "public.order_ops.touch"
            && e.to.as_str() == "public.customers"));
        assert!(g.edges().iter().any(|e| e.kind == EdgeKind::Reads
            && e.from.as_str() == "public.order_ops.count_all"
            && e.to.as_str() == "public.orders"));
        assert!(!g
            .edges()
            .iter()
            .any(|e| e.from.as_str() == "public.order_ops" && e.kind != EdgeKind::Contains));
    }

    #[test]
    fn pkg_member_꼴_호출은_멤버_정점으로_해석된다() {
        // CALL order_ops.touch()의 qualifier는 스키마가 아니라 패키지다.
        let mut pkg = routine(
            "order_ops",
            Some("plsql"),
            "PACKAGE order_ops IS END order_ops;",
        );
        pkg.kind = "package".into();
        let mut touch = routine(
            "touch",
            Some("plsql"),
            "PROCEDURE touch IS BEGIN NULL; END touch;",
        );
        touch.kind = "procedure".into();
        touch.member_of = Some("order_ops".into());
        let caller = routine("nightly", Some("plsql"), "BEGIN order_ops.touch(1); END");
        let doc = doc_with_routine(vec![pkg, touch, caller], "");
        let (g, _) = build(&doc);
        assert!(g.edges().iter().any(|e| e.kind == EdgeKind::Calls
            && e.from.as_str() == "public.nightly"
            && e.to.as_str() == "public.order_ops.touch"));
    }

    #[test]
    fn inferred_간선은_fk_없는_이름_규칙만_추정한다() {
        // shipments.product_id는 선언 FK가 없어 추정되고, order_items.product_id는
        // 선언 FK가 있어 추정 대상이 아니다. customer+customers 양쪽이 있으면
        // 모호라 추측하지 않는다.
        let mut order_items = table(
            "order_items",
            vec![col("id", 1), col("product_id", 2), col("qty", 3)],
        );
        order_items.constraints.push(ConstraintDoc {
            name: "oi_fk".into(),
            kind: "fk".into(),
            columns: vec!["product_id".into()],
            referenced: None,
        });
        let doc = CatalogDocument {
            context: None,
            dependencies: Vec::new(),
            version: 1,
            dialect: "postgres".into(),
            reader: "test".into(),
            limitations: vec![],
            schemas: vec![SchemaDoc {
                name: "public".into(),
                routines: vec![],
                objects: vec![
                    table("customers", vec![col("id", 1), col("name", 2)]),
                    table("products", vec![col("id", 1)]),
                    table("shipments", vec![col("id", 1), col("product_id", 2)]),
                    order_items,
                    // customer와 customers가 공존해 customer_id 후보가 모호.
                    table("customer", vec![col("id", 1)]),
                    table("returns", vec![col("id", 1), col("customer_id", 2)]),
                ],
            }],
        };
        let mut g = schemagraph_source::graph::document_to_graph(&doc);
        let (made, notes) = enrich_inferred(&mut g, &doc);
        assert_eq!(made, 1);
        assert!(g.edges().iter().any(|e| e.kind == EdgeKind::Inferred
            && e.from.as_str() == "public.shipments"
            && e.to.as_str() == "public.products"));
        // 선언 FK가 있는 product_id는 추정하지 않는다.
        assert!(!g
            .edges()
            .iter()
            .any(|e| e.kind == EdgeKind::Inferred && e.from.as_str() == "public.order_items"));
        // 모호한 customer_id(returns)는 추측하지 않고 수만 센다.
        assert!(!g
            .edges()
            .iter()
            .any(|e| e.kind == EdgeKind::Inferred && e.from.as_str() == "public.returns"));
        assert!(notes.iter().any(|n| n.contains("모호")), "notes: {notes:?}");
        // inferred는 의존성 질의 근거가 아니다.
        assert!(!EdgeKind::Inferred.is_dependency());
    }

    #[test]
    fn plpgsql의_select_into_변수는_테이블로_오인되지_않는다() {
        // SELECT .. INTO n은 변수 귀속 — 벗기지 않으면 n이 테이블처럼
        // 보여 "카탈로그에 없음" 노이즈가 된다.
        let doc = doc_with_routine(
            vec![routine(
                "order_count",
                Some("plpgsql"),
                "DECLARE n int; BEGIN SELECT count(*) INTO n FROM orders; \
                 RETURN n; END",
            )],
            "",
        );
        let (g, notes) = build(&doc);
        assert!(notes.is_empty(), "notes: {notes:?}");
        assert!(g.edges().iter().any(|e| e.kind == EdgeKind::Reads
            && e.from.as_str() == "public.order_count"
            && e.to.as_str() == "public.orders"));
    }

    #[test]
    fn trigger의_execute_function이_calls_간선이_된다() {
        let doc = doc_with_routine(
            vec![routine("trg_orders_touch_fn", Some("plpgsql"), "BEGIN END")],
            "CREATE TRIGGER trg_touch AFTER INSERT ON orders EXECUTE FUNCTION trg_orders_touch_fn()",
        );
        let (g, _) = build(&doc);
        assert!(g.edges().iter().any(|e| e.kind == EdgeKind::Calls
            && e.from.as_str() == "public.orders.trg_touch"
            && e.to.as_str() == "public.trg_orders_touch_fn"));
    }

    #[test]
    fn 없는_routine_호출은_유령을_만들지_않고_notes를_남긴다() {
        let doc = doc_with_routine(
            vec![],
            "CREATE TRIGGER trg_touch AFTER INSERT ON orders EXECUTE FUNCTION ghost_fn()",
        );
        let (g, notes) = build(&doc);
        assert!(!g.edges().iter().any(|e| e.kind == EdgeKind::Calls));
        assert!(notes.iter().any(|n| n.contains("ghost_fn")));
    }

    #[test]
    fn routine_몸체의_함수_호출이_calls_간선이_된다() {
        let doc = doc_with_routine(
            vec![
                routine("order_count", Some("sql"), "SELECT count(*) FROM orders"),
                routine(
                    "daily_report",
                    Some("sql"),
                    "SELECT order_count() FROM orders LIMIT 1",
                ),
            ],
            "",
        );
        let (g, _) = build(&doc);
        assert!(g.edges().iter().any(|e| e.kind == EdgeKind::Calls
            && e.from.as_str() == "public.daily_report"
            && e.to.as_str() == "public.order_count"));
    }

    #[test]
    fn 내장_함수는_calls도_notes도_만들지_않는다() {
        let doc = doc_with_routine(
            vec![routine(
                "order_count",
                Some("sql"),
                "SELECT count(*), coalesce(max(total), 0) FROM orders",
            )],
            "",
        );
        let (g, notes) = build(&doc);
        assert!(!g.edges().iter().any(|e| e.kind == EdgeKind::Calls));
        assert!(
            !notes
                .iter()
                .any(|n| n.contains("count") || n.contains("coalesce")),
            "notes: {notes:?}"
        );
    }

    #[test]
    fn call_문장의_프로시저가_calls_간선이_된다() {
        let mut proc = routine(
            "touch_customer",
            Some("sql"),
            "UPDATE customers SET name = name WHERE id = 1",
        );
        proc.kind = "procedure".into();
        let doc = doc_with_routine(
            vec![
                proc,
                routine("nightly", Some("sql"), "CALL touch_customer(1)"),
            ],
            "",
        );
        let (g, _) = build(&doc);
        assert!(g.edges().iter().any(|e| e.kind == EdgeKind::Calls
            && e.from.as_str() == "public.nightly"
            && e.to.as_str() == "public.touch_customer"));
    }

    #[test]
    fn 오버로드가_모호하면_간선을_생략하고_notes를_남긴다() {
        // 양쪽 다 시그니처가 있으면 이름만으로는 못 고른다 — 정점 id가
        // `name(sig)` 꼴이라 정확 일치가 없고 이름 매치만 2개다.
        let mut int_ver = routine("touch_customer", Some("plpgsql"), "BEGIN END");
        int_ver.signature = Some("integer".into());
        let mut text_ver = routine("touch_customer", Some("plpgsql"), "BEGIN END");
        text_ver.signature = Some("text".into());
        let doc = doc_with_routine(
            vec![int_ver, text_ver],
            "CREATE TRIGGER trg_touch AFTER INSERT ON orders EXECUTE FUNCTION touch_customer()",
        );
        let (g, notes) = build(&doc);
        assert!(!g.edges().iter().any(|e| e.kind == EdgeKind::Calls));
        assert!(notes.iter().any(|n| n.contains("오버로드")));
        // 간선 대신 진단 하나가 두 후보를 싣는다 — note 문구로 같은 진단을 또 만들지 않는다.
        let trigger = g
            .analysis()
            .get(&VertexId::from_raw("public.orders.trg_touch"))
            .expect("trigger analysis");
        let ambiguous: Vec<_> = trigger
            .diagnostics
            .iter()
            .filter(|d| d.message.contains("오버로드"))
            .collect();
        assert_eq!(ambiguous.len(), 1, "{:?}", trigger.diagnostics);
        assert_eq!(ambiguous[0].code, "SG_BODY_PARTIAL");
        assert_eq!(
            ambiguous[0]
                .candidates
                .iter()
                .map(VertexId::as_str)
                .collect::<Vec<_>>(),
            [
                "public.touch_customer(integer)",
                "public.touch_customer(text)"
            ]
        );
    }

    #[test]
    fn 해석된_호출의_진단에는_후보를_싣지_않는다() {
        let mut int_ver = routine("touch_customer", Some("plpgsql"), "BEGIN END");
        int_ver.signature = Some("integer".into());
        let doc = doc_with_routine(
            vec![
                int_ver,
                routine("nightly", Some("sql"), "SELECT touch_customer(1)"),
            ],
            "",
        );
        let (g, _) = build(&doc);
        assert!(g.edges().iter().any(|e| e.kind == EdgeKind::Calls
            && e.from.as_str() == "public.nightly"
            && e.to.as_str() == "public.touch_customer(integer)"));
        assert!(g
            .analysis()
            .values()
            .flat_map(|analysis| &analysis.diagnostics)
            .all(|diagnostic| diagnostic.candidates.is_empty()));
    }

    #[test]
    fn db2_sql_routine_전문에서_정적_dml을_추출한다() {
        let mut routine = routine(
            "touch_customer",
            Some("sql"),
            "CREATE PROCEDURE touch_customer(IN customer INTEGER) \
             LANGUAGE SQL MODIFIES SQL DATA \
             UPDATE customers SET name = 'touched' WHERE id = customer",
        );
        routine.kind = "procedure".into();
        let mut doc = doc_with_routine(vec![routine], "");
        doc.dialect = "db2".into();
        let (graph, notes) = build(&doc);
        assert!(
            graph.edges().iter().any(|edge| {
                edge.kind == EdgeKind::Writes
                    && edge.from.as_str() == "public.touch_customer"
                    && edge.to.as_str() == "public.customers"
            }),
            "edges: {:?}, notes: {:?}",
            graph.edges(),
            notes
        );
        assert!(notes.is_empty(), "notes: {notes:?}");
    }

    #[test]
    fn db2_begin_atomic과_주석속_가짜_sql을_구분한다() {
        let mut routine = routine(
            "touch_customer",
            Some("sql"),
            "CREATE PROCEDURE touch_customer() LANGUAGE SQL \
             BEGIN ATOMIC \
               -- UPDATE ghost_table SET value = 1;\n\
               UPDATE customers SET name = 'observed'; \
             END",
        );
        routine.kind = "procedure".into();
        let mut doc = doc_with_routine(vec![routine], "");
        doc.dialect = "db2".into();
        let (graph, notes) = build(&doc);
        assert!(
            graph.edges().iter().any(|edge| {
                edge.kind == EdgeKind::Writes
                    && edge.from.as_str() == "public.touch_customer"
                    && edge.to.as_str() == "public.customers"
            }),
            "edges: {:?}, notes: {:?}",
            graph.edges(),
            notes
        );
        assert!(!graph
            .vertices()
            .any(|vertex| vertex.id.as_str().contains("ghost_table")));
        assert!(notes.is_empty(), "notes: {notes:?}");
    }

    #[test]
    fn informix_spl_routine_header와_body를_분리한다() {
        let mut routine = routine(
            "touch_customer",
            Some("spl"),
            "CREATE PROCEDURE touch_customer(customer INTEGER) \
             RETURNING INTEGER; \
             DEFINE marker INTEGER; \
             UPDATE customers SET name = 'observed' WHERE id = customer; \
             RETURN 1; \
             END PROCEDURE",
        );
        routine.kind = "procedure".into();
        let mut doc = doc_with_routine(vec![routine], "");
        doc.dialect = "informix".into();
        let (graph, notes) = build(&doc);
        assert!(graph.edges().iter().any(|edge| {
            edge.kind == EdgeKind::Writes
                && edge.from.as_str() == "public.touch_customer"
                && edge.to.as_str() == "public.customers"
        }));
        assert!(notes.is_empty(), "notes: {notes:?}");
    }

    #[test]
    fn informix_trigger_wrapper의_static_dml이_writes가_된다() {
        let mut doc =
            doc_with_trigger("(UPDATE customers SET name = 'observed' WHERE id = NEW.customer_id)");
        doc.dialect = "informix".into();
        let (graph, notes) = build(&doc);
        assert!(graph.edges().iter().any(|edge| {
            edge.kind == EdgeKind::Writes
                && edge.from.as_str() == "main.orders.trg_touch"
                && edge.to.as_str() == "main.customers"
        }));
        assert!(notes.is_empty(), "notes: {notes:?}");
    }

    #[test]
    fn db2_unrecognized_external_body는_추측하지_않고_미추출로_센다() {
        let routine = routine(
            "external_marker",
            Some("c"),
            "CREATE FUNCTION external_marker() LANGUAGE C EXTERNAL NAME 'x UPDATE ghost_table'",
        );
        let mut doc = doc_with_routine(vec![routine], "");
        doc.dialect = "db2".into();
        let (graph, notes) = build(&doc);
        assert!(!graph
            .edges()
            .iter()
            .any(|edge| edge.from.as_str() == "public.external_marker"));
        assert!(notes.iter().any(|note| note.contains("language c")));
    }

    #[test]
    fn db2_비sql_language는_body가_sql처럼_보여도_건너뛴다() {
        let routine = routine(
            "external_marker",
            Some("c"),
            "CREATE FUNCTION external_marker() LANGUAGE C UPDATE customers SET name = 'x'",
        );
        let mut doc = doc_with_routine(vec![routine], "");
        doc.dialect = "db2".into();
        let (graph, notes) = build(&doc);
        assert!(!graph
            .edges()
            .iter()
            .any(|edge| edge.from.as_str() == "public.external_marker"));
        assert!(notes.iter().any(|note| note.contains("language c")));
    }

    #[test]
    fn informix_반환형없는_procedure와_define뒤_dml을_추출한다() {
        let mut routine = routine(
            "touch_customer",
            Some("spl"),
            "CREATE PROCEDURE touch_customer(customer INTEGER) \
             DEFINE marker INTEGER; \
             UPDATE customers SET name = 'observed' WHERE id = customer; \
             END PROCEDURE",
        );
        routine.kind = "procedure".into();
        let mut doc = doc_with_routine(vec![routine], "");
        doc.dialect = "informix".into();
        let (graph, notes) = build(&doc);
        assert!(graph.edges().iter().any(|edge| {
            edge.kind == EdgeKind::Writes
                && edge.from.as_str() == "public.touch_customer"
                && edge.to.as_str() == "public.customers"
        }));
        assert!(notes.is_empty(), "notes: {notes:?}");
    }

    #[test]
    fn informix_full_trigger_wrapper에서_action을_추출한다() {
        let mut doc = doc_with_trigger(
            "CREATE TRIGGER orders_touch INSERT ON orders \
             REFERENCING NEW AS n FOR EACH ROW \
             (-- UPDATE ghost_table SET value = 1;\n\
              UPDATE customers SET name = 'observed' WHERE id = n.customer_id)",
        );
        doc.dialect = "informix".into();
        let (graph, notes) = build(&doc);
        assert!(graph.edges().iter().any(|edge| {
            edge.kind == EdgeKind::Writes
                && edge.from.as_str() == "main.orders.trg_touch"
                && edge.to.as_str() == "main.customers"
        }));
        assert!(notes.is_empty(), "notes: {notes:?}");
    }

    #[test]
    fn informix_jdbc_raw_trigger의_qualified_assignment와_세미콜론을_추출한다() {
        let mut doc = doc_with_trigger(
            "create trigger \"informix\".orders_touch insert on \"informix\".orders \
             referencing new as n \
             for each row \
             ( update \"informix\".customers \
               set \"informix\".customers.name = 'observed' \
               where (id = n.customer_id ) );",
        );
        doc.dialect = "informix".into();
        doc.schemas[0].name = "informix".into();
        let (graph, notes) = build(&doc);
        assert!(graph.edges().iter().any(|edge| {
            edge.kind == EdgeKind::Writes
                && edge.from.as_str() == "informix.orders.trg_touch"
                && edge.to.as_str() == "informix.customers"
        }));
        assert!(
            graph.edges().iter().any(|edge| {
                edge.kind == EdgeKind::Reads
                    && edge.from.as_str() == "informix.orders.trg_touch"
                    && edge.to.as_str() == "informix.orders.customer_id"
            }),
            "edges: {:?}, notes: {:?}",
            graph.edges(),
            notes
        );
        assert!(
            !notes.iter().any(|note| note.contains("parsing failed")),
            "notes: {notes:?}"
        );
    }

    #[test]
    fn informix_malformed_full_trigger_wrapper는_limitation을_남긴다() {
        let mut doc = doc_with_trigger(
            "create trigger \"informix\".orders_touch insert on \"informix\".orders \
             referencing new as n for each row ( update ((( broken );",
        );
        doc.dialect = "informix".into();
        doc.schemas[0].name = "informix".into();
        let (graph, notes) = build(&doc);
        assert!(!graph.edges().iter().any(|edge| {
            edge.kind == EdgeKind::Writes && edge.from.as_str() == "informix.orders.trg_touch"
        }));
        assert!(notes.iter().any(|note| note.contains("parse failed")));
    }

    #[test]
    fn informix_row_alias_rewrite_preserves_unicode_and_qualified_names() {
        let text = "한글 = n.id; 한n.id; schema.n.id; foo$n.id; 'n.id'; n.id";
        assert_eq!(
            replace_trigger_alias(text, "n"),
            "한글 = NEW.id; 한n.id; schema.n.id; foo$n.id; 'n.id'; NEW.id"
        );
    }

    #[test]
    fn db2_full_trigger_wrapper에서_mode뒤_dml을_추출한다() {
        let mut doc = doc_with_trigger(
            "CREATE TRIGGER orders_touch AFTER INSERT ON orders \
             REFERENCING NEW AS n FOR EACH ROW MODE DB2SQL \
             -- UPDATE ghost_table SET value = 1;\n\
             UPDATE customers SET name = 'observed' WHERE id = n.customer_id",
        );
        doc.dialect = "db2".into();
        let (graph, notes) = build(&doc);
        assert!(graph.edges().iter().any(|edge| {
            edge.kind == EdgeKind::Writes
                && edge.from.as_str() == "main.orders.trg_touch"
                && edge.to.as_str() == "main.customers"
        }));
        assert!(notes.is_empty(), "notes: {notes:?}");
    }

    /// 대문자 카탈로그를 소문자 PL/SQL 몸체가 참조·호출하는 Oracle 문서 —
    /// 관계마다 ci 해석, 호출마다 routine·패키지 멤버 해석을 탄다.
    fn oracle_resolution_doc(procedures: usize) -> CatalogDocument {
        let mut package = routine("PKG", Some("plsql"), "PACKAGE pkg IS END pkg;");
        package.kind = "package".into();
        let mut touch = routine(
            "TOUCH",
            Some("plsql"),
            "PROCEDURE touch IS BEGIN NULL; END touch;",
        );
        touch.kind = "procedure".into();
        touch.member_of = Some("PKG".into());
        let mut routines = vec![package, touch];
        for index in 0..procedures {
            let mut procedure = routine(
                &format!("P{index}"),
                Some("plsql"),
                &format!(
                    "BEGIN UPDATE orders SET id = 1 WHERE customer_id = 0; \
                     INSERT INTO customers(id) SELECT id FROM orders; p{}(1); pkg.touch(); END;",
                    (index + 1) % procedures
                ),
            );
            procedure.kind = "procedure".into();
            routines.push(procedure);
        }
        CatalogDocument {
            context: None,
            dependencies: Vec::new(),
            version: 1,
            dialect: "oracle".into(),
            reader: "test".into(),
            limitations: vec![],
            schemas: vec![SchemaDoc {
                name: "APP".into(),
                routines,
                objects: vec![
                    table("ORDERS", vec![col("ID", 1), col("CUSTOMER_ID", 2)]),
                    table("CUSTOMERS", vec![col("ID", 1), col("NAME", 2)]),
                ],
            }],
        }
    }

    fn has_edge(g: &Graph, from: &str, to: &str, kind: EdgeKind) -> bool {
        g.edges()
            .iter()
            .any(|e| e.from.as_str() == from && e.to.as_str() == to && e.kind == kind)
    }

    #[test]
    fn 이름_해석은_참조_수와_무관하게_정점을_몇_번만_훑는다() {
        for procedures in [4, 16] {
            let doc = oracle_resolution_doc(procedures);
            take_vertex_scans();
            let (g, notes) = build(&doc);
            let scans = take_vertex_scans();
            // ci id 색인과 ci routine 색인 — 참조·호출 수와 무관한 상수다.
            assert!(scans <= 3, "{procedures} procedures: {scans} vertex scans");
            for index in 0..procedures {
                let from = format!("APP.P{index}");
                assert!(
                    has_edge(&g, &from, "APP.ORDERS", EdgeKind::Writes),
                    "{notes:?}"
                );
                assert!(has_edge(&g, &from, "APP.CUSTOMERS", EdgeKind::Writes));
                assert!(has_edge(&g, &from, "APP.ORDERS", EdgeKind::Reads));
                let next = format!("APP.P{}", (index + 1) % procedures);
                assert!(has_edge(&g, &from, &next, EdgeKind::Calls), "{notes:?}");
                assert!(
                    has_edge(&g, &from, "APP.PKG.TOUCH", EdgeKind::Calls),
                    "{notes:?}"
                );
            }
        }
        // SQL 식 안의 호출은 Binder가 정확한 이름으로 해석한다.
        let callers: Vec<_> = (0..16)
            .map(|index| {
                routine(
                    &format!("f{index}"),
                    Some("sql"),
                    &format!("SELECT f{}(1) + f{}(2)", (index + 1) % 16, (index + 2) % 16),
                )
            })
            .collect();
        take_vertex_scans();
        let (g, notes) = build(&doc_with_routine(callers, ""));
        assert!(take_vertex_scans() <= 1);
        assert!(
            has_edge(&g, "public.f0", "public.f1", EdgeKind::Calls),
            "{notes:?}"
        );
        assert!(
            has_edge(&g, "public.f15", "public.f1", EdgeKind::Calls),
            "{notes:?}"
        );
    }

    /// 색인 이전의 선형 탐색 — 색인 해석이 같은 후보를 고르는지 대조하는 기준이다.
    fn linear_vertex_hit(g: &Graph, id: &VertexId, ci: bool) -> Option<VertexId> {
        if g.vertex(id).is_some() {
            return Some(id.clone());
        }
        if !ci {
            return None;
        }
        let mut hits = g
            .vertices()
            .map(|v| &v.id)
            .filter(|v| v.as_str().eq_ignore_ascii_case(id.as_str()));
        let first = hits.next()?;
        hits.next().is_none().then(|| first.clone())
    }

    fn linear_renamed(
        g: &Graph,
        base: VertexId,
        kind: VertexKind,
        suffix: &str,
        ci: bool,
    ) -> Option<VertexId> {
        let renamed = VertexId::from_raw(&format!("{}@{suffix}", base.as_str()));
        [base, renamed]
            .into_iter()
            .filter_map(|id| linear_vertex_hit(g, &id, ci))
            .find(|id| g.vertex(id).map(|v| v.kind == kind).unwrap_or(false))
    }

    fn linear_name_eq(a: &str, b: &str, ci: bool) -> bool {
        a == b || (ci && a.eq_ignore_ascii_case(b))
    }

    fn linear_routine(g: &Graph, schema: &str, name: &str, ci: bool) -> Vec<VertexId> {
        g.vertices()
            .filter(|v| {
                linear_name_eq(&v.schema, schema, ci)
                    && linear_name_eq(&v.name, name, ci)
                    && matches!(
                        v.kind,
                        VertexKind::Function | VertexKind::Procedure | VertexKind::Package
                    )
            })
            .map(|v| v.id.clone())
            .collect()
    }

    fn linear_pkg_member(
        g: &Graph,
        schema: &str,
        pkg: &str,
        name: &str,
        ci: bool,
    ) -> Vec<VertexId> {
        let base = VertexId::routine(schema, Some(pkg), name, None);
        if let Some(id) = linear_renamed(g, base.clone(), VertexKind::Procedure, "procedure", ci)
            .or_else(|| linear_renamed(g, base, VertexKind::Function, "function", ci))
        {
            return vec![id];
        }
        let package_id = VertexId::object(schema, pkg);
        g.vertices()
            .filter(|v| {
                linear_name_eq(&v.name, name, ci)
                    && matches!(v.kind, VertexKind::Procedure | VertexKind::Function)
                    && v.id.parent().as_ref() == Some(&package_id)
            })
            .map(|v| v.id.clone())
            .collect()
    }

    /// RoutineHit을 후보 목록과 같은 모양으로 바꿔 비교한다.
    fn hit_shape(hit: RoutineHit) -> (usize, Option<VertexId>) {
        match hit {
            RoutineHit::None => (0, None),
            RoutineHit::One(id) => (1, Some(id)),
            RoutineHit::Ambiguous(candidates) => (candidates.len(), None),
        }
    }

    fn candidate_shape(candidates: Vec<VertexId>) -> (usize, Option<VertexId>) {
        match candidates.as_slice() {
            [only] => (1, Some(only.clone())),
            many => (many.len(), None),
        }
    }

    #[test]
    fn 색인_해석은_선형_탐색과_같은_후보를_고른다() {
        let mut routines = Vec::new();
        for (name, kind, signature, member_of) in [
            ("F", "function", None, None),
            ("f", "function", None, None),
            ("G", "function", Some("integer"), None),
            ("G", "function", Some("text"), None),
            ("g", "procedure", None, None),
            ("PKG", "package", None, None),
            ("M", "procedure", None, Some("PKG")),
            ("M", "function", Some("integer"), Some("PKG")),
            ("N", "function", None, Some("PKG")),
            ("K", "function", Some("integer"), Some("PKG")),
            ("K", "function", Some("text"), Some("PKG")),
            ("pkg", "package", None, None),
            ("m", "procedure", None, Some("pkg")),
            ("ORDERS", "function", None, None),
        ] {
            let mut doc_routine = routine(name, Some("plsql"), "BEGIN NULL; END");
            doc_routine.kind = kind.into();
            doc_routine.signature = signature.map(str::to_owned);
            doc_routine.member_of = member_of.map(str::to_owned);
            routines.push(doc_routine);
        }
        let mut other = routine("F", Some("plsql"), "BEGIN NULL; END");
        other.kind = "procedure".into();
        let doc = CatalogDocument {
            context: None,
            dependencies: Vec::new(),
            version: 1,
            dialect: "oracle".into(),
            reader: "test".into(),
            limitations: vec![],
            schemas: vec![
                SchemaDoc {
                    name: "APP".into(),
                    routines,
                    objects: vec![
                        table("ORDERS", vec![col("ID", 1), col("id", 2)]),
                        table("orders", vec![col("ID", 1)]),
                        table("Orders", vec![col("ID", 1)]),
                        table("CUSTOMERS", vec![col("ID", 1)]),
                    ],
                },
                SchemaDoc {
                    name: "app".into(),
                    routines: vec![other],
                    objects: vec![table("customers", vec![col("id", 1)])],
                },
            ],
        };
        let g = schemagraph_source::graph::document_to_graph(&doc);
        let index = VertexIndex::default();
        let mut ids: Vec<VertexId> = g.vertices().map(|v| v.id.clone()).collect();
        let variants: Vec<VertexId> = ids
            .iter()
            .flat_map(|id| {
                [
                    VertexId::from_raw(&id.as_str().to_ascii_lowercase()),
                    VertexId::from_raw(&id.as_str().to_ascii_uppercase()),
                ]
            })
            .collect();
        ids.extend(variants);
        ids.push(VertexId::from_raw("APP.MISSING"));
        let schemas = ["APP", "app", "App", "OTHER"];
        let names = [
            "F", "f", "G", "g", "K", "k", "M", "m", "N", "n", "PKG", "pkg", "ORDERS", "orders", "X",
        ];
        // 비교가 후보 0개·1개·2개 이상과 ci에서만 찾는 경우를 모두 지나는지도 센다.
        let mut routine_shapes = BTreeSet::new();
        let mut member_shapes = BTreeSet::new();
        let mut folded_only_hits = 0;
        for ci in [false, true] {
            let lookup = index.lookup(ci);
            for id in &ids {
                let expected = linear_vertex_hit(&g, id, ci);
                folded_only_hits += usize::from(expected.is_some() && g.vertex(id).is_none());
                assert_eq!(vertex_hit(&g, id, lookup), expected, "{id} ci={ci}");
            }
            for schema in schemas {
                for name in names {
                    let expected = candidate_shape(linear_routine(&g, schema, name, ci));
                    routine_shapes.insert((ci, expected.0.min(2)));
                    assert_eq!(
                        hit_shape(resolve_routine(&g, schema, name, lookup)),
                        expected,
                        "{schema}.{name} ci={ci}"
                    );
                    for pkg in ["PKG", "pkg", "Pkg", "X"] {
                        let expected =
                            candidate_shape(linear_pkg_member(&g, schema, pkg, name, ci));
                        member_shapes.insert((ci, expected.0.min(2)));
                        assert_eq!(
                            hit_shape(resolve_pkg_member(&g, schema, pkg, name, lookup)),
                            expected,
                            "{schema}.{pkg}.{name} ci={ci}"
                        );
                    }
                }
            }
        }
        assert_eq!(routine_shapes.len(), 6, "{routine_shapes:?}");
        assert_eq!(member_shapes.len(), 6, "{member_shapes:?}");
        assert!(folded_only_hits > 0);
    }

    /// 같은 테이블에 쓰는 단일 행 INSERT `rows`개 뒤에, 공백만 다른 사본을 덧붙인다.
    fn repeated_insert_body(rows: usize, duplicates: &[usize]) -> String {
        let mut body: String = (0..rows)
            .map(|row| format!("INSERT INTO orders (id, customer_id) VALUES ({row}, {row});\n"))
            .collect();
        for row in duplicates {
            body.push_str(&format!(
                "INSERT  INTO orders (id, customer_id)\n VALUES ({row}, {row});\n"
            ));
        }
        body
    }

    #[test]
    fn dml_문장_중복_확인은_해시가_같은_후보와만_비교한다() {
        let body = repeated_insert_body(2_000, &[7, 1_999]);
        work::take_compared_statements();
        let parsed =
            parse_routine_body(Some(&GenericDialect {}), &body).expect("INSERT만 있는 몸체");
        // 위치만 다른 사본은 합쳐지고 처음 나온 순서가 그대로 남는다.
        assert_eq!(parsed.dml_statements.len(), 2_000);
        let leading: Vec<String> = parsed
            .dml_statements
            .iter()
            .take(2)
            .map(ToString::to_string)
            .collect();
        assert_eq!(
            leading,
            [
                "INSERT INTO orders (id, customer_id) VALUES (0, 0)",
                "INSERT INTO orders (id, customer_id) VALUES (1, 1)"
            ]
        );
        // 문장 전체를 비교하는 일은 실제 사본 두 개와 드문 해시 충돌에만 생긴다.
        let compared = work::take_compared_statements();
        assert!(compared <= 4, "statement comparisons: {compared}");
    }

    #[test]
    fn 위치만_다른_같은_dml은_컬럼_근거를_한_번만_남긴다() {
        let doc = doc_with_routine(
            vec![routine(
                "touch_customer",
                Some("sql"),
                "UPDATE customers SET name = 'x' WHERE id = 1; \
                 UPDATE  customers SET name = 'x'\n WHERE id = 1; \
                 UPDATE customers SET name = 'y' WHERE id = 2",
            )],
            "",
        );
        let (g, notes) = build(&doc);
        assert!(notes.is_empty(), "notes: {notes:?}");
        let writes = g
            .edges()
            .into_iter()
            .find(|e| {
                e.kind == EdgeKind::Writes
                    && e.from.as_str() == "public.touch_customer"
                    && e.to.as_str() == "public.customers.name"
            })
            .expect("컬럼 쓰기 간선");
        // 서로 다른 UPDATE 두 개만 근거가 된다.
        assert_eq!(writes.evidence.len(), 2, "{:?}", writes.evidence);
    }

    #[test]
    fn 긴_union_all_insert는_복제하지_않고_쓰기_대상을_모은다() {
        // Oracle·구 SQL Server의 다중 행 INSERT 관용구다. 문장을 복제하면 집합 연산
        // 사슬 길이만큼 재귀해 기본 테스트 스레드(2MiB)의 스택이 넘친다.
        let rows: Vec<String> = (0..3_000)
            .map(|row| format!("SELECT {row} FROM customers"))
            .collect();
        let body = format!("INSERT INTO orders (id) {}", rows.join(" UNION ALL "));
        let parsed = parse_routine_body(Some(&GenericDialect {}), &body).expect("INSERT SELECT");
        assert_eq!(parsed.dml_statements.len(), 1);
        assert_eq!(parsed.writes, [(None, "orders".to_owned())]);
        assert_eq!(parsed.reads, [(None, "customers".to_owned())]);
    }

    #[test]
    fn 집합_연산자가_상한을_넘는_뷰는_파싱하지_않고_센_수를_남긴다() {
        let terms = parse_limits::MAX_SET_OPERATORS + 2;
        let doc = doc_with_view(&vec!["SELECT id FROM orders"; terms].join(" UNION ALL "));
        let (g, notes) = build(&doc);
        let expected = format!(
            "statement 1 has {} set operators, over the {} analysis limit",
            terms - 1,
            parse_limits::MAX_SET_OPERATORS
        );
        assert!(
            notes.iter().any(|note| note.contains(&expected)),
            "notes: {notes:?}"
        );
        let view = VertexId::object("main", "order_totals");
        assert_eq!(
            g.analysis()[&view].state,
            schemagraph_core::AnalysisState::Unsupported
        );
    }

    #[test]
    fn 이항_연산자가_상한을_넘는_routine은_미추출로_센다() {
        // `id = n OR …` 항마다 연산자가 둘이다 — 50,001항이면 100,001개로 상한을 넘는다.
        let terms = parse_limits::MAX_INFIX_OPERATORS / 2 + 1;
        let condition: Vec<String> = (0..terms).map(|term| format!("id = {term}")).collect();
        let body = format!("SELECT id FROM orders WHERE {}", condition.join(" OR "));
        let doc = doc_with_routine(vec![routine("wide_filter", Some("sql"), &body)], "");
        let (_g, notes) = build(&doc);
        let expected = format!(
            "statement 1 has an operator chain of {} infix operators, over the {} analysis limit",
            terms * 2 - 1,
            parse_limits::MAX_INFIX_OPERATORS
        );
        assert!(
            notes
                .iter()
                .any(|note| note.contains("wide_filter") && note.contains(&expected)),
            "notes: {notes:?}"
        );
    }

    #[test]
    fn 갈래가_상한보다_많아도_사슬이_없는_case_뷰는_분석한다() {
        // 갈래마다 연산자가 하나라 문장 전체로 합하면 상한을 넘지만, 갈래는 sqlparser가
        // Vec으로 모으는 형제 원소라 트리가 깊어지지 않는다.
        let arms: String = (0..=parse_limits::MAX_INFIX_OPERATORS)
            .map(|arm| format!(" WHEN id = {arm} THEN 'v{arm}'"))
            .collect();
        let mut doc = doc_with_view(&format!("SELECT CASE{arms} END AS a FROM orders"));
        doc.schemas[0].objects[2].columns = vec![col("a", 1)];
        let (g, notes) = build(&doc);
        assert!(notes.is_empty(), "notes: {notes:?}");
        let view = VertexId::object("main", "order_totals");
        assert_eq!(
            g.analysis()[&view].state,
            schemagraph_core::AnalysisState::Complete
        );
        assert!(g.edges().iter().any(|e| e.kind == EdgeKind::Reads
            && e.from == view
            && e.to.as_str() == "main.orders"));
    }

    #[test]
    fn 세미콜론_없이_이어진_tsql_문장은_합이_상한을_넘어도_분석한다() {
        // 문장마다 연산자가 백여 개라 몸체 전체로 합하면 상한을 넘지만, sqlparser는 IF 블록
        // 안의 문장을 형제 목록으로 모으므로 사슬은 문장 하나 길이다. 사슬을 더 길게 하면
        // 디버그 빌드의 기본 테스트 스레드(2MiB)가 사슬 재귀로 넘친다.
        let increments = " + 1".repeat(100);
        let statements = parse_limits::MAX_INFIX_OPERATORS / 100 + 1;
        let block =
            format!("UPDATE orders SET customer_id = customer_id{increments} WHERE id = 1 ")
                .repeat(statements);
        let body = format!("CREATE PROCEDURE touch AS BEGIN IF 1 = 1 BEGIN {block}END; END");
        let mut doc = doc_with_routine(vec![routine("touch", Some("sql"), &body)], "");
        doc.dialect = "sqlserver".into();
        let (g, notes) = build(&doc);
        assert!(
            !notes.iter().any(|note| note.contains("analysis limit")),
            "notes: {notes:?}"
        );
        assert!(g.edges().iter().any(|e| e.kind == EdgeKind::Writes
            && e.from.as_str() == "public.touch"
            && e.to.as_str() == "public.orders"));
    }

    #[test]
    fn 상한_안의_긴_union_뷰는_재귀_없이_출처를_합친다() {
        // 왼쪽 깊은 집합 연산 사슬을 재귀로 내려가면 기본 테스트 스레드(2MiB)가 넘친다.
        let terms: Vec<&str> = (0..5_000)
            .map(|term| {
                if term % 2 == 0 {
                    "SELECT id AS a FROM orders"
                } else {
                    "SELECT customer_id FROM orders"
                }
            })
            .collect();
        let mut doc = doc_with_view(&terms.join(" UNION ALL "));
        doc.schemas[0].objects[2].columns = vec![col("a", 1)];
        let (g, notes) = build(&doc);
        assert!(notes.is_empty(), "notes: {notes:?}");
        let mut sources: Vec<String> = g
            .edges()
            .into_iter()
            .filter(|e| e.kind == EdgeKind::DerivesFrom && e.from.as_str() == "main.order_totals.a")
            .map(|e| e.to.as_str().to_owned())
            .collect();
        sources.sort();
        assert_eq!(sources, ["main.orders.customer_id", "main.orders.id"]);
    }

    /// `DO $t0$ DO $t1$ … <inner> … $t1$; $t0$;`처럼 태그를 바꿔 가며 `levels`단 중첩한다.
    fn nested_do(levels: usize, inner: &str) -> String {
        (0..levels).rev().fold(inner.to_owned(), |body, level| {
            format!("DO $t{level}$ {body} $t{level}$;")
        })
    }

    #[test]
    fn 한두_단계_do_블록의_안쪽_문장은_그대로_추출한다() {
        let doc = doc_with_routine(
            vec![
                routine(
                    "touch_nested",
                    Some("plpgsql"),
                    &nested_do(2, "UPDATE customers SET name = 'x' WHERE id = 1;"),
                ),
                routine(
                    "read_quoted",
                    Some("plpgsql"),
                    "DO 'BEGIN PERFORM 1 FROM orders; END';",
                ),
            ],
            "",
        );
        let (g, notes) = build(&doc);
        assert!(notes.is_empty(), "notes: {notes:?}");
        assert!(g.edges().iter().any(|e| e.kind == EdgeKind::Writes
            && e.from.as_str() == "public.touch_nested"
            && e.to.as_str() == "public.customers"));
        assert!(g.edges().iter().any(|e| e.kind == EdgeKind::Reads
            && e.from.as_str() == "public.read_quoted"
            && e.to.as_str() == "public.orders"));
    }

    #[test]
    fn 중첩_do_블록은_깊이_상한까지만_내려가고_나머지는_미추출로_센다() {
        // 상한이 없으면 15,000단 몸체 하나로 추출 재귀가 스택을 넘는다.
        let body = nested_do(15_000, "UPDATE customers SET name = 'x' WHERE id = 1;");
        let doc = doc_with_routine(vec![routine("deep_do", Some("plpgsql"), &body)], "");
        let (g, notes) = build(&doc);
        // 상한 너머의 안쪽 UPDATE는 추측하지 않고 미추출 한 건으로 남긴다.
        assert!(
            !g.edges()
                .iter()
                .any(|e| e.kind == EdgeKind::Writes && e.from.as_str() == "public.deep_do"),
            "nested DO beyond the limit must not be extracted"
        );
        assert!(
            notes
                .iter()
                .any(|note| note.contains("public.deep_do") && note.contains("1건 미추출")),
            "notes: {notes:?}"
        );
    }

    #[test]
    fn 참조_대입이_추적_총량을_넘으면_변수_평가를_끄고_진단을_남긴다() {
        // `bN text := a`마다 64KiB 값이 하나씩 복사된다. 총량 상한을 넘은 뒤에는 앞서
        // 알던 `target` 값으로도 동적 SQL을 접지 않는다.
        let big = "x".repeat(65_000);
        let copies: String = (0..100)
            .map(|copy| format!(" b{copy} text := a;"))
            .collect();
        let body = format!(
            "DECLARE target text := 'customers'; a text := '{big}';{copies} \
             BEGIN EXECUTE 'SELECT 1 FROM ' || target; END"
        );
        let doc = doc_with_routine(vec![routine("copy_values", Some("plpgsql"), &body)], "");
        let (g, notes) = build(&doc);
        let record = &g.analysis()[&VertexId::object("public", "copy_values")];
        assert_eq!(record.state, schemagraph_core::AnalysisState::Partial);
        let limit = record
            .diagnostics
            .iter()
            .find(|diagnostic| diagnostic.code == "SG_VARIABLE_STATE_LIMIT")
            .expect("variable tracking limit diagnostic");
        assert!(
            limit.message.contains(&format!(
                "over the {}-byte limit",
                constant_sql::MAX_TRACKED_TEXT_BYTES
            )),
            "{limit:?}"
        );
        assert!(!g.edges().iter().any(|e| e.kind == EdgeKind::Reads
            && e.from.as_str() == "public.copy_values"
            && e.to.as_str() == "public.customers"));
        assert!(
            notes
                .iter()
                .any(|note| note.contains("copy_values") && note.contains("1건 미추출")),
            "notes: {notes:?}"
        );
    }

    #[test]
    fn 추적_상한_안의_변수는_동적_sql을_그대로_접는다() {
        let big = "x".repeat(65_000);
        let copies: String = (0..8).map(|copy| format!(" b{copy} text := a;")).collect();
        let body = format!(
            "DECLARE target text := 'customers'; a text := '{big}';{copies} \
             BEGIN EXECUTE 'SELECT 1 FROM ' || target; END"
        );
        let doc = doc_with_routine(vec![routine("copy_values", Some("plpgsql"), &body)], "");
        let (g, notes) = build(&doc);
        assert!(notes.is_empty(), "notes: {notes:?}");
        assert!(g.edges().iter().any(|e| e.kind == EdgeKind::Reads
            && e.from.as_str() == "public.copy_values"
            && e.to.as_str() == "public.customers"));
    }

    /// 추적 변수 상한을 하나 넘기는 선언 목록.
    fn declarations_over_the_variable_limit() -> String {
        (0..=constant_sql::MAX_TRACKED_VARIABLES)
            .map(|index| format!(" v{index} text := 'x';"))
            .collect()
    }

    #[test]
    fn 추적_상한을_넘어도_뒤에_동적_sql이_없으면_몸체는_complete다() {
        // 상한은 변수 값을 버릴 뿐이다. 그 값으로 접을 동적 SQL이 없으면 결과가 그대로라
        // 한계로 싣지 않는다.
        let body = format!(
            "DECLARE{} BEGIN UPDATE customers SET name = 'y' WHERE id = 1; END",
            declarations_over_the_variable_limit()
        );
        let doc = doc_with_routine(vec![routine("many_variables", Some("plpgsql"), &body)], "");
        let (g, notes) = build(&doc);
        assert!(notes.is_empty(), "notes: {notes:?}");
        let record = &g.analysis()[&VertexId::object("public", "many_variables")];
        assert_eq!(
            record.state,
            schemagraph_core::AnalysisState::Complete,
            "{record:?}"
        );
        assert!(g.edges().iter().any(|e| e.kind == EdgeKind::Writes
            && e.from.as_str() == "public.many_variables"
            && e.to.as_str() == "public.customers"));
    }

    #[test]
    fn 추적_상한_뒤_분기가_평가를_끈_동적_sql에는_상한_진단을_붙이지_않는다() {
        // 분기 뒤로는 상한과 무관하게 평가가 꺼진다. 그 뒤 동적 SQL의 미추출은 상한 탓이 아니다.
        let body = format!(
            "DECLARE target text := 'customers';{} BEGIN \
             IF 1 = 1 THEN UPDATE customers SET name = 'y' WHERE id = 1; END IF; \
             EXECUTE 'SELECT 1 FROM ' || target; END",
            declarations_over_the_variable_limit()
        );
        let doc = doc_with_routine(vec![routine("branch_first", Some("plpgsql"), &body)], "");
        let (g, notes) = build(&doc);
        let record = &g.analysis()[&VertexId::object("public", "branch_first")];
        assert_eq!(record.state, schemagraph_core::AnalysisState::Partial);
        assert!(
            !record
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code == "SG_VARIABLE_STATE_LIMIT"),
            "{record:?}"
        );
        assert!(
            notes
                .iter()
                .any(|note| note.contains("branch_first") && note.contains("1건 미추출")),
            "notes: {notes:?}"
        );
    }

    /// 이 수정 전의 한 번씩 찾던 인용 경계 — 스캐너가 결과를 바꾸지 않았는지 대조한다.
    fn reference_quote_end(s: &str, i: usize) -> Option<usize> {
        let bytes = s.as_bytes();
        let prefix = bytes.get(i + 1) == Some(&b'\'') && (i == 0 || !is_ident_char(bytes[i - 1]));
        match bytes[i] {
            b'\'' => Some(skip_string(s, i)),
            b'"' | b'`' | b'[' => Some(skip_quoted_ident(s, i)),
            b'-' if bytes.get(i + 1) == Some(&b'-') => Some(skip_line_comment(s, i)),
            b'/' if bytes.get(i + 1) == Some(&b'*') => Some(skip_block_comment(s, i)),
            b'$' => {
                let tag = dollar_tag(s, i)?;
                let after = i + tag.len();
                s[after..].find(tag).map(|k| after + k + tag.len())
            }
            b'e' | b'E' if prefix => Some(skip_escape_string(s, i + 1)),
            b'q' | b'Q' if prefix => {
                let (close, start) = q_quote_delimiter(s, i)?;
                let delimiter = format!("{close}'");
                s[start..]
                    .find(&delimiter)
                    .map(|offset| start + offset + delimiter.len())
            }
            _ => None,
        }
    }

    #[test]
    fn 인용_경계_스캐너는_한_번씩_찾던_결과와_같다() {
        let mut texts: Vec<String> = [
            "SELECT $1, $2 FROM t WHERE a = $$x$$ AND b = $tag$ y $tag$",
            "SELECT * FROM V$SESSION; UPDATE A$B$C SET x = 1; SELECT A$B$C",
            "$a$ unclosed $b$ closed $b$ $c$ $a$ tail $",
            "q'[a]' q'!b!' q'한c한' q'x unclosed q'(d)' Q'{e}' q'<f>' q' space",
            "e'\\'' E'x' re'y' -- note $a$\n/* $b$ */ 'it''s' \"q\"\"x\" [br]]x] `bt`",
            "$$ $$$ $a$$a$ $_$ $1$ $a1$x$a1$ q'aq'a'",
            "SELECT '한글' FROM \"표\" WHERE q'가나다가' = x",
        ]
        .map(str::to_owned)
        .to_vec();
        // 까다로운 문자만 섞은 결정적 의사 난수 문자열도 대조한다.
        let alphabet: Vec<char> = "$q'aQ_1 \n-*/\"[]e`한".chars().collect();
        let mut seed = 0x2545_f491_u64;
        for _ in 0..60 {
            texts.push(
                (0..120)
                    .map(|_| {
                        seed = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
                        alphabet[(seed >> 33) as usize % alphabet.len()]
                    })
                    .collect(),
            );
        }
        let mut indexed = (0, 0);
        for text in &texts {
            let forward = QuoteScanner::new(text);
            let backward = QuoteScanner::new(text);
            for i in 0..text.len() {
                assert_eq!(
                    forward.end(i),
                    reference_quote_end(text, i),
                    "{text:?} at {i}"
                );
            }
            for i in (0..text.len()).rev() {
                assert_eq!(
                    backward.end(i),
                    reference_quote_end(text, i),
                    "{text:?} at {i}"
                );
            }
            indexed.0 += usize::from(forward.dollar_tags.get().is_some());
            indexed.1 += usize::from(forward.q_closers.get().is_some());
        }
        // 실패한 검색 뒤 위치 색인을 쓰는 경로를 dollar·q-quote 모두 지났는지 센다.
        assert!(indexed.0 > 10 && indexed.1 > 10, "{indexed:?}");
    }

    #[test]
    fn 닫히지_않은_인용_표식이_많아도_스캔마다_끝까지_한_번만_찾는다() {
        let tags: Vec<String> = (0..2_000).map(|tag| format!("$t{tag}$")).collect();
        let dollar = format!("SELECT 1 FROM orders; {}", tags.join(" "));
        let quotes = format!("SELECT 1 FROM orders; {}", vec!["q'a"; 2_000].join(" "));
        for body in [&dollar, &quotes] {
            work::take_searched_bytes();
            assert_eq!(find_all_keywords(body, "SELECT"), [0]);
            assert_eq!(split_top_level(body).len(), 2);
            let searched = work::take_searched_bytes();
            // 스캔 두 번이 각각 처음 실패한 검색에서만 끝까지 훑는다.
            assert!(
                searched <= 2 * body.len(),
                "searched {searched} bytes of {}",
                body.len()
            );
        }
    }

    #[test]
    fn 절차형_블록의_안쪽_문장은_참조로_훑던_순서로_꺼낸다() {
        let cases: [(&dyn Dialect, &str); 2] = [
            (
                &MsSqlDialect {},
                "IF @a = 1 BEGIN UPDATE orders SET id = 1; DELETE FROM customers; END \
                 ELSE BEGIN INSERT INTO orders (id) VALUES (2); END; \
                 WHILE @a > 0 BEGIN UPDATE customers SET id = 3; END",
            ),
            (
                &GenericDialect {},
                "CASE WHEN 1 = 1 THEN UPDATE orders SET id = 1; \
                 ELSE DELETE FROM customers; END CASE",
            ),
        ];
        let mut blocks = 0;
        for (dialect, sql) in cases {
            let statements = sqlparser::parser::Parser::parse_sql(dialect, sql).expect("블록 문장");
            for statement in statements {
                let by_reference: Vec<Statement> =
                    nested_statements(&statement).into_iter().cloned().collect();
                blocks += usize::from(!by_reference.is_empty());
                assert_eq!(into_nested_statements(statement), by_reference);
            }
        }
        // IF·WHILE·CASE 블록을 모두 지났는지 센다.
        assert_eq!(blocks, 3);
    }
}
