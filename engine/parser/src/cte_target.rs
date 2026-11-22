//! 앞에 WITH가 붙은 DML의 대상 이름이 그 문장의 CTE 이름과 같을 때의 판정이다.
//!
//! 객체 단위 적용(`settle_cte_writes`)과 컬럼 단위 분석(`scope.rs`의 `with_statement`)이 같은
//! 판정과 같은 진단 문구를 쓰도록 한 곳에 둔다. 원칙은 하나다 — 방언의 규칙이나 이름의 비교
//! 규칙이 증명되지 않았으면 쓰기 간선은 이전처럼 남기되 `complete`를 주장하지 않는다(대상마다
//! 이름 붙은 `SG_DML_TARGET_SHAPE`, 부분 분석, 추측한 컬럼 효과 없음).
//!
//! 이름 비교는 collation을 다시 구현하지 않는다. 바이트가 같으면 같은 이름이고, 둘 다 ASCII면
//! 증명된 대소문자 규칙을 적용한다. 한쪽이라도 ASCII 밖 글자가 있고 바이트가 다르면 정확 비교
//! 규칙(MySQL `lower_case_table_names`=0의 memcmp)에서만 다르다고 증명되고, 접는 규칙이나 모르는
//! 규칙에서는 증명되지 않는다 — 그리스어 어말 시그마(`ΑΣ`→`ας`), 전각(`ａｕｄｉｔ`), 악센트
//! (`äudit`)처럼 collation마다 다른 접기를 한 번에 덮는다.

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

/// 문서가 증명하는 CTE 이름의 비교 규칙이다.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum NameCase {
    /// 철자를 바이트 그대로 비교한다 — ASCII 밖 글자의 차이도 다른 이름이다.
    Sensitive,
    /// ASCII 대소문자를 무시한다. ASCII 밖 글자를 어떻게 접는지는 증명하지 않는다.
    Insensitive,
    /// 문서로 알 수 없다.
    Unknown,
}

/// 문서의 수집 context로 CTE 이름의 비교 규칙을 정한다.
///
/// MySQL은 `lower_case_table_names`≠0이면 CTE 이름과 테이블 참조를 모두 소문자로 바꾸고, 0이면
/// 바꾸지 않은 채 memcmp로 비교한다(mysql-server 8.0 `PT_common_table_expr` 생성자의
/// `my_casedn_str`, `PT_common_table_expr::match_table_ref`의 "memcmp() is fine even if
/// lower_case_table_names==1, as CTE names have been lowercased in the ctor"). 값이 없으면 알 수
/// 없다.
///
/// SQL Server는 어느 collation이 CTE 이름을 비교하는지 문서가 말하지 않아 늘 알 수 없다고 본다.
/// Microsoft 문서는 데이터베이스 안 객체(테이블·뷰·컬럼)의 식별자에 데이터베이스 collation을,
/// 변수·GOTO 레이블·임시 테이블 식별자에 인스턴스 collation을 쓴다고 하고(COLLATE (Transact-SQL)
/// "The identifiers for variables, GOTO labels, temporary stored procedures, and temporary tables
/// are in the default collation of the server instance"), 포함된 데이터베이스는 그것들을
/// 카탈로그 collation(`Latin1_General_100_CI_AS_WS_KS_SC`)으로, Azure SQL Database는 식별자를
/// `CATALOG_COLLATION`으로 비교한다(Contained Database Collations·CREATE DATABASE). CTE 이름(질의
/// 범위 이름)은 어느 목록에도 없고, 문서가 싣는 collation은 데이터베이스 collation
/// (`DATABASEPROPERTYEX(DB_NAME(), 'Collation')`)뿐이다. 그래서 철자가 정확히 같은 대상만 CTE로
/// 판정한다("Any reference to expression_name in the query uses the common table expression").
pub(crate) fn name_case(doc: &CatalogDocument) -> NameCase {
    let context = doc.context.as_ref();
    match doc.dialect.as_str() {
        "mysql" => match context.and_then(|c| c.lower_case_table_names) {
            Some(0) => NameCase::Sensitive,
            Some(_) => NameCase::Insensitive,
            None => NameCase::Unknown,
        },
        _ => NameCase::Unknown,
    }
}

/// 대상 이름이 CTE 이름과 얼마나 같은가.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Match {
    /// 바이트까지 같다.
    Exact,
    /// 둘 다 ASCII이고 대소문자만 다르다.
    AsciiCase,
    /// 바이트가 다르고 한쪽이라도 ASCII 밖 글자가 있다 — 대소문자·악센트·전각을 접는 규칙에
    /// 따라 같은 이름일 수 있다.
    NonAscii,
}

/// 대상과 가장 가깝게 일치하는 CTE 이름이다.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Candidate {
    /// 얼마나 같은가.
    pub(crate) matched: Match,
    /// 그 CTE 이름(원래 철자).
    pub(crate) cte: String,
}

/// 대상 이름과 가장 가까운 CTE 이름이다. `Exact`·`AsciiCase`·`NonAscii` 순으로 고른다. 둘 다
/// ASCII인데 대소문자를 빼고도 다른 이름뿐이면 `None`이다 — 어떤 대소문자 규칙에서도 다르다.
pub(crate) fn name_match(target: &str, names: &[String]) -> Option<Candidate> {
    let find = |matched: Match, same: &dyn Fn(&str) -> bool| {
        names.iter().find(|name| same(name)).map(|name| Candidate {
            matched,
            cte: name.clone(),
        })
    };
    find(Match::Exact, &|name| name == target)
        .or_else(|| {
            find(Match::AsciiCase, &|name| {
                name.is_ascii() && target.is_ascii() && name.eq_ignore_ascii_case(target)
            })
        })
        .or_else(|| {
            find(Match::NonAscii, &|name| {
                !(name.is_ascii() && target.is_ascii())
            })
        })
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

/// 방언·비교 규칙과 이름 일치로 대상을 판정한다.
pub(crate) fn outcome(rule: Rule, case: NameCase, matched: Match) -> Outcome {
    match (rule, matched, case) {
        (Rule::Table, _, _) => Outcome::Table,
        (Rule::Unknown, _, _) => Outcome::Unproven,
        (Rule::Cte, Match::Exact, _) => Outcome::Cte,
        // 바이트 비교 규칙에서는 철자가 다르면 다른 이름이다(ASCII 밖 글자 포함).
        (Rule::Cte, _, NameCase::Sensitive) => Outcome::Table,
        (Rule::Cte, Match::AsciiCase, NameCase::Insensitive) => Outcome::Cte,
        // ASCII 밖 글자의 접기는 collation마다 달라 모델링하지 않는다.
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
    /// CTE 이름의 비교 규칙.
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

    /// 대상 철자와 CTE 이름들로 판정한다. 어떤 CTE 이름의 후보도 아니면 `None`이다.
    pub(crate) fn judge(&self, target: &str, names: &[String]) -> Option<(Candidate, Outcome)> {
        name_match(target, names).map(|candidate| {
            let outcome = outcome(self.rule, self.case, candidate.matched);
            (candidate, outcome)
        })
    }

    /// 판정의 `SG_DML_TARGET_SHAPE` 문구다. 평범한 DML(`Table`)이면 없다.
    ///
    /// 대상 이름을 실어 대상마다 따로 센다. 대상이 CTE 이름이면(철자가 같거나 CTE로 증명됨)
    /// "names a WITH query"라 쓰고, 대소문자를 무시하는 문서에서는 `fold`로 접은 이름을 실어 철자만
    /// 다른 같은 대상(`dupes`·`DUPES`)을 한 번만 센다. 철자가 다른데 증명되지 않은 후보면 "may
    /// name"과 두 철자를 그대로 실어 이름이 같다고 단정하지 않는다 — 같은 이름일 수도 다른 이름일
    /// 수도 있으므로 철자마다 따로 센다.
    pub(crate) fn message(
        &self,
        target: &str,
        candidate: &Candidate,
        outcome: Outcome,
    ) -> Option<String> {
        let reason = self.reason(candidate.matched, outcome)?;
        if candidate.matched == Match::Exact || outcome == Outcome::Cte {
            let target = printable(&self.fold(target));
            return Some(format!(
                "DML target '{target}' names a WITH query; {reason}"
            ));
        }
        Some(format!(
            "DML target '{}' may name the WITH query '{}'; {reason}",
            printable(target),
            printable(&candidate.cte)
        ))
    }

    /// 판정의 이유 문구다. 평범한 DML(`Table`)이면 없다.
    fn reason(&self, matched: Match, outcome: Outcome) -> Option<&'static str> {
        Some(match (outcome, self.rule, matched) {
            (Outcome::Table, _, _) => return None,
            (Outcome::Cte, _, _) if self.dialect == "mysql" => {
                "MySQL rejects a WITH query as an UPDATE or DELETE target (ER_NON_UPDATABLE_TABLE), so no write edge or column effect is recorded"
            }
            (Outcome::Cte, _, _) => {
                "SQL Server writes through it to a base relation that is not resolved, so no write edge or column effect is recorded"
            }
            (Outcome::Unproven, Rule::Cte, Match::AsciiCase) if self.dialect == "mysql" => {
                "the names differ only in letter case and the document does not record lower_case_table_names, which decides the case rule for WITH names; the write edge is kept, but target columns are not resolved"
            }
            (Outcome::Unproven, Rule::Cte, Match::AsciiCase) => {
                "the names differ only in letter case and the case rule for WITH names is not proven (SQL Server does not document which collation compares them); the write edge is kept, but target columns are not resolved"
            }
            (Outcome::Unproven, Rule::Cte, _) => {
                "the names are not byte-equal and contain non-ASCII characters, and whether the comparison folds them (letter case, accents, character width) is not proven; the write edge is kept, but target columns are not resolved"
            }
            (Outcome::Unproven, _, _) => {
                "this dialect's rule for such targets is unverified; the write edge is kept, but target columns are not resolved"
            }
        })
    }

    /// 진단에 싣고 중복을 가르는 이름이다.
    ///
    /// 대소문자를 무시하는 문서에서만 글자마다 소문자로 접는다. `str::to_lowercase`는 그리스어
    /// 어말 시그마 규칙을 적용해 `ΑΣ`를 `ας`로 바꾸므로, 그 규칙에서 같은 이름일 수 있는 `ΑΣ`·
    /// `ασ`가 두 이름으로 세어졌다. 글자 단위 `char::to_lowercase`는 문맥 없이 `Σ`를 `σ`로 바꾼다.
    /// 철자 그대로 비교하거나 규칙을 모르는 문서에서는 접지 않는다.
    fn fold(&self, name: &str) -> String {
        if self.case == NameCase::Insensitive {
            name.chars().flat_map(char::to_lowercase).collect()
        } else {
            name.to_owned()
        }
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
