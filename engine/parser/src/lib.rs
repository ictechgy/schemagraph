//! schemagraph-parser — SQL 몸체를 파싱해 의존 간선을 만든다.
//!
//! reader는 원문만 옮기고, 몸체의 의미는 여기서 해석한다 — 그래프 의미론의
//! 권위는 엔진 한 곳에 있다(DESIGN.md "파싱은 단일 소스"). 파싱 실패는
//! 숨기지 않고 limitation으로 실측해 돌려준다.
//!
//! 정직한 범위(P1): 테이블 참조는 visitor가 서브쿼리·CTE까지 전부 읽지만,
//! 컬럼 참조는 최상위 select의 별칭 맵으로만 해석한다 — 서브쿼리 스코프를
//! 섞으면 오귀속되므로, 그 존재는 limitation으로 보고한다.

use std::collections::{BTreeMap, BTreeSet};
use std::ops::ControlFlow;

use schemagraph_core::{Edge, EdgeKind, Evidence, EvidenceLayer, Graph, VertexId, VertexKind};
use schemagraph_source::document::{CatalogDocument, ObjectDoc};
use sqlparser::ast::{
    visit_expressions, visit_relations, Expr, JoinConstraint, JoinOperator, ObjectName, Query,
    SelectItem, SetExpr, Statement, TableFactor, TableObject,
};
use sqlparser::dialect::{
    Dialect, GenericDialect, MsSqlDialect, MySqlDialect, PostgreSqlDialect, SQLiteDialect,
};

mod constant_sql;

/// document의 몸체들을 파싱해 그래프에 간선을 보강한다.
///
/// 반환값은 (간선을 만든 객체 수, 실패·주의로 limitation에 실을 메시지들).
/// 호출자가 limitations를 그래프에 싣는다 — 여기서 직접 싣지 않는 이유는
/// reader가 이미 싣은 한계와 순서를 섞지 않기 위해서다.
pub fn enrich_from_document(g: &mut Graph, doc: &CatalogDocument) -> (usize, Vec<String>) {
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
    let mut enriched = 0usize;
    let mut notes: Vec<String> = Vec::new();

    for schema in &doc.schemas {
        for obj in &schema.objects {
            if let Some(body) = obj.body.as_ref().filter(|b| !b.trim().is_empty()) {
                let owner = VertexId::object(&schema.name, &obj.name);
                match obj.kind.as_str() {
                    "view" => match parse_view(dialect.as_deref(), body) {
                        Ok(parsed) => {
                            apply_view(g, &schema.name, &owner, &parsed, &mut notes, ci);
                            enriched += 1;
                        }
                        Err(msg) => notes.push(format!(
                            "view {}.{} 몸체 파싱 실패: {msg}",
                            schema.name, obj.name
                        )),
                    },
                    _ => {}
                }
            }
            for trg in &obj.triggers {
                let Some(body) = &trg.body else { continue };
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
                    ci,
                ) else {
                    notes.push(format!(
                        "trigger {}.{}.{}: 정점을 못 찾음(이름 충돌?) — 몸체 간선 생략",
                        schema.name, obj.name, trg.name
                    ));
                    continue;
                };
                let parsed_body = match doc.dialect.as_str() {
                    "db2" => parse_db2_trigger_body(dialect.as_deref(), body),
                    "informix" => parse_informix_trigger_body(dialect.as_deref(), body),
                    _ => parse_trigger_body(dialect.as_deref(), body),
                };
                match parsed_body {
                    Ok(parsed) => {
                        apply_trigger(
                            g,
                            &schema.name,
                            &obj.name,
                            &trigger_id,
                            &parsed,
                            &mut notes,
                            ci,
                        );
                        enriched += 1;
                    }
                    Err(msg) => notes.push(format!("trigger {trigger_id} 몸체 파싱 실패: {msg}")),
                }
            }
        }
        for routine in &schema.routines {
            let Some(body) = routine.body.as_ref().filter(|b| !b.trim().is_empty()) else {
                continue;
            };
            // routine 정점 id는 graph.rs와 같은 규칙 — 시그니처가 있으면 괄호로
            // 붙고, 같은 이름의 테이블/프로시저가 있으면 `id_name@kind`로 분리된다.
            let id_name = match &routine.signature {
                Some(sig) if !sig.is_empty() => format!("{}({})", routine.name, sig),
                _ => routine.name.clone(),
            };
            let (kind, suffix) = match routine.kind.as_str() {
                "procedure" => (VertexKind::Procedure, "procedure"),
                "package" => (VertexKind::Package, "package"),
                _ => (VertexKind::Function, "function"),
            };
            // 패키지 멤버는 schema.pkg.member 정점 — schema 직속이 아니다.
            let owner = match &routine.member_of {
                Some(pkg) => resolve_member(g, &schema.name, pkg, &id_name, kind, suffix, ci),
                None => resolve_object(g, &schema.name, &id_name, kind, suffix, ci),
            };
            let Some(owner) = owner else {
                notes.push(format!(
                    "routine {}.{id_name}: 정점을 못 찾음 — 몸체 간선 생략",
                    schema.name
                ));
                continue;
            };
            if matches!(doc.dialect.as_str(), "db2" | "informix")
                && catalog_routine_language_allowed(&doc.dialect, routine.language.as_deref())
            {
                let (parsed, unextracted) = parse_catalog_routine_body(&doc.dialect, body);
                apply_routine(g, &schema.name, &owner, &parsed, &mut notes, ci);
                if unextracted > 0 {
                    notes.push(format!(
                        "routine {owner}: body statement(s) {unextracted} could not be extracted"
                    ));
                }
                enriched += 1;
                continue;
            }
            if matches!(doc.dialect.as_str(), "db2" | "informix") {
                let language = routine.language.as_deref().unwrap_or("unknown");
                notes.push(format!(
                    "routine {owner}: language {language} is unsupported for {0} catalog parsing — body edges omitted",
                    doc.dialect
                ));
                continue;
            }
            match routine.language.as_deref() {
                // SQL 언어 함수는 몸체가 그대로 SQL이라 파싱 가능 — 나머지
                // 언어는 몸체 문법이 SQL이 아니라 미지원으로 보고한다.
                Some("sql") | None => match parse_routine_body(dialect.as_deref(), body) {
                    Ok(parsed) => {
                        apply_routine(g, &schema.name, &owner, &parsed, &mut notes, ci);
                        enriched += 1;
                    }
                    Err(msg) => {
                        // 도매 파싱이 실패한 몸체 — T-SQL의 DECLARE/IF/TRY
                        // 같은 절차형 구문이 섞이면 sqlparser가 문장 목록째
                        // 거부한다. 문장 추출로 부분 복구해 파싱된 문장의
                        // 간선만 취하고, 미추출은 limitation으로 센다.
                        let (parsed, unextracted) = parse_procedural_body(
                            dialect.as_deref(),
                            body,
                            constant_sql::dialect(&doc.dialect),
                            format_shadowed,
                        );
                        if parsed.has_edges() || unextracted > 0 {
                            if parsed.has_edges() {
                                apply_routine(g, &schema.name, &owner, &parsed, &mut notes, ci);
                            }
                            // 미추출 0건은 완전 복구 — 한계가 아니므로 조용히 둔다.
                            if unextracted > 0 {
                                notes.push(format!(
                                    "routine {owner}: 몸체 문장 {unextracted}건 미추출\
                                     (절차형 구문) — 몸체 간선 불완전 ({msg})"
                                ));
                            }
                            enriched += 1;
                        } else {
                            notes.push(format!("routine {owner} 몸체 파싱 실패: {msg}"));
                        }
                    }
                },
                // plpgsql·plsql은 문장 단위 추출기로 SQL 문장만 꺼내 파싱한다.
                // 꺼내지 못한 문장(동적 SQL 등)은 수를 세어 limitation으로 남긴다.
                Some("plpgsql" | "plsql" | "pl/sql") => {
                    let (parsed, unextracted) = parse_procedural_body(
                        dialect.as_deref(),
                        body,
                        constant_sql::dialect(&doc.dialect),
                        format_shadowed,
                    );
                    apply_routine(g, &schema.name, &owner, &parsed, &mut notes, ci);
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
                Some(lang) => notes.push(format!(
                    "routine {owner}: 언어 {lang}의 몸체 파싱 미지원 — 몸체 간선 없음"
                )),
            }
        }
    }
    (enriched, notes)
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
        "postgres" => Box::new(PostgreSqlDialect {}),
        "mysql" => Box::new(MySqlDialect {}),
        // MsSqlDialect는 [bracket] 식별자·TOP 같은 T-SQL 표면을 이해한다 —
        // Generic으로 파싱하면 문장째 실패하던 것이 풀린다.
        "sqlserver" | "mssql" => Box::new(MsSqlDialect {}),
        _ => Box::new(GenericDialect {}),
    })
}

/// 파싱 결과.
struct ParsedView {
    /// (스키마?, 테이블) — visitor가 서브쿼리·CTE까지 다 읽은 관계 대상.
    tables: Vec<(Option<String>, String)>,
    /// (스키마?, 테이블, 컬럼) — 최상위 별칭 맵으로 해석된 것만.
    columns: Vec<(Option<String>, String, String)>,
    /// database qualifier가 있어 catalog identity로 귀속할 수 없는 관계.
    ignored_relations: BTreeSet<String>,
    /// CTE나 서브쿼리가 있어 컬럼 해석이 불완전한 경우.
    has_nested_scope: bool,
}

/// `CREATE VIEW v AS SELECT ...` 또는 벌어진 SELECT를 파싱한다.
fn parse_view(dialect: Option<&dyn Dialect>, body: &str) -> Result<ParsedView, String> {
    let default = GenericDialect {};
    let dialect = dialect.unwrap_or(&default);
    let statements =
        sqlparser::parser::Parser::parse_sql(dialect, body).map_err(|e| e.to_string())?;
    let mut parsed = ParsedView {
        tables: Vec::new(),
        columns: Vec::new(),
        ignored_relations: BTreeSet::new(),
        has_nested_scope: false,
    };
    for stmt in &statements {
        let query = match stmt {
            Statement::CreateView { query, .. } => Some(&**query),
            Statement::Query(q) => Some(&**q),
            _ => None,
        };
        if let Some(q) = query {
            collect_query(q, &mut parsed);
        }
    }
    if parsed.tables.is_empty() && parsed.ignored_relations.is_empty() {
        return Err("몸체에서 테이블 참조를 찾지 못함".to_owned());
    }
    Ok(parsed)
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

/// 테이블 관계 전부(visitor) + 최상위 select의 컬럼 참조를 모은다.
fn collect_query(query: &Query, parsed: &mut ParsedView) {
    if query.with.is_some() {
        parsed.has_nested_scope = true;
    }
    let _ = visit_relations(query, |name| {
        if let Some((schema, table)) = relation_name_parts(name) {
            parsed.tables.push((schema, table));
        } else {
            parsed.ignored_relations.insert(relation_name_key(name));
        }
        ControlFlow::<()>::Continue(())
    });

    // 컬럼 참조는 최상위 select 스코프만 — 서브쿼리 별칭과 섞이면 오귀속된다.
    if let SetExpr::Select(select) = &*query.body {
        let mut aliases: BTreeMap<String, (Option<String>, String)> = BTreeMap::new();
        for twj in &select.from {
            fill_twj(twj, &mut aliases, parsed);
        }
        for item in &select.projection {
            let expr = match item {
                SelectItem::UnnamedExpr(e) => Some(e),
                SelectItem::ExprWithAlias { expr, .. } => Some(expr),
                _ => None,
            };
            if let Some(e) = expr {
                collect_expr_columns(e, &aliases, parsed);
            }
        }
        if let Some(sel) = &select.selection {
            collect_expr_columns(sel, &aliases, parsed);
        }
        // FROM 안에 서브쿼리(Derived)가 있으면 컬럼 해석이 불완전하다고 표시.
        for twj in &select.from {
            if is_derived(&twj.relation) || twj.joins.iter().any(|j| is_derived(&j.relation)) {
                parsed.has_nested_scope = true;
            }
        }
    }
}

fn is_derived(factor: &TableFactor) -> bool {
    matches!(factor, TableFactor::Derived { .. })
}

/// TableWithJoins 하나의 별칭을 순서대로 채우고, 조인 ON 식의 컬럼 참조를
/// 모은다. ON은 자기 왼쪽의 테이블만 참조할 수 있으므로 문서 순서대로
/// 채우면서 모으는 게 맞다.
fn fill_twj(
    twj: &sqlparser::ast::TableWithJoins,
    aliases: &mut BTreeMap<String, (Option<String>, String)>,
    parsed: &mut ParsedView,
) {
    fill_alias(&twj.relation, aliases, parsed);
    for join in &twj.joins {
        fill_alias(&join.relation, aliases, parsed);
        if let Some(on) = join_on(&join.join_operator) {
            collect_expr_columns(on, aliases, parsed);
        }
    }
}

/// 테이블 인자에서 별칭 → (스키마?, 테이블)을 채운다.
///
/// NestedJoin((a JOIN b ON ...))은 괄호일 뿐 스코프를 새로 열지 않는다 —
/// PG의 pg_get_viewdef가 FROM 절 전체를 괄호로 감싸 출력하므로 재귀가
/// 없으면 그쪽 view의 컬럼 해석이 전부 비게 된다.
fn fill_alias(
    factor: &TableFactor,
    aliases: &mut BTreeMap<String, (Option<String>, String)>,
    parsed: &mut ParsedView,
) {
    match factor {
        TableFactor::Table { name, alias, .. } => {
            let Some((schema, table)) = relation_name_parts(name) else {
                return;
            };
            let key = alias
                .as_ref()
                .map(|a| a.name.value.clone())
                .unwrap_or_else(|| table.clone());
            aliases.insert(key, (schema, table));
        }
        TableFactor::NestedJoin {
            table_with_joins, ..
        } => fill_twj(table_with_joins, aliases, parsed),
        _ => {}
    }
}

/// join operator에서 ON 조건식만 꺼낸다.
fn join_on(op: &JoinOperator) -> Option<&Expr> {
    let constraint = match op {
        JoinOperator::Inner(c)
        | JoinOperator::LeftOuter(c)
        | JoinOperator::RightOuter(c)
        | JoinOperator::FullOuter(c)
        | JoinOperator::Semi(c)
        | JoinOperator::LeftSemi(c)
        | JoinOperator::RightSemi(c)
        | JoinOperator::Anti(c)
        | JoinOperator::LeftAnti(c)
        | JoinOperator::RightAnti(c) => c,
        _ => return None,
    };
    match constraint {
        JoinConstraint::On(e) => Some(e),
        _ => None,
    }
}

/// 식 안의 `별칭.컬럼` 식별자를 수집해 별칭 맵으로 해석한다. 별칭에 없는
/// 두 파트 식별자와 한 파트 식별자는 어느 테이블의 것인지 모른다 —
/// 추측하지 않고 버린다.
fn collect_expr_columns(
    expr: &Expr,
    aliases: &BTreeMap<String, (Option<String>, String)>,
    parsed: &mut ParsedView,
) {
    let _ = visit_expressions(expr, |e| {
        match e {
            Expr::CompoundIdentifier(parts) => {
                if parts.len() == 2 {
                    let qual = &parts[0].value;
                    let col = &parts[1].value;
                    if let Some((schema, table)) = aliases.get(qual) {
                        parsed
                            .columns
                            .push((schema.clone(), table.clone(), col.clone()));
                    }
                }
            }
            // 식 안의 서브쿼리는 별칭 스코프가 달라 컬럼 해석이 불완전하다.
            Expr::Subquery(_)
            | Expr::InSubquery { .. }
            | Expr::Exists { .. }
            | Expr::AnyOp { .. }
            | Expr::AllOp { .. } => parsed.has_nested_scope = true,
            _ => {}
        }
        ControlFlow::<()>::Continue(())
    });
}

/// 파싱 결과를 그래프 간선으로 반영한다. 대상 정점이 그래프에 있을 때만
/// 간선을 만든다 — 없는 객체를 가리키는 간선은 유령 정점을 만들고, 추측으로
/// 만들지 않는 것이 원칙이다(못 본 대상은 limitations로 남긴다).
fn apply_view(
    g: &mut Graph,
    schema: &str,
    owner: &VertexId,
    parsed: &ParsedView,
    notes: &mut Vec<String>,
    ci: bool,
) {
    if parsed.has_nested_scope {
        notes.push(format!(
            "view {owner}: 서브쿼리/CTE의 컬럼 참조는 미해석 — reads 간선이 부분적일 수 있음"
        ));
    }
    note_ignored_relations(owner, &parsed.ignored_relations, notes);

    let mut made = std::collections::BTreeSet::new();
    for (ref_schema, table) in &parsed.tables {
        let target_schema = ref_schema.clone().unwrap_or_else(|| schema.to_owned());
        let Some(target) = vertex_hit(g, &VertexId::object(&target_schema, table), ci) else {
            notes.push(format!(
                "view {owner}가 참조하는 {target_schema}.{table}이 카탈로그에 없음 \
                 (다른 스키마이거나 미수집)"
            ));
            continue;
        };
        if made.insert(target.clone()) {
            g.add_edge(Edge {
                from: owner.clone(),
                to: target.clone(),
                kind: EdgeKind::Reads,
                evidence: vec![Evidence {
                    layer: EvidenceLayer::BodyParse,
                    detail: format!("view {owner} reads {target}"),
                }],
            });
        }
    }
    for (ref_schema, table, column) in &parsed.columns {
        let target_schema = ref_schema.clone().unwrap_or_else(|| schema.to_owned());
        let Some(target) = vertex_hit(g, &VertexId::member(&target_schema, table, column), ci)
        else {
            // 컬럼 정점이 없으면(함수 반환값·계산 컬럼 등) 조용히 넘긴다 —
            // object 레벨 간선이 이미 있고, 없는 컬럼을 추측으로 만들지 않는다.
            continue;
        };
        g.add_edge(Edge {
            from: owner.clone(),
            to: target,
            kind: EdgeKind::Reads,
            evidence: vec![Evidence {
                layer: EvidenceLayer::BodyParse,
                detail: format!("view {owner} reads {target_schema}.{table}.{column}"),
            }],
        });
    }
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
}

impl ParsedTrigger {
    /// 간선이 될 수확이 하나라도 있나 — 부분 복구 폴백이 "못 읽은 몸체"와
    /// "읽을 게 없던 몸체"를 구분하는 기준이다.
    fn has_edges(&self) -> bool {
        !(self.writes.is_empty()
            && self.reads.is_empty()
            && self.fired_columns.is_empty()
            && self.calls.is_empty()
            && self.ignored_relations.is_empty())
    }
}

/// `CREATE TRIGGER ... BEGIN <문장들> END`를 파싱한다. sqlparser는
/// CREATE TRIGGER 자체를 못 파는 방언이 많아 껍질(BEGIN..END)은 직접
/// 벗기고 안쪽 문장만 파서에 넘긴다. 껍질이 없는 몸체(reader가 내부
/// 문장만 저장한 경우)는 통째로 파싱한다.
fn parse_trigger_body(dialect: Option<&dyn Dialect>, body: &str) -> Result<ParsedTrigger, String> {
    let default = GenericDialect {};
    let dialect = dialect.unwrap_or(&default);
    let calls = extract_execute_targets(body);
    let inner = extract_trigger_inner(body).unwrap_or(body);
    let statements = sqlparser::parser::Parser::parse_sql(dialect, inner.trim());
    let mut parsed = ParsedTrigger {
        writes: Vec::new(),
        reads: Vec::new(),
        fired_columns: Vec::new(),
        calls,
        ignored_relations: BTreeSet::new(),
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
            for stmt in &stmts {
                collect_trigger_stmt(stmt, &mut parsed);
            }
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
    parse_trigger_body(dialect, &inner)
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

fn strip_db2_routine_body<'a>(tail: &'a str) -> Option<&'a str> {
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

fn strip_informix_routine_body<'a>(tail: &'a str) -> Option<&'a str> {
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
    let bytes = text.as_bytes();
    let mut open = None;
    let mut scan = 0;
    while scan < bytes.len() {
        if let Some(end) = quoted_or_comment_end(text, scan) {
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
        if let Some(end) = quoted_or_comment_end(text, index) {
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
    let bytes = text.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if let Some(end) = quoted_or_comment_end(text, index) {
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
    parse_trigger_body(dialect, inner)
}

fn parse_db2_trigger_body(
    dialect: Option<&dyn Dialect>,
    body: &str,
) -> Result<ParsedTrigger, String> {
    let inner = strip_catalog_trigger_header("db2", body).unwrap_or(body);
    parse_trigger_body(dialect, inner)
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
    if !trimmed.starts_with('(') || !trimmed.ends_with(')') {
        return None;
    }
    let bytes = trimmed.as_bytes();
    let mut depth = 0usize;
    let mut index = 0;
    while index < bytes.len() {
        if let Some(end) = quoted_or_comment_end(trimmed, index) {
            index = end;
            continue;
        }
        match bytes[index] {
            b'(' => depth += 1,
            b')' => {
                depth = depth.checked_sub(1)?;
                if depth == 0 && index + 1 != bytes.len() {
                    return None;
                }
            }
            _ => {}
        }
        index += 1;
    }
    (depth == 0).then(|| trimmed[1..trimmed.len() - 1].trim())
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
    let bytes = s.as_bytes();
    let mut found = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if let Some(end) = quoted_or_comment_end(s, i) {
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
        }
    }

    fn invalidate(&mut self) {
        self.evaluation_enabled = false;
        self.variables.clear();
    }

    fn observe_begin(&mut self) {
        if self.outer_block_seen {
            self.invalidate();
        } else {
            self.outer_block_seen = true;
        }
    }

    fn invalidate_control_flow(&mut self) {
        self.invalidate();
    }

    fn assign(&mut self, name: &str, expression: &str) {
        let variables = self.variables.clone();
        self.assign_from(name, expression, &variables);
    }

    fn assign_from(&mut self, name: &str, expression: &str, variables: &BTreeMap<String, String>) {
        let key = normalize_variable_name(name);
        if key.is_empty() {
            return;
        }
        if self.evaluation_enabled {
            if let Some(capacity) = self.capacities.get(&key).copied() {
                if let Some(value) = constant_sql::evaluate_with_format(
                    expression,
                    self.dialect,
                    variables,
                    !self.format_shadowed,
                ) {
                    let unicode_safe = self.dialect != constant_sql::TextDialect::MsSql
                        || self.unicode_capable.contains(&key)
                        || value.is_ascii();
                    if unicode_safe && constant_sql::fits_capacity(&value, capacity) {
                        self.variables.insert(key, value);
                        return;
                    }
                }
            }
        }
        self.variables.remove(&key);
    }

    fn declare(&mut self, name: &str, type_text: &str, expression: Option<&str>) {
        let key = normalize_variable_name(name);
        self.variables.remove(&key);
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
    }

    fn invalidate_variable(&mut self, name: &str) {
        self.variables.remove(&normalize_variable_name(name));
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
    let inner = extract_as_body(body).unwrap_or(std::borrow::Cow::Borrowed(body));
    let mut state = ExtractionState::new(text_dialect, format_shadowed);
    let (stmts, mut unextracted) = extract_procedural_statements(&inner, &mut state);
    let default = GenericDialect {};
    let dialect = dialect.unwrap_or(&default);
    let mut parsed = ParsedTrigger {
        writes: Vec::new(),
        reads: Vec::new(),
        fired_columns: Vec::new(),
        calls: Vec::new(),
        ignored_relations: BTreeSet::new(),
    };
    for stmt in &stmts {
        match sqlparser::parser::Parser::parse_sql(dialect, stmt) {
            Ok(list) => {
                for s in &list {
                    collect_trigger_stmt(s, &mut parsed);
                }
            }
            // 추출기가 SQL이 아닌 조각을 문장으로 오인한 경우 — 하나의
            // 실패가 전체를 쓰러뜨리지 않게 수만 센다.
            Err(_) => unextracted += 1,
        }
    }
    (parsed, unextracted)
}

/// 절차형 몸체(plpgsql·plsql)에서 SQL 문장을 문장 단위로 추출한다.
///
/// 전체 문법을 파싱하는 대신 `;`로 나눈 조각에서 제어 구조 키워드를 벗겨
/// SQL 문장만 복원한다 — 조건식은 서브쿼리를 담을 수 있어 `(`가 있을 때
/// `SELECT`로 싸고, `FOR .. IN <질의>`·`PERFORM`·`EXECUTE '리터럴'`·
/// `:=` 대입·PL/SQL bare 호출은 그에 맞는 SQL로 재작성한다. 꺼내지 못한
/// 문장(동적 SQL 식, 인식 못 한 구문)은 수를 돌려 limitation으로 남긴다 —
/// 추측으로 채우지 않는다.
fn extract_procedural_statements(body: &str, state: &mut ExtractionState) -> (Vec<String>, usize) {
    let mut stmts: Vec<String> = Vec::new();
    let mut unextracted = 0usize;
    // DECLARE/IS 섹션 안의 조각은 선언문 — 의존 대상이 아니라 미추출로 세지
    // 않되, DEFAULT/:= 안의 서브쿼리는 추출한다.
    let mut in_declare = false;
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
                    if is_statement_head(&word) {
                        in_declare = false;
                        record_variable_writes(rest, &word, state);
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
    let b = s.as_bytes();
    let n = b.len();
    let mut parts = Vec::new();
    let mut start = 0usize;
    let mut i = 0usize;
    while i < n {
        if let Some(end) = quoted_or_comment_end(s, i) {
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
    let b = s.as_bytes();
    let n = b.len();
    let mut depth = 0i32;
    let mut i = 0usize;
    while i < n {
        if let Some(end) = quoted_or_comment_end(s, i) {
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

/// 같은 인용 규칙을 문장 분할과 키워드 탐색에 적용해 경계 해석이 어긋나지 않게 한다.
fn quoted_or_comment_end(s: &str, i: usize) -> Option<usize> {
    let bytes = s.as_bytes();
    match bytes[i] {
        b'\'' => Some(skip_string(s, i)),
        b'"' | b'`' | b'[' => Some(skip_quoted_ident(s, i)),
        b'-' if bytes.get(i + 1) == Some(&b'-') => Some(skip_line_comment(s, i)),
        b'/' if bytes.get(i + 1) == Some(&b'*') => Some(skip_block_comment(s, i)),
        b'$' => dollar_quote_end(s, i),
        b'e' | b'E'
            if bytes.get(i + 1) == Some(&b'\'') && (i == 0 || !is_ident_char(bytes[i - 1])) =>
        {
            Some(skip_escape_string(s, i + 1))
        }
        b'q' | b'Q'
            if bytes.get(i + 1) == Some(&b'\'') && (i == 0 || !is_ident_char(bytes[i - 1])) =>
        {
            q_quote_end(s, i)
        }
        _ => None,
    }
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
    let b = s.as_bytes();
    let mut j = i + 1;
    if j < b.len() && (b[j].is_ascii_alphabetic() || b[j] == b'_') {
        while j < b.len() && (b[j].is_ascii_alphanumeric() || b[j] == b'_') {
            j += 1;
        }
    }
    if j >= b.len() || b[j] != b'$' {
        return None;
    }
    let tag = &s[i..=j];
    s[j + 1..].find(tag).map(|k| j + 1 + k + tag.len())
}

/// Oracle `q'구분자...구분자'`의 끝 다음 위치 — 괄호 구분자는 대칭,
/// 나머지는 같은 문자가 닫는다.
fn q_quote_end(s: &str, i: usize) -> Option<usize> {
    let open = s.get(i + 2..)?.chars().next()?;
    let close = match open {
        '[' => ']',
        '(' => ')',
        '{' => '}',
        '<' => '>',
        c if !c.is_whitespace() && c != '\'' => c,
        _ => return None,
    };
    let start = i + 2 + open.len_utf8();
    let delimiter = format!("{close}'");
    s[start..]
        .find(&delimiter)
        .map(|offset| start + offset + delimiter.len())
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
    state: &ExtractionState,
) {
    let tail = skip_ws_comments(tail);
    let Some((expression, remaining)) = split_dynamic_expression(tail) else {
        *unextracted += 1;
        return;
    };
    let Some(sql) = state.evaluate(expression) else {
        *unextracted += 1;
        return;
    };
    if !remaining.is_empty() && !(allow_bindings && dynamic_binding_head(remaining).is_some()) {
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

/// `DO $$..$$`·`DO '..'` 중첩 익명 블록 — 몸체를 꺼내 재귀 추출한다.
fn strip_do<'a>(
    rest: &'a str,
    kwlen: usize,
    stmts: &mut Vec<String>,
    unextracted: &mut usize,
    state: &mut ExtractionState,
) -> &'a str {
    let tail = rest[kwlen..].trim_start();
    let inner = if tail.starts_with('$') {
        undollar_quote(tail).map(|c| c.into_owned())
    } else if tail.starts_with('\'') {
        unquote_sql_string(tail).map(|c| c.into_owned())
    } else {
        None
    };
    match inner {
        Some(b) => {
            let (mut s, u) = extract_procedural_statements(&b, state);
            stmts.append(&mut s);
            *unextracted += u;
        }
        None => *unextracted += 1,
    }
    ""
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
    let original = state.variables.clone();
    for (name, expression) in assignments {
        let key = normalize_variable_name(&name);
        if constant_sql::references_variables(&expression, state.dialect, &targets) {
            state.invalidate_variable(&name);
        } else {
            state.assign_from(&name, &expression, &original);
            // A value may have become unknown due to type/capacity constraints;
            // leave it unknown instead of restoring an older assignment.
            if !state.variables.contains_key(&key) {
                state.invalidate_variable(&name);
            }
        }
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
    let bytes = s.as_bytes();
    let mut parts = Vec::new();
    let mut start = 0;
    let mut depth = 0i32;
    let mut position = 0;
    while position < bytes.len() {
        if let Some(end) = quoted_or_comment_end(s, position) {
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
    let bytes = rest.as_bytes();
    let mut position = 0;
    let mut open = None;
    let mut depth = 0i32;
    while position < bytes.len() {
        if let Some(end) = quoted_or_comment_end(rest, position) {
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

/// 문장 하나의 쓰기/읽기/발사-컬럼 참조를 모은다.
fn collect_trigger_stmt(stmt: &Statement, parsed: &mut ParsedTrigger) {
    // write_targets는 절차형 블록까지 재귀로 본다 — 블록 안의 UPDATE 대상이
    // 부모 문장의 visit_relations에서 reads로 오인되지 않게 미리 다 알아야 한다.
    let write_targets = write_targets(stmt);
    for t in &write_targets {
        if !parsed.writes.contains(t) {
            parsed.writes.push(t.clone());
        }
    }
    let _ = visit_relations(stmt, |name| {
        let Some(t) = relation_name_parts(name) else {
            parsed.ignored_relations.insert(relation_name_key(name));
            return ControlFlow::<()>::Continue(());
        };
        // 재귀 방문으로 중첩 문장이 여러 번 오므로 중복을 거른다.
        if !t.1.is_empty() && !write_targets.contains(&t) && !parsed.reads.contains(&t) {
            parsed.reads.push(t);
        }
        ControlFlow::<()>::Continue(())
    });
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
            // 몸체 안의 함수 호출 — 내장 함수는 routine이 아니라 걸러야
            // count() 같은 호출이 "카탈로그에 없음" 노이즈를 만들지 않는다.
            Expr::Function(f) => {
                let t = object_name_parts(&f.name);
                if !t.1.is_empty() && !is_builtin_function(&t.1) && !parsed.calls.contains(&t) {
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
    // 절차형 블록(IF/WHILE/CASE) 안의 문장 — 도매 파싱이 성공해도 DML·CALL이
    // 블록 안에 중첩돼 있어 재귀로 내려가지 않으면 writes가 reads로 오분류되고
    // CALL이 조용히 빠진다. 관계·식 수확은 visitor가 이미 트리 전체를 본다.
    for s in nested_statements(stmt) {
        collect_trigger_stmt(s, parsed);
    }
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
    let mut targets = write_targets_shallow(stmt);
    for s in nested_statements(stmt) {
        targets.extend(write_targets(s));
    }
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
        Statement::Update { table, .. } => table_factor_name(&table.relation).into_iter().collect(),
        Statement::Insert(insert) => match &insert.table {
            TableObject::TableName(name) => relation_name_parts(name).into_iter().collect(),
            _ => vec![],
        },
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

/// TableFactor::Table이면 (스키마?, 테이블)을 돌린다.
fn table_factor_name(factor: &TableFactor) -> Option<(Option<String>, String)> {
    if let TableFactor::Table { name, .. } = factor {
        relation_name_parts(name)
    } else {
        None
    }
}

/// routine 이름 해석 결과 — 오버로드는 호출 정확도상 구분 못 하므로
/// 모호함을 숨기지 않고 별도로 보고한다.
enum RoutineHit {
    One(VertexId),
    None,
    Ambiguous(usize),
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
    ci: bool,
) -> Option<VertexId> {
    resolve_renamed(g, VertexId::member(schema, obj, name), kind, suffix, ci)
}

/// 객체 레벨 정점(routine 등)을 같은 규칙으로 찾는다.
fn resolve_object(
    g: &Graph,
    schema: &str,
    name: &str,
    kind: VertexKind,
    suffix: &str,
    ci: bool,
) -> Option<VertexId> {
    resolve_renamed(g, VertexId::object(schema, name), kind, suffix, ci)
}

/// base id가 다른 kind에게 점유됐으면 `base@suffix`를 시도한다 —
/// graph.rs의 resolve_collision과 같은 명명 규칙을 공유해야 한다.
fn resolve_renamed(
    g: &Graph,
    base: VertexId,
    kind: VertexKind,
    suffix: &str,
    ci: bool,
) -> Option<VertexId> {
    let renamed = VertexId::from_raw(&format!("{}@{suffix}", base.as_str()));
    [base, renamed]
        .into_iter()
        .filter_map(|id| vertex_hit(g, &id, ci))
        .find(|id| g.vertex(id).map(|v| v.kind == kind).unwrap_or(false))
}

/// 정점 조회 — 정확 일치가 없을 때 ci가 켜져 있으면 대소문자 무시 단일 후보를
/// 찾는다. Oracle의 미인용 식별자는 대문자로 접혀 몸체의 `orders`와 카탈로그의
/// `ORDERS`가 같은 대상이지만, 대소문자만 다른 두 객체가 있으면 어느 쪽인지
/// 추측할 수 없어 None을 돌려 유령 간선을 막는다.
fn vertex_hit(g: &Graph, id: &VertexId, ci: bool) -> Option<VertexId> {
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
    if hits.next().is_none() {
        Some(first.clone())
    } else {
        None
    }
}

/// 스키마 안에서 routine 정점을 이름으로 찾는다. 정확한 id(name 그대로)가
/// 먼저이고, 없으면 routine kind 정점의 표시 이름과 비교한다 — 호출자는
/// 시그니처를 모르므로 이름이 유일할 때만 받아들인다(resolve()와 같은 철학).
fn resolve_routine(g: &Graph, schema: &str, name: &str, ci: bool) -> RoutineHit {
    let exact = VertexId::object(schema, name);
    // 정확한 id가 routine kind일 때만 바로 받는다 — 같은 이름의 테이블이
    // base id를 차지한 채 routine이 `name@function`으로 분리됐을 수 있어
    // kind를 확인하지 않으면 calls 간선이 테이블을 가리킨다.
    if g.vertex(&exact)
        .map(|v| {
            matches!(
                v.kind,
                schemagraph_core::VertexKind::Function
                    | schemagraph_core::VertexKind::Procedure
                    | schemagraph_core::VertexKind::Package
            )
        })
        .unwrap_or(false)
    {
        return RoutineHit::One(exact);
    }
    let name_eq = |a: &str, b: &str| a == b || (ci && a.eq_ignore_ascii_case(b));
    let hits: Vec<VertexId> = g
        .vertices()
        .filter(|v| {
            name_eq(&v.schema, schema)
                && name_eq(&v.name, name)
                && matches!(
                    v.kind,
                    schemagraph_core::VertexKind::Function
                        | schemagraph_core::VertexKind::Procedure
                        | schemagraph_core::VertexKind::Package
                )
        })
        .map(|v| v.id.clone())
        .collect();
    match hits.len() {
        0 => RoutineHit::None,
        1 => RoutineHit::One(hits.into_iter().next().unwrap()),
        n => RoutineHit::Ambiguous(n),
    }
}

/// trigger 파싱 결과를 그래프 간선으로 반영한다 — DML + calls + fired-columns.
fn apply_trigger(
    g: &mut Graph,
    schema: &str,
    owner_table: &str,
    trigger_id: &VertexId,
    parsed: &ParsedTrigger,
    notes: &mut Vec<String>,
    ci: bool,
) {
    if g.vertex(trigger_id).is_none() {
        notes.push(format!(
            "trigger {trigger_id}의 정점이 카탈로그에 없음 — 간선 생략"
        ));
        return;
    }
    note_ignored_relations(trigger_id, &parsed.ignored_relations, notes);
    apply_dml_edges(g, schema, trigger_id, parsed, notes, ci);
    apply_call_edges(g, schema, trigger_id, &parsed.calls, notes, ci);
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
            ci,
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
}

/// routine 파싱 결과를 그래프 간선으로 반영한다 — trigger와 같은 DML/calls
/// 규칙이지만 발사 테이블(NEW/OLD)은 없다.
fn apply_routine(
    g: &mut Graph,
    schema: &str,
    owner: &VertexId,
    parsed: &ParsedTrigger,
    notes: &mut Vec<String>,
    ci: bool,
) {
    if g.vertex(owner).is_none() {
        notes.push(format!(
            "routine {owner}의 정점이 카탈로그에 없음 — 간선 생략"
        ));
        return;
    }
    note_ignored_relations(owner, &parsed.ignored_relations, notes);
    apply_dml_edges(g, schema, owner, parsed, notes, ci);
    apply_call_edges(g, schema, owner, &parsed.calls, notes, ci);
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
    schema: &str,
    from: &VertexId,
    parsed: &ParsedTrigger,
    notes: &mut Vec<String>,
    ci: bool,
) {
    for (kind, targets) in [
        (EdgeKind::Writes, &parsed.writes),
        (EdgeKind::Reads, &parsed.reads),
    ] {
        for (ref_schema, table) in targets {
            let target_schema = ref_schema.clone().unwrap_or_else(|| schema.to_owned());
            let Some(target) = vertex_hit(g, &VertexId::object(&target_schema, table), ci) else {
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

/// EXECUTE FUNCTION/PROCEDURE 호출 대상을 간선으로 — routine 정점 id는
/// 시그니처를 포함할 수 있어 이름으로 해석한다.
fn apply_call_edges(
    g: &mut Graph,
    schema: &str,
    from: &VertexId,
    calls: &[(Option<String>, String)],
    notes: &mut Vec<String>,
    ci: bool,
) {
    for (ref_schema, name) in calls {
        let target_schema = ref_schema.clone().unwrap_or_else(|| schema.to_owned());
        let hit = match resolve_routine(g, &target_schema, name, ci) {
            RoutineHit::None => {
                // `pkg.member()` 꼴 — qualifier가 스키마가 아니라 패키지면
                // 멤버 정점 schema.pkg.member를 찾는다.
                ref_schema
                    .as_ref()
                    .map(|pkg| resolve_pkg_member(g, schema, pkg, name, ci))
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
            RoutineHit::Ambiguous(n) => notes.push(format!(
                "{from}이(가) 부르는 {target_schema}.{name}에 {n}개 오버로드가 있어 간선 생략"
            )),
        }
    }
}

/// `schema.pkg.member` 정점을 찾는다 — 멤버 kind는 procedure/function 둘
/// 다 될 수 있어 둘을 시도하고, 시그니처가 붙은 id(`pkg.m(int)`)는
/// `schema.pkg.` 접두 안의 이름 스캔으로 찾는다. 오버로드가 여럿이면
/// Ambiguous — resolve_routine과 같이 추측하지 않는다.
fn resolve_pkg_member(g: &Graph, schema: &str, pkg: &str, name: &str, ci: bool) -> RoutineHit {
    let base = VertexId::member(schema, pkg, name);
    if let Some(id) = resolve_renamed(g, base.clone(), VertexKind::Procedure, "procedure", ci)
        .or_else(|| resolve_renamed(g, base, VertexKind::Function, "function", ci))
    {
        return RoutineHit::One(id);
    }
    let prefix = format!("{schema}.{pkg}.");
    let name_eq = |a: &str, b: &str| a == b || (ci && a.eq_ignore_ascii_case(b));
    let hits: Vec<VertexId> = g
        .vertices()
        .filter(|v| {
            name_eq(&v.name, name)
                && matches!(v.kind, VertexKind::Procedure | VertexKind::Function)
                && v.id.as_str().len() >= prefix.len()
                && v.id.as_str()[..prefix.len()].eq_ignore_ascii_case(&prefix)
        })
        .map(|v| v.id.clone())
        .collect();
    match hits.len() {
        0 => RoutineHit::None,
        1 => RoutineHit::One(hits.into_iter().next().unwrap()),
        n => RoutineHit::Ambiguous(n),
    }
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
        assert!(notes[0].contains("파싱 실패"));
    }

    #[test]
    fn 서브쿼리가_있으면_불완전_표시를_남긴다() {
        let doc = doc_with_view(
            "CREATE VIEW order_totals AS SELECT o.id FROM orders o WHERE o.customer_id IN (SELECT id FROM customers)",
        );
        let (_g, notes) = build(&doc);
        assert!(notes.iter().any(|n| n.contains("미해석")));
    }

    /// Oracle은 카탈로그가 대문자(미인용 식별자 접힘)인데 몸체는 소문자로
    /// 쓰이는 게 보통 — ci 해석이 소문자 참조를 대문자 정점으로 연결해야 한다.
    #[test]
    fn oracle의_소문자_몸체가_대문자_정점으로_해석된다() {
        let doc = CatalogDocument {
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
    fn oracle의_대소문자_충돌은_추측하지_않는다() {
        let doc = CatalogDocument {
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
        assert!(!g
            .edges()
            .iter()
            .any(|e| e.kind == EdgeKind::Reads && e.from.as_str() == "SGFIX.V"));
        assert!(notes.iter().any(|n| n.contains("Orders")));
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
        assert!(notes.iter().any(|n| n.contains("파싱 실패")));
    }

    /// routine이 달린 document — PG 방언의 EXECUTE FUNCTION trigger도 실험한다.
    fn doc_with_routine(routines: Vec<RoutineDoc>, trigger_body: &str) -> CatalogDocument {
        CatalogDocument {
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
                .any(|n| n.contains("sync_orders") && n.contains("파싱 실패")),
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
                .any(|n| n.contains("noop") && n.contains("파싱 실패")),
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
                BTreeSet::from([(EdgeKind::Writes, "public.orders")])
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
}
