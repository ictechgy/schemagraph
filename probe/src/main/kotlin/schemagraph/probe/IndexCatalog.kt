package schemagraph.probe

import java.sql.Connection
import java.util.SortedMap

/** JDBC 기본 목록이 숨기는 식·부분 인덱스 여부를 원래 카탈로그에서 옮긴다. */
internal fun catalogIndexes(conn: Connection, dialect: String, schema: String, table: String): List<IndexDoc>? = when (dialect) {
    "sqlite" -> sqliteCatalogIndexes(conn, schema, table)
    "postgres" -> postgresCatalogIndexes(conn, schema, table)[table].orEmpty()
    else -> null
}

/** 스키마 [schema]의 인덱스를 한 번에 읽어 테이블 이름별로 나눈다(PostgresSchemaMetadata). 테이블마다 읽을 때와
 *  같은 질의에서 테이블 조건만 뺐으므로 테이블 안의 결과가 같다. */
internal fun postgresSchemaIndexes(conn: Connection, schema: String): Map<String, List<IndexDoc>> =
    postgresCatalogIndexes(conn, schema, null)

private fun quoteIdentifier(value: String) = "\"${value.replace("\"", "\"\"")}\""

private fun sqliteCatalogIndexes(conn: Connection, schema: String, table: String): List<IndexDoc> {
    data class Header(val name: String, val unique: Boolean, val partial: Boolean)
    val indexes = conn.createStatement().use { statement ->
        statement.executeQuery("PRAGMA ${quoteIdentifier(schema)}.index_list(${quoteIdentifier(table)})").use { rows ->
            buildList { while (rows.next()) add(Header(rows.getString("name"), rows.getInt("unique") == 1, rows.getInt("partial") == 1)) }
        }
    }
    return indexes.map { index ->
        var complete = true
        val columns = conn.createStatement().use { statement ->
            statement.executeQuery("PRAGMA ${quoteIdentifier(schema)}.index_xinfo(${quoteIdentifier(index.name)})").use { rows ->
                buildList {
                    while (rows.next()) {
                        if (rows.getInt("key") == 0) continue
                        val column = rows.getString("name")
                        if (rows.getInt("cid") >= 0 && column != null) add(rows.getInt("seqno") to column)
                        else complete = false
                    }
                }
            }
        }.sortedBy { it.first }.map { it.second }
        IndexDoc(index.name, index.unique, columns, definitionComplete = complete && columns.isNotEmpty(), hasPredicate = index.partial)
    }.sortedBy { it.name }
}

/** 인덱스 하나의 정의를 행마다 모으는 중간값. 식 키는 컬럼이 없어 정의가 불완전하다. */
private class PostgresIndexDefinition(val unique: Boolean, var complete: Boolean, val predicate: String?, val columns: MutableList<String>)

/** [table]이 null이면 스키마 전체를 읽는다. 테이블마다 인덱스는 이름 순서이고, 컬럼은 키 순서다. */
private fun postgresCatalogIndexes(conn: Connection, schema: String, table: String?): Map<String, List<IndexDoc>> {
    val query = """
        SELECT tbl.relname AS table_name, cls.relname, idx.indisunique, a.attname, u.ord,
               idx.indisvalid AND idx.indisready AS ready, pg_get_expr(idx.indpred,idx.indrelid) AS predicate
        FROM pg_index idx
        JOIN pg_class cls ON cls.oid=idx.indexrelid
        JOIN pg_class tbl ON tbl.oid=idx.indrelid
        JOIN pg_namespace ns ON ns.oid=tbl.relnamespace
        JOIN LATERAL unnest(idx.indkey) WITH ORDINALITY u(attnum,ord) ON u.ord<=idx.indnkeyatts
        LEFT JOIN pg_attribute a ON a.attrelid=tbl.oid AND a.attnum=u.attnum
        WHERE ns.nspname=?${if (table != null) " AND tbl.relname=?" else ""}
        ORDER BY tbl.relname,cls.relname,u.ord
    """.trimIndent()
    val tables = mutableMapOf<String, SortedMap<String, PostgresIndexDefinition>>()
    conn.prepareStatement(query).use { statement ->
        statement.setString(1, schema)
        if (table != null) statement.setString(2, table)
        statement.executeQuery().use { rows ->
            while (rows.next()) {
                val definitions = tables.getOrPut(rows.getString("table_name")) { sortedMapOf() }
                val definition = definitions.getOrPut(rows.getString("relname")) {
                    PostgresIndexDefinition(rows.getBoolean("indisunique"), rows.getBoolean("ready"), rows.getString("predicate"), mutableListOf())
                }
                val column = rows.getString("attname")
                if (column != null) definition.columns += column else definition.complete = false
            }
        }
    }
    return tables.mapValues { (_, definitions) -> definitions.map { (name, value) -> postgresIndexDoc(name, value) } }
}

/** 모은 정의를 문서 값으로 옮긴다. 키 컬럼을 하나도 못 읽었으면 정의가 불완전하다. */
private fun postgresIndexDoc(name: String, value: PostgresIndexDefinition) = IndexDoc(
    name, value.unique, value.columns,
    definitionComplete = value.complete && value.columns.isNotEmpty(),
    hasPredicate = value.predicate != null, predicate = value.predicate,
)
