package schemagraph.probe

import java.sql.Connection

/** SQLite PK의 실제 nullability를 JDBC의 일반 PK 추정 대신 카탈로그에서 옮긴다. */
internal fun sqliteCatalogColumns(conn: Connection, schema: String, table: String): List<ColumnDoc> {
    fun quote(value: String) = "\"${value.replace("\"", "\"\"")}\""
    val target = "${quote(schema)}.table_xinfo(${quote(table)})"
    val columns = conn.createStatement().use { statement ->
        statement.executeQuery("PRAGMA $target").use { rows ->
            buildList {
                while (rows.next()) add(ColumnDoc(
                    name = rows.getString("name"), dataType = rows.getString("type"),
                    nullable = rows.getInt("notnull") == 0, default = rows.getString("dflt_value"),
                    ordinal = rows.getInt("cid") + 1, pkPosition = rows.getInt("pk"),
                ))
            }
        }
    }
    val hasPkIndex = conn.createStatement().use { statement ->
        statement.executeQuery("PRAGMA ${quote(schema)}.index_list(${quote(table)})").use { rows ->
            var found = false
            while (rows.next()) if (rows.getString("origin") == "pk") found = true
            found
        }
    }
    // DESC 열 제약과 복합 키에는 PK 인덱스가 있어 rowid 별칭과 구별된다.
    val pkCount = columns.count { it.pkPosition > 0 }
    return columns.map {
        if (it.pkPosition > 0 && pkCount == 1 && !hasPkIndex && it.dataType.equals("INTEGER", ignoreCase = true))
            it.copy(nullable = false)
        else it
    }
}
