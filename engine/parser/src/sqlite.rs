//! SQLite trigger의 특수 RAISE 구문을 일반 routine 호출과 구별한다.

use sqlparser::ast::{Expr, Function, FunctionArg, FunctionArgExpr, FunctionArguments};

/// RAISE의 첫 인자는 컬럼이 아니라 충돌 처리 키워드다. 인용·한정·잘못된
/// 인자 수·일반 함수 전용 수식자는 숨기지 않고 기존 미해석 진단에 남긴다.
pub(super) fn raise_action(function: &Function) -> Option<&Expr> {
    let [name] = function.name.0.as_slice() else {
        return None;
    };
    let name = name.as_ident()?;
    if name.quote_style.is_some()
        || !name.value.eq_ignore_ascii_case("raise")
        || function.uses_odbc_syntax
        || !matches!(&function.parameters, FunctionArguments::None)
        || function.filter.is_some()
        || function.null_treatment.is_some()
        || function.over.is_some()
        || !function.within_group.is_empty()
    {
        return None;
    }
    let FunctionArguments::List(arguments) = &function.args else {
        return None;
    };
    if arguments.duplicate_treatment.is_some() || !arguments.clauses.is_empty() {
        return None;
    }
    let [FunctionArg::Unnamed(FunctionArgExpr::Expr(action)), rest @ ..] =
        arguments.args.as_slice()
    else {
        return None;
    };
    let Expr::Identifier(identifier) = action else {
        return None;
    };
    if identifier.quote_style.is_some() {
        return None;
    }
    match (identifier.value.to_ascii_uppercase().as_str(), rest) {
        ("IGNORE", [])
        | ("ROLLBACK" | "ABORT" | "FAIL", [FunctionArg::Unnamed(FunctionArgExpr::Expr(_))]) => {
            Some(action)
        }
        _ => None,
    }
}
