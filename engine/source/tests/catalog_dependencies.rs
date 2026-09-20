use schemagraph_core::EdgeKind;
use schemagraph_source::codec::{document_from_value, document_to_value};
use schemagraph_source::context::comparison_notes;
use schemagraph_source::document::{
    CatalogDependency, CatalogDocument, CatalogObjectRef, CollectionContext, ColumnDoc, ObjectDoc,
    RoutineDoc, SchemaDoc,
};
use schemagraph_source::{graph, ndjson};
use serde_json::{json, Value};

fn object(name: &str, kind: &str, columns: &[&str]) -> ObjectDoc {
    ObjectDoc {
        name: name.into(),
        kind: kind.into(),
        columns: columns
            .iter()
            .enumerate()
            .map(|(index, name)| ColumnDoc {
                name: (*name).into(),
                data_type: "INTEGER".into(),
                nullable: true,
                default: None,
                ordinal: index as u32 + 1,
                pk_position: 0,
            })
            .collect(),
        constraints: vec![],
        indexes: vec![],
        triggers: vec![],
        body: None,
        usage: None,
    }
}

fn reference(schema: &str, name: &str, kind: Option<&str>) -> CatalogObjectRef {
    CatalogObjectRef {
        schema: schema.into(),
        name: name.into(),
        kind: kind.map(str::to_owned),
        member: None,
        signature: None,
        database: None,
    }
}

fn dependency(source: CatalogObjectRef, target: CatalogObjectRef) -> CatalogDependency {
    CatalogDependency {
        source,
        target,
        catalog: "fixture_catalog".into(),
        dependency_type: "REFERENCES".into(),
    }
}

fn document() -> CatalogDocument {
    CatalogDocument {
        version: 1,
        dialect: "postgres".into(),
        reader: "fixture-reader".into(),
        schemas: vec![SchemaDoc {
            name: "app".into(),
            objects: vec![
                object("consumer", "table", &[]),
                object("thing", "table", &["id"]),
            ],
            routines: vec![
                RoutineDoc {
                    name: "f".into(),
                    kind: "function".into(),
                    language: Some("sql".into()),
                    body: None,
                    signature: Some("integer".into()),
                    usage: None,
                    member_of: None,
                    source: None,
                },
                RoutineDoc {
                    name: "f".into(),
                    kind: "function".into(),
                    language: Some("sql".into()),
                    body: None,
                    signature: Some("text".into()),
                    usage: None,
                    member_of: None,
                    source: None,
                },
                RoutineDoc {
                    name: "f".into(),
                    kind: "function".into(),
                    language: Some("sql".into()),
                    body: None,
                    signature: None,
                    usage: None,
                    member_of: None,
                    source: None,
                },
                RoutineDoc {
                    name: "f".into(),
                    kind: "procedure".into(),
                    language: Some("sql".into()),
                    body: None,
                    signature: None,
                    usage: None,
                    member_of: None,
                    source: None,
                },
            ],
        }],
        limitations: vec![],
        context: Some(CollectionContext {
            source_id: "app-prod".into(),
            database: Some("local-db".into()),
            schema_filter: Some(vec!["app".into(), "reporting".into()]),
            catalog_complete: true,
        }),
        dependencies: vec![],
    }
}

#[test]
fn catalog_object_reference_never_substitutes_a_same_named_package_member() {
    let mut doc = document();
    doc.schemas[0].routines = vec![
        serde_json::from_value(json!({"name":"pkg","kind":"package"})).unwrap(),
        serde_json::from_value(json!({"name":"target","kind":"function","member_of":"pkg"}))
            .unwrap(),
    ];
    let target = reference("app", "target", Some("function"));
    let package_only = graph::document_to_graph(&doc);
    assert!(
        schemagraph_source::dependencies::resolve_reference(&package_only, &doc, &target).is_none()
    );
    doc.schemas[0]
        .routines
        .push(serde_json::from_value(json!({"name":"target","kind":"function"})).unwrap());
    let with_global = graph::document_to_graph(&doc);
    let resolved =
        schemagraph_source::dependencies::resolve_reference(&with_global, &doc, &target).unwrap();
    assert_eq!(resolved.as_str(), "app.target");
}

fn dependency_document() -> CatalogDocument {
    let mut doc = document();
    let source = reference("app", "consumer", Some("table"));
    let mut integer_function = reference("app", "f", Some("function"));
    integer_function.signature = Some("integer".into());
    let mut text_function = reference("app", "f", Some("function"));
    text_function.signature = Some("text".into());
    let procedure = reference("app", "f", Some("procedure"));
    let mut column = reference("app", "thing", Some("table"));
    column.member = Some("id".into());
    let mut external = reference("app", "thing", Some("table"));
    external.database = Some("other-db".into());
    doc.dependencies = vec![
        dependency(source.clone(), text_function),
        dependency(source.clone(), external),
        dependency(source.clone(), column),
        dependency(source.clone(), procedure),
        dependency(source.clone(), integer_function),
    ];
    doc
}

fn edge_exists(g: &schemagraph_core::Graph, from: &str, to: &str, kind: EdgeKind) -> bool {
    g.edges()
        .iter()
        .any(|edge| edge.from.as_str() == from && edge.to.as_str() == to && edge.kind == kind)
}

#[test]
fn json_and_ndjson_v1_v2_round_trip_context_and_dependencies() {
    let mut doc = dependency_document();
    // 입력 순서와 중복은 reader마다 달라도 wire 결과는 정렬·중복 제거되어야 한다.
    let duplicate = doc.dependencies[0].clone();
    doc.dependencies.reverse();
    doc.dependencies.push(duplicate);

    let v1 = document_to_value(&doc, 1).unwrap();
    let v1_decoded = document_from_value(v1.clone()).unwrap();
    assert_eq!(v1_decoded.dependencies.len(), 5);
    assert_eq!(v1_decoded.dependencies, {
        let mut expected = doc.dependencies.clone();
        expected.sort();
        expected.dedup();
        expected
    });
    assert_eq!(v1_decoded.context, doc.context);

    let v2 = document_to_value(&doc, 2).unwrap();
    assert_eq!(v2["producer"]["name"], "fixture-reader");
    assert_eq!(v2["required_features"], json!(["catalog-dependencies-v1"]));
    assert!(v2.get("reader").is_none());
    assert_eq!(document_from_value(v2.clone()).unwrap(), v1_decoded);

    for version in [1, 2] {
        let encoded = ndjson::document_to_ndjson_version(&doc, version).unwrap();
        let decoded = ndjson::document_from_ndjson(&encoded).unwrap();
        assert_eq!(
            decoded, v1_decoded,
            "NDJSON v{version} changed the document"
        );
    }
}

#[test]
fn codec_reports_unknown_fields_and_missing_dependency_feature() {
    let doc = dependency_document();
    let mut value = document_to_value(&doc, 1).unwrap();
    value["dependencies"][0]["target"]["future_identity"] = json!(true);
    let decoded = document_from_value(value).unwrap();
    assert!(decoded
        .limitations
        .iter()
        .any(|note| note.contains("future_identity")));

    let mut missing = document_to_value(&doc, 2).unwrap();
    missing["required_features"] = json!([]);
    let error = document_from_value(missing).unwrap_err();
    assert!(error.contains("catalog-dependencies-v1"), "{error}");
}

#[test]
fn ndjson_context_trailer_requires_matching_identity_and_filter() {
    let doc = dependency_document();
    let encoded = ndjson::document_to_ndjson_version(&doc, 2).unwrap();
    let mut lines: Vec<Value> = encoded
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();

    let header_context = lines[0]["context"].clone();
    lines.last_mut().unwrap()["context"] = header_context;
    lines.last_mut().unwrap()["context"]["catalog_complete"] = json!(false);
    let accepted = lines
        .iter()
        .map(|line| serde_json::to_string(line).unwrap())
        .collect::<Vec<_>>()
        .join("\n");
    let decoded = ndjson::document_from_ndjson(&format!("{accepted}\n")).unwrap();
    assert_eq!(decoded.context.unwrap().catalog_complete, false);

    for (field, value) in [
        ("source_id", json!("other-app")),
        ("schema_filter", json!(["other"])),
    ] {
        let mut mismatched: Vec<Value> = encoded
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        let header_context = mismatched[0]["context"].clone();
        mismatched.last_mut().unwrap()["context"] = header_context;
        mismatched.last_mut().unwrap()["context"][field] = value;
        let text = mismatched
            .iter()
            .map(|line| serde_json::to_string(line).unwrap())
            .collect::<Vec<_>>()
            .join("\n");
        let error = ndjson::document_from_ndjson(&format!("{text}\n")).unwrap_err();
        assert!(error.contains("context"), "{field}: {error}");
    }

    let v1 = ndjson::document_to_ndjson_version(&doc, 1).unwrap();
    let without_trailer = v1
        .lines()
        .take(v1.lines().count() - 1)
        .collect::<Vec<_>>()
        .join("\n");
    let decoded = ndjson::document_from_ndjson(&format!("{without_trailer}\n")).unwrap();
    let context = decoded.context.unwrap();
    assert!(!context.catalog_complete);
    assert!(decoded
        .limitations
        .iter()
        .any(|note| note.contains("legacy NDJSON")));
}

#[test]
fn dependency_resolution_keeps_kind_signature_columns_and_external_targets_distinct() {
    let doc = dependency_document();
    let graph = graph::document_to_graph(&doc);
    let source = "app.consumer";
    assert!(edge_exists(
        &graph,
        source,
        "app.f(integer)",
        EdgeKind::DependsOn
    ));
    assert!(edge_exists(
        &graph,
        source,
        "app.f(text)",
        EdgeKind::DependsOn
    ));
    assert!(!edge_exists(&graph, source, "app.f", EdgeKind::DependsOn));
    assert!(edge_exists(
        &graph,
        source,
        "app.f@procedure",
        EdgeKind::DependsOn
    ));
    assert!(edge_exists(
        &graph,
        source,
        "app.thing.id",
        EdgeKind::DependsOn
    ));
    assert!(!edge_exists(
        &graph,
        source,
        "app.thing",
        EdgeKind::DependsOn
    ));
    assert!(graph
        .limitations()
        .iter()
        .any(|note| note.contains("dependency") && note.contains("no target inferred")));

    let depends_on: Vec<_> = graph
        .edges()
        .into_iter()
        .filter(|edge| edge.kind == EdgeKind::DependsOn)
        .collect();
    assert_eq!(
        depends_on.len(),
        4,
        "duplicate or ghost dependency edges: {depends_on:?}"
    );
}

#[test]
fn dependency_order_and_comparison_context_are_deterministic() {
    let mut first = dependency_document();
    first.dependencies.reverse();
    let mut second = dependency_document();
    second.dependencies.sort();
    assert_eq!(
        document_from_value(document_to_value(&first, 1).unwrap()).unwrap(),
        document_from_value(document_to_value(&second, 1).unwrap()).unwrap()
    );

    let mut changed_source = first.clone();
    changed_source.context.as_mut().unwrap().source_id = "other-app".into();
    let notes = comparison_notes(&first, &changed_source);
    assert!(notes
        .iter()
        .any(|note| note.contains("source identities differ")));

    let mut changed_filter = first.clone();
    changed_filter.context.as_mut().unwrap().schema_filter = Some(vec!["app".into()]);
    let notes = comparison_notes(&first, &changed_filter);
    assert!(notes
        .iter()
        .any(|note| note.contains("schema collection filters differ")));

    let mut incomplete = first.clone();
    incomplete.context.as_mut().unwrap().catalog_complete = false;
    let notes = comparison_notes(&first, &incomplete);
    assert!(notes
        .iter()
        .any(|note| note.contains("incomplete catalog collection")));

    let without_context = CatalogDocument {
        context: None,
        ..first
    };
    let notes = comparison_notes(&without_context, &changed_source);
    assert!(notes
        .iter()
        .any(|note| note.contains("collection context is unavailable")));
}

#[test]
fn external_query_records_require_v2_feature_declaration() {
    let mut doc = dependency_document();
    doc.schemas[0].routines.push(RoutineDoc {
        name: "query_1".into(),
        kind: "query".into(),
        language: Some("sql".into()),
        body: Some("SELECT 1".into()),
        signature: None,
        usage: None,
        member_of: None,
        source: Some("queries/report.sql".into()),
    });
    assert!(document_to_value(&doc, 1).is_err());
    let mut v2 = document_to_value(&doc, 2).unwrap();
    assert!(v2["required_features"]
        .as_array()
        .unwrap()
        .iter()
        .any(|feature| feature == "external-queries-v1"));
    v2["required_features"] = json!(["catalog-dependencies-v1"]);
    let error = document_from_value(v2).unwrap_err();
    assert!(error.contains("external-queries-v1"), "{error}");

    let v2 = document_to_value(&doc, 2).unwrap();
    let mut v1_shape = v2;
    v1_shape["version"] = json!(1);
    v1_shape["reader"] = json!("fixture-reader");
    v1_shape.as_object_mut().unwrap().remove("producer");
    v1_shape
        .as_object_mut()
        .unwrap()
        .remove("required_features");
    let error = document_from_value(v1_shape).unwrap_err();
    assert!(error.contains("external query records require"), "{error}");
}
