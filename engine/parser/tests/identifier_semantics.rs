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
    analyze_document(json!({
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
}

/// `analyze`와 같은 카탈로그에 수집 database를 싣는다. 3부분 이름(`db.schema.table`)은
/// 그 database와 같을 때만 로컬 관계로 해석된다.
fn analyze_in_database(
    dialect: &str,
    database: &str,
    objects: Vec<Value>,
    routines: Vec<Value>,
) -> Graph {
    analyze_document(json!({
        "version": 1,
        "dialect": dialect,
        "reader": "identifier-semantics-test",
        "context": {"source_id": "t", "database": database, "catalog_complete": true},
        "schemas": [{
            "name": "s",
            "objects": objects,
            "routines": routines
        }],
        "limitations": []
    }))
}

/// 문서 JSON을 그래프로 만들고 몸체를 분석해 note를 그래프 한계로 싣는다.
fn analyze_document(document: Value) -> Graph {
    let document = document_from_value(document).expect("test catalog must decode");
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
fn review_recursive_forward_cte_alias_is_not_a_physical_relation() {
    for (sql, physical_later) in [
        ("WITH RECURSIVE first_cte AS (SELECT id FROM later_cte), later_cte AS (SELECT id FROM audit) SELECT id FROM first_cte", false),
        ("WITH first_cte AS (SELECT id FROM later_cte), later_cte AS (SELECT id FROM audit) SELECT id FROM first_cte", true),
    ] {
        let graph = analyze("postgres", vec![table("later_cte", &["id"]), table("audit", &["id"])], vec![function("read_ctes", None, sql)]);
        assert_eq!(has_edge(&graph, "s.read_ctes", "s.later_cte", EdgeKind::Reads), physical_later);
        assert!(has_edge(&graph, "s.read_ctes", "s.audit", EdgeKind::Reads));
    }
    let graph = analyze("postgres", vec![table("audit", &["id"])], vec![function("read_ctes", None,
        "WITH RECURSIVE first_cte AS (SELECT id FROM later_cte), later_cte AS (SELECT id FROM audit) SELECT id FROM first_cte")]);
    assert!(!graph
        .limitations()
        .iter()
        .any(|note| note.contains("later_cte")));
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
fn review_postgres_sequence_builtins_are_not_missing_user_functions() {
    for dialect in ["postgres", "postgresql"] {
        for expression in [
            "nextval('s.order_seq'::regclass)",
            "PG_CATALOG.NEXTVAL('s.order_seq'::regclass)",
            "\"pg_catalog\".\"nextval\"('s.order_seq'::regclass)",
            "currval('s.order_seq'::regclass)",
            "setval('s.order_seq'::regclass, 42)",
            "pg_catalog.setval('s.order_seq'::regclass, 42, false)",
            "lastval()",
        ] {
            let mut orders = table("orders", &["id"]);
            orders["columns"][0]["default"] = json!(expression);
            let graph = analyze(dialect, vec![orders], vec![]);
            assert!(
                !graph
                    .limitations()
                    .iter()
                    .any(|note| note.contains("DEFAULT")),
                "{dialect}: {expression}: {:?}",
                graph.limitations()
            );
        }
    }
}

#[test]
fn review_sequence_builtin_names_keep_user_functions_and_unknown_names() {
    for expression in ["nextval()", "s.nextval()"] {
        let mut orders = table("orders", &["id"]);
        orders["columns"][0]["default"] = json!(expression);
        let graph = analyze(
            "postgres",
            vec![orders],
            vec![function("nextval", None, "SELECT 1")],
        );
        assert!(has_edge(
            &graph,
            "s.orders.id",
            "s.nextval",
            EdgeKind::DependsOn
        ));
    }
    for (dialect, expression) in [
        ("postgres", "s.nextval()"),
        ("postgres", "\"PG_CATALOG\".nextval()"),
        ("postgres", "\"NextVal\"()"),
        ("postgres", "pg_catalog.not_a_builtin()"),
        ("sqlite", "nextval()"),
    ] {
        let mut orders = table("orders", &["id"]);
        orders["columns"][0]["default"] = json!(expression);
        let graph = analyze(dialect, vec![orders], vec![]);
        assert!(
            graph
                .limitations()
                .iter()
                .any(|note| note.contains("DEFAULT") && note.contains("unavailable")),
            "{dialect}: {expression}: {:?}",
            graph.limitations()
        );
    }
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

/// 한 routine이 남긴 진단 문구들이다. 코드만 보는 `diagnostics`로는 어느 이름이 풀리지
/// 않았는지 가를 수 없어 문구 전체를 본다.
fn diagnostic_messages(graph: &Graph, id: &str) -> Vec<String> {
    graph.analysis()[&vertex(id)]
        .diagnostics
        .iter()
        .map(|diagnostic| format!("{}: {}", diagnostic.code, diagnostic.message))
        .collect()
}

/// UTF8 PostgreSQL은 따옴표 없는 식별자에서 ASCII `A`–`Z`만 접는다(`scansup.c`
/// `downcase_identifier`). PostgreSQL 16.15에서 `CREATE TABLE ΑΣ`는 `ΑΣ`로 저장되고,
/// `UPDATE ΑΣ`는 같은 스키마의 `ας`를 두고 `ΑΣ`를 바꿨다. Unicode 소문자로 접으면
/// `ΑΣ`가 어말 시그마 규칙으로 `ας`가 되어, `ας`가 없으면 누락, 있으면 거짓 쓰기였다.
#[test]
fn postgres_unquoted_greek_names_fold_only_ascii_letters() {
    let body = "UPDATE ΑΣ SET name = 'x'";
    let alone = analyze(
        "postgres",
        vec![table("ΑΣ", &["id", "name"])],
        vec![function("r", None, body)],
    );
    let detail = format!("{:?} {:?}", alone.edges(), alone.limitations());
    assert!(
        has_edge(&alone, "s.r", "s.ΑΣ", EdgeKind::Writes),
        "{detail}"
    );
    assert!(
        has_edge(&alone, "s.r", "s.ΑΣ.name", EdgeKind::Writes),
        "{detail}"
    );
    assert!(alone.limitations().is_empty(), "{detail}");
    assert_eq!(state(&alone, "s.r"), AnalysisState::Complete, "{detail}");

    let both = analyze(
        "postgres",
        vec![table("ΑΣ", &["id", "name"]), table("ας", &["id", "name"])],
        vec![function("r", None, body)],
    );
    let detail = format!("{:?}", both.edges());
    assert!(has_edge(&both, "s.r", "s.ΑΣ", EdgeKind::Writes), "{detail}");
    assert!(
        has_edge(&both, "s.r", "s.ΑΣ.name", EdgeKind::Writes),
        "{detail}"
    );
    assert!(!has_dependency_to(&both, "s.r", "s.ας"), "{detail}");
    assert!(!has_dependency_to(&both, "s.r", "s.ας.name"), "{detail}");
}

/// 악센트 글자도 같다. PostgreSQL 16.15에서 `CREATE TABLE Été (id int, Nom text)`는 테이블
/// `Été`·컬럼 `nom`으로 저장된다(ASCII `N`만 접힘). `UPDATE Été SET Nom = …`는 그 테이블에
/// 쓰고, `UPDATE ÉTÉ …`는 `relation "ÉtÉ" does not exist`로 거절된다.
#[test]
fn postgres_unquoted_accented_names_keep_non_ascii_letters() {
    let graph = analyze(
        "postgres",
        vec![
            table("Été", &["id", "nom"]),
            view("v", &["value"], "SELECT Nom FROM Été"),
        ],
        vec![
            function("touch", None, "UPDATE Été SET Nom = 'x'"),
            function("shout", None, "UPDATE ÉTÉ SET nom = 'x'"),
        ],
    );
    let detail = format!("{:?} {:?}", graph.edges(), graph.limitations());
    assert!(
        has_edge(&graph, "s.touch", "s.Été", EdgeKind::Writes),
        "{detail}"
    );
    assert!(
        has_edge(&graph, "s.touch", "s.Été.nom", EdgeKind::Writes),
        "{detail}"
    );
    assert_eq!(
        state(&graph, "s.touch"),
        AnalysisState::Complete,
        "{detail}"
    );
    assert!(
        has_edge(&graph, "s.v.value", "s.Été.nom", EdgeKind::DerivesFrom),
        "{detail}"
    );
    assert_eq!(state(&graph, "s.v"), AnalysisState::Complete, "{detail}");
    assert!(!has_dependency_to(&graph, "s.shout", "s.Été"), "{detail}");
    assert!(
        graph
            .limitations()
            .iter()
            .any(|note| note == "s.shout references s.ÉtÉ, which is missing from the catalog"),
        "{detail}"
    );
}

/// 컬럼 이름도 같은 규칙이다. PostgreSQL 16.15에서 `ΌΝΟΜΑ` 컬럼은 그대로 저장되고 `όνομα`는
/// `column "όνομα" does not exist`다. `Prénom`으로 만든 컬럼은 `prénom`이고, `PRÉNOM`은
/// `prÉnom`이라 `column "prÉnom" does not exist`, `PRéNOM`은 그 컬럼이다.
#[test]
fn postgres_unquoted_non_ascii_column_names_fold_only_ascii_letters() {
    let graph = analyze(
        "postgres",
        vec![
            table("people", &["id", "ΌΝΟΜΑ", "prénom"]),
            view("v_greek", &["n"], "SELECT ΌΝΟΜΑ AS n FROM people"),
            view("v_upper", &["p"], "SELECT PRÉNOM AS p FROM people"),
            view("v_ascii", &["p"], "SELECT PRéNOM AS p FROM people"),
        ],
        vec![function("rename", None, "UPDATE people SET ΌΝΟΜΑ = 'x'")],
    );
    let detail = format!("{:?} {:?}", graph.edges(), graph.limitations());
    assert!(
        has_edge(
            &graph,
            "s.v_greek.n",
            "s.people.ΌΝΟΜΑ",
            EdgeKind::DerivesFrom
        ),
        "{detail}"
    );
    assert_eq!(
        state(&graph, "s.v_greek"),
        AnalysisState::Complete,
        "{detail}"
    );
    assert!(
        has_edge(&graph, "s.rename", "s.people.ΌΝΟΜΑ", EdgeKind::Writes),
        "{detail}"
    );
    assert_eq!(
        state(&graph, "s.rename"),
        AnalysisState::Complete,
        "{detail}"
    );
    assert!(
        has_edge(
            &graph,
            "s.v_ascii.p",
            "s.people.prénom",
            EdgeKind::DerivesFrom
        ),
        "{detail}"
    );
    assert!(
        !has_dependency_to(&graph, "s.v_upper", "s.people.prénom"),
        "{detail}"
    );
    assert!(
        !has_edge(
            &graph,
            "s.v_upper.p",
            "s.people.prénom",
            EdgeKind::DerivesFrom
        ),
        "{detail}"
    );
    let upper = diagnostic_messages(&graph, "s.v_upper");
    assert!(
        upper
            .iter()
            .any(|message| message.starts_with("SG_COLUMN_UNRESOLVED")
                && message.contains("'prÉnom'")),
        "{upper:?}"
    );
}

/// routine 호출 이름도 같은 규칙이다. PostgreSQL 16.15에서 `CREATE FUNCTION Écho(int)`는
/// `Écho`로 저장되고, `ÉCHO(1)`은 그 함수를 부르며(`É`는 그대로, `CHO`만 접힘) `écho(1)`은
/// `function écho(integer) does not exist`다.
#[test]
fn postgres_unquoted_routine_calls_fold_only_ascii_letters() {
    let graph = analyze(
        "postgres",
        vec![
            table("people", &["id"]),
            view("v_upper", &["e"], "SELECT ÉCHO(id) AS e FROM people"),
            view("v_lower", &["e"], "SELECT écho(id) AS e FROM people"),
        ],
        vec![
            function("Écho", None, "SELECT 1"),
            function("caller", None, "SELECT ÉCHO(1)"),
        ],
    );
    let detail = format!("{:?} {:?}", graph.edges(), graph.limitations());
    assert!(
        has_edge(&graph, "s.v_upper", "s.Écho", EdgeKind::Calls),
        "{detail}"
    );
    assert!(
        has_edge(&graph, "s.caller", "s.Écho", EdgeKind::Calls),
        "{detail}"
    );
    assert!(
        !has_edge(&graph, "s.v_lower", "s.Écho", EdgeKind::Calls),
        "{detail}"
    );
}

/// 수집 database로 한정한 3부분 이름도 같은 규칙이다. 객체 단위 해석
/// (`local_qualified_relation`)은 이미 접힌 이름을 한 번 더 정규화하므로, 그 규칙만 Unicode로
/// 남으면 컬럼 단위는 `ΑΣ`에 쓰는데 객체 단위는 `ας`를 가리킨다. PostgreSQL 16.15에서
/// 현재 database로 한정한 `UPDATE postgres.s.ΑΣ`는 `ΑΣ`만 바꿨다.
#[test]
fn postgres_three_part_names_fold_only_ascii_letters() {
    let graph = analyze_in_database(
        "postgres",
        "app",
        vec![table("ΑΣ", &["id", "name"]), table("ας", &["id", "name"])],
        vec![function("r", None, "UPDATE APP.s.ΑΣ SET name = 'x'")],
    );
    let detail = format!("{:?} {:?}", graph.edges(), graph.limitations());
    assert!(
        has_edge(&graph, "s.r", "s.ΑΣ", EdgeKind::Writes),
        "{detail}"
    );
    assert!(
        has_edge(&graph, "s.r", "s.ΑΣ.name", EdgeKind::Writes),
        "{detail}"
    );
    assert!(!has_dependency_to(&graph, "s.r", "s.ας"), "{detail}");
    assert!(!has_dependency_to(&graph, "s.r", "s.ας.name"), "{detail}");
    assert!(graph.limitations().is_empty(), "{detail}");
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
    // 확정 간선 없이 진단만 두 오버로드를 후보로 싣는다.
    let candidates: Vec<_> = graph.analysis()[&vertex("s.v")]
        .diagnostics
        .iter()
        .filter(|diagnostic| diagnostic.code == "SG_CALL_AMBIGUOUS")
        .flat_map(|diagnostic| diagnostic.candidates.iter().map(VertexId::as_str))
        .collect();
    assert_eq!(candidates, ["s.foo(integer)", "s.foo(text)"]);
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

#[test]
fn mariadb_and_postgresql_documents_parse_with_their_engine_dialects() {
    // JDBC 프로브는 jdbc:mariadb: URL을 "mariadb"로 싣는다. MySQL 전용 DIV와 PostgreSQL
    // 전용 `#`(비트 XOR)는 일반 방언이 거부하므로, 별칭을 일반 방언으로 보내면 의존이 사라진다.
    for (dialect, body) in [
        ("mysql", "SELECT total DIV 2 AS half FROM orders"),
        ("mariadb", "SELECT total DIV 2 AS half FROM orders"),
        ("postgres", "SELECT total # 1 AS half FROM orders"),
        ("postgresql", "SELECT total # 1 AS half FROM orders"),
    ] {
        let graph = analyze(
            dialect,
            vec![
                table("orders", &["id", "total"]),
                view("v", &["half"], body),
            ],
            vec![],
        );
        assert_eq!(
            state(&graph, "s.v"),
            AnalysisState::Complete,
            "{dialect}: {:?}",
            graph.limitations()
        );
        assert!(
            has_edge(&graph, "s.v", "s.orders", EdgeKind::Reads),
            "{dialect}"
        );
        assert!(
            has_edge(&graph, "s.v.half", "s.orders.total", EdgeKind::DerivesFrom),
            "{dialect}"
        );
    }
}

#[test]
fn unrecognized_dialect_reports_generic_parsing_once_with_the_measured_text_count() {
    let generic_note = |graph: &Graph| -> Vec<String> {
        graph
            .limitations()
            .iter()
            .filter(|note| note.contains("generic SQL parsing"))
            .cloned()
            .collect()
    };
    let mut orders = table("orders", &["id"]);
    orders["columns"][0]["default"] = json!("0");
    let objects = || vec![orders.clone(), view("v", &["id"], "SELECT id FROM orders")];
    let routines = || {
        vec![function(
            "count_orders",
            None,
            "SELECT count(*) FROM orders",
        )]
    };
    // 뷰 몸체, 컬럼 기본값, routine 몸체를 하나씩 센다. 일반 방언으로도 파싱은 계속한다.
    let graph = analyze("snowflake", objects(), routines());
    assert_eq!(
        generic_note(&graph),
        ["dialect 'snowflake' is not recognized; generic SQL parsing used for 3 body/default text(s)"]
    );
    assert!(has_edge(&graph, "s.v", "s.orders", EdgeKind::Reads));
    // 엔진이 알고 일부러 일반 방언으로 읽는 이름과, 읽을 SQL이 없는 문서는 알리지 않는다.
    for known in [
        "oracle", "db2", "informix", "h2", "external", "postgres", "mariadb",
    ] {
        assert!(
            generic_note(&analyze(known, objects(), routines())).is_empty(),
            "{known}"
        );
    }
    assert!(generic_note(&analyze(
        "snowflake",
        vec![table("orders", &["id"])],
        vec![]
    ))
    .is_empty());
}

#[test]
fn sqlserver_case_only_mismatch_is_explained_without_guessing_a_match() {
    // SQL Server의 대소문자 구분은 collation에 달렸고 문서는 collation을 싣지 않는다.
    // 대소문자만 다른 카탈로그 객체를 이어 주지는 않되, 누락 이유를 알린다.
    let hint = |text: &str| text.contains("differs only by letter case");
    let graph = analyze(
        "sqlserver",
        vec![
            table("Orders", &["Id"]),
            view("v", &["Id"], "SELECT Id FROM s.orders"),
        ],
        vec![function("p", None, "SELECT Id FROM orders")],
    );
    assert!(!has_edge(&graph, "s.v", "s.Orders", EdgeKind::Reads));
    assert!(!has_edge(&graph, "s.p", "s.Orders", EdgeKind::Reads));
    let view_notes: Vec<_> = graph.analysis()[&vertex("s.v")]
        .diagnostics
        .iter()
        .filter(|d| d.code == "SG_RELATION_UNRESOLVED")
        .map(|d| d.message.clone())
        .collect();
    assert!(
        view_notes.iter().any(|m| hint(m) && m.contains("s.Orders")),
        "{view_notes:?}"
    );
    assert!(
        graph
            .limitations()
            .iter()
            .any(|note| note.starts_with("s.p references s.orders")
                && hint(note)
                && note.contains("s.Orders")),
        "{:?}",
        graph.limitations()
    );

    // 대소문자만 다른 후보가 둘이면 고르지 않고 수만 알린다.
    let graph = analyze(
        "sqlserver",
        vec![table("Orders", &["Id"]), table("ORDERS", &["Id"])],
        vec![function("p", None, "SELECT Id FROM orders")],
    );
    assert!(graph
        .limitations()
        .iter()
        .any(|note| note.contains("2 catalog objects differ only by letter case")));

    // PostgreSQL의 따옴표 없는 이름은 확실히 접히므로 대소문자 힌트가 필요 없다.
    let graph = analyze(
        "postgres",
        vec![
            table("Orders", &["Id"]),
            view("v", &["Id"], "SELECT 1 AS \"Id\" FROM s.orders"),
        ],
        vec![],
    );
    assert!(!graph.limitations().iter().any(|note| hint(note)));
}
