//! MySQL·MariaDB 네이티브 reader — information_schema를 읽어
//! CatalogDocument를 만든다.
//!
//! PG와 달리 MySQL은 카탈로그가 information_schema 하나로 모인다 —
//! pg 카탈로그를 따로 팔 이유가 없다. "스키마"는 곧 데이터베이스이고,
//! URL에 데이터베이스가 있으면 그것만, 없으면 시스템 스키마를 뺀 전부를 읽는다.

use sqlx::mysql::{MySqlConnectOptions, MySqlPool, MySqlPoolOptions};
use sqlx::Row;
use std::collections::{BTreeMap, BTreeSet};
use std::str::FromStr;

use crate::document::*;
use crate::SourceError;

/// `mysql://…` URL로 접속해 카탈로그를 읽는다.
pub async fn read(url: &str) -> Result<CatalogDocument, SourceError> {
    let options = MySqlConnectOptions::from_str(url)
        .map_err(|e| SourceError::Connect(format!("mysql URL 해석 실패: {e}")))?;

    // URL에 데이터베이스가 있으면 그것만 — 없으면 사용자 스키마 전부.
    let scope_db = options.get_database().map(|d| d.to_owned());

    let pool = MySqlPoolOptions::new()
        .after_connect(|conn, _| {
            Box::pin(async move {
                // 읽기 전용 세션 고정 — MySQL 8은 transaction_read_only,
                // MariaDB는 tx_read_only라 둘 다 시도한다. 둘 다 실패해도
                // 스캔은 계속하되 세션이 쓰기 가능이라는 사실은 기록해 둔다.
                let mysql8 =
                    sqlx::Executor::execute(&mut *conn, "SET SESSION transaction_read_only = 1")
                        .await;
                if mysql8.is_err() {
                    let _ =
                        sqlx::Executor::execute(&mut *conn, "SET SESSION tx_read_only = 1").await;
                }
                Ok(())
            })
        })
        .connect_with(options)
        .await
        .map_err(|e| SourceError::Connect(format!("mysql 접속 실패: {e}")))?;

    let mut doc = CatalogDocument {
        context: None,
        dependencies: Vec::new(),
        version: DOCUMENT_VERSION,
        dialect: "mysql".to_owned(),
        reader: "native-sqlx".to_owned(),
        schemas: Vec::new(),
        limitations: Vec::new(),
    };

    let mut catalog_complete = true;
    for schema in schema_list(&pool, scope_db.as_deref()).await? {
        doc.schemas
            .push(read_schema(&pool, &schema, &mut doc.limitations, &mut catalog_complete).await?);
    }
    doc.schemas.sort_by(|a, b| a.name.cmp(&b.name));
    doc.context = Some(CollectionContext {
        source_id: String::new(),
        database: scope_db.clone(),
        schema_filter: scope_db.map(|name| vec![name]),
        catalog_complete,
    });
    Ok(doc)
}

/// 읽을 스키마 목록 — URL의 데이터베이스가 있으면 그 하나, 없으면
/// 시스템 스키마(mysql·sys·information_schema·performance_schema)를 뺀 전부.
async fn schema_list(pool: &MySqlPool, only: Option<&str>) -> Result<Vec<String>, SourceError> {
    if let Some(db) = only {
        return Ok(vec![db.to_owned()]);
    }
    let rows = sqlx::query(
        "SELECT CAST(schema_name AS CHAR) AS name FROM information_schema.schemata \
         WHERE schema_name NOT IN ('mysql','sys','information_schema','performance_schema') \
         ORDER BY schema_name",
    )
    .fetch_all(pool)
    .await
    .map_err(SourceError::Query)?;
    Ok(rows.iter().map(|r| r.get::<String, _>("name")).collect())
}

async fn read_schema(
    pool: &MySqlPool,
    schema: &str,
    limitations: &mut Vec<String>,
    catalog_complete: &mut bool,
) -> Result<SchemaDoc, SourceError> {
    let metadata_start = limitations.len();
    let mut objects = read_objects(pool, schema).await?;
    let mut expr_index_cols = 0usize;
    for obj in &mut objects {
        obj.columns = read_columns(pool, schema, &obj.name).await?;
        obj.constraints = read_constraints(pool, schema, &obj.name).await?;
        let (indexes, expr_cols) = read_indexes(pool, schema, &obj.name).await?;
        obj.indexes = indexes;
        expr_index_cols += expr_cols;
        obj.triggers = read_triggers(pool, schema, &obj.name).await?;
    }
    if expr_index_cols > 0 {
        limitations.push(format!(
            "{schema}: 식(functional) 인덱스 컬럼 {expr_index_cols}개는 이름이 없어 생략됨"
        ));
    }
    let mut routines = read_routines(pool, schema, limitations).await?;
    *catalog_complete &= limitations.len() == metadata_start;
    attach_usage(pool, schema, &mut objects, limitations).await;
    sort_all(&mut objects, &mut routines);
    Ok(SchemaDoc {
        name: schema.to_owned(),
        objects,
        routines,
    })
}

/// 사용 통계 — performance_schema·sys는 서버 재시작에 리셋된다. sys 스키마가
/// 없는 설치도 있어 실패해도 스캔을 죽이지 않고 limitation으로 신고한다.
async fn attach_usage(
    pool: &MySqlPool,
    schema: &str,
    objects: &mut [ObjectDoc],
    limitations: &mut Vec<String>,
) {
    // MariaDB는 performance_schema=OFF가 기본값이다 — sys 통계 뷰는 쿼리는
    // 되지만 0행이라, 꺼져 있으면 그 이유를 limitation으로 남기고 수확을
    // 건너뛴다(0행을 "관측된 0"으로 오독하지 않기 위해).
    let pfs: Option<String> = sqlx::query_scalar("SELECT CAST(@@performance_schema AS CHAR)")
        .fetch_one(pool)
        .await
        .ok();
    if matches!(pfs.as_deref(), Some("0") | Some("OFF")) {
        limitations
            .push("performance_schema=OFF — usage 미수집(통계 비활성, MariaDB 기본값)".to_owned());
        return;
    }

    // 통계의 유효 시작점 — performance_schema는 재시작에 리셋된다.
    let since: Option<String> = sqlx::query_scalar::<_, String>(
        "SELECT CAST(NOW() - INTERVAL VARIABLE_VALUE SECOND AS CHAR) \
         FROM performance_schema.global_status WHERE VARIABLE_NAME='Uptime'",
    )
    .fetch_optional(pool)
    .await
    .ok()
    .flatten();

    // `reads`는 MySQL 예약어라 별칭을 백틱으로 감싼다.
    let rows = sqlx::query(
        "SELECT CAST(table_name AS CHAR) AS name, \
         CAST(rows_fetched AS SIGNED) AS `reads`, \
         CAST(rows_inserted + rows_updated + rows_deleted AS SIGNED) AS `writes` \
         FROM sys.schema_table_statistics WHERE table_schema = ?",
    )
    .bind(schema)
    .fetch_all(pool)
    .await;
    match rows {
        Ok(rows) => {
            for r in &rows {
                let name: String = r.get("name");
                if let Some(obj) = objects.iter_mut().find(|o| o.name == name) {
                    obj.usage = Some(UsageDoc {
                        since: since.clone(),
                        reads: r.get::<i64, _>("reads").max(0) as u64,
                        writes: r.get::<i64, _>("writes").max(0) as u64,
                        scans: None,
                        total_ms: None,
                        self_ms: None,
                    });
                }
            }
        }
        Err(e) => limitations.push(format!(
            "{schema}: sys.schema_table_statistics 미수확 — usage 증거 없음: {e}"
        )),
    }

    // 재시작 이후 한 번도 안 쓰인 인덱스 — 올라 있지 않은 인덱스의 사용량은
    // 모르는 것이지 0이 아니라서, 명단에 있는 것만 0 관측으로 싣는다.
    let rows = sqlx::query(
        "SELECT CAST(object_name AS CHAR) AS obj, CAST(index_name AS CHAR) AS idx \
         FROM sys.schema_unused_indexes WHERE object_schema = ?",
    )
    .bind(schema)
    .fetch_all(pool)
    .await;
    match rows {
        Ok(rows) => {
            for r in &rows {
                let (table, index): (String, String) = (r.get("obj"), r.get("idx"));
                if let Some(idx) = objects
                    .iter_mut()
                    .find(|o| o.name == table)
                    .and_then(|o| o.indexes.iter_mut().find(|i| i.name == index))
                {
                    idx.usage = Some(UsageDoc {
                        since: since.clone(),
                        reads: 0,
                        writes: 0,
                        scans: None,
                        total_ms: None,
                        self_ms: None,
                    });
                }
            }
        }
        Err(e) => limitations.push(format!(
            "{schema}: sys.schema_unused_indexes 미수확 — index usage 증거 없음: {e}"
        )),
    }
}

/// table·view·sequence 목록 — TABLE_TYPE은 'BASE TABLE'|'VIEW'이고,
/// MariaDB는 'SEQUENCE'도 여기 보고한다(MySQL엔 시퀀스가 없다).
async fn read_objects(pool: &MySqlPool, schema: &str) -> Result<Vec<ObjectDoc>, SourceError> {
    let rows = sqlx::query(
        "SELECT CAST(table_name AS CHAR) AS name, CAST(table_type AS CHAR) AS kind FROM information_schema.tables \
         WHERE table_schema = ?",
    )
    .bind(schema)
    .fetch_all(pool)
    .await
    .map_err(SourceError::Query)?;

    let mut objects: Vec<ObjectDoc> = rows
        .iter()
        .map(|r| {
            let raw_kind = r.get::<String, _>("kind");
            let kind = match raw_kind.as_str() {
                "BASE TABLE" | "SYSTEM VERSIONED" => "table",
                "VIEW" => "view",
                "SEQUENCE" => "sequence",
                other => other,
            };
            ObjectDoc {
                name: r.get("name"),
                kind: kind.to_owned(),
                columns: vec![],
                constraints: vec![],
                indexes: vec![],
                triggers: vec![],
                body: None,
                usage: None,
            }
        })
        .collect();

    // view 정의 원문 — 파싱은 엔진의 일, reader는 옮기기만 한다.
    let views = sqlx::query(
        "SELECT CAST(table_name AS CHAR) AS name, CAST(view_definition AS CHAR) AS def FROM information_schema.views \
         WHERE table_schema = ?",
    )
    .bind(schema)
    .fetch_all(pool)
    .await
    .map_err(SourceError::Query)?;
    for r in &views {
        let name: String = r.get("name");
        if let Some(obj) = objects.iter_mut().find(|o| o.name == name) {
            obj.body = r.get::<Option<String>, _>("def");
        }
    }
    Ok(objects)
}

async fn read_columns(
    pool: &MySqlPool,
    schema: &str,
    table: &str,
) -> Result<Vec<ColumnDoc>, SourceError> {
    let rows = sqlx::query(
        "SELECT CAST(column_name AS CHAR) AS name, CAST(data_type AS CHAR) AS data_type, CAST(is_nullable AS CHAR) AS nullable, \
                CAST(column_default AS CHAR) AS default_value, CAST(ordinal_position AS SIGNED) AS ordinal \
         FROM information_schema.columns \
         WHERE table_schema = ? AND table_name = ? ORDER BY ordinal_position",
    )
    .bind(schema)
    .bind(table)
    .fetch_all(pool)
    .await
    .map_err(SourceError::Query)?;

    // PK 안에서의 위치는 columns에는 없고 key_column_usage에 있다.
    // ordinal_position은 MySQL에선 UNSIGNED, MariaDB에선 SIGNED로 온다 —
    // SIGNED로 캐스트해 둘을 같은 디코딩으로 맞춘다.
    let pk_rows = sqlx::query(
        "SELECT CAST(column_name AS CHAR) AS name, CAST(ordinal_position AS SIGNED) AS pos FROM information_schema.key_column_usage \
         WHERE table_schema = ? AND table_name = ? AND constraint_name = 'PRIMARY'",
    )
    .bind(schema)
    .bind(table)
    .fetch_all(pool)
    .await
    .map_err(SourceError::Query)?;
    let pk_pos: BTreeMap<String, u32> = pk_rows
        .iter()
        .map(|r| (r.get("name"), r.get::<i64, _>("pos") as u32))
        .collect();

    Ok(rows
        .iter()
        .map(|r| {
            let name: String = r.get("name");
            ColumnDoc {
                pk_position: pk_pos.get(&name).copied().unwrap_or(0),
                name,
                data_type: r.get("data_type"),
                nullable: r.get::<String, _>("nullable") == "YES",
                default: r.get::<Option<String>, _>("default_value"),
                ordinal: r.get::<i64, _>("ordinal") as u32,
            }
        })
        .collect())
}

/// PK·FK·UNIQUE·CHECK 제약 — 로컬 컬럼과 FK 대상은 key_column_usage가
/// 행마다 referenced_*를 직접 준다(컬럼 대응은 PG만큼 정확하다).
/// CHECK 절 원문은 ConstraintDoc에 담을 자리가 없어 제약의 존재만 기록한다 —
/// document 모델이 절을 담게 되면 check_constraints를 읽어 붙인다.
async fn read_constraints(
    pool: &MySqlPool,
    schema: &str,
    table: &str,
) -> Result<Vec<ConstraintDoc>, SourceError> {
    let rows = sqlx::query(
        "SELECT CAST(constraint_name AS CHAR) AS name, CAST(constraint_type AS CHAR) AS kind \
         FROM information_schema.table_constraints \
         WHERE table_schema = ? AND table_name = ? \
           AND constraint_type IN ('PRIMARY KEY','FOREIGN KEY','UNIQUE','CHECK')",
    )
    .bind(schema)
    .bind(table)
    .fetch_all(pool)
    .await
    .map_err(SourceError::Query)?;

    let mut constraints = Vec::new();
    for r in &rows {
        let con_name: String = r.get("name");
        let con_type: String = r.get("kind");

        // 컬럼 목록과 FK 대상을 한 쿼리로 — CHECK는 key_column_usage에 행이
        // 없어(MySQL 8.0.16+는 절만 기록한다) 컬럼이 비는데, 그것이 정직하다:
        // 절 안의 컬럼을 reader가 추측해 채우지 않는다.
        let cols = sqlx::query(
            "SELECT CAST(column_name AS CHAR) AS name, ordinal_position AS pos, \
                    CAST(referenced_table_schema AS CHAR) AS ref_schema, \
                    CAST(referenced_table_name AS CHAR) AS ref_table, \
                    CAST(referenced_column_name AS CHAR) AS ref_column \
             FROM information_schema.key_column_usage \
             WHERE table_schema = ? AND table_name = ? AND constraint_name = ? \
             ORDER BY ordinal_position",
        )
        .bind(schema)
        .bind(table)
        .bind(&con_name)
        .fetch_all(pool)
        .await
        .map_err(SourceError::Query)?;

        let columns: Vec<String> = cols.iter().map(|c| c.get("name")).collect();
        let referenced = if con_type == "FOREIGN KEY" {
            let first = cols.first();
            first.map(|c| ReferencedDoc {
                schema: c.get::<Option<String>, _>("ref_schema"),
                table: c.get::<String, _>("ref_table"),
                columns: cols
                    .iter()
                    .map(|c| c.get::<Option<String>, _>("ref_column"))
                    .collect::<Option<Vec<String>>>()
                    .unwrap_or_default(),
            })
        } else {
            None
        };

        constraints.push(ConstraintDoc {
            name: con_name.clone(),
            kind: match con_type.as_str() {
                "PRIMARY KEY" => "pk",
                "FOREIGN KEY" => "fk",
                "UNIQUE" => "unique",
                _ => "check",
            }
            .to_owned(),
            columns,
            referenced,
        });
    }
    Ok(constraints)
}

/// 인덱스 — PRIMARY는 제약이지 인덱스로 세지 않는다. 반환값 둘째는
/// 컬럼명이 NULL인 식 인덱스 컬럼 수(한계 보고용).
async fn read_indexes(
    pool: &MySqlPool,
    schema: &str,
    table: &str,
) -> Result<(Vec<IndexDoc>, usize), SourceError> {
    let rows = sqlx::query(
        "SELECT CAST(index_name AS CHAR) AS name, non_unique AS non_unique, seq_in_index AS seq, CAST(column_name AS CHAR) AS col \
         FROM information_schema.statistics \
         WHERE table_schema = ? AND table_name = ? AND index_name <> 'PRIMARY' \
         ORDER BY index_name, seq_in_index",
    )
    .bind(schema)
    .bind(table)
    .fetch_all(pool)
    .await
    .map_err(SourceError::Query)?;

    let mut by_name: BTreeMap<String, (bool, Vec<String>, bool)> = BTreeMap::new();
    let mut expr_cols = 0usize;
    for r in &rows {
        let name: String = r.get("name");
        let entry = by_name
            .entry(name)
            .or_insert_with(|| (r.get::<i64, _>("non_unique") == 0, Vec::new(), true));
        match r.get::<Option<String>, _>("col") {
            // MySQL 8 functional 인덱스는 column_name이 NULL이다 — 식을
            // 컬럼으로 위장하지 않고 개수만 센다.
            Some(col) => entry.1.push(col),
            None => {
                entry.2 = false;
                expr_cols += 1;
            }
        }
    }
    Ok((
        by_name
            .into_iter()
            .map(|(name, (unique, columns, complete))| IndexDoc {
                has_predicate: None,
                definition_complete: Some(complete),
                predicate: None,
                name,
                unique,
                columns,
                usage: None,
            })
            .collect(),
        expr_cols,
    ))
}

/// 사용자 트리거 — ACTION_STATEMENT가 몸체 원문이다. MySQL은
/// CREATE TRIGGER 전문을 보관하지 않아 몸체만 온다 — 파서가
/// 껍질 없는 몸체를 통째로 파싱하는 경로가 그대로 쓰인다.
async fn read_triggers(
    pool: &MySqlPool,
    schema: &str,
    table: &str,
) -> Result<Vec<TriggerDoc>, SourceError> {
    let rows = sqlx::query(
        "SELECT CAST(trigger_name AS CHAR) AS name, CAST(action_statement AS CHAR) AS def FROM information_schema.triggers \
         WHERE trigger_schema = ? AND event_object_table = ?",
    )
    .bind(schema)
    .bind(table)
    .fetch_all(pool)
    .await
    .map_err(SourceError::Query)?;
    Ok(rows
        .iter()
        .map(|r| TriggerDoc {
            name: r.get("name"),
            body: r.get::<Option<String>, _>("def"),
        })
        .collect())
}

/// function/procedure — MySQL은 오버로딩이 없어 시그니처를 인자 타입
/// 목록으로만 채운다(정점 id가 `name(int,text)` 꼴로 정보를 담는다).
/// ROUTINE_DEFINITION은 권한이 없으면 NULL이라, 전부 비면 limitation.
async fn read_routines(
    pool: &MySqlPool,
    schema: &str,
    limitations: &mut Vec<String>,
) -> Result<Vec<RoutineDoc>, SourceError> {
    let rows = sqlx::query(
        "SELECT CAST(routine_name AS CHAR) AS name, CAST(routine_type AS CHAR) AS kind, CAST(routine_definition AS CHAR) AS def \
         FROM information_schema.routines WHERE routine_schema = ?",
    )
    .bind(schema)
    .fetch_all(pool)
    .await
    .map_err(SourceError::Query)?;

    let mut routines = Vec::new();
    let mut null_bodies = 0usize;
    let mut seen_ids = BTreeSet::new();
    for r in &rows {
        let name: String = r.get("name");
        let raw_type: String = r.get("kind");
        let body = r.get::<Option<String>, _>("def");
        if body.is_none() {
            null_bodies += 1;
        }
        let signature = routine_signature(pool, schema, &name).await?;
        let id_name = match &signature {
            Some(sig) if !sig.is_empty() => format!("{name}({sig})"),
            _ => name.clone(),
        };
        // MySQL은 같은 이름의 FUNCTION과 PROCEDURE가 공존할 수 있다 —
        // 시그니처까지 같으면 정점 id가 충돌해 둘이 하나로 합쳐진다.
        // 숨기지 않고 limitation으로 보고한다.
        if !seen_ids.insert(id_name) {
            limitations.push(format!(
                "{schema}.{name}: function과 procedure의 정점 id가 충돌해 한 정점으로 합쳐짐"
            ));
        }
        routines.push(RoutineDoc {
            source: None,
            name,
            kind: if raw_type == "PROCEDURE" {
                "procedure"
            } else {
                "function"
            }
            .to_owned(),
            // MySQL 저장 루틴은 언어가 SQL뿐이다 — 카탈로그에 언어 열이 없어
            // "몸체는 SQL이다"는 제품 지식으로 채운다.
            language: Some("sql".to_owned()),
            body,
            signature,
            usage: None,
            member_of: None,
        });
    }
    if null_bodies > 0 {
        limitations.push(format!(
            "{schema}: routine 몸체 {null_bodies}개를 읽지 못함(ROUTINE_DEFINITION NULL — 권한 확인)"
        ));
    }
    Ok(routines)
}

/// 인자 타입 목록을 "int,varchar(20)" 꼴로 — routine의 signature 필드다.
/// MySQL 함수 인자는 parameter_mode가 NULL이고 프로시저는 IN/OUT/INOUT.
/// ordinal_position=0은 인자가 아니라 함수의 반환값 행이라 뺀다.
async fn routine_signature(
    pool: &MySqlPool,
    schema: &str,
    specific_name: &str,
) -> Result<Option<String>, SourceError> {
    let rows = sqlx::query(
        "SELECT CAST(dtd_identifier AS CHAR) AS dtd FROM information_schema.parameters \
         WHERE specific_schema = ? AND specific_name = ? AND ordinal_position > 0 \
         ORDER BY ordinal_position",
    )
    .bind(schema)
    .bind(specific_name)
    .fetch_all(pool)
    .await
    .map_err(SourceError::Query)?;
    let types: Vec<String> = rows.iter().map(|r| r.get("dtd")).collect();
    Ok(if types.is_empty() {
        None
    } else {
        Some(types.join(","))
    })
}

/// 컬렉션 정렬 — 결정적 출력 계약.
fn sort_all(objects: &mut [ObjectDoc], routines: &mut [RoutineDoc]) {
    objects.sort_by(|a, b| a.name.cmp(&b.name));
    routines.sort_by(|a, b| a.name.cmp(&b.name).then(a.signature.cmp(&b.signature)));
    for obj in objects.iter_mut() {
        obj.columns.sort_by_key(|c| c.ordinal);
        obj.constraints.sort_by(|a, b| a.name.cmp(&b.name));
        obj.indexes.sort_by(|a, b| a.name.cmp(&b.name));
        obj.triggers.sort_by(|a, b| a.name.cmp(&b.name));
    }
}
