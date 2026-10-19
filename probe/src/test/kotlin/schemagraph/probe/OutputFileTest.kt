package schemagraph.probe

import java.io.ByteArrayOutputStream
import java.io.FileOutputStream
import java.io.PrintStream
import java.nio.file.AccessDeniedException
import java.nio.file.Files
import java.nio.file.LinkOption
import java.nio.file.Path
import java.nio.file.attribute.PosixFilePermissions
import java.sql.DriverManager
import java.sql.SQLException
import kotlin.io.path.listDirectoryEntries
import kotlin.io.path.name
import kotlin.io.path.readText
import kotlin.io.path.writeText
import kotlin.test.Test
import kotlin.test.assertContentEquals
import kotlin.test.assertEquals
import kotlin.test.assertFailsWith
import kotlin.test.assertNull
import kotlin.test.assertTrue

class OutputFileTest {
    private fun <T> withTemporaryDirectory(block: (Path) -> T): T {
        val directory = Files.createTempDirectory("schemagraph-output-")
        try {
            return block(directory)
        } finally {
            directory.toFile().deleteRecursively()
        }
    }

    private fun createSqlite(path: Path) {
        DriverManager.getConnection("jdbc:sqlite:$path").use { connection ->
            connection.createStatement().use { it.execute("CREATE TABLE accounts (id INTEGER PRIMARY KEY)") }
        }
    }

    /** sqlite-jdbc가 url로 실제로 연 main DB 파일. */
    private fun sqliteMainFile(url: String): Path =
        DriverManager.getConnection(url, connectionProperties("sqlite", null, null)).use { connection ->
            connection.createStatement().use { statement ->
                statement.executeQuery("PRAGMA database_list").use { rows ->
                    assertTrue(rows.next())
                    Path.of(rows.getString("file"))
                }
            }
        }

    /** main의 파일 출력처럼 연결 전에 출력을 정하고 쓴 뒤 교체한다. */
    private fun commitOutput(path: Path, contents: String) =
        writeOutput(prepareOutput(path.toString())) { it.write(contents) }

    private fun entries(directory: Path) = directory.listDirectoryEntries().map { it.name }.sorted()

    // 원본 SQLite를 -o로 주면 JSON은 수집 뒤 DB를 문서로 덮었고, NDJSON은 수집 전에 DB를 비운 뒤 빈
    // 카탈로그를 냈다. 경로 표기·링크·-wal/-shm 모두 연결 전에 거절해야 한다. SQLite가 실제로 연
    // 파일과 비교 대상이 같은지도 함께 본다. probe-go TestOutputNamingTheSQLiteSourceIsRejected와 같다.
    @Test
    fun outputNamingTheSqliteSourceIsRejected() = withTemporaryDirectory { directory ->
        val database = directory.resolve("app.db")
        createSqlite(database)
        val original = Files.readAllBytes(database)
        createSqlite(directory.resolve("100%.db"))
        Files.createSymbolicLink(directory.resolve("link.db"), Path.of("app.db"))
        Files.createLink(directory.resolve("hard.db"), database)
        Files.createDirectory(directory.resolve("sub"))
        // 작업 디렉터리 기준 상대 경로 — JVM은 chdir할 수 없어 실제 경로끼리 relativize한다.
        val relative = Path.of("").toRealPath().relativize(directory.toRealPath())
        val cases = mutableListOf(
            "jdbc:sqlite:$database" to "$database",
            "jdbc:sqlite:$database" to "$directory/sub/../app.db",
            "jdbc:sqlite:$relative/app.db" to "$database",
            "jdbc:sqlite:$database" to "$relative/link.db",
            "jdbc:sqlite:$directory/link.db?busy_timeout=1000" to "$directory/hard.db",
            "jdbc:sqlite:file:$database?mode=ro" to "$database",
            "jdbc:sqlite:file://localhost$database" to "$database",
            "jdbc:sqlite:file:$directory/%61pp.db" to "$database",
            "jdbc:sqlite:file:$directory/100%.db" to "$directory/100%.db",
            "jdbc:sqlite:$directory/link.db" to "$directory/app.db-wal",
            "jdbc:sqlite:$database" to "$directory/app.db-shm",
            "jdbc:sqlite:$directory/link.db" to "$directory/link.db-wal",
            // 원본은 실제 경로, 출력은 상대 경로여도(macOS 임시 디렉터리는 symlink 아래) 없는 -wal을 같은 이름으로 본다.
            "jdbc:sqlite:${directory.toRealPath()}/app.db" to "$relative/app.db-wal",
        )
        if (Files.exists(directory.resolve("APP.DB"))) {
            // 대소문자를 무시하는 파일 시스템(macOS 기본값)에서는 다른 표기도 같은 파일이다.
            cases += "jdbc:sqlite:$database" to "$directory/APP.DB"
        }
        for ((url, output) in cases) {
            assertTrue(Files.isSameFile(sqliteMainFile(url), sqliteDatabasePath(url)!!), "SQLite opened another file for $url")
            assertEquals(SQLITE_SOURCE_ALIAS_ERROR, sqliteOutputAliasError(url, prepareOutput(output)), "--url $url -o $output")
        }
        for (output in listOf("$directory/catalog.json", "$directory/app.db.json", "$directory/sub/app.db")) {
            assertNull(sqliteOutputAliasError("jdbc:sqlite:$database", prepareOutput(output)), output)
        }
        assertNull(sqliteDatabasePath("jdbc:sqlite::memory:"))
        assertNull(sqliteDatabasePath("jdbc:sqlite:file:shared?mode=memory&cache=shared"))
        assertContentEquals(original, Files.readAllBytes(database))
    }

    // sqlite-jdbc는 URL 접두를 소문자로 바꿔 비교하므로 jdbc:SQLite:도 같은 파일을 연다. 방언 판정이
    // 대소문자를 구분하면 원본 -o 거절을 건너뛰어 JSON 문서가 DB를 덮었다. 그래서 실제 CLI 경로
    // (runProbe)로 종료 코드와 원본 바이트를 본다.
    @Test
    fun outputNamingTheSqliteSourceIsRejectedForAnySubprotocolCase() = withTemporaryDirectory { directory ->
        val database = directory.resolve("app.db")
        createSqlite(database)
        val original = Files.readAllBytes(database)
        for (subprotocol in listOf("SQLite", "SQLITE")) {
            val stderr = ByteArrayOutputStream()
            val code = PrintStream(stderr, true, Charsets.UTF_8).use {
                runProbe(arrayOf("--url", "jdbc:$subprotocol:$database", "-o", "$database"), it)
            }
            assertEquals(2, code, "jdbc:$subprotocol: stderr=$stderr")
            assertTrue(SQLITE_SOURCE_ALIAS_ERROR in stderr.toString(Charsets.UTF_8), "jdbc:$subprotocol: stderr=$stderr")
            assertContentEquals(original, Files.readAllBytes(database), "jdbc:$subprotocol: the database was rewritten")
            assertTrue(Files.isSameFile(sqliteMainFile("jdbc:$subprotocol:$database"), sqliteDatabasePath("jdbc:$subprotocol:$database")!!))
        }
        assertEquals(listOf("app.db"), entries(directory))
    }

    // NDJSON은 연결 직후 -o를 잘라 열어, 트레일러 전에 끊긴 수집이 이전 스냅샷을 잃게 했다(writer도
    // 닫지 않았다). 실패한 쓰기는 이전 파일과 디렉터리(임시 파일 없음)를 그대로 둬야 한다.
    @Test
    fun failedWriteKeepsThePreviousFileAndRemovesTheTemporaryFile() = withTemporaryDirectory { directory ->
        val path = directory.resolve("catalog.ndjson")
        path.writeText("previous snapshot\n")
        val failure = assertFailsWith<SQLException> {
            writeOutput(prepareOutput(path.toString())) { writer ->
                writer.write("{\"type\":\"document\"}")
                writer.newLine()
                throw SQLException("connection reset")
            }
        }
        assertEquals("connection reset", failure.message)
        assertEquals("previous snapshot\n", path.readText())
        assertEquals(listOf("catalog.ndjson"), entries(directory))
    }

    // 교체는 기존 권한을 지키고, 새 파일은 지금의 FileOutputStream처럼 0666&~umask로 만든다(0600
    // 전환은 계약 결정이라 하지 않는다).
    @Test
    fun committedOutputReplacesTheFileAndKeepsPermissions() = withTemporaryDirectory { directory ->
        val path = directory.resolve("catalog.json")
        path.writeText("previous\n")
        Files.setPosixFilePermissions(path, PosixFilePermissions.fromString("rw-r-----"))
        commitOutput(path, "next\n")
        assertEquals("next\n", path.readText())
        assertEquals("rw-r-----", PosixFilePermissions.toString(Files.getPosixFilePermissions(path)))
        val fresh = directory.resolve("fresh.json")
        commitOutput(fresh, "next\n")
        val control = directory.resolve("control.json")
        FileOutputStream(control.toFile()).close()
        assertEquals(Files.getPosixFilePermissions(control), Files.getPosixFilePermissions(fresh))
        assertEquals(listOf("catalog.json", "control.json", "fresh.json"), entries(directory))
    }

    // symlink 출력은 지금처럼 가리키는 파일을 갱신하고 링크는 남긴다. 대상이 없는 링크는 지금처럼
    // 링크를 따라 대상 파일을 만든다(symlink 거절은 계약 결정이라 하지 않는다).
    @Test
    fun symlinkOutputUpdatesTheLinkedFile() = withTemporaryDirectory { directory ->
        val real = directory.resolve("real.json")
        real.writeText("previous\n")
        val link = Files.createSymbolicLink(directory.resolve("link.json"), Path.of("real.json"))
        commitOutput(link, "next\n")
        assertTrue(Files.isSymbolicLink(link))
        assertEquals("next\n", real.readText())
        val dangling = Files.createSymbolicLink(directory.resolve("dangling.json"), Path.of("created.json"))
        commitOutput(dangling, "next\n")
        assertTrue(Files.isSymbolicLink(dangling))
        assertEquals("next\n", directory.resolve("created.json").readText())
        assertEquals(listOf("created.json", "dangling.json", "link.json", "real.json"), entries(directory))
    }

    // 장치·디렉터리는 rename으로 바꿀 대상이 아니다. /dev/null은 지금처럼 제자리에 쓰고, 디렉터리는
    // 지금처럼 쓰기 오류가 난다.
    @Test
    fun nonRegularOutputsAreWrittenInPlace() = withTemporaryDirectory { directory ->
        val devNull = Path.of("/dev/null")
        if (Files.exists(devNull, LinkOption.NOFOLLOW_LINKS)) {
            assertTrue(prepareOutput("/dev/null").inPlace)
            commitOutput(devNull, "next\n")
        }
        val target = prepareOutput(directory.toString())
        assertTrue(target.inPlace)
        assertFailsWith<java.io.FileNotFoundException> { writeOutput(target) { it.write("next\n") } }
        assertEquals(emptyList(), entries(directory))
    }

    // 제자리 쓰기는 읽기 전용 파일에서 실패했다. rename은 파일 권한 없이도 바꿀 수 있으므로 같은
    // 검사를 먼저 해서 보호된 스냅샷을 바꾸지 않는다.
    @Test
    fun readOnlyOutputIsNotReplaced() = withTemporaryDirectory { directory ->
        if (System.getProperty("user.name") == "root") return@withTemporaryDirectory
        val path = directory.resolve("catalog.json")
        path.writeText("protected\n")
        Files.setPosixFilePermissions(path, PosixFilePermissions.fromString("r--r--r--"))
        assertFailsWith<AccessDeniedException> { prepareOutput(path.toString()) }
        assertEquals("protected\n", path.readText())
    }
}
