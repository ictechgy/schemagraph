//! SQL 스코프와 값 계보를 필요한 간선·금지된 간선 양쪽으로 검증한다.

use schemagraph_core::{AnalysisState, EdgeKind, Graph, VertexId};
use schemagraph_source::document::{CatalogDocument, ColumnDoc, ObjectDoc, SchemaDoc};

fn columns(names: &[&str]) -> Vec<ColumnDoc> {
    names
        .iter()
        .enumerate()
        .map(|(i, name)| ColumnDoc {
            name: (*name).into(),
            data_type: "INTEGER".into(),
            nullable: true,
            default: None,
            ordinal: i as u32 + 1,
            pk_position: 0,
        })
        .collect()
}

fn object(name: &str, names: &[&str]) -> ObjectDoc {
    ObjectDoc {
        name: name.into(),
        kind: "table".into(),
        columns: columns(names),
        constraints: vec![],
        indexes: vec![],
        triggers: vec![],
        body: None,
        usage: None,
    }
}

fn scan(sql: &str, outputs: &[&str]) -> Graph {
    let mut view = object("v", outputs);
    view.kind = "view".into();
    view.body = Some(sql.into());
    let doc = CatalogDocument {
        context: None,
        dependencies: Vec::new(),
        version: 1,
        dialect: "postgres".into(),
        reader: "fixture".into(),
        limitations: vec![],
        schemas: vec![SchemaDoc {
            name: "s".into(),
            routines: vec![],
            objects: vec![
                object("orders", &["id", "customer_id", "amount"]),
                object("customers", &["id", "name"]),
                view,
            ],
        }],
    };
    let mut graph = schemagraph_source::graph::document_to_graph(&doc);
    let (_, notes) = super::enrich_from_document(&mut graph, &doc);
    for note in notes {
        graph.add_limitation(note);
    }
    for edge in graph.edges() {
        assert!(graph.vertex(&edge.from).is_some());
        assert!(graph.vertex(&edge.to).is_some());
    }
    graph
}

fn edge(g: &Graph, from: &str, to: &str, kind: EdgeKind) -> bool {
    g.edges()
        .iter()
        .any(|e| e.from.as_str() == from && e.to.as_str() == to && e.kind == kind)
}

fn complete(g: &Graph) {
    assert_eq!(
        g.analysis()[&VertexId::from_raw("s.v")].state,
        AnalysisState::Complete,
        "{:?}",
        g.limitations()
    );
}

#[test]
fn unqualified_column_and_predicate_have_distinct_lineage() {
    let g = scan(
        "SELECT amount AS total FROM orders WHERE customer_id IS NOT NULL",
        &["total"],
    );
    complete(&g);
    assert!(edge(
        &g,
        "s.v.total",
        "s.orders.amount",
        EdgeKind::DerivesFrom
    ));
    assert!(edge(&g, "s.v", "s.orders.customer_id", EdgeKind::Reads));
    assert!(!edge(
        &g,
        "s.v.total",
        "s.orders.customer_id",
        EdgeKind::DerivesFrom
    ));
    assert!(g.origins().values().flatten().any(|o| o.location.is_some()));
}

#[test]
fn ambiguous_unqualified_column_is_not_guessed() {
    let g = scan(
        "SELECT id FROM orders o JOIN customers c ON c.id=o.customer_id",
        &["id"],
    );
    assert_eq!(
        g.analysis()[&VertexId::from_raw("s.v")].state,
        AnalysisState::Partial
    );
    assert!(!edge(&g, "s.v.id", "s.orders.id", EdgeKind::DerivesFrom));
    assert!(!edge(&g, "s.v.id", "s.customers.id", EdgeKind::DerivesFrom));
    assert!(g
        .analysis()
        .values()
        .flat_map(|a| &a.diagnostics)
        .any(|d| d.code == "SG_COLUMN_AMBIGUOUS"));
}

#[test]
fn wildcard_and_cte_column_aliases_preserve_position() {
    let g = scan(
        "WITH q(a,b,c) AS (SELECT * FROM orders) SELECT q.* FROM q",
        &["first", "second", "third"],
    );
    complete(&g);
    for (out, input) in [
        ("first", "id"),
        ("second", "customer_id"),
        ("third", "amount"),
    ] {
        assert!(edge(
            &g,
            &format!("s.v.{out}"),
            &format!("s.orders.{input}"),
            EdgeKind::DerivesFrom
        ));
    }
    assert!(!g.limitations().iter().any(|n| n.contains("s.q")));
}

#[test]
fn cte_shadows_table_but_schema_qualified_table_does_not() {
    let g=scan("WITH orders AS (SELECT name FROM customers) SELECT q.name,o.id FROM orders q CROSS JOIN s.orders o",&["name","id"]);
    complete(&g);
    assert!(edge(
        &g,
        "s.v.name",
        "s.customers.name",
        EdgeKind::DerivesFrom
    ));
    assert!(edge(&g, "s.v.id", "s.orders.id", EdgeKind::DerivesFrom));
}

#[test]
fn correlated_subquery_keeps_inner_and_outer_aliases_separate() {
    let g=scan("SELECT o.id,(SELECT c.name FROM customers c WHERE c.id=o.customer_id) AS name FROM orders o",&["id","name"]);
    complete(&g);
    assert!(edge(&g, "s.v.id", "s.orders.id", EdgeKind::DerivesFrom));
    assert!(edge(
        &g,
        "s.v.name",
        "s.customers.name",
        EdgeKind::DerivesFrom
    ));
    assert!(edge(&g, "s.v", "s.orders.customer_id", EdgeKind::Reads));
    assert!(!edge(&g, "s.v.id", "s.customers.id", EdgeKind::DerivesFrom));
}

#[test]
fn inner_alias_does_not_resolve_an_unknown_outer_column() {
    let g = scan(
        "SELECT o.name FROM orders o WHERE EXISTS (SELECT 1 FROM customers o)",
        &["name"],
    );
    assert_eq!(
        g.analysis()[&VertexId::from_raw("s.v")].state,
        AnalysisState::Partial
    );
    assert!(!edge(
        &g,
        "s.v.name",
        "s.customers.name",
        EdgeKind::DerivesFrom
    ));
}

#[test]
fn union_collects_both_values_but_except_rhs_only_affects_rows() {
    let union = scan(
        "SELECT id FROM orders UNION ALL SELECT id FROM customers",
        &["id"],
    );
    complete(&union);
    assert!(edge(&union, "s.v.id", "s.orders.id", EdgeKind::DerivesFrom));
    assert!(edge(
        &union,
        "s.v.id",
        "s.customers.id",
        EdgeKind::DerivesFrom
    ));
    let except = scan(
        "SELECT id FROM orders EXCEPT SELECT id FROM customers",
        &["id"],
    );
    complete(&except);
    assert!(edge(&except, "s.v", "s.customers.id", EdgeKind::Reads));
    assert!(!edge(
        &except,
        "s.v.id",
        "s.customers.id",
        EdgeKind::DerivesFrom
    ));
}

#[test]
fn nested_join_keeps_each_qualified_relation() {
    let g = scan(
        "SELECT o.id,c.name FROM (orders o JOIN customers c ON o.customer_id=c.id)",
        &["id", "name"],
    );
    complete(&g);
    assert!(edge(&g, "s.v.id", "s.orders.id", EdgeKind::DerivesFrom));
    assert!(edge(
        &g,
        "s.v.name",
        "s.customers.name",
        EdgeKind::DerivesFrom
    ));
}

#[test]
fn join_using_merges_unqualified_key_and_preserves_qualified_keys() {
    let g = scan(
        "SELECT id,o.id,c.id FROM orders o JOIN customers c USING(id)",
        &["merged", "left_id", "right_id"],
    );
    complete(&g);
    for source in ["s.orders.id", "s.customers.id"] {
        assert!(edge(&g, "s.v.merged", source, EdgeKind::DerivesFrom));
    }
    assert!(!edge(
        &g,
        "s.v.left_id",
        "s.customers.id",
        EdgeKind::DerivesFrom
    ));
}

#[test]
fn plain_left_and_explicit_inner_join_conditions_are_reads_not_value_lineage() {
    for join in ["JOIN", "INNER JOIN", "LEFT JOIN", "LEFT OUTER JOIN"] {
        let g = scan(
            &format!("SELECT o.amount FROM orders o {join} customers c ON o.customer_id=c.id"),
            &["amount"],
        );
        complete(&g);
        for input in ["s.orders.customer_id", "s.customers.id"] {
            assert!(edge(&g, "s.v", input, EdgeKind::Reads), "{join}: {input}");
            assert!(!edge(&g, "s.v.amount", input, EdgeKind::DerivesFrom));
        }
    }
}

#[test]
fn grouping_having_window_and_ordering_are_analyzed() {
    let g=scan("SELECT customer_id AS c,SUM(amount) AS total FROM orders GROUP BY customer_id HAVING SUM(amount)>0 ORDER BY total",&["c","total"]);
    complete(&g);
    assert!(edge(
        &g,
        "s.v.total",
        "s.orders.amount",
        EdgeKind::DerivesFrom
    ));
    let window = scan(
        "SELECT ROW_NUMBER() OVER(PARTITION BY customer_id ORDER BY amount) AS n FROM orders",
        &["n"],
    );
    complete(&window);
    assert!(edge(&window, "s.v", "s.orders.amount", EdgeKind::Reads));
    assert!(edge(
        &window,
        "s.v",
        "s.orders.customer_id",
        EdgeKind::Reads
    ));
}

#[test]
fn order_by_output_alias_takes_precedence_over_input_column() {
    let g = scan("SELECT customer_id AS id FROM orders ORDER BY id", &["id"]);
    complete(&g);
    assert!(!edge(&g, "s.v", "s.orders.id", EdgeKind::Reads));
    assert!(edge(
        &g,
        "s.v.id",
        "s.orders.customer_id",
        EdgeKind::DerivesFrom
    ));
}

#[test]
fn constants_succeed_without_fabricating_a_relation() {
    let g = scan("SELECT 1 AS one", &["one"]);
    complete(&g);
    assert!(!g
        .outgoing(&VertexId::from_raw("s.v"))
        .iter()
        .any(|e| e.kind.is_dependency()));
}

#[test]
fn recursive_cte_is_partial_without_ghost_vertices() {
    let g=scan("WITH RECURSIVE q(id) AS (SELECT id FROM orders UNION ALL SELECT id FROM q) SELECT id FROM q",&["id"]);
    assert_eq!(
        g.analysis()[&VertexId::from_raw("s.v")].state,
        AnalysisState::Partial
    );
    assert!(edge(&g, "s.v", "s.orders.id", EdgeKind::Reads));
}

#[test]
fn missing_relation_blocks_wildcard_lineage_instead_of_shifting_columns() {
    let g = scan(
        "SELECT q.*,o.id FROM missing q CROSS JOIN orders o",
        &["id"],
    );
    assert_eq!(
        g.analysis()[&VertexId::from_raw("s.v")].state,
        AnalysisState::Partial
    );
    assert!(!edge(&g, "s.v.id", "s.orders.id", EdgeKind::DerivesFrom));
}
