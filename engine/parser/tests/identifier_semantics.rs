use schemagraph_core::{AnalysisState, EdgeKind, Graph, VertexId};
use schemagraph_parser::enrich_from_document;
use schemagraph_source::codec::document_from_value;
use schemagraph_source::graph::document_to_graph;
use serde_json::{json, Value};

fn table(name: &str, columns: &[&str]) -> Value {
    json!({
        "name": name,
        "kind": "table",
        "columns": columns.iter().enumerate().map(|(i, column)| json!({
            "name": column,
            "data_type": "INTEGER",
            "nullable": true,
            "ordinal": i + 1,
            "pk_position": 0
        })).collect::<Vec<_>>(),
        "constraints": [],
        "indexes": [],
        "triggers": []
    })
}

fn view(name: &str, columns: &[&str], body: &str) -> Value {
    let mut value = table(name, columns);
    value["kind"] = json!("view");
    value["body"] = json!(body);
    value
}

fn function(name: &str, signature: Option<&str>, body: &str) -> Value {
    let mut value = json!({
        "name": name,
        "kind": "function",
        "language": "sql",
        "body": body
    });
    if let Some(signature) = signature {
        value["signature"] = json!(signature);
    }
    value
}

fn analyze(dialect: &str, objects: Vec<Value>, routines: Vec<Value>) -> Graph {
    let document = document_from_value(json!({
        "version": 1,
        "dialect": dialect,
        "reader": "identifier-semantics-test",
        "schemas": [{
            "name": "s",
            "objects": objects,
            "routines": routines
        }],
        "limitations": []
    }))
    .expect("test catalog must decode");
    let mut graph = document_to_graph(&document);
    let (_, notes) = enrich_from_document(&mut graph, &document);
    for note in notes {
        graph.add_limitation(note);
    }
    graph
}

fn vertex(name: &str) -> VertexId {
    VertexId::from_raw(name)
}

fn has_edge(graph: &Graph, from: &str, to: &str, kind: EdgeKind) -> bool {
    graph
        .edges()
        .iter()
        .any(|edge| edge.from.as_str() == from && edge.to.as_str() == to && edge.kind == kind)
}

fn has_dependency_to(graph: &Graph, from: &str, to: &str) -> bool {
    graph
        .outgoing(&vertex(from))
        .iter()
        .any(|edge| edge.to.as_str() == to && edge.kind.is_dependency())
}

#[test]
fn feedback_default_expression_keeps_its_function_dependency() {
    let mut orders = table("orders", &["id", "literal"]);
    orders["columns"][0]["default"] = json!("S.Next_Id()");
    orders["columns"][1]["default"] = json!("0");
    let graph = analyze(
        "postgres",
        vec![orders],
        vec![function("next_id", None, "SELECT 1")],
    );
    assert!(has_edge(
        &graph,
        "s.orders.id",
        "s.next_id",
        EdgeKind::DependsOn
    ));
    assert!(!has_dependency_to(&graph, "s.orders.literal", "s.next_id"));
    assert!(!has_edge(
        &graph,
        "s.orders.id",
        "s.next_id",
        EdgeKind::Calls
    ));
}

#[test]
fn feedback_default_expression_does_not_guess_overloads_or_accept_trailing_sql() {
    let mut orders = table("orders", &["ambiguous", "invalid"]);
    orders["columns"][0]["default"] = json!("next_id()");
    orders["columns"][1]["default"] = json!("next_id(); SELECT 1");
    let graph = analyze(
        "postgres",
        vec![orders],
        vec![
            function("next_id", Some("integer"), "SELECT 1"),
            function("next_id", Some("text"), "SELECT 1"),
        ],
    );
    assert!(!graph
        .edges()
        .iter()
        .any(|edge| edge.kind == EdgeKind::DependsOn));
    assert!(graph
        .limitations()
        .iter()
        .any(|note| note.contains("DEFAULT") && note.contains("overload")));
    assert!(graph
        .limitations()
        .iter()
        .any(|note| note.contains("DEFAULT") && note.contains("parse failed")));
}

#[test]
fn feedback_cte_scope_keeps_physical_reads_and_outer_reads_of_written_relations() {
    let graph = analyze(
        "postgres",
        vec![table("audit", &["id"])],
        vec![
            function(
                "same_name",
                None,
                "WITH audit AS (SELECT id FROM audit) SELECT * FROM audit",
            ),
            function(
                "outer_read",
                None,
                "WITH gone AS (DELETE FROM audit RETURNING id) SELECT id FROM audit",
            ),
        ],
    );
    assert!(has_edge(&graph, "s.same_name", "s.audit", EdgeKind::Reads));
    assert!(has_edge(&graph, "s.outer_read", "s.audit", EdgeKind::Reads));
    assert!(has_edge(
        &graph,
        "s.outer_read",
        "s.audit",
        EdgeKind::Writes
    ));
    assert!(!graph.limitations().iter().any(|note| note.contains("gone")));
}

fn state(graph: &Graph, id: &str) -> AnalysisState {
    graph
        .analysis()
        .get(&vertex(id))
        .unwrap_or_else(|| {
            panic!(
                "missing analysis for {id}; vertices: {:?}",
                graph.vertices().map(|v| v.id.as_str()).collect::<Vec<_>>()
            )
        })
        .state
}

fn diagnostics(graph: &Graph, id: &str) -> Vec<String> {
    graph.analysis()[&vertex(id)]
        .diagnostics
        .iter()
        .map(|diagnostic| diagnostic.code.clone())
        .collect()
}

#[test]
fn feedback_postgres_body_folds_only_unquoted_relation_names() {
    let graph = analyze(
        "postgres",
        vec![table("orders", &["id"]), table("audit", &["id"])],
        vec![
            function("read_orders", None, "SELECT count(*) FROM S.Orders"),
            function("archive", None, "INSERT INTO Audit SELECT id FROM ORDERS"),
            function("quoted", None, "SELECT count(*) FROM \"Orders\""),
        ],
    );
    assert!(has_edge(
        &graph,
        "s.read_orders",
        "s.orders",
        EdgeKind::Reads
    ));
    assert!(has_edge(&graph, "s.archive", "s.orders", EdgeKind::Reads));
    assert!(has_edge(&graph, "s.archive", "s.audit", EdgeKind::Writes));
    assert!(!has_edge(&graph, "s.quoted", "s.orders", EdgeKind::Reads));
}

#[test]
fn feedback_truncate_is_a_write_without_an_invented_read() {
    let graph = analyze(
        "postgres",
        vec![table("audit", &["id"]), table("orders", &["id"])],
        vec![function("cleanup", None, "TRUNCATE audit, orders")],
    );
    for target in ["s.audit", "s.orders"] {
        assert!(has_edge(&graph, "s.cleanup", target, EdgeKind::Writes));
        assert!(!has_edge(&graph, "s.cleanup", target, EdgeKind::Reads));
    }
}

#[test]
fn feedback_data_modifying_cte_keeps_write_and_hides_only_its_alias() {
    let graph = analyze(
        "postgres",
        vec![table("audit", &["id"])],
        vec![function(
            "cleanup",
            None,
            "WITH x AS (DELETE FROM audit RETURNING id) SELECT count(*) FROM x",
        )],
    );
    assert!(has_edge(&graph, "s.cleanup", "s.audit", EdgeKind::Writes));
    assert!(!has_edge(&graph, "s.cleanup", "s.audit", EdgeKind::Reads));
    assert!(!graph.limitations().iter().any(|text| text.contains("s.x")));
}

#[test]
fn mysql_column_lookup_is_case_insensitive_for_quoted_and_unquoted_names() {
    let graph = analyze(
        "mysql",
        vec![
            table("orders", &["AMOUNT"]),
            view("plain", &["value"], "SELECT amount FROM orders"),
            view("quoted", &["value"], "SELECT `AMOUNT` FROM orders"),
            view(
                "ordered",
                &["value"],
                "SELECT amount AS TOTAL FROM orders ORDER BY total",
            ),
        ],
        vec![],
    );

    for view_name in ["plain", "quoted", "ordered"] {
        let output = format!("s.{view_name}.value");
        assert_eq!(
            state(&graph, &format!("s.{view_name}")),
            AnalysisState::Complete
        );
        assert!(has_edge(
            &graph,
            &output,
            "s.orders.AMOUNT",
            EdgeKind::DerivesFrom
        ));
        assert!(!has_edge(
            &graph,
            &output,
            "s.orders.amount",
            EdgeKind::DerivesFrom
        ));
    }
}

#[test]
fn postgres_distinct_on_uses_the_output_alias_before_input_columns() {
    let graph = analyze(
        "postgres",
        vec![
            table("orders", &["id", "amount"]),
            view(
                "v",
                &["id"],
                "SELECT DISTINCT ON (id) amount AS id FROM orders ORDER BY id",
            ),
        ],
        vec![],
    );

    assert_eq!(state(&graph, "s.v"), AnalysisState::Complete);
    assert!(has_edge(
        &graph,
        "s.v.id",
        "s.orders.amount",
        EdgeKind::DerivesFrom
    ));
    assert!(!has_dependency_to(&graph, "s.v", "s.orders.id"));
    assert!(!has_dependency_to(&graph, "s.v.id", "s.orders.id"));
}

#[test]
fn overloaded_call_does_not_guess_a_zero_argument_routine() {
    let graph = analyze(
        "postgres",
        vec![
            table("orders", &["id"]),
            view("v", &["value"], "SELECT foo(id) FROM orders"),
        ],
        vec![
            function("foo", None, "SELECT 1"),
            function("foo", Some("integer"), "SELECT 1"),
        ],
    );

    assert!(!has_edge(&graph, "s.v", "s.foo", EdgeKind::Calls));
    let one_argument = has_edge(&graph, "s.v", "s.foo(integer)", EdgeKind::Calls);
    if !one_argument {
        assert_eq!(state(&graph, "s.v"), AnalysisState::Partial);
        let codes = diagnostics(&graph, "s.v");
        assert!(
            codes.iter().any(|code| code == "SG_CALL_AMBIGUOUS")
                || codes.iter().any(|code| code == "SG_CALL_UNRESOLVED"),
            "expected a conservative call diagnostic, got {codes:?}"
        );
    }
}

#[test]
fn ambiguous_same_arity_overloads_report_partial_without_a_call_edge() {
    let graph = analyze(
        "postgres",
        vec![
            table("orders", &["id"]),
            view("v", &["value"], "SELECT foo(id) FROM orders"),
        ],
        vec![
            function("foo", Some("integer"), "SELECT 1"),
            function("foo", Some("text"), "SELECT 1"),
        ],
    );

    assert_eq!(
        state(&graph, "s.v"),
        AnalysisState::Partial,
        "diagnostics: {:?}, edges: {:?}",
        diagnostics(&graph, "s.v"),
        graph
            .outgoing(&vertex("s.v"))
            .iter()
            .map(|edge| (edge.to.as_str(), edge.kind))
            .collect::<Vec<_>>()
    );
    assert!(!has_dependency_to(&graph, "s.v", "s.foo(integer)"));
    assert!(!has_dependency_to(&graph, "s.v", "s.foo(text)"));
    assert!(diagnostics(&graph, "s.v")
        .iter()
        .any(|code| code == "SG_CALL_AMBIGUOUS"));
}

#[test]
fn quoted_postgres_and_oracle_identifiers_remain_case_distinct() {
    let postgres = analyze(
        "postgres",
        vec![
            table("Orders", &["Amount"]),
            table("orders", &["amount"]),
            view("v", &["value"], "SELECT \"Amount\" FROM \"Orders\""),
        ],
        vec![],
    );
    assert!(has_edge(
        &postgres,
        "s.v.value",
        "s.Orders.Amount",
        EdgeKind::DerivesFrom
    ));
    assert!(!has_edge(
        &postgres,
        "s.v.value",
        "s.orders.amount",
        EdgeKind::DerivesFrom
    ));

    let oracle = analyze(
        "oracle",
        vec![
            table("ORDERS", &["AMOUNT"]),
            table("orders", &["amount"]),
            view("v", &["value"], "SELECT \"amount\" FROM \"orders\""),
        ],
        vec![],
    );
    assert!(has_edge(
        &oracle,
        "s.v.value",
        "s.orders.amount",
        EdgeKind::DerivesFrom
    ));
    assert!(!has_edge(
        &oracle,
        "s.v.value",
        "s.ORDERS.AMOUNT",
        EdgeKind::DerivesFrom
    ));
}

#[test]
fn user_defined_builtin_name_is_called_instead_of_silently_ignored() {
    let graph = analyze(
        "postgres",
        vec![
            table("orders", &["id"]),
            view("v", &["value"], "SELECT s.format(id) FROM orders"),
        ],
        vec![function("format", None, "SELECT 1")],
    );

    assert_eq!(state(&graph, "s.v"), AnalysisState::Complete);
    assert!(has_edge(&graph, "s.v", "s.format", EdgeKind::Calls));
    assert!(!diagnostics(&graph, "s.v")
        .iter()
        .any(|code| code == "SG_CALL_UNRESOLVED"));
}

#[test]
fn quoted_postgres_builtin_name_is_unresolved_instead_of_folded_to_a_builtin() {
    let graph = analyze(
        "postgres",
        vec![
            table("orders", &["id"]),
            view("v", &["value"], "SELECT \"SUM\"(id) FROM orders"),
        ],
        vec![],
    );

    assert_eq!(
        state(&graph, "s.v"),
        AnalysisState::Partial,
        "diagnostics: {:?}, edges: {:?}",
        diagnostics(&graph, "s.v"),
        graph
            .outgoing(&vertex("s.v"))
            .iter()
            .map(|edge| (edge.to.as_str(), edge.kind))
            .collect::<Vec<_>>()
    );
    assert!(!graph
        .outgoing(&vertex("s.v"))
        .iter()
        .any(|edge| edge.kind == EdgeKind::Calls));
    assert!(diagnostics(&graph, "s.v")
        .iter()
        .any(|code| code == "SG_CALL_UNRESOLVED"));
}

#[test]
fn quoted_dotted_names_and_literal_routine_names_resolve_to_escaped_ids() {
    let graph = analyze(
        "postgres",
        vec![
            table("t.u", &["c.d", "id"]),
            view("v", &["value"], "SELECT \"c.d\" FROM \"t.u\""),
            view(
                "call",
                &["value"],
                "SELECT \"fn(integer)\"(id) FROM \"t.u\"",
            ),
        ],
        vec![
            function("fn(integer)", None, "SELECT 1"),
            function("fn", Some("integer"), "SELECT 1"),
        ],
    );
    let dotted_column = VertexId::member("s", "t.u", "c.d");
    assert!(graph.vertex(&VertexId::object("s", "t.u")).is_some());
    assert!(graph.vertex(&dotted_column).is_some());
    assert!(has_edge(
        &graph,
        "s.v.value",
        dotted_column.as_str(),
        EdgeKind::DerivesFrom
    ));

    let literal_name = VertexId::routine("s", None, "fn(integer)", None);
    let typed_name = VertexId::routine("s", None, "fn", Some("integer"));
    assert_ne!(literal_name, typed_name);
    assert_eq!(
        graph.vertex(&literal_name).map(|v| v.name.as_str()),
        Some("fn(integer)")
    );
    assert_eq!(
        graph.vertex(&typed_name).map(|v| v.name.as_str()),
        Some("fn")
    );
    assert!(has_edge(
        &graph,
        "s.call",
        literal_name.as_str(),
        EdgeKind::Calls
    ));
}
