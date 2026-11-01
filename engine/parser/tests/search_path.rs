//! PostgreSQL routine 정의문 머리의 `SET search_path`가 한 부분 이름의 해석 순서를 정하는지
//! 검증한다. pg_get_functiondef는 proconfig를 `SET search_path TO 'app', 'public'` 줄로
//! 싣기 때문에 문서 형식을 바꾸지 않고도 실제 해석 순서를 알 수 있다.

use schemagraph_core::{EdgeKind, Graph, VertexId};
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
    let document = document_from_value(json!({
        "version": 1, "dialect": "postgres", "reader": "search-path-test", "limitations": [],
        "schemas": [
            {"name": "app", "objects": [table("items", &["id", "order_id"])],
             "routines": [routine("helper", "sql", "SELECT 1")]},
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
fn role_dependent_search_path_entry_is_reported_not_guessed() {
    let sql = definition(
        "count_items",
        "sql",
        " SET search_path TO '$user', 'app'\n",
        "SELECT count(*) FROM items",
    );
    let graph = analyze(vec![routine("count_items", "sql", &sql)]);
    // $user는 호출 role에 따라 다르다. 나머지 경로로 해석하되 그 사실을 남긴다.
    assert!(has_edge(
        &graph,
        "public.count_items",
        "app.items",
        EdgeKind::Reads
    ));
    let notes = notes_for(&graph, "public.count_items");
    assert!(
        notes
            .iter()
            .any(|note| note.contains("search_path entry $user depends on the calling role")),
        "{notes:?}"
    );
}

#[test]
fn search_path_without_collected_schemas_keeps_the_routine_schema_and_says_so() {
    // `SET search_path TO ''`는 보안 권장 꼴이다. 사용자 스키마가 경로에 없으니 PostgreSQL은
    // 한 부분 사용자 이름을 찾지 못한다 — 확실하지 않은 쪽은 기존처럼 살려 두고 이유를 남긴다.
    let sql = definition(
        "count_items",
        "sql",
        " SET search_path TO ''\n",
        "SELECT count(*) FROM items",
    );
    let graph = analyze(vec![routine("count_items", "sql", &sql)]);
    assert!(has_edge(
        &graph,
        "public.count_items",
        "public.items",
        EdgeKind::Reads
    ));
    let notes = notes_for(&graph, "public.count_items");
    assert!(
        notes
            .iter()
            .any(|note| note.contains("search_path names no collected schema")),
        "{notes:?}"
    );
}

#[test]
fn search_path_schemas_outside_the_collection_are_reported() {
    // audit는 수집하지 않았다. 그곳의 같은 이름이 public을 가릴 수 있으므로 알린다.
    let sql = definition(
        "count_items",
        "sql",
        " SET search_path TO 'audit', 'public'\n",
        "SELECT count(*) FROM orders",
    );
    let graph = analyze(vec![routine("count_items", "sql", &sql)]);
    assert!(has_edge(
        &graph,
        "public.count_items",
        "public.orders",
        EdgeKind::Reads
    ));
    let notes = notes_for(&graph, "public.count_items");
    assert!(
        notes
            .iter()
            .any(|note| note.contains("search_path schemas not collected (audit)")),
        "{notes:?}"
    );
}
