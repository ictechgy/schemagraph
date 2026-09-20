//! catalog document의 NDJSON(행 단위 JSON) 전송 형식.
//!
//! 단일 JSON 문서와 같은 정보를 담지만 레코드가 한 줄씩이어서, 큰 카탈로그를
//! 순차 전송할 수 있다. 현재 리더는 전체 문서를 메모리에 조립한다. 첫 줄은 반드시
//! `{"type":"document", ...}` 헤더이고, 이후 스키마마다 `schema` 행 하나와
//! 그 스키마의 `object`·`routine` 행들이 오며, 끝에 `limitations` 행이 온다.
//! 스트리밍 프로브는 마지막까지 한계를 모르므로 헤더의 limitations는 비어
//! 있고 트레일러가 진짜 목록을 싣는다 — 리더는 둘을 합집합으로 읽는다.
//!
//! 알 수 없는 레코드 타입은 조용히 건너뛰지 않고 오류로 거부한다 — 소비자가
//! "못 읽은 행이 있었는데 없는 척"하는 그래프를 믿게 만들면 안 되기 때문이다.

use serde_json::Value;

use crate::document::{CatalogDocument, DOCUMENT_VERSION};

/// 문서가 NDJSON 형식인지 판별한다. 첫 비어있지 않은 줄이 `type:"document"`를
/// 가진 JSON 객체이면 NDJSON으로 본다. 단일 JSON 문서는 이 판별을 통과하지
/// 못한다(그 문서엔 `type` 키가 없다).
pub fn is_ndjson_document(text: &str) -> bool {
    text.lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .and_then(|l| serde_json::from_str::<Value>(l).ok())
        .and_then(|v| v.get("type").and_then(Value::as_str).map(str::to_owned))
        .as_deref()
        == Some("document")
}

/// NDJSON 텍스트를 CatalogDocument로 재조립한다. 헤더의 `version`은 단일 JSON
/// 경로와 같은 규칙으로 검사하고, `schema` 없이 온 `object`·`routine`이나
/// 알 수 없는 `type`은 오류다.
pub fn document_from_ndjson(text: &str) -> Result<CatalogDocument, String> {
    let mut header: Option<Value> = None;
    let mut schemas: Vec<Value> = Vec::new();
    let mut limitations: Vec<String> = Vec::new();
    let mut extra_notes = std::collections::BTreeSet::new();
    let mut trailer_seen = false;
    let mut version = 0;
    for (number, raw) in text.lines().enumerate() {
        if raw.trim().is_empty() {
            continue;
        }
        let record: Value = serde_json::from_str(raw)
            .map_err(|error| format!("invalid NDJSON at line {}: {error}", number + 1))?;
        let kind = record
            .get("type")
            .and_then(Value::as_str)
            .ok_or_else(|| format!("NDJSON line {} has no record type", number + 1))?;
        if header.is_none() && kind != "document" {
            return Err("NDJSON must start with a document header".into());
        }
        if version == 2 && trailer_seen {
            return Err("catalog v2 NDJSON must end with exactly one limitations trailer".into());
        }
        match kind {
            "document" => {
                if header.is_some() {
                    return Err("NDJSON has more than one document header".into());
                }
                version = crate::codec::wire_version(&record)?;
                if record.get("schemas").is_some() {
                    return Err(
                        "NDJSON header cannot embed schemas; emit schema and data records".into(),
                    );
                }
                let mut value = record;
                let fields = value
                    .as_object_mut()
                    .ok_or("NDJSON header must be an object")?;
                fields.remove("type");
                if version == 1 {
                    fields
                        .entry("reader")
                        .or_insert_with(|| Value::String("unknown".into()));
                }
                fields
                    .entry("limitations")
                    .or_insert_with(|| serde_json::json!([]));
                header = Some(value);
            }
            "schema" => {
                let name = record
                    .get("name")
                    .and_then(Value::as_str)
                    .ok_or("NDJSON schema requires a name")?;
                schemas.push(serde_json::json!({"name":name, "objects":[], "routines":[]}));
                record_unknown_keys(&record, &["type", "name"], &mut extra_notes);
            }
            "object" | "routine" => {
                let schema = schemas
                    .last_mut()
                    .ok_or("NDJSON object/routine appeared before its schema")?;
                if let Some(owner) = record.get("schema") {
                    if owner.as_str() != schema["name"].as_str() {
                        return Err(
                            "NDJSON record schema does not match the preceding schema record"
                                .into(),
                        );
                    }
                }
                let data = record
                    .get("data")
                    .ok_or("NDJSON object/routine requires data")?;
                let collection = if kind == "object" {
                    "objects"
                } else {
                    "routines"
                };
                // 스키마 레코드 생성 시 두 컬렉션을 배열로 만들었다.
                schema[collection]
                    .as_array_mut()
                    .expect("schema collections are arrays")
                    .push(data.clone());
                record_unknown_keys(&record, &["type", "schema", "data"], &mut extra_notes);
            }
            "limitations" => {
                let data = record
                    .get("data")
                    .ok_or("NDJSON limitations trailer requires data")?;
                let items: Vec<String> = serde_json::from_value(data.clone())
                    .map_err(|error| format!("invalid NDJSON limitations: {error}"))?;
                limitations.extend(items);
                trailer_seen = true;
                record_unknown_keys(&record, &["type", "data"], &mut extra_notes);
            }
            _ => {
                return Err(format!(
                    "unsupported NDJSON record type '{kind}'; upgrade the reader"
                ))
            }
        }
    }
    let mut value = header.ok_or("NDJSON document header is missing")?;
    if version == 2 && !trailer_seen {
        return Err("catalog v2 NDJSON is incomplete: limitations trailer is missing".into());
    }
    let mut from_header: Vec<String> = serde_json::from_value(value["limitations"].clone())
        .map_err(|error| format!("invalid header limitations: {error}"))?;
    from_header.append(&mut limitations);
    from_header.extend(extra_notes);
    value["limitations"] = serde_json::json!(from_header);
    value["schemas"] = serde_json::json!(schemas);
    crate::codec::document_from_value(value)
}

fn record_unknown_keys(
    record: &Value,
    known: &[&str],
    notes: &mut std::collections::BTreeSet<String>,
) {
    if let Some(map) = record.as_object() {
        for key in map.keys().filter(|key| !known.contains(&key.as_str())) {
            notes.insert(format!("ignored NDJSON record field: {key}"));
        }
    }
}

/// CatalogDocument를 NDJSON 텍스트로 직렬화한다. 프로브가 출력하는 형식과
/// 같은 레이아웃이라 왕복(round-trip)이 성립해야 한다.
pub fn document_to_ndjson(doc: &CatalogDocument) -> String {
    // v1은 고정 지원 버전이고 document의 모든 필드는 JSON으로 직렬화 가능하다.
    document_to_ndjson_version(doc, DOCUMENT_VERSION)
        .expect("v1 catalog serialization is supported")
}

/// 협상 메타데이터만 바꾸고 레코드의 의미는 동일하게 유지하는 v1/v2 출력이다.
pub fn document_to_ndjson_version(doc: &CatalogDocument, version: u32) -> Result<String, String> {
    let metadata = CatalogDocument {
        version: DOCUMENT_VERSION,
        dialect: doc.dialect.clone(),
        reader: doc.reader.clone(),
        schemas: Vec::new(),
        limitations: Vec::new(),
    };
    let mut header = crate::codec::document_to_value(&metadata, version)?;
    // 위 함수는 CatalogDocument 구조체를 JSON 객체로 직렬화한다.
    header
        .as_object_mut()
        .expect("catalog header is an object")
        .remove("schemas");
    header["type"] = serde_json::json!("document");
    header["limitations"] = serde_json::json!([]);
    if version == 2 {
        header["required_features"] = serde_json::json!(crate::codec::document_features(doc));
    }
    let mut output = String::new();
    append_record(&mut output, header)?;
    for schema in &doc.schemas {
        append_record(
            &mut output,
            serde_json::json!({"type":"schema", "name":schema.name}),
        )?;
        for object in &schema.objects {
            append_record(
                &mut output,
                serde_json::json!({"type":"object", "schema":schema.name, "data":object}),
            )?;
        }
        for routine in &schema.routines {
            append_record(
                &mut output,
                serde_json::json!({"type":"routine", "schema":schema.name, "data":routine}),
            )?;
        }
    }
    append_record(
        &mut output,
        serde_json::json!({"type":"limitations", "data":doc.limitations}),
    )?;
    Ok(output)
}

fn append_record(output: &mut String, record: Value) -> Result<(), String> {
    output.push_str(&serde_json::to_string(&record).map_err(|error| error.to_string())?);
    output.push('\n');
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::document::{ColumnDoc, ObjectDoc, SchemaDoc};

    fn sample_doc() -> CatalogDocument {
        CatalogDocument {
            version: DOCUMENT_VERSION,
            dialect: "sqlite".into(),
            reader: "probe-jdbc".into(),
            schemas: vec![SchemaDoc {
                name: "main".into(),
                objects: vec![ObjectDoc {
                    name: "t".into(),
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
                    indexes: vec![],
                    triggers: vec![],
                    body: None,
                    usage: None,
                }],
                routines: vec![],
            }],
            limitations: vec!["x 한계".into()],
        }
    }

    #[test]
    fn ndjson_round_trip() {
        let doc = sample_doc();
        let text = document_to_ndjson(&doc);
        assert!(is_ndjson_document(&text));
        let back = document_from_ndjson(&text).unwrap();
        assert_eq!(back, doc);
    }

    #[test]
    fn single_json_is_not_ndjson() {
        let json = serde_json::to_string(&sample_doc()).unwrap();
        assert!(!is_ndjson_document(&json));
    }

    #[test]
    fn ndjson_rejects_unknown_record_type() {
        let text = concat!(
            "{\"type\":\"document\",\"version\":1,\"dialect\":\"sqlite\",\"reader\":\"x\",\"limitations\":[]}\n",
            "{\"type\":\"schema\",\"name\":\"main\"}\n",
            "{\"type\":\"wat\",\"data\":{}}\n",
        );
        let err = document_from_ndjson(text).unwrap_err();
        assert!(err.contains("wat"), "{err}");
    }

    #[test]
    fn ndjson_rejects_object_before_schema() {
        let text = concat!(
            "{\"type\":\"document\",\"version\":1,\"dialect\":\"sqlite\",\"reader\":\"x\",\"limitations\":[]}\n",
            "{\"type\":\"object\",\"schema\":\"main\",\"data\":{\"name\":\"t\",\"kind\":\"table\",\"columns\":[],\"constraints\":[],\"indexes\":[],\"triggers\":[]}}\n",
        );
        assert!(document_from_ndjson(text).is_err());
    }

    #[test]
    fn ndjson_rejects_wrong_version() {
        let text = "{\"type\":\"document\",\"version\":99,\"dialect\":\"sqlite\",\"reader\":\"x\",\"limitations\":[]}\n";
        assert!(document_from_ndjson(text).is_err());
    }

    #[test]
    fn ndjson_trailer_limitations() {
        // 스트리밍 정본 — 헤더는 비어 있고 트레일러가 진짜 목록을 싣는다.
        // 헤더·트레일러는 합집합으로 정렬·중복 제거된다.
        let text = concat!(
            "{\"type\":\"document\",\"version\":1,\"dialect\":\"sqlite\",\"reader\":\"x\",\"limitations\":[\"b\",\"a\"]}\n",
            "{\"type\":\"schema\",\"name\":\"main\"}\n",
            "{\"type\":\"limitations\",\"data\":[\"b\",\"c\"]}\n",
        );
        let doc = document_from_ndjson(text).unwrap();
        assert_eq!(
            doc.limitations,
            vec!["a".to_string(), "b".to_string(), "c".to_string()]
        );
    }
}
