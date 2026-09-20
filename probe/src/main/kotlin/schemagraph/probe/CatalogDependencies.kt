package schemagraph.probe

import java.sql.Connection

/** 공유된 정적 카탈로그 쿼리를 바인딩하여 원래 참조 행만 수집한다. */
internal fun collectCatalogDependencies(conn: Connection, dialect: String, schema: String, limitations: MutableList<String>): List<CatalogDependency> {
    val catalog = when (dialect) {
        "postgres" -> "pg_depend"
        "sqlserver" -> "sys.sql_expression_dependencies"
        "oracle" -> "ALL_DEPENDENCIES"
        else -> { limitations += "Catalog dependency collection is unsupported for $dialect"; return emptyList() }
    }
    return runCatching {
        val resource = CatalogDependency::class.java.getResourceAsStream("/catalog/catalog-$dialect.sql")
            ?: error("Bundled catalog query is missing; rebuild the probe distribution")
        val query = resource.bufferedReader().use { it.readLines().filterNot { line -> line.trimStart().startsWith("--") }.joinToString("\n") }.replace(":schema", "?")
        conn.prepareStatement(query).use { statement ->
            statement.setString(1, schema)
            statement.executeQuery().use { rows ->
                buildList {
                    while (rows.next()) add(CatalogDependency(
                        source = CatalogObjectRef(schema = rows.getString("source_schema"), name = rows.getString("source_name"), kind = rows.getString("source_kind"), signature = rows.getString("source_signature")),
                        target = CatalogObjectRef(schema = rows.getString("target_schema"), name = rows.getString("target_name"), kind = rows.getString("target_kind"), signature = rows.getString("target_signature"), member = rows.getString("target_member"), database = rows.getString("target_database")),
                        catalog = catalog, dependencyType = rows.getString("dependency_type"),
                    ))
                }
            }
        }
    }.getOrElse { error -> limitations += "$catalog dependency metadata unavailable for $schema: ${error.javaClass.simpleName}: ${error.message}"; emptyList() }
}
