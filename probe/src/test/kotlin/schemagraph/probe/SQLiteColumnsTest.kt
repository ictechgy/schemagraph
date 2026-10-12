package schemagraph.probe

import java.nio.file.Files
import java.nio.file.Path
import java.sql.DriverManager
import kotlin.test.Test
import kotlin.test.assertEquals
import kotlin.test.assertTrue

class SQLiteColumnsTest {
    @Test
    fun primaryKeyNullabilityMatchesStorage() {
        DriverManager.getConnection("jdbc:sqlite::memory:").use { connection ->
            val fixture = Files.readString(Path.of("../Fixtures/sqlite-nullability.sql"))
            fixture.split(';').filter { it.isNotBlank() }.forEach { sql ->
                connection.createStatement().use { it.execute(sql) }
            }
            val tables = connection.createStatement().use { statement ->
                statement.executeQuery("SELECT name FROM sqlite_master WHERE type='table'").use { rows ->
                    buildList { while (rows.next()) add(rows.getString(1)) }
                }
            }
            assertEquals(9, tables.size)
            for (table in tables) for (column in sqliteCatalogColumns(connection, "main", table)) {
                assertEquals(table.startsWith("nullable_"), column.nullable, "$table.${column.name}")
                assertTrue(column.pkPosition > 0)
            }
        }
    }
}
