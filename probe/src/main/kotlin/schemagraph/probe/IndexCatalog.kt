package schemagraph.probe

import java.sql.Connection

/** JDBC 기본 목록이 숨기는 식·부분 인덱스 여부를 원래 카탈로그에서 옮긴다. */
internal fun catalogIndexes(conn: Connection, dialect: String, schema: String, table: String): List<IndexDoc>? = when (dialect) {
    "sqlite" -> sqliteCatalogIndexes(conn, schema, table)
    "postgres" -> postgresCatalogIndexes(conn, schema, table)
    else -> null
}

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

private fun postgresCatalogIndexes(conn: Connection, schema: String, table: String): List<IndexDoc> {
    val query = """
        SELECT cls.relname, idx.indisunique, a.attname, u.ord,
               idx.indisvalid AND idx.indisready AS ready, pg_get_expr(idx.indpred,idx.indrelid) AS predicate
        FROM pg_index idx
        JOIN pg_class cls ON cls.oid=idx.indexrelid
        JOIN pg_class tbl ON tbl.oid=idx.indrelid
        JOIN pg_namespace ns ON ns.oid=tbl.relnamespace
        JOIN LATERAL unnest(idx.indkey) WITH ORDINALITY u(attnum,ord) ON u.ord<=idx.indnkeyatts
        LEFT JOIN pg_attribute a ON a.attrelid=tbl.oid AND a.attnum=u.attnum
        WHERE ns.nspname=? AND tbl.relname=?
        ORDER BY cls.relname,u.ord
    """.trimIndent()
    data class Definition(val unique: Boolean, var complete: Boolean, val predicate: String?, val columns: MutableList<String>)
    val definitions = sortedMapOf<String, Definition>()
    conn.prepareStatement(query).use { statement ->
        statement.setString(1, schema)
        statement.setString(2, table)
        statement.executeQuery().use { rows ->
            while (rows.next()) {
                val name = rows.getString("relname")
                val definition = definitions.getOrPut(name) {
                    Definition(rows.getBoolean("indisunique"), rows.getBoolean("ready"), rows.getString("predicate"), mutableListOf())
                }
                val column = rows.getString("attname")
                if (column != null) definition.columns += column else definition.complete = false
            }
        }
    }
    return definitions.map { (name, value) -> IndexDoc(name, value.unique, value.columns, definitionComplete = value.complete && value.columns.isNotEmpty(), hasPredicate = value.predicate != null, predicate = value.predicate) }
}
