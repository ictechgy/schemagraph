//! 그래프에 부착된 카탈로그 사실을 보수적으로 검사한다.
//!
//! 이 모듈은 인덱스를 만들거나 지우라는 운영 판단을 내리지 않는다. 완전한
//! 카탈로그에서 관찰한 외래 키와 인덱스의 순서 관계, 그리고 구조화된 SQL
//! 진단만 보고한다.

use schemagraph_core::{Diagnostic, Graph, SourceLocation, VertexId};

/// lint 결과가 카탈로그 사실로 확정되었는지 나타낸다.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum LintStatus {
    /// 완전한 메타데이터에서 관찰된 사실이다.
    Confirmed,
    /// 메타데이터가 부족하거나 분석이 모호해 확정할 수 없다.
    Unverified,
}

impl LintStatus {
    /// JSON·CLI가 공유하는 안정적인 상태 라벨이다.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Confirmed => "confirmed",
            Self::Unverified => "unverified",
        }
    }
}

/// 한 건의 사실 기반 lint 결과.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LintFinding {
    /// 규칙의 안정적인 이름이다.
    pub rule: String,
    /// 이 finding을 확정할 수 있었는지 나타낸다.
    pub status: LintStatus,
    /// FK 제약 또는 분석 대상 객체의 정점 id다.
    pub subject: VertexId,
    /// FK finding이면 FK가 속한 테이블이다.
    pub table: Option<VertexId>,
    /// FK finding이면 FK의 로컬 키 컬럼들이다.
    pub columns: Vec<VertexId>,
    /// 구조화된 분석 진단 코드다.
    pub code: Option<String>,
    /// 원문 진단 또는 메타데이터 사실의 설명이다.
    pub message: String,
    /// 분석 진단의 원문 위치다.
    pub location: Option<SourceLocation>,
}

/// schema lint의 결정적 보고서.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LintReport {
    /// 출력된 finding 목록이다.
    pub findings: Vec<LintFinding>,
    /// 출력 제한 전 전체 finding 수다.
    pub total_findings: usize,
    /// `confirmed` 상태인 전체 finding 수다.
    pub confirmed_count: usize,
    /// 출력 제한으로 finding을 생략했는지 나타낸다.
    pub truncated: bool,
    /// 메타데이터 부재·불완전 등 소비자가 고려할 한계다.
    pub limitations: Vec<String>,
    /// 모든 검사 입력과 출력이 완전하게 관찰되었는지 나타낸다.
    pub complete: bool,
}

/// 외래 키 컬럼의 ordered key prefix와 일치하는 완전·비부분 인덱스를 찾는다.
fn has_unfiltered_prefix(
    metadata: &schemagraph_core::SchemaMetadata,
    table: &VertexId,
    columns: &[VertexId],
) -> (bool, bool) {
    let mut incomplete = false;
    let found = metadata.indexes.values().any(|index| {
        if &index.table != table {
            return false;
        }
        if !index.complete {
            incomplete = true;
            return false;
        }
        !index.has_predicate && index.columns.starts_with(columns)
    });
    (found, incomplete)
}

/// 테이블 PK 컬럼 순서가 완전하게 관찰됐고 FK가 그 prefix인지 확인한다.
fn has_primary_key_prefix(
    metadata: &schemagraph_core::SchemaMetadata,
    table: &VertexId,
    columns: &[VertexId],
) -> (bool, bool) {
    let mut primary: Vec<_> = metadata
        .columns
        .iter()
        .filter(|(id, column)| id.parent().as_ref() == Some(table) && column.pk_position > 0)
        .map(|(id, column)| (column.pk_position, id.clone()))
        .collect();
    let has_table_columns = metadata
        .columns
        .keys()
        .any(|id| id.parent().as_ref() == Some(table));
    if !has_table_columns || primary.is_empty() {
        return (false, true);
    }
    primary.sort_by_key(|(position, _)| *position);
    let positions_complete = primary
        .iter()
        .enumerate()
        .all(|(index, (position, _))| *position == index as u32 + 1);
    if !positions_complete {
        return (false, true);
    }
    let primary_columns: Vec<_> = primary.into_iter().map(|(_, id)| id).collect();
    (primary_columns.starts_with(columns), false)
}

/// 외래 키의 인덱스 prefix 사실을 보고한다.
fn foreign_key_findings(
    graph: &Graph,
    metadata: &schemagraph_core::SchemaMetadata,
    findings: &mut Vec<LintFinding>,
    complete: &mut bool,
) {
    for (subject, foreign_key) in &metadata.foreign_keys {
        if !foreign_key.complete || foreign_key.columns.is_empty() {
            *complete = false;
            findings.push(LintFinding {
                rule: "fk-index-prefix".into(),
                status: LintStatus::Unverified,
                subject: subject.clone(),
                table: Some(foreign_key.table.clone()),
                columns: foreign_key.columns.clone(),
                code: None,
                message: "foreign-key metadata is incomplete; index coverage is unverified".into(),
                location: None,
            });
            continue;
        }
        if graph.vertex(&foreign_key.table).is_none() {
            *complete = false;
            findings.push(LintFinding {
                rule: "fk-index-prefix".into(),
                status: LintStatus::Unverified,
                subject: subject.clone(),
                table: Some(foreign_key.table.clone()),
                columns: foreign_key.columns.clone(),
                code: None,
                message:
                    "foreign-key table is not present in the graph; index coverage is unverified"
                        .into(),
                location: None,
            });
            continue;
        }
        let (found, incomplete_index) =
            has_unfiltered_prefix(metadata, &foreign_key.table, &foreign_key.columns);
        let (primary_found, incomplete_primary) =
            has_primary_key_prefix(metadata, &foreign_key.table, &foreign_key.columns);
        if found || primary_found {
            continue;
        }
        let status = if incomplete_index || incomplete_primary {
            *complete = false;
            LintStatus::Unverified
        } else {
            LintStatus::Confirmed
        };
        findings.push(LintFinding {
            rule: "fk-index-prefix".into(),
            status,
            subject: subject.clone(),
            table: Some(foreign_key.table.clone()),
            columns: foreign_key.columns.clone(),
            code: None,
            message: "no unfiltered index or primary-key prefix observed; partial indexes may still be useful".into(),
            location: None,
        });
    }
}

/// 구조화된 unresolved/ambiguous 진단만 lint finding으로 표면화한다.
fn diagnostic_findings(graph: &Graph, findings: &mut Vec<LintFinding>) {
    for (subject, analysis) in graph.analysis() {
        for diagnostic in &analysis.diagnostics {
            if !is_reference_diagnostic(&diagnostic.code) {
                continue;
            }
            findings.push(diagnostic_finding(subject, diagnostic));
        }
    }
}

fn is_reference_diagnostic(code: &str) -> bool {
    code.ends_with("_UNRESOLVED") || code.ends_with("_AMBIGUOUS")
}

fn diagnostic_finding(subject: &VertexId, diagnostic: &Diagnostic) -> LintFinding {
    let rule = if diagnostic.code.ends_with("_AMBIGUOUS") {
        "ambiguous-reference"
    } else {
        "unresolved-reference"
    };
    LintFinding {
        rule: rule.into(),
        status: LintStatus::Unverified,
        subject: subject.clone(),
        table: None,
        columns: Vec::new(),
        code: Some(diagnostic.code.clone()),
        message: diagnostic.message.clone(),
        location: diagnostic.location.clone(),
    }
}

fn finding_order(finding: &LintFinding) -> (&str, &str, &str, &str) {
    (
        &finding.rule,
        finding.subject.as_str(),
        finding.code.as_deref().unwrap_or(""),
        &finding.message,
    )
}

/// 그래프의 schema metadata와 구조화된 분석 진단을 보수적으로 lint한다.
pub fn lint(graph: &Graph, max_results: usize) -> LintReport {
    let mut findings = Vec::new();
    let mut complete = true;
    let mut limitations = graph.limitations().to_vec();
    match graph.schema_metadata() {
        Some(metadata) => foreign_key_findings(graph, metadata, &mut findings, &mut complete),
        None => {
            complete = false;
            limitations.push("schema metadata unavailable; FK index coverage is unverified".into());
        }
    }
    diagnostic_findings(graph, &mut findings);
    findings.sort_by(|a, b| finding_order(a).cmp(&finding_order(b)));
    findings.dedup();
    let total_findings = findings.len();
    let confirmed_count = findings
        .iter()
        .filter(|finding| finding.status == LintStatus::Confirmed)
        .count();
    let truncated = total_findings > max_results;
    findings.truncate(max_results);
    limitations.sort();
    limitations.dedup();
    LintReport {
        findings,
        total_findings,
        confirmed_count,
        truncated,
        limitations,
        complete: complete && !truncated,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use schemagraph_core::{
        ColumnMetadata, ForeignKeyMetadata, Graph, IndexMetadata, SchemaMetadata, Vertex,
        VertexKind,
    };
    use std::collections::BTreeMap;

    fn id(raw: &str) -> VertexId {
        VertexId::from_raw(raw)
    }

    fn graph_with_metadata(metadata: Option<SchemaMetadata>) -> Graph {
        let mut graph = Graph::new();
        for (raw, kind, name, schema) in [
            ("app", VertexKind::Schema, "app", "app"),
            ("app.orders", VertexKind::Table, "orders", "app"),
            (
                "app.orders.customer_id",
                VertexKind::Column,
                "customer_id",
                "app",
            ),
            ("app.orders.id", VertexKind::Column, "id", "app"),
            (
                "app.orders.orders_fk",
                VertexKind::Constraint,
                "orders_fk",
                "app",
            ),
            (
                "app.orders.orders_idx",
                VertexKind::Index,
                "orders_idx",
                "app",
            ),
        ] {
            graph.add_vertex(Vertex {
                id: id(raw),
                kind,
                name: name.into(),
                schema: schema.into(),
            });
        }
        if let Some(metadata) = metadata {
            graph.set_schema_metadata(metadata);
        }
        graph
    }

    fn metadata(index: Option<IndexMetadata>, complete: bool) -> SchemaMetadata {
        let table = id("app.orders");
        let column = id("app.orders.customer_id");
        let mut columns = BTreeMap::new();
        columns.insert(
            column.clone(),
            ColumnMetadata {
                data_type: "INTEGER".into(),
                nullable: false,
                ordinal: 2,
                pk_position: 0,
            },
        );
        let mut foreign_keys = BTreeMap::new();
        foreign_keys.insert(
            id("app.orders.orders_fk"),
            ForeignKeyMetadata {
                table: table.clone(),
                columns: vec![column],
                target_table: Some(id("app.customers")),
                target_columns: vec![id("app.customers.id")],
                complete,
            },
        );
        let indexes = index
            .map(|value| (id("app.orders.orders_idx"), value))
            .into_iter()
            .collect();
        SchemaMetadata {
            columns,
            indexes,
            foreign_keys,
        }
    }

    #[test]
    fn composite_prefix_order_is_factual_and_partial_is_not_confirmed() {
        let table = id("app.orders");
        let first = id("app.orders.customer_id");
        let second = id("app.orders.id");
        let good = IndexMetadata {
            table: table.clone(),
            columns: vec![first.clone(), second],
            unique: false,
            has_predicate: false,
            complete: true,
        };
        assert!(
            lint(&graph_with_metadata(Some(metadata(Some(good), true))), 10)
                .findings
                .is_empty()
        );
        let partial = IndexMetadata {
            table,
            columns: vec![first],
            unique: false,
            has_predicate: true,
            complete: true,
        };
        let report = lint(
            &graph_with_metadata(Some(metadata(Some(partial), true))),
            10,
        );
        assert_eq!(report.confirmed_count, 0);
        assert_eq!(report.findings[0].status, LintStatus::Unverified);
        assert!(!report.complete);
        assert!(report.findings[0].message.contains("partial indexes"));
    }

    #[test]
    fn incomplete_metadata_and_missing_metadata_are_unverified() {
        let incomplete = IndexMetadata {
            table: id("app.orders"),
            columns: vec![],
            unique: false,
            has_predicate: false,
            complete: false,
        };
        let report = lint(
            &graph_with_metadata(Some(metadata(Some(incomplete), true))),
            10,
        );
        assert_eq!(report.confirmed_count, 0);
        assert!(!report.complete);
        assert_eq!(report.findings[0].status, LintStatus::Unverified);
        let unavailable = lint(&graph_with_metadata(None), 10);
        assert!(unavailable.findings.is_empty());
        assert!(!unavailable.complete);
        assert!(unavailable
            .limitations
            .iter()
            .any(|note| note.contains("metadata unavailable")));
    }

    #[test]
    fn primary_key_prefix_covers_fk_and_reversed_or_missing_order_does_not() {
        let first = id("app.orders.id");
        let second = id("app.orders.customer_id");
        let mut metadata = metadata(None, true);
        metadata.columns.insert(
            first.clone(),
            ColumnMetadata {
                data_type: "INTEGER".into(),
                nullable: false,
                ordinal: 1,
                pk_position: 1,
            },
        );
        metadata.columns.insert(
            second.clone(),
            ColumnMetadata {
                data_type: "INTEGER".into(),
                nullable: false,
                ordinal: 2,
                pk_position: 2,
            },
        );
        let fk = metadata
            .foreign_keys
            .get_mut(&id("app.orders.orders_fk"))
            .unwrap();
        fk.columns = vec![first.clone(), second.clone()];
        let report = lint(&graph_with_metadata(Some(metadata.clone())), 10);
        assert!(
            report.findings.is_empty(),
            "PK prefix should cover FK: {report:?}"
        );

        let fk = metadata
            .foreign_keys
            .get_mut(&id("app.orders.orders_fk"))
            .unwrap();
        fk.columns.reverse();
        let report = lint(&graph_with_metadata(Some(metadata.clone())), 10);
        assert_eq!(report.confirmed_count, 1);
        assert!(report.findings[0].message.contains("primary-key prefix"));

        metadata.columns.get_mut(&first).unwrap().pk_position = 0;
        let report = lint(&graph_with_metadata(Some(metadata)), 10);
        assert_eq!(report.findings[0].status, LintStatus::Unverified);
        assert!(!report.complete);
    }

    #[test]
    fn diagnostics_are_structured_and_result_limit_cannot_hide_counts() {
        let mut graph = graph_with_metadata(Some(metadata(None, false)));
        graph.set_analysis(
            id("app.orders"),
            schemagraph_core::ObjectAnalysis {
                state: schemagraph_core::AnalysisState::Partial,
                scope: "object-dependencies".into(),
                body_hash: None,
                diagnostics: vec![Diagnostic {
                    code: "SG_COLUMN_AMBIGUOUS".into(),
                    message: "ambiguous column".into(),
                    location: None,
                }],
                source: None,
            },
        );
        let report = lint(&graph, 1);
        assert_eq!(report.total_findings, 2);
        assert_eq!(report.findings.len(), 1);
        assert!(report.truncated);
        assert!(!report.complete);
        assert!(report
            .findings
            .iter()
            .all(|finding| !finding.message.to_lowercase().contains("drop")));
    }
}
