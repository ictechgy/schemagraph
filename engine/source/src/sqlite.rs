//! SQLite 네이티브 reader — sqlx로 카탈로그를 읽어 CatalogDocument를 만든다.
//!
//! PRAGMA 계열은 파라미터 바인딩이 안 되므로 식별자를 직접 이스케이프해
//! 포맷한다. 읽기 전용(`mode=ro`)으로 연다 — 스캐너가 DB를 바꾸는 일은
//! 없어야 한다.

use sqlx::sqlite::{SqliteConnectOptions, SqlitePool, SqlitePoolOptions, SqliteRow};
use sqlx::Row;
use std::path::PathBuf;
use std::str::FromStr;

use crate::document::*;
use crate::SourceError;

#[cfg(test)]
thread_local! {
    /// 이 테스트 스레드가 보낸 PRAGMA 문. 같은 사실을 다시 묻는지 테스트가 본다.
    static PRAGMA_STATEMENTS: std::cell::RefCell<Vec<String>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// 현재 테스트 스레드가 보낸 PRAGMA 문을 돌려주고 비운다.
#[cfg(test)]
fn take_pragma_statements() -> Vec<String> {
    PRAGMA_STATEMENTS.with(std::cell::RefCell::take)
}

/// PRAGMA 하나를 실행한다. 모든 PRAGMA가 이 길로 나가야 테스트가 왕복을 셀 수 있다.
async fn pragma(pool: &SqlitePool, statement: &str) -> Result<Vec<SqliteRow>, SourceError> {
    #[cfg(test)]
    PRAGMA_STATEMENTS.with(|statements| statements.borrow_mut().push(statement.to_owned()));
    sqlx::query(statement)
        .fetch_all(pool)
        .await
        .map_err(SourceError::Query)
}

/// `sqlite:경로`·`sqlite://경로`·베어 파일 경로를 받아 카탈로그를 읽는다.
pub async fn read(url: &str) -> Result<CatalogDocument, SourceError> {
    let path = strip_sqlite_url(url);
    let options = connect_options(&path)?;
    let pool = SqlitePoolOptions::new()
        .connect_with(options)
        .await
        .map_err(|e| {
            // 접속 문자열이나 URL 오타가 경로로 남았을 수 있어 경로의 자격 증명 형태는 가린다.
            let path = crate::redact_connection_secrets(&path);
            SourceError::Connect(format!("sqlite 접속 실패 ({path}): {e}"))
        })?;

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

/// CLI가 SQLite 입력 파일을 출력으로 덮어쓰지 않도록 실제 접속과 같은 경로를 돌려준다.
/// URL 인코딩도 sqlx의 해석을 공유하며, 메모리 DB에는 보호할 파일이 없다.
pub fn database_path(url: &str) -> Result<Option<PathBuf>, SourceError> {
    let path = strip_sqlite_url(url);
    if path == ":memory:" {
        return Ok(None);
    }
    let options = connect_options(&path)?;
    let filename = options.get_filename();
    if filename == std::path::Path::new(":memory:") {
        return Ok(None);
    }
    let Some(uri) = filename
        .to_str()
        .and_then(|name| name.strip_prefix("file:"))
    else {
        return Ok(Some(filename.to_owned()));
    };
    // sqlx는 SQLITE_OPEN_URI로 열므로 file: 접두는 실제 파일 이름의 일부가 아니다.
    // SQLite가 URI의 경로를 한 번 더 디코딩하는 의미를 같은 sqlx 파서로 재사용한다.
    let uri_path = uri.split(['?', '#']).next().unwrap_or(uri);
    let uri_path = match uri_path.strip_prefix("//") {
        Some(authority_path) => {
            let (authority, path) = authority_path
                .split_once('/')
                .unwrap_or((authority_path, ""));
            if !matches!(authority, "" | "localhost") {
                return Err(SourceError::Connect(
                    "SQLite file URI must use an empty or localhost authority".into(),
                ));
            }
            format!("/{path}")
        }
        None => uri_path.to_owned(),
    };
    if uri_path == ":memory:" || uri_path.is_empty() {
        return Ok(None);
    }
    let decoded = connect_options(&uri_path)?.get_filename().to_owned();
    Ok((decoded != std::path::Path::new(":memory:")).then_some(decoded))
}

fn connect_options(path: &str) -> Result<SqliteConnectOptions, SourceError> {
    SqliteConnectOptions::from_str(&format!("sqlite:{path}"))
        .map(|options| options.read_only(true))
        .map_err(|e| SourceError::Connect(format!("sqlite 경로 해석 실패: {e}")))
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
    let rows = pragma(pool, "PRAGMA database_list").await?;
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
                // 한 번 읽은 컬럼·index_list를 제약·인덱스 수집에 넘겨 같은 PRAGMA를 다시 보내지 않는다.
                let (columns, index_list) = read_columns(pool, schema, &name).await?;
                let constraints = read_constraints(pool, schema, &name, &columns).await?;
                let indexes = read_indexes(pool, schema, &index_list).await?;
                objects.push(ObjectDoc {
                    name,
                    kind: if kind == "table" { "table" } else { "view" }.to_owned(),
                    columns,
                    constraints,
                    indexes,
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

/// 컬럼과 그 테이블의 index_list 행을 함께 읽는다.
///
/// index_list는 INTEGER rowid 별칭 판정(PK 인덱스 유무)과 인덱스 목록 양쪽의 사실이다.
/// 행을 돌려줘 read_indexes가 같은 PRAGMA를 다시 보내지 않게 한다.
async fn read_columns(
    pool: &SqlitePool,
    schema: &str,
    table: &str,
) -> Result<(Vec<ColumnDoc>, Vec<SqliteRow>), SourceError> {
    // table_xinfo는 hidden/generated 컬럼까지 보여준다.
    let rows = pragma(
        pool,
        &format!(
            "PRAGMA {schema}.table_xinfo({table})",
            schema = qi(schema),
            table = qi(table)
        ),
    )
    .await?;
    // INTEGER rowid 별칭만 별도 PK 인덱스가 없다. DESC 열 제약은
    // 인덱스가 있으므로 일반 nullable PK이며, STRICT/WITHOUT ROWID는
    // table_xinfo.notnull이 이미 강제 nullability를 보고한다.
    let indexes = pragma(
        pool,
        &format!("PRAGMA {}.index_list({})", qi(schema), qi(table)),
    )
    .await?;
    let has_pk_index = indexes
        .iter()
        .any(|row| row.get::<String, _>("origin") == "pk");
    let pk_count = rows
        .iter()
        .filter(|row| row.get::<i64, _>("pk") > 0)
        .count();
    let columns = rows
        .iter()
        .enumerate()
        .map(|(i, r)| ColumnDoc {
            name: r.get::<String, _>("name"),
            data_type: r.get::<String, _>("type"),
            nullable: r.get::<i64, _>("notnull") == 0
                && !(r.get::<i64, _>("pk") > 0
                    && pk_count == 1
                    && !has_pk_index
                    && r.get::<String, _>("type").eq_ignore_ascii_case("INTEGER")),
            default: r.get::<Option<String>, _>("dflt_value"),
            ordinal: (i + 1) as u32,
            pk_position: r.get::<i64, _>("pk") as u32,
        })
        .collect();
    Ok((columns, indexes))
}

/// FK와 PK 제약. PK는 read_columns가 이미 읽은 pk 위치로 만든다 — table_xinfo를
/// 다시 보내면 같은 사실을 테이블마다 한 번 더 묻는다.
async fn read_constraints(
    pool: &SqlitePool,
    schema: &str,
    table: &str,
    columns: &[ColumnDoc],
) -> Result<Vec<ConstraintDoc>, SourceError> {
    let mut constraints = Vec::new();

    // FK 목록. 여러 컬럼 FK는 같은 id로 묶인다.
    let fk_rows = pragma(
        pool,
        &format!(
            "PRAGMA {schema}.foreign_key_list({table})",
            schema = qi(schema),
            table = qi(table)
        ),
    )
    .await?;

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
    let pk_cols = columns
        .iter()
        .filter(|c| c.pk_position > 0)
        .map(|c| (c.pk_position, c.name.clone()))
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

/// index_list 행마다 index_xinfo로 키 컬럼을 읽는다. 행은 read_columns가 이미 읽은 것이다.
async fn read_indexes(
    pool: &SqlitePool,
    schema: &str,
    list: &[SqliteRow],
) -> Result<Vec<IndexDoc>, SourceError> {
    let mut indexes = Vec::new();
    for r in list {
        let idx_name: String = r.get("name");
        let cols = pragma(
            pool,
            &format!(
                "PRAGMA {schema}.index_xinfo({idx})",
                schema = qi(schema),
                idx = qi(&idx_name)
            ),
        )
        .await?;
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

    #[tokio::test]
    async fn primary_key_nullability_matches_sqlite_storage() {
        let doc = fixture_doc(include_str!("../../../Fixtures/sqlite-nullability.sql")).await;
        assert_eq!(doc.schemas[0].objects.len(), 9);
        for object in &doc.schemas[0].objects {
            for column in &object.columns {
                assert_eq!(
                    column.nullable,
                    object.name.starts_with("nullable_"),
                    "{}.{}",
                    object.name,
                    column.name
                );
                assert!(column.pk_position > 0);
            }
        }
    }

    #[tokio::test]
    async fn encoded_memory_urls_are_readable_without_a_local_input_file() {
        for url in [
            "sqlite::memory:",
            "sqlite:%3Amemory%3A",
            "sqlite:file:%253Amemory%253A",
        ] {
            assert_eq!(database_path(url).unwrap(), None, "{url}");
            let document = read(url).await.unwrap();
            assert_eq!(document.dialect, "sqlite");
            assert!(document
                .schemas
                .iter()
                .all(|schema| schema.objects.is_empty()));
        }
    }

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

    /// 테이블마다 같은 PRAGMA를 다시 보내지 않는다. PK 위치·PK 인덱스 유무·인덱스 목록은
    /// table_xinfo·index_list 한 번씩이면 충분하다. 문장이 테이블마다 달라 sqlx 문장
    /// 캐시로도 재사용되지 않으므로, 다시 물으면 1만 테이블에서 PRAGMA가 3만 번 는다.
    #[tokio::test]
    async fn catalog_pragmas_run_once_per_table() {
        take_pragma_statements();
        let doc = fixture_doc(
            "CREATE TABLE parent(a INTEGER, b TEXT, PRIMARY KEY(b, a));
             CREATE TABLE child(id INTEGER PRIMARY KEY, a INTEGER, b TEXT,
                                FOREIGN KEY(b, a) REFERENCES parent(b, a));
             CREATE INDEX child_ab ON child(a, b);
             CREATE VIEW v AS SELECT id FROM child",
        )
        .await;
        let statements = take_pragma_statements();
        let mut seen = std::collections::BTreeSet::new();
        let repeated: Vec<&String> = statements.iter().filter(|s| !seen.insert(*s)).collect();
        assert!(
            repeated.is_empty(),
            "repeated PRAGMA statements: {repeated:?}"
        );

        // 한 번 읽은 사실을 넘겨받아도 PK 순서·FK·인덱스는 그대로다.
        let object = |name: &str| {
            doc.schemas[0]
                .objects
                .iter()
                .find(|o| o.name == name)
                .unwrap()
        };
        let pk = |name: &str| {
            object(name)
                .constraints
                .iter()
                .find(|c| c.kind == "pk")
                .map(|c| c.columns.clone())
        };
        let indexes = |name: &str| -> Vec<String> {
            object(name)
                .indexes
                .iter()
                .map(|i| i.name.clone())
                .collect()
        };
        assert_eq!(pk("parent").unwrap(), ["b", "a"]);
        assert_eq!(pk("child").unwrap(), ["id"]);
        assert_eq!(pk("v"), None);
        assert_eq!(indexes("parent"), ["sqlite_autoindex_parent_1"]);
        assert_eq!(indexes("child"), ["child_ab"]);
        assert!(indexes("v").is_empty());
        let fk = object("child")
            .constraints
            .iter()
            .find(|c| c.kind == "fk")
            .unwrap();
        assert_eq!(fk.columns, ["b", "a"]);
        assert_eq!(fk.referenced.as_ref().unwrap().columns, ["b", "a"]);
        // INTEGER rowid 별칭 판정은 같은 index_list 행으로 한다.
        let child_id = &object("child").columns[0];
        assert!(!child_id.nullable && child_id.pk_position == 1);
    }
}
