//! 그래프에 부착된 카탈로그 사실을 보수적으로 검사한다.
//!
//! 이 모듈은 인덱스를 만들거나 지우라는 운영 판단을 내리지 않는다. 완전한
//! 카탈로그에서 관찰한 외래 키와 인덱스의 순서 관계, 그리고 구조화된 SQL
//! 진단만 보고한다.

use schemagraph_core::{Diagnostic, Graph, SchemaMetadata, SourceLocation, VertexId, VertexKind};
use std::collections::BTreeMap;

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

/// FK 컬럼과 참조 컬럼의 선언 타입 문자열이 다른지 보고한다.
///
/// 수집기가 옮긴 선언 타입을 그대로 비교한다 — 방언별 암묵 변환이나 affinity를
/// 추측하지 않으므로 "선언이 다르다"는 사실만 말한다. 불완전한 FK는
/// `fk-index-prefix`가 이미 미확인으로 알리므로 여기서 중복 보고하지 않는다.
fn foreign_key_type_findings(
    metadata: &SchemaMetadata,
    findings: &mut Vec<LintFinding>,
    complete: &mut bool,
) {
    for (subject, foreign_key) in &metadata.foreign_keys {
        if !foreign_key.complete || foreign_key.columns.len() != foreign_key.target_columns.len() {
            continue;
        }
        for (column, target) in foreign_key.columns.iter().zip(&foreign_key.target_columns) {
            if let Some(finding) = type_pair_finding(metadata, subject, foreign_key, column, target)
            {
                if finding.status == LintStatus::Unverified {
                    *complete = false;
                }
                findings.push(finding);
            }
        }
    }
}

/// FK 컬럼 한 쌍의 선언 타입을 비교해 차이가 있거나 판정할 수 없으면 finding을 만든다.
fn type_pair_finding(
    metadata: &SchemaMetadata,
    subject: &VertexId,
    foreign_key: &schemagraph_core::ForeignKeyMetadata,
    column: &VertexId,
    target: &VertexId,
) -> Option<LintFinding> {
    let finding = |status, message: String| LintFinding {
        rule: "fk-type-mismatch".into(),
        status,
        subject: subject.clone(),
        table: Some(foreign_key.table.clone()),
        columns: vec![column.clone()],
        code: None,
        message,
        location: None,
    };
    let (Some(local), Some(remote)) = (metadata.columns.get(column), metadata.columns.get(target))
    else {
        return Some(finding(
            LintStatus::Unverified,
            format!(
                "column types of {} -> {} were not collected; type agreement is unverified",
                column.as_str(),
                target.as_str()
            ),
        ));
    };
    (normalized_type(&local.data_type) != normalized_type(&remote.data_type)).then(|| {
        finding(
            LintStatus::Confirmed,
            format!(
                "declared type {} of {} differs from {} of referenced {}",
                local.data_type,
                column.as_str(),
                remote.data_type,
                target.as_str()
            ),
        )
    })
}

/// 대소문자와 공백 차이만 접어 선언 타입을 비교한다 — 동의어 해석은 하지 않는다.
fn normalized_type(data_type: &str) -> String {
    data_type
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

/// PK 컬럼이 하나도 보고되지 않은 테이블을 알린다.
///
/// `pk_position` 0은 "PK 아님"과 "못 읽음"을 구분하지 못하므로, 수집기가
/// 카탈로그를 완전하게 읽었다고 선언했을 때만 확정한다.
fn missing_primary_key_findings(
    graph: &Graph,
    metadata: &SchemaMetadata,
    findings: &mut Vec<LintFinding>,
    complete: &mut bool,
) {
    let mut has_primary_key = BTreeMap::<VertexId, bool>::new();
    for (column, value) in &metadata.columns {
        if let Some(table) = column.parent() {
            *has_primary_key.entry(table).or_default() |= value.pk_position > 0;
        }
    }
    let tables = has_primary_key.into_iter().filter(|(table, has_pk)| {
        !has_pk
            && graph
                .vertex(table)
                .is_some_and(|v| v.kind == VertexKind::Table)
    });
    for (table, _) in tables {
        let (status, message) = if metadata.catalog_complete {
            (
                LintStatus::Confirmed,
                "no primary-key column was declared in a complete catalog",
            )
        } else {
            *complete = false;
            (LintStatus::Unverified, "no primary-key column was reported, but the collector did not declare a complete catalog")
        };
        findings.push(LintFinding {
            rule: "table-without-primary-key".into(),
            status,
            subject: table.clone(),
            table: Some(table),
            columns: Vec::new(),
            code: None,
            message: message.into(),
            location: None,
        });
    }
}

/// 키 컬럼·순서·유일성이 같은 조건 없는 인덱스를 알린다.
///
/// 수집기가 access method·operator class·정렬 방향을 옮기지 않으므로 같은
/// 컬럼의 btree와 GIN을 구분할 수 없다. 그래서 항상 미확인이며, 규칙의
/// 경계이지 입력 누락이 아니므로 보고서 완전성은 낮추지 않는다.
fn duplicate_index_findings(metadata: &SchemaMetadata, findings: &mut Vec<LintFinding>) {
    let mut groups = BTreeMap::<(&VertexId, &[VertexId], bool), Vec<&VertexId>>::new();
    for (id, index) in &metadata.indexes {
        if index.complete && !index.has_predicate && !index.columns.is_empty() {
            groups
                .entry((&index.table, index.columns.as_slice(), index.unique))
                .or_default()
                .push(id);
        }
    }
    for ((table, columns, _), ids) in groups {
        let Some((first, rest)) = ids.split_first() else {
            continue;
        };
        for id in rest {
            findings.push(LintFinding {
                rule: "duplicate-index".into(),
                status: LintStatus::Unverified,
                subject: (*id).clone(),
                table: Some(table.clone()),
                columns: columns.to_vec(),
                code: None,
                message: format!("same key columns, order, and uniqueness as {}; access method, operator class, and sort order are not collected, so compare the definitions before treating either as redundant", first.as_str()),
                location: None,
            });
        }
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
        Some(metadata) => {
            foreign_key_findings(graph, metadata, &mut findings, &mut complete);
            foreign_key_type_findings(metadata, &mut findings, &mut complete);
            missing_primary_key_findings(graph, metadata, &mut findings, &mut complete);
            duplicate_index_findings(metadata, &mut findings);
        }
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
            catalog_complete: complete,
        }
    }

    /// 이 테스트들이 검증하는 FK 인덱스 규칙의 finding만 고른다 — fixture에는
    /// 참조 대상과 PK가 없어 새 규칙도 finding을 내며, 그 규칙은 전용 테스트가 본다.
    fn prefix_findings(report: &LintReport) -> Vec<&LintFinding> {
        report
            .findings
            .iter()
            .filter(|finding| finding.rule == "fk-index-prefix")
            .collect()
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
        assert!(prefix_findings(&lint(
            &graph_with_metadata(Some(metadata(Some(good), true))),
            10
        ))
        .is_empty());
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
        let prefix = prefix_findings(&report);
        assert!(prefix.iter().all(|f| f.status == LintStatus::Unverified));
        assert!(!report.complete);
        assert!(prefix[0].message.contains("partial indexes"));
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
        assert!(!report.complete);
        assert_eq!(prefix_findings(&report)[0].status, LintStatus::Unverified);
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
        // FK 인덱스·진단·PK 부재(카탈로그 미완전이라 미확인) 세 건이다.
        assert_eq!(report.total_findings, 3);
        assert_eq!(report.findings.len(), 1);
        assert!(report.truncated);
        assert!(!report.complete);
        assert!(report
            .findings
            .iter()
            .all(|finding| !finding.message.to_lowercase().contains("drop")));
    }

    /// 새 규칙 전용 fixture — 부모·자식·PK 없는 테이블·view와 인덱스를 갖는다.
    fn rule_fixture(child_type: &str, catalog_complete: bool) -> Graph {
        let mut graph = Graph::new();
        let objects = [
            ("app.parent", VertexKind::Table),
            ("app.child", VertexKind::Table),
            ("app.nopk", VertexKind::Table),
            ("app.report", VertexKind::View),
        ];
        let members = [
            ("app.parent.id", VertexKind::Column),
            ("app.child.id", VertexKind::Column),
            ("app.child.parent_id", VertexKind::Column),
            ("app.nopk.x", VertexKind::Column),
            ("app.report.x", VertexKind::Column),
            ("app.child.child_fk", VertexKind::Constraint),
            ("app.child.i1", VertexKind::Index),
            ("app.child.i2", VertexKind::Index),
            ("app.child.i3", VertexKind::Index),
            ("app.child.i4", VertexKind::Index),
        ];
        for (raw, kind) in objects.into_iter().chain(members) {
            let name = raw.rsplit('.').next().unwrap();
            graph.add_vertex(Vertex {
                id: id(raw),
                kind,
                name: name.into(),
                schema: "app".into(),
            });
        }
        let column = |data_type: &str, pk_position| ColumnMetadata {
            data_type: data_type.into(),
            nullable: pk_position == 0,
            ordinal: 1,
            pk_position,
        };
        let mut metadata = SchemaMetadata {
            catalog_complete,
            ..SchemaMetadata::default()
        };
        for (raw, value) in [
            ("app.parent.id", column("integer", 1)),
            ("app.child.id", column("integer", 1)),
            ("app.child.parent_id", column(child_type, 0)),
            ("app.nopk.x", column("text", 0)),
            ("app.report.x", column("text", 0)),
        ] {
            metadata.columns.insert(id(raw), value);
        }
        let index = |unique, has_predicate| IndexMetadata {
            table: id("app.child"),
            columns: vec![id("app.child.parent_id")],
            unique,
            has_predicate,
            complete: true,
        };
        metadata
            .indexes
            .insert(id("app.child.i1"), index(false, false));
        metadata
            .indexes
            .insert(id("app.child.i2"), index(false, false));
        metadata
            .indexes
            .insert(id("app.child.i3"), index(true, false));
        metadata
            .indexes
            .insert(id("app.child.i4"), index(false, true));
        metadata.foreign_keys.insert(
            id("app.child.child_fk"),
            ForeignKeyMetadata {
                table: id("app.child"),
                columns: vec![id("app.child.parent_id")],
                target_table: Some(id("app.parent")),
                target_columns: vec![id("app.parent.id")],
                complete: true,
            },
        );
        graph.set_schema_metadata(metadata);
        graph
    }

    fn rule_findings<'a>(report: &'a LintReport, rule: &str) -> Vec<&'a LintFinding> {
        report.findings.iter().filter(|f| f.rule == rule).collect()
    }

    #[test]
    fn fk_type_mismatch_reports_declared_type_difference_only() {
        let report = lint(&rule_fixture("BIGINT", true), 50);
        let mismatch = rule_findings(&report, "fk-type-mismatch");
        assert_eq!(mismatch.len(), 1, "{report:?}");
        assert_eq!(mismatch[0].status, LintStatus::Confirmed);
        assert!(mismatch[0].message.contains("BIGINT") && mismatch[0].message.contains("integer"));
        // 대소문자·공백 차이는 같은 선언으로 본다.
        let same = lint(&rule_fixture(" INTEGER ", true), 50);
        assert!(rule_findings(&same, "fk-type-mismatch").is_empty());
    }

    #[test]
    fn fk_type_without_target_metadata_is_unverified_and_incomplete() {
        let mut graph = rule_fixture("integer", true);
        let mut metadata = graph.schema_metadata().unwrap().clone();
        metadata.columns.remove(&id("app.parent.id"));
        graph.set_schema_metadata(metadata);
        let report = lint(&graph, 50);
        let finding = rule_findings(&report, "fk-type-mismatch");
        assert_eq!(finding[0].status, LintStatus::Unverified);
        assert!(!report.complete);
    }

    #[test]
    fn missing_primary_key_is_confirmed_only_in_a_complete_catalog() {
        let report = lint(&rule_fixture("integer", true), 50);
        let missing = rule_findings(&report, "table-without-primary-key");
        // 뷰(app.report)와 PK가 있는 테이블은 대상이 아니다.
        assert_eq!(
            missing
                .iter()
                .map(|f| f.subject.as_str())
                .collect::<Vec<_>>(),
            ["app.nopk"]
        );
        assert_eq!(missing[0].status, LintStatus::Confirmed);
        let incomplete = lint(&rule_fixture("integer", false), 50);
        assert_eq!(
            rule_findings(&incomplete, "table-without-primary-key")[0].status,
            LintStatus::Unverified
        );
        assert!(!incomplete.complete);
    }

    #[test]
    fn duplicate_index_is_unverified_without_lowering_completeness() {
        let report = lint(&rule_fixture("integer", true), 50);
        let duplicates = rule_findings(&report, "duplicate-index");
        // i3는 유일성, i4는 조건이 달라 제외되고 i2만 i1의 중복 후보다.
        assert_eq!(duplicates.len(), 1);
        assert_eq!(duplicates[0].subject.as_str(), "app.child.i2");
        assert!(duplicates[0].message.contains("app.child.i1"));
        assert_eq!(duplicates[0].status, LintStatus::Unverified);
        assert!(report.complete, "{report:?}");
    }
}
