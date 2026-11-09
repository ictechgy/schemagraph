package schemagraph.probe

import java.sql.DriverManager
import kotlin.test.Test
import kotlin.test.assertEquals
import kotlin.test.assertNull

/** 서버의 이름 비교 규칙 원값을 문맥에 싣고, 판정은 엔진에 맡기는지 본다. */
class NameRulesTest {
    private fun context(dialect: String, answers: Map<String, String>): CollectionContext? =
        DriverManager.getConnection("jdbc:h2:mem:").use { h2 ->
            h2.createStatement().use { it.execute("CREATE SCHEMA SGFIX") }
            Extractor(ScriptedConnection(h2, answers).connection, dialect, listOf("SGFIX")).extract().context
        }

    @Test
    fun mysqlFamilyCarriesLowerCaseTableNames() {
        assertEquals(1, context("mysql", mapOf(LOWER_CASE_TABLE_NAMES_SQL to "1"))?.lowerCaseTableNames)
        assertEquals(2, context("mariadb", mapOf(LOWER_CASE_TABLE_NAMES_SQL to "2"))?.lowerCaseTableNames)
        // 0·1·2 밖의 값과 빈 응답은 규칙을 모르는 것이다.
        assertNull(context("mysql", mapOf(LOWER_CASE_TABLE_NAMES_SQL to "7"))?.lowerCaseTableNames)
        assertNull(context("mysql", emptyMap())?.lowerCaseTableNames)
    }

    @Test
    fun sqlServerCarriesDatabaseCollationAndOtherDialectsCarryNeither() {
        val collation = "SQL_Latin1_General_CP1_CI_AS"
        assertEquals(collation, context("sqlserver", mapOf(DATABASE_COLLATION_SQL to collation))?.collation)
        val postgres = context("postgres", mapOf(LOWER_CASE_TABLE_NAMES_SQL to "1", DATABASE_COLLATION_SQL to collation))
        assertNull(postgres?.lowerCaseTableNames)
        assertNull(postgres?.collation)
    }

    @Test
    fun absentRulesAreOmittedFromTheWire() {
        val bare = mapper.readTree(mapper.writeValueAsString(CollectionContext(sourceId = "app", catalogComplete = true)))
        assertEquals(false, bare.has("lower_case_table_names") || bare.has("collation"), bare.toString())
        val withRules = mapper.readTree(
            mapper.writeValueAsString(
                CollectionContext(sourceId = "app", catalogComplete = true, lowerCaseTableNames = 1, collation = "X_CI_AS"),
            ),
        )
        assertEquals(1, withRules["lower_case_table_names"].asInt(), withRules.toString())
        assertEquals("X_CI_AS", withRules["collation"].asText(), withRules.toString())
    }
}
