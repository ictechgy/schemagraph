//! 앞에 WITH가 붙은 DML의 대상 이름이 그 문장의 CTE 이름과 같을 때의 판정이다.
//!
//! 객체 단위 적용(`settle_cte_writes`)과 컬럼 단위 분석(`scope.rs`의 `with_statement`)이 같은
//! 판정과 같은 진단 문구를 쓰도록 한 곳에 둔다. 원칙은 하나다 — 방언의 규칙이나 이름의 대소문자
//! 규칙이 증명되지 않았으면 쓰기 간선은 이전처럼 남기되 `complete`를 주장하지 않는다(대상마다
//! 이름 붙은 `SG_DML_TARGET_SHAPE`, 부분 분석, 추측한 컬럼 효과 없음).

use schemagraph_core::Diagnostic;
use schemagraph_source::document::CatalogDocument;

/// CTE 이름과 같은 DML 대상을 방언이 무엇으로 보는가.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Rule {
    /// 같은 이름의 실제 테이블이다. PostgreSQL은 수정 문장의 대상을 실제 관계로만 연다
    /// (`setTargetTable`: "CTEs don't hide tables (for this purpose)"). PostgreSQL 16·SQLite
    /// 3.53.1에서 `WITH audit AS (…) INSERT|UPDATE|DELETE … audit`가 실제 `audit`를 바꿨다.
    Table,
    /// CTE다. SQL Server는 "Any reference to expression_name in the query uses the common table
    /// expression and not the base object"라 CTE를 통해 원본 테이블에 쓴다. MySQL은 CTE가 같은
    /// 이름의 테이블을 가리고(`QN shadows tmp table`) CTE를 UPDATE·DELETE 대상으로 쓰면
    /// `ER_NON_UPDATABLE_TABLE`로 거절한다(mysql-server 8.0 `mysql-test/include/with_non_recursive.inc`).
    Cte,
    /// 확인하지 않은 방언이다(MariaDB·Oracle·DB2·Informix·H2·`external` 등).
    Unknown,
}

/// 문서 방언 이름의 `Rule`이다.
pub(crate) fn rule(dialect: &str) -> Rule {
    match dialect {
        "postgres" | "postgresql" | "sqlite" => Rule::Table,
        "sqlserver" | "mssql" | "mysql" => Rule::Cte,
        _ => Rule::Unknown,
    }
}

/// 문서가 증명하는 CTE 이름의 대소문자 규칙이다.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum NameCase {
    /// 대소문자를 구분한다.
    Sensitive,
    /// 대소문자를 무시한다.
    Insensitive,
    /// 문서로 알 수 없다.
    Unknown,
}

/// 문서의 수집 context로 CTE 이름의 대소문자 규칙을 정한다.
///
/// SQL Server는 데이터베이스 collation을 따른다 — 문서는 그 collation이 "all metadata in the
/// database … and any other strings used in the database"에 쓰인다고 한다. `_CI`(터키어·아제르바이잔어·
/// CP1254 제외)는 무시, `_CS`·`_BIN`·`_BIN2`는 구분, collation이 없거나 그 밖이면 알 수 없다. MySQL은
/// `lower_case_table_names`≠0이면 CTE 이름과 테이블 참조를 모두 소문자로 바꿔 비교하고, 0이면 그대로
/// 비교한다(mysql-server 8.0 `PT_common_table_expr` 생성자·`match_table_ref`). 값이 없으면 알 수 없다.
pub(crate) fn name_case(doc: &CatalogDocument) -> NameCase {
    let context = doc.context.as_ref();
    match doc.dialect.as_str() {
        "sqlserver" | "mssql" => sqlserver_case(context.and_then(|c| c.collation.as_deref())),
        "mysql" => match context.and_then(|c| c.lower_case_table_names) {
            Some(0) => NameCase::Sensitive,
            Some(_) => NameCase::Insensitive,
            None => NameCase::Unknown,
        },
        _ => NameCase::Unknown,
    }
}

/// SQL Server collation 이름의 대소문자 속성이다.
fn sqlserver_case(collation: Option<&str>) -> NameCase {
    let Some(collation) = collation else {
        return NameCase::Unknown;
    };
    if super::collation_is_case_insensitive(collation) {
        return NameCase::Insensitive;
    }
    let sensitive = collation.split('_').any(|part| {
        ["CS", "BIN", "BIN2"]
            .iter()
            .any(|attribute| part.eq_ignore_ascii_case(attribute))
    });
    if sensitive {
        NameCase::Sensitive
    } else {
        NameCase::Unknown
    }
}

/// 대상 이름이 CTE 이름과 얼마나 같은가.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Match {
    /// 철자까지 같다.
    Exact,
    /// ASCII 대소문자만 다르다.
    AsciiCase,
    /// ASCII 밖 글자의 대소문자도 다르다(유니코드 소문자로는 같다).
    OtherCase,
}

/// 대상 이름과 가장 가까운 CTE 이름의 `Match`다. 대소문자를 빼고도 다르면 `None`이다.
pub(crate) fn name_match(target: &str, names: &[String]) -> Option<Match> {
    if names.iter().any(|name| name == target) {
        return Some(Match::Exact);
    }
    if names.iter().any(|name| name.eq_ignore_ascii_case(target)) {
        return Some(Match::AsciiCase);
    }
    let folded = target.to_lowercase();
    names
        .iter()
        .any(|name| name.to_lowercase() == folded)
        .then_some(Match::OtherCase)
}

/// 판정 결과다.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Outcome {
    /// 실제 테이블에 쓰는 평범한 DML이다.
    Table,
    /// 대상은 CTE다 — 그 이름으로 가는 쓰기 간선은 거짓이라 남기지 않는다.
    Cte,
    /// 증명되지 않았다 — 쓰기 간선은 남기고 컬럼 효과는 추측하지 않는다.
    Unproven,
}

/// 방언·대소문자 규칙과 이름 일치로 대상을 판정한다.
pub(crate) fn outcome(rule: Rule, case: NameCase, matched: Match) -> Outcome {
    match (rule, matched, case) {
        (Rule::Table, _, _) => Outcome::Table,
        (Rule::Unknown, _, _) => Outcome::Unproven,
        (Rule::Cte, Match::Exact, _) => Outcome::Cte,
        (Rule::Cte, _, NameCase::Sensitive) => Outcome::Table,
        (Rule::Cte, Match::AsciiCase, NameCase::Insensitive) => Outcome::Cte,
        // ASCII 밖 글자의 대소문자 접기는 collation마다 달라 모델링하지 않는다.
        (Rule::Cte, _, _) => Outcome::Unproven,
    }
}

/// 판정에 필요한 문서 쪽 사실이다.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Context<'a> {
    /// 문서 방언 이름.
    pub(crate) dialect: &'a str,
    /// 방언의 규칙.
    pub(crate) rule: Rule,
    /// CTE 이름의 대소문자 규칙.
    pub(crate) case: NameCase,
}

impl<'a> Context<'a> {
    /// 문서에서 판정 context를 만든다.
    pub(crate) fn new(dialect: &'a str, case: NameCase) -> Self {
        Self {
            dialect,
            rule: rule(dialect),
            case,
        }
    }

    /// 대상 철자와 CTE 이름들로 판정한다. CTE 이름과 대소문자를 빼고도 다르면 `None`이다.
    pub(crate) fn judge(&self, target: &str, names: &[String]) -> Option<Outcome> {
        name_match(target, names).map(|matched| outcome(self.rule, self.case, matched))
    }

    /// 판정의 `SG_DML_TARGET_SHAPE` 문구다. 평범한 DML(`Table`)이면 없다.
    ///
    /// 대상 이름을 실어 대상마다 따로 센다. 대소문자를 무시하는 문서에서는 접은 이름을 실어,
    /// 철자만 다른 같은 대상(`dupes`·`DUPES`)을 한 번만 센다.
    pub(crate) fn message(&self, target: &str, outcome: Outcome) -> Option<String> {
        let shown = if self.case == NameCase::Insensitive {
            printable(&target.to_lowercase())
        } else {
            printable(target)
        };
        let reason = match (outcome, self.rule) {
            (Outcome::Table, _) => return None,
            (Outcome::Cte, _) if self.dialect == "mysql" => {
                "MySQL rejects a WITH query as an UPDATE or DELETE target (ER_NON_UPDATABLE_TABLE), so no write edge or column effect is recorded"
            }
            (Outcome::Cte, _) => {
                "SQL Server writes through it to a base relation that is not resolved, so no write edge or column effect is recorded"
            }
            (Outcome::Unproven, Rule::Cte) => {
                "the names differ only in letter case and the document does not prove the case rule; the write edge is kept, but target columns are not resolved"
            }
            (Outcome::Unproven, _) => {
                "this dialect's rule for such targets is unverified; the write edge is kept, but target columns are not resolved"
            }
        };
        Some(format!("DML target '{shown}' names a WITH query; {reason}"))
    }
}

/// CTE 이름 대상 진단을 같은 문구마다 한 번만 남긴다.
pub(crate) fn push_diagnostic(diagnostics: &mut Vec<Diagnostic>, message: &str) {
    let known = diagnostics.iter().any(|diagnostic| {
        diagnostic.code == "SG_DML_TARGET_SHAPE" && diagnostic.message == message
    });
    if !known {
        diagnostics.push(Diagnostic {
            code: "SG_DML_TARGET_SHAPE".into(),
            message: message.to_owned(),
            location: None,
            candidates: Vec::new(),
        });
    }
}

/// 진단 문구에 싣는 이름의 제어문자를 `\u{..}`로 바꾼다. 줄바꿈·ESC가 그대로 실리면 한계 목록에
/// 가짜 줄이나 터미널 색 제어가 생긴다(네이티브 reader 한계 문구의 `printable`과 같은 규칙).
fn printable(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_control() || c == '\u{2028}' || c == '\u{2029}' {
                c.escape_unicode().to_string()
            } else {
                c.to_string()
            }
        })
        .collect()
}
