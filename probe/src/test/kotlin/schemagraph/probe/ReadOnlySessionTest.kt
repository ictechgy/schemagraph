package schemagraph.probe

import java.nio.file.Files
import java.nio.file.Path
import java.sql.DriverManager
import java.sql.SQLException
import kotlin.test.Test
import kotlin.test.assertContentEquals
import kotlin.test.assertEquals
import kotlin.test.assertFailsWith
import kotlin.test.assertFalse
import kotlin.test.assertNotNull
import kotlin.test.assertNull
import kotlin.test.assertTrue

class ReadOnlySessionTest {
    private fun <T> withTemporaryDirectory(block: (Path) -> T): T {
        val directory = Files.createTempDirectory("schemagraph-readonly-")
        try {
            return block(directory)
        } finally {
            directory.toFile().deleteRecursively()
        }
    }

    @Test
    fun sqliteWalCopyIsCollectedWithoutRewritingTheOriginalFile() = withTemporaryDirectory { directory ->
        val source = directory.resolve("source.db")
        val copy = directory.resolve("copy.db")
        DriverManager.getConnection("jdbc:sqlite:$source").use { writer ->
            writer.createStatement().use { statement ->
                statement.execute("PRAGMA journal_mode=WAL")
                statement.execute("PRAGMA wal_autocheckpoint=0")
                statement.execute("CREATE TABLE accounts (id INTEGER PRIMARY KEY, name TEXT NOT NULL)")
                statement.execute("CREATE VIEW account_names AS SELECT name FROM accounts")
            }
            // 마지막 연결이 닫히기 전에 사본을 떠서 체크포인트되지 않은 -wal을 함께 옮긴다.
            Files.copy(source, copy)
            Files.copy(directory.resolve("source.db-wal"), directory.resolve("copy.db-wal"))
        }
        val original = Files.readAllBytes(copy)

        val document = DriverManager.getConnection("jdbc:sqlite:$copy", connectionProperties("sqlite", null, null))
            .use { Extractor(it, "sqlite", emptyList()).extract() }

        // -wal에만 있는 객체도 읽고, 닫을 때 체크포인트로 원본 파일을 다시 쓰지 않는다.
        assertEquals(listOf("account_names", "accounts"), document.schemas.single().objects.map { it.name })
        assertContentEquals(original, Files.readAllBytes(copy))
        assertTrue(Files.exists(directory.resolve("copy.db-wal")))
        assertTrue(document.limitations.none { it.startsWith("read-only session hint") }, "${document.limitations}")
    }

    @Test
    fun missingSqliteTargetsAreRejectedWithoutCreatingAFile() = withTemporaryDirectory { directory ->
        val missing = directory.resolve("typo.db")
        assertNotNull(sqliteTargetError("jdbc:sqlite:$missing"))
        assertNotNull(sqliteTargetError("jdbc:sqlite:$directory"))
        assertNotNull(sqliteTargetError("jdbc:sqlite::memory:"))
        assertNotNull(sqliteTargetError("jdbc:sqlite:"))
        // 사전 검사를 건너뛰는 file: URI도 읽기 전용으로 열려 드라이버가 빈 DB를 만들지 않는다.
        assertFailsWith<SQLException> {
            DriverManager.getConnection("jdbc:sqlite:file:$missing", connectionProperties("sqlite", null, null)).close()
        }
        assertFalse(Files.exists(missing))

        val present = directory.resolve("present.db")
        DriverManager.getConnection("jdbc:sqlite:$present").use { it.createStatement().use { s -> s.execute("CREATE TABLE t (id INTEGER)") } }
        assertNull(sqliteTargetError("jdbc:sqlite:$present"))
        assertNull(sqliteTargetError("jdbc:sqlite:$present?busy_timeout=1000"))
    }

    @Test
    fun connectionPropertiesRequestTheDriversReadOnlyModes() {
        val postgres = connectionProperties("postgres", "collector", "PLACEHOLDER")
        assertEquals("always", postgres.getProperty("readOnlyMode"))
        assertEquals("collector", postgres.getProperty("user"))
        assertEquals("PLACEHOLDER", postgres.getProperty("password"))
        assertEquals("1", connectionProperties("sqlite", null, null).getProperty("open_mode"))
        assertEquals(emptyMap<Any, Any>(), connectionProperties("h2", null, null).toMap())
    }

    @Test
    fun postgresSessionThatStaysWritableIsReported() {
        for ((state, reported) in listOf("off" to true, "on" to false)) {
            DriverManager.getConnection("jdbc:h2:mem:").use { h2 ->
                val scripted = ScriptedConnection(h2, answers = mapOf("SHOW transaction_read_only" to state))
                val document = Extractor(scripted.connection, "postgres", emptyList()).extract()
                assertEquals(
                    reported,
                    document.limitations.any { it.startsWith("PostgreSQL session is not read-only") },
                    "transaction_read_only=$state limitations=${document.limitations}",
                )
                assertTrue("SHOW transaction_read_only" in scripted.executed)
            }
        }
    }
}
