//! 사용 통계 창 안에서 읽힌 기록이 없는 테이블·인덱스를 보고한다.
//!
//! `dead`는 그래프 도달성으로 소비자 객체를 보지만, 이 질의는 DB가 센 사용
//! 카운터만 본다. 통계는 리셋 이후만 유효하고 앱·배치 사용을 다 담지 못하므로
//! 후보마다 `since`와 판정을 흔드는 사실(유일성 강제·FK 지원·SQL 참조)을 함께
//! 싣고, 삭제 가능 여부는 말하지 않는다. 사용 기록이 없는 객체는 "관측된 0"과
//! 다르므로 후보가 아니라 미관측으로 센다.

use schemagraph_core::{Graph, SchemaMetadata, Usage, Vertex, VertexId, VertexKind};
use std::collections::BTreeSet;

/// 후보 하나와 판정을 흔드는 관찰 사실.
#[derive(Debug, Clone, PartialEq)]
pub struct UnusedCandidate<'a> {
    /// 읽힌 기록이 없는 테이블 또는 인덱스.
    pub vertex: &'a Vertex,
    /// 수집된 사용 카운터 — 항상 존재한다(없으면 미관측이다).
    pub usage: &'a Usage,
    /// 유일 인덱스라 읽히지 않아도 제약을 강제할 수 있다.
    pub enforces_uniqueness: bool,
    /// 같은 이름의 제약을 받치는 인덱스다.
    pub backs_constraint: bool,
    /// 이 인덱스가 키 prefix로 덮는 FK 제약.
    pub covers_foreign_keys: Vec<VertexId>,
    /// 이 테이블을 참조하는 SQL 몸체(뷰·루틴·질의) 정점 수.
    pub body_dependents: usize,
    /// 이 테이블의 인덱스 중 사용 기록이 없는 것이 있다 — index-only 읽기를 배제하지 못한다.
    pub index_without_usage: bool,
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
pub fn unused(graph: &Graph, max: usize) -> UnusedReport<'_> {
    let mut report = UnusedReport {
        candidates: Vec::new(),
        total: 0,
        truncated: false,
        unobserved: Unobserved::default(),
    };
    let targets = graph
        .vertices()
        .filter(|vertex| matches!(vertex.kind, VertexKind::Table | VertexKind::Index));
    for vertex in targets {
        let Some(usage) = graph.usage(&vertex.id) else {
            count_unobserved(&mut report.unobserved, vertex.kind);
            continue;
        };
        if let Some(candidate) = candidate(graph, vertex, usage) {
            report.total += 1;
            if report.candidates.len() < max {
                report.candidates.push(candidate);
            }
        }
    }
    report.truncated = report.total > report.candidates.len();
    report
}

/// 사용 기록이 없는 객체를 종류별로 센다.
fn count_unobserved(unobserved: &mut Unobserved, kind: VertexKind) {
    match kind {
        VertexKind::Table => unobserved.tables += 1,
        _ => unobserved.indexes += 1,
    }
}

/// 읽기 0인 객체를 후보로 만든다. 인덱스로 읽힌 테이블은 후보가 아니다.
fn candidate<'a>(
    graph: &'a Graph,
    vertex: &'a Vertex,
    usage: &'a Usage,
) -> Option<UnusedCandidate<'a>> {
    if usage.reads > 0 {
        return None;
    }
    let mut candidate = UnusedCandidate {
        vertex,
        usage,
        enforces_uniqueness: false,
        backs_constraint: false,
        covers_foreign_keys: Vec::new(),
        body_dependents: 0,
        index_without_usage: false,
    };
    if vertex.kind == VertexKind::Table {
        // index-only scan은 테이블 튜플을 가져오지 않아 테이블 읽기 카운터에 잡히지 않는다.
        let indexes = table_indexes(graph, &vertex.id);
        if indexes
            .iter()
            .any(|index| graph.usage(index).is_some_and(|u| u.reads > 0))
        {
            return None;
        }
        candidate.index_without_usage = indexes.iter().any(|index| graph.usage(index).is_none());
        candidate.body_dependents = body_dependents(graph, &vertex.id);
    } else {
        describe_index(graph, vertex, &mut candidate);
    }
    Some(candidate)
}

/// 테이블이 소유한 인덱스 정점.
fn table_indexes(graph: &Graph, table: &VertexId) -> Vec<VertexId> {
    graph
        .outgoing(table)
        .iter()
        .filter(|edge| {
            graph
                .vertex(&edge.to)
                .is_some_and(|v| v.kind == VertexKind::Index)
        })
        .map(|edge| edge.to.clone())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

/// 테이블 또는 그 컬럼을 의존 간선으로 참조하는 SQL 몸체 정점 수.
fn body_dependents(graph: &Graph, table: &VertexId) -> usize {
    let is_body = |id: &VertexId| {
        graph.vertex(id).is_some_and(|v| {
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
    let owned: Vec<VertexId> = std::iter::once(table.clone())
        .chain(graph.outgoing(table).iter().map(|edge| edge.to.clone()))
        .collect();
    owned
        .iter()
        .flat_map(|id| graph.incoming(id))
        .filter(|edge| edge.kind.is_dependency() && is_body(&edge.from))
        .map(|edge| edge.from.clone())
        .collect::<BTreeSet<_>>()
        .len()
}

/// 인덱스 후보에 유일성·제약·FK 지원 사실을 붙인다.
fn describe_index(graph: &Graph, vertex: &Vertex, candidate: &mut UnusedCandidate) {
    let Some(metadata) = graph.schema_metadata() else {
        return;
    };
    let Some(index) = metadata.indexes.get(&vertex.id) else {
        return;
    };
    candidate.enforces_uniqueness = index.unique;
    candidate.backs_constraint = graph.outgoing(&index.table).iter().any(|edge| {
        graph
            .vertex(&edge.to)
            .is_some_and(|v| v.kind == VertexKind::Constraint && v.name == vertex.name)
    });
    candidate.covers_foreign_keys = covered_foreign_keys(metadata, &index.table, &index.columns);
}

/// 인덱스 키가 prefix로 덮는 같은 테이블의 FK 제약.
fn covered_foreign_keys(
    metadata: &SchemaMetadata,
    table: &VertexId,
    columns: &[VertexId],
) -> Vec<VertexId> {
    metadata
        .foreign_keys
        .iter()
        .filter(|(_, fk)| {
            &fk.table == table && !fk.columns.is_empty() && columns.starts_with(&fk.columns)
        })
        .map(|(id, _)| id.clone())
        .collect()
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
}
