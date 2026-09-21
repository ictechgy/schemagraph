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
    scan_with_dialect(sql, outputs, "postgres")
}

fn scan_with_dialect(sql: &str, outputs: &[&str], dialect: &str) -> Graph {
    let mut view = object("v", outputs);
    view.kind = "view".into();
    view.body = Some(sql.into());
    let doc = CatalogDocument {
        context: None,
        dependencies: Vec::new(),
        version: 1,
        dialect: dialect.into(),
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

fn has_diagnostic(g: &Graph, code: &str) -> bool {
    g.analysis()
        .values()
        .flat_map(|analysis| &analysis.diagnostics)
        .any(|diagnostic| diagnostic.code == code)
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
fn outer_join_using_lineage_follows_the_preserved_side() {
    for (join, source) in [
        ("LEFT JOIN", "s.orders.id"),
        ("RIGHT JOIN", "s.customers.id"),
        ("FULL JOIN", "both"),
    ] {
        let graph = scan(
            &format!(
                "SELECT id AS merged, o.id AS left_id, c.id AS right_id FROM orders o {join} customers c USING(id)"
            ),
            &["merged", "left_id", "right_id"],
        );
        complete(&graph);
        assert!(
            edge(&graph, "s.v.merged", "s.orders.id", EdgeKind::DerivesFrom)
                == (source != "s.customers.id")
        );
        assert!(
            edge(
                &graph,
                "s.v.merged",
                "s.customers.id",
                EdgeKind::DerivesFrom
            ) == (source != "s.orders.id")
        );
        assert!(edge(
            &graph,
            "s.v.left_id",
            "s.orders.id",
            EdgeKind::DerivesFrom
        ));
        assert!(edge(
            &graph,
            "s.v.right_id",
            "s.customers.id",
            EdgeKind::DerivesFrom
        ));
        assert!(edge(&graph, "s.v", "s.orders.id", EdgeKind::Reads));
        assert!(edge(&graph, "s.v", "s.customers.id", EdgeKind::Reads));
    }
}

#[test]
fn natural_outer_join_using_lineage_follows_the_preserved_side() {
    for (join, source) in [
        ("LEFT JOIN", "s.orders.id"),
        ("RIGHT JOIN", "s.customers.id"),
        ("FULL JOIN", "both"),
    ] {
        let graph = scan(
            &format!("SELECT id AS merged FROM orders o NATURAL {join} customers c"),
            &["merged"],
        );
        complete(&graph);
        assert!(
            edge(&graph, "s.v.merged", "s.orders.id", EdgeKind::DerivesFrom)
                == (source != "s.customers.id")
        );
        assert!(
            edge(
                &graph,
                "s.v.merged",
                "s.customers.id",
                EdgeKind::DerivesFrom
            ) == (source != "s.orders.id")
        );
        assert!(edge(&graph, "s.v", "s.orders.id", EdgeKind::Reads));
        assert!(edge(&graph, "s.v", "s.customers.id", EdgeKind::Reads));
    }
}

#[test]
fn sqlite_outer_join_using_keeps_joined_column_order() {
    for (join, source) in [
        ("LEFT JOIN", "s.orders.id"),
        ("RIGHT JOIN", "s.customers.id"),
        ("FULL JOIN", "both"),
    ] {
        let graph = scan_with_dialect(
            &format!("SELECT * FROM orders o {join} customers c USING(id)"),
            &["id", "customer_id", "amount", "name"],
            "sqlite",
        );
        complete(&graph);
        assert!(
            edge(&graph, "s.v.id", "s.orders.id", EdgeKind::DerivesFrom)
                == (source != "s.customers.id")
        );
        assert!(
            edge(&graph, "s.v.id", "s.customers.id", EdgeKind::DerivesFrom)
                == (source != "s.orders.id")
        );
        assert!(edge(
            &graph,
            "s.v.customer_id",
            "s.orders.customer_id",
            EdgeKind::DerivesFrom
        ));
        assert!(edge(
            &graph,
            "s.v.name",
            "s.customers.name",
            EdgeKind::DerivesFrom
        ));
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
fn named_and_inherited_windows_match_inline_lineage() {
    let inline = scan(
        "SELECT ROW_NUMBER() OVER (PARTITION BY customer_id ORDER BY amount) AS position FROM orders",
        &["position"],
    );
    let named = scan(
        "SELECT ROW_NUMBER() OVER ranking AS position FROM orders WINDOW ranking AS (PARTITION BY customer_id ORDER BY amount)",
        &["position"],
    );
    let inherited = scan(
        "SELECT ROW_NUMBER() OVER ranking AS position FROM orders WINDOW base AS (PARTITION BY customer_id), ranking AS (base ORDER BY amount)",
        &["position"],
    );
    for graph in [&inline, &named, &inherited] {
        complete(graph);
        assert!(edge(
            graph,
            "s.v.position",
            "s.orders.customer_id",
            EdgeKind::DerivesFrom
        ));
        assert!(edge(
            graph,
            "s.v.position",
            "s.orders.amount",
            EdgeKind::DerivesFrom
        ));
        assert!(edge(graph, "s.v", "s.orders.customer_id", EdgeKind::Reads));
        assert!(edge(graph, "s.v", "s.orders.amount", EdgeKind::Reads));
    }
}

#[test]
fn named_windows_are_query_local_and_unused_definitions_do_not_contaminate_values() {
    let unused = scan(
        "SELECT amount AS total FROM orders WINDOW unused AS (PARTITION BY customer_id ORDER BY id)",
        &["total"],
    );
    complete(&unused);
    assert!(edge(
        &unused,
        "s.v.total",
        "s.orders.amount",
        EdgeKind::DerivesFrom
    ));
    assert!(edge(
        &unused,
        "s.v",
        "s.orders.customer_id",
        EdgeKind::Reads
    ));
    assert!(edge(&unused, "s.v", "s.orders.id", EdgeKind::Reads));

    let shadowed = scan(
        "SELECT (SELECT ROW_NUMBER() OVER ranking FROM orders inner_orders WINDOW ranking AS (PARTITION BY amount)) AS position FROM orders outer_orders WINDOW ranking AS (PARTITION BY customer_id)",
        &["position"],
    );
    complete(&shadowed);
    assert!(edge(
        &shadowed,
        "s.v.position",
        "s.orders.amount",
        EdgeKind::DerivesFrom
    ));
    assert!(!edge(
        &shadowed,
        "s.v.position",
        "s.orders.customer_id",
        EdgeKind::DerivesFrom
    ));

    let leaked = scan(
        "SELECT (SELECT ROW_NUMBER() OVER ranking FROM orders inner_orders) AS position FROM orders outer_orders WINDOW ranking AS (PARTITION BY customer_id)",
        &["position"],
    );
    assert_eq!(
        leaked.analysis()[&VertexId::from_raw("s.v")].state,
        AnalysisState::Partial
    );
    assert!(has_diagnostic(&leaked, "SG_WINDOW_UNKNOWN"));
}

#[test]
fn invalid_named_windows_are_partial_without_guessing_sources() {
    let unknown = scan(
        "SELECT ROW_NUMBER() OVER missing AS position FROM orders",
        &["position"],
    );
    assert_eq!(
        unknown.analysis()[&VertexId::from_raw("s.v")].state,
        AnalysisState::Partial
    );
    assert!(has_diagnostic(&unknown, "SG_WINDOW_UNKNOWN"));

    let cycle = scan(
        "SELECT ROW_NUMBER() OVER first_window AS position FROM orders WINDOW first_window AS (second_window), second_window AS (first_window)",
        &["position"],
    );
    assert_eq!(
        cycle.analysis()[&VertexId::from_raw("s.v")].state,
        AnalysisState::Partial
    );
    assert!(has_diagnostic(&cycle, "SG_WINDOW_CYCLE"));

    let definitions = (0..=64)
        .map(|index| {
            if index == 64 {
                format!("w{index} AS (PARTITION BY id)")
            } else {
                format!("w{index} AS (w{})", index + 1)
            }
        })
        .collect::<Vec<_>>()
        .join(", ");
    let depth = scan(
        &format!("SELECT ROW_NUMBER() OVER w0 AS position FROM orders WINDOW {definitions}"),
        &["position"],
    );
    assert_eq!(
        depth.analysis()[&VertexId::from_raw("s.v")].state,
        AnalysisState::Partial
    );
    assert!(has_diagnostic(&depth, "SG_WINDOW_DEPTH"));
}

#[test]
fn recursive_window_expressions_are_bounded_and_partial() {
    let graph = scan(
        "SELECT ROW_NUMBER() OVER recursive_window AS position FROM orders WINDOW recursive_window AS (PARTITION BY ROW_NUMBER() OVER recursive_window)",
        &["position"],
    );
    assert_eq!(
        graph.analysis()[&VertexId::from_raw("s.v")].state,
        AnalysisState::Partial
    );
    assert!(has_diagnostic(&graph, "SG_WINDOW_CYCLE"));
    assert!(!edge(
        &graph,
        "s.v.position",
        "s.orders.customer_id",
        EdgeKind::DerivesFrom
    ));
}

#[test]
fn branching_named_windows_have_a_shared_expansion_budget() {
    let mut definitions = vec!["w0 AS (PARTITION BY customer_id)".to_owned()];
    for index in 1..24 {
        let parent = index - 1;
        definitions.push(format!(
            "w{index} AS (PARTITION BY ROW_NUMBER() OVER w{parent} + ROW_NUMBER() OVER w{parent})"
        ));
    }
    let graph = scan(
        &format!(
            "SELECT ROW_NUMBER() OVER w23 AS position FROM orders WINDOW {}",
            definitions.join(", ")
        ),
        &["position"],
    );
    assert_eq!(
        graph.analysis()[&VertexId::from_raw("s.v")].state,
        AnalysisState::Partial
    );
    assert!(has_diagnostic(&graph, "SG_WINDOW_BUDGET"));
    assert!(!edge(
        &graph,
        "s.v.position",
        "s.orders.customer_id",
        EdgeKind::DerivesFrom
    ));
}

#[test]
fn named_window_diagnostics_taint_projection_lineage() {
    let duplicate = scan(
        "SELECT ROW_NUMBER() OVER ranking AS position FROM orders WINDOW ranking AS (PARTITION BY customer_id), ranking AS (PARTITION BY amount)",
        &["position"],
    );
    assert_eq!(
        duplicate.analysis()[&VertexId::from_raw("s.v")].state,
        AnalysisState::Partial
    );
    assert!(has_diagnostic(&duplicate, "SG_WINDOW_DUPLICATE"));
    assert!(!edge(
        &duplicate,
        "s.v.position",
        "s.orders.customer_id",
        EdgeKind::DerivesFrom
    ));
    assert!(!edge(
        &duplicate,
        "s.v.position",
        "s.orders.amount",
        EdgeKind::DerivesFrom
    ));

    let unknown = scan(
        "SELECT ROW_NUMBER() OVER missing AS position FROM orders",
        &["position"],
    );
    let diagnostic = unknown.analysis()[&VertexId::from_raw("s.v")]
        .diagnostics
        .iter()
        .find(|diagnostic| diagnostic.code == "SG_WINDOW_UNKNOWN")
        .expect("unknown window diagnostic");
    assert!(diagnostic.location.is_some());
    assert!(!edge(
        &unknown,
        "s.v.position",
        "s.orders.customer_id",
        EdgeKind::DerivesFrom
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
