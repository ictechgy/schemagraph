package schemagraph.probe

import java.sql.DriverManager
import kotlin.test.Test
import kotlin.test.assertEquals

class IndexStatisticsTest {
    @Test
    fun oracleIndexInfoNeverAsksTheDriverForExactStatistics() {
        DriverManager.getConnection("jdbc:h2:mem:").use { h2 ->
            h2.createStatement().use { statement ->
                for (sql in listOf(
                    "CREATE SCHEMA SGFIX",
                    "CREATE TABLE SGFIX.ORDERS (ID INT NOT NULL, CUSTOMER_ID INT, CODE VARCHAR(10))",
                    "CREATE UNIQUE INDEX ORDERS_CODE_UQ ON SGFIX.ORDERS (CODE)",
                    "CREATE INDEX ORDERS_CUSTOMER_IX ON SGFIX.ORDERS (CUSTOMER_ID, ID)",
                    "CREATE VIEW SGFIX.ORDER_CODES AS SELECT CODE FROM SGFIX.ORDERS",
                )) statement.execute(sql)
            }
            val scripted = ScriptedConnection(h2)
            val document = Extractor(scripted.connection, "oracle", listOf("SGFIX")).extract()

            // approximate=false면 ojdbc가 조회 전에 테이블마다 ANALYZE(19.3)나 DBMS_STATS(23.x)를
            // 실행한다. 인덱스가 있는 테이블에만, 근사 통계로 한 번씩 물어야 한다.
            assertEquals(
                listOf(listOf<Any?>("SGFIX", "ORDERS", false, true)),
                scripted.indexInfoCalls.map { it.drop(1) },
            )
            val orders = document.schemas.single().objects.single { it.name == "ORDERS" }
            assertEquals(
                listOf(
                    IndexDoc("ORDERS_CODE_UQ", true, listOf("CODE")),
                    IndexDoc("ORDERS_CUSTOMER_IX", false, listOf("CUSTOMER_ID", "ID")),
                ),
                orders.indexes,
            )
        }
    }
}
