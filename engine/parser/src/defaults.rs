//! 컬럼 DEFAULT 식의 함수 의존성을 실제 카탈로그 정점에 연결한다.

use std::ops::ControlFlow;

use schemagraph_core::{Edge, EdgeKind, Evidence, EvidenceLayer, Graph, VertexKind};
use schemagraph_source::document::CatalogDocument;
use sqlparser::ast::{visit_expressions, Expr};
use sqlparser::dialect::Dialect;

use super::{
    is_builtin_function, parse_limits, resolve_member, resolve_routine, scope, RoutineHit,
};

pub(super) fn enrich(
    graph: &mut Graph,
    document: &CatalogDocument,
    catalog: &scope::CatalogIndex<'_>,
    dialect: Option<&dyn Dialect>,
    ci: bool,
    notes: &mut Vec<String>,
) {
    for schema in &document.schemas {
        for object in &schema.objects {
            for column in &object.columns {
                let Some(default) = column.default.as_deref() else {
                    continue;
                };
                let Some(owner) = resolve_member(
                    graph,
                    &schema.name,
                    &object.name,
                    &column.name,
                    VertexKind::Column,
                    "column",
                    catalog.lookup(ci),
                ) else {
                    continue;
                };
                let Some(dialect) = dialect else {
                    notes.push(format!(
                        "column {owner} DEFAULT: unsupported dialect; dependencies unavailable"
                    ));
                    continue;
                };
                let expression = match parse_limits::parse_expression(dialect, default) {
                    Ok(expression) => expression,
                    Err(_) => {
                        // 기본값 원문과 파서 오류에는 비밀 리터럴이 있을 수 있어 싣지 않는다.
                        notes.push(format!(
                            "column {owner} DEFAULT parse failed; dependencies unavailable"
                        ));
                        continue;
                    }
                };
                let _ = visit_expressions(&expression, |expression| {
                    let Expr::Function(function) = expression else {
                        return ControlFlow::<()>::Continue(());
                    };
                    let parts = function
                        .name
                        .0
                        .iter()
                        .filter_map(|part| part.as_ident())
                        .collect::<Vec<_>>();
                    if parts.is_empty() || parts.len() > 2 || parts.len() != function.name.0.len() {
                        notes.push(format!(
                            "column {owner} DEFAULT: unsupported function qualification"
                        ));
                        return ControlFlow::Continue(());
                    }
                    let name = scope::name(&document.dialect, parts[parts.len() - 1]);
                    let target_schema = if parts.len() == 2 {
                        scope::name(&document.dialect, parts[0])
                    } else {
                        schema.name.clone()
                    };
                    // pg_catalog 내장 시퀀스 함수는 사용자 routine 카탈로그에 없다.
                    // 정규화한 식별자로만 대조해 따옴표 대소문자와 사용자 스키마를 보존한다.
                    let sequence_builtin =
                        matches!(document.dialect.as_str(), "postgres" | "postgresql")
                            && (parts.len() == 1 || target_schema == "pg_catalog")
                            && matches!(
                                name.as_str(),
                                "nextval" | "currval" | "setval" | "lastval"
                            );
                    match resolve_routine(graph, &target_schema, &name, catalog.lookup(ci)) {
                        RoutineHit::One(target) if graph.vertex(&target).is_some_and(|vertex| vertex.kind == VertexKind::Function) => {
                            graph.add_edge(Edge {
                                from: owner.clone(), to: target.clone(), kind: EdgeKind::DependsOn,
                                evidence: vec![Evidence { layer: EvidenceLayer::BodyParse,
                                    detail: format!("column {owner} DEFAULT depends on {target}") }],
                            });
                        }
                        RoutineHit::Ambiguous(candidates) => notes.push(format!("column {owner} DEFAULT: {target_schema}.{name} has {} overloads; dependency unresolved", candidates.len())),
                        RoutineHit::None if sequence_builtin => {},
                        RoutineHit::None if parts.len() == 1 && parts[0].quote_style.is_none() && is_builtin_function(&name) => {},
                        _ => notes.push(format!("column {owner} DEFAULT: function {target_schema}.{name} unavailable in catalog; dependency unresolved")),
                    }
                    ControlFlow::Continue(())
                });
            }
        }
    }
}
