//! 그래프의 계보 사실을 작업(SQL 몸체) 단위로 묶는다 — OpenLineage 같은 외부
//! 계보 형식으로 내보내기 위한 순수 질의다.
//!
//! 새 판정을 만들지 않는다. 값 계보는 `derives-from` 간선을, 간접 사용은
//! origin 역할이 `join`·`predicate`인 `reads` 간선을 그대로 옮긴다. 그 밖의
//! 역할(정렬·그룹화 등)은 그래프가 구분해 기록하지 않으므로 추측해 붙이지 않는다.

use schemagraph_core::{EdgeKind, Graph, Origin, VertexId, VertexKind};
use std::collections::{BTreeMap, BTreeSet};

/// 간접 사용의 종류 — OpenLineage INDIRECT subtype과 1:1로 대응한다.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum IndirectUse {
    /// 조인 조건에 쓰인 컬럼.
    Join,
    /// 필터(WHERE 등) 조건에 쓰인 컬럼.
    Filter,
}

/// 한 SQL 몸체(뷰·루틴·트리거·질의)가 만든 계보.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Job {
    /// 읽은 데이터셋(테이블·뷰·materialized view).
    pub inputs: BTreeSet<VertexId>,
    /// 만든 데이터셋 — 뷰는 자기 자신, DML 몸체는 쓴 테이블이다.
    pub outputs: BTreeSet<VertexId>,
    /// 출력 컬럼 → 값을 제공한 입력 컬럼.
    pub direct: BTreeMap<VertexId, BTreeSet<VertexId>>,
    /// 입력 컬럼 → 결과 행을 좌우한 간접 사용 종류.
    pub indirect: BTreeMap<VertexId, BTreeSet<IndirectUse>>,
}

/// 계보를 가진 작업 전체. 키는 작업 정점 id라 순서가 결정적이다.
pub fn jobs(graph: &Graph) -> BTreeMap<VertexId, Job> {
    let mut jobs = BTreeMap::<VertexId, Job>::new();
    for edge in graph.edges() {
        let origins = graph
            .origins()
            .get(&(edge.from.clone(), edge.to.clone(), edge.kind));
        match edge.kind {
            EdgeKind::DerivesFrom => add_direct(graph, &mut jobs, &edge.from, &edge.to, origins),
            EdgeKind::Reads if is_job(graph, &edge.from) => {
                add_read(graph, &mut jobs, &edge.from, &edge.to, origins)
            }
            EdgeKind::Writes if is_job(graph, &edge.from) => {
                if let Some(dataset) = dataset_of(graph, &edge.to) {
                    jobs.entry(edge.from.clone())
                        .or_default()
                        .outputs
                        .insert(dataset);
                }
            }
            _ => {}
        }
    }
    for (id, job) in &mut jobs {
        if is_view(graph, id) {
            job.outputs.insert(id.clone());
        }
    }
    jobs.retain(|_, job| !job.outputs.is_empty());
    jobs
}

/// 계보를 만드는 SQL 몸체 정점인지.
fn is_job(graph: &Graph, id: &VertexId) -> bool {
    graph.vertex(id).is_some_and(|vertex| {
        matches!(
            vertex.kind,
            VertexKind::View
                | VertexKind::MaterializedView
                | VertexKind::Function
                | VertexKind::Procedure
                | VertexKind::Package
                | VertexKind::Trigger
                | VertexKind::Query
        )
    })
}

/// 뷰처럼 자기 자신이 출력 데이터셋인 몸체인지.
fn is_view(graph: &Graph, id: &VertexId) -> bool {
    graph.vertex(id).is_some_and(|vertex| {
        matches!(vertex.kind, VertexKind::View | VertexKind::MaterializedView)
    })
}

/// 정점이 속한 데이터셋(테이블·뷰·materialized view). 컬럼이면 부모다.
fn dataset_of(graph: &Graph, id: &VertexId) -> Option<VertexId> {
    let is_dataset = |id: &VertexId| {
        graph.vertex(id).is_some_and(|vertex| {
            matches!(
                vertex.kind,
                VertexKind::Table | VertexKind::View | VertexKind::MaterializedView
            )
        })
    };
    if is_dataset(id) {
        return Some(id.clone());
    }
    let is_column = graph
        .vertex(id)
        .is_some_and(|vertex| vertex.kind == VertexKind::Column);
    id.parent().filter(|parent| is_column && is_dataset(parent))
}

/// `derives-from`(출력 컬럼 → 입력 컬럼)을 그 값을 만든 작업에 귀속한다.
///
/// DML 계보는 origin 역할 `dml-owner:<작업>:…`이 작업을 밝히고, 그 밖의 역할
/// (뷰 정의의 `value`)이나 origin이 없으면 출력 컬럼의 부모 뷰가 작업이다. 같은
/// 간선에 둘 다 있으면(루틴이 쓰는 갱신 가능 뷰) 두 작업 모두에 귀속한다.
fn add_direct(
    graph: &Graph,
    jobs: &mut BTreeMap<VertexId, Job>,
    output: &VertexId,
    input: &VertexId,
    origins: Option<&BTreeSet<Origin>>,
) {
    let mut owners = BTreeSet::new();
    let mut defines_view = origins.is_none_or(|origins| origins.is_empty());
    for origin in origins.into_iter().flatten() {
        match dml_owner(&origin.role) {
            Some(owner) => {
                owners.insert(owner);
            }
            None => defines_view = true,
        }
    }
    if defines_view {
        owners.extend(output.parent());
    }
    let (Some(output_dataset), Some(input_dataset)) =
        (dataset_of(graph, output), dataset_of(graph, input))
    else {
        return;
    };
    for owner in owners.into_iter().filter(|owner| is_job(graph, owner)) {
        let job = jobs.entry(owner).or_default();
        job.direct
            .entry(output.clone())
            .or_default()
            .insert(input.clone());
        job.inputs.insert(input_dataset.clone());
        job.outputs.insert(output_dataset.clone());
    }
}

/// 작업의 `reads`를 입력 데이터셋과 간접 사용으로 옮긴다.
fn add_read(
    graph: &Graph,
    jobs: &mut BTreeMap<VertexId, Job>,
    job: &VertexId,
    target: &VertexId,
    origins: Option<&BTreeSet<Origin>>,
) {
    let Some(dataset) = dataset_of(graph, target) else {
        return;
    };
    let entry = jobs.entry(job.clone()).or_default();
    entry.inputs.insert(dataset);
    if graph
        .vertex(target)
        .is_none_or(|vertex| vertex.kind != VertexKind::Column)
    {
        return;
    }
    let uses = origins
        .into_iter()
        .flatten()
        .filter_map(|origin| indirect_use(&origin.role));
    for kind in uses {
        entry
            .indirect
            .entry(target.clone())
            .or_default()
            .insert(kind);
    }
}

/// `dml-owner:<작업 id>:<역할>`에서 작업 id를 꺼낸다.
fn dml_owner(role: &str) -> Option<VertexId> {
    let rest = role.strip_prefix("dml-owner:")?;
    let (owner, _) = rest.rsplit_once(':')?;
    Some(VertexId::from_raw(owner))
}

/// origin 역할을 간접 사용 종류로 옮긴다. DML 역할은 마지막 세그먼트를 본다.
fn indirect_use(role: &str) -> Option<IndirectUse> {
    match role.rsplit(':').next()? {
        "join" => Some(IndirectUse::Join),
        "predicate" => Some(IndirectUse::Filter),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use schemagraph_core::{Edge, Vertex};

    fn id(raw: &str) -> VertexId {
        VertexId::from_raw(raw)
    }

    fn origin(role: &str) -> Origin {
        Origin {
            body_hash: "sha256:test".into(),
            role: role.into(),
            location: None,
        }
    }

    /// 뷰 report(orders·customers 조인)와 orders에 쓰는 함수 load를 갖는다.
    fn fixture() -> Graph {
        let mut graph = Graph::new();
        let vertices = [
            ("s.orders", VertexKind::Table),
            ("s.orders.id", VertexKind::Column),
            ("s.orders.customer_id", VertexKind::Column),
            ("s.orders.total", VertexKind::Column),
            ("s.customers", VertexKind::Table),
            ("s.customers.id", VertexKind::Column),
            ("s.customers.name", VertexKind::Column),
            ("s.report", VertexKind::View),
            ("s.report.name", VertexKind::Column),
            ("s.staging", VertexKind::Table),
            ("s.staging.total", VertexKind::Column),
            ("s.load", VertexKind::Function),
            ("s.readonly", VertexKind::Function),
        ];
        for (raw, kind) in vertices {
            graph.add_vertex(Vertex {
                id: id(raw),
                kind,
                name: raw.rsplit('.').next().unwrap().into(),
                schema: "s".into(),
            });
        }
        let mut edge = |from: &str, to: &str, kind, role: &str| {
            graph.add_edge(Edge {
                from: id(from),
                to: id(to),
                kind,
                evidence: vec![],
            });
            graph.add_origin((id(from), id(to), kind), origin(role));
        };
        edge("s.report", "s.orders", EdgeKind::Reads, "relation");
        edge("s.report", "s.orders.customer_id", EdgeKind::Reads, "join");
        edge("s.report", "s.customers.id", EdgeKind::Reads, "join");
        edge(
            "s.report",
            "s.customers.name",
            EdgeKind::Reads,
            "projection",
        );
        edge(
            "s.report.name",
            "s.customers.name",
            EdgeKind::DerivesFrom,
            "value",
        );
        edge(
            "s.load",
            "s.staging.total",
            EdgeKind::Reads,
            "dml-owner:s.load:value",
        );
        edge(
            "s.load",
            "s.staging.total",
            EdgeKind::Reads,
            "dml-owner:s.load:predicate",
        );
        edge(
            "s.load",
            "s.orders.total",
            EdgeKind::Writes,
            "dml-owner:s.load:write",
        );
        edge(
            "s.orders.total",
            "s.staging.total",
            EdgeKind::DerivesFrom,
            "dml-owner:s.load:value",
        );
        edge("s.readonly", "s.orders.id", EdgeKind::Reads, "predicate");
        graph
    }

    #[test]
    fn view_lineage_keeps_value_sources_and_join_columns_apart() {
        let jobs = jobs(&fixture());
        let report = &jobs[&id("s.report")];
        assert_eq!(report.outputs, BTreeSet::from([id("s.report")]));
        assert_eq!(
            report.inputs,
            BTreeSet::from([id("s.customers"), id("s.orders")])
        );
        assert_eq!(
            report.direct[&id("s.report.name")],
            BTreeSet::from([id("s.customers.name")])
        );
        let joins: Vec<_> = report.indirect.keys().map(|k| k.as_str()).collect();
        assert_eq!(joins, ["s.customers.id", "s.orders.customer_id"]);
        // projection은 값 계보(derives-from)가 대신 표현하므로 간접 사용이 아니다.
        assert!(!report.indirect.contains_key(&id("s.customers.name")));
    }

    #[test]
    fn dml_lineage_is_attributed_to_the_owning_routine() {
        let jobs = jobs(&fixture());
        let load = &jobs[&id("s.load")];
        assert_eq!(load.outputs, BTreeSet::from([id("s.orders")]));
        assert_eq!(
            load.direct[&id("s.orders.total")],
            BTreeSet::from([id("s.staging.total")])
        );
        assert_eq!(
            load.indirect[&id("s.staging.total")],
            BTreeSet::from([IndirectUse::Filter])
        );
        // 뷰에 잘못 귀속되지 않는다.
        assert!(!jobs[&id("s.report")]
            .direct
            .contains_key(&id("s.orders.total")));
    }

    /// 같은 간선에 뷰 정의(value)와 루틴의 DML 소유가 함께 있으면 둘 다 계보를 가진다.
    #[test]
    fn view_value_and_dml_owner_on_one_edge_both_keep_lineage() {
        let mut graph = fixture();
        graph.add_origin(
            (
                id("s.report.name"),
                id("s.customers.name"),
                EdgeKind::DerivesFrom,
            ),
            origin("dml-owner:s.load:value"),
        );
        let jobs = jobs(&graph);
        assert!(jobs[&id("s.report")]
            .direct
            .contains_key(&id("s.report.name")));
        assert!(jobs[&id("s.load")]
            .direct
            .contains_key(&id("s.report.name")));
    }

    #[test]
    fn read_only_routines_without_outputs_are_not_jobs() {
        assert!(!jobs(&fixture()).contains_key(&id("s.readonly")));
    }
}
