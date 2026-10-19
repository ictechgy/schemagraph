package schemagraph.probe

import java.sql.DriverManager
import kotlin.test.Test
import kotlin.test.assertEquals
import kotlin.test.assertNotNull
import kotlin.test.assertTrue

class BodyQueryBindingTest {
    /** 백슬래시를 이스케이프로 읽는 세션(MySQL 기본 sql_mode, PostgreSQL
     *  standard_conforming_strings=off)에서 따옴표 이중화를 깨는 모양의 스키마 이름. */
    private val hostileSchema = "sgmarker\\' OR '1'='1' -- "

    /** 방언마다 스키마 하나를 대상으로 하는 몸체 조회 — 각 표식은 정확히 한 조회에만 있다. */
    private val bodyQueries = mapOf(
        "oracle" to listOf(
            "FROM ALL_VIEWS", "FROM ALL_MVIEWS", "FROM ALL_TRIGGERS", "JOIN ALL_SOURCE",
            "FROM ALL_PROCEDURES", "PACKAGE_NAME IS NOT NULL", "PACKAGE_NAME IS NULL",
        ),
        "sqlserver" to listOf(
            "FROM sys.views", "FROM sys.triggers", "m.definition FROM sys.objects", "FROM sys.parameters",
        ),
        "postgres" to listOf("FROM pg_views", "FROM pg_trigger", "FROM pg_proc"),
        "mysql" to listOf(
            "INFORMATION_SCHEMA.VIEWS", "INFORMATION_SCHEMA.TRIGGERS",
            "INFORMATION_SCHEMA.ROUTINES", "INFORMATION_SCHEMA.PARAMETERS",
        ),
    )

    private fun quoteIdentifier(name: String) = "\"${name.replace("\"", "\"\"")}\""

    @Test
    fun bodyQueriesBindTheSchemaInsteadOfSplicingItIntoSql() {
        for ((dialect, markers) in bodyQueries) {
            DriverManager.getConnection("jdbc:h2:mem:").use { h2 ->
                h2.createStatement().use { it.execute("CREATE SCHEMA ${quoteIdentifier(hostileSchema)}") }
                val scripted = ScriptedConnection(h2)
                Extractor(scripted.connection, dialect, listOf(hostileSchema)).extract()

                val sqlTexts = scripted.executed + scripted.prepared.map { it.sql }
                assertTrue(sqlTexts.none { "sgmarker" in it }, "$dialect spliced the schema name into SQL text")
                for (marker in markers) {
                    val call = scripted.prepared.singleOrNull { marker in it.sql }
                    assertNotNull(call, "$dialect body query '$marker' was not a single prepared statement")
                    assertEquals(call.sql.count { it == '?' }, call.bindings.size, "$dialect '$marker' placeholders")
                    assertTrue(call.bindings.values.all { it == hostileSchema }, "$dialect '$marker' bound ${call.bindings}")
                }
            }
        }
    }

    @Test
    fun boundBodyQueriesStillReadViewDefinitionsFromARealDriver() {
        val schema = "O'Brien"
        DriverManager.getConnection("jdbc:h2:mem:").use { h2 ->
            h2.createStatement().use { statement ->
                statement.execute("CREATE SCHEMA ${quoteIdentifier(schema)}")
                statement.execute("CREATE TABLE ${quoteIdentifier(schema)}.ACCOUNTS (ID INT PRIMARY KEY, NAME VARCHAR(20))")
                statement.execute("CREATE VIEW ${quoteIdentifier(schema)}.ACCOUNT_NAMES AS SELECT NAME FROM ${quoteIdentifier(schema)}.ACCOUNTS")
            }
            val document = Extractor(h2, "h2", listOf(schema)).extract()
            val view = document.schemas.single { it.name == schema }.objects.single { it.name == "ACCOUNT_NAMES" }
            assertTrue(view.body.orEmpty().contains("ACCOUNTS"), "view body = ${view.body}")
            assertTrue(document.limitations.none { it.startsWith("views ") }, "limitations = ${document.limitations}")
        }
    }
}
