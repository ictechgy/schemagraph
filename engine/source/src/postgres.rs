//! PostgreSQL 네이티브 reader — pg 카탈로그를 읽어 CatalogDocument를 만든다.
//!
//! information_schema는 표준인 척하는 반쪽이라(FK의 컬럼 대응·trigger
//! 몸체·routine 언어가 빠진다) pg_catalog를 주로 쓴다. 세션은 읽기 전용으로
//! 고정한다 — 스캐너가 DB를 바꾸는 일은 없어야 한다(sqlite의 mode=ro와 같은 의도).

use sqlx::postgres::{PgConnectOptions, PgPool, PgPoolOptions};
use sqlx::Row;
use std::str::FromStr;

use crate::document::*;
use crate::SourceError;

/// `postgres://…` URL로 접속해 카탈로그를 읽는다.
pub async fn read(url: &str) -> Result<CatalogDocument, SourceError> {
    read_with_dependencies(url, false).await
}

/// 선택적 의존 카탈로그 조회 실패는 일반 스키마 수집을 숨기지 않고 한계로 보고한다.
pub async fn read_with_dependencies(
    url: &str,
    include_dependencies: bool,
) -> Result<CatalogDocument, SourceError> {
    let options = PgConnectOptions::from_str(url)
        .map_err(|e| SourceError::Connect(format!("postgres URL 해석 실패: {e}")))?;
    let pool = PgPoolOptions::new()
        .after_connect(|conn, _| {
            Box::pin(async move {
                // 읽기 전용 세션 고정 — sqlite reader의 read_only(true)와 같은 의도.
                sqlx::Executor::execute(
                    &mut *conn,
                    "SET SESSION CHARACTERISTICS AS TRANSACTION READ ONLY",
                )
                .await?;
                // 서버가 렌더링하는 뷰 이름을 한정해 수집 세션의 search_path 영향을 없앤다.
                sqlx::Executor::execute(conn, "SET search_path = pg_catalog")
                    .await
                    .map(|_| ())
            })
        })
        .connect_with(options)
        .await
        .map_err(|e| SourceError::Connect(format!("postgres 접속 실패: {e}")))?;

    let mut doc = CatalogDocument {
        context: None,
        dependencies: Vec::new(),
        version: DOCUMENT_VERSION,
        dialect: "postgres".to_owned(),
        reader: "native-sqlx".to_owned(),
        schemas: Vec::new(),
        limitations: Vec::new(),
    };

    let mut catalog_complete = true;
    for schema in schema_list(&pool).await? {
        doc.schemas
            .push(read_schema(&pool, &schema, &mut doc.limitations, &mut catalog_complete).await?);
    }
    doc.schemas.sort_by(|a, b| a.name.cmp(&b.name));
    let database = sqlx::query_scalar::<_, String>("SELECT current_database()")
        .fetch_one(&pool)
        .await
        .map_err(SourceError::Query)?;
    doc.context = Some(CollectionContext {
        source_id: String::new(),
        database: Some(database),
        schema_filter: None,
        catalog_complete,
    });
    if include_dependencies {
        for schema in &doc.schemas {
            match read_dependencies(&pool, &schema.name).await {
                Ok(rows) => doc.dependencies.extend(rows),
                Err(error) => {
                    doc.limitations.push(format!(
                        "pg_depend metadata unavailable for {}: {error}",
                        schema.name
                    ));
                    if let Some(context) = &mut doc.context {
                        context.catalog_complete = false;
                    }
                }
            }
        }
        doc.dependencies.sort();
        doc.dependencies.dedup();
    }
    Ok(doc)
}

async fn read_dependencies(
    pool: &PgPool,
    schema: &str,
) -> Result<Vec<CatalogDependency>, sqlx::Error> {
    let query = include_str!("sql/catalog-postgres.sql").replace(":schema", "$1");
    let rows = sqlx::query(&query).bind(schema).fetch_all(pool).await?;
    Ok(rows
        .iter()
        .map(|row| CatalogDependency {
            source: CatalogObjectRef {
                schema: row.get("source_schema"),
                name: row.get("source_name"),
                kind: row.get("source_kind"),
                signature: row.get("source_signature"),
                member: None,
                database: None,
            },
            target: CatalogObjectRef {
                schema: row.get("target_schema"),
                name: row.get("target_name"),
                kind: row.get("target_kind"),
                signature: row.get("target_signature"),
                member: row.get("target_member"),
                database: row.get("target_database"),
            },
            catalog: "pg_depend".into(),
            dependency_type: row.get("dependency_type"),
        })
        .collect())
}

/// 사용자 스키마 목록 — pg_%(시스템)과 information_schema는 제외한다.
async fn schema_list(pool: &PgPool) -> Result<Vec<String>, SourceError> {
    let rows = sqlx::query(
        "SELECT nspname FROM pg_namespace \
         WHERE nspname NOT LIKE 'pg\\_%' AND nspname <> 'information_schema' \
         ORDER BY nspname",
    )
    .fetch_all(pool)
    .await
    .map_err(SourceError::Query)?;
    Ok(rows.iter().map(|r| r.get::<String, _>("nspname")).collect())
}

async fn read_schema(
    pool: &PgPool,
    schema: &str,
    limitations: &mut Vec<String>,
    catalog_complete: &mut bool,
) -> Result<SchemaDoc, SourceError> {
    let metadata_start = limitations.len();
    let mut objects = read_objects(pool, schema).await?;
    for obj in &mut objects {
        obj.columns = read_columns(pool, schema, &obj.name).await?;
        obj.constraints = read_constraints(pool, schema, &obj.name).await?;
        obj.indexes = read_indexes(pool, schema, &obj.name).await?;
        obj.triggers = read_triggers(pool, schema, &obj.name).await?;
    }
    check_catalog_visibility(pool, schema, &objects, limitations).await;
    let mut routines = read_routines(pool, schema, limitations).await?;
    *catalog_complete &= limitations.len() == metadata_start;
    attach_usage(pool, schema, &mut objects, &mut routines, limitations).await;
    sort_all(&mut objects, &mut routines);
    Ok(SchemaDoc {
        name: schema.to_owned(),
        objects,
        routines,
    })
}

/// 권한으로 걸러진 0행을 실제 빈 스키마와 구분한다. 추정 개수는 만들지 않는다.
async fn check_catalog_visibility(
    pool: &PgPool,
    schema: &str,
    objects: &[ObjectDoc],
    limitations: &mut Vec<String>,
) {
    let query = include_str!("sql/visibility-postgres.sql").replace(":schema", "$1");
    let rows = match sqlx::query(&query).bind(schema).fetch_all(pool).await {
        Ok(rows) => rows,
        Err(error) => {
            limitations.push(format!(
                "{schema}: catalog visibility could not be checked: {error}; verify metadata permissions"
            ));
            return;
        }
    };
    let known: std::collections::BTreeMap<_, std::collections::BTreeSet<_>> = objects
        .iter()
        .map(|object| {
            (
                object.name.as_str(),
                object
                    .columns
                    .iter()
                    .map(|column| column.name.as_str())
                    .collect(),
            )
        })
        .collect();
    let mut relations = std::collections::BTreeSet::new();
    let mut columns = 0usize;
    for row in rows {
        let name: String = row.get("relation_name");
        let column: Option<String> = row.get("column_name");
        let object = known.get(name.as_str());
        if object.is_none() {
            relations.insert(name);
        }
        if let Some(column) = column {
            if !object.is_some_and(|columns| columns.contains(column.as_str())) {
                columns += 1;
            }
        }
    }
    if !relations.is_empty() || columns > 0 {
        limitations.push(format!(
            "{schema}: catalog visibility check found {} uncollected relations and {columns} uncollected columns; verify metadata permissions and reader coverage",
            relations.len()
        ));
    }
}

/// 사용 통계 — pg_stat은 stats_reset 이후만 유효하다. 카탈로그 읽기와 달리
/// 실패해도 스캔을 죽이지 않는다(증거가 빠질 뿐) — 대신 limitation으로 신고.
async fn attach_usage(
    pool: &PgPool,
    schema: &str,
    objects: &mut [ObjectDoc],
    routines: &mut [RoutineDoc],
    limitations: &mut Vec<String>,
) {
    // 통계의 유효 시작점 — 테이블별 리셋 시각은 없어 데이터베이스 리셋을 쓴다.
    // pg_stat_reset()은 DB 통계 전부를 리셋하므로 이 시각이 모든 카운터의
    // 공통 하한이다(단일 테이블 리셋은 추적 불가 — 보수적 하한).
    // 리셋된 적 없는 클러스터에선 NULL이므로 서버 기동 시각으로 폴백한다 —
    // 카운터가 기동 이전을 관측했을 리 없으니 유효한 보수적 하한이다.
    let since: Option<String> = sqlx::query_scalar::<_, String>(
        "SELECT COALESCE(stats_reset::text, pg_postmaster_start_time()::text) \
         FROM pg_stat_database WHERE datname = current_database()",
    )
    .fetch_optional(pool)
    .await
    .ok()
    .flatten();

    let rows = sqlx::query(
        "SELECT relname, seq_tup_read + COALESCE(idx_tup_fetch, 0) AS reads, \
         n_tup_ins + n_tup_upd + n_tup_del AS writes \
         FROM pg_stat_user_tables WHERE schemaname = $1",
    )
    .bind(schema)
    .fetch_all(pool)
    .await;
    match rows {
        Ok(rows) => {
            for r in &rows {
                let name: String = r.get("relname");
                if let Some(obj) = objects.iter_mut().find(|o| o.name == name) {
                    obj.usage = Some(UsageDoc {
                        since: since.clone(),
                        reads: r.get::<i64, _>("reads").max(0) as u64,
                        writes: r.get::<i64, _>("writes").max(0) as u64,
                        total_ms: None,
                        self_ms: None,
                    });
                }
            }
        }
        Err(e) => limitations.push(format!("pg_stat_user_tables 미수확 — usage 증거 없음: {e}")),
    }

    let rows = sqlx::query(
        "SELECT relname, indexrelname, idx_scan AS reads \
         FROM pg_stat_user_indexes WHERE schemaname = $1",
    )
    .bind(schema)
    .fetch_all(pool)
    .await;
    match rows {
        Ok(rows) => {
            for r in &rows {
                let (table, index): (String, String) = (r.get("relname"), r.get("indexrelname"));
                if let Some(idx) = objects
                    .iter_mut()
                    .find(|o| o.name == table)
                    .and_then(|o| o.indexes.iter_mut().find(|i| i.name == index))
                {
                    // 인덱스는 idx_scan이 "쓰였다"의 단위 — 테이블의 행 단위와
                    // 혼동하지 않게 kind별로만 비교해야 한다.
                    idx.usage = Some(UsageDoc {
                        since: since.clone(),
                        reads: r.get::<i64, _>("reads").max(0) as u64,
                        writes: 0,
                        total_ms: None,
                        self_ms: None,
                    });
                }
            }
        }
        Err(e) => limitations.push(format!(
            "pg_stat_user_indexes 미수확 — index usage 증거 없음: {e}"
        )),
    }

    // routine 호출 통계 — calls는 routine의 "reads"다(단위는 kind별로 다르다는
    // 계약). funcname은 시그니처가 없어 오버로드면 어느 것의 calls인지
    // 알 수 없다 — 둘 다에 달면 이중 집계라, 이름이 유일할 때만 귀속한다.
    // track_functions 기본값은 none — 그러면 뷰가 0행(0 호출이 아니라 미수집)이라
    // 비어 있는 이유를 limitation으로 남겨 "호출 0"으로 오독되지 않게 한다.
    let tracking: Option<String> = sqlx::query_scalar("SELECT current_setting('track_functions')")
        .fetch_one(pool)
        .await
        .ok();
    if tracking.as_deref() == Some("none") {
        limitations
            .push("track_functions=none — routine usage 미수집(함수 통계 비활성)".to_owned());
        return;
    }
    let rows = sqlx::query(
        "SELECT funcname, calls, total_time, self_time \
         FROM pg_stat_user_functions WHERE schemaname = $1",
    )
    .bind(schema)
    .fetch_all(pool)
    .await;
    match rows {
        Ok(rows) => {
            let mut ambiguous = 0usize;
            for r in &rows {
                let name: String = r.get("funcname");
                let mut hits = routines.iter_mut().filter(|rt| rt.name == name);
                match (hits.next(), hits.next()) {
                    (Some(rt), None) => {
                        // track_calls는 track_functions와 별개 스위치다 — 꺼져
                        // 있으면 시간 컬럼이 전부 NULL이라 Option으로 받는다.
                        rt.usage = Some(UsageDoc {
                            since: since.clone(),
                            reads: r.get::<i64, _>("calls").max(0) as u64,
                            writes: 0,
                            total_ms: r.get::<Option<f64>, _>("total_time"),
                            self_ms: r.get::<Option<f64>, _>("self_time"),
                        });
                    }
                    (Some(_), Some(_)) => ambiguous += 1,
                    _ => {}
                }
            }
            if ambiguous > 0 {
                limitations.push(format!(
                    "{schema}: 오버로드된 routine {ambiguous}개는 funcname만으로 \
                     귀속 못 해 usage 미수집"
                ));
            }
        }
        Err(e) => limitations.push(format!(
            "pg_stat_user_functions 미수확 — routine usage 증거 없음: {e}"
        )),
    }
}

/// table·view·materialized-view·sequence 목록.
async fn read_objects(pool: &PgPool, schema: &str) -> Result<Vec<ObjectDoc>, SourceError> {
    let mut objects: Vec<ObjectDoc> = Vec::new();

    let rows = sqlx::query(
        "SELECT table_name AS name, table_type AS kind \
         FROM information_schema.tables WHERE table_schema = $1",
    )
    .bind(schema)
    .fetch_all(pool)
    .await
    .map_err(SourceError::Query)?;
    for r in &rows {
        let raw_kind = r.get::<String, _>("kind");
        let kind = match raw_kind.as_str() {
            "BASE TABLE" | "FOREIGN" => "table",
            "VIEW" => "view",
            other => other,
        };
        objects.push(ObjectDoc {
            name: r.get("name"),
            kind: kind.to_owned(),
            columns: vec![],
            constraints: vec![],
            indexes: vec![],
            triggers: vec![],
            body: None,
            usage: None,
        });
    }

    // view 정의 원문 — 파싱은 엔진의 일, reader는 옮기기만 한다.
    let views = sqlx::query("SELECT viewname, definition FROM pg_views WHERE schemaname = $1")
        .bind(schema)
        .fetch_all(pool)
        .await
        .map_err(SourceError::Query)?;
    for r in &views {
        let name: String = r.get("viewname");
        if let Some(obj) = objects.iter_mut().find(|o| o.name == name) {
            obj.body = r.get::<Option<String>, _>("definition");
        }
    }

    // materialized view — information_schema.tables에 없어 따로 읽는다.
    let mvs = sqlx::query("SELECT matviewname, definition FROM pg_matviews WHERE schemaname = $1")
        .bind(schema)
        .fetch_all(pool)
        .await
        .map_err(SourceError::Query)?;
    for r in &mvs {
        objects.push(ObjectDoc {
            name: r.get("matviewname"),
            kind: "materialized-view".to_owned(),
            columns: vec![],
            constraints: vec![],
            indexes: vec![],
            triggers: vec![],
            body: r.get::<Option<String>, _>("definition"),
            usage: None,
        });
    }

    let seqs = sqlx::query("SELECT sequencename FROM pg_sequences WHERE schemaname = $1")
        .bind(schema)
        .fetch_all(pool)
        .await
        .map_err(SourceError::Query)?;
    for r in &seqs {
        objects.push(ObjectDoc {
            name: r.get("sequencename"),
            kind: "sequence".to_owned(),
            columns: vec![],
            constraints: vec![],
            indexes: vec![],
            triggers: vec![],
            body: None,
            usage: None,
        });
    }
    Ok(objects)
}

async fn read_columns(
    pool: &PgPool,
    schema: &str,
    table: &str,
) -> Result<Vec<ColumnDoc>, SourceError> {
    let rows = sqlx::query(
        "SELECT column_name, data_type, is_nullable, column_default, ordinal_position \
         FROM information_schema.columns \
         WHERE table_schema = $1 AND table_name = $2 ORDER BY ordinal_position",
    )
    .bind(schema)
    .bind(table)
    .fetch_all(pool)
    .await
    .map_err(SourceError::Query)?;
    Ok(rows
        .iter()
        .map(|r| ColumnDoc {
            name: r.get("column_name"),
            data_type: r.get("data_type"),
            nullable: r.get::<String, _>("is_nullable") == "YES",
            default: r.get::<Option<String>, _>("column_default"),
            ordinal: r.get::<i32, _>("ordinal_position") as u32,
            // pk_position은 제약 조회에서 별도로 채운다 — information_schema의
            // constraint_column_usage는 PK 순서를 안 준다.
            pk_position: 0,
        })
        .collect())
}

/// PK·FK·UNIQUE·CHECK 제약. 컬럼 대응은 pg_constraint의 conkey/confkey를
/// ordinality로 풀어 member 레벨까지 정확히 만든다.
async fn read_constraints(
    pool: &PgPool,
    schema: &str,
    table: &str,
) -> Result<Vec<ConstraintDoc>, SourceError> {
    let rows = sqlx::query(
        "SELECT con.conname, con.contype::text AS contype, con.conkey, con.confkey, \
                frel.relname AS ftable, fn.nspname AS fschema \
         FROM pg_constraint con \
         JOIN pg_class rel ON rel.oid = con.conrelid \
         JOIN pg_namespace n ON n.oid = rel.relnamespace \
         LEFT JOIN pg_class frel ON frel.oid = con.confrelid \
         LEFT JOIN pg_namespace fn ON fn.oid = frel.relnamespace \
         WHERE n.nspname = $1 AND rel.relname = $2 AND con.contype IN ('p','f','u')",
    )
    .bind(schema)
    .bind(table)
    .fetch_all(pool)
    .await
    .map_err(SourceError::Query)?;

    let mut constraints = Vec::new();
    for r in &rows {
        let contype = r.get::<String, _>("contype");
        let conkey = r.get::<Option<Vec<i16>>, _>("conkey").unwrap_or_default();
        // attnum 배열을 컬럼명으로 번역한다.
        let columns = attnames(pool, schema, table, &conkey).await?;
        let referenced = if contype == "f" {
            let ftable = r.get::<String, _>("ftable");
            let fschema = r.get::<Option<String>, _>("fschema");
            let fkey = r.get::<Option<Vec<i16>>, _>("confkey").unwrap_or_default();
            Some(ReferencedDoc {
                // 대상 컬럼은 대상 스키마에서 번역해야 한다 — 같은 이름의
                // 테이블이 다른 스키마에 있으면 엉뚱한 컬럼을 가리킨다.
                columns: attnames(pool, fschema.as_deref().unwrap_or(schema), &ftable, &fkey)
                    .await?,
                schema: fschema,
                table: ftable,
            })
        } else {
            None
        };
        constraints.push(ConstraintDoc {
            name: r.get("conname"),
            kind: match contype.as_str() {
                "p" => "pk",
                "f" => "fk",
                _ => "unique",
            }
            .to_owned(),
            columns,
            referenced,
        });
    }
    Ok(constraints)
}

/// attnum 배열을 컬럼명으로 번역한다 — 한 쿼리로 묶는다.
async fn attnames(
    pool: &PgPool,
    schema: &str,
    table: &str,
    attnums: &[i16],
) -> Result<Vec<String>, SourceError> {
    if attnums.is_empty() {
        return Ok(vec![]);
    }
    let rows = sqlx::query(
        "SELECT a.attname, u.ord FROM pg_attribute a \
         JOIN pg_class rel ON rel.oid = a.attrelid \
         JOIN pg_namespace n ON n.oid = rel.relnamespace \
         JOIN unnest($3::int2[]) WITH ORDINALITY u(attnum, ord) ON u.attnum = a.attnum \
         WHERE n.nspname = $1 AND rel.relname = $2 ORDER BY u.ord",
    )
    .bind(schema)
    .bind(table)
    .bind(attnums)
    .fetch_all(pool)
    .await
    .map_err(SourceError::Query)?;
    Ok(rows.iter().map(|r| r.get::<String, _>("attname")).collect())
}

async fn read_indexes(
    pool: &PgPool,
    schema: &str,
    table: &str,
) -> Result<Vec<IndexDoc>, SourceError> {
    let rows = sqlx::query(
        "SELECT cls.relname AS index_name, idx.indisunique, \
                array_remove(array_agg(a.attname ORDER BY u.ord), NULL) AS cols, \
                (idx.indisvalid AND idx.indisready AND bool_and(a.attname IS NOT NULL)) AS definition_complete, \
                pg_get_expr(idx.indpred, idx.indrelid) AS predicate \
         FROM pg_index idx \
         JOIN pg_class cls ON cls.oid = idx.indexrelid \
         JOIN pg_class tbl ON tbl.oid = idx.indrelid \
         JOIN pg_namespace n ON n.oid = tbl.relnamespace \
         JOIN unnest(idx.indkey) WITH ORDINALITY u(attnum, ord) ON u.ord <= idx.indnkeyatts \
         LEFT JOIN pg_attribute a ON a.attrelid = tbl.oid AND a.attnum = u.attnum \
         WHERE n.nspname = $1 AND tbl.relname = $2 AND NOT idx.indisprimary \
         GROUP BY cls.relname, idx.indisunique, idx.indrelid, idx.indpred, idx.indisvalid, idx.indisready",
    )
    .bind(schema)
    .bind(table)
    .fetch_all(pool)
    .await
    .map_err(SourceError::Query)?;
    Ok(rows
        .iter()
        .map(|r| IndexDoc {
            has_predicate: None,
            definition_complete: Some(r.get("definition_complete")),
            predicate: r.get("predicate"),
            name: r.get("index_name"),
            unique: r.get("indisunique"),
            // indkey=0(식 인덱스)은 attname이 NULL이라 array_remove로 뺐다.
            columns: r.get::<Option<Vec<String>>, _>("cols").unwrap_or_default(),
            usage: None,
        })
        .collect())
}

/// 사용자 트리거 — tgisinternal=false로 제약 트리거(FK enforce 등)를 뺀다.
async fn read_triggers(
    pool: &PgPool,
    schema: &str,
    table: &str,
) -> Result<Vec<TriggerDoc>, SourceError> {
    let rows = sqlx::query(
        "SELECT tg.tgname, pg_get_triggerdef(tg.oid) AS def \
         FROM pg_trigger tg \
         JOIN pg_class cls ON cls.oid = tg.tgrelid \
         JOIN pg_namespace n ON n.oid = cls.relnamespace \
         WHERE NOT tg.tgisinternal AND n.nspname = $1 AND cls.relname = $2",
    )
    .bind(schema)
    .bind(table)
    .fetch_all(pool)
    .await
    .map_err(SourceError::Query)?;
    Ok(rows
        .iter()
        .map(|r| TriggerDoc {
            name: r.get("tgname"),
            body: r.get::<Option<String>, _>("def"),
        })
        .collect())
}

/// 집계도 호출 대상이므로 보존하되, 존재하지 않는 SQL 몸체를 합성하지 않는다.
async fn read_routines(
    pool: &PgPool,
    schema: &str,
    limitations: &mut Vec<String>,
) -> Result<Vec<RoutineDoc>, SourceError> {
    let rows = sqlx::query(
        "SELECT p.proname, l.lanname, p.prokind::text AS prokind, \
                pg_get_function_identity_arguments(p.oid) AS sig, \
                CASE WHEN p.prokind IN ('f','p','w') THEN pg_get_functiondef(p.oid) ELSE NULL END AS def \
         FROM pg_proc p \
         JOIN pg_namespace n ON n.oid = p.pronamespace \
         JOIN pg_language l ON l.oid = p.prolang \
         WHERE n.nspname = $1",
    )
    .bind(schema)
    .fetch_all(pool)
    .await
    .map_err(SourceError::Query)?;

    let mut routines = Vec::new();
    let mut skipped = 0usize;
    for r in &rows {
        let prokind = r.get::<String, _>("prokind");
        let kind = match prokind.as_str() {
            "f" | "a" | "w" => "function",
            "p" => "procedure",
            _ => {
                skipped += 1;
                continue;
            }
        };
        routines.push(RoutineDoc {
            source: None,
            name: r.get("proname"),
            kind: kind.to_owned(),
            language: Some(r.get("lanname")),
            body: r.get::<Option<String>, _>("def"),
            signature: Some(r.get("sig")),
            usage: None,
            member_of: None,
        });
    }
    if skipped > 0 {
        limitations.push(format!(
            "{schema}: {skipped} routines have unknown PostgreSQL prokind values; definitions were not collected"
        ));
    }
    Ok(routines)
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
