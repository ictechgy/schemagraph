//! catalog document의 NDJSON(행 단위 JSON) 전송 형식.
//!
//! 단일 JSON 문서와 같은 정보를 담지만 레코드가 한 줄씩이어서, 큰 카탈로그를
//! 통째로 메모리에 올리지 않고 순차적으로 읽고 쓸 수 있다. 첫 줄은 반드시
//! `{"type":"document", ...}` 헤더이고, 이후 스키마마다 `schema` 행 하나와
//! 그 스키마의 `object`·`routine` 행들이 온다.
//!
//! 알 수 없는 레코드 타입은 조용히 건너뛰지 않고 오류로 거부한다 — 소비자가
//! "못 읽은 행이 있었는데 없는 척"하는 그래프를 믿게 만들면 안 되기 때문이다.

use serde_json::Value;

use crate::document::{CatalogDocument, ObjectDoc, RoutineDoc, SchemaDoc, DOCUMENT_VERSION};

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
    let mut header: Option<(u32, String, String, Vec<String>)> = None;
    let mut schemas: Vec<SchemaDoc> = Vec::new();

    for (n, raw) in text.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() {
            continue;
        }
        let v: Value =
            serde_json::from_str(line).map_err(|e| format!("NDJSON {}행 파싱 실패: {e}", n + 1))?;
        let ty = v
            .get("type")
            .and_then(Value::as_str)
            .ok_or_else(|| format!("NDJSON {}행에 type 키가 없다", n + 1))?;
        match ty {
            "document" => {
                if header.is_some() {
                    return Err("NDJSON에 document 헤더가 둘 이상 있다".into());
                }
                let version = v
                    .get("version")
                    .and_then(Value::as_u64)
                    .ok_or("NDJSON 헤더에 version이 없다")? as u32;
                let dialect = v
                    .get("dialect")
                    .and_then(Value::as_str)
                    .ok_or("NDJSON 헤더에 dialect가 없다")?
                    .to_owned();
                let reader = v
                    .get("reader")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown")
                    .to_owned();
                let limitations = v
                    .get("limitations")
                    .and_then(|l| serde_json::from_value::<Vec<String>>(l.clone()).ok())
                    .unwrap_or_default();
                header = Some((version, dialect, reader, limitations));
            }
            "schema" => {
                let name = v
                    .get("name")
                    .and_then(Value::as_str)
                    .ok_or_else(|| format!("NDJSON {}행 schema에 name이 없다", n + 1))?;
                schemas.push(SchemaDoc {
                    name: name.to_owned(),
                    objects: Vec::new(),
                    routines: Vec::new(),
                });
            }
            "object" => {
                let schema = schemas
                    .last_mut()
                    .ok_or_else(|| format!("NDJSON {}행: schema 선언 전에 object가 왔다", n + 1))?;
                let data = v
                    .get("data")
                    .ok_or_else(|| format!("NDJSON {}행 object에 data가 없다", n + 1))?;
                let obj: ObjectDoc = serde_json::from_value(data.clone())
                    .map_err(|e| format!("NDJSON {}행 object 파싱 실패: {e}", n + 1))?;
                schema.objects.push(obj);
            }
            "routine" => {
                let schema = schemas.last_mut().ok_or_else(|| {
                    format!("NDJSON {}행: schema 선언 전에 routine이 왔다", n + 1)
                })?;
                let data = v
                    .get("data")
                    .ok_or_else(|| format!("NDJSON {}행 routine에 data가 없다", n + 1))?;
                let r: RoutineDoc = serde_json::from_value(data.clone())
                    .map_err(|e| format!("NDJSON {}행 routine 파싱 실패: {e}", n + 1))?;
                schema.routines.push(r);
            }
            other => {
                return Err(format!(
                    "NDJSON {}행: 알 수 없는 레코드 타입 '{other}' — 지원되지 않는 행을 건너뛰지 않는다",
                    n + 1
                ));
            }
        }
    }

    let (version, dialect, reader, limitations) =
        header.ok_or("NDJSON에 document 헤더 행이 없다")?;
    if version != DOCUMENT_VERSION {
        return Err(format!(
            "document 버전 {version}은 지원하지 않는다 (이 엔진은 v{DOCUMENT_VERSION})"
        ));
    }
    Ok(CatalogDocument {
        version,
        dialect,
        reader,
        schemas,
        limitations,
    })
}

/// CatalogDocument를 NDJSON 텍스트로 직렬화한다. 프로브가 출력하는 형식과
/// 같은 레이아웃이라 왕복(round-trip)이 성립해야 한다.
pub fn document_to_ndjson(doc: &CatalogDocument) -> String {
    let mut out = String::new();
    let mut push = |v: Value| {
        out.push_str(&serde_json::to_string(&v).unwrap_or_default());
        out.push('\n');
    };
    push(serde_json::json!({
        "type": "document",
        "version": doc.version,
        "dialect": doc.dialect,
        "reader": doc.reader,
        "limitations": doc.limitations,
    }));
    for schema in &doc.schemas {
        push(serde_json::json!({"type": "schema", "name": schema.name}));
        for obj in &schema.objects {
            push(serde_json::json!({
                "type": "object",
                "schema": schema.name,
                "data": obj,
            }));
        }
        for r in &schema.routines {
            push(serde_json::json!({
                "type": "routine",
                "schema": schema.name,
                "data": r,
            }));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::document::{ColumnDoc, ObjectDoc};

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
}
