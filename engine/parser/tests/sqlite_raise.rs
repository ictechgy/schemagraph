use schemagraph_core::{AnalysisState, EdgeKind, Graph, VertexId};
use schemagraph_parser::enrich_from_document;
use schemagraph_source::{codec::document_from_value, graph::document_to_graph};
use serde_json::json;

fn analyze(dialect: &str, body: &str, routines: serde_json::Value) -> Graph {
    let document = document_from_value(json!({
        "version": 1,
        "dialect": dialect,
        "reader": "sqlite-raise-regression",
        "schemas": [{
            "name": "s",
            "objects": [
                {
                    "name": "orders", "kind": "table",
                    "columns": [
                        {"name": "id", "data_type": "INTEGER", "nullable": false, "ordinal": 1, "pk_position": 0},
                        {"name": "customer_id", "data_type": "INTEGER", "nullable": true, "ordinal": 2, "pk_position": 0}
                    ],
                    "constraints": [], "indexes": [],
                    "triggers": [{"name": "guard", "body": body}]
                },
                {
                    "name": "customers", "kind": "table",
                    "columns": [
                        {"name": "id", "data_type": "INTEGER", "nullable": false, "ordinal": 1, "pk_position": 0},
                        {"name": "name", "data_type": "TEXT", "nullable": true, "ordinal": 2, "pk_position": 0}
                    ],
                    "constraints": [], "indexes": [], "triggers": []
                }
            ],
            "routines": routines
        }],
        "limitations": []
    }))
    .expect("regression catalog must decode");
    let mut graph = document_to_graph(&document);
    let (_, notes) = enrich_from_document(&mut graph, &document);
    for note in notes {
        graph.add_limitation(note);
    }
    for edge in graph.edges() {
        assert!(graph.vertex(&edge.from).is_some());
        assert!(graph.vertex(&edge.to).is_some());
    }
    graph
}

fn state(graph: &Graph) -> AnalysisState {
    graph.analysis()[&VertexId::from_raw("s.orders.guard")].state
}

fn has_edge(graph: &Graph, to: &str, kind: EdgeKind) -> bool {
    graph.edges().iter().any(|edge| {
        edge.from.as_str() == "s.orders.guard" && edge.to.as_str() == to && edge.kind == kind
    })
}

#[test]
fn sqlite_raise_actions_preserve_trigger_dependencies_without_a_routine_call() {
    for expression in [
        "RAISE(ABORT, 'blocked')",
        "RAISE(FAIL, 'blocked')",
        "RAISE(ROLLBACK, 'blocked')",
        "RAISE(IGNORE)",
        "rAiSe(aBoRt, 'blocked')",
    ] {
        let body = format!(
            "CREATE TRIGGER guard AFTER INSERT ON orders BEGIN \
             SELECT {expression} WHERE NEW.customer_id IN (SELECT id FROM customers); \
             UPDATE customers SET name = 'blocked' WHERE id = NEW.customer_id; END"
        );
        let graph = analyze("sqlite", &body, json!([]));
        assert_eq!(
            state(&graph),
            AnalysisState::Complete,
            "{expression}: {:?}",
            graph.limitations()
        );
        assert!(
            graph.limitations().is_empty(),
            "{expression}: {:?}",
            graph.limitations()
        );
        assert!(has_edge(&graph, "s.customers", EdgeKind::Reads));
        assert!(has_edge(&graph, "s.customers", EdgeKind::Writes));
        assert!(has_edge(&graph, "s.orders.customer_id", EdgeKind::Reads));
        assert!(!graph
            .edges()
            .iter()
            .any(|edge| edge.kind == EdgeKind::Calls));
    }
}

#[test]
fn sqlite_raise_message_subqueries_remain_read_dependencies() {
    let graph = analyze(
        "sqlite",
        "SELECT RAISE(ABORT, (SELECT name FROM customers WHERE id = NEW.customer_id))",
        json!([]),
    );
    assert_eq!(
        state(&graph),
        AnalysisState::Complete,
        "limitations: {:?}; analysis: {:?}",
        graph.limitations(),
        graph.analysis()
    );
    assert!(has_edge(&graph, "s.customers", EdgeKind::Reads));
    assert!(has_edge(&graph, "s.orders.customer_id", EdgeKind::Reads));
}

#[test]
fn sqlite_unknown_functions_in_raise_messages_still_report_partial_analysis() {
    let graph = analyze(
        "sqlite",
        "SELECT RAISE(ABORT, custom_message(NEW.customer_id)) FROM customers",
        json!([]),
    );
    assert_eq!(state(&graph), AnalysisState::Partial);
    assert!(graph
        .limitations()
        .iter()
        .any(|note| note.contains("custom_message")));
    assert!(!graph
        .limitations()
        .iter()
        .any(|note| note.contains("s.RAISE")));
    assert!(has_edge(&graph, "s.customers", EdgeKind::Reads));
    assert!(has_edge(&graph, "s.orders.customer_id", EdgeKind::Reads));
}

#[test]
fn malformed_or_qualified_raise_calls_are_not_silently_accepted() {
    for expression in [
        "RAISE(ABORT)",
        "RAISE(IGNORE, 'blocked')",
        "RAISE(UNKNOWN, 'blocked')",
        "RAISE('ABORT', 'blocked')",
        "s.RAISE(ABORT, 'blocked')",
        "\"RAISE\"(ABORT, 'blocked')",
        "RAISE(DISTINCT ABORT, 'blocked')",
        "RAISE(ABORT, 'blocked') FILTER (WHERE 1 = 1)",
        "RAISE(ABORT, 'blocked') OVER ()",
        "RAISE(ABORT, )",
    ] {
        let graph = analyze("sqlite", &format!("SELECT {expression}"), json!([]));
        assert_ne!(state(&graph), AnalysisState::Complete, "{expression}");
        assert!(!graph.limitations().is_empty(), "{expression}");
    }
}

#[test]
fn sqlite_raise_inside_dml_keeps_message_sources_and_skips_the_action_keyword() {
    let graph = analyze(
        "sqlite",
        "INSERT INTO customers (id, name) SELECT NEW.customer_id, \
         RAISE(ABORT, (SELECT id FROM orders WHERE id = NEW.id))",
        json!([]),
    );
    assert_eq!(
        state(&graph),
        AnalysisState::Complete,
        "limitations: {:?}; analysis: {:?}",
        graph.limitations(),
        graph.analysis()
    );
    assert!(has_edge(&graph, "s.customers", EdgeKind::Writes));
    assert!(has_edge(&graph, "s.orders.id", EdgeKind::Reads));
    assert!(has_edge(&graph, "s.orders.customer_id", EdgeKind::Reads));
    assert!(!graph
        .edges()
        .iter()
        .any(|edge| edge.kind == EdgeKind::Calls));
}

#[test]
fn sqlite_raise_does_not_hide_temporal_self_lineage_limitations() {
    let graph = analyze(
        "sqlite",
        "INSERT INTO customers (id, name) SELECT NEW.customer_id, \
         RAISE(ABORT, (SELECT name FROM customers WHERE id = NEW.customer_id))",
        json!([]),
    );
    assert_eq!(state(&graph), AnalysisState::Partial);
    assert!(graph.analysis()[&VertexId::from_raw("s.orders.guard")]
        .diagnostics
        .iter()
        .any(|diagnostic| diagnostic.code == "SG_TEMPORAL_SELF_LINEAGE"));
    assert!(!graph
        .limitations()
        .iter()
        .any(|note| note.contains("s.RAISE")));
    assert!(has_edge(&graph, "s.customers", EdgeKind::Writes));
    assert!(has_edge(&graph, "s.customers.name", EdgeKind::Reads));
}

#[test]
fn other_dialect_raise_routines_are_resolved_or_reported() {
    let graph = analyze("postgres", "SELECT raise('blocked')", json!([]));
    assert_eq!(state(&graph), AnalysisState::Partial);
    assert!(graph
        .limitations()
        .iter()
        .any(|note| note.contains("raise")));

    let graph = analyze(
        "postgres",
        "SELECT s.raise('blocked')",
        json!([{"name": "raise", "kind": "function", "language": "sql", "body": "SELECT 1"}]),
    );
    assert_eq!(state(&graph), AnalysisState::Complete);
    assert!(has_edge(&graph, "s.raise", EdgeKind::Calls));
}
