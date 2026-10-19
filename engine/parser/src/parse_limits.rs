//! 트리를 만들기 전에 문장별 연산 사슬 길이를 세어, 분석 재귀가 스택을 넘지 않게 한다.
//!
//! sqlparser는 집합 연산과 이항 연산 사슬을 반복문으로 왼쪽 깊은 트리로 만든다. 그래서
//! 괄호·부질의 중첩에 거는 자체 재귀 한도(50)가 사슬에는 걸리지 않는다. 그 트리를
//! 방문·해시·비교·복제·해제하는 코드는 모두 재귀라 사슬 길이만큼 스택을 쓰고, 스택
//! 넘침은 catch_unwind로도 잡히지 않아 그래프도 limitation도 없이 프로세스가 죽는다.
//! 토큰만 세면 트리를 만들기 전에 거를 수 있고, 거른 몸체는 파싱 실패와 같은 경로로
//! 실제로 센 수를 담은 limitation이 된다.

use sqlparser::ast::Statement;
use sqlparser::dialect::Dialect;
use sqlparser::keywords::Keyword;
use sqlparser::parser::{Parser, ParserError, ParserOptions};
use sqlparser::tokenizer::{Token, TokenWithSpan, Tokenizer};

/// 한 문장에 둘 수 있는 집합 연산자(UNION·INTERSECT·EXCEPT·MINUS) 수.
///
/// 이 상한을 두기 전에도 메인 스레드에서 1만 2천 항 UNION 뷰까지는 분석됐다. 그보다
/// 넉넉하게 잡되, 상한까지의 사슬을 CLI 분석 스레드 스택이 감당하는 크기로 둔다.
pub(crate) const MAX_SET_OPERATORS: usize = 20_000;

/// 한 문장에서 이항·후위 연산자 자리에 온 토큰 수.
///
/// 연산자 토큰 수는 사슬 깊이의 상한이다. `id = 0 OR id = 1 …`처럼 항마다 연산자가
/// 둘이라, 상한을 두기 전에 분석되던 5만 항 OR 사슬도 이 상한 안에 든다.
pub(crate) const MAX_INFIX_OPERATORS: usize = 100_000;

/// `Parser::parse_sql`과 같은 문장을 돌려주되, 사슬 상한을 넘는 문장이 있으면 트리를
/// 만들지 않고 실제로 센 수를 담은 오류를 돌려준다.
pub(crate) fn parse_sql(dialect: &dyn Dialect, sql: &str) -> Result<Vec<Statement>, ParserError> {
    // Parser::parse_sql(try_with_sql)과 같은 토큰화 옵션이다. 토큰을 한 번만 만들어
    // 세기와 파싱에 함께 쓴다.
    let tokens = Tokenizer::new(dialect, sql)
        .with_unescape(ParserOptions::new().unescape)
        .tokenize_with_location()?;
    check_chain_lengths(&tokens)?;
    Parser::new(dialect)
        .with_tokens_with_locations(tokens)
        .parse_statements()
}

/// 한 문장에서 센 사슬 연산자 수.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct ChainCounts {
    pub(crate) set_operators: usize,
    pub(crate) infix_operators: usize,
}

/// `;`로 나뉜 문장마다 사슬 연산자를 센다.
///
/// 문장 경계만 끊어 세므로 한 문장 안의 여러 사슬은 합산된다 — 합이 그 문장 트리의
/// 깊이 상한이라 과소 계산하지 않는다. 단항 연산자는 sqlparser의 재귀 한도에 걸리므로
/// 피연산자 뒤에 온 기호만 이항으로 센다.
pub(crate) fn statement_chain_counts(tokens: &[TokenWithSpan]) -> Vec<ChainCounts> {
    let mut statements = vec![ChainCounts::default()];
    let mut after_operand = false;
    for token in tokens.iter().map(|token| &token.token) {
        if matches!(token, Token::Whitespace(_)) {
            continue;
        }
        if matches!(token, Token::SemiColon) {
            statements.push(ChainCounts::default());
        } else {
            // statements는 처음부터 원소 하나를 갖고 push만 하므로 비지 않는다.
            let counts = statements
                .last_mut()
                .expect("statement counts are never empty");
            count_token(token, after_operand, counts);
        }
        after_operand = ends_operand(token);
    }
    statements
}

/// 상한을 넘는 첫 문장을 그 문장에서 센 수와 함께 오류로 알린다.
fn check_chain_lengths(tokens: &[TokenWithSpan]) -> Result<(), ParserError> {
    for (index, counts) in statement_chain_counts(tokens).into_iter().enumerate() {
        for (kind, count, limit) in [
            ("set", counts.set_operators, MAX_SET_OPERATORS),
            ("infix", counts.infix_operators, MAX_INFIX_OPERATORS),
        ] {
            if count > limit {
                return Err(ParserError::ParserError(format!(
                    "statement {} has {count} chained {kind} operators, over the {limit} analysis limit; it was not parsed",
                    index + 1
                )));
            }
        }
    }
    Ok(())
}

/// 토큰 하나가 집합 연산자나 이항·후위 연산자면 그 수를 더한다.
fn count_token(token: &Token, after_operand: bool, counts: &mut ChainCounts) {
    match token {
        Token::Word(word) if is_set_operator(word.keyword) => counts.set_operators += 1,
        Token::Word(word) if is_infix_keyword(word.keyword) => counts.infix_operators += 1,
        Token::Word(_) => {}
        // PostgreSQL의 후위 `!`(팩토리얼)는 `x ! !`처럼 연산자 뒤에 와도 사슬을 잇는다.
        Token::ExclamationMark => counts.infix_operators += 1,
        token if after_operand && is_operator_symbol(token) => counts.infix_operators += 1,
        _ => {}
    }
}

fn is_set_operator(keyword: Keyword) -> bool {
    matches!(
        keyword,
        Keyword::UNION | Keyword::EXCEPT | Keyword::INTERSECT | Keyword::MINUS
    )
}

/// sqlparser가 중위 우선순위를 주는 키워드(기본 표와 PostgreSQL·MySQL·SQLite 재정의).
fn is_infix_keyword(keyword: Keyword) -> bool {
    matches!(
        keyword,
        Keyword::OR
            | Keyword::AND
            | Keyword::XOR
            | Keyword::IS
            | Keyword::IN
            | Keyword::BETWEEN
            | Keyword::OVERLAPS
            | Keyword::LIKE
            | Keyword::ILIKE
            | Keyword::RLIKE
            | Keyword::REGEXP
            | Keyword::MATCH
            | Keyword::SIMILAR
            | Keyword::MEMBER
            | Keyword::OPERATOR
            | Keyword::DIV
            | Keyword::COLLATE
            | Keyword::AT
    )
}

/// 연산자로 쓰일 수 있는 기호 토큰. 피연산자·구분자로 확실한 토큰만 빼고 모두 센다 —
/// 모르는 기호를 빼먹어 깊이를 과소 계산하는 것보다 넉넉히 세는 편이 안전하다.
fn is_operator_symbol(token: &Token) -> bool {
    !(is_literal(token)
        || matches!(
            token,
            Token::EOF
                | Token::Word(_)
                | Token::Number(..)
                | Token::Char(_)
                | Token::Placeholder(_)
                | Token::Whitespace(_)
                | Token::Comma
                | Token::Period
                | Token::SemiColon
                | Token::LParen
                | Token::RParen
                | Token::RBracket
                | Token::LBrace
                | Token::RBrace
        ))
}

/// 이 토큰 뒤의 기호가 이항 연산자일 수 있는가. 연산자·여는 괄호·구분자 뒤의 기호는
/// 단항 연산자나 와일드카드다. 후위 `!` 뒤는 이미 끝난 피연산자로 본다.
fn ends_operand(token: &Token) -> bool {
    match token {
        Token::Word(word) => !is_set_operator(word.keyword) && !is_infix_keyword(word.keyword),
        Token::ExclamationMark => true,
        Token::Comma | Token::Period | Token::SemiColon | Token::LParen | Token::LBrace => false,
        token => !is_operator_symbol(token),
    }
}

fn is_literal(token: &Token) -> bool {
    matches!(
        token,
        Token::SingleQuotedString(_)
            | Token::DoubleQuotedString(_)
            | Token::TripleSingleQuotedString(_)
            | Token::TripleDoubleQuotedString(_)
            | Token::DollarQuotedString(_)
            | Token::SingleQuotedByteStringLiteral(_)
            | Token::DoubleQuotedByteStringLiteral(_)
            | Token::TripleSingleQuotedByteStringLiteral(_)
            | Token::TripleDoubleQuotedByteStringLiteral(_)
            | Token::SingleQuotedRawStringLiteral(_)
            | Token::DoubleQuotedRawStringLiteral(_)
            | Token::TripleSingleQuotedRawStringLiteral(_)
            | Token::TripleDoubleQuotedRawStringLiteral(_)
            | Token::NationalStringLiteral(_)
            | Token::EscapedStringLiteral(_)
            | Token::UnicodeStringLiteral(_)
            | Token::HexStringLiteral(_)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlparser::dialect::{GenericDialect, PostgreSqlDialect};

    /// 문장마다 (집합 연산자 수, 이항·후위 연산자 수).
    fn counts(dialect: &dyn Dialect, sql: &str) -> Vec<(usize, usize)> {
        let tokens = Tokenizer::new(dialect, sql)
            .tokenize_with_location()
            .expect("tokens");
        statement_chain_counts(&tokens)
            .into_iter()
            .map(|counts| (counts.set_operators, counts.infix_operators))
            .collect()
    }

    #[test]
    fn 사슬_계수는_이항_자리의_연산자만_문장마다_센다() {
        let generic = GenericDialect {};
        // 단항 부호와 함수 인자의 `*`는 사슬을 만들지 않는다.
        assert_eq!(counts(&generic, "SELECT count(*), -1, +2 FROM t"), [(0, 0)]);
        assert_eq!(
            counts(&generic, "SELECT a + b - c FROM t WHERE x = 1 OR y = -2"),
            [(0, 5)]
        );
        assert_eq!(
            counts(
                &generic,
                "SELECT 1 UNION ALL SELECT 2 EXCEPT SELECT 3; SELECT x::int::text"
            ),
            [(2, 0), (0, 2)]
        );
        assert_eq!(
            counts(
                &generic,
                "SELECT a IS NOT NULL AND b NOT IN (1, 2) AND c BETWEEN 1 AND 2"
            ),
            [(0, 6)]
        );
        // 투영의 `*`는 넉넉히 세고, `t.*`는 세지 않는다.
        assert_eq!(
            counts(&generic, "SELECT * FROM t; ; SELECT t.* FROM t"),
            [(0, 1), (0, 0), (0, 0)]
        );
    }

    #[test]
    fn 연산자_뒤에_이어지는_후위_팩토리얼도_사슬로_센다() {
        assert_eq!(
            counts(&PostgreSqlDialect {}, "SELECT 5 ! ! ! + 1"),
            [(0, 4)]
        );
    }

    #[test]
    fn 상한을_넘는_문장은_트리를_만들지_않고_센_수를_알린다() {
        let terms = MAX_SET_OPERATORS + 2;
        let sql = vec!["SELECT 1"; terms].join(" UNION ALL ");
        let error = parse_sql(&GenericDialect {}, &format!("SELECT 0; {sql}"))
            .expect_err("over the set-operator limit");
        assert_eq!(
            error.to_string(),
            format!(
                "sql parser error: statement 2 has {} chained set operators, over the {} analysis limit; it was not parsed",
                MAX_SET_OPERATORS + 1,
                MAX_SET_OPERATORS
            )
        );
        // 상한 안의 문장은 sqlparser와 같은 결과다.
        let within = "SELECT a FROM t WHERE a = 1 UNION SELECT b FROM u";
        assert_eq!(
            parse_sql(&GenericDialect {}, within).expect("within limits"),
            Parser::parse_sql(&GenericDialect {}, within).expect("sqlparser")
        );
    }
}
