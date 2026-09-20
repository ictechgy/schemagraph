//! schemagraph-analysis — 그래프 위의 판정 질의.
//!
//! 출력은 그래프 사실이지 판정이 아니다. "도달할 수 없다"고 말할 뿐
//! "지워도 된다"고는 말하지 않는다(AGENTS.md "삭제 판정 금지").

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use schemagraph_core::{EdgeKind, Graph, Level, Usage, Vertex, VertexId, VertexKind};

mod retention;
pub use retention::{RetentionPolicy, Suppression};
pub mod paths;

/// 질의 대상을 못 찾았을 때. `notFound`에도 limitations를 싣는다 —
/// 없는 것과 이 도구가 못 보는 것을 소비자가 구분해야 한다.
#[derive(Debug)]
pub enum Resolve {
    Found(VertexId),
    /// 유일 후보가 없다. 가까운 후보들을 실어준다.
    NotFound {
        candidates: Vec<VertexId>,
    },
}

/// 이름 또는 정규 id를 정점으로 해석한다.
///
/// 정확한 id가 먼저이고, 없으면 말단 이름 일치가 유일할 때만 받아들인다 —
/// 모호한 짧은 이름을 임의로 골라 잡으면 소비자가 엉뚱한 객체를 보게 된다.
pub fn resolve(graph: &Graph, name: &str) -> Resolve {
    // 정규 id 그대로 입력된 경우가 먼저다.
    let exact = VertexId::schema(name);
    if graph.vertex(&exact).is_some() {
        return Resolve::Found(exact);
    }
    // 말단 이름 일치는 유일할 때만 받아들인다 — 모호한 짧은 이름을 임의로
    // 골라 잡으면 소비자가 엉뚱한 객체를 보게 된다.
    let mut candidates: Vec<VertexId> = graph
        .vertices()
        .filter(|v| v.name == name)
        .map(|v| v.id.clone())
        .collect();
    match candidates.len() {
        1 => Resolve::Found(candidates.remove(0)),
        _ => Resolve::NotFound { candidates },
    }
}

/// 이웃 하나와 그쪽으로 가는 간선 종류들.
///
/// 이웃에 닿는 간선은 전부 준다 — 하나만 고르면 나머지 관계가 사라진다
/// (DESIGN.md "에이전트 출력 계약").
#[derive(Debug, Clone)]
pub struct Neighbor {
    pub vertex: Vertex,
    /// subject와 이 이웃 사이의 간선 종류(정렬·중복 제거).
    pub edges: Vec<EdgeKind>,
    /// subject로부터의 최단 의존 거리.
    pub distance: u32,
}

/// `query` 결과 보고.
#[derive(Debug)]
pub struct QueryReport {
    /// 해석된 대상 정점.
    pub subject: Vertex,
    /// 이 대상을 의존하는 이웃들(들어오는 방향).
    pub dependents: Vec<Neighbor>,
    /// 이 대상이 의존하는 이웃들(나가는 방향).
    pub dependencies: Vec<Neighbor>,
    /// 대상이 자기 자신을 가리키는 의존 간선의 종류(자기 참조 FK 등).
    /// 이웃 목록에 섞지 않고 따로 싣는다 — 자기를 이웃으로 세면 거리가
    /// 왜곡되고, 안 실으면 자기 의존이라는 사실이 사라진다.
    pub self_edges: Vec<EdgeKind>,
    pub depth: u32,
    /// 최대 이웃 수에 걸려 잘렸으면 true — 잘렸으면 잘렸다고 말한다.
    pub truncated: bool,
    /// 그래프의 실측 한계를 그대로 계승.
    pub limitations: Vec<String>,
}

/// BFS로 depth까지의 의존성 이웃을 모은다. `contains`는 의존성이 아니라
/// 따라가지 않는다. `inferred`는 추정이라 거리 계산에서도 제외한다 —
/// 추정 간선으로 도달성이 오염되지 않게 한다.
pub fn query(graph: &Graph, subject: &VertexId, depth: u32, max_neighbors: usize) -> QueryReport {
    let vertex = graph
        .vertex(subject)
        .expect("query는 resolve로 확인된 id만 받는다");

    let (dependents, dep_trunc) = bfs(graph, subject, depth, max_neighbors, Direction::In);
    let (dependencies, out_trunc) = bfs(graph, subject, depth, max_neighbors, Direction::Out);

    // 자기 참조는 이웃이 아니라 자기 간선으로 보고한다(cycles의 selfLoop와 같은 처리).
    let mut self_edges: Vec<EdgeKind> = graph
        .outgoing(subject)
        .iter()
        .filter(|e| e.to == *subject && e.kind.is_dependency())
        .map(|e| e.kind)
        .collect();
    self_edges.sort();
    self_edges.dedup();

    QueryReport {
        subject: vertex.clone(),
        dependents,
        dependencies,
        self_edges,
        depth,
        truncated: dep_trunc || out_trunc,
        limitations: graph.limitations().to_vec(),
    }
}

/// `impact` 보고 — 이 대상을 바꾸거나 지우면 깨지는 것들의 목록.
///
/// `query`의 dependents를 무제한 깊이로 펼친 것이다. 그래프 사실
/// ("이 정점들이 대상에 도달한다")을 보고할 뿐, 깨짐의 심각도 판정은
/// 소비자가 한다.
#[derive(Debug)]
pub struct ImpactReport {
    pub subject: Vertex,
    /// 대상에 의존 도달하는 정점들. distance가 곧 전파 거리다.
    pub impacted: Vec<Neighbor>,
    pub truncated: bool,
    pub limitations: Vec<String>,
}

/// 대상의 역방향 전이 클로저. "이 컬럼/테이블을 바꾸면 뭐가 깨지나"의 답.
pub fn impact(graph: &Graph, subject: &VertexId, max_neighbors: usize) -> ImpactReport {
    let vertex = graph
        .vertex(subject)
        .expect("impact는 resolve로 확인된 id만 받는다");
    let (impacted, truncated) = bfs(graph, subject, u32::MAX, max_neighbors, Direction::In);
    ImpactReport {
        subject: vertex.clone(),
        impacted,
        truncated,
        limitations: graph.limitations().to_vec(),
    }
}

/// `dead` 후보 하나의 판정 근거.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeadReason {
    /// DB 내부에서 이 객체를 참조하는 의존 간선이 하나도 없다.
    NoDependents,
    /// 의존자가 있었지만 전부 이미 dead 후보다 — 참조자가 죽으면 피참조도 죽는다.
    AllDependentsDead,
}

/// dead 후보 — 그래프 사실만 보고한다("지워도 된다"는 판정이 아니다).
#[derive(Debug)]
pub struct DeadCandidate {
    pub vertex: Vertex,
    pub reason: DeadReason,
    /// 사용 통계 증거 — 있다고 후보가 확정되지는 않는다(통계는 since 이후만
    /// 유효하고 애플리케이션 조회는 그래프에 없다). None = 미수집, 0 = 관측된 0.
    pub usage: Option<Usage>,
    /// 실제 후보에 적용된 명시적 예외 사유다.
    pub suppression: Option<String>,
}

/// `dead` 보고.
#[derive(Debug)]
pub struct DeadReport {
    /// id 순 정렬된 후보들.
    pub candidates: Vec<DeadCandidate>,
    pub truncated: bool,
    pub limitations: Vec<String>,
    /// 사용자가 선언한 루트와 그 의존 대상이다. 후보와 구분한다.
    pub retained: Vec<(VertexId, String)>,
    pub total_candidates: usize,
    pub unsuppressed_count: usize,
}

/// 존재 가치가 "다른 객체가 호출/조회해줘야" 생기는 kind들.
/// table은 직접 조회되는 기본 객체, trigger는 자동 발사, index/constraint/
/// sequence/type/column은 내부 장치라 후보가 아니다.
fn is_consumer_kind(kind: VertexKind) -> bool {
    matches!(
        kind,
        VertexKind::View
            | VertexKind::MaterializedView
            | VertexKind::Function
            | VertexKind::Procedure
            | VertexKind::Package
    )
}

/// DB 내부 도달성 기반 dead 후보 판정 — fixpoint.
///
/// 애플리케이션 쿼리는 그래프에 없으므로 "후보"는 "DB 내부 참조 없음"의
/// 뜻이지 "삭제 가능"의 뜻이 아니다. 이 계약은 출력 limitations에도 실린다.
pub fn dead(graph: &Graph, max_candidates: usize) -> DeadReport {
    dead_with_policy(graph, max_candidates, &RetentionPolicy::default())
}

/// 루트에서 도달하는 객체를 살려두면서 기존의 DB 내부 참조 부재 후보를 계산한다.
pub fn dead_with_policy(
    graph: &Graph,
    max_candidates: usize,
    policy: &RetentionPolicy,
) -> DeadReport {
    let protected = retention::retained(graph, policy);
    let mut dead_set: BTreeSet<VertexId> = BTreeSet::new();
    let mut reasons: BTreeMap<VertexId, DeadReason> = BTreeMap::new();
    loop {
        let mut changed = false;
        for v in graph.vertices() {
            if !is_consumer_kind(v.kind)
                || dead_set.contains(&v.id)
                || protected.contains_key(&v.id)
            {
                continue;
            }
            let dependents: Vec<&VertexId> = graph
                .incoming(&v.id)
                .iter()
                .filter(|e| e.kind.is_dependency())
                .map(|e| &e.from)
                .collect();
            let reason = if dependents.is_empty() {
                Some(DeadReason::NoDependents)
            } else if dependents.iter().all(|d| dead_set.contains(*d)) {
                Some(DeadReason::AllDependentsDead)
            } else {
                None
            };
            if let Some(r) = reason {
                dead_set.insert(v.id.clone());
                reasons.insert(v.id.clone(), r);
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    let mut candidates: Vec<DeadCandidate> = dead_set
        .iter()
        .filter_map(|id| {
            graph.vertex(id).map(|v| DeadCandidate {
                vertex: v.clone(),
                reason: reasons[id],
                usage: graph.usage(id).cloned(),
                suppression: retention::suppression(id, policy),
            })
        })
        .collect();
    candidates.sort_by(|a, b| a.vertex.id.as_str().cmp(b.vertex.id.as_str()));
    let total_candidates = candidates.len();
    let unsuppressed_count = candidates
        .iter()
        .filter(|c| c.suppression.is_none())
        .count();
    let truncated = candidates.len() > max_candidates;
    candidates.truncate(max_candidates);
    let mut limitations = graph.limitations().to_vec();
    limitations.extend(retention::policy_notes(graph, policy));
    limitations.push(
        "application queries are not in the graph — candidates mean no \
         in-database dependents, not safe to delete"
            .to_owned(),
    );
    DeadReport {
        candidates,
        truncated,
        limitations,
        retained: protected.into_iter().collect(),
        total_candidates,
        unsuppressed_count,
    }
}

enum Direction {
    In,
    Out,
}

fn bfs(
    graph: &Graph,
    root: &VertexId,
    depth: u32,
    max_neighbors: usize,
    direction: Direction,
) -> (Vec<Neighbor>, bool) {
    // 정점 → (최단 거리, 그 방향 간선 종류 집합)
    let mut seen: BTreeMap<VertexId, (u32, BTreeSet<EdgeKind>)> = BTreeMap::new();
    let mut queue: VecDeque<(VertexId, u32)> = VecDeque::new();
    queue.push_back((root.clone(), 0));
    let mut visited = BTreeSet::new();
    visited.insert(root.clone());
    let mut truncated = false;

    while let Some((id, dist)) = queue.pop_front() {
        if dist >= depth {
            continue;
        }
        let edges = match direction {
            Direction::Out => graph.outgoing(&id),
            Direction::In => graph.incoming(&id),
        };
        for edge in edges {
            if !edge.kind.is_dependency() {
                continue;
            }
            let next = match direction {
                Direction::Out => edge.to.clone(),
                Direction::In => edge.from.clone(),
            };
            if next == *root {
                continue;
            }
            seen.entry(next.clone())
                .or_insert_with(|| (dist + 1, BTreeSet::new()))
                .1
                .insert(edge.kind);
            if visited.insert(next.clone()) {
                queue.push_back((next, dist + 1));
            }
        }
    }

    let mut neighbors: Vec<Neighbor> = seen
        .into_iter()
        .filter_map(|(id, (distance, kinds))| {
            graph.vertex(&id).map(|v| Neighbor {
                vertex: v.clone(),
                edges: kinds.into_iter().collect(),
                distance,
            })
        })
        .collect();
    neighbors.sort_by(|a, b| (&a.vertex.id, a.distance).cmp(&(&b.vertex.id, b.distance)));
    if neighbors.len() > max_neighbors {
        neighbors.truncate(max_neighbors);
        truncated = true;
    }
    (neighbors, truncated)
}

/// 순환 하나. 크기 1은 자기 참조(`self_loop`)다.
#[derive(Debug)]
pub struct Cycle {
    pub members: Vec<VertexId>,
    pub self_loop: bool,
}

/// `cycles` 보고.
#[derive(Debug)]
pub struct CyclesReport {
    pub cycles: Vec<Cycle>,
    pub level: Level,
    pub limitations: Vec<String>,
}

/// 주어진 레벨로 투영한 뒤 SCC로 순환을 잡는다.
///
/// 크기>1 SCC는 순환이고, 크기 1은 자기 의존 간선이 있을 때만 순환이다
/// (SCC 알고리즘은 자기 루프도 크기 1로 돌려주므로 간선 존재를 별도 확인).
pub fn cycles(graph: &Graph, level: Level) -> CyclesReport {
    let projected = graph.project(level);
    let mut cycles: Vec<Cycle> = Vec::new();
    for component in projected.strongly_connected() {
        if component.len() > 1 {
            cycles.push(Cycle {
                members: component,
                self_loop: false,
            });
        } else {
            let id = &component[0];
            let has_self = projected
                .outgoing(id)
                .iter()
                .any(|e| e.to == *id && e.kind.is_dependency());
            if has_self {
                cycles.push(Cycle {
                    members: component,
                    self_loop: true,
                });
            }
        }
    }
    cycles.sort_by(|a, b| a.members.cmp(&b.members));
    CyclesReport {
        cycles,
        level,
        limitations: graph.limitations().to_vec(),
    }
}

// ── rules ────────────────────────────────────────────────────────────────
// 설정 파일이 선언한 금지 규칙을 그래프에 대조한다. cartograph의 레이어
// 규칙에 대응한다 — "reporting 스키마는 core를 쓰면 안 된다" 같은 선언을
// 간선 단위로 검사한다.

/// 금지 규칙 하나: `from` 글롭이 `to` 글롭을 참조하는 의존성 간선이 있으면
/// 위반이다.
#[derive(Debug, Clone)]
pub struct Rule {
    /// 규칙 이름 — 보고와 CI 로그에서 사람이 읽는 이름이다.
    pub name: String,
    /// 출발 정점 id 글롭 (`*`는 임의 문자열, `?`는 한 글자).
    pub from: String,
    /// 도착 정점 id 글롭.
    pub to: String,
    /// 검사할 간선 종류. None이면 의존성 간선 전부(Contains·Inferred 제외
    /// — 담는 관계는 쓰는 관계가 아니고, 추정은 판정 근거가 아니다).
    pub kinds: Option<BTreeSet<EdgeKind>>,
}

/// 규칙 위반 한 건 — 위반한 간선을 그대로 보고한다.
#[derive(Debug)]
pub struct RuleViolation {
    /// 위반한 규칙 이름.
    pub rule: String,
    /// 위반 간선의 출발 정점.
    pub from: VertexId,
    /// 위반 간선의 도착 정점.
    pub to: VertexId,
    /// 위반 간선의 종류.
    pub kind: EdgeKind,
}

/// `rules` 결과 보고.
#[derive(Debug)]
pub struct RulesReport {
    /// 검사한 규칙 수 — 규칙이 0이면 "통과"가 아니라 "검사한 게 없다"다.
    pub checked: usize,
    /// 규칙 이름 순으로 정렬된 위반 목록.
    pub violations: Vec<RuleViolation>,
    /// 그래프가 실은 분석 한계(파서 노트 등)를 그대로 전달한다.
    pub limitations: Vec<String>,
}

/// 모든 규칙을 그래프의 모든 간선에 대조한다.
pub fn rules(graph: &Graph, rule_set: &[Rule]) -> RulesReport {
    let mut violations = Vec::new();
    for rule in rule_set {
        for edge in graph.edges() {
            if !edge.kind.is_dependency() {
                continue;
            }
            if let Some(kinds) = &rule.kinds {
                if !kinds.contains(&edge.kind) {
                    continue;
                }
            }
            if glob_match(&rule.from, edge.from.as_str()) && glob_match(&rule.to, edge.to.as_str())
            {
                violations.push(RuleViolation {
                    rule: rule.name.clone(),
                    from: edge.from.clone(),
                    to: edge.to.clone(),
                    kind: edge.kind,
                });
            }
        }
    }
    violations.sort_by(|a, b| {
        a.rule
            .cmp(&b.rule)
            .then(a.from.cmp(&b.from))
            .then(a.to.cmp(&b.to))
    });
    RulesReport {
        checked: rule_set.len(),
        violations,
        limitations: graph.limitations().to_vec(),
    }
}

/// `diff` 보고 — 두 그래프의 구조 델타.
///
/// usage는 비교하지 않는다 — 카운트·시각은 관측 부속물이라 항상 달라 모든
/// 정점이 "changed"로 나온다. 스키마 델타는 정점·간선의 구조만이다.
#[derive(Debug)]
pub struct GraphDiff {
    /// 새 그래프에만 있는 정점.
    pub vertices_added: Vec<Vertex>,
    /// 옛 그래프에만 있는 정점.
    pub vertices_removed: Vec<Vertex>,
    /// id는 같은데 kind가 바뀐 정점.
    pub vertices_changed: Vec<VertexKindChange>,
    /// 새 그래프에만 있는 간선. evidence는 비교하지 않는다 — 같은 관계를
    /// 다른 경로로 관측한 것은 델타가 아니다.
    pub edges_added: Vec<EdgeKey>,
    /// 옛 그래프에만 있는 간선.
    pub edges_removed: Vec<EdgeKey>,
    /// 새 그래프의 분석 한계 — 지금 상태에서 못 보는 것.
    pub limitations: Vec<String>,
}

/// id는 같은데 kind가 바뀐 정점(테이블이 뷰로 바뀐 경우 등).
#[derive(Debug, Clone)]
pub struct VertexKindChange {
    pub id: VertexId,
    pub old_kind: VertexKind,
    pub new_kind: VertexKind,
}

/// 간선 비교 키 — (kind, from, to). evidence는 관측 경로라 제외한다.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct EdgeKey {
    pub kind: EdgeKind,
    pub from: VertexId,
    pub to: VertexId,
}

/// 두 그래프를 정점 id·간선 (kind,from,to) 기준으로 비교한다.
/// 컬렉션은 id/from·to 순이라 출력도 그 순서를 따른다.
pub fn diff_graph(old: &Graph, new: &Graph) -> GraphDiff {
    let old_vertices: BTreeMap<&VertexId, &Vertex> = old.vertices().map(|v| (&v.id, v)).collect();
    let new_vertices: BTreeMap<&VertexId, &Vertex> = new.vertices().map(|v| (&v.id, v)).collect();

    let mut vertices_added = Vec::new();
    let mut vertices_removed = Vec::new();
    let mut vertices_changed = Vec::new();
    for (id, nv) in &new_vertices {
        match old_vertices.get(id) {
            Some(ov) if ov.kind != nv.kind => vertices_changed.push(VertexKindChange {
                id: (*id).clone(),
                old_kind: ov.kind,
                new_kind: nv.kind,
            }),
            Some(_) => {}
            None => vertices_added.push((*nv).clone()),
        }
    }
    for (id, ov) in &old_vertices {
        if !new_vertices.contains_key(id) {
            vertices_removed.push((*ov).clone());
        }
    }

    fn keys(g: &Graph) -> BTreeSet<EdgeKey> {
        g.edges()
            .iter()
            .map(|e| EdgeKey {
                kind: e.kind,
                from: e.from.clone(),
                to: e.to.clone(),
            })
            .collect()
    }
    let old_edges = keys(old);
    let new_edges = keys(new);
    let edges_added: Vec<EdgeKey> = new_edges.difference(&old_edges).cloned().collect();
    let edges_removed: Vec<EdgeKey> = old_edges.difference(&new_edges).cloned().collect();

    GraphDiff {
        vertices_added,
        vertices_removed,
        vertices_changed,
        edges_added,
        edges_removed,
        limitations: new.limitations().to_vec(),
    }
}

/// 글롭 매칭 — `*`는 임의 길이(점 포함), `?`는 정확히 한 글자.
/// 정규식 없이 직접 매칭한다 — 정점 id가 `.`을 많이 쓰는데 `*`가 점을
/// 포함해야 `reporting.*`가 `reporting.orders.id`까지 덮는다.
pub fn glob_match(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    // 재귀 DP 대신 두 포인터 백트래킹 — 글롭 표준 알고리즘.
    let (mut pi, mut ti) = (0usize, 0usize);
    let (mut star_p, mut star_t) = (usize::MAX, 0usize);
    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star_p = pi;
            star_t = ti;
            pi += 1;
        } else if star_p != usize::MAX {
            pi = star_p + 1;
            star_t += 1;
            ti = star_t;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

#[cfg(test)]
mod tests {
    use super::*;
    use schemagraph_core::{Edge, Evidence, EvidenceLayer, Vertex, VertexKind};

    fn table(schema: &str, name: &str) -> Vertex {
        Vertex {
            id: VertexId::object(schema, name),
            kind: VertexKind::Table,
            name: name.into(),
            schema: schema.into(),
        }
    }

    fn dep(from: &VertexId, to: &VertexId) -> Edge {
        Edge {
            from: from.clone(),
            to: to.clone(),
            kind: EdgeKind::References,
            evidence: vec![Evidence {
                layer: EvidenceLayer::Catalog,
                detail: "test".into(),
            }],
        }
    }

    /// a -> b -> c 체인.
    fn chain() -> Graph {
        let mut g = Graph::new();
        for n in ["a", "b", "c"] {
            g.add_vertex(table("s", n));
        }
        g.add_edge(dep(
            &VertexId::object("s", "a"),
            &VertexId::object("s", "b"),
        ));
        g.add_edge(dep(
            &VertexId::object("s", "b"),
            &VertexId::object("s", "c"),
        ));
        g
    }

    #[test]
    fn 이름은_유일할_때만_해석된다() {
        let g = chain();
        match resolve(&g, "a") {
            Resolve::Found(id) => assert_eq!(id.as_str(), "s.a"),
            _ => panic!("resolve 실패"),
        }
        match resolve(&g, "s.a") {
            Resolve::Found(id) => assert_eq!(id.as_str(), "s.a"),
            _ => panic!("정규 id 해석 실패"),
        }
        assert!(matches!(resolve(&g, "nope"), Resolve::NotFound { .. }));
    }

    #[test]
    fn query는_양방향_이웃과_간선종류를_준다() {
        let g = chain();
        let report = query(&g, &VertexId::object("s", "b"), 2, 256);
        // a가 b를 의존 → dependents에 a.
        assert!(report
            .dependents
            .iter()
            .any(|n| n.vertex.id.as_str() == "s.a"));
        // b는 c를 의존 → dependencies에 c.
        assert!(report
            .dependencies
            .iter()
            .any(|n| n.vertex.id.as_str() == "s.c"));
        assert!(!report.truncated);
    }

    #[test]
    fn query는_contains를_타지_않는다() {
        let mut g = chain();
        g.add_vertex(Vertex {
            id: VertexId::member("s", "a", "id"),
            kind: VertexKind::Column,
            name: "id".into(),
            schema: "s".into(),
        });
        g.add_edge(Edge {
            from: VertexId::object("s", "a"),
            to: VertexId::member("s", "a", "id"),
            kind: EdgeKind::Contains,
            evidence: vec![],
        });
        let report = query(&g, &VertexId::object("s", "a"), 3, 256);
        // contains는 의존성이 아니므로 dependencies에 컬럼이 나오지 않는다.
        assert!(!report
            .dependencies
            .iter()
            .any(|n| n.vertex.id.as_str() == "s.a.id"));
    }

    #[test]
    fn cycles는_순환과_자기루프를_보고한다() {
        let mut g = chain();
        // c -> a 추가로 a->b->c->a 순환 완성.
        g.add_edge(dep(
            &VertexId::object("s", "c"),
            &VertexId::object("s", "a"),
        ));
        // 자기 참조 d.
        g.add_vertex(table("s", "d"));
        g.add_edge(dep(
            &VertexId::object("s", "d"),
            &VertexId::object("s", "d"),
        ));
        // 순환에 안 끼는 e.
        g.add_vertex(table("s", "e"));

        let report = cycles(&g, Level::Object);
        assert_eq!(report.cycles.len(), 2);
        let big = &report.cycles[0];
        assert_eq!(big.members.len(), 3);
        assert!(!big.self_loop);
        let solo = &report.cycles[1];
        assert_eq!(solo.members.len(), 1);
        assert!(solo.self_loop);
    }

    #[test]
    fn impact는_전이_의존자를_전부_모은다() {
        // a -> b -> c: c를 바꾸면 b와 a 둘 다 깨진다.
        let g = chain();
        let report = impact(&g, &VertexId::object("s", "c"), 256);
        assert_eq!(report.impacted.len(), 2);
        // distance 1이 b, distance 2가 a.
        let b = report
            .impacted
            .iter()
            .find(|n| n.vertex.id.as_str() == "s.b")
            .unwrap();
        let a = report
            .impacted
            .iter()
            .find(|n| n.vertex.id.as_str() == "s.a")
            .unwrap();
        assert_eq!(b.distance, 1);
        assert_eq!(a.distance, 2);
        // a를 바꾸면 아무것도 안 깨진다.
        let report_a = impact(&g, &VertexId::object("s", "a"), 256);
        assert!(report_a.impacted.is_empty());
    }

    #[test]
    fn member의_순환은_object로_투영해_잡는다() {
        let mut g = Graph::new();
        for (t, c) in [("a", "x"), ("b", "y")] {
            g.add_vertex(table("s", t));
            g.add_vertex(Vertex {
                id: VertexId::member("s", t, c),
                kind: VertexKind::Column,
                name: c.into(),
                schema: "s".into(),
            });
        }
        g.add_edge(dep(
            &VertexId::member("s", "a", "x"),
            &VertexId::member("s", "b", "y"),
        ));
        g.add_edge(dep(
            &VertexId::member("s", "b", "y"),
            &VertexId::member("s", "a", "x"),
        ));
        // object 레벨에서도 순환이 보여야 한다.
        let report = cycles(&g, Level::Object);
        assert_eq!(report.cycles.len(), 1);
        assert_eq!(report.cycles[0].members.len(), 2);
    }

    fn view(schema: &str, name: &str) -> Vertex {
        Vertex {
            id: VertexId::object(schema, name),
            kind: VertexKind::View,
            name: name.into(),
            schema: schema.into(),
        }
    }

    fn reads(from: &VertexId, to: &VertexId) -> Edge {
        Edge {
            kind: EdgeKind::Reads,
            ..dep(from, to)
        }
    }

    #[test]
    fn dead는_아무도_안_보는_view를_후보로_잡는다() {
        // t는 테이블이라 후보 아님. v1은 t를 읽지만 아무도 v1을 안 읽는다.
        let mut g = Graph::new();
        g.add_vertex(table("s", "t"));
        g.add_vertex(view("s", "v1"));
        g.add_edge(reads(
            &VertexId::object("s", "v1"),
            &VertexId::object("s", "t"),
        ));
        let report = dead(&g, 256);
        assert_eq!(report.candidates.len(), 1);
        assert_eq!(report.candidates[0].vertex.id.as_str(), "s.v1");
        assert_eq!(report.candidates[0].reason, DeadReason::NoDependents);
        // 삭제 판정 금지 계약이 limitations에 실려 있어야 한다.
        assert!(report
            .limitations
            .iter()
            .any(|l| l.contains("not safe to delete")));
    }

    #[test]
    fn dead는_dead만을_보는_view까지_연쇄로_잡는다() {
        // v_dead -> v_mid -> t: v_dead를 아무도 안 보고(씨앗),
        // v_mid의 유일한 의존자가 v_dead라 연쇄로 죽는다.
        let mut g = Graph::new();
        g.add_vertex(table("s", "t"));
        g.add_vertex(view("s", "v_mid"));
        g.add_vertex(view("s", "v_dead"));
        g.add_edge(reads(
            &VertexId::object("s", "v_mid"),
            &VertexId::object("s", "t"),
        ));
        g.add_edge(reads(
            &VertexId::object("s", "v_dead"),
            &VertexId::object("s", "v_mid"),
        ));
        let report = dead(&g, 256);
        assert_eq!(report.candidates.len(), 2);
        let mid = report
            .candidates
            .iter()
            .find(|c| c.vertex.id.as_str() == "s.v_mid")
            .unwrap();
        assert_eq!(mid.reason, DeadReason::AllDependentsDead);
    }

    #[test]
    fn dead_후보는_usage_증거를_같이_싣는다() {
        let mut g = Graph::new();
        g.add_vertex(view("s", "v_obs"));
        g.add_vertex(view("s", "v_none"));
        // v_obs는 관측된 0 — 미수집(v_none의 None)과 구분되어야 한다.
        g.set_usage(
            VertexId::object("s", "v_obs"),
            Usage {
                since: Some("2025-01-01".into()),
                reads: 0,
                writes: 0,
                total_ms: None,
                self_ms: None,
            },
        );
        let report = dead(&g, 256);
        assert_eq!(report.candidates.len(), 2);
        let without = report
            .candidates
            .iter()
            .find(|c| c.vertex.id.as_str() == "s.v_none")
            .unwrap();
        assert!(without.usage.is_none());
        let with_usage = report
            .candidates
            .iter()
            .find(|c| c.vertex.id.as_str() == "s.v_obs")
            .unwrap();
        assert_eq!(with_usage.usage.as_ref().map(|u| u.reads), Some(0));
    }

    #[test]
    fn 살아있는_의존자가_있으면_후보가_아니다() {
        // trigger가 v1을 읽는다 — trigger는 자동 발사라 죽지 않으므로 v1은 산다.
        let mut g = Graph::new();
        g.add_vertex(table("s", "t"));
        g.add_vertex(view("s", "v1"));
        g.add_vertex(Vertex {
            id: VertexId::member("s", "t", "trg"),
            kind: VertexKind::Trigger,
            name: "trg".into(),
            schema: "s".into(),
        });
        g.add_edge(reads(
            &VertexId::member("s", "t", "trg"),
            &VertexId::object("s", "v1"),
        ));
        let report = dead(&g, 256);
        assert!(report.candidates.is_empty());
    }

    #[test]
    fn glob은_별이_점까지_덮고_물음표는_한글자다() {
        assert!(glob_match("reporting.*", "reporting.orders"));
        // `*`는 점도 덮는다 — member id까지 매치돼야 스키마 규칙이 컬럼에도 적용된다.
        assert!(glob_match("reporting.*", "reporting.orders.id"));
        assert!(glob_match("*", "anything.at.all"));
        assert!(!glob_match("core.*", "corex.orders"));
        assert!(glob_match("s.t?", "s.t1"));
        assert!(!glob_match("s.t?", "s.t12"));
        assert!(glob_match("a*b*c", "aXbYc"));
        assert!(!glob_match("a*b", "ab_")); // 패턴은 'b'로 끝나야 한다
        assert!(glob_match("a*b", "axb"));
        assert!(!glob_match("", "x"));
        assert!(glob_match("", ""));
    }

    #[test]
    fn rules는_금지_간선을_규칙별로_보고한다() {
        let mut g = Graph::new();
        g.add_vertex(table("reporting", "rpt"));
        g.add_vertex(table("core", "cust"));
        g.add_vertex(table("core", "ord"));
        // reporting.rpt -> core.cust (reads) — 금지 대상.
        g.add_edge(reads(
            &VertexId::object("reporting", "rpt"),
            &VertexId::object("core", "cust"),
        ));
        // reporting.rpt -> core.ord (contains 아닌 writes) — kinds 필터 밖이면 통과.
        let mut w = dep(
            &VertexId::object("reporting", "rpt"),
            &VertexId::object("core", "ord"),
        );
        w.kind = EdgeKind::Writes;
        g.add_edge(w);

        // 규칙 1: reporting.* -> core.* 모든 의존성 — 2건 위반.
        let r_all = Rule {
            name: "reporting→core 금지".into(),
            from: "reporting.*".into(),
            to: "core.*".into(),
            kinds: None,
        };
        let report = rules(&g, &[r_all]);
        assert_eq!(report.checked, 1);
        assert_eq!(report.violations.len(), 2);

        // 규칙 2: kinds=[writes] — writes 간선만 위반.
        let r_writes = Rule {
            name: "reporting→core 쓰기 금지".into(),
            from: "reporting.*".into(),
            to: "core.*".into(),
            kinds: Some(BTreeSet::from([EdgeKind::Writes])),
        };
        let report2 = rules(&g, &[r_writes]);
        assert_eq!(report2.violations.len(), 1);
        assert_eq!(report2.violations[0].kind, EdgeKind::Writes);
    }

    #[test]
    fn rules는_contains와_inferred는_간선으로_세지_않는다() {
        let mut g = Graph::new();
        g.add_vertex(table("reporting", "rpt"));
        g.add_vertex(table("core", "cust"));
        let mut contains = dep(
            &VertexId::object("reporting", "rpt"),
            &VertexId::object("core", "cust"),
        );
        contains.kind = EdgeKind::Contains;
        g.add_edge(contains);
        let r = Rule {
            name: "r".into(),
            from: "*".into(),
            to: "*".into(),
            kinds: None,
        };
        assert!(rules(&g, &[r]).violations.is_empty());
    }

    #[test]
    fn diff는_정점과_간선의_구조_델타를_잡는다() {
        let old = chain();
        let mut new = Graph::new();
        // b를 view로 바꾸고(kind change), d를 추가하고, a→b 간선을 뺀다.
        for n in ["a", "c"] {
            new.add_vertex(table("s", n));
        }
        let mut view_b = table("s", "b");
        view_b.kind = VertexKind::View;
        new.add_vertex(view_b);
        new.add_vertex(table("s", "d"));
        new.add_edge(dep(
            &VertexId::object("s", "b"),
            &VertexId::object("s", "c"),
        ));

        let report = diff_graph(&old, &new);
        assert!(report.vertices_added.iter().any(|v| v.id.as_str() == "s.d"));
        assert!(report
            .vertices_changed
            .iter()
            .any(|c| c.id.as_str() == "s.b"
                && c.old_kind == VertexKind::Table
                && c.new_kind == VertexKind::View));
        assert!(report.edges_removed.iter().any(|e| e.from.as_str() == "s.a"
            && e.to.as_str() == "s.b"
            && e.kind == EdgeKind::References));
        assert!(report.edges_added.is_empty() && report.vertices_removed.is_empty());
    }

    #[test]
    fn diff는_usage_차이를_델타로_세지_않는다() {
        let mut old = Graph::new();
        let mut new = Graph::new();
        for g in [&mut old, &mut new] {
            g.add_vertex(table("s", "t"));
        }
        old.set_usage(
            VertexId::object("s", "t"),
            Usage {
                since: Some("2026-01-01".into()),
                reads: 10,
                writes: 1,
                total_ms: None,
                self_ms: None,
            },
        );
        // new는 usage가 아예 없다 — 관측 부속물의 차이는 델타가 아니다.
        let report = diff_graph(&old, &new);
        assert!(
            report.vertices_changed.is_empty()
                && report.vertices_added.is_empty()
                && report.vertices_removed.is_empty()
        );
    }
}
