use schemagraph_source::codec::{document_from_json, document_from_value, document_to_value};
use schemagraph_source::document::{
    CatalogDocument, ColumnDoc, IndexDoc, ObjectDoc, RoutineDoc, SchemaDoc, UsageDoc,
    DOCUMENT_VERSION,
};
use schemagraph_source::ndjson::{document_from_ndjson, document_to_ndjson_version};
use serde_json::{json, Value};

fn sample_document() -> CatalogDocument {
    CatalogDocument {
        context: None,
        dependencies: Vec::new(),
        version: DOCUMENT_VERSION,
        dialect: "sqlite".into(),
        reader: "fixture-reader".into(),
        schemas: vec![SchemaDoc {
            name: "main".into(),
            objects: vec![ObjectDoc {
                name: "items".into(),
                kind: "table".into(),
                columns: vec![ColumnDoc {
                    name: "id".into(),
                    data_type: "INTEGER".into(),
                    nullable: false,
                    default: None,
                    ordinal: 1,
                    pk_position: 1,
                }],
                constraints: vec![],
                indexes: vec![IndexDoc {
                    has_predicate: None,
                    definition_complete: None,
                    predicate: None,
                    name: "items_idx".into(),
                    unique: true,
                    columns: vec!["id".into()],
                    usage: Some(UsageDoc {
                        since: None,
                        reads: 17,
                        writes: 2,
                        total_ms: None,
                        self_ms: None,
                    }),
                }],
                triggers: vec![],
                body: None,
                usage: None,
            }],
            routines: vec![RoutineDoc {
                source: None,
                name: "member".into(),
                kind: "procedure".into(),
                language: None,
                body: None,
                signature: None,
                usage: Some(UsageDoc {
                    since: None,
                    reads: 3,
                    writes: 1,
                    total_ms: None,
                    self_ms: None,
                }),
                member_of: Some("package".into()),
            }],
        }],
        limitations: vec!["fixture limitation".into()],
    }
}

fn records(text: &str) -> Vec<Value> {
    text.lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

fn encode_records(records: &[Value]) -> String {
    records
        .iter()
        .map(|record| serde_json::to_string(record).unwrap())
        .collect::<Vec<_>>()
        .join("\n")
        + "\n"
}

#[test]
fn v1_and_v2_json_ndjson_normalize_to_the_same_v1_document() {
    let expected = sample_document();
    let v1_json = document_to_value(&expected, 1).unwrap();
    let v2_json = document_to_value(&expected, 2).unwrap();
    assert_eq!(document_from_value(v1_json).unwrap(), expected);
    assert_eq!(document_from_value(v2_json).unwrap(), expected);

    let v1_ndjson = document_to_ndjson_version(&expected, 1).unwrap();
    let v2_ndjson = document_to_ndjson_version(&expected, 2).unwrap();
    assert_eq!(document_from_ndjson(&v1_ndjson).unwrap(), expected);
    assert_eq!(document_from_ndjson(&v2_ndjson).unwrap(), expected);

    assert_eq!(
        document_from_json(
            &serde_json::to_string(&document_to_value(&expected, 2).unwrap()).unwrap()
        )
        .unwrap(),
        expected
    );
}

#[test]
fn v2_declares_nested_features_and_rejects_unknown_required_features() {
    let value = document_to_value(&sample_document(), 2).unwrap();
    assert_eq!(value["producer"]["name"], "fixture-reader");
    assert!(value.get("reader").is_none());
    assert_eq!(
        value["required_features"],
        json!(["package-members-v1", "usage-v1"])
    );
    assert!(document_from_value(value.clone()).is_ok());

    let mut unknown = value;
    unknown["required_features"] = json!(["future-feature"]);
    let error = document_from_value(unknown).unwrap_err();
    assert!(error.contains("unsupported required"), "{error}");

    let mut missing = document_to_value(&sample_document(), 2).unwrap();
    missing["required_features"] = json!([]);
    let error = document_from_value(missing).unwrap_err();
    assert!(error.contains("without declaring"), "{error}");
}

#[test]
fn unknown_json_and_nested_ndjson_fields_are_reported_as_limitations() {
    let mut json_value = document_to_value(&sample_document(), 1).unwrap();
    json_value["schemas"][0]["objects"][0]["future_object_field"] = json!(true);
    let json_doc = document_from_value(json_value).unwrap();
    assert!(json_doc
        .limitations
        .iter()
        .any(|note| note.contains("schemas[].objects[].future_object_field")));

    let mut ndjson_records = records(&document_to_ndjson_version(&sample_document(), 2).unwrap());
    let object = ndjson_records
        .iter_mut()
        .find(|record| record["type"] == "object")
        .unwrap();
    object["data"]["future_nested_field"] = json!("ignored");
    let ndjson_doc = document_from_ndjson(&encode_records(&ndjson_records)).unwrap();
    assert!(
        ndjson_doc
            .limitations
            .iter()
            .any(|note| note.contains("schemas[].objects[].future_nested_field")),
        "{:?}",
        ndjson_doc.limitations
    );
}

#[test]
fn v2_ndjson_requires_one_final_trailer_and_ordered_records() {
    let text = document_to_ndjson_version(&sample_document(), 2).unwrap();
    let all = records(&text);
    assert_eq!(all.last().unwrap()["type"], "limitations");

    let without_trailer = encode_records(&all[..all.len() - 1]);
    assert!(document_from_ndjson(&without_trailer)
        .unwrap_err()
        .contains("trailer is missing"));

    let mut after_trailer = all.clone();
    after_trailer.push(json!({"type":"schema", "name":"later"}));
    assert!(document_from_ndjson(&encode_records(&after_trailer))
        .unwrap_err()
        .contains("limitations trailer"));

    let mut duplicate_header = all.clone();
    duplicate_header.insert(1, duplicate_header[0].clone());
    assert!(document_from_ndjson(&encode_records(&duplicate_header))
        .unwrap_err()
        .contains("more than one document header"));

    let mut mismatch = all.clone();
    let object = mismatch
        .iter_mut()
        .find(|record| record["type"] == "object")
        .unwrap();
    object["schema"] = json!("other");
    assert!(document_from_ndjson(&encode_records(&mismatch))
        .unwrap_err()
        .contains("does not match"));

    let mut header_not_first = all;
    header_not_first.insert(0, json!({"type":"schema", "name":"early"}));
    assert!(document_from_ndjson(&encode_records(&header_not_first))
        .unwrap_err()
        .contains("start with a document header"));
}

#[test]
fn v1_without_trailer_remains_accepted_and_large_version_is_rejected() {
    let legacy = concat!(
        r#"{"type":"document","version":1,"dialect":"sqlite","reader":"legacy","limitations":[]}"#,
        "\n",
        r#"{"type":"schema","name":"main"}"#,
        "\n",
    );
    let document = document_from_ndjson(legacy).unwrap();
    assert_eq!(document.version, DOCUMENT_VERSION);
    assert_eq!(document.reader, "legacy");

    let overflow =
        r#"{"version":4294967297,"dialect":"sqlite","reader":"x","schemas":[],"limitations":[]}"#;
    let error = document_from_json(overflow).unwrap_err();
    assert!(error.contains("unsupported catalog version"), "{error}");
}

#[test]
fn buffered_inputs_preserve_documents_across_tiny_read_boundaries() {
    use std::io::{BufReader, Cursor};
    let document = sample_document();
    for version in [1, 2] {
        let json = serde_json::to_vec(&document_to_value(&document, version).unwrap()).unwrap();
        let reader = BufReader::with_capacity(3, Cursor::new(json));
        assert_eq!(
            schemagraph_source::codec::document_from_reader(reader).unwrap(),
            document
        );
        let ndjson = document_to_ndjson_version(&document, version).unwrap();
        let reader = BufReader::with_capacity(3, Cursor::new(ndjson.as_bytes()));
        assert_eq!(
            schemagraph_source::ndjson::document_from_reader(reader).unwrap(),
            document
        );
    }
}

#[test]
fn stream_io_failures_cannot_look_like_complete_catalogs() {
    use std::io::{self, BufReader, Read};
    struct Interrupted;
    impl Read for Interrupted {
        fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
            Err(io::Error::new(
                io::ErrorKind::Other,
                "fixture input failure",
            ))
        }
    }
    let error =
        schemagraph_source::ndjson::document_from_reader(BufReader::new(Interrupted)).unwrap_err();
    assert!(error.contains("fixture input failure"), "{error}");
    let error = schemagraph_source::codec::document_from_reader(Interrupted).unwrap_err();
    assert!(error.contains("fixture input failure"), "{error}");
}
