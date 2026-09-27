//! 여러 루트에서 한 번에 도는 다중 루트 의존성 탐색.
//!
//! 루트마다 `budget::walk`를 따로 돌리면 루트끼리 겹치는 부분 그래프를 루트 수만큼
//! 다시 검사한다. 여기서는 모든 루트를 거리 0에 둔 다중 출발 BFS 한 번으로 도달 정점,
//! 가장 가까운 루트로부터의 최단 거리, `via`를 구한다. 그다음 BFS가 검사한 간선만
//! 다시 써서 "어느 루트가 이 정점에 닿는가"(루트 집합)를 전파한다. 그래프를 두 번
//! 읽지 않으므로 루트 집합 계산은 예산이 허락한 부분 그래프 안에서만 일어난다.

use crate::budget::Budget;
use crate::{Neighbor, Reach};
use schemagraph_core::{Edge, Graph, VertexId};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};

/// 도달 정점 하나에 싣는 루트 번호의 최대 개수.
///
/// 루트가 수백 개인 요청에서 모든 도달 정점이 루트 번호를 전부 실으면 출력이 루트 수 ×
/// 정점 수로 커진다. 넘치면 작은 번호부터 이만큼만 싣고 `roots_truncated`로 알린다.
pub const MAX_ROOTS_PER_VERTEX: usize = 64;

/// 다중 루트 탐색이 도달한 정점 하나.
#[derive(Debug, Clone)]
pub struct ReachedVertex {
    /// 정점과 거리·간선 종류·via. `distance`는 가장 가까운 루트로부터의 최단 거리이고,
    /// `via`는 그 최단 경로의 직전 정점(루트이거나 다른 도달 정점)이다. 같은 최단 거리의
    /// 부모가 여럿이면 id의 사전식 최소를 고른다 — 단일 루트 탐색과 같은 규칙이다.
    pub neighbor: Neighbor,
    /// 이 정점에 닿는 모든 루트의 입력 순번(오름차순). 가장 가까운 루트만이 아니다.
    /// 최대 [`MAX_ROOTS_PER_VERTEX`]개다.
    pub roots: Vec<usize>,
    /// 루트 번호가 상한을 넘어 잘렸는가.
    pub roots_truncated: bool,
}

/// 다중 루트 탐색 결과.
///
/// `complete`가 false면 예산 소진·취소·찾지 못한 루트 때문에 부재를 결론낼 수 없다.
/// 이때는 도달 정점뿐 아니라 각 정점의 루트 집합·간선 종류·via도 검사한 간선에서
/// 얻은 하한이다.
#[derive(Debug, Clone)]
pub struct MultiRootReport {
    /// 도달 정점들. 루트 자신은 싣지 않는다. (거리, id) 순이다.
    pub reached: Vec<ReachedVertex>,
    /// 방문해 기록한 정점 수(서로 다른 루트 포함).
    pub visited: usize,
    /// 검사한 의존성 간선 수.
    pub examined_edges: usize,
    /// 탐색 또는 출력이 제한된 이유. 사전식 정렬·중복 제거한다.
    pub truncation_reasons: Vec<String>,
    /// 요청된 depth 범위의 탐색과 루트 집합 전파가 모두 끝났는가.
    pub complete: bool,
    /// 어느 도달 정점이든 루트 번호가 상한에 걸려 잘렸는가.
    pub roots_truncated: bool,
}

/// 여러 루트에서 의존성 이웃을 한 번의 예산으로 탐색한다.
///
/// `roots[i]`가 `None`이면 호출자가 해석하지 못한 루트다. 그 순번은 비워 두되 다른
/// 루트의 순번은 입력 위치 그대로 유지한다 — 소비자가 루트 번호로 자기 입력을 찾는다.
/// 해석하지 못했거나 그래프에 없는 루트가 있으면 `root-not-found`를 기록한다.
///
/// 방향·간선 필터·자기 간선·유령 정점 처리와 `max_results`의 의미는 `budget::walk`와
/// 같고, 루트가 하나면 같은 이웃(거리·간선 종류·via)과 같은 탐색 수치를 낸다. 예산은
/// 루트마다가 아니라 탐색 전체에 한 번 적용된다. 한 루트가 다른 루트에 닿아도 그
/// 루트는 도달 정점으로 다시 싣지 않지만, 그 너머 정점의 루트 집합에는 반영된다.
/// `depth`는 루트 집합에도 적용된다 — 어떤 루트에서 `depth` 걸음 안에 닿는 정점만
/// 그 루트의 번호를 받는다.
pub fn walk_roots(
    graph: &Graph,
    roots: &[Option<VertexId>],
    depth: u32,
    max_results: usize,
    reverse: bool,
    budget: Budget,
    cancel: Option<&AtomicBool>,
) -> MultiRootReport {
    let mut reasons = BTreeSet::new();
    if cancelled(cancel) {
        reasons.insert("cancelled");
    }
    let seeds = found_roots(graph, roots, &mut reasons);
    // 루트 수가 정점 예산보다 크면 명시적인 무작업 예산처럼 다룬다 — 일부 루트만
    // 심으면 어떤 루트가 빠졌는지가 출력에서 사라진다.
    if seeds.is_empty() || seeds.len() > budget.max_visited {
        if !seeds.is_empty() {
            reasons.insert("visited-limit");
        }
        return finish(Vec::new(), 0, 0, reasons, false);
    }
    let search = Search {
        graph,
        seeds: &seeds,
        depth,
        reverse,
        budget,
        cancel,
    };
    let explored = search.run(&mut reasons);
    let sets = RootSets::propagate(&explored, roots, depth, cancel, &mut reasons);
    let mut reached = collect(graph, explored.seen, &seeds, &sets, cancel, &mut reasons);
    if reached.len() > max_results {
        reasons.insert("result-limit");
        reached.truncate(max_results);
    }
    finish(
        reached,
        explored.visited,
        explored.examined_edges,
        reasons,
        true,
    )
}

/// 그래프에 실제로 있는 루트 id만 모은다. 빠진 루트가 있으면 이유를 남긴다.
fn found_roots(
    graph: &Graph,
    roots: &[Option<VertexId>],
    reasons: &mut BTreeSet<&'static str>,
) -> BTreeSet<VertexId> {
    let mut seeds = BTreeSet::new();
    for root in roots {
        match root {
            Some(id) if graph.vertex(id).is_some() => {
                seeds.insert(id.clone());
            }
            _ => {
                reasons.insert("root-not-found");
            }
        }
    }
    seeds
}

/// 최종 보고를 만든다. `traversed`가 false면 탐색 자체를 하지 않은 결과다.
fn finish(
    reached: Vec<ReachedVertex>,
    visited: usize,
    examined_edges: usize,
    reasons: BTreeSet<&str>,
    traversed: bool,
) -> MultiRootReport {
    let complete = traversed && !reasons.iter().any(|reason| *reason != "result-limit");
    MultiRootReport {
        roots_truncated: reached.iter().any(|vertex| vertex.roots_truncated),
        reached,
        visited,
        examined_edges,
        truncation_reasons: reasons.into_iter().map(str::to_owned).collect(),
        complete,
    }
}

fn cancelled(cancel: Option<&AtomicBool>) -> bool {
    cancel.is_some_and(|flag| flag.load(Ordering::Relaxed))
}

/// 다중 출발 BFS의 입력.
struct Search<'a> {
    graph: &'a Graph,
    seeds: &'a BTreeSet<VertexId>,
    depth: u32,
    reverse: bool,
    budget: Budget,
    cancel: Option<&'a AtomicBool>,
}

/// BFS가 남긴 기록. `arcs`는 검사한 간선 중 양 끝이 모두 기록된 것(부모 → 자식)이다.
struct Explored {
    seen: BTreeMap<VertexId, Reach>,
    arcs: Vec<(VertexId, VertexId)>,
    visited: usize,
    examined_edges: usize,
}

impl Search<'_> {
    /// 모든 루트를 거리 0으로 심고 BFS를 돈다.
    ///
    /// 큐는 정렬된 루트 순서로 시작하고 인접 간선도 정렬해 넣는다. via는 발견 순서와
    /// 무관하게 `Reach::observe`의 사전식 최소 규칙으로 정해진다.
    fn run(&self, reasons: &mut BTreeSet<&'static str>) -> Explored {
        let mut explored = Explored {
            seen: BTreeMap::new(),
            arcs: Vec::new(),
            visited: self.seeds.len(),
            examined_edges: 0,
        };
        let mut queue = VecDeque::new();
        for seed in self.seeds {
            explored.seen.insert(seed.clone(), Reach::root(seed));
            queue.push_back(seed.clone());
        }
        while let Some(id) = queue.pop_front() {
            if !self.expand(&id, &mut explored, &mut queue, reasons) {
                break;
            }
        }
        explored
    }

    /// 정점 하나를 펼친다. 탐색을 멈춰야 하면 false다.
    fn expand(
        &self,
        id: &VertexId,
        explored: &mut Explored,
        queue: &mut VecDeque<VertexId>,
        reasons: &mut BTreeSet<&'static str>,
    ) -> bool {
        if cancelled(self.cancel) {
            reasons.insert("cancelled");
            return false;
        }
        let distance = explored.seen[id].distance();
        if distance >= self.depth {
            return true;
        }
        // 간선 예산을 이미 다 썼으면 dense adjacency를 복사하지 않는다.
        if explored.examined_edges >= self.budget.max_examined_edges {
            reasons.insert("edge-limit");
            return false;
        }
        let Some(edges) = self.sorted_dependency_edges(id) else {
            reasons.insert("cancelled");
            return false;
        };
        for edge in edges {
            if !self.visit(id, distance, edge, explored, queue, reasons) {
                return false;
            }
        }
        true
    }

    /// 한 정점의 의존성 간선을 결정적인 순서로 모은다. 취소되면 None이다.
    fn sorted_dependency_edges(&self, id: &VertexId) -> Option<Vec<&Edge>> {
        let adjacency = if self.reverse {
            self.graph.incoming(id)
        } else {
            self.graph.outgoing(id)
        };
        let mut edges: Vec<&Edge> = Vec::new();
        for edge in adjacency {
            if cancelled(self.cancel) {
                return None;
            }
            if edge.kind.is_dependency() {
                edges.push(edge);
            }
        }
        edges.sort_by(|a, b| (&a.from, &a.to, a.kind).cmp(&(&b.from, &b.to, b.kind)));
        (!cancelled(self.cancel)).then_some(edges)
    }

    /// 간선 하나를 검사한다. 탐색을 멈춰야 하면 false다.
    ///
    /// 루트로 들어오는 간선은 거리를 바꾸지 않지만(루트는 이웃이 아니다) 루트 집합
    /// 전파를 위해 arc로 남긴다 — 루트 B를 거쳐 가는 정점도 루트 A에서 닿기 때문이다.
    fn visit(
        &self,
        parent: &VertexId,
        distance: u32,
        edge: &Edge,
        explored: &mut Explored,
        queue: &mut VecDeque<VertexId>,
        reasons: &mut BTreeSet<&'static str>,
    ) -> bool {
        if cancelled(self.cancel) {
            reasons.insert("cancelled");
            return false;
        }
        if explored.examined_edges >= self.budget.max_examined_edges {
            reasons.insert("edge-limit");
            return false;
        }
        explored.examined_edges += 1;
        let next = if self.reverse { &edge.from } else { &edge.to };
        // 등록되지 않은 id는 유령 정점이다 — 노출하지도, 예산을 쓰지도 않는다.
        if self.graph.vertex(next).is_none() {
            return true;
        }
        if self.seeds.contains(next) {
            explored.arcs.push((parent.clone(), next.clone()));
            return true;
        }
        if let Some(reach) = explored.seen.get_mut(next) {
            reach.observe(parent, distance, edge.kind);
            explored.arcs.push((parent.clone(), next.clone()));
            return true;
        }
        if explored.seen.len() >= self.budget.max_visited {
            reasons.insert("visited-limit");
            return false;
        }
        let reach = Reach::discovered(parent, distance, edge.kind);
        explored.seen.insert(next.clone(), reach);
        explored.visited += 1;
        explored.arcs.push((parent.clone(), next.clone()));
        queue.push_back(next.clone());
        true
    }
}

/// 기록된 정점마다 "닿는 루트 순번"의 비트 집합.
struct RootSets {
    index: BTreeMap<VertexId, usize>,
    bits: Vec<Vec<u64>>,
}

impl RootSets {
    /// BFS가 남긴 arc로 루트 집합을 전파한다.
    ///
    /// 걸음마다 새로 들어온 비트(델타)만 자식에게 넘기는 동기식 전파라서, 비트가 정점에
    /// 처음 들어오는 걸음 수가 곧 그 루트로부터의 최단 거리다. 그래서 `depth` 걸음에서
    /// 멈추면 루트마다 `depth` 안에 닿는 정점만 그 번호를 받는다. 각 (정점, 루트) 비트는
    /// 한 번만 델타가 되므로 전체 비용은 arc 수 × 루트 수/64에 비례한다.
    fn propagate(
        explored: &Explored,
        roots: &[Option<VertexId>],
        depth: u32,
        cancel: Option<&AtomicBool>,
        reasons: &mut BTreeSet<&'static str>,
    ) -> Self {
        let index: BTreeMap<VertexId, usize> = explored
            .seen
            .keys()
            .enumerate()
            .map(|(position, id)| (id.clone(), position))
            .collect();
        let words = roots.len().div_ceil(64);
        let mut sets = Self {
            bits: vec![vec![0; words]; index.len()],
            index,
        };
        let children = sets.children(&explored.arcs);
        let mut delta = sets.seed(roots);
        let mut steps = 0u32;
        while !delta.is_empty() && steps < depth {
            if cancelled(cancel) {
                reasons.insert("cancelled");
                break;
            }
            delta = sets.step(&children, delta);
            steps += 1;
        }
        sets
    }

    /// arc를 정점 번호의 자식 목록으로 바꾼다. 평행 간선과 자기 간선은 한 번만 남긴다.
    fn children(&self, arcs: &[(VertexId, VertexId)]) -> Vec<Vec<usize>> {
        let mut children = vec![Vec::new(); self.index.len()];
        for (parent, child) in arcs {
            let (parent, child) = (self.index[parent], self.index[child]);
            if parent != child {
                children[parent].push(child);
            }
        }
        for list in &mut children {
            list.sort_unstable();
            list.dedup();
        }
        children
    }

    /// 루트 정점에 자기 순번 비트를 켜고 첫 델타로 돌려준다. 같은 정점을 가리키는
    /// 루트가 여럿이면 그 순번이 모두 켜진다.
    fn seed(&mut self, roots: &[Option<VertexId>]) -> BTreeMap<usize, Vec<u64>> {
        let mut delta: BTreeMap<usize, Vec<u64>> = BTreeMap::new();
        for (position, root) in roots.iter().enumerate() {
            let Some(&node) = root.as_ref().and_then(|id| self.index.get(id)) else {
                continue;
            };
            let (word, bit) = (position / 64, 1u64 << (position % 64));
            self.bits[node][word] |= bit;
            let words = self.bits[node].len();
            delta.entry(node).or_insert_with(|| vec![0; words])[word] |= bit;
        }
        delta
    }

    /// 한 걸음 전파한다 — 이번 델타를 자식에게 넘기고, 자식에게 새로 켜진 비트를 다음
    /// 델타로 돌려준다.
    fn step(
        &mut self,
        children: &[Vec<usize>],
        delta: BTreeMap<usize, Vec<u64>>,
    ) -> BTreeMap<usize, Vec<u64>> {
        let mut next: BTreeMap<usize, Vec<u64>> = BTreeMap::new();
        for (node, incoming) in delta {
            for &child in &children[node] {
                for (word, value) in incoming.iter().enumerate() {
                    let added = value & !self.bits[child][word];
                    if added != 0 {
                        self.bits[child][word] |= added;
                        let words = incoming.len();
                        next.entry(child).or_insert_with(|| vec![0; words])[word] |= added;
                    }
                }
            }
        }
        next
    }

    /// 정점에 닿는 루트 순번을 오름차순으로 최대 상한까지 돌려준다. 잘렸으면 true다.
    fn roots_of(&self, id: &VertexId) -> (Vec<usize>, bool) {
        let mut roots = Vec::new();
        for (word, value) in self.bits[self.index[id]].iter().enumerate() {
            let mut rest = *value;
            while rest != 0 {
                if roots.len() == MAX_ROOTS_PER_VERTEX {
                    return (roots, true);
                }
                roots.push(word * 64 + rest.trailing_zeros() as usize);
                rest &= rest - 1;
            }
        }
        (roots, false)
    }
}

/// 루트가 아닌 기록 정점을 (거리, id) 순의 도달 정점으로 만든다.
///
/// 거리 순을 먼저 두는 이유: 결과 개수 제한으로 잘려도 가까운 정점이 남고, 남은 정점의
/// via는 항상 루트이거나 목록 안의 더 가까운 정점이다.
fn collect(
    graph: &Graph,
    seen: BTreeMap<VertexId, Reach>,
    seeds: &BTreeSet<VertexId>,
    sets: &RootSets,
    cancel: Option<&AtomicBool>,
    reasons: &mut BTreeSet<&'static str>,
) -> Vec<ReachedVertex> {
    let mut reached = Vec::new();
    for (id, reach) in seen {
        if cancelled(cancel) {
            reasons.insert("cancelled");
            break;
        }
        if seeds.contains(&id) {
            continue;
        }
        let Some(vertex) = graph.vertex(&id) else {
            continue;
        };
        let (roots, roots_truncated) = sets.roots_of(&id);
        reached.push(ReachedVertex {
            neighbor: reach.into_neighbor(vertex.clone()),
            roots,
            roots_truncated,
        });
    }
    reached.sort_by(|a, b| {
        (a.neighbor.distance, &a.neighbor.vertex.id)
            .cmp(&(b.neighbor.distance, &b.neighbor.vertex.id))
    });
    reached
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::budget::walk;
    use schemagraph_core::{EdgeKind, Vertex, VertexKind};

    fn id(name: &str) -> VertexId {
        VertexId::object("s", name)
    }

    fn table(name: &str) -> Vertex {
        Vertex {
            id: id(name),
            kind: VertexKind::Table,
            name: name.into(),
            schema: "s".into(),
        }
    }

    /// `(from, to, kind)` 목록으로 그래프를 만든다. 정점은 간선에 나온 이름 전부다.
    fn graph(edges: &[(&str, &str, EdgeKind)]) -> Graph {
        let mut graph = Graph::new();
        let names: BTreeSet<&str> = edges.iter().flat_map(|(f, t, _)| [*f, *t]).collect();
        for name in names {
            graph.add_vertex(table(name));
        }
        for (from, to, kind) in edges {
            graph.add_edge(Edge {
                from: id(from),
                to: id(to),
                kind: *kind,
                evidence: vec![],
            });
        }
        graph
    }

    fn roots(names: &[&str]) -> Vec<Option<VertexId>> {
        names.iter().map(|name| Some(id(name))).collect()
    }

    fn dependents(graph: &Graph, roots: &[Option<VertexId>], depth: u32) -> MultiRootReport {
        walk_roots(
            graph,
            roots,
            depth,
            usize::MAX,
            true,
            Budget::unlimited(),
            None,
        )
    }

    /// 도달 정점을 (id, depth, via, roots)로 줄인다.
    fn rows(report: &MultiRootReport) -> Vec<(String, u32, String, Vec<usize>)> {
        report
            .reached
            .iter()
            .map(|vertex| {
                (
                    vertex.neighbor.vertex.id.as_str().to_owned(),
                    vertex.neighbor.distance,
                    vertex.neighbor.via.as_str().to_owned(),
                    vertex.roots.clone(),
                )
            })
            .collect()
    }

    fn row(
        name: &str,
        depth: u32,
        via: &str,
        roots: &[usize],
    ) -> (String, u32, String, Vec<usize>) {
        (
            format!("s.{name}"),
            depth,
            format!("s.{via}"),
            roots.to_vec(),
        )
    }

    /// ```text
    /// orders -> customers, invoices -> customers    (customers의 의존자)
    /// report -> orders, report -> products          (두 루트가 공유하는 의존자)
    /// audit -> report                               (report 너머)
    /// catalog -> products                           (products만)
    /// ```
    fn shop() -> Graph {
        use EdgeKind::*;
        graph(&[
            ("orders", "customers", References),
            ("invoices", "customers", References),
            ("report", "orders", Reads),
            ("report", "products", Reads),
            ("audit", "report", Reads),
            ("catalog", "products", Reads),
        ])
    }

    #[test]
    fn 루트_집합은_닿는_모든_루트를_싣고_via는_가장_가까운_루트_쪽이다() {
        let report = dependents(&shop(), &roots(&["customers", "products"]), u32::MAX);
        assert!(report.complete);
        assert_eq!(
            rows(&report),
            [
                row("catalog", 1, "products", &[1]),
                row("invoices", 1, "customers", &[0]),
                row("orders", 1, "customers", &[0]),
                row("report", 1, "products", &[0, 1]),
                row("audit", 2, "report", &[0, 1]),
            ]
        );
        // report는 products에서 1걸음, customers에서 2걸음이다 — via는 가까운 쪽이다.
    }

    /// 루트 B가 루트 A에 의존하면 B는 도달 정점으로 다시 나오지 않지만, B 너머 정점은
    /// A에서도 닿으므로 A의 순번을 받는다.
    #[test]
    fn 다른_루트를_거쳐_닿는_정점도_그_루트의_순번을_받는다() {
        use EdgeKind::*;
        let g = graph(&[("b", "a", Reads), ("y", "b", Reads), ("x", "a", Reads)]);
        let report = dependents(&g, &roots(&["a", "b"]), u32::MAX);
        assert_eq!(
            rows(&report),
            [row("x", 1, "a", &[0]), row("y", 1, "b", &[0, 1])]
        );
    }

    /// depth는 루트 집합에도 적용된다 — y는 b에서 1걸음이지만 a에서는 2걸음이다.
    #[test]
    fn depth_안에_닿는_루트만_순번을_받는다() {
        use EdgeKind::*;
        let g = graph(&[("b", "a", Reads), ("y", "b", Reads)]);
        let one = dependents(&g, &roots(&["a", "b"]), 1);
        assert_eq!(rows(&one), [row("y", 1, "b", &[1])]);
        let two = dependents(&g, &roots(&["a", "b"]), 2);
        assert_eq!(rows(&two), [row("y", 1, "b", &[0, 1])]);
    }

    /// 같은 거리의 두 루트가 부모 후보면 사전식으로 작은 루트가 via다. via를 따라가면
    /// depth 걸음 만에 루트에 닿고, 각 걸음은 실제 의존 간선이다.
    #[test]
    fn via는_사전식_최소_부모이고_실제_간선을_따라_루트로_돌아간다() {
        use EdgeKind::*;
        let g = graph(&[
            ("v", "zeta", Reads),
            ("v", "alpha", Reads),
            ("w", "v", Reads),
            ("w", "zeta", Calls),
        ]);
        let root_list = roots(&["zeta", "alpha"]);
        let report = dependents(&g, &root_list, u32::MAX);
        assert_eq!(
            rows(&report),
            [row("v", 1, "alpha", &[0, 1]), row("w", 1, "zeta", &[0, 1])]
        );
        let seeds: BTreeSet<&VertexId> = root_list.iter().flatten().collect();
        let depth: BTreeMap<&VertexId, u32> = report
            .reached
            .iter()
            .map(|vertex| (&vertex.neighbor.vertex.id, vertex.neighbor.distance))
            .collect();
        for vertex in &report.reached {
            let (node, via) = (&vertex.neighbor.vertex.id, &vertex.neighbor.via);
            // 역방향 탐색이므로 node -> via 의존 간선이 있어야 한다.
            assert!(g
                .outgoing(node)
                .iter()
                .any(|edge| edge.to == *via && edge.kind.is_dependency()));
            let via_depth = if seeds.contains(via) { 0 } else { depth[via] };
            assert_eq!(via_depth + 1, vertex.neighbor.distance);
        }
    }

    /// 루트가 하나면 기존 단일 루트 walk와 같은 이웃·수치를 낸다(정렬만 다르다).
    #[test]
    fn 루트가_하나면_budget_walk와_같다() {
        use crate::tests::via_fixture;
        for (graph, root, reverse) in [
            (via_fixture(false), id("s"), true),
            (via_fixture(true), id("s"), true),
            (via_fixture(false), id("w"), false),
            (shop(), id("customers"), true),
        ] {
            for depth in [1, 2, u32::MAX] {
                let single = walk(
                    &graph,
                    &root,
                    depth,
                    usize::MAX,
                    reverse,
                    Budget::unlimited(),
                    None,
                );
                let multi = walk_roots(
                    &graph,
                    &[Some(root.clone())],
                    depth,
                    usize::MAX,
                    reverse,
                    Budget::unlimited(),
                    None,
                );
                let mut reached = multi.reached.clone();
                reached.sort_by(|a, b| a.neighbor.vertex.id.cmp(&b.neighbor.vertex.id));
                assert_eq!(reached.len(), single.neighbors.len());
                for (actual, expected) in reached.iter().zip(&single.neighbors) {
                    assert_eq!(actual.neighbor.vertex.id, expected.vertex.id);
                    assert_eq!(actual.neighbor.distance, expected.distance);
                    assert_eq!(actual.neighbor.via, expected.via);
                    assert_eq!(actual.neighbor.edges, expected.edges);
                    assert_eq!(actual.roots, [0]);
                }
                assert_eq!(
                    (multi.visited, multi.examined_edges),
                    (single.visited, single.examined_edges)
                );
                assert_eq!(multi.complete, single.complete);
            }
        }
    }

    /// 간선 삽입 순서와 무관하게 같은 결과이고, 루트 순서를 바꾸면 순번만 바뀐다.
    #[test]
    fn 출력은_삽입_순서와_무관하고_루트_순번은_입력_위치를_따른다() {
        use crate::tests::via_fixture;
        let forward = dependents(&via_fixture(false), &roots(&["s", "x"]), u32::MAX);
        let backward = dependents(&via_fixture(true), &roots(&["s", "x"]), u32::MAX);
        assert_eq!(rows(&forward), rows(&backward));
        let swapped = dependents(&via_fixture(false), &roots(&["x", "s"]), u32::MAX);
        let remapped: Vec<_> = rows(&forward)
            .into_iter()
            .map(|(name, depth, via, roots)| {
                let mut roots: Vec<usize> = roots.iter().map(|r| 1 - r).collect();
                roots.sort_unstable();
                (name, depth, via, roots)
            })
            .collect();
        assert_eq!(rows(&swapped), remapped);
    }

    /// 해석하지 못한 루트는 순번만 비우고, 나머지 루트는 입력 위치의 순번을 유지한다.
    #[test]
    fn 찾지_못한_루트는_순번을_비우고_나머지는_계속_탐색한다() {
        let root_list = vec![None, Some(id("products")), Some(id("missing"))];
        let report = dependents(&shop(), &root_list, u32::MAX);
        assert_eq!(report.truncation_reasons, ["root-not-found"]);
        assert!(!report.complete);
        assert_eq!(
            rows(&report),
            [
                row("catalog", 1, "products", &[1]),
                row("report", 1, "products", &[1]),
                row("audit", 2, "report", &[1]),
            ]
        );
        let none = dependents(&shop(), &[None], u32::MAX);
        assert!(none.reached.is_empty() && none.visited == 0 && !none.complete);
    }

    /// 같은 정점을 가리키는 루트가 둘이면 두 순번이 모두 실린다.
    #[test]
    fn 중복_루트는_두_순번을_모두_싣는다() {
        let report = dependents(&shop(), &roots(&["products", "products"]), 1);
        assert_eq!(
            rows(&report),
            [
                row("catalog", 1, "products", &[0, 1]),
                row("report", 1, "products", &[0, 1])
            ]
        );
    }

    /// 루트가 상한보다 많이 닿으면 작은 순번부터 상한만큼 싣고 잘렸다고 표시한다.
    #[test]
    fn 루트_순번은_상한에서_잘리고_roots_truncated로_알린다() {
        let names: Vec<String> = (0..70).map(|n| format!("t{n:02}")).collect();
        let edges: Vec<(&str, &str, EdgeKind)> = names
            .iter()
            .map(|name| ("view", name.as_str(), EdgeKind::Reads))
            .chain([("few", "t00", EdgeKind::Reads)])
            .collect();
        let g = graph(&edges);
        let root_list: Vec<Option<VertexId>> = names.iter().map(|name| Some(id(name))).collect();
        let report = dependents(&g, &root_list, u32::MAX);
        assert!(report.roots_truncated && report.complete);
        let view = &report.reached[1];
        assert_eq!(view.neighbor.vertex.id.as_str(), "s.view");
        assert_eq!(view.roots, (0..64).collect::<Vec<_>>());
        assert!(view.roots_truncated);
        assert!(!report.reached[0].roots_truncated);
        assert_eq!(report.reached[0].roots, [0]);
    }

    /// 결과 제한은 가까운 정점을 남기고 탐색을 멈추지 않는다. 예산 소진은 탐색을 멈춘다.
    #[test]
    fn 결과_제한과_예산_소진을_구분한다() {
        let root_list = roots(&["customers", "products"]);
        let limited = walk_roots(
            &shop(),
            &root_list,
            u32::MAX,
            2,
            true,
            Budget::unlimited(),
            None,
        );
        assert_eq!(limited.truncation_reasons, ["result-limit"]);
        assert!(limited.complete);
        assert_eq!(limited.reached.len(), 2);
        assert!(limited.reached.iter().all(|v| v.neighbor.distance == 1));

        let budget = |max_visited, max_examined_edges| Budget {
            max_visited,
            max_examined_edges,
        };
        let run = |budget| {
            walk_roots(
                &shop(),
                &root_list,
                u32::MAX,
                usize::MAX,
                true,
                budget,
                None,
            )
        };
        let edges = run(budget(usize::MAX, 1));
        assert_eq!(edges.truncation_reasons, ["edge-limit"]);
        assert_eq!(edges.examined_edges, 1);
        assert!(!edges.complete);
        let vertices = run(budget(3, usize::MAX));
        assert_eq!(vertices.truncation_reasons, ["visited-limit"]);
        assert_eq!(vertices.visited, 3);
        let too_many_roots = run(budget(1, usize::MAX));
        assert_eq!(too_many_roots.truncation_reasons, ["visited-limit"]);
        assert_eq!(too_many_roots.visited, 0);
        assert!(too_many_roots.reached.is_empty());
    }

    #[test]
    fn 취소는_불완전한_결과로_보고한다() {
        let flag = AtomicBool::new(true);
        let report = walk_roots(
            &shop(),
            &roots(&["customers"]),
            u32::MAX,
            usize::MAX,
            true,
            Budget::unlimited(),
            Some(&flag),
        );
        assert_eq!(report.truncation_reasons, ["cancelled"]);
        assert!(!report.complete && report.reached.is_empty());
    }

    /// 정방향(dependencies)도 같은 규칙이다 — 두 루트가 공유하는 대상은 두 순번을 받는다.
    #[test]
    fn 정방향_탐색도_루트_집합을_전파한다() {
        let report = walk_roots(
            &shop(),
            &roots(&["audit", "catalog"]),
            u32::MAX,
            usize::MAX,
            false,
            Budget::unlimited(),
            None,
        );
        assert_eq!(
            rows(&report),
            [
                row("products", 1, "catalog", &[0, 1]),
                row("report", 1, "audit", &[0]),
                row("orders", 2, "report", &[0]),
                row("customers", 3, "orders", &[0]),
            ]
        );
    }
}
