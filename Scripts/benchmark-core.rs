//! 동일한 생성 그래프에서 간선 구축 비용과 반복 조회 비용을 분리해 측정한다.
use schemagraph_core::{Edge, EdgeKind, Evidence, EvidenceLayer, Graph, Vertex, VertexId, VertexKind};
use std::time::Instant;

fn main() {
    let count = std::env::args().nth(1).and_then(|v|v.parse::<usize>().ok()).unwrap_or(30_000);
    let start = Instant::now();
    let mut graph = Graph::new();
    for n in 0..=count {
        let name = format!("t{n:06}");
        graph.add_vertex(Vertex {id:VertexId::object("public",&name),kind:VertexKind::Table,name,schema:"public".into()});
    }
    let root = VertexId::object("public","t000000");
    for n in 1..=count {
        graph.add_edge(Edge {from:root.clone(),to:VertexId::object("public",&format!("t{n:06}")),kind:EdgeKind::Reads,evidence:vec![Evidence {layer:EvidenceLayer::BodyParse,detail:"wide generated view".into()}]});
    }
    let build_us = start.elapsed().as_micros();
    let query = Instant::now();
    let mut checksum=0usize;
    for _ in 0..100 {
        for edge in graph.outgoing(&root) { checksum += usize::from(graph.vertex(&edge.to).is_some()); }
    }
    assert_eq!(checksum,count*100);
    println!("{{\"vertices\":{},\"edges\":{},\"build_us\":{},\"query_100_us\":{},\"checksum\":{}}}",graph.vertices().count(),graph.edges().len(),build_us,query.elapsed().as_micros(),checksum);
}
