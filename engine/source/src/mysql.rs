//! MySQL·MariaDB 네이티브 reader — information_schema를 읽어
//! CatalogDocument를 만든다.
//!
//! PG와 달리 MySQL은 카탈로그가 information_schema 하나로 모인다 —
//! pg 카탈로그를 따로 팔 이유가 없다. "스키마"는 곧 데이터베이스이고,
//! URL에 데이터베이스가 있으면 그것만, 없으면 시스템 스키마를 뺀 전부를 읽는다.

use sqlx::mysql::{
    MySqlConnectOptions, MySqlConnection, MySqlPool, MySqlPoolOptions, MySqlSslMode,
};
use sqlx::Row;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Display;
use std::str::FromStr;
use std::sync::{Arc, OnceLock};

use crate::document::*;
use crate::transport::{is_local_endpoint, ConnectMode, TlsPolicy, Transport, TransportCheck};
use crate::SourceError;

/// 평문 원격 연결 limitation의 해결 방향. 공개 CA가 아닌 관리형 DB는 CA 파일이 필요하다.
const TLS_REMEDY: &str = "use ssl-mode=VERIFY_IDENTITY (with ssl-ca=<CA file> for a private CA) \
                          to require a verified TLS connection";

/// 읽기 전용 세션을 고정하지 못했을 때의 한계. Go 프로브와 같은 문구로 시작해 수집기와
/// 무관하게 같은 사실로 읽힌다. 네이티브 reader는 그 뒤 괄호 안에 원인을 덧붙인다.
const READ_ONLY_UNAVAILABLE: &str =
    "read-only session mode was unavailable; only catalog queries were issued";

/// 읽기 전용 세션을 고정하는 문장. MySQL 8은 transaction_read_only, MariaDB는
/// tx_read_only라 이 순서로 시도한다.
const READ_ONLY_STATEMENTS: [&str; 2] = [
    "SET SESSION transaction_read_only = 1",
    "SET SESSION tx_read_only = 1",
];

/// `mysql://…` URL로 접속해 카탈로그를 읽는다.
pub async fn read(url: &str) -> Result<CatalogDocument, SourceError> {
    let options = MySqlConnectOptions::from_str(url)
        .map_err(|e| SourceError::Connect(format!("mysql URL could not be parsed: {e}")))?;

    // URL에 데이터베이스가 있으면 그것만 — 없으면 사용자 스키마 전부.
    let scope_db = options.get_database().map(|d| d.to_owned());

    // after_connect는 연결마다 돈다 — 어느 연결이든 고정에 실패하면 문서 전체의 한계다.
    // 원인은 처음 실패한 연결의 것을 남긴다.
    let read_only_cause = Arc::new(OnceLock::<String>::new());
    let (pool, transport) = connect(&options, &read_only_cause)
        .await
        .map_err(|e| SourceError::Connect(format!("mysql connection failed: {e}")))?;

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
    record_read_only_failure(&mut doc, read_only_cause.get().map(String::as_str));
    transport.record_limitation(&mut doc, TLS_REMEDY);
    Ok(doc)
}

/// URL의 ssl-mode를 전송 판정의 세 갈래로 접는다.
fn tls_policy(mode: MySqlSslMode) -> TlsPolicy {
    match mode {
        MySqlSslMode::Disabled => TlsPolicy::Never,
        MySqlSslMode::Preferred => TlsPolicy::Prefer,
        MySqlSslMode::Required | MySqlSslMode::VerifyCa | MySqlSslMode::VerifyIdentity => {
            TlsPolicy::Required
        }
    }
}

/// 접속 옵션으로 전송 판정을 준비한다. sqlx는 socket 인자가 있으면 Unix 소켓으로 붙는다.
fn transport_check(options: &MySqlConnectOptions) -> TransportCheck {
    let uses_socket = options.get_socket().is_some();
    TransportCheck::new(
        is_local_endpoint(options.get_host(), uses_socket),
        tls_policy(options.get_ssl_mode()),
    )
}

/// 풀을 열고 카탈로그를 읽을 연결의 전송을 함께 돌려준다. 네트워크를 지나는 PREFERRED
/// 연결은 REQUIRED로 먼저 열고, 서버가 TLS를 거절했을 때만 DISABLED로 다시 연다
/// (TransportCheck::open). 거절된 시도는 after_connect에 닿기 전에 끝나므로, 읽기 전용 고정
/// 실패 원인은 실제로 쓰는 풀의 연결에서만 남는다.
async fn connect(
    options: &MySqlConnectOptions,
    read_only_cause: &Arc<OnceLock<String>>,
) -> Result<(MySqlPool, Transport), sqlx::Error> {
    transport_check(options)
        .open(|mode| {
            pool_options(Arc::clone(read_only_cause)).connect_with(options_for(options, mode))
        })
        .await
}

/// 여는 방식을 ssl-mode로 옮긴다. REQUIRED는 PREFERRED처럼 인증서를 검증하지 않으므로,
/// 서버가 TLS를 받는 한 sqlx 자체 PREFERRED와 같은 연결이다. ssl-mode 말고는 URL 그대로다.
fn options_for(options: &MySqlConnectOptions, mode: ConnectMode) -> MySqlConnectOptions {
    let options = options.clone();
    match mode {
        ConnectMode::AsGiven => options,
        ConnectMode::RequireTls => options.ssl_mode(MySqlSslMode::Required),
        ConnectMode::Plaintext => options.ssl_mode(MySqlSslMode::Disabled),
    }
}

/// 연결마다 세션을 읽기 전용으로 고정하는 풀 설정. 고정 실패 원인은 처음 실패한 연결의 것을
/// `first_cause`에 남긴다.
fn pool_options(first_cause: Arc<OnceLock<String>>) -> MySqlPoolOptions {
    MySqlPoolOptions::new().after_connect(move |conn, _| {
        let first_cause = Arc::clone(&first_cause);
        Box::pin(async move {
            // 둘 다 실패해도 스캔은 계속하되(reader는 카탈로그 SELECT만 보낸다)
            // 세션이 쓰기 가능이라는 사실과 그 원인은 limitation으로 남긴다.
            if let Some(cause) = pin_read_only_session(conn).await {
                first_cause.get_or_init(|| cause);
            }
            Ok(())
        })
    })
}

/// 읽기 전용 세션을 고정한다. 처음 성공한 문장에서 멈추고, 모두 거절되면 원인을 돌려준다.
async fn pin_read_only_session(conn: &mut MySqlConnection) -> Option<String> {
    let mut attempts = Vec::new();
    for statement in READ_ONLY_STATEMENTS {
        let attempt = sqlx::Executor::execute(&mut *conn, statement).await;
        let pinned = attempt.is_ok();
        attempts.push((statement, attempt));
        if pinned {
            break;
        }
    }
    read_only_failure(&attempts)
}

/// 연결 하나의 SET 시도 결과에서 고정 실패 원인을 만든다. 하나라도 성공했으면 세션은
/// 고정됐다(None). 모두 거절되면 문장마다 오류를 남긴다. 변수 미지원·권한 거부·연결
/// 오류는 해결 방향이 달라 사용자가 구분할 수 있어야 하기 때문이다.
fn read_only_failure<T, E: Display>(attempts: &[(&str, Result<T, E>)]) -> Option<String> {
    if attempts.iter().any(|(_, attempt)| attempt.is_ok()) {
        return None;
    }
    let causes: Vec<String> = attempts
        .iter()
        .filter_map(|(statement, attempt)| {
            attempt
                .as_ref()
                .err()
                .map(|error| format!("{statement}: {error}"))
        })
        .collect();
    Some(causes.join("; "))
}

/// 읽기 전용 세션 고정 실패를 문서에 남긴다. Go 프로브처럼 접속 단계의 사실이라
/// limitations 맨 앞에 둔다. 수집한 카탈로그 자체는 온전하므로 catalog_complete는
/// 건드리지 않는다(Go 프로브와 같다). 실패가 없으면 문서를 바꾸지 않는다.
fn record_read_only_failure(doc: &mut CatalogDocument, cause: Option<&str>) {
    if let Some(cause) = cause {
        doc.limitations
            .insert(0, format!("{READ_ONLY_UNAVAILABLE} ({cause})"));
    }
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
            "{schema}: omitted {expr_index_cols} functional index columns because they have no names"
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
            .push("performance_schema=OFF — usage unavailable because statistics are disabled (MariaDB default)".to_owned());
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
            "{schema}: sys.schema_table_statistics unavailable; no usage evidence — {e}"
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
            "{schema}: sys.schema_unused_indexes unavailable; no index usage evidence — {e}"
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
                "{schema}.{name}: function and procedure share a vertex id and will be merged"
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
            "{schema}: {null_bodies} routine bodies were unavailable (ROUTINE_DEFINITION is NULL; check permissions)"
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

#[cfg(test)]
mod tests {
    use super::*;

    /// 한 문장이라도 성공하면 세션은 고정됐다. 모두 거절되면 문장마다 오류를 시도 순서대로
    /// 남긴다 — 어느 변수가 왜 거절됐는지가 해결 방향을 정한다.
    #[test]
    fn read_only_failure_keeps_each_rejection_until_a_statement_pins_the_session() {
        let pinned: [&[(&str, Result<(), &str>)]; 2] = [
            &[("SET a", Ok(()))],
            &[("SET a", Err("unknown variable")), ("SET b", Ok(()))],
        ];
        for attempts in pinned {
            assert_eq!(read_only_failure(attempts), None, "{attempts:?}");
        }
        let rejected: &[(&str, Result<(), &str>)] = &[
            ("SET a", Err("unknown variable")),
            ("SET b", Err("access denied")),
        ];
        assert_eq!(
            read_only_failure(rejected).as_deref(),
            Some("SET a: unknown variable; SET b: access denied")
        );
    }

    /// 수집을 마친 문서. 다른 한계와 완전성 표시가 있어야 기록이 그것을 건드리는지 보인다.
    fn collected_document(catalog_complete: bool) -> CatalogDocument {
        CatalogDocument {
            version: DOCUMENT_VERSION,
            dialect: "mysql".into(),
            reader: "native-sqlx".into(),
            schemas: vec![SchemaDoc {
                name: "app".into(),
                objects: vec![],
                routines: vec![],
            }],
            limitations: vec!["performance_schema=OFF".into()],
            context: Some(CollectionContext {
                source_id: String::new(),
                database: Some("app".into()),
                schema_filter: Some(vec!["app".into()]),
                catalog_complete,
            }),
            dependencies: vec![],
        }
    }

    /// 고정 실패는 Go 프로브와 같은 문구로 시작하는 한계로 맨 앞에 서고, 원인은 괄호에 실린다.
    /// 다른 한계·스키마·catalog_complete는 그대로다. 실패가 없으면 문서가 바뀌지 않는다.
    #[test]
    fn read_only_failure_is_the_first_limitation_and_keeps_catalog_completeness() {
        for catalog_complete in [true, false] {
            let collected = collected_document(catalog_complete);
            let mut doc = collected.clone();
            record_read_only_failure(&mut doc, None);
            assert_eq!(doc, collected);
            record_read_only_failure(&mut doc, Some("SET SESSION tx_read_only = 1: denied"));
            assert_eq!(
                doc.limitations,
                [
                    "read-only session mode was unavailable; only catalog queries were issued \
                     (SET SESSION tx_read_only = 1: denied)",
                    "performance_schema=OFF",
                ]
            );
            assert_eq!(doc.context, collected.context);
            assert_eq!(doc.schemas, collected.schemas);
        }
    }

    /// URL의 ssl-mode를 판정의 세 갈래로 접는다. 값은 대소문자를 가리지 않는다(sqlx).
    #[test]
    fn ssl_modes_map_to_tls_policies() {
        for (mode, policy) in [
            ("DISABLED", TlsPolicy::Never),
            ("PREFERRED", TlsPolicy::Prefer),
            ("REQUIRED", TlsPolicy::Required),
            ("VERIFY_CA", TlsPolicy::Required),
            ("verify_identity", TlsPolicy::Required),
        ] {
            let url = format!("mysql://u@db.example/app?ssl-mode={mode}");
            let options = MySqlConnectOptions::from_str(&url).unwrap();
            assert_eq!(tls_policy(options.get_ssl_mode()), policy, "{mode}");
        }
    }

    /// TLS를 요구해 먼저 여는 것은 네트워크를 지나는 PREFERRED(기본) 연결뿐이다. socket 인자는
    /// Unix 소켓이다.
    #[test]
    fn only_remote_preferred_connections_negotiate_tls() {
        for (url, negotiates) in [
            ("mysql://u@127.0.0.1/app", false),
            ("mysql://u@localhost/app", false),
            ("mysql://u@[::1]/app", false),
            ("mysql://u@db.example/app?socket=/tmp/mysql.sock", false),
            ("mysql://u@db.example/app", true),
            ("mysql://u@db.example/app?ssl-mode=DISABLED", false),
            ("mysql://u@db.example/app?ssl-mode=REQUIRED", false),
        ] {
            let options = MySqlConnectOptions::from_str(url).unwrap();
            assert_eq!(
                transport_check(&options).negotiates_tls(),
                negotiates,
                "{url}"
            );
        }
    }

    /// 여는 방식은 ssl-mode만 바꾸고 접속 대상과 다른 인자는 URL 그대로 둔다. MySqlSslMode에는
    /// 비교 연산이 없어 Debug 표기로 본다.
    #[test]
    fn connect_modes_change_only_the_ssl_mode() {
        let options =
            MySqlConnectOptions::from_str("mysql://u@db.example:3307/app?ssl-ca=/etc/ca.pem")
                .unwrap();
        for (mode, ssl_mode) in [
            (ConnectMode::AsGiven, "Preferred"),
            (ConnectMode::RequireTls, "Required"),
            (ConnectMode::Plaintext, "Disabled"),
        ] {
            let opened = options_for(&options, mode);
            assert_eq!(format!("{:?}", opened.get_ssl_mode()), ssl_mode, "{mode:?}");
            assert_eq!(opened.get_host(), "db.example");
            assert_eq!(opened.get_port(), 3307);
            assert_eq!(opened.get_database(), Some("app"));
        }
    }

    /// 원격 평문 연결의 limitation은 MySQL URL 인자로 해결 방향을 말하고, 읽기 전용 고정 실패보다
    /// 앞에 선다(접속 단계 순서). 모드로 끈 평문과 서버가 TLS를 거절한 평문은 원인 문구로 갈린다.
    #[test]
    fn remote_plaintext_limitation_precedes_the_read_only_failure() {
        let first_limitation = |transport: Transport| {
            let mut doc = collected_document(true);
            record_read_only_failure(&mut doc, Some("denied"));
            transport.record_limitation(&mut doc, TLS_REMEDY);
            assert!(doc.limitations[1].starts_with(READ_ONLY_UNAVAILABLE));
            assert_eq!(doc.context, collected_document(true).context);
            doc.limitations[0].clone()
        };
        assert_eq!(
            first_limitation(Transport::Plaintext),
            "connection-not-encrypted: the catalog was read over an unencrypted connection; \
             use ssl-mode=VERIFY_IDENTITY (with ssl-ca=<CA file> for a private CA) to require \
             a verified TLS connection"
        );
        assert_eq!(
            first_limitation(Transport::TlsRefused),
            "connection-not-encrypted: the server did not accept TLS, so the catalog was read \
             over an unencrypted connection; use ssl-mode=VERIFY_IDENTITY (with ssl-ca=<CA \
             file> for a private CA) to require a verified TLS connection"
        );
    }
}
