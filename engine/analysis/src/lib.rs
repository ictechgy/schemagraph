//! schemagraph-analysis — 그래프 위의 판정 질의.
//!
//! 출력은 그래프 사실이지 판정이 아니다. "도달할 수 없다"고 말할 뿐
//! "지워도 된다"고는 말하지 않는다(AGENTS.md "삭제 판정 금지").

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use schemagraph_core::{Edge, EdgeKind, Graph, Level, Usage, Vertex, VertexId, VertexKind};

mod retention;
pub use retention::{RetentionPolicy, Suppression};
pub mod budget;
pub mod lineage;
pub mod paths;
pub mod review;
pub mod schema_lint;
pub mod search;
pub mod traversal;
pub mod unused;

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
    // 사용자가 건넨 canonical id는 이미 schema/object/member 구분자를
    // 포함하므로 다시 schema 컴포넌트로 escape하면 안 된다. v1 graph의
    // raw id도 from_raw로 그대로 읽어 compatibility를 유지한다.
    let exact = VertexId::from_raw(name);
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
    /// 탐색 중 이 이웃에 닿은 모든 의존 간선의 종류(정렬·중복 제거) — via 간선 하나만을
    /// 뜻하지 않고, 확장된 어느 정점에서 온 간선이든 거리와 무관하게 모은다.
    pub edges: Vec<EdgeKind>,
    /// subject로부터의 최단 의존 거리.
    pub distance: u32,
    /// subject에서 이 이웃까지 최단 경로의 직전 정점(BFS 부모)이다.
    ///
    /// 거리 1 이웃은 subject 자신이다. 같은 최단 거리의 부모가 여럿이면 id의
    /// 사전식(바이트) 최소를 고른다 — 그래프 삽입 순서에 따라 달라지면 같은
    /// 그래프가 다른 리포트가 되어 diff와 캐시가 무의미해진다. via를 따라
    /// 거슬러 올라가면 subject에 닿는 최단 경로 하나가 복원된다. 결과 개수
    /// 제한으로 잘린 목록에서는 via 정점이 목록에 없을 수 있다.
    pub via: VertexId,
}

/// BFS가 발견한 정점 하나의 누적 기록 — `bfs`와 `budget::walk`가 공유한다.
///
/// 두 탐색이 같은 규칙으로 거리·간선 종류·via를 쌓아야 예산 없는 walk가
/// query와 같은 이웃을 낸다(budget 테스트가 이 동치를 고정한다).
#[derive(Debug)]
pub(crate) struct Reach {
    distance: u32,
    edges: BTreeSet<EdgeKind>,
    via: VertexId,
}

impl Reach {
    /// 탐색 루트의 기록이다. 루트는 이웃으로 내보내지 않으므로 via는 자기 자신이다.
    pub(crate) fn root(root: &VertexId) -> Self {
        Self {
            distance: 0,
            edges: BTreeSet::new(),
            via: root.clone(),
        }
    }

    /// `parent`에서 `kind` 간선으로 처음 발견한 정점의 기록이다.
    ///
    /// BFS는 거리 순으로 정점을 꺼내므로 첫 발견 거리가 곧 최단 거리다.
    pub(crate) fn discovered(parent: &VertexId, parent_distance: u32, kind: EdgeKind) -> Self {
        Self {
            distance: parent_distance + 1,
            edges: BTreeSet::from([kind]),
            via: parent.clone(),
        }
    }

    /// 이미 발견한 정점에 `parent`의 `kind` 간선이 또 닿았을 때 기록을 갱신한다.
    ///
    /// 간선 종류는 거리와 무관하게 모두 모은다(기존 `edges` 계약). via는 같은
    /// 최단 거리의 부모끼리만 비교해 사전식 최소 id를 남긴다 — 더 먼 부모를
    /// 거치는 경로는 최단 경로가 아니다.
    pub(crate) fn observe(&mut self, parent: &VertexId, parent_distance: u32, kind: EdgeKind) {
        self.edges.insert(kind);
        if parent_distance + 1 == self.distance && *parent < self.via {
            self.via = parent.clone();
        }
    }

    /// 루트로부터의 최단 거리.
    pub(crate) fn distance(&self) -> u32 {
        self.distance
    }

    /// 누적 기록을 출력용 이웃으로 바꾼다.
    pub(crate) fn into_neighbor(self, vertex: Vertex) -> Neighbor {
        Neighbor {
            vertex,
            edges: self.edges.into_iter().collect(),
            distance: self.distance,
            via: self.via,
        }
    }
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

#[cfg(test)]
thread_local! {
    /// dead 판정이 훑은 인접 간선 수. 테스트 스레드마다 따로 센다.
    static DEAD_EDGE_VISITS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// dead 판정이 인접 간선을 훑은 수를 더한다.
#[cfg(test)]
fn count_dead_edge_visits(count: usize) {
    DEAD_EDGE_VISITS.with(|visits| visits.set(visits.get() + count));
}

/// 현재 테스트 스레드의 dead 인접 간선 방문 수를 돌려주고 0으로 되돌린다.
#[cfg(test)]
fn take_dead_edge_visits() -> usize {
    DEAD_EDGE_VISITS.with(std::cell::Cell::take)
}

/// dead 후보를 작업 목록으로 전파하는 상태다.
///
/// 변화가 없을 때까지 정점 전체를 다시 도는 고정점은 패스마다의 진전이 id 정렬 순서에
/// 달려, 소비자 id가 의존 대상 id보다 사전식으로 뒤에 오는 view 사슬에서 패스마다 하나만
/// 확정해 O(정점 × 사슬 길이)가 된다. 그래서 후보가 될 수 있는 정점마다 아직 후보가 아닌
/// 서로 다른 의존자 수를 세고, 0이 된 정점만 의존 대상에 전파해 O(정점 + 간선)로 같은 최소
/// 고정점을 구한다. 비소비자·보존·유령 의존자와 순환 구성원은 후보가 되지 않으므로, 그들이
/// 의존하는 정점은 반복 고정점에서처럼 살아남는다.
struct DeadPropagation<'g> {
    /// 아직 후보가 아닌 정점 → 후보가 아닌 서로 다른 의존자 수. 의존자가 있는 정점만 담는다.
    remaining: BTreeMap<&'g VertexId, usize>,
    /// 후보로 확정했지만 아직 의존 대상에 전파하지 않은 정점.
    queue: VecDeque<&'g VertexId>,
    /// 확정한 후보와 사유.
    reasons: BTreeMap<VertexId, DeadReason>,
    /// 모호한 호출이 가리킬 수 있는 후보 관계 — 간선처럼 의존자로 센다.
    possible: PossibleCalls<'g>,
}

/// 모호한 오버로드 호출처럼 하나로 좁히지 못한 호출의 호출자↔후보 관계다.
///
/// 확정 간선이 아니라 query/impact/cycles에는 섞지 않는다. 다만 후보 중 하나는 실제로
/// 불리므로, dead에서 후보를 의존자 없는 정점으로 내면 쓰이는 routine을 후보로 오보한다.
/// 그래서 dead 전파에서만 호출자를 후보의 의존자로 센다 — 호출자가 살아 있으면 후보도
/// 살고, 호출자가 후보가 되면 후보도 그 뒤를 따른다. 그래프에 없는 후보는 무시한다.
struct PossibleCalls<'g> {
    /// 호출자 → 그 호출이 가리킬 수 있는 정점.
    callees: BTreeMap<&'g VertexId, BTreeSet<&'g VertexId>>,
    /// 후보 정점 → 그것을 가리킬 수 있는 호출자.
    callers: BTreeMap<&'g VertexId, BTreeSet<&'g VertexId>>,
}

impl<'g> PossibleCalls<'g> {
    /// 분석 진단에 실린 후보 정점에서 호출자↔후보 관계를 모은다.
    fn new(graph: &'g Graph) -> Self {
        let mut possible = Self {
            callees: BTreeMap::new(),
            callers: BTreeMap::new(),
        };
        for (owner, analysis) in graph.analysis() {
            let candidates = analysis
                .diagnostics
                .iter()
                .flat_map(|diagnostic| &diagnostic.candidates)
                .filter_map(|candidate| graph.vertex(candidate).map(|vertex| &vertex.id));
            for candidate in candidates {
                possible.callees.entry(owner).or_default().insert(candidate);
                possible.callers.entry(candidate).or_default().insert(owner);
            }
        }
        possible
    }

    /// `id`를 가리킬 수 있는 호출자.
    fn callers_of(&self, id: &VertexId) -> impl Iterator<Item = &'g VertexId> + '_ {
        self.callers.get(id).into_iter().flatten().copied()
    }

    /// `id`의 모호한 호출이 가리킬 수 있는 정점.
    fn callees_of(&self, id: &VertexId) -> impl Iterator<Item = &'g VertexId> + '_ {
        self.callees.get(id).into_iter().flatten().copied()
    }

    /// 실측 limitation — 모호한 호출의 후보가 있을 때만 그 수를 싣는다.
    fn limitation(&self) -> Option<String> {
        (!self.callers.is_empty()).then(|| {
            format!(
                "routines named by ambiguous overloaded calls: {}; each counts as called \
                 until a single target is resolved",
                self.callers.len()
            )
        })
    }
}

impl<'g> DeadPropagation<'g> {
    /// 보존되지 않은 소비자 정점의 의존자 수를 세고, 의존자가 없는 정점은 바로 후보로 둔다.
    fn seed(
        graph: &'g Graph,
        protected: &BTreeMap<VertexId, String>,
        possible: PossibleCalls<'g>,
    ) -> Self {
        let mut state = Self {
            remaining: BTreeMap::new(),
            queue: VecDeque::new(),
            reasons: BTreeMap::new(),
            possible,
        };
        let consumers = graph
            .vertices()
            .filter(|v| is_consumer_kind(v.kind) && !protected.contains_key(&v.id));
        for vertex in consumers {
            let mut dependents = dependency_ends(graph.incoming(&vertex.id), |edge| &edge.from);
            dependents.extend(state.possible.callers_of(&vertex.id));
            match dependents.len() {
                0 => state.confirm(&vertex.id, DeadReason::NoDependents),
                count => {
                    state.remaining.insert(&vertex.id, count);
                }
            }
        }
        state
    }

    /// 후보를 확정하고 전파 대기열에 넣는다.
    fn confirm(&mut self, id: &'g VertexId, reason: DeadReason) {
        self.reasons.insert(id.clone(), reason);
        self.queue.push_back(id);
    }

    /// 확정한 후보마다 의존 대상의 남은 의존자 수를 한 번씩 줄여 고정점까지 전파한다.
    fn run(mut self, graph: &'g Graph) -> BTreeMap<VertexId, DeadReason> {
        while let Some(id) = self.queue.pop_front() {
            // 의존자 수를 셀 때와 같은 기준 — 확정 간선과 모호한 호출의 후보를 한 집합으로.
            let mut targets = dependency_ends(graph.outgoing(id), |edge| &edge.to);
            targets.extend(self.possible.callees_of(id));
            for target in targets {
                self.release(target);
            }
        }
        self.reasons
    }

    /// 의존자 하나가 후보가 됐음을 반영한다. 남은 의존자가 없으면 그 정점도 후보다.
    fn release(&mut self, target: &'g VertexId) {
        let Some(count) = self.remaining.get_mut(target) else {
            return;
        };
        // 후보마다 한 번, 서로 다른 대상마다 한 번만 불리므로 처음 센 수보다 많이 줄지 않는다.
        *count -= 1;
        if *count == 0 {
            self.remaining.remove(target);
            self.confirm(target, DeadReason::AllDependentsDead);
        }
    }
}

/// 의존 간선(`contains`·`inferred` 제외)의 서로 다른 반대쪽 끝이다. 같은 객체가 여러 종류의
/// 간선으로 이어져도 하나로 센다 — 의존자 수와 전파 횟수가 같은 기준이어야 한다.
fn dependency_ends<'g>(
    edges: &'g [Edge],
    end: impl Fn(&'g Edge) -> &'g VertexId,
) -> BTreeSet<&'g VertexId> {
    #[cfg(test)]
    count_dead_edge_visits(edges.len());
    edges
        .iter()
        .filter(|edge| edge.kind.is_dependency())
        .map(end)
        .collect()
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
    let possible = PossibleCalls::new(graph);
    let possible_note = possible.limitation();
    let reasons = DeadPropagation::seed(graph, &protected, possible).run(graph);
    let mut candidates: Vec<DeadCandidate> = reasons
        .iter()
        .filter_map(|(id, reason)| {
            graph.vertex(id).map(|v| DeadCandidate {
                vertex: v.clone(),
                reason: *reason,
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
    limitations.extend(possible_note);
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
    // 정점 → (최단 거리, 그 방향 간선 종류 집합, 최단 경로 부모)
    let mut seen: BTreeMap<VertexId, Reach> = BTreeMap::new();
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
            // 등록되지 않은 id를 가리키는 간선은 따라가지 않는다 — 유령 정점이
            // 다른 이웃의 via로 노출되면 안 된다(budget::walk와 같은 규칙).
            if graph.vertex(&next).is_none() {
                continue;
            }
            match seen.get_mut(&next) {
                Some(reach) => reach.observe(&id, dist, edge.kind),
                None => {
                    seen.insert(next.clone(), Reach::discovered(&id, dist, edge.kind));
                }
            }
            if visited.insert(next.clone()) {
                queue.push_back((next, dist + 1));
            }
        }
    }

    let mut neighbors: Vec<Neighbor> = seen
        .into_iter()
        .filter_map(|(id, reach)| graph.vertex(&id).map(|v| reach.into_neighbor(v.clone())))
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

/// rules 작업량을 시간 대신 횟수로 확인하게 하는 테스트 전용 계수다.
#[cfg(test)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct RulesWork {
    /// 그래프 간선 목록을 모아 정렬한 횟수.
    edge_lists: usize,
    /// 글롭 패턴을 문자 목록으로 준비한 횟수.
    patterns: usize,
}

#[cfg(test)]
thread_local! {
    /// 테스트는 스레드마다 병렬로 돌므로 계수도 스레드마다 따로 둔다.
    static RULES_WORK: std::cell::Cell<RulesWork> =
        const { std::cell::Cell::new(RulesWork { edge_lists: 0, patterns: 0 }) };
}

/// 현재 테스트 스레드의 rules 계수를 갱신한다.
#[cfg(test)]
fn count_rules_work(update: impl FnOnce(&mut RulesWork)) {
    RULES_WORK.with(|work| {
        let mut value = work.get();
        update(&mut value);
        work.set(value);
    });
}

/// 현재 테스트 스레드의 rules 계수를 돌려주고 0으로 되돌린다.
#[cfg(test)]
fn take_rules_work() -> RulesWork {
    RULES_WORK.with(std::cell::Cell::take)
}

/// 모든 규칙을 그래프의 모든 간선에 대조한다.
///
/// 의존 간선 목록과 규칙별 글롭 패턴은 한 번씩만 준비한다. 규칙마다 전체 간선을 다시 모아
/// 정렬하고 간선마다 패턴 문자를 다시 모으면, 큰 그래프의 CI 게이트가 규칙 수에 비례해
/// 느려진다. 간선은 규칙마다 같은 (from, to, kind) 순서로 돌므로 위반 순서도 그대로다.
pub fn rules(graph: &Graph, rule_set: &[Rule]) -> RulesReport {
    #[cfg(test)]
    count_rules_work(|work| work.edge_lists += 1);
    let edges: Vec<&Edge> = graph
        .edges()
        .into_iter()
        .filter(|edge| edge.kind.is_dependency())
        .collect();
    let mut violations = Vec::new();
    for rule in rule_set {
        push_rule_violations(rule, &edges, &mut violations);
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

/// 규칙 하나를 의존 간선 목록에 대조해 위반을 간선 순서대로 더한다.
fn push_rule_violations(rule: &Rule, edges: &[&Edge], violations: &mut Vec<RuleViolation>) {
    let (from, to) = (GlobPattern::new(&rule.from), GlobPattern::new(&rule.to));
    let selected = edges.iter().filter(|edge| {
        rule.kinds
            .as_ref()
            .is_none_or(|kinds| kinds.contains(&edge.kind))
    });
    for edge in selected {
        if from.matches(edge.from.as_str()) && to.matches(edge.to.as_str()) {
            violations.push(RuleViolation {
                rule: rule.name.clone(),
                from: edge.from.clone(),
                to: edge.to.clone(),
                kind: edge.kind,
            });
        }
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
    GlobPattern::new(pattern).matches(text)
}

/// 한 번 준비해 여러 문자열에 대조하는 글롭 패턴이다.
///
/// 패턴 문자를 매칭마다 다시 모으지 않으려고 둔다. `?`는 바이트가 아니라 유니코드
/// 스칼라 하나와 맞는다 — 비ASCII 식별자에서도 `glob_match`와 같은 뜻이어야 한다.
struct GlobPattern(Vec<char>);

impl GlobPattern {
    /// 패턴 문자를 한 번 모은다.
    fn new(pattern: &str) -> Self {
        #[cfg(test)]
        count_rules_work(|work| work.patterns += 1);
        Self(pattern.chars().collect())
    }

    /// 재귀 DP 대신 두 포인터 백트래킹 — 글롭 표준 알고리즘.
    ///
    /// 패턴 위치는 문자 단위, 문자열 위치는 문자 경계의 바이트 단위라서 문자열을 문자
    /// 목록으로 복사하지 않는다.
    fn matches(&self, text: &str) -> bool {
        let pattern = self.0.as_slice();
        let (mut pi, mut ti) = (0usize, 0usize);
        let (mut star_p, mut star_t) = (usize::MAX, 0usize);
        while let Some(current) = text[ti..].chars().next() {
            if pi < pattern.len() && (pattern[pi] == '?' || pattern[pi] == current) {
                pi += 1;
                ti += current.len_utf8();
            } else if pi < pattern.len() && pattern[pi] == '*' {
                star_p = pi;
                star_t = ti;
                pi += 1;
            } else if star_p != usize::MAX {
                pi = star_p + 1;
                // star_t ≤ ti < text.len()이고 둘 다 문자 경계라 star_t 자리에는 항상 글자가 있다.
                let skipped = text[star_t..]
                    .chars()
                    .next()
                    .expect("star_t는 ti 이하의 문자 경계다");
                star_t += skipped.len_utf8();
                ti = star_t;
            } else {
                return false;
            }
        }
        while pi < pattern.len() && pattern[pi] == '*' {
            pi += 1;
        }
        pi == pattern.len()
    }
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

    /// via 규칙(최단 거리 부모만, 동률이면 사전식 최소)을 가르는 그래프다.
    /// `reversed`면 간선을 반대 순서로 넣어 삽입 순서와 무관한지 본다.
    ///
    /// ```text
    /// z_near, a_near -> s               거리 1
    /// x -> z_near, x -> a_near          거리 2, 부모 동률 → a_near
    /// q1 -> a_near, b1 -> z_near        거리 2
    /// w -> q1, w -> b1                  거리 3, 부모 동률 → b1 (walk는 q1에서 먼저 발견)
    /// a_far -> x                        거리 3
    /// y -> z_near, y -> a_far           거리 2 — a_far가 사전식으로 작아도 더 멀다
    /// ```
    pub(crate) fn via_fixture(reversed: bool) -> Graph {
        let mut g = Graph::new();
        for name in ["s", "a_near", "z_near", "x", "q1", "b1", "w", "a_far", "y"] {
            g.add_vertex(table("s", name));
        }
        let mut edges = vec![
            ("z_near", "s"),
            ("a_near", "s"),
            ("x", "z_near"),
            ("x", "a_near"),
            ("q1", "a_near"),
            ("b1", "z_near"),
            ("w", "q1"),
            ("w", "b1"),
            ("a_far", "x"),
            ("y", "z_near"),
            ("y", "a_far"),
        ];
        if reversed {
            edges.reverse();
        }
        for (from, to) in edges {
            g.add_edge(dep(
                &VertexId::object("s", from),
                &VertexId::object("s", to),
            ));
        }
        g
    }

    /// via_fixture에서 s의 impact가 내야 할 (id, distance, via)다 — 손으로 도출했다.
    pub(crate) const VIA_EXPECTED: &[(&str, u32, &str)] = &[
        ("s.a_far", 3, "s.x"),
        ("s.a_near", 1, "s.s"),
        ("s.b1", 2, "s.z_near"),
        ("s.q1", 2, "s.a_near"),
        ("s.w", 3, "s.b1"),
        ("s.x", 2, "s.a_near"),
        ("s.y", 2, "s.z_near"),
        ("s.z_near", 1, "s.s"),
    ];

    /// 이웃 목록을 (id, distance, via)로 줄인다.
    pub(crate) fn via_triples(neighbors: &[Neighbor]) -> Vec<(&str, u32, &str)> {
        neighbors
            .iter()
            .map(|n| (n.vertex.id.as_str(), n.distance, n.via.as_str()))
            .collect()
    }

    #[test]
    fn impact의_via는_최단_부모_중_사전식_최소이고_삽입_순서와_무관하다() {
        for reversed in [false, true] {
            let g = via_fixture(reversed);
            let report = impact(&g, &VertexId::object("s", "s"), 256);
            assert_eq!(via_triples(&report.impacted), VIA_EXPECTED, "{reversed}");
        }
    }

    #[test]
    fn query의_via는_방향마다_subject에서_거슬러_올라간다() {
        let g = chain();
        let report = query(&g, &VertexId::object("s", "a"), 2, 256);
        // a -> b -> c: 의존 방향으로 b의 부모는 a, c의 부모는 b다.
        assert_eq!(
            via_triples(&report.dependencies),
            [("s.b", 1, "s.a"), ("s.c", 2, "s.b")]
        );
        let report = query(&g, &VertexId::object("s", "c"), 2, 256);
        assert_eq!(
            via_triples(&report.dependents),
            [("s.a", 2, "s.b"), ("s.b", 1, "s.c")]
        );
    }

    #[test]
    fn 등록되지_않은_정점은_via로_노출되지_않는다() {
        let mut g = chain();
        g.add_vertex(table("s", "x"));
        // ghost는 정점 없이 간선에만 있는 id다. x는 ghost를 거쳐야만 c에 닿는다.
        let ghost = VertexId::object("s", "ghost");
        g.add_edge(dep(&ghost, &VertexId::object("s", "c")));
        g.add_edge(dep(&VertexId::object("s", "x"), &ghost));
        let report = impact(&g, &VertexId::object("s", "c"), 256);
        assert_eq!(
            via_triples(&report.impacted),
            [("s.a", 2, "s.b"), ("s.b", 1, "s.c")]
        );
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
                scans: None,
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

    /// 길이 `length`인 view 사슬 — 위치 i의 view가 i-1을 읽고 위치 0이 테이블을 읽는다.
    ///
    /// `ascending_consumers`면 소비자 id가 의존 대상 id보다 사전식으로 뒤에 온다. id 순으로
    /// 정점 전체를 다시 도는 고정점은 이 순서에서 패스마다 하나만 확정한다.
    fn view_chain(length: usize, ascending_consumers: bool) -> Graph {
        let name = |position: usize| {
            let rank = if ascending_consumers {
                position
            } else {
                length - 1 - position
            };
            format!("v_{rank:06}")
        };
        let mut g = Graph::new();
        g.add_vertex(table("s", "base"));
        for position in 0..length {
            let current = name(position);
            g.add_vertex(view("s", &current));
            let target = match position {
                0 => "base".to_owned(),
                _ => name(position - 1),
            };
            g.add_edge(reads(
                &VertexId::object("s", &current),
                &VertexId::object("s", &target),
            ));
        }
        g
    }

    #[test]
    fn dead_연쇄는_이름_순서와_무관하게_간선을_상수_번만_본다() {
        for ascending in [true, false] {
            let g = view_chain(200, ascending);
            take_dead_edge_visits();
            let report = dead(&g, 1024);
            let visits = take_dead_edge_visits();
            assert_eq!(report.total_candidates, 200, "{ascending}");
            // 정점 201개·간선 200개 — 패스를 반복하면 사슬 길이의 제곱이 된다.
            assert!(visits <= 2 * (201 + 200), "{ascending}: {visits}");
        }
    }

    #[test]
    fn dead_만_길이_view_사슬은_두_이름_순서에서_같은_후보와_사유를_낸다() {
        for ascending in [true, false] {
            let report = dead(&view_chain(10_000, ascending), 10_000);
            assert_eq!(report.total_candidates, 10_000, "{ascending}");
            assert!(!report.truncated);
            let seeds: Vec<_> = report
                .candidates
                .iter()
                .filter(|c| c.reason == DeadReason::NoDependents)
                .map(|c| c.vertex.id.as_str())
                .collect();
            // 의존자가 없는 것은 사슬 맨 위 소비자 하나뿐이고 나머지는 연쇄로 죽는다.
            let top = if ascending {
                "s.v_009999"
            } else {
                "s.v_000000"
            };
            assert_eq!(seeds, [top]);
            assert!(report
                .candidates
                .windows(2)
                .all(|pair| pair[0].vertex.id < pair[1].vertex.id));
        }
    }

    #[test]
    fn dead는_순환_자기참조_유령_의존자를_살리고_같은_의존자는_한_번만_센다() {
        let mut g = Graph::new();
        let names = [
            "c1", "c2", "by_cycle", "selfie", "by_self", "by_ghost", "top", "mid", "hinted",
        ];
        for name in names {
            g.add_vertex(view("s", name));
        }
        g.add_vertex(table("s", "t"));
        let id = |name: &str| VertexId::object("s", name);
        // c1 ↔ c2 순환과 순환 구성원만 읽는 view.
        g.add_edge(reads(&id("c1"), &id("c2")));
        g.add_edge(reads(&id("c2"), &id("c1")));
        g.add_edge(reads(&id("c1"), &id("by_cycle")));
        // 자기 자신을 읽는 view와 그것만 읽는 view.
        g.add_edge(reads(&id("selfie"), &id("selfie")));
        g.add_edge(reads(&id("selfie"), &id("by_self")));
        // 정점 없이 간선에만 있는 의존자도 살아 있는 의존자로 본다.
        g.add_edge(reads(&id("ghost"), &id("by_ghost")));
        // 같은 의존자가 종류가 다른 간선 두 개로 읽는다.
        g.add_edge(reads(&id("top"), &id("mid")));
        g.add_edge(Edge {
            kind: EdgeKind::DependsOn,
            ..dep(&id("top"), &id("mid"))
        });
        // 추정 간선은 의존자가 아니다.
        g.add_edge(Edge {
            kind: EdgeKind::Inferred,
            ..dep(&id("t"), &id("hinted"))
        });
        let report = dead(&g, 256);
        let got: Vec<_> = report
            .candidates
            .iter()
            .map(|c| (c.vertex.id.as_str(), c.reason))
            .collect();
        assert_eq!(
            got,
            [
                ("s.hinted", DeadReason::NoDependents),
                ("s.mid", DeadReason::AllDependentsDead),
                ("s.top", DeadReason::NoDependents),
            ]
        );
    }

    /// 함수 정점. 오버로드는 시그니처로 id가 갈린다.
    fn function(schema: &str, name: &str, signature: Option<&str>) -> Vertex {
        Vertex {
            id: VertexId::routine(schema, None, name, signature),
            kind: VertexKind::Function,
            name: name.into(),
            schema: schema.into(),
        }
    }

    /// owner 몸체가 후보 중 하나를 불렀지만 하나로 좁히지 못한 분석 기록을 싣는다.
    fn ambiguous_call(g: &mut Graph, owner: &VertexId, candidates: &[&VertexId]) {
        g.set_analysis(
            owner.clone(),
            schemagraph_core::ObjectAnalysis {
                state: schemagraph_core::AnalysisState::Partial,
                scope: "object-dependencies".into(),
                body_hash: None,
                diagnostics: vec![schemagraph_core::Diagnostic::new(
                    "SG_CALL_AMBIGUOUS",
                    "ambiguous overload",
                    None,
                )
                .with_candidates(candidates.iter().map(|id| (*id).clone()))],
                source: None,
            },
        );
    }

    #[test]
    fn dead는_모호한_오버로드_호출의_후보를_호출자의_생사에_묶는다() {
        let mut g = Graph::new();
        g.add_vertex(table("s", "t"));
        let trigger = Vertex {
            id: VertexId::member("s", "t", "trg"),
            kind: VertexKind::Trigger,
            name: "trg".into(),
            schema: "s".into(),
        };
        g.add_vertex(trigger.clone());
        // 트리거가 부르는 live_caller는 살아 있고, 그 몸체가 over(*) 중 하나를 부른다.
        let live_caller = function("s", "live_caller", None);
        let over_int = function("s", "over", Some("integer"));
        let over_text = function("s", "over", Some("text"));
        // 아무도 부르지 않는 orphan은 후보이고, 그 몸체만 쓰는 lone(*)도 함께 후보다.
        let orphan = function("s", "orphan", None);
        let lone_int = function("s", "lone", Some("integer"));
        let lone_text = function("s", "lone", Some("text"));
        for vertex in [
            &live_caller,
            &over_int,
            &over_text,
            &orphan,
            &lone_int,
            &lone_text,
        ] {
            g.add_vertex(vertex.clone());
        }
        g.add_edge(Edge {
            kind: EdgeKind::Calls,
            ..dep(&trigger.id, &live_caller.id)
        });
        ambiguous_call(&mut g, &live_caller.id, &[&over_int.id, &over_text.id]);
        // 그래프에 없는 후보는 의존자를 만들지 않는다.
        let ghost = VertexId::routine("s", None, "lone", Some("ghost"));
        ambiguous_call(&mut g, &orphan.id, &[&lone_int.id, &lone_text.id, &ghost]);

        let report = dead(&g, 256);
        let got: Vec<_> = report
            .candidates
            .iter()
            .map(|c| (c.vertex.id.as_str(), c.reason))
            .collect();
        assert_eq!(
            got,
            [
                ("s.lone(integer)", DeadReason::AllDependentsDead),
                ("s.lone(text)", DeadReason::AllDependentsDead),
                ("s.orphan", DeadReason::NoDependents),
            ]
        );
        assert!(
            report.limitations.iter().any(|l| l
                == "routines named by ambiguous overloaded calls: 4; each counts as \
                    called until a single target is resolved"),
            "{:?}",
            report.limitations
        );
    }

    #[test]
    fn dead는_모호한_호출이_없으면_후보_limitation을_싣지_않는다() {
        let report = dead(&chain(), 256);
        assert!(!report
            .limitations
            .iter()
            .any(|l| l.contains("ambiguous overloaded calls")));
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
    fn glob의_물음표는_유니코드_스칼라_하나와_맞는다() {
        assert!(glob_match("s.t?", "s.t한"));
        assert!(glob_match("s.t?", "s.t\u{e9}"));
        assert!(!glob_match("s.t??", "s.t한"));
        assert!(glob_match("보고.*", "보고.주문.id"));
        assert!(glob_match("*?", "한"));
        assert!(glob_match("a*?b", "a한b"));
        assert!(!glob_match("a*?b", "ab"));
        assert!(glob_match("*한*", "가나한다"));
        assert!(!glob_match("*한?", "가나한"));
        // 결합 문자는 스칼라 두 개다 — 글자 모양(grapheme)이 아니라 스칼라로 센다.
        assert!(!glob_match("?", "e\u{301}"));
        assert!(glob_match("??", "e\u{301}"));
        // 별 뒤 백트래킹이 다중 바이트 문자 경계에서 다시 시작해도 결과가 같다.
        assert!(glob_match("*가*나", "가가나나"));
        assert!(glob_match("a*한한", "a한한한"));
        assert!(!glob_match("*한한한한", "a한한한"));
    }

    #[test]
    fn rules는_간선_목록과_규칙_패턴을_한_번씩만_준비한다() {
        let id = |number: usize| VertexId::object("s", &format!("t{number:02}"));
        let mut g = Graph::new();
        for number in 0..20 {
            g.add_vertex(table("s", &format!("t{number:02}")));
        }
        for number in 1..20 {
            g.add_edge(reads(&id(number), &id(number - 1)));
        }
        let rule_set: Vec<Rule> = (0..4)
            .map(|number| Rule {
                name: format!("r{number}"),
                from: "s.t1*".into(),
                to: "*".into(),
                kinds: None,
            })
            .collect();
        take_rules_work();
        let report = rules(&g, &rule_set);
        // s.t10..s.t19에서 나가는 간선 10개가 규칙마다 위반이다.
        assert_eq!(report.violations.len(), 4 * 10);
        assert_eq!(
            take_rules_work(),
            RulesWork {
                edge_lists: 1,
                patterns: 2 * 4
            }
        );
    }

    #[test]
    fn rules_위반은_규칙_이름_끝점_간선_종류_순이고_같은_이름은_규칙_순서를_지킨다() {
        let mut g = Graph::new();
        g.add_vertex(table("rpt", "a"));
        g.add_vertex(table("core", "b"));
        let (a, b) = (VertexId::object("rpt", "a"), VertexId::object("core", "b"));
        // 같은 끝점에 종류만 다른 간선 두 개. 넣는 순서를 종류 순서와 반대로 둔다.
        g.add_edge(Edge {
            kind: EdgeKind::Writes,
            ..dep(&a, &b)
        });
        g.add_edge(reads(&a, &b));
        let rule = |name: &str, kinds: Option<BTreeSet<EdgeKind>>| Rule {
            name: name.into(),
            from: "rpt.*".into(),
            to: "core.*".into(),
            kinds,
        };
        let report = rules(
            &g,
            &[
                rule("z", None),
                rule("dup", Some(BTreeSet::from([EdgeKind::Writes]))),
                rule("a", None),
                rule("dup", None),
            ],
        );
        let got: Vec<_> = report
            .violations
            .iter()
            .map(|v| (v.rule.as_str(), v.kind))
            .collect();
        assert_eq!(
            got,
            [
                ("a", EdgeKind::Reads),
                ("a", EdgeKind::Writes),
                ("dup", EdgeKind::Writes),
                ("dup", EdgeKind::Reads),
                ("dup", EdgeKind::Writes),
                ("z", EdgeKind::Reads),
                ("z", EdgeKind::Writes),
            ]
        );
    }

    #[test]
    fn rules는_비ascii_id에도_물음표를_글자_하나로_대조한다() {
        let mut g = Graph::new();
        for (schema, name) in [("보고", "주문1"), ("보고", "주문12"), ("핵심", "고객")]
        {
            g.add_vertex(table(schema, name));
        }
        for name in ["주문1", "주문12"] {
            g.add_edge(reads(
                &VertexId::object("보고", name),
                &VertexId::object("핵심", "고객"),
            ));
        }
        let rule = Rule {
            name: "r".into(),
            from: "보고.주문?".into(),
            to: "핵심.??".into(),
            kinds: None,
        };
        let report = rules(&g, &[rule]);
        let got: Vec<_> = report.violations.iter().map(|v| v.from.as_str()).collect();
        assert_eq!(got, ["보고.주문1"]);
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
                scans: None,
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
