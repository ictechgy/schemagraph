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
            fill_alias(&twj.relation, &mut aliases);
            for join in &twj.joins {
                fill_alias(&join.relation, &mut aliases);
                if let Some(on) = join_on(&join.join_operator) {
                    collect_expr_columns(on, &aliases, parsed);
                }
            }
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

/// 테이블 인자에서 별칭 → (스키마?, 테이블)을 채운다.
fn fill_alias(factor: &TableFactor, aliases: &mut BTreeMap<String, (Option<String>, String)>) {
    if let TableFactor::Table { name, alias, .. } = factor {
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
}

/// `CREATE TRIGGER ... BEGIN <문장들> END`를 파싱한다. sqlparser는
/// CREATE TRIGGER 자체를 못 파는 방언이 많아 껍질(BEGIN..END)은 직접
/// 벗기고 안쪽 문장만 파서에 넘긴다. 껍질이 없는 몸체(reader가 내부
/// 문장만 저장한 경우)는 통째로 파싱한다.
fn parse_trigger_body(dialect: Option<&dyn Dialect>, body: &str) -> Result<ParsedTrigger, String> {
    let default = GenericDialect {};
    let dialect = dialect.unwrap_or(&default);
    let inner = extract_trigger_inner(body).unwrap_or(body);
    let statements =
        sqlparser::parser::Parser::parse_sql(dialect, inner.trim()).map_err(|e| e.to_string())?;
    if statements.is_empty() {
        return Err("본문에서 문장을 찾지 못함".to_owned());
    }
    let mut parsed = ParsedTrigger {
        writes: Vec::new(),
        reads: Vec::new(),
        fired_columns: Vec::new(),
    };
    for stmt in &statements {
        collect_trigger_stmt(stmt, &mut parsed);
    }
    Ok(parsed)
}

/// BEGIN..END 안쪽을 꺼낸다. 단어 경계가 아닌 BEGIN/END는 무시한다 —
/// 문자열 리터럴 안의 BEGIN까지 정확히 거르지는 못하지만, 못 벗기면
/// 파싱 실패로 limitations에 남는다(조용히 틀리지 않는다).
fn extract_trigger_inner(body: &str) -> Option<&str> {
    // 대문자 사본을 만들지 않는다 — to_uppercase는 비ASCII에서 바이트
    // 오프셋을 바꿔 슬라이스를 틀어뜨린다. 원문에서 case-insensitive로 찾는다.
    let begin = find_all_keywords(body, "BEGIN").first().copied()? + "BEGIN".len();
    let end = find_all_keywords(body, "END").last().copied()?;
    (end > begin).then_some(body[begin..end].trim())
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
        if let Expr::CompoundIdentifier(parts) = e {
            if parts.len() == 2 {
                let q = parts[0].value.to_ascii_lowercase();
                if q == "new" || q == "old" {
                    parsed.fired_columns.push(parts[1].value.clone());
                }
            }
        }
        ControlFlow::<()>::Continue(())
    });
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

/// trigger 파싱 결과를 그래프 간선으로 반영한다 — writes/reads/fired-columns.
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
    for (kind, targets) in [
        (EdgeKind::Writes, &parsed.writes),
        (EdgeKind::Reads, &parsed.reads),
    ] {
        for (ref_schema, table) in targets {
            let target_schema = ref_schema.clone().unwrap_or_else(|| schema.to_owned());
            let target = VertexId::object(&target_schema, table);
            if g.vertex(&target).is_none() {
                notes.push(format!(
                    "trigger {trigger_id}가 참조하는 {target_schema}.{table}이 카탈로그에 없음"
                ));
                continue;
            }
            g.add_edge(Edge {
                from: trigger_id.clone(),
                to: target.clone(),
                kind,
                evidence: vec![Evidence {
                    layer: EvidenceLayer::BodyParse,
                    detail: format!("trigger {trigger_id} {:?} {target}", kind),
                }],
            });
        }
    }
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
}
