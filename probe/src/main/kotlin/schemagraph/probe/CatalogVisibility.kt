package schemagraph.probe

import java.sql.Connection

/** JDBC 드라이버가 권한에 따라 숨긴 행을 PostgreSQL 원본 이름 목록과 대조한다. */
internal fun checkPostgresVisibility(conn: Connection, schema: String, objects: List<ObjectDoc>, limitations: MutableList<String>) {
    val known = objects.associate { it.name to it.columns.map { column -> column.name }.toSet() }
    val missingRelations = mutableSetOf<String>()
    var missingColumns = 0
    try {
        val resource = ObjectDoc::class.java.getResourceAsStream("/catalog/visibility-postgres.sql")
            ?: error("Bundled visibility query is missing; rebuild the probe")
        val query = resource.bufferedReader().use { it.readText() }.replace(":schema", "?")
        conn.prepareStatement(query).use { statement ->
            statement.setString(1, schema)
            statement.executeQuery().use { rows ->
                while (rows.next()) {
                    val relation = rows.getString("relation_name")
                    val column = rows.getString("column_name")
                    if (relation !in known) missingRelations += relation
                    if (column != null && column !in known[relation].orEmpty()) missingColumns++
                }
            }
        }
        if (missingRelations.isNotEmpty() || missingColumns > 0) {
            limitations += "$schema: catalog visibility check found ${missingRelations.size} uncollected relations and $missingColumns uncollected columns; verify metadata permissions and reader coverage"
        }
    } catch (error: Exception) {
        limitations += "$schema: catalog visibility could not be checked (${error.javaClass.simpleName}); verify metadata permissions"
    }
}
