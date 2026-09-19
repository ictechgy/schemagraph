//! schemagraph-parser — SQL 몸체를 파싱해 의존 간선을 만든다.
//!
//! reader는 원문만 옮기고, 몸체의 의미는 여기서 해석한다 — 그래프 의미론의
//! 권위는 엔진 한 곳에 있다(DESIGN.md "파싱은 단일 소스"). 파싱 실패는
//! 숨기지 않고 limitation으로 실측해 돌려준다.
//!
//! 정직한 범위(P1): 테이블 참조는 visitor가 서브쿼리·CTE까지 전부 읽지만,
//! 컬럼 참조는 최상위 select의 별칭 맵으로만 해석한다 — 서브쿼리 스코프를
//! 섞으면 오귀속되므로, 그 존재는 limitation으로 보고한다.

use std::collections::BTreeMap;
use std::ops::ControlFlow;

use schemagraph_core::{Edge, EdgeKind, Evidence, EvidenceLayer, Graph, VertexId};
use schemagraph_source::document::CatalogDocument;
use sqlparser::ast::{
    visit_expressions, visit_relations, Expr, JoinConstraint, JoinOperator, ObjectName, Query,
    SelectItem, SetExpr, Statement, TableFactor, TableObject,
};
use sqlparser::dialect::{Dialect, GenericDialect, MySqlDialect, PostgreSqlDialect, SQLiteDialect};

/// document의 몸체들을 파싱해 그래프에 간선을 보강한다.
///
/// 반환값은 (간선을 만든 객체 수, 실패·주의로 limitation에 실을 메시지들).
/// 호출자가 limitations를 그래프에 싣는다 — 여기서 직접 싣지 않는 이유는
/// reader가 이미 싣은 한계와 순서를 섞지 않기 위해서다.
pub fn enrich_from_document(g: &mut Graph, doc: &CatalogDocument) -> (usize, Vec<String>) {
    let dialect = dialect_for(&doc.dialect);
    let mut enriched = 0usize;
    let mut notes: Vec<String> = Vec::new();

    for schema in &doc.schemas {
        for obj in &schema.objects {
            if let Some(body) = obj.body.as_ref().filter(|b| !b.trim().is_empty()) {
                let owner = VertexId::object(&schema.name, &obj.name);
                match obj.kind.as_str() {
                    "view" => match parse_view(dialect.as_deref(), body) {
                        Ok(parsed) => {
                            apply_view(g, &schema.name, &owner, &parsed, &mut notes);
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
                let trigger_id = VertexId::member(&schema.name, &obj.name, &trg.name);
                match parse_trigger_body(dialect.as_deref(), body) {
                    Ok(parsed) => {
                        apply_trigger(g, &schema.name, &obj.name, &trigger_id, &parsed, &mut notes);
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
            // routine 정점 id는 graph.rs와 같은 규칙 — 시그니처가 있으면 괄호로 붙는다.
            let id_name = match &routine.signature {
                Some(sig) if !sig.is_empty() => format!("{}({})", routine.name, sig),
                _ => routine.name.clone(),
            };
            let owner = VertexId::object(&schema.name, &id_name);
            match routine.language.as_deref() {
                // SQL 언어 함수는 몸체가 그대로 SQL이라 파싱 가능 — 나머지
                // 언어(plpgsql 등)는 몸체 문법이 SQL이 아니라 미지원으로 보고한다.
                Some("sql") | None => match parse_routine_body(dialect.as_deref(), body) {
                    Ok(parsed) => {
                        apply_routine(g, &schema.name, &owner, &parsed, &mut notes);
                        enriched += 1;
                    }
                    Err(msg) => notes.push(format!("routine {owner} 몸체 파싱 실패: {msg}")),
                },
                Some(lang) => notes.push(format!(
                    "routine {owner}: 언어 {lang}의 몸체 파싱 미지원 — 몸체 간선 없음"
                )),
            }
        }
    }
    (enriched, notes)
}

/// doc.dialect 문자열 → sqlparser 방언. 모르는 방언은 Generic으로 돌린다 —
/// 파싱을 아예 포기하는 것보다 Generic이 낫다(못 읽으면 실패로 센다).
fn dialect_for(dialect: &str) -> Option<Box<dyn Dialect>> {
    Some(match dialect {
        "sqlite" => Box::new(SQLiteDialect {}),
        "postgres" => Box::new(PostgreSqlDialect {}),
        "mysql" => Box::new(MySqlDialect {}),
        _ => Box::new(GenericDialect {}),
    })
}

/// 파싱 결과.
struct ParsedView {
    /// (스키마?, 테이블) — visitor가 서브쿼리·CTE까지 다 읽은 관계 대상.
    tables: Vec<(Option<String>, String)>,
    /// (스키마?, 테이블, 컬럼) — 최상위 별칭 맵으로 해석된 것만.
    columns: Vec<(Option<String>, String, String)>,
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
    if parsed.tables.is_empty() {
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

/// 테이블 관계 전부(visitor) + 최상위 select의 컬럼 참조를 모은다.
fn collect_query(query: &Query, parsed: &mut ParsedView) {
    if query.with.is_some() {
        parsed.has_nested_scope = true;
    }
    let _ = visit_relations(query, |name| {
        let (schema, table) = object_name_parts(name);
        if !table.is_empty() {
            parsed.tables.push((schema, table));
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
            let (schema, table) = object_name_parts(name);
            if table.is_empty() {
                return;
            }
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
) {
    if parsed.has_nested_scope {
        notes.push(format!(
            "view {owner}: 서브쿼리/CTE의 컬럼 참조는 미해석 — reads 간선이 부분적일 수 있음"
        ));
    }

    let mut made = std::collections::BTreeSet::new();
    for (ref_schema, table) in &parsed.tables {
        let target_schema = ref_schema.clone().unwrap_or_else(|| schema.to_owned());
        let target = VertexId::object(&target_schema, table);
        if g.vertex(&target).is_none() {
            notes.push(format!(
                "view {owner}가 참조하는 {target_schema}.{table}이 카탈로그에 없음 \
                 (다른 스키마이거나 미수집)"
            ));
            continue;
        }
        if made.insert(target.clone()) {
            g.add_edge(Edge {
                from: owner.clone(),
                to: target,
                kind: EdgeKind::Reads,
                evidence: vec![Evidence {
                    layer: EvidenceLayer::BodyParse,
                    detail: format!("view {owner} reads {target_schema}.{table}"),
                }],
            });
        }
    }
    for (ref_schema, table, column) in &parsed.columns {
        let target_schema = ref_schema.clone().unwrap_or_else(|| schema.to_owned());
        let target = VertexId::member(&target_schema, table, column);
        if g.vertex(&target).is_none() {
            // 컬럼 정점이 없으면(함수 반환값·계산 컬럼 등) 조용히 넘긴다 —
            // object 레벨 간선이 이미 있고, 없는 컬럼을 추측으로 만들지 않는다.
            continue;
        }
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
    };
    match statements {
        Ok(stmts) if !stmts.is_empty() => {
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

/// routine 몸체 파싱 — CREATE FUNCTION 전문이 들어오면 AS 뒤의 몸체만
/// 꺼내고, 나머지는 trigger와 같은 문장 수집 골격으로 파싱한다.
fn parse_routine_body(dialect: Option<&dyn Dialect>, body: &str) -> Result<ParsedTrigger, String> {
    let inner = extract_as_body(body).unwrap_or(std::borrow::Cow::Borrowed(body));
    parse_trigger_body(dialect, &inner)
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

/// BEGIN..END 안쪽을 꺼낸다. 단어 경계가 아닌 BEGIN/END는 무시한다 —
/// 문자열 리터럴 안의 BEGIN까지 정확히 거르지는 못하지만, 못 벗기면
/// 파싱 실패로 limitations에 남는다(조용히 틀리지 않는다).
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

/// 원문에서 대소문자 무관·단어 경계인 키워드의 모든 위치를 돌린다.
fn find_all_keywords(s: &str, kw: &str) -> Vec<usize> {
    let bytes = s.as_bytes();
    (0..=bytes.len().saturating_sub(kw.len()))
        .filter(|&i| {
            bytes[i..i + kw.len()].eq_ignore_ascii_case(kw.as_bytes())
                && word_boundary(s, i, kw.len())
        })
        .collect()
}

/// 위치 i부터 len 바이트가 독립 단어인지 — 식별자 문자로 붙어 있으면
/// 키워드가 아니다.
fn word_boundary(s: &str, i: usize, len: usize) -> bool {
    let ident = |b: u8| b.is_ascii_alphanumeric() || b == b'_';
    let before_ok = i == 0 || !ident(s.as_bytes()[i - 1]);
    let after_ok = i + len >= s.len() || !ident(s.as_bytes()[i + len]);
    before_ok && after_ok
}

/// 문장 하나의 쓰기/읽기/발사-컬럼 참조를 모은다.
fn collect_trigger_stmt(stmt: &Statement, parsed: &mut ParsedTrigger) {
    let write_targets = write_targets(stmt);
    parsed.writes.extend(write_targets.iter().cloned());
    let _ = visit_relations(stmt, |name| {
        let t = object_name_parts(name);
        if !t.1.is_empty() && !write_targets.contains(&t) {
            parsed.reads.push(t);
        }
        ControlFlow::<()>::Continue(())
    });
    let _ = visit_expressions(stmt, |e| {
        match e {
            Expr::CompoundIdentifier(parts) => {
                if parts.len() == 2 {
                    let q = parts[0].value.to_ascii_lowercase();
                    if q == "new" || q == "old" {
                        parsed.fired_columns.push(parts[1].value.clone());
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

/// DML 문장의 쓰기 대상. SELECT-only 문장은 빈 벡터다.
fn write_targets(stmt: &Statement) -> Vec<(Option<String>, String)> {
    match stmt {
        Statement::Update { table, .. } => table_factor_name(&table.relation).into_iter().collect(),
        Statement::Insert(insert) => match &insert.table {
            TableObject::TableName(name) => {
                let t = object_name_parts(name);
                if t.1.is_empty() {
                    vec![]
                } else {
                    vec![t]
                }
            }
            _ => vec![],
        },
        Statement::Delete(delete) => {
            if !delete.tables.is_empty() {
                delete.tables.iter().map(object_name_parts).collect()
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
        let t = object_name_parts(name);
        (!t.1.is_empty()).then_some(t)
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

/// 스키마 안에서 routine 정점을 이름으로 찾는다. 정확한 id(name 그대로)가
/// 먼저이고, 없으면 routine kind 정점의 표시 이름과 비교한다 — 호출자는
/// 시그니처를 모르므로 이름이 유일할 때만 받아들인다(resolve()와 같은 철학).
fn resolve_routine(g: &Graph, schema: &str, name: &str) -> RoutineHit {
    let exact = VertexId::object(schema, name);
    if g.vertex(&exact).is_some() {
        return RoutineHit::One(exact);
    }
    let hits: Vec<VertexId> = g
        .vertices()
        .filter(|v| {
            v.schema == schema
                && v.name == name
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
) {
    if g.vertex(trigger_id).is_none() {
        notes.push(format!(
            "trigger {trigger_id}의 정점이 카탈로그에 없음 — 간선 생략"
        ));
        return;
    }
    apply_dml_edges(g, schema, trigger_id, parsed, notes);
    apply_call_edges(g, schema, trigger_id, &parsed.calls, notes);
    // NEW.x / OLD.x는 발사 테이블의 컬럼을 읽는다는 뜻이다.
    let mut made = std::collections::BTreeSet::new();
    for column in &parsed.fired_columns {
        let target = VertexId::member(schema, owner_table, column);
        if g.vertex(&target).is_none() || !made.insert(target.clone()) {
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
) {
    if g.vertex(owner).is_none() {
        notes.push(format!(
            "routine {owner}의 정점이 카탈로그에 없음 — 간선 생략"
        ));
        return;
    }
    apply_dml_edges(g, schema, owner, parsed, notes);
    apply_call_edges(g, schema, owner, &parsed.calls, notes);
}

/// 몸체 문장의 writes/reads 대상을 간선으로 만든다 — trigger와 routine이 공유.
fn apply_dml_edges(
    g: &mut Graph,
    schema: &str,
    from: &VertexId,
    parsed: &ParsedTrigger,
    notes: &mut Vec<String>,
) {
    for (kind, targets) in [
        (EdgeKind::Writes, &parsed.writes),
        (EdgeKind::Reads, &parsed.reads),
    ] {
        for (ref_schema, table) in targets {
            let target_schema = ref_schema.clone().unwrap_or_else(|| schema.to_owned());
            let target = VertexId::object(&target_schema, table);
            if g.vertex(&target).is_none() {
                notes.push(format!(
                    "{from}이(가) 참조하는 {target_schema}.{table}이 카탈로그에 없음"
                ));
                continue;
            }
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
) {
    for (ref_schema, name) in calls {
        let target_schema = ref_schema.clone().unwrap_or_else(|| schema.to_owned());
        match resolve_routine(g, &target_schema, name) {
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
    fn plpgsql_routine은_파싱대신_한계를_보고한다() {
        let doc = doc_with_routine(
            vec![routine(
                "touch_customer",
                Some("plpgsql"),
                "BEGIN UPDATE customers SET name = name WHERE id = 1; END",
            )],
            "",
        );
        let (g, notes) = build(&doc);
        assert!(notes.iter().any(|n| n.contains("plpgsql")));
        assert!(!g
            .edges()
            .iter()
            .any(|e| e.from.as_str() == "public.touch_customer"));
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
}
