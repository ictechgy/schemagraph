//! SQLite 네이티브 reader — sqlx로 카탈로그를 읽어 CatalogDocument를 만든다.
//!
//! PRAGMA 계열은 파라미터 바인딩이 안 되므로 식별자를 직접 이스케이프해
//! 포맷한다. 읽기 전용(`mode=ro`)으로 연다 — 스캐너가 DB를 바꾸는 일은
//! 없어야 한다.

use sqlx::sqlite::{SqliteConnectOptions, SqlitePool, SqlitePoolOptions};
use sqlx::Row;
use std::str::FromStr;

use crate::document::*;
use crate::SourceError;

/// `sqlite:경로`·`sqlite://경로`·베어 파일 경로를 받아 카탈로그를 읽는다.
pub async fn read(url: &str) -> Result<CatalogDocument, SourceError> {
    let path = strip_sqlite_url(url);
    let options = SqliteConnectOptions::from_str(&format!("sqlite:{path}"))
        .map_err(|e| SourceError::Connect(format!("sqlite 경로 해석 실패: {e}")))?
        .read_only(true);
    let pool = SqlitePoolOptions::new()
        .connect_with(options)
        .await
        .map_err(|e| SourceError::Connect(format!("sqlite 접속 실패 ({path}): {e}")))?;

    let mut doc = CatalogDocument {
        context: None,
        dependencies: Vec::new(),
        version: DOCUMENT_VERSION,
        dialect: "sqlite".to_owned(),
        reader: "native-sqlx".to_owned(),
        schemas: Vec::new(),
        limitations: Vec::new(),
    };

    let schema_names = database_list(&pool).await?;
    for schema in schema_names {
        doc.schemas
            .push(read_schema(&pool, &schema, &mut doc.limitations).await?);
    }
    doc.schemas.sort_by(|a, b| a.name.cmp(&b.name));
    doc.context = Some(CollectionContext {
        source_id: String::new(),
        database: Some("main".into()),
        schema_filter: None,
        catalog_complete: doc.limitations.is_empty(),
    });
    Ok(doc)
}

/// `sqlite:`/`sqlite://` 접두와 쿼리스트링을 벗긴다.
fn strip_sqlite_url(url: &str) -> String {
    let body = url
        .strip_prefix("sqlite://")
        .or_else(|| url.strip_prefix("sqlite:"))
        .unwrap_or(url);
    body.split('?').next().unwrap_or(body).to_owned()
}

/// attached database 목록. temp는 스캔하지 않는다 — 임시 객체는 세션 한정이라
/// 의존성 분석의 대상이 아니다.
async fn database_list(pool: &SqlitePool) -> Result<Vec<String>, SourceError> {
    let rows = sqlx::query("PRAGMA database_list")
        .fetch_all(pool)
        .await
        .map_err(SourceError::Query)?;
    Ok(rows
        .iter()
        .map(|r| r.get::<String, _>("name"))
        .filter(|n| n != "temp")
        .collect())
}

/// PRAGMA에 넣을 식별자 이스케이프. 큰따옴표를 두 번 쓴다.
fn qi(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

async fn read_schema(
    pool: &SqlitePool,
    schema: &str,
    limitations: &mut Vec<String>,
) -> Result<SchemaDoc, SourceError> {
    // sqlite_master는 테이블·뷰·트리거·인덱스를 한 데 담는다.
    // sqlite_% 접두는 내부 객체(autoincrement 시퀀스 등)라 제외한다.
    let rows = sqlx::query(&format!(
        "SELECT name, type, sql, tbl_name FROM {schema}.sqlite_master \
         WHERE name NOT LIKE 'sqlite_%' ORDER BY name",
        schema = qi(schema)
    ))
    .fetch_all(pool)
    .await
    .map_err(SourceError::Query)?;

    let mut objects: Vec<ObjectDoc> = Vec::new();
    let mut pending_triggers: Vec<(String, String, Option<String>)> = Vec::new();
    let mut inline_constraints = false;

    for row in &rows {
        let (name, kind, sql, tbl_name): (String, String, Option<String>, Option<String>) = (
            row.get("name"),
            row.get("type"),
            row.get("sql"),
            row.get("tbl_name"),
        );
        match kind.as_str() {
            "table" | "view" => {
                // UNIQUE/CHECK는 DDL 안에 박혀 있어 PRAGMA로 못 읽는다.
                // 엔진의 몸체 파서가 보강할 때까지 한계로 남긴다.
                if let Some(ddl) = &sql {
                    let lower = ddl.to_lowercase();
                    if lower.contains("unique") || lower.contains("check") {
                        inline_constraints = true;
                    }
                }
                objects.push(ObjectDoc {
                    name: name.clone(),
                    kind: if kind == "table" { "table" } else { "view" }.to_owned(),
                    columns: read_columns(pool, schema, &name).await?,
                    constraints: read_constraints(pool, schema, &name).await?,
                    indexes: read_indexes(pool, schema, &name).await?,
                    triggers: Vec::new(),
                    body: sql,
                    // SQLite는 사용 통계 카탈로그가 없다 — 미수집과 0 관측을 구분해 None.
                    usage: None,
                });
            }
            "trigger" => {
                if let Some(tbl) = tbl_name {
                    pending_triggers.push((tbl, name, sql));
                }
            }
            // autoindex·수동 인덱스는 index_list에서 컬럼 정보와 함께 읽는다.
            _ => {}
        }
    }

    for (tbl, name, sql) in pending_triggers {
        if let Some(obj) = objects.iter_mut().find(|o| o.name == tbl) {
            obj.triggers.push(TriggerDoc { name, body: sql });
        } else {
            limitations.push(format!(
                "trigger {schema}.{name}의 대상 {tbl}을 스캔에서 찾지 못함"
            ));
        }
    }

    if inline_constraints {
        limitations.push(
            "DDL 인라인 UNIQUE/CHECK 제약은 카탈로그에 없어 수집하지 못함 (인라인 제약 파싱 미지원)"
                .to_owned(),
        );
    }

    objects.sort_by(|a, b| a.name.cmp(&b.name));
    for obj in &mut objects {
        obj.columns.sort_by_key(|c| c.ordinal);
        obj.constraints.sort_by(|a, b| a.name.cmp(&b.name));
        obj.indexes.sort_by(|a, b| a.name.cmp(&b.name));
        obj.triggers.sort_by(|a, b| a.name.cmp(&b.name));
    }
    Ok(SchemaDoc {
        name: schema.to_owned(),
        objects,
        routines: Vec::new(),
    })
}

async fn read_columns(
    pool: &SqlitePool,
    schema: &str,
    table: &str,
) -> Result<Vec<ColumnDoc>, SourceError> {
    // table_xinfo는 hidden/generated 컬럼까지 보여준다.
    let rows = sqlx::query(&format!(
        "PRAGMA {schema}.table_xinfo({table})",
        schema = qi(schema),
        table = qi(table)
    ))
    .fetch_all(pool)
    .await
    .map_err(SourceError::Query)?;
    Ok(rows
        .iter()
        .enumerate()
        .map(|(i, r)| ColumnDoc {
            name: r.get::<String, _>("name"),
            data_type: r.get::<String, _>("type"),
            nullable: r.get::<i64, _>("notnull") == 0 && r.get::<i64, _>("pk") == 0,
            default: r.get::<Option<String>, _>("dflt_value"),
            ordinal: (i + 1) as u32,
            pk_position: r.get::<i64, _>("pk") as u32,
        })
        .collect())
}

async fn read_constraints(
    pool: &SqlitePool,
    schema: &str,
    table: &str,
) -> Result<Vec<ConstraintDoc>, SourceError> {
    let mut constraints = Vec::new();

    // FK 목록. 여러 컬럼 FK는 같은 id로 묶인다.
    let fk_rows = sqlx::query(&format!(
        "PRAGMA {schema}.foreign_key_list({table})",
        schema = qi(schema),
        table = qi(table)
    ))
    .fetch_all(pool)
    .await
    .map_err(SourceError::Query)?;

    let mut fk_groups: std::collections::BTreeMap<i64, Vec<(String, String)>> =
        std::collections::BTreeMap::new();
    let mut fk_targets: std::collections::BTreeMap<i64, String> = std::collections::BTreeMap::new();
    for r in &fk_rows {
        let id = r.get::<i64, _>("id");
        fk_targets.insert(id, r.get::<String, _>("table"));
        fk_groups.entry(id).or_default().push((
            r.get::<String, _>("from"),
            r.get::<Option<String>, _>("to").unwrap_or_default(),
        ));
    }
    for (id, pairs) in fk_groups {
        constraints.push(ConstraintDoc {
            name: format!("{table}_fk_{id}"),
            kind: "fk".to_owned(),
            columns: pairs.iter().map(|(from, _)| from.clone()).collect(),
            referenced: Some(ReferencedDoc {
                // SQLite의 foreign_key_list는 대상 스키마를 안 알려준다 —
                // 같은 스키마로 단정하지 않고 None으로 둔다.
                schema: None,
                table: fk_targets[&id].clone(),
                columns: pairs.iter().map(|(_, to)| to.clone()).collect(),
            }),
        });
    }

    // PK는 table_xinfo의 pk 위치로 재구성한다(SQLite는 제약 이름이 없다).
    let pk_cols = read_columns(pool, schema, table)
        .await?
        .into_iter()
        .filter(|c| c.pk_position > 0)
        .map(|c| (c.pk_position, c.name))
        .collect::<Vec<_>>();
    if !pk_cols.is_empty() {
        let mut pk_cols = pk_cols;
        pk_cols.sort();
        constraints.push(ConstraintDoc {
            name: format!("{table}_pk"),
            kind: "pk".to_owned(),
            columns: pk_cols.into_iter().map(|(_, n)| n).collect(),
            referenced: None,
        });
    }
    Ok(constraints)
}

async fn read_indexes(
    pool: &SqlitePool,
    schema: &str,
    table: &str,
) -> Result<Vec<IndexDoc>, SourceError> {
    let list = sqlx::query(&format!(
        "PRAGMA {schema}.index_list({table})",
        schema = qi(schema),
        table = qi(table)
    ))
    .fetch_all(pool)
    .await
    .map_err(SourceError::Query)?;

    let mut indexes = Vec::new();
    for r in &list {
        let idx_name: String = r.get("name");
        let cols = sqlx::query(&format!(
            "PRAGMA {schema}.index_xinfo({idx})",
            schema = qi(schema),
            idx = qi(&idx_name)
        ))
        .fetch_all(pool)
        .await
        .map_err(SourceError::Query)?;
        // key=0은 WITHOUT ROWID 보조 키라 인덱스 prefix가 아니다. key=1인
        // 식/표현식 항목은 이름을 복원할 수 없으므로 definition을 불완전하게 둔다.
        let mut definition_complete = true;
        let columns: Vec<String> = cols
            .iter()
            .filter_map(|c| {
                if c.get::<i64, _>("key") == 0 {
                    return None;
                }
                let cid: i64 = c.get("cid");
                if cid < 0 {
                    definition_complete = false;
                    return None;
                }
                match c.get::<Option<String>, _>("name") {
                    Some(name) => Some(name),
                    None => {
                        definition_complete = false;
                        None
                    }
                }
            })
            .collect();
        let partial = r.get::<i64, _>("partial") == 1;
        indexes.push(IndexDoc {
            has_predicate: Some(partial),
            definition_complete: Some(definition_complete),
            predicate: None,
            name: idx_name,
            unique: r.get::<i64, _>("unique") == 1,
            columns,
            usage: None,
        });
    }
    Ok(indexes)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// fixture SQL을 적용한 임시 DB를 만들어 document를 읽는다.
    async fn fixture_doc(sql: &str) -> CatalogDocument {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("t.db");
        let pool = SqlitePoolOptions::new()
            .connect(&format!("sqlite:{}?mode=rwc", path.display()))
            .await
            .unwrap();
        // 트리거 몸체의 BEGIN..END 안에도 ';'가 있으므로 naive split은 못 쓴다 —
        // raw_sql이 문 단위로 나눠 실행해 준다.
        sqlx::raw_sql(sql).execute(&pool).await.unwrap();
        pool.close().await;
        let doc = read(&format!("sqlite:{}", path.display())).await.unwrap();
        // tempdir이 drop돼도 doc은 이미 메모리에 있다.
        doc
    }

    #[tokio::test]
    async fn 테이블과_fk와_pk를_읽는다() {
        let doc = fixture_doc(
            "CREATE TABLE customers(id INTEGER PRIMARY KEY, name TEXT);
             CREATE TABLE orders(id INTEGER PRIMARY KEY, customer_id INTEGER NOT NULL REFERENCES customers(id))",
        )
        .await;
        let main = &doc.schemas[0];
        assert_eq!(main.name, "main");
        let orders = main.objects.iter().find(|o| o.name == "orders").unwrap();
        let fk = orders.constraints.iter().find(|c| c.kind == "fk").unwrap();
        assert_eq!(fk.columns, ["customer_id"]);
        let referenced = fk.referenced.as_ref().unwrap();
        assert_eq!(referenced.table, "customers");
        assert_eq!(referenced.columns, ["id"]);
        assert!(orders.constraints.iter().any(|c| c.kind == "pk"));
    }

    #[tokio::test]
    async fn view_본문을_수집한다() {
        let doc = fixture_doc(
            "CREATE TABLE a(id INTEGER PRIMARY KEY);
             CREATE VIEW v AS SELECT id FROM a",
        )
        .await;
        let view = doc.schemas[0]
            .objects
            .iter()
            .find(|o| o.name == "v")
            .unwrap();
        assert_eq!(view.kind, "view");
        assert!(view.body.as_ref().unwrap().contains("SELECT"));
    }

    #[tokio::test]
    async fn 트리거는_대상_테이블에_붙는다() {
        let doc = fixture_doc(
            "CREATE TABLE a(id INTEGER PRIMARY KEY);
             CREATE TRIGGER t1 AFTER INSERT ON a BEGIN SELECT 1; END",
        )
        .await;
        let a = doc.schemas[0]
            .objects
            .iter()
            .find(|o| o.name == "a")
            .unwrap();
        assert_eq!(a.triggers.len(), 1);
        assert_eq!(a.triggers[0].name, "t1");
    }
}
