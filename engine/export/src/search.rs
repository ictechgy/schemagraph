//! 이름 검색 결과를 에이전트 출력 계약의 JSON으로 만든다.

use schemagraph_analysis::search::{SearchHit, SearchReport};
use schemagraph_core::VertexKind;

/// 검색 결과를 얼마나 자세히 공개할지 정한다.
///
/// 큰 스키마에서 호출자가 먼저 이름만 훑고, 필요한 후보만 요약을 받도록
/// 두 단계로 나눈다 — 한 번에 모든 속성을 싣으면 토큰 예산을 낭비한다.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchDetail {
    /// 정점 id만 싣는다.
    Names,
    /// 종류·스키마·이름과 의존 이웃 수를 함께 싣는다.
    Summary,
}

impl SearchDetail {
    /// CLI·MCP가 공유하는 와이어 라벨이다.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Names => "names",
            Self::Summary => "summary",
        }
    }

    /// 와이어 라벨을 해석한다. 모르는 값은 `None`이다.
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "names" => Some(Self::Names),
            "summary" => Some(Self::Summary),
            _ => None,
        }
    }
}

/// 검색 결과를 JSON 값으로 만든다. 결과가 없어도 `limitations`를 실어
/// "그래프에 없음"과 "수집하지 못함"을 소비자가 구분하게 한다.
pub fn to_value(
    report: &SearchReport,
    pattern: &str,
    kind: Option<VertexKind>,
    detail: SearchDetail,
    limitations: &[String],
) -> serde_json::Value {
    let matches: Vec<serde_json::Value> = report
        .matches
        .iter()
        .map(|hit| match detail {
            SearchDetail::Names => serde_json::json!(hit.vertex.id.as_str()),
            SearchDetail::Summary => summary(hit),
        })
        .collect();
    let mut value = serde_json::json!({
        "detail": detail.as_str(),
        "limitations": limitations,
        "matches": matches,
        "pattern": pattern,
        "total": report.total,
        "truncated": report.truncated,
    });
    // 선택 필드는 지정됐을 때만 싣는 것이 출력 계약이다.
    if let Some(kind) = kind {
        value["kind"] = serde_json::json!(super::vertex_kind_str(kind));
    }
    value
}

/// 요약 단계의 한 건 — 이웃 수는 계산했을 때만 싣는다(없는 선택 필드는 키 생략).
fn summary(hit: &SearchHit) -> serde_json::Value {
    let mut value = serde_json::json!({
        "id": hit.vertex.id.as_str(),
        "kind": super::vertex_kind_str(hit.vertex.kind),
        "name": hit.vertex.name,
        "schema": hit.vertex.schema,
    });
    if let Some(counts) = hit.neighbors {
        value["dependencies"] = serde_json::json!(counts.dependencies);
        value["dependents"] = serde_json::json!(counts.dependents);
    }
    value
}
