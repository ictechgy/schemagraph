//! 동적 SQL에 사용할 수 있는 작은 상수 텍스트 평가기.
//!
//! 절차형 SQL 전체를 평가하려는 모듈이 아니다. 문자열 리터럴, 이미 알고
//! 있는 텍스트 변수, PostgreSQL `format()`의 제한된 형식, 방언별 문자열
//! 연결만 다룬다. 나머지는 모두 실패시켜 호출자가 미추출 한계로 보고한다.

use std::collections::{BTreeMap, BTreeSet};

/// 상수 텍스트 식을 해석할 때 적용할 SQL 방언의 차이.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TextDialect {
    /// PostgreSQL의 `||` 연결과 `format()`을 허용한다.
    Postgres,
    /// Oracle의 `||` 연결과 q-quote를 허용한다.
    Oracle,
    /// T-SQL의 `+` 연결과 N 문자열을 허용한다.
    MsSql,
    /// 알려지지 않은 방언에서는 연결식도 보수적으로 평가하지 않는다.
    Other,
}

/// 절차형 변수 선언에서 보존할 수 있는 텍스트 타입의 상한.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TextCapacity {
    /// 엔진의 전체 상수 텍스트 상한까지만 적용한다.
    Unbounded,
    /// 선언된 바이트 길이까지 보존한다.
    Bytes(usize),
}

/// 출력과 재귀를 제한해 악의적이거나 실수로 커진 식이 메모리를 폭발시키지
/// 않게 한다. 동적 SQL은 분석 보조 자료이므로 한계를 넘으면 미추출이 맞다.
const MAX_TEXT_BYTES: usize = 64 * 1024;
const MAX_DEPTH: usize = 32;
const MAX_FORMAT_ARGS: usize = 64;

/// 방언 이름을 상수 식 평가 방언으로 바꾼다.
pub(crate) fn dialect(name: &str) -> TextDialect {
    match name {
        "postgres" | "postgresql" => TextDialect::Postgres,
        "oracle" => TextDialect::Oracle,
        "sqlserver" | "mssql" => TextDialect::MsSql,
        _ => TextDialect::Other,
    }
}

/// 제한된 텍스트 타입만 변수 추적 대상으로 인정한다. CHAR와 숫자·날짜
/// 타입은 패딩·변환 의미를 재현하지 않으므로 보수적으로 거부한다.
pub(crate) fn declared_text_capacity(type_text: &str) -> Option<TextCapacity> {
    let normalized = type_text
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_lowercase();
    let normalized = normalized.strip_prefix("constant ").unwrap_or(&normalized);
    match normalized {
        "text" | "clob" | "ntext" | "varchar" | "nvarchar" | "character varying" => {
            return Some(TextCapacity::Unbounded)
        }
        _ => {}
    }
    for prefix in [
        "varchar",
        "nvarchar",
        "varchar2",
        "nvarchar2",
        "character varying",
    ] {
        let Some(rest) = normalized.strip_prefix(prefix) else {
            continue;
        };
        let Some(inner) = rest
            .strip_prefix('(')
            .and_then(|value| value.strip_suffix(')'))
        else {
            continue;
        };
        let size = inner.split_whitespace().next()?;
        if size == "max" {
            return Some(TextCapacity::Unbounded);
        }
        return size.parse::<usize>().ok().map(TextCapacity::Bytes);
    }
    None
}

/// SQL Server에서 해당 타입이 Unicode code page 보존을 보장하는지 확인한다.
pub(crate) fn is_unicode_text_type(type_text: &str) -> bool {
    let normalized = type_text
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_lowercase();
    normalized == "ntext"
        || normalized == "nvarchar"
        || normalized.starts_with("nvarchar(")
        || normalized == "nvarchar2"
        || normalized.starts_with("nvarchar2(")
}

/// 타입 상한과 evaluator 전체 상한을 함께 적용한다.
pub(crate) fn fits_capacity(value: &str, capacity: TextCapacity) -> bool {
    value.len() <= MAX_TEXT_BYTES
        && match capacity {
            TextCapacity::Unbounded => true,
            TextCapacity::Bytes(limit) => value.len() <= limit,
        }
}

/// 식 전체가 제한된 텍스트 식으로 확정되면 그 결과를 돌려준다.
#[cfg(test)]
pub(crate) fn evaluate(
    source: &str,
    dialect: TextDialect,
    variables: &BTreeMap<String, String>,
) -> Option<String> {
    evaluate_with_format(source, dialect, variables, true)
}

/// PostgreSQL `format` 이름이 사용자 routine으로 가려졌을 때처럼 호출자가
/// built-in folding을 금지할 수 있는 평가 진입점.
pub(crate) fn evaluate_with_format(
    source: &str,
    dialect: TextDialect,
    variables: &BTreeMap<String, String>,
    allow_format: bool,
) -> Option<String> {
    if dialect == TextDialect::MsSql && has_non_unicode_literal(source) {
        return None;
    }
    let mut parser = Parser {
        source,
        position: 0,
        dialect,
        variables,
        depth: 0,
        allow_format,
    };
    let result = parser.expression().ok()?;
    parser.skip_space_and_comments();
    (parser.position == source.len()).then_some(result)
}

struct Parser<'a> {
    source: &'a str,
    position: usize,
    dialect: TextDialect,
    variables: &'a BTreeMap<String, String>,
    depth: usize,
    allow_format: bool,
}

impl<'a> Parser<'a> {
    fn expression(&mut self) -> Result<String, ()> {
        self.enter()?;
        let mut value = self.atom()?;
        loop {
            self.skip_space_and_comments();
            let operator_len = match self.dialect {
                TextDialect::Postgres | TextDialect::Oracle => {
                    self.source[self.position..].starts_with("||").then_some(2)
                }
                TextDialect::MsSql => {
                    (self.source.as_bytes().get(self.position) == Some(&b'+')).then_some(1)
                }
                TextDialect::Other => None,
            };
            let Some(operator_len) = operator_len else {
                break;
            };
            self.position += operator_len;
            let right = self.atom()?;
            append_checked(&mut value, &right)?;
        }
        self.leave();
        Ok(value)
    }

    fn atom(&mut self) -> Result<String, ()> {
        self.skip_space_and_comments();
        if self.source.as_bytes().get(self.position) == Some(&b'(') {
            self.position += 1;
            let value = self.expression()?;
            self.skip_space_and_comments();
            if self.source.as_bytes().get(self.position) != Some(&b')') {
                return Err(());
            }
            self.position += 1;
            return Ok(value);
        }
        if let Some((value, end)) = parse_string(self.source, self.position, self.dialect) {
            self.position = end;
            return bounded(value);
        }

        let start = self.position;
        let name_end = identifier_end(self.source, start);
        if name_end == start {
            return Err(());
        }
        let mut name_end = name_end;
        self.position = name_end;
        self.skip_space_and_comments();
        if self.source.as_bytes().get(self.position) == Some(&b'.') {
            self.position += 1;
            let qualified_end = identifier_end(self.source, self.position);
            if qualified_end == self.position {
                return Err(());
            }
            name_end = qualified_end;
            self.position = qualified_end;
            self.skip_space_and_comments();
        }
        let name = &self.source[start..name_end];
        if self.source.as_bytes().get(self.position) == Some(&b'(') {
            self.position += 1;
            return self.call(name);
        }
        self.variables
            .get(&normalize_variable(name))
            .cloned()
            .ok_or(())
    }

    fn call(&mut self, name: &str) -> Result<String, ()> {
        let qualified = name.eq_ignore_ascii_case("pg_catalog.format");
        let unqualified = name.eq_ignore_ascii_case("format");
        if self.dialect != TextDialect::Postgres
            || (!qualified && (!unqualified || !self.allow_format))
        {
            return Err(());
        }
        let mut args = Vec::new();
        self.skip_space_and_comments();
        if self.source.as_bytes().get(self.position) != Some(&b')') {
            loop {
                if args.len() == MAX_FORMAT_ARGS {
                    return Err(());
                }
                args.push(self.expression()?);
                self.skip_space_and_comments();
                match self.source.as_bytes().get(self.position) {
                    Some(b',') => self.position += 1,
                    Some(b')') => break,
                    _ => return Err(()),
                }
            }
        }
        if self.source.as_bytes().get(self.position) != Some(&b')') {
            return Err(());
        }
        self.position += 1;
        let Some(template) = args.first() else {
            return Err(());
        };
        format_template(template, &args[1..])
    }

    fn enter(&mut self) -> Result<(), ()> {
        if self.depth >= MAX_DEPTH {
            return Err(());
        }
        self.depth += 1;
        Ok(())
    }

    fn leave(&mut self) {
        self.depth = self.depth.saturating_sub(1);
    }

    fn skip_space_and_comments(&mut self) {
        loop {
            let before = self.position;
            while self
                .source
                .as_bytes()
                .get(self.position)
                .is_some_and(|byte| byte.is_ascii_whitespace())
            {
                self.position += 1;
            }
            if self.source[self.position..].starts_with("--") {
                self.position += 2;
                while self
                    .source
                    .as_bytes()
                    .get(self.position)
                    .is_some_and(|byte| *byte != b'\n')
                {
                    self.position += 1;
                }
            } else if self.source[self.position..].starts_with("/*") {
                let Some(end) = self.source[self.position + 2..].find("*/") else {
                    self.position = self.source.len();
                    return;
                };
                self.position += end + 4;
            }
            if before == self.position {
                return;
            }
        }
    }
}

/// 문자열·주석 밖에서 multi-assignment RHS가 같은 문장의 대입 대상을
/// 참조하는지 확인한다. SQL Server의 대입 순서는 보장되지 않으므로 이런
/// 값은 순차 실행 결과로 접지 않는다.
pub(crate) fn references_variables(
    source: &str,
    dialect: TextDialect,
    names: &BTreeSet<String>,
) -> bool {
    let bytes = source.as_bytes();
    let mut position = 0;
    while position < bytes.len() {
        if let Some(end) = skip_comment(source, position) {
            position = end;
            continue;
        }
        if let Some((_, end)) = parse_string(source, position, dialect) {
            position = end;
            continue;
        }
        let end = identifier_end(source, position);
        if end > position {
            if names.contains(&normalize_variable(&source[position..end])) {
                return true;
            }
            position = end;
            continue;
        }
        position += source[position..].chars().next().map_or(1, char::len_utf8);
    }
    false
}

fn bounded(value: String) -> Result<String, ()> {
    (value.len() <= MAX_TEXT_BYTES).then_some(value).ok_or(())
}

fn append_checked(target: &mut String, suffix: &str) -> Result<(), ()> {
    if target.len().saturating_add(suffix.len()) > MAX_TEXT_BYTES {
        return Err(());
    }
    target.push_str(suffix);
    Ok(())
}

fn normalize_variable(name: &str) -> String {
    name.trim_start_matches('@').to_ascii_lowercase()
}

fn has_non_unicode_literal(source: &str) -> bool {
    let bytes = source.as_bytes();
    let mut position = 0;
    while position < bytes.len() {
        if let Some(end) = skip_comment(source, position) {
            position = end;
            continue;
        }
        if let Some((value, end)) = parse_string(source, position, TextDialect::MsSql) {
            let is_n_literal = position + 1 < bytes.len()
                && matches!(bytes[position], b'n' | b'N')
                && bytes[position + 1] == b'\'';
            if !is_n_literal && !value.is_ascii() {
                return true;
            }
            position = end;
            continue;
        }
        position += source[position..].chars().next().map_or(1, char::len_utf8);
    }
    false
}

fn skip_comment(source: &str, position: usize) -> Option<usize> {
    if source[position..].starts_with("--") {
        return Some(
            source[position + 2..]
                .find('\n')
                .map_or(source.len(), |end| position + 2 + end + 1),
        );
    }
    if source[position..].starts_with("/*") {
        return source[position + 2..]
            .find("*/")
            .map(|end| position + 2 + end + 2)
            .or(Some(source.len()));
    }
    None
}

fn identifier_end(source: &str, start: usize) -> usize {
    let bytes = source.as_bytes();
    let mut end = start;
    if bytes.get(end) == Some(&b'@') {
        end += 1;
    }
    while bytes
        .get(end)
        .is_some_and(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'$' | b'#'))
    {
        end += 1;
    }
    if end == start || (end == start + 1 && bytes[start] == b'@') {
        start
    } else {
        end
    }
}

fn parse_string(source: &str, start: usize, dialect: TextDialect) -> Option<(String, usize)> {
    let bytes = source.as_bytes();
    match bytes.get(start).copied()? {
        b'\'' => parse_quoted(source, start, false),
        b'n' | b'N' if bytes.get(start + 1) == Some(&b'\'') => {
            parse_quoted(source, start + 1, false)
        }
        b'e' | b'E' if dialect == TextDialect::Postgres && bytes.get(start + 1) == Some(&b'\'') => {
            parse_quoted(source, start + 1, true)
        }
        b'q' | b'Q' if dialect == TextDialect::Oracle && bytes.get(start + 1) == Some(&b'\'') => {
            parse_q_quoted(source, start)
        }
        b'$' if dialect == TextDialect::Postgres => parse_dollar_quoted(source, start),
        _ => None,
    }
}

fn parse_quoted(source: &str, quote: usize, escapes: bool) -> Option<(String, usize)> {
    let bytes = source.as_bytes();
    let mut position = quote + 1;
    let mut value = Vec::new();
    while position < bytes.len() {
        match bytes[position] {
            b'\'' if bytes.get(position + 1) == Some(&b'\'') => {
                push_bytes(&mut value, b"'")?;
                position += 2;
            }
            b'\'' => return Some((String::from_utf8(value).ok()?, position + 1)),
            b'\\' if escapes => {
                let (escaped, end) = parse_escape(source, position)?;
                push_bytes(&mut value, &escaped)?;
                position = end;
            }
            _ => {
                let character = source[position..].chars().next()?;
                let mut encoded = [0; 4];
                push_bytes(&mut value, character.encode_utf8(&mut encoded).as_bytes())?;
                position += character.len_utf8();
            }
        }
    }
    None
}

fn push_bytes(target: &mut Vec<u8>, bytes: &[u8]) -> Option<()> {
    if target.len().saturating_add(bytes.len()) > MAX_TEXT_BYTES {
        return None;
    }
    target.extend_from_slice(bytes);
    Some(())
}

fn parse_escape(source: &str, slash: usize) -> Option<(Vec<u8>, usize)> {
    let bytes = source.as_bytes();
    let code = *bytes.get(slash + 1)?;
    let simple = match code {
        b'b' => Some(b'\x08'),
        b'f' => Some(b'\x0c'),
        b'n' => Some(b'\n'),
        b'r' => Some(b'\r'),
        b't' => Some(b'\t'),
        b'v' => Some(b'\x0b'),
        b'\\' => Some(b'\\'),
        b'\'' => Some(b'\''),
        b'"' => Some(b'"'),
        _ => None,
    };
    if let Some(character) = simple {
        return Some((vec![character], slash + 2));
    }
    if (b'0'..=b'7').contains(&code) {
        let mut end = slash + 1;
        let mut digits = 0;
        while digits < 3
            && bytes
                .get(end)
                .is_some_and(|digit| (b'0'..=b'7').contains(digit))
        {
            end += 1;
            digits += 1;
        }
        let value = u8::from_str_radix(&source[slash + 1..end], 8).ok()?;
        if value == 0 {
            return None;
        }
        return Some((vec![value], end));
    }
    if code == b'x' {
        let mut end = slash + 2;
        while end < slash + 4 && bytes.get(end).is_some_and(u8::is_ascii_hexdigit) {
            end += 1;
        }
        if end == slash + 2 {
            return None;
        }
        let value = u8::from_str_radix(&source[slash + 2..end], 16).ok()?;
        if value == 0 {
            return None;
        }
        return Some((vec![value], end));
    }
    if code == b'u' || code == b'U' {
        let width = if code == b'u' { 4 } else { 8 };
        let digits = source.get(slash + 2..slash + 2 + width)?;
        let value = u32::from_str_radix(digits, 16).ok()?;
        let character = char::from_u32(value)?;
        let mut encoded = [0; 4];
        return Some((
            character.encode_utf8(&mut encoded).as_bytes().to_vec(),
            slash + 2 + width,
        ));
    }
    None
}

fn parse_dollar_quoted(source: &str, start: usize) -> Option<(String, usize)> {
    let bytes = source.as_bytes();
    let mut tag_end = start + 1;
    if bytes.get(tag_end) == Some(&b'$') {
        // $$는 빈 태그 형식이다.
    } else {
        if !bytes
            .get(tag_end)
            .is_some_and(|byte| byte.is_ascii_alphabetic() || *byte == b'_')
        {
            return None;
        }
        tag_end += 1;
        while bytes
            .get(tag_end)
            .is_some_and(|byte| byte.is_ascii_alphanumeric() || *byte == b'_')
        {
            tag_end += 1;
        }
    }
    if bytes.get(tag_end) != Some(&b'$') {
        return None;
    }
    let tag = &source[start..=tag_end];
    let content_start = tag_end + 1;
    let close = source[content_start..].find(tag)? + content_start;
    if close - content_start > MAX_TEXT_BYTES {
        return None;
    }
    Some((source[content_start..close].to_owned(), close + tag.len()))
}

fn parse_q_quoted(source: &str, start: usize) -> Option<(String, usize)> {
    let delimiter = source.get(start + 2..)?.chars().next()?;
    let closing = match delimiter {
        '[' => ']',
        '(' => ')',
        '{' => '}',
        '<' => '>',
        character if !character.is_whitespace() && character != '\'' => character,
        _ => return None,
    };
    let content_start = start + 2 + delimiter.len_utf8();
    let end_marker = format!("{closing}'");
    let close = source[content_start..].find(&end_marker)? + content_start;
    if close - content_start > MAX_TEXT_BYTES {
        return None;
    }
    Some((
        source[content_start..close].to_owned(),
        close + end_marker.len(),
    ))
}

fn format_template(template: &str, args: &[String]) -> Result<String, ()> {
    let mut output = String::new();
    let mut position = 0;
    let mut next_argument = 0usize;
    let mut used_explicit = false;
    let mut used_implicit = false;
    while position < template.len() {
        let character = template[position..].chars().next().ok_or(())?;
        position += character.len_utf8();
        if character != '%' {
            output.push(character);
            continue;
        }
        let next = template[position..].chars().next().ok_or(())?;
        if next == '%' {
            position += 1;
            append_checked(&mut output, "%")?;
            continue;
        }
        let digit_start = position;
        while let Some(digit) = template[position..].chars().next() {
            if !digit.is_ascii_digit() {
                break;
            }
            position += digit.len_utf8();
        }
        let explicit = if position > digit_start && template[position..].starts_with('$') {
            let number = template[digit_start..position]
                .parse::<usize>()
                .map_err(|_| ())?;
            position += 1;
            used_explicit = true;
            if used_implicit {
                return Err(());
            }
            (number > 0).then_some(number - 1)
        } else {
            // 폭·플래그·정밀도는 의도적으로 지원 범위에서 제외한다.
            if position > digit_start {
                return Err(());
            }
            used_implicit = true;
            if used_explicit {
                return Err(());
            }
            None
        };
        let kind = template[position..].chars().next().ok_or(())?;
        position += kind.len_utf8();
        if !matches!(kind, 's' | 'I' | 'L') {
            return Err(());
        }
        let index = explicit.unwrap_or_else(|| {
            let current = next_argument;
            next_argument += 1;
            current
        });
        let value = args.get(index).ok_or(())?;
        let rendered = match kind {
            's' => value.clone(),
            'I' => quote_identifier(value),
            'L' => quote_literal(value)?,
            _ => unreachable!(),
        };
        append_checked(&mut output, &rendered)?;
    }
    bounded(output)
}

fn quote_identifier(value: &str) -> String {
    let mut output = String::with_capacity(value.len() + 2);
    output.push('"');
    for character in value.chars() {
        if character == '"' {
            output.push('"');
        }
        output.push(character);
    }
    output.push('"');
    output
}

fn quote_literal(value: &str) -> Result<String, ()> {
    // standard_conforming_strings 설정을 문서에서 알 수 없으므로 역슬래시가
    // 있는 값은 quote_literal 결과를 안전하게 재현할 수 없어 거부한다.
    if value.contains('\\') || value.contains('\0') {
        return Err(());
    }
    let mut output = String::with_capacity(value.len() + 2);
    output.push('\'');
    for character in value.chars() {
        if character == '\'' {
            output.push('\'');
        }
        output.push(character);
    }
    output.push('\'');
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vars(entries: &[(&str, &str)]) -> BTreeMap<String, String> {
        entries
            .iter()
            .map(|(name, value)| (normalize_variable(name), (*value).to_owned()))
            .collect()
    }

    #[test]
    fn evaluates_parenthesized_concatenation_and_unicode() {
        assert_eq!(
            evaluate(
                "('SELECT ' || '한글' || ' FROM customers')",
                TextDialect::Postgres,
                &BTreeMap::new()
            ),
            Some("SELECT 한글 FROM customers".to_owned())
        );
    }

    #[test]
    fn evaluates_tsql_plus_and_known_variables() {
        assert_eq!(
            evaluate(
                "@head + N' customers'",
                TextDialect::MsSql,
                &vars(&[("@head", "SELECT * FROM")])
            ),
            Some("SELECT * FROM customers".to_owned())
        );
    }

    #[test]
    fn rejects_unknown_suffix_and_arbitrary_functions() {
        assert_eq!(
            evaluate(
                "'SELECT 1' || suffix",
                TextDialect::Postgres,
                &BTreeMap::new()
            ),
            None
        );
        assert_eq!(
            evaluate(
                "concat('SELECT ', '1')",
                TextDialect::Postgres,
                &BTreeMap::new()
            ),
            None
        );
        assert_eq!(
            evaluate("format('%s', 'x')", TextDialect::MsSql, &BTreeMap::new()),
            None
        );
    }

    #[test]
    fn evaluates_postgres_format_specifiers_and_escapes() {
        assert_eq!(
            evaluate(
                "format('SELECT %1$I FROM %2$I WHERE note = %3$L %%', '한글', 'customers', 'a''b')",
                TextDialect::Postgres,
                &BTreeMap::new()
            ),
            Some("SELECT \"한글\" FROM \"customers\" WHERE note = 'a''b' %".to_owned())
        );
        assert_eq!(
            evaluate("E'SELECT \\n 1'", TextDialect::Postgres, &BTreeMap::new()),
            Some("SELECT \n 1".to_owned())
        );
        assert_eq!(
            evaluate("E'caf\\xC3\\xA9'", TextDialect::Postgres, &BTreeMap::new()),
            Some("café".to_owned())
        );
        assert_eq!(
            evaluate("E'line\\012next'", TextDialect::Postgres, &BTreeMap::new()),
            Some("line\nnext".to_owned())
        );
    }

    #[test]
    fn skips_comments_and_preserves_quoted_text() {
        assert_eq!(
            evaluate(
                "'SELECT ''EXECUTE; -- data''' /* operator */ || ' FROM 한글' -- tail\n",
                TextDialect::Postgres,
                &BTreeMap::new()
            ),
            Some("SELECT 'EXECUTE; -- data' FROM 한글".to_owned())
        );
    }

    #[test]
    fn rejects_unsupported_format_shapes_and_caps_depth() {
        assert_eq!(
            evaluate(
                "format('%10s', 'x')",
                TextDialect::Postgres,
                &BTreeMap::new()
            ),
            None
        );
        assert_eq!(
            evaluate(
                "format('%s', unknown)",
                TextDialect::Postgres,
                &BTreeMap::new()
            ),
            None
        );
        assert_eq!(
            evaluate(
                "format('%1$s %s', 'x', 'y')",
                TextDialect::Postgres,
                &BTreeMap::new()
            ),
            None
        );
        assert_eq!(
            evaluate(
                "format('%L', 'a\\\\b')",
                TextDialect::Postgres,
                &BTreeMap::new()
            ),
            None
        );
        assert_eq!(
            evaluate_with_format(
                "format('%s', 'x')",
                TextDialect::Postgres,
                &BTreeMap::new(),
                false
            ),
            None
        );
        assert_eq!(
            evaluate_with_format(
                "pg_catalog.format('%s', 'x')",
                TextDialect::Postgres,
                &BTreeMap::new(),
                false
            ),
            Some("x".to_owned())
        );
        let nested = "(".repeat(MAX_DEPTH + 1) + "'x'" + &")".repeat(MAX_DEPTH + 1);
        assert_eq!(
            evaluate(&nested, TextDialect::Postgres, &BTreeMap::new()),
            None
        );
        let oversized = format!("'{}'", "x".repeat(MAX_TEXT_BYTES + 1));
        assert_eq!(
            evaluate(&oversized, TextDialect::Postgres, &BTreeMap::new()),
            None
        );
        let oversized_dollar = format!("$${}$$", "x".repeat(MAX_TEXT_BYTES + 1));
        assert_eq!(
            evaluate(&oversized_dollar, TextDialect::Postgres, &BTreeMap::new()),
            None
        );
        let oversized_q = format!("q'[{}]'", "x".repeat(MAX_TEXT_BYTES + 1));
        assert_eq!(
            evaluate(&oversized_q, TextDialect::Oracle, &BTreeMap::new()),
            None
        );
    }

    #[test]
    fn only_tracks_safe_text_declarations_within_capacity() {
        assert_eq!(
            declared_text_capacity("text"),
            Some(TextCapacity::Unbounded)
        );
        assert_eq!(
            declared_text_capacity("VARCHAR2(8)"),
            Some(TextCapacity::Bytes(8))
        );
        assert_eq!(declared_text_capacity("CHAR(8)"), None);
        assert_eq!(declared_text_capacity("INTEGER"), None);
        assert!(fits_capacity("12345678", TextCapacity::Bytes(8)));
        assert!(!fits_capacity("123456789", TextCapacity::Bytes(8)));
    }
}
