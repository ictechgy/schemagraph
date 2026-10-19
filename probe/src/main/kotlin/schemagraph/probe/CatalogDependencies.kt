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

/**
 * 문서와 NDJSON에 싣는 의존성 순서 — 중복을 빼고 한 줄 직렬화 문자열 순으로 정렬한다.
 * 정렬 키(Jackson 직렬화)는 서로 다른 의존성마다 한 번만 만든다. sortedBy는 비교마다 selector를
 * 두 번 부른다. 그래서 DB가 이미 정렬해 보낸 입력에서도 직렬화가 2·D번이었고, DB 정렬 규칙이
 * 바이트 순서와 다른 입력(SQL Server의 대소문자 무시 정렬 등)에서는 약 2·D·log2 D번까지 늘었다.
 * 같은 문자열을 같은 안정 정렬로 비교하므로 순서는 예전과 같다.
 * @param key 정렬 키. 기본값은 NDJSON 한 줄 직렬화이고, 테스트는 호출 수를 세려고 바꿔 넣는다.
 */
internal fun distinctSortedDependencies(
    dependencies: List<CatalogDependency>,
    key: (CatalogDependency) -> String = { lineMapper.writeValueAsString(it) },
): List<CatalogDependency> =
    dependencies.distinct().map { it to key(it) }.sortedBy { it.second }.map { it.first }
