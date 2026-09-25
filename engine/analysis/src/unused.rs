//! 사용 통계 창 안에서 읽힌 기록이 없는 테이블·인덱스를 보고한다.
//!
//! `dead`는 그래프 도달성으로 소비자 객체를 보지만, 이 질의는 DB가 센 사용
//! 카운터만 본다. 통계는 리셋 이후만 유효하고 앱·배치 사용을 다 담지 못하므로
//! 후보마다 `since`와 판정을 흔드는 사실(유일성 강제·FK 지원·SQL 참조)을 함께
//! 싣고, 삭제 가능 여부는 말하지 않는다. 사용 기록이 없는 객체는 "관측된 0"과
//! 다르므로 후보가 아니라 미관측으로 센다.

use crate::schema_lint::backs_constraint;
use schemagraph_core::{EdgeKind, Graph, SchemaMetadata, Usage, Vertex, VertexId, VertexKind};
use std::collections::{BTreeMap, BTreeSet};

/// 후보 하나와 판정을 흔드는 관찰 사실.
#[derive(Debug, Clone, PartialEq)]
pub struct UnusedCandidate<'a> {
    /// 읽힌 기록이 없는 테이블 또는 인덱스.
    pub vertex: &'a Vertex,
    /// 수집된 사용 카운터 — 항상 존재한다(없으면 미관측이다).
    pub usage: &'a Usage,
    /// 유일 인덱스라 읽히지 않아도 제약을 강제할 수 있다.
    pub enforces_uniqueness: bool,
    /// 같은 이름의 PK·UNIQUE 계열 제약을 받치는 인덱스다.
    pub backs_constraint: bool,
    /// 조건 없고 완전한 이 인덱스가 키 prefix로 덮는 FK 제약.
    pub covers_foreign_keys: Vec<VertexId>,
    /// 인덱스 메타데이터가 없어 위 세 사실을 판정하지 못했다("해당 없음"이 아니다).
    pub metadata_unavailable: bool,
    /// 이 테이블을 참조하는 SQL 몸체 정점 수(테이블 자신의 트리거·멤버는 제외).
    pub body_dependents: usize,
    /// 이 테이블의 인덱스 중 사용 기록이 없는 것이 있다 — index-only 읽기를 배제하지 못한다.
    pub index_without_usage: bool,
    /// 스캔 횟수를 수집하지 않은 테이블이다 — 늘 비어 있는 채로 폴링되는 테이블도 읽기 0으로 보인다.
    pub scan_count_unavailable: bool,
}

/// 미관측 객체 수 — 사용 기록이 없어 판정 대상에서 뺀 테이블·인덱스.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Unobserved {
    /// 사용 기록이 없는 테이블 수.
    pub tables: usize,
    /// 사용 기록이 없는 인덱스 수.
    pub indexes: usize,
}

/// 미사용 후보 보고서.
#[derive(Debug, Clone, PartialEq)]
pub struct UnusedReport<'a> {
    /// id 순으로 최대 `max`개까지 담은 후보.
    pub candidates: Vec<UnusedCandidate<'a>>,
    /// 상한과 무관한 전체 후보 수.
    pub total: usize,
    /// `total`이 담긴 수보다 커서 결과를 잘랐는지 여부.
    pub truncated: bool,
    /// 사용 기록이 없어 판정하지 못한 객체 수.
    pub unobserved: Unobserved,
}

/// 읽힌 기록이 없는 테이블·인덱스를 id 순으로 최대 `max`개 보고한다.
///
/// 후보 여부는 모든 객체에 대해 싸게 판정하고, 사실은 담을 후보에만 붙인다.
pub fn unused(graph: &Graph, max: usize) -> UnusedReport<'_> {
    let mut unobserved = Unobserved::default();
    let mut selected = Vec::new();
    let targets = graph
        .vertices()
        .filter(|vertex| matches!(vertex.kind, VertexKind::Table | VertexKind::Index));
    for vertex in targets {
        match graph.usage(&vertex.id) {
            None if vertex.kind == VertexKind::Table => unobserved.tables += 1,
            None => unobserved.indexes += 1,
            Some(usage) if is_candidate(graph, vertex, usage) => selected.push((vertex, usage)),
            Some(_) => {}
        }
    }
    let total = selected.len();
    let covering = foreign_keys_by_table(graph.schema_metadata());
    let candidates: Vec<_> = selected
        .into_iter()
        .take(max)
        .map(|(vertex, usage)| describe(graph, &covering, vertex, usage))
        .collect();
    UnusedReport {
        truncated: total > candidates.len(),
        candidates,
        total,
        unobserved,
    }
}

/// 읽기 0인지 판정한다. 테이블은 스캔이 있었거나 인덱스가 읽혔으면 후보가 아니다 —
/// 튜플 수인 reads는 빈 테이블 폴링과 index-only scan을 세지 못한다.
fn is_candidate(graph: &Graph, vertex: &Vertex, usage: &Usage) -> bool {
    if usage.reads > 0 {
        return false;
    }
    if vertex.kind != VertexKind::Table {
        return true;
    }
    if usage.scans.is_some_and(|scans| scans > 0) {
        return false;
    }
    !table_indexes(graph, &vertex.id)
        .iter()
        .any(|index| graph.usage(index).is_some_and(|u| u.reads > 0))
}

/// 후보에 삭제 해석을 흔드는 사실을 붙인다.
fn describe<'a>(
    graph: &'a Graph,
    covering: &BTreeMap<&VertexId, Vec<(&VertexId, &[VertexId])>>,
    vertex: &'a Vertex,
    usage: &'a Usage,
) -> UnusedCandidate<'a> {
    let mut candidate = UnusedCandidate {
        vertex,
        usage,
        enforces_uniqueness: false,
        backs_constraint: false,
        covers_foreign_keys: Vec::new(),
        metadata_unavailable: false,
        body_dependents: 0,
        index_without_usage: false,
        scan_count_unavailable: false,
    };
    if vertex.kind == VertexKind::Table {
        let indexes = table_indexes(graph, &vertex.id);
        candidate.index_without_usage = indexes.iter().any(|index| graph.usage(index).is_none());
        candidate.scan_count_unavailable = usage.scans.is_none();
        candidate.body_dependents = body_dependents(graph, &vertex.id);
    } else {
        describe_index(graph, covering, vertex, &mut candidate);
    }
    candidate
}

/// 테이블이 `contains`로 소유한 정점(테이블 자신 포함).
fn owned(graph: &Graph, table: &VertexId) -> BTreeSet<VertexId> {
    std::iter::once(table.clone())
        .chain(
            graph
                .outgoing(table)
                .iter()
                .filter(|edge| edge.kind == EdgeKind::Contains)
                .map(|edge| edge.to.clone()),
        )
        .collect()
}

/// 테이블이 소유한 인덱스 정점.
fn table_indexes(graph: &Graph, table: &VertexId) -> Vec<VertexId> {
    owned(graph, table)
        .into_iter()
        .filter(|id| {
            graph
                .vertex(id)
                .is_some_and(|v| v.kind == VertexKind::Index)
        })
        .collect()
}

/// 테이블 또는 그 멤버를 의존 간선으로 참조하는 SQL 몸체 정점 수.
/// 테이블이 소유한 정점(자기 트리거 등)은 세지 않는다 — 외부 참조만 삭제 해석을 흔든다.
fn body_dependents(graph: &Graph, table: &VertexId) -> usize {
    let owned = owned(graph, table);
    let is_body = |id: &VertexId| {
        !owned.contains(id)
            && graph.vertex(id).is_some_and(|v| {
                matches!(
                    v.kind,
                    VertexKind::View
                        | VertexKind::MaterializedView
                        | VertexKind::Function
                        | VertexKind::Procedure
                        | VertexKind::Package
                        | VertexKind::Query
                        | VertexKind::Trigger
                )
            })
    };
    owned
        .iter()
        .flat_map(|id| graph.incoming(id))
        .filter(|edge| edge.kind.is_dependency() && is_body(&edge.from))
        .map(|edge| edge.from.clone())
        .collect::<BTreeSet<_>>()
        .len()
}

/// FK를 테이블별로 한 번 묶는다 — 인덱스마다 전체 FK를 훑지 않기 위해서다.
fn foreign_keys_by_table(
    metadata: Option<&SchemaMetadata>,
) -> BTreeMap<&VertexId, Vec<(&VertexId, &[VertexId])>> {
    let mut by_table = BTreeMap::<&VertexId, Vec<(&VertexId, &[VertexId])>>::new();
    for (id, fk) in metadata.map(|m| &m.foreign_keys).into_iter().flatten() {
        if !fk.columns.is_empty() {
            by_table
                .entry(&fk.table)
                .or_default()
                .push((id, &fk.columns));
        }
    }
    by_table
}

/// 인덱스 후보에 유일성·제약·FK 지원 사실을 붙인다. 메타데이터가 없으면 모른다고 표시한다.
fn describe_index(
    graph: &Graph,
    covering: &BTreeMap<&VertexId, Vec<(&VertexId, &[VertexId])>>,
    vertex: &Vertex,
    candidate: &mut UnusedCandidate,
) {
    let Some(index) = graph
        .schema_metadata()
        .and_then(|metadata| metadata.indexes.get(&vertex.id))
    else {
        candidate.metadata_unavailable = true;
        return;
    };
    candidate.enforces_uniqueness = index.unique;
    candidate.backs_constraint = backs_constraint(graph, &index.table, &vertex.id);
    // 조건부·불완전 인덱스는 FK 검사를 덮는다고 말할 수 없다 — lint와 같은 기준이다.
    if index.complete && !index.has_predicate {
        candidate.covers_foreign_keys = covering
            .get(&index.table)
            .into_iter()
            .flatten()
            .filter(|(_, columns)| index.columns.starts_with(columns))
            .map(|(id, _)| (*id).clone())
            .collect();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use schemagraph_core::{Edge, EdgeKind, ForeignKeyMetadata, IndexMetadata};

    fn id(raw: &str) -> VertexId {
        VertexId::from_raw(raw)
    }

    fn usage(reads: u64, writes: u64) -> Usage {
        Usage {
            since: Some("2026-09-01".into()),
            reads,
            writes,
            scans: Some(0),
            total_ms: None,
            self_ms: None,
        }
    }

    fn edge(from: &str, to: &str, kind: EdgeKind) -> Edge {
        Edge {
            from: id(from),
            to: id(to),
            kind,
            evidence: vec![],
        }
    }

    /// 읽힌 테이블·유휴 테이블·index-only로 읽힌 테이블·미관측 테이블과 인덱스를 갖는다.
    fn fixture() -> Graph {
        let mut graph = Graph::new();
        let vertices = [
            ("s.read", VertexKind::Table),
            ("s.idle", VertexKind::Table),
            ("s.idle.v", VertexKind::Column),
            ("s.idle.idle_v", VertexKind::Index),
            ("s.idle.idle_u", VertexKind::Index),
            ("s.idle.idle_u@constraint", VertexKind::Constraint),
            ("s.idle.idle_fk", VertexKind::Constraint),
            ("s.ionly", VertexKind::Table),
            ("s.ionly.ionly_v", VertexKind::Index),
            ("s.unseen", VertexKind::Table),
            ("s.report", VertexKind::View),
        ];
        for (raw, kind) in vertices {
            let name = raw.rsplit('.').next().unwrap().split('@').next().unwrap();
            graph.add_vertex(Vertex {
                id: id(raw),
                kind,
                name: name.into(),
                schema: "s".into(),
            });
        }
        for (table, member) in [
            ("s.idle", "s.idle.v"),
            ("s.idle", "s.idle.idle_v"),
            ("s.idle", "s.idle.idle_u"),
            ("s.idle", "s.idle.idle_u@constraint"),
            ("s.idle", "s.idle.idle_fk"),
            ("s.ionly", "s.ionly.ionly_v"),
        ] {
            graph.add_edge(edge(table, member, EdgeKind::Contains));
        }
        graph.add_edge(edge("s.report", "s.idle.v", EdgeKind::Reads));
        for (raw, reads, writes) in [
            ("s.read", 9, 0),
            ("s.idle", 0, 4),
            ("s.idle.idle_v", 0, 0),
            ("s.idle.idle_u", 0, 0),
            ("s.ionly", 0, 0),
            ("s.ionly.ionly_v", 3, 0),
        ] {
            graph.set_usage(id(raw), usage(reads, writes));
        }
        let mut metadata = SchemaMetadata::default();
        let index = |columns: Vec<&str>, unique| IndexMetadata {
            table: id("s.idle"),
            columns: columns.into_iter().map(id).collect(),
            unique,
            has_predicate: false,
            complete: true,
        };
        metadata
            .indexes
            .insert(id("s.idle.idle_v"), index(vec!["s.idle.v"], false));
        metadata
            .indexes
            .insert(id("s.idle.idle_u"), index(vec!["s.idle.v"], true));
        metadata.foreign_keys.insert(
            id("s.idle.idle_fk"),
            ForeignKeyMetadata {
                table: id("s.idle"),
                columns: vec![id("s.idle.v")],
                target_table: Some(id("s.read")),
                target_columns: vec![],
                complete: true,
            },
        );
        graph.set_schema_metadata(metadata);
        graph
    }

    fn ids(report: &UnusedReport) -> Vec<String> {
        report
            .candidates
            .iter()
            .map(|c| c.vertex.id.as_str().to_owned())
            .collect()
    }

    #[test]
    fn zero_reads_are_candidates_and_missing_usage_is_unobserved() {
        let graph = fixture();
        let report = unused(&graph, 10);
        // s.read는 읽혔고, s.ionly는 인덱스로 읽혔으며, s.unseen은 기록이 없다.
        assert_eq!(ids(&report), ["s.idle", "s.idle.idle_u", "s.idle.idle_v"]);
        assert_eq!(
            report.unobserved,
            Unobserved {
                tables: 1,
                indexes: 0
            }
        );
        assert!(!report.truncated);
    }

    #[test]
    fn candidates_carry_facts_that_weaken_a_removal_reading() {
        let graph = fixture();
        let report = unused(&graph, 10);
        let by_id = |raw: &str| {
            report
                .candidates
                .iter()
                .find(|c| c.vertex.id.as_str() == raw)
                .unwrap()
        };
        let table = by_id("s.idle");
        assert_eq!((table.usage.writes, table.body_dependents), (4, 1));
        let unique = by_id("s.idle.idle_u");
        assert!(unique.enforces_uniqueness && unique.backs_constraint);
        assert_eq!(unique.covers_foreign_keys, vec![id("s.idle.idle_fk")]);
        assert!(!by_id("s.idle.idle_v").backs_constraint);
    }

    #[test]
    fn truncation_keeps_the_full_total() {
        let graph = fixture();
        let report = unused(&graph, 1);
        assert_eq!(
            (report.candidates.len(), report.total, report.truncated),
            (1, 3, true)
        );
    }

    /// 늘 비어 있는 채 폴링되는 테이블은 튜플 0이어도 스캔이 쌓이므로 후보가 아니다.
    #[test]
    fn scanned_empty_table_is_not_a_candidate_and_missing_scans_are_flagged() {
        let mut graph = fixture();
        let mut polled = usage(0, 0);
        polled.scans = Some(5);
        graph.set_usage(id("s.idle"), polled);
        assert!(!ids(&unused(&graph, 10)).contains(&"s.idle".to_owned()));
        let mut unknown = usage(0, 0);
        unknown.scans = None;
        graph.set_usage(id("s.idle"), unknown);
        let report = unused(&graph, 10);
        let table = report
            .candidates
            .iter()
            .find(|c| c.vertex.id.as_str() == "s.idle")
            .unwrap();
        assert!(table.scan_count_unavailable);
    }

    /// FK 대상 테이블을 읽는 뷰와 테이블 자신의 트리거는 이 테이블의 SQL 참조가 아니다.
    #[test]
    fn body_dependents_ignore_fk_targets_and_own_triggers() {
        let mut graph = fixture();
        for (raw, kind) in [
            ("s.child", VertexKind::Table),
            ("s.child.audit", VertexKind::Trigger),
            ("s.read_report", VertexKind::View),
        ] {
            graph.add_vertex(Vertex {
                id: id(raw),
                kind,
                name: raw.rsplit('.').next().unwrap().into(),
                schema: "s".into(),
            });
        }
        graph.add_edge(edge("s.child", "s.read", EdgeKind::References));
        graph.add_edge(edge("s.read_report", "s.read", EdgeKind::Reads));
        graph.add_edge(edge("s.child", "s.child.audit", EdgeKind::Contains));
        graph.add_edge(edge("s.child.audit", "s.child", EdgeKind::Fires));
        graph.set_usage(id("s.child"), usage(0, 0));
        let report = unused(&graph, 10);
        let child = report
            .candidates
            .iter()
            .find(|c| c.vertex.id.as_str() == "s.child")
            .unwrap();
        assert_eq!(child.body_dependents, 0);
    }

    /// 조건부 인덱스는 FK를 덮는다고 말하지 않고, 메타데이터가 없으면 모른다고 표시한다.
    #[test]
    fn partial_index_does_not_cover_fk_and_missing_metadata_is_explicit() {
        let mut graph = fixture();
        let mut metadata = graph.schema_metadata().unwrap().clone();
        metadata
            .indexes
            .get_mut(&id("s.idle.idle_u"))
            .unwrap()
            .has_predicate = true;
        metadata.indexes.remove(&id("s.idle.idle_v"));
        graph.set_schema_metadata(metadata);
        let report = unused(&graph, 10);
        let by_id = |raw: &str| {
            report
                .candidates
                .iter()
                .find(|c| c.vertex.id.as_str() == raw)
                .unwrap()
        };
        assert!(by_id("s.idle.idle_u").covers_foreign_keys.is_empty());
        let unknown = by_id("s.idle.idle_v");
        assert!(unknown.metadata_unavailable && !unknown.enforces_uniqueness);
    }
}
