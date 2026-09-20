//! 명시한 논리 DB와 수집 범위로 권한 부족·필터 변경을 실제 DROP과 구분한다.

use crate::document::{CatalogDocument, CollectionContext};

/// 논리 이름만 받아 인증정보가 포함된 URL의 저장을 막는다.
pub fn validate_source_id(value: &str) -> Result<(), String> {
    if value.is_empty()
        || value.len() > 128
        || !value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_-./".contains(&b))
    {
        return Err("source id must be a nonempty logical label using letters, digits, _, -, ., or /; do not supply a connection URL".into());
    }
    Ok(())
}

/// 입력 스냅샷을 실제로 필터링하며 기존에 알려진 불완전 상태를 지우지 않는다.
pub fn annotate(
    doc: &mut CatalogDocument,
    source_id: Option<&str>,
    schemas: &[String],
) -> Result<(), String> {
    if let Some(id) = source_id {
        validate_source_id(id)?;
    }
    let mut filter = schemas.to_vec();
    filter.sort();
    filter.dedup();
    if !filter.is_empty() {
        doc.schemas.retain(|s| filter.contains(&s.name));
        doc.dependencies
            .retain(|d| filter.contains(&d.source.schema));
    }
    if source_id.is_none() && filter.is_empty() {
        return Ok(());
    }
    let context = doc.context.get_or_insert_with(|| CollectionContext {
        source_id: String::new(),
        database: None,
        schema_filter: None,
        catalog_complete: doc.limitations.is_empty(),
    });
    if let Some(id) = source_id {
        context.source_id = id.to_owned();
    }
    if !filter.is_empty() {
        if let Some(previous) = &context.schema_filter {
            if filter.iter().any(|s| !previous.contains(s)) {
                context.catalog_complete = false;
            }
        }
        context.schema_filter = Some(filter);
    }
    Ok(())
}

/// 비교할 수 없는 이유를 모두 반환한다. 컨텍스트 부재를 같은 DB라고 추정하지 않는다.
pub fn comparison_notes(before: &CatalogDocument, after: &CatalogDocument) -> Vec<String> {
    let mut notes = Vec::new();
    if before.dialect != after.dialect {
        notes.push("snapshot dialects differ".into());
    }
    if before.reader != after.reader {
        notes.push("snapshot producers differ; metadata normalization may differ".into());
    }
    match (&before.context, &after.context) {
        (Some(a), Some(b)) => {
            if a.source_id.is_empty() || b.source_id.is_empty() {
                notes.push(
                    "snapshot logical source identity is missing; scan with --source-id".into(),
                );
            } else if a.source_id != b.source_id {
                notes.push("snapshot logical source identities differ".into());
            }
            if a.schema_filter != b.schema_filter {
                notes.push("snapshot schema collection filters differ".into());
            }
            if !a.catalog_complete || !b.catalog_complete {
                notes.push("one or both snapshots report incomplete catalog collection".into());
            }
        }
        _ => notes.push(
            "snapshot collection context is unavailable; capture both documents with --source-id"
                .into(),
        ),
    }
    notes.sort();
    notes.dedup();
    notes
}
