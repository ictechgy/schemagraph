//! 트리를 만들기 전에 문장별 연산 사슬 길이를 세어, 분석 재귀가 스택을 넘지 않게 한다.
//!
//! sqlparser는 집합 연산과 이항 연산 사슬을 반복문으로 왼쪽 깊은 트리로 만든다. 그래서
//! 괄호·부질의 중첩에 거는 자체 재귀 한도(50)가 사슬에는 걸리지 않는다. 그 트리를
//! 방문·해시·비교·복제·해제하는 코드는 모두 재귀라 사슬 길이만큼 스택을 쓰고, 스택
//! 넘침은 catch_unwind로도 잡히지 않아 그래프도 limitation도 없이 프로세스가 죽는다.
//! 토큰만 세면 트리를 만들기 전에 거를 수 있고, 거른 몸체는 파싱 실패와 같은 경로로
//! 실제로 센 수를 담은 limitation이 된다.
//!
//! 쉼표로 나뉜 목록(VALUES 행, IN 목록, 인자), CASE 갈래, 세미콜론 없이 이어진 T-SQL
//! 문장은 sqlparser가 Vec으로 모으므로 원소가 아무리 많아도 트리가 깊어지지 않는다. 그래서
//! 이항 연산자는 문장 전체 합이 아니라 그런 경계로 끊긴 구간마다 세고, 괄호·CASE 안
//! 피연산자의 사슬은 바깥 구간에 더해 가장 긴 경로를 잰다.

use sqlparser::ast::Statement;
use sqlparser::dialect::Dialect;
use sqlparser::keywords::Keyword;
use sqlparser::parser::{Parser, ParserError, ParserOptions};
use sqlparser::tokenizer::{Token, TokenWithSpan, Tokenizer, Word};

/// 한 문장에 둘 수 있는 집합 연산자(UNION·INTERSECT·EXCEPT·MINUS) 수.
///
/// 이 상한을 두기 전에도 메인 스레드에서 1만 2천 항 UNION 뷰까지는 분석됐다. 그보다
/// 넉넉하게 잡되, 상한까지의 사슬을 CLI 분석 스레드 스택이 감당하는 크기로 둔다.
pub(crate) const MAX_SET_OPERATORS: usize = 20_000;

/// 한 연산자 사슬에 둘 수 있는 이항·후위 연산자 수.
///
/// 사슬은 같은 묶음 안의 쉼표·CASE 갈래·문장 경계에서 끊고, 괄호·CASE 안 피연산자의
/// 사슬은 바깥 사슬에 이어서 센다 — 트리 깊이의 상한이다. `id = 0 OR id = 1 …`처럼 항마다
/// 연산자가 둘이라, 상한을 두기 전에 분석되던 5만 항 OR 사슬도 이 상한 안에 든다.
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
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ChainCounts {
    /// 문장 안 집합 연산자의 합.
    pub(crate) set_operators: usize,
    /// 가장 긴 이항·후위 연산자 사슬. 괄호·CASE 안 피연산자의 사슬까지 이어서 센 값이다.
    pub(crate) infix_chain: usize,
}

/// `;`로 나뉜 문장마다 사슬 연산자를 센다.
///
/// 집합 연산자는 문장 안의 합을 센다. 이항 연산자는 같은 묶음(괄호·대괄호·중괄호·CASE·
/// 타입 꺾쇠) 안에서 쉼표·CASE 갈래·문장 경계로 나뉜 구간마다 세고, 구간 안에서 닫힌 안쪽
/// 묶음의 사슬을 그 구간에 더한다. 그래서 값은 트리 깊이의 상한이라 과소 계산하지 않으면서,
/// 원소가 많은 평평한 목록에서는 쌓이지 않는다. 값은 언제나 문장 전체의 연산자 합 이하라,
/// 합으로 상한 안이던 문장은 그대로 파싱된다. 단항 연산자는 sqlparser의 재귀 한도에
/// 걸리므로 피연산자 뒤에 온 기호만 이항으로 센다. 토큰마다 묶음 스택을 다시 훑지 않아
/// 토큰 수에 선형이다.
pub(crate) fn statement_chain_counts(tokens: &[TokenWithSpan]) -> Vec<ChainCounts> {
    let mut statements = Vec::new();
    let mut scan = ChainScan::default();
    for token in tokens.iter().map(|token| &token.token) {
        match token {
            Token::Whitespace(_) => {}
            Token::SemiColon => statements.push(std::mem::take(&mut scan).finish()),
            token => scan.step(token),
        }
    }
    statements.push(scan.finish());
    statements
}

/// 상한을 넘는 첫 문장을 그 문장에서 센 수와 함께 오류로 알린다.
fn check_chain_lengths(tokens: &[TokenWithSpan]) -> Result<(), ParserError> {
    for (index, counts) in statement_chain_counts(tokens).into_iter().enumerate() {
        let statement = index + 1;
        if counts.set_operators > MAX_SET_OPERATORS {
            let finding = format!(
                "statement {statement} has {} set operators",
                counts.set_operators
            );
            return Err(over_limit(finding, MAX_SET_OPERATORS));
        }
        if counts.infix_chain > MAX_INFIX_OPERATORS {
            let finding = format!(
                "statement {statement} has an operator chain of {} infix operators",
                counts.infix_chain
            );
            return Err(over_limit(finding, MAX_INFIX_OPERATORS));
        }
    }
    Ok(())
}

/// 센 값과 상한을 담은 파싱 오류. 기존 파싱 실패 경로가 그대로 limitation으로 옮긴다.
fn over_limit(finding: String, limit: usize) -> ParserError {
    ParserError::ParserError(format!(
        "{finding}, over the {limit} analysis limit; it was not parsed"
    ))
}

/// 사슬을 셀 때 여닫는 묶음의 종류.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum GroupKind {
    /// 문장 자체. 언제나 맨 아래에 있고 문장 끝에서만 닫는다.
    Statement,
    Paren,
    Bracket,
    Brace,
    /// `ARRAY<…>`·`STRUCT<…>` 타입 인자. 안의 쉼표는 필드 경계라 바깥 사슬을 끊지 않는다.
    /// sqlparser 0.58에서 괄호 없이 식 한가운데에 쉼표가 오는 문법은 이 타입 인자뿐이다 —
    /// sqlparser를 올릴 때 새 꺾쇠 문법이 생겼는지 다시 확인한다.
    Angle,
    /// `CASE … END`. WHEN·THEN·ELSE가 갈래 경계다.
    Case,
}

impl GroupKind {
    /// 종류별로 열린 묶음 수를 세는 배열의 크기.
    const COUNT: usize = 6;
}

/// 묶음 하나의 사슬 계수.
#[derive(Debug)]
struct Group {
    kind: GroupKind,
    /// 지금 구간에서 센 연산자 수와, 그 구간 안에서 닫힌 안쪽 묶음 사슬의 합.
    ///
    /// 안쪽 묶음은 최댓값이 아니라 합으로 더한다. 묶음 경계를 잘못 짚더라도 한 사슬의
    /// 연산자가 형제 묶음으로 갈라져 덜 세이지 않게 하기 위해서다.
    current: usize,
    /// 끝난 구간 중 가장 긴 사슬.
    longest: usize,
    /// 끝난 구간 사슬의 합. 구간 경계를 믿을 수 없게 되면 지금 구간에 모두 합친다.
    finished: usize,
    /// 쉼표·CASE 갈래에서 구간을 끊는가.
    splits: bool,
}

impl Group {
    /// 막 연 묶음 — 구간 하나가 비어 있고 쉼표 등에서 끊을 수 있다.
    fn new(kind: GroupKind) -> Self {
        Self {
            kind,
            current: 0,
            longest: 0,
            finished: 0,
            splits: true,
        }
    }

    /// 쉼표·CASE 갈래·문장 경계 — 앞뒤 구간은 형제 원소라 깊이가 쌓이지 않는다.
    fn end_segment(&mut self) {
        if !self.splits {
            return;
        }
        self.longest = self.longest.max(self.current);
        self.finished += self.current;
        self.current = 0;
    }

    /// 구간 경계를 더는 믿지 않는다 — 끝난 구간을 모두 지금 구간에 합치고 더 끊지 않는다.
    fn merge_segments(&mut self) {
        self.current += self.finished;
        self.finished = 0;
        self.splits = false;
    }

    /// 닫힌 묶음의 사슬 — 가장 긴 구간이다. 바깥 구간에 피연산자 하나로 더해진다.
    fn close(self) -> usize {
        // 합친 뒤에는 current가 끝난 구간의 합이라 longest보다 작지 않다.
        self.longest.max(self.current)
    }
}

/// 앞 토큰 뒤의 자리. CASE·문장 머리 키워드를 경계로 볼지 식별자로 볼지 정한다.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Operand {
    /// 피연산자가 확실히 끝났다 — 뒤에 오는 WHEN·THEN·ELSE·END와 문장 머리 키워드는
    /// 식을 잇지 못하므로 경계다.
    Complete,
    /// 피연산자가 와야 한다 — 뒤에 오는 CASE·문장 머리 키워드는 식별자다.
    Expected,
    /// 어느 쪽인지 모른다. 뒤에 피연산자를 받는 키워드(`AT TIME ZONE`, `NOT` 등)일 수 있다.
    Unknown,
}

/// 바로 앞 토큰 가운데, 다음 토큰의 구조 역할을 바꾸는 것.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Previous {
    #[default]
    Other,
    /// `.` 뒤의 낱말은 필드 이름이다.
    Period,
    /// `::` 뒤의 첫 낱말은 타입 이름이다.
    DoubleColon,
    /// `ARRAY`·`STRUCT` 뒤의 `<`는 타입 인자를 연다.
    TypeConstructor,
    /// 묶음을 연 `CASE` 바로 뒤의 WHEN은 sqlparser가 언제나 키워드로 읽는다.
    CaseStart,
}

impl Previous {
    /// 방금 받은 토큰이 다음 토큰에 주는 역할.
    fn of(token: &Token) -> Self {
        match token {
            Token::Period => Self::Period,
            Token::DoubleColon => Self::DoubleColon,
            Token::Word(word) if matches!(word.keyword, Keyword::ARRAY | Keyword::STRUCT) => {
                Self::TypeConstructor
            }
            _ => Self::Other,
        }
    }
}

/// 한 문장의 토큰을 차례로 받아 사슬 연산자를 세는 상태.
#[derive(Debug)]
struct ChainScan {
    set_operators: usize,
    /// 열린 묶음. 맨 아래는 언제나 Statement 묶음이다.
    groups: Vec<Group>,
    /// 종류별로 열린 묶음 수. 닫는 괄호가 짝을 찾으려고 스택을 훑지 않게 한다.
    open_by_kind: [usize; GroupKind::COUNT],
    /// 앞 토큰이 피연산자를 끝냈을 수 있는가 — 뒤 기호를 이항 연산자로 셀지 정한다.
    after_operand: bool,
    /// 앞 토큰 뒤의 자리 — CASE·문장 머리 키워드의 역할 판정에만 쓴다.
    operand: Operand,
    previous: Previous,
}

impl Default for ChainScan {
    fn default() -> Self {
        let mut open_by_kind = [0; GroupKind::COUNT];
        open_by_kind[GroupKind::Statement as usize] = 1;
        Self {
            set_operators: 0,
            groups: vec![Group::new(GroupKind::Statement)],
            open_by_kind,
            after_operand: false,
            operand: Operand::Unknown,
            previous: Previous::Other,
        }
    }
}

impl ChainScan {
    /// 공백이 아닌 토큰 하나를 센다.
    fn step(&mut self, token: &Token) {
        self.count(token);
        let open_cases = self.open_by_kind[GroupKind::Case as usize];
        self.operand = self.structure(token);
        self.after_operand = ends_operand(token);
        self.previous = if self.open_by_kind[GroupKind::Case as usize] > open_cases {
            Previous::CaseStart
        } else {
            Previous::of(token)
        };
    }

    /// 집합 연산자는 문장 합에, 이항·후위 연산자는 지금 구간에 더한다.
    fn count(&mut self, token: &Token) {
        if matches!(token, Token::Word(word) if is_set_operator(word.keyword)) {
            self.set_operators += 1;
        } else if is_infix_token(token, self.after_operand) {
            self.top().current += 1;
        }
    }

    /// 묶음을 여닫고 구간을 끊은 뒤, 이 토큰 뒤의 자리를 돌려준다.
    fn structure(&mut self, token: &Token) -> Operand {
        match token {
            Token::LParen => self.open(GroupKind::Paren),
            Token::LBracket => self.open(GroupKind::Bracket),
            Token::LBrace => self.open(GroupKind::Brace),
            Token::Lt if self.previous == Previous::TypeConstructor => self.open(GroupKind::Angle),
            Token::RParen => self.close_through(GroupKind::Paren),
            Token::RBracket => self.close_through(GroupKind::Bracket),
            Token::RBrace => self.close_through(GroupKind::Brace),
            Token::Gt => self.close_angles(1),
            Token::ShiftRight => self.close_angles(2),
            Token::Comma => {
                self.top().end_segment();
                Operand::Expected
            }
            Token::Period => Operand::Expected,
            Token::Word(word) => self.word(word),
            token => operand_after_symbol(token),
        }
    }

    /// 낱말 뒤의 자리. 식별자, `.` 뒤 필드 이름, `::` 뒤 타입 이름은 피연산자를 끝낸다.
    /// 그 밖의 키워드는 뒤에 피연산자를 받을 수 있어 끝났다고 보지 않는다.
    fn word(&mut self, word: &Word) -> Operand {
        let named = matches!(self.previous, Previous::Period | Previous::DoubleColon);
        if word.keyword == Keyword::NoKeyword || named {
            return Operand::Complete;
        }
        match word.keyword {
            // 타입 꺾쇠 안의 `case`는 필드 이름이다.
            Keyword::CASE if self.top_kind() != GroupKind::Angle => self.open(GroupKind::Case),
            Keyword::WHEN | Keyword::THEN | Keyword::ELSE | Keyword::END => {
                self.case_keyword(word.keyword)
            }
            Keyword::NULL | Keyword::TRUE | Keyword::FALSE => Operand::Complete,
            keyword if is_statement_keyword(keyword) => self.statement_keyword(),
            _ => Operand::Unknown,
        }
    }

    /// 문장 머리 키워드. 피연산자가 확실히 끝난 뒤라면 세미콜론 없이 이어진 다음 문장이나
    /// 절이 시작된다(T-SQL 블록·IF 안의 문장 목록) — 앞뒤 식은 형제라 구간을 끊는다.
    /// 연산자 뒤처럼 피연산자를 기다리는 자리면 sqlparser가 식별자로 받을 수 있어 끊지 않는다.
    fn statement_keyword(&mut self) -> Operand {
        if self.operand == Operand::Complete {
            self.top().end_segment();
        }
        Operand::Unknown
    }

    /// WHEN·THEN·ELSE·END. sqlparser는 이 키워드도 식별자로 받으므로, 피연산자가 확실히
    /// 끝난 뒤에만 경계로 보고 피연산자를 기다리는 자리면 식별자로 본다. 사슬 한가운데의
    /// `then`으로 사슬을 끊으면 깊이를 덜 세기 때문이다. CASE 밖에서는 T-SQL `IF … ELSE`·
    /// 블록 `END`·MERGE `WHEN` 같은 문장·절 경계라 같은 묶음의 구간을 끊는다.
    fn case_keyword(&mut self, keyword: Keyword) -> Operand {
        if keyword == Keyword::WHEN && self.previous == Previous::CaseStart {
            // `CASE WHEN end THEN …`의 `end`는 조건 피연산자다. 이 WHEN을 식별자로 보면
            // 뒤 `end`를 CASE 끝으로 읽어 묶음을 일찍 닫는다.
            return Operand::Expected;
        }
        let in_case = self.top_kind() == GroupKind::Case;
        match (self.operand, keyword) {
            (Operand::Expected, _) => Operand::Complete,
            (Operand::Complete, Keyword::END) => {
                if in_case {
                    self.close_top();
                } else {
                    self.top().end_segment();
                }
                Operand::Complete
            }
            (Operand::Complete, _) => {
                self.top().end_segment();
                Operand::Expected
            }
            (Operand::Unknown, Keyword::END) => {
                if in_case {
                    self.distrust_case_ends();
                }
                Operand::Unknown
            }
            (Operand::Unknown, _) => Operand::Unknown,
        }
    }

    /// 맨 위 CASE 묶음이 이 END에서 끝났는지 알 수 없다. 끝났다면 뒤 토큰은 바깥 사슬인데
    /// 안쪽 묶음의 마지막 갈래로 들어가, 바깥 사슬과 앞 갈래를 잇는 경로를 덜 센다. 그래서
    /// 맨 위에 이어진 CASE 묶음들의 갈래를 모두 합쳐 어느 쪽이든 덜 세지 않게 한다. 괄호
    /// 너머의 CASE는 닫는 괄호가 함께 닫으므로 건드리지 않는다.
    ///
    /// 이미 합친 묶음을 만나면 멈춘다. 합칠 때 그 아래로 이어진 CASE 묶음도 모두 합쳤고,
    /// 합친 묶음은 다시 구간을 끊지 않으므로 결과가 같다 — 그래서 END가 많아도 묶음마다
    /// 한 번만 합쳐 토큰 수에 선형이다.
    fn distrust_case_ends(&mut self) {
        for group in self.groups.iter_mut().rev() {
            #[cfg(test)]
            count_group_visit();
            if group.kind != GroupKind::Case || !group.splits {
                break;
            }
            group.merge_segments();
        }
    }

    /// 묶음을 연다. 여는 기호 바로 뒤에는 피연산자가 와야 한다.
    fn open(&mut self, kind: GroupKind) -> Operand {
        self.groups.push(Group::new(kind));
        self.open_by_kind[kind as usize] += 1;
        Operand::Expected
    }

    /// 닫는 괄호와 짝인 묶음까지 닫는다. 그 사이에 END·`>` 없이 남은 CASE·꺾쇠 묶음도 함께
    /// 닫는다. 짝이 없는 닫는 괄호는 파싱 오류 자리라 묶음을 건드리지 않는다. 짝 유무는
    /// 종류별 수로 알아, 닫는 괄호마다 스택을 훑지 않는다.
    fn close_through(&mut self, kind: GroupKind) -> Operand {
        if self.open_by_kind[kind as usize] > 0 {
            while self.close_top() != kind {}
        }
        Operand::Complete
    }

    /// 맨 위의 꺾쇠 묶음을 `>`는 하나, `>>`는 둘까지 닫는다. 닫지 못했으면 비교·시프트
    /// 연산자다.
    fn close_angles(&mut self, count: usize) -> Operand {
        let mut closed = false;
        for _ in 0..count {
            if self.top_kind() != GroupKind::Angle {
                break;
            }
            self.close_top();
            closed = true;
        }
        if closed {
            Operand::Complete
        } else {
            Operand::Expected
        }
    }

    /// 맨 위 묶음을 닫아 그 사슬을 바깥 구간에 더하고, 닫은 묶음의 종류를 돌려준다.
    /// 부르는 쪽은 맨 위가 Statement 묶음이 아님을 확인한 뒤에만 부른다.
    fn close_top(&mut self) -> GroupKind {
        #[cfg(test)]
        count_group_visit();
        let group = self.groups.pop().expect("a nested group is open");
        let kind = group.kind;
        self.open_by_kind[kind as usize] -= 1;
        let chain = group.close();
        self.top().current += chain;
        kind
    }

    /// 문장 끝 — 닫히지 않은 묶음을 모두 닫아 가장 긴 사슬을 낸다.
    fn finish(mut self) -> ChainCounts {
        while self.groups.len() > 1 {
            self.close_top();
        }
        let statement = self.groups.pop().expect("the statement group stays open");
        ChainCounts {
            set_operators: self.set_operators,
            infix_chain: statement.close(),
        }
    }

    /// 지금 토큰이 속한 맨 안쪽 묶음.
    fn top(&mut self) -> &mut Group {
        // groups는 Statement 묶음을 늘 맨 아래에 둔다 — close_top은 그 위만 닫고, 그 묶음을
        // 꺼내는 finish는 상태를 소비한다.
        self.groups
            .last_mut()
            .expect("the statement group stays open")
    }

    /// 맨 안쪽 묶음의 종류.
    fn top_kind(&self) -> GroupKind {
        self.groups
            .last()
            .map_or(GroupKind::Statement, |group| group.kind)
    }
}

/// 기호 토큰 뒤의 자리. 후위 `!`와 와일드카드 `*`는 피연산자를 끝낼 수도 있다.
fn operand_after_symbol(token: &Token) -> Operand {
    if is_literal(token) || matches!(token, Token::Number(..) | Token::Placeholder(_)) {
        Operand::Complete
    } else if matches!(token, Token::ExclamationMark | Token::Mul) || !is_operator_symbol(token) {
        Operand::Unknown
    } else {
        Operand::Expected
    }
}

/// 이 토큰이 이항·후위 연산자 자리인가(집합 연산자는 따로 센다).
fn is_infix_token(token: &Token, after_operand: bool) -> bool {
    match token {
        Token::Word(word) => is_infix_keyword(word.keyword),
        // PostgreSQL의 후위 `!`(팩토리얼)는 `x ! !`처럼 연산자 뒤에 와도 사슬을 잇는다.
        Token::ExclamationMark => true,
        token => after_operand && is_operator_symbol(token),
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

/// 세미콜론 없이 다음 문장을 여는 문장 머리 키워드(T-SQL 블록·IF·WHILE 안에서 흔한 것).
///
/// sqlparser 0.58에서 이 키워드는 중위·후위 연산자나 타입 이름의 뒷부분으로 쓰이지 않아,
/// 피연산자가 끝난 뒤에 오면 앞 식이 거기서 끝난다. `WITH`는 `x::timestamp with time zone`
/// 처럼 타입 이름을 이어 사슬 한가운데에 올 수 있으므로 넣지 않는다 — sqlparser를 올릴 때
/// 이 키워드가 식을 잇는 문법이 생겼는지 다시 확인한다.
fn is_statement_keyword(keyword: Keyword) -> bool {
    matches!(
        keyword,
        Keyword::SELECT
            | Keyword::INSERT
            | Keyword::UPDATE
            | Keyword::DELETE
            | Keyword::MERGE
            | Keyword::SET
            | Keyword::DECLARE
            | Keyword::IF
            | Keyword::WHILE
            | Keyword::BEGIN
            | Keyword::RETURN
            | Keyword::EXEC
            | Keyword::EXECUTE
            | Keyword::PRINT
            | Keyword::RAISERROR
            | Keyword::TRUNCATE
            | Keyword::OPEN
            | Keyword::CLOSE
            | Keyword::FETCH
            | Keyword::DEALLOCATE
            | Keyword::COMMIT
            | Keyword::ROLLBACK
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
thread_local! {
    /// 묶음 스택에서 꺼내거나 훑은 묶음 수. 테스트는 스레드마다 병렬로 돌므로 스레드마다 둔다.
    static GROUP_VISITS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// 묶음 하나를 꺼내거나 훑었다고 센다.
#[cfg(test)]
fn count_group_visit() {
    GROUP_VISITS.with(|visits| visits.set(visits.get() + 1));
}

/// 현재 테스트 스레드의 묶음 방문 수를 돌려주고 0으로 되돌린다.
#[cfg(test)]
fn take_group_visits() -> usize {
    GROUP_VISITS.with(std::cell::Cell::take)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlparser::dialect::{GenericDialect, MsSqlDialect, PostgreSqlDialect};

    /// 문장마다 (집합 연산자 수, 가장 긴 이항·후위 연산자 사슬).
    fn counts(dialect: &dyn Dialect, sql: &str) -> Vec<(usize, usize)> {
        let tokens = Tokenizer::new(dialect, sql)
            .tokenize_with_location()
            .expect("tokens");
        statement_chain_counts(&tokens)
            .into_iter()
            .map(|counts| (counts.set_operators, counts.infix_chain))
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
    fn 쉼표와_case_갈래로_나뉜_평평한_목록은_사슬이_쌓이지_않는다() {
        let postgres = PostgreSqlDialect {};
        // 다중 행 VALUES — 행과 값은 sqlparser가 Vec으로 모으는 형제 원소다.
        assert_eq!(
            counts(
                &postgres,
                "INSERT INTO t (id, a) VALUES (0::integer, 'v0'::text), (1::integer, 'v1'::text), (2::integer, 'v2'::text)"
            ),
            [(0, 1)]
        );
        // CASE 갈래의 조건·결과도 갈래마다 따로 파싱된다.
        assert_eq!(
            counts(
                &postgres,
                "SELECT CASE WHEN a = 'k0' THEN 'v0' WHEN a = 'k1' THEN 'v1'::text ELSE NULL END AS a FROM t"
            ),
            [(0, 1)]
        );
        // 인자·IN 목록 안의 식은 바깥 사슬에 가장 긴 원소 하나만 더한다.
        assert_eq!(
            counts(
                &postgres,
                "SELECT f(a + b, c * d) + 1 FROM t WHERE x IN (a + 1, b - 2, c / 3)"
            ),
            [(0, 4)]
        );
        // 짝이 맞지 않는 괄호도 문장 끝에서 닫아 센다.
        assert_eq!(
            counts(&postgres, "SELECT a + b) + c; SELECT (a + (b + c"),
            [(0, 2), (0, 2)]
        );
    }

    #[test]
    fn 괄호와_case_안의_사슬은_바깥_사슬에_이어서_센다() {
        let generic = GenericDialect {};
        // 안쪽 괄호의 사슬은 바깥 사슬의 피연산자라 깊이가 더해진다.
        assert_eq!(counts(&generic, "SELECT ((a + b) + c) + d"), [(0, 3)]);
        // CASE 식 하나는 바깥 사슬의 피연산자다 — WHEN·THEN·END가 바깥 사슬을 끊지 않는다.
        assert_eq!(
            counts(&generic, "SELECT x + CASE WHEN a THEN b + c END + y"),
            [(0, 3)]
        );
        // sqlparser는 CASE 키워드도 식별자로 받는다. 연산자 뒤의 `then`·`end`는 갈래
        // 경계가 아니라 사슬의 피연산자다.
        assert_eq!(
            counts(
                &generic,
                "SELECT CASE WHEN c THEN x + then + end + y ELSE 0 END"
            ),
            [(0, 3)]
        );
        // `CASE` 바로 뒤의 WHEN은 키워드라, 조건 자리의 `end`는 CASE 끝이 아니라 피연산자다.
        assert_eq!(
            counts(&generic, "SELECT x + CASE WHEN end THEN a + b END + y"),
            [(0, 3)]
        );
        // 타입 꺾쇠 안의 쉼표는 필드 경계라 바깥 사슬을 끊지 않는다.
        assert_eq!(
            counts(
                &generic,
                "SELECT a + b + NULL::STRUCT<p INT, q INT> + c + d"
            ),
            [(0, 6)]
        );
    }

    #[test]
    fn 세미콜론_없이_이어진_문장은_문장마다_사슬을_끊는다() {
        let mssql = MsSqlDialect {};
        // T-SQL 블록·IF·ELSE 안의 문장은 sqlparser가 형제 목록으로 모은다.
        assert_eq!(
            counts(
                &mssql,
                "IF @x = 1 BEGIN UPDATE t SET a = a + 1 WHERE id = 1 \
                 UPDATE t SET b = b + 1 WHERE id = 2 END ELSE SET @y = @y + 1"
            ),
            [(0, 3)]
        );
        // 연산자 뒤의 문장 머리 키워드는 sqlparser가 식별자로 받을 수 있어 사슬을 끊지 않는다.
        assert_eq!(
            counts(&mssql, "SELECT x + update + y + set + z FROM t"),
            [(0, 4)]
        );
    }

    #[test]
    fn 짝_없는_닫는_괄호와_end가_많아도_묶음_스택을_다시_훑지_않는다() {
        // 닫는 괄호마다 짝을 찾으려고, 키워드 뒤 END마다 이어진 CASE를 합치려고 스택 전체를
        // 훑으면 토큰 수의 제곱이 된다. 묶음은 닫을 때와 처음 합칠 때만 본다.
        let groups = 20_000;
        let sql = format!(
            "SELECT {}{} FROM t; SELECT {}{}",
            "[".repeat(groups),
            ")".repeat(groups),
            "CASE ".repeat(groups),
            "NOT END ".repeat(groups)
        );
        take_group_visits();
        assert_eq!(counts(&GenericDialect {}, &sql).len(), 2);
        let visits = take_group_visits();
        assert!(
            visits <= 5 * groups,
            "visited {visits} groups for {groups} nested groups"
        );
    }

    #[test]
    fn 괄호마다_상한_안이어도_이어진_사슬이_상한을_넘으면_거른다() {
        // 괄호 한 겹마다 6만 개씩이라도 트리 깊이는 둘의 합이다.
        let inner = format!("(x{})", " + 1".repeat(60_000));
        let sql = format!("SELECT a FROM t WHERE {inner}{} = 0", " + 1".repeat(60_000));
        let error = parse_sql(&GenericDialect {}, &sql).expect_err("nested chain over the limit");
        assert_eq!(
            error.to_string(),
            format!(
                "sql parser error: statement 1 has an operator chain of 120001 infix operators, over the {MAX_INFIX_OPERATORS} analysis limit; it was not parsed"
            )
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
                "sql parser error: statement 2 has {} set operators, over the {} analysis limit; it was not parsed",
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
