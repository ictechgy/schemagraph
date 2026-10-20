package schemagraph.probe

import java.sql.Connection
import java.sql.DatabaseMetaData
import java.sql.ResultSet

/**
 * pgjdbc의 getDriverName. 42.7.8 PgDatabaseMetaData는 getPrimaryKeys·getImportedKeys의 table이 null이면
 * 테이블 조건만 빼고 같은 질의·같은 정렬로 스키마 전체를 돌려준다. JDBC 명세는 table을 요구하고 H2처럼
 * null을 거부하는 드라이버도 있어, 스키마 단위 수집은 이 드라이버에만 쓴다.
 */
internal const val PGJDBC_DRIVER_NAME = "PostgreSQL JDBC Driver"

/**
 * PostgreSQL 스키마 하나의 컬럼·PK·FK·인덱스를 묶음마다 한 번씩 읽어 테이블 이름별로 나눈다.
 *
 * 객체마다 getColumns·getPrimaryKeys·getImportedKeys·인덱스 SQL을 부르면 연결 하나에서 직렬 왕복이 객체
 * 수의 네 배가 되어, 원격 DB에서는 수집 시간이 왕복 지연에 비례한다. 행은 객체마다 읽던 경로와 같은
 * 함수로 읽고 정확한 스키마·테이블 이름으로 나눈다. getColumns의 스키마·테이블 인자는 LIKE 패턴이라 객체마다
 * 묻던 때는 테이블 `a_b`가 `a1b`의 컬럼을, 스키마 `my_app`의 테이블이 `myxapp`의 같은 이름 테이블 컬럼을
 * 함께 잡았다 — 그런 이름 충돌이 없는 DB에서는 결과가 같다.
 * 묶음은 처음 쓰일 때 읽는다(시퀀스만 있는 스키마는 묻지 않고, 인덱스는 테이블·물화뷰가 있을 때만 묻는다).
 * 실패하면 그 오류를 객체마다 꺼내 쓰게 해, 객체마다 묻던 때 모든 객체가 실패한 것과 같은 limitation이 된다.
 */
internal class PostgresSchemaMetadata(
    private val conn: Connection,
    private val meta: DatabaseMetaData,
    private val catalog: String?,
    private val schema: String,
) {
    private val columns: Result<Map<String, List<ColumnDoc>>> by lazy {
        runCatching {
            meta.getColumns(catalog, schema, null, null).use { rs ->
                groupRows(rs, { it.getString("TABLE_SCHEM") == schema }, { it.getString("TABLE_NAME") }, ::readColumnDoc)
            }
        }
    }

    private val primaryKeys: Result<Map<String, PrimaryKeyRows>> by lazy {
        runCatching {
            meta.getPrimaryKeys(catalog, schema, null).use { rs ->
                groupRows(rs, { true }, { it.getString("TABLE_NAME") }, ::readPrimaryKeyRow)
            }.mapValues { (_, rows) -> rows.toPrimaryKeyRows() }
        }
    }

    private val importedKeys: Result<Map<String, List<ImportedKeyRow>>> by lazy {
        runCatching {
            meta.getImportedKeys(catalog, schema, null).use { rs ->
                groupRows(rs, { true }, { it.getString("FKTABLE_NAME") }, ::readImportedKeyRow)
            }
        }
    }

    private val indexes: Result<Map<String, List<IndexDoc>>> by lazy { runCatching { postgresSchemaIndexes(conn, schema) } }

    /** 테이블 [table]의 컬럼(행이 없으면 빈 목록). 스키마 묶음이 실패했으면 그 오류를 던진다. */
    fun columnsOf(table: String): List<ColumnDoc> = columns.getOrThrow()[table].orEmpty()

    /** 테이블 [table]의 PK 행(없으면 빈 행). 스키마 묶음이 실패했으면 그 오류를 던진다. */
    fun primaryKeysOf(table: String): PrimaryKeyRows = primaryKeys.getOrThrow()[table] ?: PrimaryKeyRows(emptyList(), null)

    /** 테이블 [table]의 FK 행(getImportedKeys 순서). 스키마 묶음이 실패했으면 그 오류를 던진다. */
    fun importedKeysOf(table: String): List<ImportedKeyRow> = importedKeys.getOrThrow()[table].orEmpty()

    /** 테이블·물화뷰 [table]의 인덱스(이름 순). 스키마 묶음이 실패했으면 그 오류를 던진다. */
    fun indexesOf(table: String): List<IndexDoc> = indexes.getOrThrow()[table].orEmpty()
}

/** [keep]을 통과한 행을 [key] 값별로 읽은 순서대로 모은다. */
private fun <T> groupRows(
    rs: ResultSet, keep: (ResultSet) -> Boolean, key: (ResultSet) -> String, read: (ResultSet) -> T,
): Map<String, List<T>> {
    val groups = linkedMapOf<String, MutableList<T>>()
    while (rs.next()) {
        if (keep(rs)) groups.getOrPut(key(rs)) { mutableListOf() } += read(rs)
    }
    return groups
}
