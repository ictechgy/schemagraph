package schemagraph.probe

import java.sql.DriverManager
import kotlin.test.Test
import kotlin.test.assertEquals
import kotlin.test.assertNull
import kotlin.test.assertTrue

class FetchSizeTest {
    @Test
    fun oracleConnectionsPrefetchLargeBatchesForDriverMetadataToo() {
        assertEquals("1000", connectionProperties("oracle", null, null).getProperty("defaultRowPrefetch"))
        assertNull(connectionProperties("postgres", null, null).getProperty("defaultRowPrefetch"))
    }

    @Test
    fun oracleBodyQueriesFetchLargeBatchesEvenOnCallerConnections() {
        // ALL_SOURCE는 소스 한 줄이 한 행이다 — 연결 속성 없이 넘어온 연결에서도 몸체
        // 조회는 ojdbc 기본(왕복당 10행) 대신 큰 묶음으로 받아야 한다. 다른 방언은 그대로 둔다.
        for ((dialect, expected) in listOf("oracle" to 1000, "postgres" to null)) {
            DriverManager.getConnection("jdbc:h2:mem:").use { h2 ->
                h2.createStatement().use { it.execute("CREATE SCHEMA SGFIX") }
                val scripted = ScriptedConnection(h2)
                Extractor(scripted.connection, dialect, listOf("SGFIX")).extract()
                val bodyQueries = scripted.prepared.filter { "SGFIX" in it.bindings.values }
                assertTrue(bodyQueries.isNotEmpty(), dialect)
                assertEquals(listOf(expected), bodyQueries.map { it.fetchSize }.distinct(), dialect)
            }
        }
    }
}
