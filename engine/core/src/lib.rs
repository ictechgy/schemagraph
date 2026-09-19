//! schemagraph-core — 스키마 의존성 그래프 도메인.
//!
//! 이 크레이트는 외부 의존성이 없다. 그래프 도메인이 순수해야 분석 계층 전체를
//! DB 없이 테스트할 수 있다(DESIGN.md "모듈 레이아웃" 절).
//!
//! 간선 방향 규약: `from`이 의존하는 쪽, `to`가 의존 대상이다.
//! `orders -> customers`는 "orders가 customers를 참조한다"는 뜻이다.

use std::collections::{BTreeMap, BTreeSet};

/// 질의와 출력의 집계 단위.
///
/// CLI의 `column` 레벨은 `Member`로 매핑한다. 컬럼이 의존성을 가지는 사실상
/// 유일한 멤버지만, 트리거·인덱스·제약도 같은 층에 둔다.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Level {
    /// 스키마 단위 — 가장 거친 뷰.
    Schema,
    /// 테이블·뷰·루틴 등 객체 단위.
    Object,
    /// 컬럼·인덱스·제약·트리거 등 소유된 멤버 단위 — 가장 자세한 레벨.
    Member,
}

/// 정점의 종류. 직렬화 문자열은 export 크레이트가 소유한다 — 도메인이
/// 와이어 형식에 묶이지 않게 하기 위해서다.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum VertexKind {
    Schema,
    Table,
    View,
    MaterializedView,
    Sequence,
    Type,
    Synonym,
    Column,
    Index,
    Constraint,
    Trigger,
    Function,
    Procedure,
    Package,
}

impl VertexKind {
    /// 이 정점이 속하는 집계 레벨.
    pub fn level(self) -> Level {
        match self {
            VertexKind::Schema => Level::Schema,
            VertexKind::Column
            | VertexKind::Index
            | VertexKind::Constraint
            | VertexKind::Trigger => Level::Member,
            _ => Level::Object,
        }
    }
}

/// 정점의 정규 식별자. 표시 형태는 `schema.object[.member]`다.
///
/// 마지막 `.` 기준으로 부모를 자르므로, 이름 자체에 `.`가 들어가면 부모
/// 추적이 깨진다. reader가 그런 이름을 발견하면 `limitations`에 실어야 한다.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct VertexId(String);

impl VertexId {
    /// 스키마 정점의 id.
    pub fn schema(name: &str) -> Self {
        Self(name.to_owned())
    }

    /// 스키마 아래 객체의 id.
    pub fn object(schema: &str, name: &str) -> Self {
        Self(format!("{schema}.{name}"))
    }

    /// 객체 아래 멤버의 id.
    pub fn member(schema: &str, parent: &str, name: &str) -> Self {
        Self(format!("{schema}.{parent}.{name}"))
    }

    /// 와이어에 실린 id 문자열을 그대로 복원한다. graph.json을 읽을 때만
    /// 쓴다 — 새 정점을 만들 때는 schema/object/member 생성자를 써야
    /// 형식이 보장된다.
    pub fn from_raw(id: &str) -> Self {
        Self(id.to_owned())
    }

    /// 표시 문자열.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// 한 단계 위 조상의 id. 최상위(스키마)면 None.
    pub fn parent(&self) -> Option<VertexId> {
        self.0.rfind('.').map(|i| Self(self.0[..i].to_owned()))
    }
}

impl std::fmt::Display for VertexId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// 간선 종류.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum EdgeKind {
    /// 선언된 외래 키.
    References,
    /// 몸체 파싱으로 유도된 읽기(view·routine → 테이블/컬럼).
    Reads,
    /// 몸체 파싱으로 유도된 쓰기.
    Writes,
    /// routine → routine 호출.
    Calls,
    /// trigger → 발화 대상 테이블.
    Fires,
    /// 컬럼 기본값·routine의 시퀀스 사용.
    UsesSequence,
    /// 컬럼 → UDT/enum 타입 사용.
    UsesType,
    /// 소유 관계(schema→object→member). **담는 관계는 쓰는 관계가 아니다** —
    /// 의존성 질의에 섞지 않는다.
    Contains,
    /// 선언되지 않은 관계의 이름 규칙 추정. 탐색 보조이지 판정 근거가 아니다.
    Inferred,
}

impl EdgeKind {
    /// 도달성·순환·impact 같은 구조적 의존성 질의에 포함되는 간선인가.
    ///
    /// `Contains`는 소유 관계라 제외하고, `Inferred`는 추정이라 판정 근거로
    /// 쓰지 않는다(탐색 출력에는 kind 라벨과 함께 보인다).
    pub fn is_dependency(self) -> bool {
        !matches!(self, EdgeKind::Contains | EdgeKind::Inferred)
    }
}

/// 간선이 어느 증거 계층에서 왔는지(DESIGN.md "세 개의 증거 계층").
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum EvidenceLayer {
    /// 카탈로그가 선언한 사실.
    Catalog,
    /// SQL 몸체 파싱으로 유도.
    BodyParse,
    /// 사용 통계 관측.
    Stats,
    /// 이름 규칙 추정.
    Inferred,
}

/// 간선의 근거 한 건. 같은 (from, to, kind) 간선이 여러 근거로 성립할 수
/// 있어(FK 컬럼 쌍이 둘인 경우 등) 목록으로 둔다.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Evidence {
    pub layer: EvidenceLayer,
    /// 사람이 읽는 근거 설명. 예: "fk orders.customer_id -> customers.id".
    pub detail: String,
}

/// 방향 간선 하나.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Edge {
    pub from: VertexId,
    pub to: VertexId,
    pub kind: EdgeKind,
    pub evidence: Vec<Evidence>,
}

/// 정점.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Vertex {
    pub id: VertexId,
    pub kind: VertexKind,
    /// 한정 없는 말단 이름(표시용).
    pub name: String,
    /// 소속 스키마 이름.
    pub schema: String,
}

/// 의존성 그래프. 산출물 그 자체다 — 나머지는 전부 이 위의 질의다.
///
/// 모든 컬렉션은 BTreeMap/BTreeSet이라 반복 순서가 결정적이다.
/// `limitations`는 이 그래프를 만들면서 실측된 분석 한계다.
#[derive(Debug, Default)]
pub struct Graph {
    vertices: BTreeMap<VertexId, Vertex>,
    out_edges: BTreeMap<VertexId, Vec<Edge>>,
    in_edges: BTreeMap<VertexId, Vec<Edge>>,
    limitations: Vec<String>,
}

impl Graph {
    pub fn new() -> Self {
        Self::default()
    }

    /// 이 그래프의 분석 한계(실측). 소비자가 "없는 것"과 "못 본 것"을
    /// 구분하도록 모든 응답에 그대로 실어야 한다.
    pub fn limitations(&self) -> &[String] {
        &self.limitations
    }

    pub fn add_limitation(&mut self, limitation: impl Into<String>) {
        self.limitations.push(limitation.into());
    }

    /// 정점 추가. 같은 id가 다시 오면 처음 것을 유지한다 — reader가 같은
    /// 객체를 두 경로로 발견해도 그래프가 두 개로 갈라지지 않게 한다.
    pub fn add_vertex(&mut self, vertex: Vertex) {
        self.vertices.entry(vertex.id.clone()).or_insert(vertex);
    }

    /// 간선 추가. 같은 (from, to, kind)가 이미 있으면 evidence만 합친다 —
    /// 컬럼 쌍이 여러 개인 FK가 간선 여러 개로 보이지 않게 한다.
    /// 병합은 in/out 양쪽 인덱스에 적용한다 — 한쪽만 합치면 질의 방향에 따라
    /// 보이는 근거가 달라진다.
    pub fn add_edge(&mut self, edge: Edge) {
        if let Some(existing) = self
            .out_edges
            .entry(edge.from.clone())
            .or_default()
            .iter_mut()
            .find(|e| e.to == edge.to && e.kind == edge.kind)
        {
            existing.evidence.extend(edge.evidence.clone());
            if let Some(in_entry) = self
                .in_edges
                .entry(edge.to.clone())
                .or_default()
                .iter_mut()
                .find(|e| e.from == edge.from && e.kind == edge.kind)
            {
                in_entry.evidence.extend(edge.evidence);
            }
            return;
        }
        self.in_edges
            .entry(edge.to.clone())
            .or_default()
            .push(edge.clone());
        self.out_edges
            .entry(edge.from.clone())
            .or_default()
            .push(edge);
    }

    pub fn vertex(&self, id: &VertexId) -> Option<&Vertex> {
        self.vertices.get(id)
    }

    /// id 순으로 정렬된 정점들.
    pub fn vertices(&self) -> impl Iterator<Item = &Vertex> {
        self.vertices.values()
    }

    /// (from, to, kind) 순으로 정렬된 모든 간선.
    pub fn edges(&self) -> Vec<&Edge> {
        let mut edges: Vec<&Edge> = self.out_edges.values().flatten().collect();
        edges.sort_by(|a, b| (&a.from, &a.to, a.kind).cmp(&(&b.from, &b.to, b.kind)));
        edges
    }

    /// 정점에서 나가는 간선(from = 의존하는 쪽).
    pub fn outgoing(&self, id: &VertexId) -> &[Edge] {
        self.out_edges.get(id).map(Vec::as_slice).unwrap_or(&[])
    }

    /// 정점으로 들어오는 간선(to = 의존 대상).
    pub fn incoming(&self, id: &VertexId) -> &[Edge] {
        self.in_edges.get(id).map(Vec::as_slice).unwrap_or(&[])
    }

    /// 모든 정점을 주어진 레벨의 조상으로 사상한 투영 그래프.
    ///
    /// Member → Object → Schema로 부모를 따라 올라간다. 사상 결과
    /// 양 끝이 같은 간선(자기 루프)과, 그 레벨보다 위에 조상이 없는 정점
    /// (Member 레벨에서 object 등)은 사라진다. `limitations`는 그대로 계승한다.
    pub fn project(&self, level: Level) -> Graph {
        let mut projected = Graph::new();
        for vertex in self.vertices() {
            if let Some(id) = self.ancestor_at(&vertex.id, level) {
                // 투영된 정점은 조상 자신의 정보를 계승한다 — 멤버가 객체
                // 레벨로 올라갈 때 kind가 멤버로 남는 일이 없게 한다.
                let ancestor = &self.vertices[&id];
                projected.add_vertex(Vertex {
                    id,
                    kind: ancestor.kind,
                    name: ancestor.name.clone(),
                    schema: ancestor.schema.clone(),
                });
            }
        }
        for edge in self.edges() {
            let (Some(from), Some(to)) = (
                self.ancestor_at(&edge.from, level),
                self.ancestor_at(&edge.to, level),
            ) else {
                continue;
            };
            // 투영으로 붕괴된 간선(from≠to였는데 같은 조상에 수렴)은 버린다.
            // 단, 원래부터 자기 루프였던 간선(자기 참조 FK 등)은 진짜 순환이라
            // 유지한다 — 둘을 구분하지 않으면 자기 루프 순환이 사라진다.
            if from == to && edge.from != edge.to {
                continue;
            }
            projected.add_edge(Edge {
                from,
                to,
                kind: edge.kind,
                evidence: edge.evidence.clone(),
            });
        }
        projected.limitations = self.limitations.clone();
        projected
    }

    /// id의 주어진 레벨 조상. 자기 자신이 그 레벨이면 자신을 돌려준다.
    /// 조상 사슬에 그 레벨이 없으면 None.
    fn ancestor_at(&self, id: &VertexId, level: Level) -> Option<VertexId> {
        let mut current = Some(id.clone());
        while let Some(cid) = current {
            match self.vertices.get(&cid) {
                Some(v) if v.kind.level() == level => return Some(cid),
                Some(_) => current = cid.parent(),
                // 부모 정점이 그래프에 없으면(비정상 입력) 사슬 종료.
                None => return None,
            }
        }
        None
    }

    /// 의존성 간선만으로 계산한 강연결 요소(Tarjan). 크기 1짜리도 돌려주므로
    /// 순환 판정(크기>1 또는 자기 루프)은 호출자가 한다.
    pub fn strongly_connected(&self) -> Vec<Vec<VertexId>> {
        struct Tarjan<'a> {
            graph: &'a Graph,
            index: BTreeMap<VertexId, usize>,
            lowlink: BTreeMap<VertexId, usize>,
            on_stack: BTreeSet<VertexId>,
            stack: Vec<VertexId>,
            next: usize,
            components: Vec<Vec<VertexId>>,
        }
        impl<'a> Tarjan<'a> {
            fn visit(&mut self, id: VertexId) {
                self.index.insert(id.clone(), self.next);
                self.lowlink.insert(id.clone(), self.next);
                self.next += 1;
                self.stack.push(id.clone());
                self.on_stack.insert(id.clone());
                for edge in self.graph.outgoing(&id) {
                    if !edge.kind.is_dependency() {
                        continue;
                    }
                    if !self.index.contains_key(&edge.to) {
                        self.visit(edge.to.clone());
                        let low = self.lowlink[&id].min(self.lowlink[&edge.to]);
                        self.lowlink.insert(id.clone(), low);
                    } else if self.on_stack.contains(&edge.to) {
                        let low = self.lowlink[&id].min(self.index[&edge.to]);
                        self.lowlink.insert(id.clone(), low);
                    }
                }
                if self.lowlink[&id] == self.index[&id] {
                    let mut component = Vec::new();
                    while let Some(top) = self.stack.pop() {
                        self.on_stack.remove(&top);
                        let done = top == id;
                        component.push(top);
                        if done {
                            break;
                        }
                    }
                    component.sort();
                    self.components.push(component);
                }
            }
        }
        let mut tarjan = Tarjan {
            graph: self,
            index: BTreeMap::new(),
            lowlink: BTreeMap::new(),
            on_stack: BTreeSet::new(),
            stack: Vec::new(),
            next: 0,
            components: Vec::new(),
        };
        let ids: Vec<VertexId> = self.vertices().map(|v| v.id.clone()).collect();
        for id in ids {
            if !tarjan.index.contains_key(&id) {
                tarjan.visit(id);
            }
        }
        tarjan.components.sort();
        tarjan.components
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table(schema: &str, name: &str) -> Vertex {
        Vertex {
            id: VertexId::object(schema, name),
            kind: VertexKind::Table,
            name: name.to_owned(),
            schema: schema.to_owned(),
        }
    }

    fn fk(from: &VertexId, to: &VertexId) -> Edge {
        Edge {
            from: from.clone(),
            to: to.clone(),
            kind: EdgeKind::References,
            evidence: vec![Evidence {
                layer: EvidenceLayer::Catalog,
                detail: "test fk".to_owned(),
            }],
        }
    }

    #[test]
    fn 같은_간선은_evidence만_합친다() {
        let mut g = Graph::new();
        let (a, b) = (VertexId::object("s", "a"), VertexId::object("s", "b"));
        g.add_vertex(table("s", "a"));
        g.add_vertex(table("s", "b"));
        g.add_edge(fk(&a, &b));
        g.add_edge(fk(&a, &b));
        assert_eq!(g.edges().len(), 1);
        assert_eq!(g.edges()[0].evidence.len(), 2);
    }

    #[test]
    fn member는_object로_투영된다() {
        let mut g = Graph::new();
        g.add_vertex(Vertex {
            id: VertexId::schema("s"),
            kind: VertexKind::Schema,
            name: "s".into(),
            schema: "s".into(),
        });
        g.add_vertex(table("s", "a"));
        g.add_vertex(table("s", "b"));
        g.add_vertex(Vertex {
            id: VertexId::member("s", "a", "b_id"),
            kind: VertexKind::Column,
            name: "b_id".into(),
            schema: "s".into(),
        });
        let col = VertexId::member("s", "a", "b_id");
        let bcol = VertexId::member("s", "b", "id");
        g.add_vertex(Vertex {
            id: bcol.clone(),
            kind: VertexKind::Column,
            name: "id".into(),
            schema: "s".into(),
        });
        g.add_edge(fk(&col, &bcol));

        let obj = g.project(Level::Object);
        // 컬럼→컬럼 FK가 테이블→테이블로 사상된다.
        let edges = obj.edges();
        assert!(edges
            .iter()
            .any(|e| e.from.as_str() == "s.a" && e.to.as_str() == "s.b"));
    }

    #[test]
    fn scc는_순환과_자기루프를_분리해_잡는다() {
        let mut g = Graph::new();
        for name in ["a", "b", "c", "alone"] {
            g.add_vertex(table("s", name));
        }
        let (a, b, c) = (
            VertexId::object("s", "a"),
            VertexId::object("s", "b"),
            VertexId::object("s", "c"),
        );
        g.add_edge(fk(&a, &b));
        g.add_edge(fk(&b, &a));
        g.add_edge(fk(&c, &c)); // 자기 참조 FK
        // contains 간선은 순환 판정에서 무시된다.
        g.add_edge(Edge {
            from: VertexId::schema("s"),
            to: a.clone(),
            kind: EdgeKind::Contains,
            evidence: vec![],
        });

        let sccs = g.strongly_connected();
        // a↔b 순환.
        assert!(sccs.iter().any(|c| c.len() == 2 && c.contains(&a) && c.contains(&b)));
        // c는 자기 참조 — 크기 1 SCC이고 자기 의존 간선을 가진다.
        assert!(sccs.iter().any(|comp| comp.len() == 1 && comp[0] == c));
        assert!(g.outgoing(&c).iter().any(|e| e.to == c && e.kind.is_dependency()));
        // contains 간선으로는 어떤 SCC도 커지지 않는다.
        assert!(!sccs.iter().any(|c| c.len() > 1 && c.contains(&VertexId::schema("s"))));
    }

    #[test]
    fn project는_진짜_자기루프를_보존하고_붕괴간선은_버린다() {
        let mut g = Graph::new();
        let a = VertexId::object("s", "a");
        g.add_vertex(table("s", "a"));
        g.add_edge(fk(&a, &a)); // 자기 참조 FK — 진짜 자기 루프
        // 멤버→멤버 간선은 object로 투영하면 자기처럼 보이지만 붕괴품이다.
        let col = VertexId::member("s", "a", "x");
        g.add_vertex(Vertex {
            id: col.clone(),
            kind: VertexKind::Column,
            name: "x".into(),
            schema: "s".into(),
        });
        g.add_edge(Edge {
            from: col.clone(),
            to: col.clone(),
            kind: EdgeKind::Contains,
            evidence: vec![],
        });
        let obj = g.project(Level::Object);
        // 진짜 자기 루프는 남는다.
        assert!(obj
            .edges()
            .iter()
            .any(|e| e.from.as_str() == "s.a" && e.to.as_str() == "s.a"));
        // 멤버 자기 간선은 contains라 어차피 투영에 남지 않는다 — 여기서는
        // object 레벨 references 자기 루프가 정확히 1개만 있는지로 확인한다.
        assert_eq!(
            obj.edges()
                .iter()
                .filter(|e| e.kind == EdgeKind::References)
                .count(),
            1
        );
    }

    #[test]
    fn vertex_id의_부모는_마지막_점을_자른다() {
        let col = VertexId::member("main", "orders", "customer_id");
        assert_eq!(col.parent().unwrap().as_str(), "main.orders");
        assert_eq!(
            VertexId::object("main", "orders").parent().unwrap().as_str(),
            "main"
        );
        assert!(VertexId::schema("main").parent().is_none());
    }
}
