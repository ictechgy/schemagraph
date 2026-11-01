//! PostgreSQL routine 정의문 머리의 `SET search_path`가 한 부분 이름의 해석 순서를 정하는지
//! 검증한다. pg_get_functiondef는 proconfig를 `SET search_path TO 'app', 'public'` 줄로
//! 싣기 때문에 문서 형식을 바꾸지 않고도 실제 해석 순서를 알 수 있다.

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
            "name": column, "data_type": "integer", "nullable": true,
            "ordinal": i + 1, "pk_position": 0
        })).collect::<Vec<_>>(),
        "constraints": [], "indexes": [], "triggers": []
    })
}

fn routine(name: &str, language: &str, body: &str) -> Value {
    json!({"name": name, "kind": "function", "language": language, "body": body})
}

/// pg_get_functiondef 모양의 정의문. `config`는 `SET ...` 줄(없으면 빈 문자열)이다.
fn definition(name: &str, language: &str, config: &str, body: &str) -> String {
    format!(
        "CREATE OR REPLACE FUNCTION public.{name}()\n RETURNS bigint\n LANGUAGE {language}\n{config}AS $function${body}$function$\n"
    )
}

/// `app`(items, helper)과 `public`(orders, items)에 걸친 문서를 분석한다.
fn analyze(routines: Vec<Value>) -> Graph {
    analyze_with(vec![routine("helper", "sql", "SELECT 1")], routines)
}

/// `app` routine을 따로 정해 같은 두 스키마 문서를 분석한다.
fn analyze_with(app_routines: Vec<Value>, routines: Vec<Value>) -> Graph {
    let document = document_from_value(json!({
        "version": 1, "dialect": "postgres", "reader": "search-path-test", "limitations": [],
        "schemas": [
            {"name": "app", "objects": [table("items", &["id", "order_id"])],
             "routines": app_routines},
            {"name": "public",
             "objects": [table("orders", &["id"]), table("items", &["id", "order_id"])],
             "routines": routines}
        ]
    }))
    .expect("test catalog must decode");
    let mut graph = document_to_graph(&document);
    let (_, notes) = enrich_from_document(&mut graph, &document);
    for note in notes {
        graph.add_limitation(note);
    }
    graph
}

fn signed(name: &str, signature: &str, body: &str) -> Value {
    let mut value = routine(name, "sql", body);
    value["signature"] = json!(signature);
    value
}

fn state(graph: &Graph, owner: &str) -> AnalysisState {
    graph.analysis()[&VertexId::from_raw(owner)].state
}

fn has_edge(graph: &Graph, from: &str, to: &str, kind: EdgeKind) -> bool {
    graph
        .edges()
        .iter()
        .any(|edge| edge.from.as_str() == from && edge.to.as_str() == to && edge.kind == kind)
}

fn notes_for(graph: &Graph, owner: &str) -> Vec<String> {
    graph
        .analysis()
        .get(&VertexId::from_raw(owner))
        .map(|analysis| {
            analysis
                .diagnostics
                .iter()
                .map(|d| format!("{}: {}", d.code, d.message))
                .collect()
        })
        .unwrap_or_default()
}

const APP_FIRST: &str = " SET search_path TO 'app', 'public'\n";

#[test]
fn declared_search_path_resolves_unqualified_names_in_path_order() {
    let sql = definition(
        "count_items",
        "sql",
        APP_FIRST,
        "SELECT count(*) FROM items JOIN orders ON orders.id = items.order_id \
         WHERE helper() = 1 AND EXISTS (SELECT 1 FROM public.items)",
    );
    let plpgsql = definition(
        "copy_items",
        "plpgsql",
        APP_FIRST,
        "\nBEGIN\n  INSERT INTO items (id) SELECT id FROM orders;\nEND;\n",
    );
    let graph = analyze(vec![
        routine("count_items", "sql", &sql),
        routine("copy_items", "plpgsql", &plpgsql),
    ]);
    // 첫 경로 스키마 app의 items가 public.items를 가린다. orders는 public에만 있다.
    assert!(has_edge(
        &graph,
        "public.count_items",
        "app.items",
        EdgeKind::Reads
    ));
    assert!(has_edge(
        &graph,
        "public.count_items",
        "public.orders",
        EdgeKind::Reads
    ));
    assert!(has_edge(
        &graph,
        "public.count_items",
        "app.helper",
        EdgeKind::Calls
    ));
    // 스키마를 적은 이름은 경로와 무관하다.
    assert!(has_edge(
        &graph,
        "public.count_items",
        "public.items",
        EdgeKind::Reads
    ));
    assert!(has_edge(
        &graph,
        "public.copy_items",
        "app.items",
        EdgeKind::Writes
    ));
    assert!(has_edge(
        &graph,
        "public.copy_items",
        "app.items.id",
        EdgeKind::Writes
    ));
    assert!(has_edge(
        &graph,
        "public.copy_items",
        "public.orders",
        EdgeKind::Reads
    ));
    assert!(!has_edge(
        &graph,
        "public.copy_items",
        "public.items",
        EdgeKind::Writes
    ));
    for owner in ["public.count_items", "public.copy_items"] {
        assert_eq!(notes_for(&graph, owner), Vec::<String>::new(), "{owner}");
    }
}

#[test]
fn routines_without_a_declared_search_path_keep_their_own_schema() {
    let sql = definition("count_items", "sql", "", "SELECT count(*) FROM items");
    // 몸체 안의 SET search_path 문자열은 routine 설정이 아니다.
    let plpgsql = definition(
        "copy_items",
        "plpgsql",
        "",
        "\nBEGIN\n  PERFORM 'SET search_path TO app';\n  INSERT INTO items (id) VALUES (1);\nEND;\n",
    );
    let graph = analyze(vec![
        routine("count_items", "sql", &sql),
        routine("copy_items", "plpgsql", &plpgsql),
        // pg_get_functiondef가 아닌 몸체 원문만 실린 문서도 그대로다.
        routine("bare", "sql", "SELECT count(*) FROM items"),
    ]);
    assert!(has_edge(
        &graph,
        "public.count_items",
        "public.items",
        EdgeKind::Reads
    ));
    assert!(has_edge(
        &graph,
        "public.copy_items",
        "public.items",
        EdgeKind::Writes
    ));
    assert!(has_edge(
        &graph,
        "public.bare",
        "public.items",
        EdgeKind::Reads
    ));
    assert!(!graph
        .edges()
        .iter()
        .any(|edge| edge.from.as_str().starts_with("public.")
            && edge.to.as_str().starts_with("app.")));
}

#[test]
fn search_path_names_follow_postgres_quoting_and_skip_special_entries() {
    // 따옴표 없는 이름은 소문자로 접고, 큰따옴표·문자열 이름은 그대로 둔다. pg_catalog·pg_temp는
    // 수집 대상이 아니라 건너뛴다. 어느 경로 스키마에도 없는 이름은 첫 경로 스키마의 누락이다.
    let ddl = "CREATE FUNCTION public.ddl_form() RETURNS bigint LANGUAGE sql \
               SET search_path = pg_catalog, APP, \"public\", pg_temp \
               AS $$SELECT count(*) FROM items, orders, ghosts$$";
    let graph = analyze(vec![routine("ddl_form", "sql", ddl)]);
    assert!(has_edge(
        &graph,
        "public.ddl_form",
        "app.items",
        EdgeKind::Reads
    ));
    assert!(has_edge(
        &graph,
        "public.ddl_form",
        "public.orders",
        EdgeKind::Reads
    ));
    let notes = notes_for(&graph, "public.ddl_form");
    assert!(
        notes.iter().any(|note| note.contains("app.ghosts")),
        "{notes:?}"
    );
    assert!(
        !notes.iter().any(|note| note.contains("public.ghosts")),
        "{notes:?}"
    );
}

#[test]
fn role_dependent_search_path_entry_is_reported_when_a_name_passes_it() {
    let sql = definition(
        "count_items",
        "sql",
        " SET search_path TO '$user', 'app'\n",
        "SELECT count(*) FROM items",
    );
    let graph = analyze(vec![routine("count_items", "sql", &sql)]);
    // $user는 호출 role에 따라 다르다. 나머지 경로로 해석하되, 그 앞을 지난 이름을 남긴다.
    assert!(has_edge(
        &graph,
        "public.count_items",
        "app.items",
        EdgeKind::Reads
    ));
    assert_eq!(
        notes_for(&graph, "public.count_items"),
        [
            "SG_BODY_PARTIAL: routine public.count_items: search_path entries outside the \
          collection ($user) may hold unqualified names items"
        ]
    );
}

#[test]
fn search_path_without_collected_schemas_leaves_unqualified_names_unresolved() {
    // `SET search_path TO ''`·pg_catalog만 있는 경로는 보안 권장 꼴이다. PostgreSQL은 한 부분
    // 사용자 이름을 찾지 못하므로 routine 스키마로 대신 잇지 않는다.
    for config in [
        " SET search_path TO ''\n",
        " SET search_path TO 'pg_catalog', 'pg_temp'\n",
    ] {
        let sql = definition("count_items", "sql", config, "SELECT count(*) FROM items");
        let graph = analyze(vec![routine("count_items", "sql", &sql)]);
        assert!(
            !graph
                .outgoing(&VertexId::from_raw("public.count_items"))
                .iter()
                .any(|edge| edge.kind.is_dependency()),
            "{config}"
        );
        let notes = notes_for(&graph, "public.count_items");
        assert!(
            notes.iter().any(|note| note.contains(
                "search_path names no collected schema; unqualified names items were not resolved"
            )),
            "{config}: {notes:?}"
        );
    }
}

#[test]
fn search_path_schemas_outside_the_collection_are_reported_only_when_they_can_shadow() {
    // audit는 수집하지 않았다. 그곳의 같은 이름이 public을 가릴 수 있으므로 알린다.
    let before = definition(
        "count_before",
        "sql",
        " SET search_path TO 'audit', 'public'\n",
        "SELECT count(*) FROM orders",
    );
    // 이름을 먼저 찾은 스키마 뒤의 수집하지 않은 항목은 해석을 바꾸지 않는다.
    let after = definition(
        "count_after",
        "sql",
        " SET search_path TO 'public', 'audit'\n",
        "SELECT count(*) FROM orders",
    );
    let graph = analyze(vec![
        routine("count_before", "sql", &before),
        routine("count_after", "sql", &after),
    ]);
    for owner in ["public.count_before", "public.count_after"] {
        assert!(
            has_edge(&graph, owner, "public.orders", EdgeKind::Reads),
            "{owner}"
        );
    }
    assert_eq!(
        notes_for(&graph, "public.count_before"),
        [
            "SG_BODY_PARTIAL: routine public.count_before: search_path entries outside the \
          collection (audit) may hold unqualified names orders"
        ]
    );
    assert_eq!(
        notes_for(&graph, "public.count_after"),
        Vec::<String>::new()
    );
    assert_eq!(state(&graph, "public.count_after"), AnalysisState::Complete);
}

#[test]
fn qualified_bodies_stay_complete_whatever_the_search_path_holds() {
    // SECURITY DEFINER 권장 꼴처럼 모든 이름을 스키마로 적은 몸체는 경로의 영향을 받지 않는다.
    let configs = [
        " SET search_path TO ''\n",
        " SET search_path TO '$user', 'public'\n",
        " SET search_path TO 'pg_catalog', 'pg_temp'\n",
        " SET search_path TO 'audit', 'public'\n",
    ];
    let routines: Vec<Value> = configs
        .iter()
        .enumerate()
        .map(|(index, config)| {
            let name = format!("qualified_{index}");
            routine(
                &name,
                "sql",
                &definition(&name, "sql", config, "SELECT count(*) FROM public.orders"),
            )
        })
        .collect();
    let graph = analyze(routines);
    for index in 0..configs.len() {
        let owner = format!("public.qualified_{index}");
        assert!(
            has_edge(&graph, &owner, "public.orders", EdgeKind::Reads),
            "{owner}"
        );
        assert_eq!(notes_for(&graph, &owner), Vec::<String>::new(), "{owner}");
        assert_eq!(state(&graph, &owner), AnalysisState::Complete, "{owner}");
    }
}

#[test]
fn unqualified_calls_gather_overloads_from_every_search_path_schema() {
    // PostgreSQL은 경로의 모든 스키마에서 같은 이름의 함수를 모으고, 같은 인자 목록만 앞 스키마가
    // 가린다. app.f(i integer)와 public.f(t text)는 서로 가리지 않으므로 f('x')는 하나로 정할 수 없다.
    let cross = definition(
        "cross_call",
        "sql",
        " SET search_path TO 'app', 'public'\n",
        "SELECT f('x')",
    );
    let hidden = definition(
        "hidden_call",
        "sql",
        " SET search_path TO 'app', 'public'\n",
        "SELECT g(1)",
    );
    let graph = analyze_with(
        vec![
            signed("f", "i integer", "SELECT 1"),
            signed("g", "integer", "SELECT 1"),
        ],
        vec![
            routine("cross_call", "sql", &cross),
            routine("hidden_call", "sql", &hidden),
            signed("f", "t text", "SELECT 2"),
            signed("g", "integer", "SELECT 2"),
        ],
    );
    assert!(!graph
        .outgoing(&VertexId::from_raw("public.cross_call"))
        .iter()
        .any(|edge| edge.kind == EdgeKind::Calls));
    let candidates: Vec<String> = graph.analysis()[&VertexId::from_raw("public.cross_call")]
        .diagnostics
        .iter()
        .flat_map(|diagnostic| diagnostic.candidates.iter().map(|id| id.to_string()))
        .collect();
    assert_eq!(candidates, ["app.f(i integer)", "public.f(t text)"]);
    // 같은 인자 목록은 앞 스키마의 것 하나로 정해진다.
    assert!(has_edge(
        &graph,
        "public.hidden_call",
        "app.g(integer)",
        EdgeKind::Calls
    ));
    assert!(!has_edge(
        &graph,
        "public.hidden_call",
        "public.g(integer)",
        EdgeKind::Calls
    ));
}

#[test]
fn search_path_declared_after_the_body_is_read() {
    // 손으로 쓴 정의문은 SET 절을 AS 몸체 뒤에 둘 수 있다.
    let ddl = "CREATE FUNCTION public.after_as() RETURNS bigint \
               AS $$SELECT count(*) FROM items$$ LANGUAGE sql SET search_path = app";
    let graph = analyze(vec![routine("after_as", "sql", ddl)]);
    assert!(has_edge(
        &graph,
        "public.after_as",
        "app.items",
        EdgeKind::Reads
    ));
    assert!(!has_edge(
        &graph,
        "public.after_as",
        "public.items",
        EdgeKind::Reads
    ));
}
