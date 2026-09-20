//! 해석의 성공 범위와 원문 근거를 그래프와 함께 보존한다.

/// 명시한 분석 범위에서 확인한 상태다. 실행 시점 SQL 전체의 안전성을 뜻하지 않는다.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum AnalysisState {
    /// 지원한 문법의 참조를 모두 해석했다.
    Complete,
    /// 일부 참조를 확인했지만 누락되거나 모호한 부분이 있다.
    Partial,
    /// 몸체가 없거나 언어를 지원하지 않아 해당 분석을 수행하지 못했다.
    Unsupported,
}

/// 원문 기준 위치만 허용한다. 재작성한 SQL의 위치를 원문 위치로 오인하지 않는다.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct SourceLocation {
    pub line: u64,
    pub column: u64,
    pub end_line: u64,
    pub end_column: u64,
}

/// 메시지 번역과 무관하게 소비자가 누락 원인을 분류하도록 안정적인 코드를 제공한다.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Diagnostic {
    pub code: String,
    pub message: String,
    pub location: Option<SourceLocation>,
}

/// 객체별 분석 범위를 보존해 미지원과 성공한 빈 결과를 구별한다.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjectAnalysis {
    pub state: AnalysisState,
    pub scope: String,
    pub body_hash: Option<String>,
    pub diagnostics: Vec<Diagnostic>,
    /// 파일 생산자가 준 상대 경로다. 절대 경로나 연결 문자열은 넣지 않는다.
    pub source: Option<String>,
}

/// 같은 간선의 서로 다른 SQL 출처를 원문 해시와 위치로 식별한다.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Origin {
    pub body_hash: String,
    /// value, predicate, relation, call 등 수집한 의미를 설명한다.
    pub role: String,
    pub location: Option<SourceLocation>,
}
