package schemagraph.probe

import java.nio.file.Files
import java.sql.DriverManager
import kotlin.test.Test
import kotlin.test.assertEquals
import kotlin.test.assertFalse
import kotlin.test.assertTrue

/** JDBC 수집 범위가 네이티브·Go 수집기와 같은 객체를 보는지 확인한다. */
class CatalogScopeTest {
    @Test
    fun systemSchemasDependOnTheDialect() {
        // 다른 방언의 시스템 스키마 이름은 PostgreSQL에서 평범한 사용자 스키마다.
        for (name in listOf("system", "sys", "mysql", "performance_schema", "xdb")) {
            assertFalse(isSystemSchema("postgres", name), "postgres $name")
        }
        for (name in listOf("pg_catalog", "pg_toast", "pg_temp_3", "pg_toast_temp_3", "information_schema")) {
            assertTrue(isSystemSchema("postgres", name), "postgres $name")
        }
        assertTrue(isSystemSchema("mysql", "performance_schema"))
        assertTrue(isSystemSchema("mariadb", "sys"))
        assertFalse(isSystemSchema("mysql", "system"))
        assertTrue(isSystemSchema("sqlserver", "sys"))
        assertFalse(isSystemSchema("sqlserver", "mysql"))
        assertTrue(isSystemSchema("oracle", "SYSTEM"))
        assertTrue(isSystemSchema("oracle", "PUBLIC"))
        assertFalse(isSystemSchema("h2", "SYSTEM"))
        assertTrue(isSystemSchema("h2", "INFORMATION_SCHEMA"))
        assertFalse(isSystemSchema("sqlite", "main"))
        // 방언을 모르면 예전처럼 모든 방언의 목록을 쓴다.
        assertTrue(isSystemSchema("vertica", "sys"))
    }

    @Test
    fun partitionedAndForeignTablesAreTables() {
        assertEquals("table", tableKindOf("PARTITIONED TABLE"))
        assertEquals("table", tableKindOf("FOREIGN TABLE"))
        assertEquals("table", tableKindOf("BASE TABLE"))
        assertEquals("materialized-view", tableKindOf("MATERIALIZED VIEW"))
        assertEquals("view", tableKindOf("VIEW"))
        assertEquals(null, tableKindOf("SYSTEM TABLE"))
        assertEquals(null, tableKindOf("PARTITIONED INDEX"))
        assertEquals(null, tableKindOf("TEMPORARY TABLE"))
    }

    @Test
    fun sqliteTriggerTargetMatchesTableCaseInsensitively() {
        val file = Files.createTempFile("sg-trigger", ".db")
        try {
            DriverManager.getConnection("jdbc:sqlite:$file").use { connection ->
                connection.createStatement().use {
                    it.execute("CREATE TABLE orders(id INTEGER PRIMARY KEY)")
                    it.execute("CREATE TRIGGER t_upper AFTER INSERT ON ORDERS BEGIN SELECT 1; END")
                }
                val document = Extractor(connection, "sqlite", emptyList()).extract()
                val orders = document.schemas.single().objects.single { it.name == "orders" }
                assertEquals(listOf("t_upper"), orders.triggers.map { it.name })
            }
        } finally {
            Files.deleteIfExists(file)
        }
    }
}
