package schemagraph.probe

import java.nio.charset.Charset
import java.sql.Connection
import java.sql.DatabaseMetaData
import java.sql.SQLException
import java.util.Arrays
import java.util.Locale
import java.util.regex.Pattern

// sqlite-jdbc의 getPrimaryKeys·getImportedKeys는 호출마다 sqlite_schema 전체를 lower(name)으로
// 훑고, FK 행마다 대상 테이블을 찾느라 한 번 더 훑는다. 테이블이 N개면 수집이 O(N²)이다.
// 이 파일은 그 드라이버의 규칙(DDL 정규식, 같은 PRAGMA 원문, 결과 행의 값과 순서)을 그대로 옮기고
// DDL만 수집당 한 번 읽는다. 출력 바이트는 그대로 두고 비용만 O(N)으로 줄이려는 것이다.
// 드라이버가 예외를 던지거나 결과문을 escape 없이 조립해 결과가 달라지는 입력은 흉내 내지 않는다.
// null을 돌려 드라이버 메타데이터에 맡기므로 실패 문구까지 이전과 같다.

/** 규칙을 옮겨 온 sqlite-jdbc 버전. 다른 버전은 그 드라이버의 규칙을 따르도록 메타데이터를 쓴다. */
internal const val REPLICATED_SQLITE_JDBC_VERSION = "3.51.1.0"

/** 드라이버는 결과 행마다 SELECT 항 하나를 UNION으로 잇는다. 번들 SQLite의 MAX_COMPOUND_SELECT(500)를
 *  넘으면 "too many terms in compound SELECT"로 실패하므로 그런 테이블은 드라이버에 맡긴다. */
private const val DRIVER_COMPOUND_TERMS = 500

// 아래 세 정규식은 sqlite-jdbc 3.51.1.0 JDBC3DatabaseMetaData의 원문과 플래그를 그대로 옮겼다.
/** PK_UNNAMED_PATTERN — 이름 없는 PK의 컬럼 목록. */
private val PK_UNNAMED = Pattern.compile(
    ".*PRIMARY\\s+KEY\\s*\\((.*?)\\).*", Pattern.CASE_INSENSITIVE or Pattern.DOTALL)

/** PK_NAMED_PATTERN — 이름 있는 PK. 앞의 `.*`가 탐욕적이라 PK 앞의 마지막 CONSTRAINT를 잡는다. */
private val PK_NAMED = Pattern.compile(
    ".*CONSTRAINT\\s*(.*?)\\s*PRIMARY\\s+KEY\\s*\\((.*?)\\).*", Pattern.CASE_INSENSITIVE or Pattern.DOTALL)

/** ImportedKeyFinder.FK_NAMED_PATTERN — 이름 있는 FK를 선언 순서로 찾는다. */
private val FK_NAMED = Pattern.compile(
    "CONSTRAINT\\s*\"?([A-Za-z_][A-Za-z\\d_]*)?\"?\\s*FOREIGN\\s+KEY\\s*\\((.*?)\\)",
    Pattern.CASE_INSENSITIVE or Pattern.DOTALL)

/** Java String.split(",")와 같은 분할(끝의 빈 조각은 버린다). Kotlin split은 빈 조각을 남긴다. */
private val COMMA = Pattern.compile(",")

/** getPrimaryKeys 결과에서 Extractor가 읽는 값 — (KEY_SEQ, COLUMN_NAME) 쌍과 첫 PK_NAME. */
internal class PrimaryKeyRows(val columns: List<Pair<Int, String>>, val name: String?)

/** getImportedKeys 결과 한 행에서 Extractor가 FK를 묶을 때 읽는 값. 이름이 비면 null이다. */
internal data class ImportedKeyRow(
    val name: String?,
    val keySeq: Int,
    val column: String,
    val referencedSchema: String?,
    val referencedTable: String,
    val referencedColumn: String,
)

/** 규칙을 옮긴 번들 드라이버인지 본다. 다른 드라이버의 메타데이터 규칙은 옮기지 않았다. */
internal fun replicatesSqliteDriver(meta: DatabaseMetaData): Boolean =
    meta.driverName == "SQLite JDBC" && meta.driverVersion == REPLICATED_SQLITE_JDBC_VERSION

/** foreign_key_list 한 행(id, seq, table, from, to). to가 null이면 대상 컬럼을 생략한 참조다. */
private class ForeignKeyPart(val id: Int, val seq: Int, val table: String, val from: String, val to: String?)

/** PrimaryKeyFinder(table)의 결과. columns가 null이면 드라이버가 PK를 찾지 못한 것이다. */
private class FoundPrimaryKey(val name: String?, val columns: List<String>?)

/**
 * sqlite_schema를 한 번 읽은 이름 재료. 드라이버의 `lower(name) = lower('…')` 전체 훑기를 대신한다.
 * SQLite lower()는 ASCII만 바꾸고, 훑기는 첫 일치 행을 쓰므로 같은 순서로 읽어 처음 본 행만 둔다.
 */
private class SchemaText(
    /** type이 table·view인 첫 행의 DDL(PrimaryKeyFinder의 조회). */
    val relations: Map<String, String>,
    /** type과 무관한 첫 행의 DDL(FK 이름 조회). 트리거는 테이블과 이름 공간이 달라 같은 이름이 있을 수 있다. */
    val anyType: Map<String, String?>,
    /** BINARY 비교는 DB 인코딩 바이트의 memcmp다(드라이버 결과문의 ORDER BY PKTABLE_NAME). */
    val encoding: Charset,
)

/**
 * sqlite-jdbc가 getPrimaryKeys·getImportedKeys로 돌려줄 행을 같은 값·순서로 만든다.
 *
 * catalog·schema 인자가 null인 호출만 대신한다(3.51.1.0의 getTables가 둘 다 NULL로 준다).
 * 드라이버처럼 스키마를 붙이지 않은 PRAGMA를 보내므로 temp·main·attach 순서로 찾는 것도 같다.
 * 반환이 null이면 그 입력에서 드라이버 결과를 정확히 옮길 수 없는 경우(대개 드라이버가 예외를 던진다)라,
 * 호출자는 드라이버 메타데이터를 그대로 불러 같은 결과(대개 같은 실패)를 남긴다.
 */
internal class SQLiteKeyReader(private val conn: Connection) {
    /** DDL이 없는 table·view 행이 있으면 드라이버가 NPE를 던지는 DB라 전부 드라이버에 맡긴다(null). */
    private val schemaText: SchemaText? by lazy { readSchemaText() }

    /** 이름별 PrimaryKeyFinder 결과. 드라이버는 PK 조회와 FK 대상마다 같은 테이블을 다시 훑는다. */
    private val primaryKeys = HashMap<String, FoundPrimaryKey?>()

    /** getPrimaryKeys(null, null, table)과 같은 행. */
    fun primaryKeyRows(table: String): PrimaryKeyRows? = declineOnSqlError {
        val found = primaryKeyOf(table) ?: return null
        val columns = found.columns ?: return PrimaryKeyRows(emptyList(), null)
        // 빈 목록이면 드라이버 결과문이 `from ()`로 깨지고, 항이 많으면 UNION 상한에 걸린다.
        if (columns.isEmpty() || columns.size > DRIVER_COMPOUND_TERMS) return null
        // 드라이버는 찾은 컬럼을 결과문에 넣으며 인용 해제를 한 번 더 한다.
        PrimaryKeyRows(columns.mapIndexed { index, column -> index + 1 to unquoteIdentifier(column) }, found.name)
    }

    /** getImportedKeys(null, null, table)과 같은 행 — 결과문의 ORDER BY PKTABLE_NAME, KEY_SEQ(안정 정렬)까지 같다. */
    fun importedKeyRows(table: String): List<ImportedKeyRow>? = declineOnSqlError {
        // 드라이버의 ImportedKeyFinder는 공백뿐인 이름을 SQLException으로 거부한다. 결과문은 FKTABLE_NAME을
        // escape 없이 '…'로 감싸므로(quote), 이름에 작은따옴표가 있으면 문이 깨지거나 다른 식이 된다.
        if (isJavaBlank(table) || '\'' in table) return null
        val text = schemaText ?: return null
        val parts = foreignKeyList(table)
        if (parts.size > DRIVER_COMPOUND_TERMS) return null
        val names = foreignKeyNames(text, table) ?: return null
        val declared = declaredForeignKeys(table, parts)
        val rows = parts.map { part ->
            importedKeyRow(part, if (part.id < declared) names.getOrNull(part.id) else null) ?: return null
        }
        rows.sortedWith(compareBy<ImportedKeyRow, String>(binaryOrder(text.encoding)) { it.referencedTable }.thenBy { it.keySeq })
    }

    /** 드라이버의 FK 행 하나. 대상 컬럼을 생략했으면 대상 PK의 첫 컬럼을 쓰고, 못 찾으면 드라이버가 NPE를 던진다. */
    private fun importedKeyRow(part: ForeignKeyPart, name: String?): ImportedKeyRow? {
        val referencedColumn = part.to ?: primaryKeyOf(part.table)?.columns?.firstOrNull() ?: return null
        return ImportedKeyRow(name, part.seq + 1, part.from, null, part.table, referencedColumn)
    }

    /** PrimaryKeyFinder는 FK 대상마다 다시 만들어진다. 같은 이름이면 결과가 같아 한 번만 계산한다. */
    private fun primaryKeyOf(table: String): FoundPrimaryKey? = primaryKeys.getOrPut(table) { findPrimaryKey(table) }

    /** PrimaryKeyFinder 생성자. 드라이버가 SQLException을 던지는 입력(공백 이름, 없는 테이블)은 null이다. */
    private fun findPrimaryKey(table: String): FoundPrimaryKey? {
        // 드라이버는 시스템 표에 빈 결과를 주려고 이 두 이름만 대소문자까지 비교해 건너뛴다.
        if (table == "sqlite_schema" || table == "sqlite_master") return FoundPrimaryKey(null, null)
        if (isJavaBlank(table)) return null
        val sql = schemaText?.relations?.get(asciiLower(table)) ?: return null
        return declaredPrimaryKey(sql) ?: FoundPrimaryKey(null, tableInfoPrimaryKey(table)?.let { listOf(unquoteIdentifier(it)) })
    }

    /** 정규식으로 PK를 찾지 못하면 드라이버는 table_info에서 pk가 0이 아닌 마지막 컬럼 하나만 쓴다. */
    private fun tableInfoPrimaryKey(table: String): String? = conn.createStatement().use { statement ->
        statement.executeQuery("pragma table_info('${escapeLiteral(table)}');").use { rows ->
            var column: String? = null
            while (rows.next()) if (rows.getInt(6) != 0) column = rows.getString(2)
            column
        }
    }

    /** 드라이버가 보내는 것과 같은 foreign_key_list PRAGMA. */
    private fun foreignKeyList(table: String): List<ForeignKeyPart> = conn.createStatement().use { statement ->
        statement.executeQuery("pragma foreign_key_list('${escapeLiteral(table)}');").use { rows ->
            buildList {
                while (rows.next()) {
                    add(ForeignKeyPart(rows.getInt(1), rows.getInt(2), rows.getString(3), rows.getString(4), rows.getString(5)))
                }
            }
        }
    }

    /**
     * ImportedKeyFinder가 이름을 붙이는 FK 수. 드라이버는 테이블 이름을 기본 로캘로 소문자화해
     * PRAGMA를 한 번 더 보낸다. ASCII 대소문자만 다르면 SQLite가 같은 테이블을 찾으므로 첫 결과를 센다.
     */
    private fun declaredForeignKeys(table: String, parts: List<ForeignKeyPart>): Int {
        val lowered = table.lowercase(Locale.getDefault())
        val listed = if (lowered == asciiLower(table)) parts else foreignKeyList(lowered)
        var count = 0
        var previous = -1
        for (part in listed) if (part.id != previous) { count++; previous = part.id }
        return count
    }

    /** sqlite_schema를 드라이버의 훑기와 같은 순서로 한 번 읽는다. */
    private fun readSchemaText(): SchemaText? {
        val relations = HashMap<String, String>()
        val anyType = HashMap<String, String?>()
        conn.createStatement().use { statement ->
            statement.executeQuery("SELECT type, name, sql FROM sqlite_schema").use { rows ->
                while (rows.next()) {
                    val key = asciiLower(rows.getString(2) ?: continue)
                    val sql = rows.getString(3)
                    if (!anyType.containsKey(key)) anyType[key] = sql
                    if (rows.getString(1) in RELATION_TYPES && !relations.containsKey(key)) relations[key] = sql ?: return null
                }
            }
        }
        return SchemaText(relations, anyType, databaseEncoding())
    }

    /** main DB의 텍스트 인코딩. attach한 DB도 main과 인코딩이 같아야 한다(SQLite 규칙). */
    private fun databaseEncoding(): Charset = conn.createStatement().use { statement ->
        statement.executeQuery("PRAGMA encoding").use { rows -> rows.next(); Charset.forName(rows.getString(1)) }
    }
}

/** PrimaryKeyFinder가 찾는 sqlite_schema type(`type in ('table', 'view')`). */
private val RELATION_TYPES = setOf("table", "view")

/** 옮긴 경로의 SQLite 오류는 드라이버 경로가 같은 조회를 다시 해 자기 문구로 남기게 한다(null로 넘긴다). */
private inline fun <T> declineOnSqlError(block: () -> T?): T? =
    try {
        block()
    } catch (error: SQLException) {
        null
    }

/** 드라이버의 PK 정규식 두 개 — 이름 있는 PK를 먼저 찾는다. */
private fun declaredPrimaryKey(sql: String): FoundPrimaryKey? {
    val named = PK_NAMED.matcher(sql)
    // 드라이버는 escape한 이름을 인용 해제해 문자열 리터럴로 되돌린다. 값은 원문을 인용 해제한 것과 같다.
    if (named.find()) return FoundPrimaryKey(unquoteIdentifier(named.group(1)), splitColumns(named.group(2)))
    val unnamed = PK_UNNAMED.matcher(sql)
    return if (unnamed.find()) FoundPrimaryKey(null, splitColumns(unnamed.group(1))) else null
}

/** ImportedKeyFinder.getForeignKeyNames — 선언 순서의 이름을 뒤집어 foreign_key_list id(마지막 선언이 0)에 맞춘다. */
private fun foreignKeyNames(text: SchemaText, table: String): List<String?>? {
    val key = asciiLower(table)
    if (!text.anyType.containsKey(key)) return emptyList()
    // 첫 일치 행의 DDL이 없으면 드라이버가 matcher(null)에서 NPE를 던진다.
    val sql = text.anyType[key] ?: return null
    val names = mutableListOf<String?>()
    val matcher = FK_NAMED.matcher(sql)
    while (matcher.find()) names += matcher.group(1)
    return names.asReversed()
}

/** 드라이버의 `group.split(",")` 뒤 컬럼마다 인용 해제. COLLATE·DESC 같은 꼬리도 이름에 남는다. */
private fun splitColumns(text: String): List<String> = COMMA.split(text).map(::unquoteIdentifier)

/** 드라이버의 unquoteIdentifier — Java trim(U+0020 이하 제거) 뒤 한 겹의 "", ``, []를 벗긴다. */
private fun unquoteIdentifier(name: String): String {
    val trimmed = name.trim { it <= ' ' }
    if (trimmed.length <= 2) return trimmed
    val first = trimmed.first()
    val last = trimmed.last()
    val quoted = (first == '`' && last == '`') || (first == '"' && last == '"') || (first == '[' && last == ']')
    return if (quoted) trimmed.substring(1, trimmed.length - 1) else trimmed
}

/** Java `name.trim().length() == 0`과 같은 판정 — 드라이버가 "Invalid table name"으로 거부한다. */
private fun isJavaBlank(name: String): Boolean = name.all { it <= ' ' }

/** SQLite lower()와 같은 소문자화 — ASCII A-Z만 바꾼다(번들 SQLite에는 ICU가 없다). */
private fun asciiLower(value: String): String =
    buildString(value.length) { for (char in value) append(if (char in 'A'..'Z') char + ('a' - 'A') else char) }

/** 드라이버의 escape — 작은따옴표만 두 겹으로 만든다. */
private fun escapeLiteral(value: String): String = value.replace("'", "''")

/** SQLite BINARY 정렬 — DB 인코딩 바이트를 부호 없이 사전순으로 비교하고, 같으면 짧은 쪽이 앞이다. */
private fun binaryOrder(encoding: Charset): Comparator<String> =
    Comparator { left, right -> Arrays.compareUnsigned(left.toByteArray(encoding), right.toByteArray(encoding)) }
