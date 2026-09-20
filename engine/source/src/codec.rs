//! 전송 버전과 내부 카탈로그 타입을 분리한다. v1을 계속 읽으면서 v2의
//! 필드 변경과 필수 기능을 검증한 뒤 같은 그래프로 정규화한다.

use std::collections::BTreeSet;

use serde_json::{json, Value};

use crate::document::{unknown_field_paths, CatalogDocument, DOCUMENT_VERSION};

/// 새 생산자가 명시적으로 선택할 수 있는 전송 버전. 기본 출력은 v1을 유지한다.
pub const LATEST_DOCUMENT_VERSION: u32 = 2;
/// 의미를 모르는 필수 기능을 조용히 무시하지 않도록 명시한 소비자 능력이다.
pub const SUPPORTED_FEATURES: &[&str] = &[
    "catalog-dependencies-v1",
    "external-queries-v1",
    "package-members-v1",
    "usage-v1",
];

/// 범위를 먼저 검사해 큰 버전 정수가 u32로 잘려 구버전으로 오인되지 않게 한다.
pub fn wire_version(value: &Value) -> Result<u32, String> {
    let number = value
        .get("version")
        .and_then(Value::as_u64)
        .ok_or("catalog version must be an unsigned integer; use document version 1 or 2")?;
    match number {
        1 | 2 => Ok(number as u32),
        _ => Err(format!(
            "unsupported catalog version {number}; this engine reads versions 1 and 2"
        )),
    }
}

/// 같은 내용의 JSON·NDJSON이 같은 내부 문서와 한계를 만들도록 JSON 로딩을 공유한다.
pub fn document_from_json(text: &str) -> Result<CatalogDocument, String> {
    let value =
        serde_json::from_str(text).map_err(|error| format!("invalid catalog JSON: {error}"))?;
    document_from_value(value)
}

/// 원문 전체 문자열을 따로 보관하지 않고 입력을 읽어 큰 카탈로그의 복제를 줄인다.
pub fn document_from_reader(reader: impl std::io::Read) -> Result<CatalogDocument, String> {
    let value = serde_json::from_reader(reader)
        .map_err(|error| format!("invalid catalog JSON: {error}"))?;
    document_from_value(value)
}

/// v2에서 제거된 키와 필수 기능을 검사한 후 v1 도메인 타입으로 정규화한다.
pub fn document_from_value(value: Value) -> Result<CatalogDocument, String> {
    let unknown = unknown_field_paths(&value).into_iter().collect();
    let doc = decode_document(value)?;
    Ok(finish_document(doc, unknown))
}

/// NDJSON은 헤더와 본문을 따로 읽으므로 필드 변환과 최종 진단 집계를 분리한다.
pub(crate) fn decode_document(mut value: Value) -> Result<CatalogDocument, String> {
    let version = wire_version(&value)?;
    if version == 1 && required_features(&value).contains("external-queries-v1") {
        return Err("external query records require catalog v2 and external-queries-v1".into());
    }
    if version == 2 {
        validate_v2(&value)?;
        let reader = value["producer"]["name"].clone();
        let map = value
            .as_object_mut()
            .ok_or("catalog document must be an object")?;
        map.remove("producer");
        map.remove("required_features");
        map.insert("reader".into(), reader);
        map.insert("version".into(), json!(DOCUMENT_VERSION));
    }
    serde_json::from_value(value).map_err(|error| format!("invalid catalog fields: {error}"))
}

pub(crate) fn finish_document(
    mut doc: CatalogDocument,
    unknown: BTreeSet<String>,
) -> CatalogDocument {
    doc.dependencies.sort();
    doc.dependencies.dedup();
    if let Some(context) = &mut doc.context {
        if let Some(filter) = &mut context.schema_filter {
            filter.sort();
            filter.dedup();
        }
    }
    if !unknown.is_empty() {
        doc.limitations.push(format!(
            "ignored catalog fields: {}",
            unknown.into_iter().collect::<Vec<_>>().join(", ")
        ));
    }
    doc.limitations.extend(duplicate_record_notes(&doc));
    doc.limitations.sort();
    doc.limitations.dedup();
    doc
}

/// 같은 식별자의 반복을 버전과 무관하게 세어 그래프 병합이 손실 없이 보이지 않게 한다.
/// 기존 v1 생산자의 입력은 계속 받되 소비자가 중복의 영향을 알 수 있게 한다.
fn duplicate_record_notes(doc: &CatalogDocument) -> Vec<String> {
    let mut counts = std::collections::BTreeMap::from([
        (
            "schema",
            duplicate_count(doc.schemas.iter().map(|schema| &schema.name)),
        ),
        ("object", 0),
        ("routine", 0),
        ("column", 0),
        ("constraint", 0),
        ("index", 0),
        ("trigger", 0),
    ]);
    for schema in &doc.schemas {
        *counts.entry("object").or_default() += duplicate_count(
            schema
                .objects
                .iter()
                .map(|object| (&object.name, &object.kind)),
        );
        *counts.entry("routine").or_default() +=
            duplicate_count(schema.routines.iter().map(|routine| {
                (
                    &routine.name,
                    &routine.kind,
                    routine.signature.as_deref().unwrap_or(""),
                    &routine.member_of,
                )
            }));
        for object in &schema.objects {
            *counts.entry("column").or_default() +=
                duplicate_count(object.columns.iter().map(|column| &column.name));
            *counts.entry("constraint").or_default() +=
                duplicate_count(object.constraints.iter().map(|constraint| &constraint.name));
            *counts.entry("index").or_default() +=
                duplicate_count(object.indexes.iter().map(|index| &index.name));
            *counts.entry("trigger").or_default() +=
                duplicate_count(object.triggers.iter().map(|trigger| &trigger.name));
        }
    }
    counts.into_iter().filter(|(_, count)| *count > 0).map(|(kind, count)| {
        format!("catalog contains {count} duplicate {kind} identities; repeated records may be merged; emit unique records")
    }).collect()
}

fn duplicate_count<T: Ord>(identities: impl IntoIterator<Item = T>) -> usize {
    let mut seen = BTreeSet::new();
    identities.into_iter().fold(0, |count, identity| {
        count + usize::from(!seen.insert(identity))
    })
}

fn validate_v2(value: &Value) -> Result<(), String> {
    if value.get("reader").is_some() {
        return Err("catalog v2 removed 'reader'; use 'producer.name' or emit version 1".into());
    }
    if value
        .get("producer")
        .and_then(|producer| producer.get("name"))
        .and_then(Value::as_str)
        .is_none_or(str::is_empty)
    {
        return Err("catalog v2 requires a nonempty 'producer.name'".into());
    }
    let features = value
        .get("required_features")
        .and_then(Value::as_array)
        .ok_or("catalog v2 requires 'required_features' as an array of feature names")?;
    let mut declared = BTreeSet::new();
    for feature in features {
        let name = feature
            .as_str()
            .ok_or("required_features must contain only strings")?;
        if !SUPPORTED_FEATURES.contains(&name) {
            return Err(format!("unsupported required catalog feature '{name}'; upgrade the engine or emit a compatible document"));
        }
        declared.insert(name.to_owned());
    }
    for needed in required_features(value) {
        if !declared.contains(needed) {
            return Err(format!(
                "catalog v2 uses '{needed}' without declaring it in required_features"
            ));
        }
    }
    Ok(())
}

/// 기능을 쓰는 레코드만 확인해 미지 필드의 내용을 알려진 의미로 오인하지 않는다.
fn required_features(value: &Value) -> BTreeSet<&'static str> {
    let mut features = BTreeSet::new();
    if value
        .get("dependencies")
        .and_then(Value::as_array)
        .is_some_and(|d| !d.is_empty())
    {
        features.insert("catalog-dependencies-v1");
    }
    let schemas = value
        .get("schemas")
        .and_then(Value::as_array)
        .into_iter()
        .flatten();
    for schema in schemas {
        for object in schema
            .get("objects")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            add_usage_feature(object, &mut features);
            for index in object
                .get("indexes")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                add_usage_feature(index, &mut features);
            }
        }
        for routine in schema
            .get("routines")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            if routine.get("kind").and_then(Value::as_str) == Some("query") {
                features.insert("external-queries-v1");
            }
            add_usage_feature(routine, &mut features);
            if routine.get("member_of").is_some_and(|v| !v.is_null()) {
                features.insert("package-members-v1");
            }
        }
    }
    features
}

fn add_usage_feature(value: &Value, features: &mut BTreeSet<&'static str>) {
    if value.get("usage").is_some_and(|v| !v.is_null()) {
        features.insert("usage-v1");
    }
}

/// 스트리밍 헤더를 만들 때 본문 전체를 JSON으로 복제하지 않고 기능만 찾는다.
pub(crate) fn document_features(doc: &CatalogDocument) -> BTreeSet<&'static str> {
    let mut features = BTreeSet::new();
    if !doc.dependencies.is_empty() {
        features.insert("catalog-dependencies-v1");
    }
    for schema in &doc.schemas {
        if schema.routines.iter().any(|r| r.kind == "query") {
            features.insert("external-queries-v1");
        }
        if schema.objects.iter().any(|object| {
            object.usage.is_some() || object.indexes.iter().any(|index| index.usage.is_some())
        }) || schema
            .routines
            .iter()
            .any(|routine| routine.usage.is_some())
        {
            features.insert("usage-v1");
        }
        if schema
            .routines
            .iter()
            .any(|routine| routine.member_of.is_some())
        {
            features.insert("package-members-v1");
        }
    }
    features
}

/// 출력 버전 선택은 그래프 의미를 바꾸지 않고 전송 메타데이터만 변환한다.
pub fn document_to_value(doc: &CatalogDocument, version: u32) -> Result<Value, String> {
    if version == 1 && document_features(doc).contains("external-queries-v1") {
        return Err("external query records require document version 2".into());
    }
    if ![1, 2].contains(&version) {
        return Err(format!(
            "unsupported output catalog version {version}; choose 1 or 2"
        ));
    }
    if version == 2 && doc.reader.is_empty() {
        return Err(
            "catalog v2 requires a nonempty producer name; set the catalog reader identity".into(),
        );
    }
    let mut value = serde_json::to_value(doc).map_err(|error| error.to_string())?;
    value["version"] = json!(version);
    if version == 2 {
        let features = document_features(doc);
        value["required_features"] = json!(features);
        value["producer"] = json!({"name": doc.reader});
        // CatalogDocument는 구조체이므로 serde가 항상 객체로 직렬화한다.
        value
            .as_object_mut()
            .expect("catalog serializes to an object")
            .remove("reader");
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Value {
        json!({"version": 1, "reader": "fixture", "dialect": "sqlite", "schemas": [], "limitations": []})
    }

    #[test]
    fn versions_round_trip_to_the_same_catalog() {
        let doc = document_from_value(sample()).unwrap();
        let v2 = document_to_value(&doc, 2).unwrap();
        assert!(v2.get("reader").is_none());
        assert_eq!(v2["producer"]["name"], "fixture");
        assert_eq!(document_from_value(v2).unwrap(), doc);
        assert_eq!(
            document_from_value(document_to_value(&doc, 1).unwrap()).unwrap(),
            doc
        );
    }

    #[test]
    fn unsupported_or_truncated_versions_are_rejected() {
        for version in [
            json!(0),
            json!(3),
            json!(4294967297_u64),
            json!(-1),
            json!(1.5),
        ] {
            let mut value = sample();
            value["version"] = version;
            assert!(document_from_value(value).is_err());
        }
    }

    #[test]
    fn v2_requires_renamed_fields_and_known_capabilities() {
        let doc = document_from_value(sample()).unwrap();
        let v2 = document_to_value(&doc, 2).unwrap();
        let mut removed = v2.clone();
        removed["reader"] = json!("old");
        assert!(document_from_value(removed)
            .unwrap_err()
            .contains("removed"));
        let mut unsupported = v2.clone();
        unsupported["required_features"] = json!(["future-body-v2"]);
        assert!(document_from_value(unsupported)
            .unwrap_err()
            .contains("unsupported required"));
        let mut missing = v2;
        missing.as_object_mut().unwrap().remove("required_features");
        assert!(document_from_value(missing).is_err());
    }

    #[test]
    fn v2_does_not_silently_drop_undeclared_semantics() {
        let doc = document_from_value(sample()).unwrap();
        let mut value = document_to_value(&doc, 2).unwrap();
        value["schemas"] = json!([{"name":"s", "objects":[], "routines":[{
            "name":"member", "kind":"procedure", "member_of":"pkg",
            "usage":{"reads":0,"writes":0}
        }]}]);
        assert!(document_from_value(value.clone())
            .unwrap_err()
            .contains("without declaring"));
        value["required_features"] = json!(["package-members-v1", "usage-v1"]);
        let decoded = document_from_value(value).unwrap();
        let encoded = document_to_value(&decoded, 2).unwrap();
        assert_eq!(
            encoded["required_features"],
            json!(["package-members-v1", "usage-v1"])
        );
    }

    #[test]
    fn optional_future_fields_are_reported_in_both_versions() {
        let mut v1 = sample();
        v1["extra"] = json!(true);
        let doc = document_from_value(v1).unwrap();
        assert!(doc.limitations.iter().any(|note| note.contains("extra")));
        let mut v2 = document_to_value(&doc, 2).unwrap();
        v2["producer"]["build"] = json!("future");
        let decoded = document_from_value(v2).unwrap();
        assert!(decoded
            .limitations
            .iter()
            .any(|note| note.contains("producer.build")));
    }

    #[test]
    fn every_unknown_path_is_reported_without_an_unmarked_cap() {
        let mut value = sample();
        for number in 0..25 {
            value[format!("future_{number:02}")] = json!(true);
        }
        let doc = document_from_value(value).unwrap();
        for number in 0..25 {
            assert!(doc
                .limitations
                .iter()
                .any(|note| note.contains(&format!("future_{number:02}"))));
        }
    }

    #[test]
    fn repeated_records_are_counted_in_both_versions_and_transports() {
        let mut value = sample();
        let object = json!({"name":"items", "kind":"table", "columns":[], "constraints":[], "indexes":[], "triggers":[]});
        value["schemas"] =
            json!([{"name":"main", "objects":[object.clone(), object], "routines":[]}]);
        let original: CatalogDocument = serde_json::from_value(value).unwrap();
        for version in [1, 2] {
            let from_json =
                document_from_value(document_to_value(&original, version).unwrap()).unwrap();
            let from_ndjson = crate::ndjson::document_from_ndjson(
                &crate::ndjson::document_to_ndjson_version(&original, version).unwrap(),
            )
            .unwrap();
            assert_eq!(from_json, from_ndjson);
            assert!(from_json
                .limitations
                .iter()
                .any(|note| note.contains("1 duplicate object identities")));
            // 재출력·재입력에서도 동일한 한계를 중복으로 늘리지 않는다.
            assert_eq!(
                document_from_value(document_to_value(&from_json, version).unwrap()).unwrap(),
                from_json
            );
        }
    }
}
