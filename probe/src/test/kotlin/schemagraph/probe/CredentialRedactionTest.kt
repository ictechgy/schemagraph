package schemagraph.probe

import java.io.ByteArrayOutputStream
import java.io.PrintStream
import java.nio.file.Files
import java.sql.Connection
import java.sql.Driver
import java.sql.DriverManager
import java.sql.DriverPropertyInfo
import java.sql.SQLException
import java.sql.SQLFeatureNotSupportedException
import java.util.Properties
import java.util.logging.Logger
import kotlin.test.Test
import kotlin.test.assertEquals
import kotlin.test.assertFalse
import kotlin.test.assertTrue

class CredentialRedactionTest {
    /** runProbe를 exitProcess 없이 돌려 종료 코드와 stderr를 돌려준다. */
    private fun probe(vararg args: String): Pair<Int?, String> {
        val buffer = ByteArrayOutputStream()
        val code = PrintStream(buffer, true, Charsets.UTF_8).use { runProbe(arrayOf(*args), it) }
        return code to buffer.toString(Charsets.UTF_8)
    }

    private fun assertNoneIn(stderr: String, vararg secrets: String) {
        for (secret in secrets) assertFalse(secret in stderr, "stderr disclosed '$secret': $stderr")
    }

    // Go 프로브용 URL(자격 증명 포함)을 JDBC 프로브에 그대로 넘긴 경우 — 원문 없이 형식만 안내한다.
    @Test
    fun nonJdbcUrlErrorOmitsTheUrl() {
        val (code, stderr) = probe("--url", "postgres://demo_user:PLACEHOLDER@db.invalid/app", "-o", "-")
        assertEquals(2, code)
        assertTrue("not a JDBC URL" in stderr, stderr)
        assertNoneIn(stderr, "demo_user", "PLACEHOLDER", "db.invalid")
    }

    // 번들되지 않은 Oracle 드라이버를 --driver 없이 쓰면 DriverManager가 "No suitable driver found
    // for <URL 전체>"를 던진다. subprotocol과 --driver만 안내하고 출력 파일은 만들지 않는다.
    @Test
    fun missingDriverNamesTheSubprotocolInsteadOfTheUrl() {
        val directory = Files.createTempDirectory("schemagraph-redaction-")
        try {
            val output = directory.resolve("catalog.json")
            val (code, stderr) = probe("--url", "jdbc:oracle:thin:demo_user/PLACEHOLDER@//db.invalid:1521/svc", "-o", output.toString())
            assertEquals(2, code)
            assertTrue("subprotocol 'oracle'" in stderr && "--driver" in stderr, stderr)
            assertNoneIn(stderr, "demo_user", "PLACEHOLDER", "db.invalid")
            assertFalse(Files.exists(output))
        } finally {
            directory.toFile().deleteRecursively()
        }
    }

    // 그 밖의 연결 실패는 드라이버 메시지를 싣되, 드라이버가 되풀이한 URL 원문·userinfo·user/pw@·
    // password= 값·--password 값을 가린다.
    @Test
    fun driverErrorsAreScrubbedOfCredentials() {
        val url = "jdbc:sgecho://demo_user:PLACEHOLDER_URL@db.invalid/app?password=PLACEHOLDER_QUERY"
        val driver = EchoingDriver(
            "cannot reach $url (parsed //demo_user:PLACEHOLDER_URL@db.invalid/app, password=PLACEHOLDER_QUERY; " +
                "fallback demo_user/PLACEHOLDER_SLASH@db.invalid; credential PLACEHOLDER_ARG)",
        )
        DriverManager.registerDriver(driver)
        try {
            val (code, stderr) = probe("--url", url, "--user", "demo_user", "--password", "PLACEHOLDER_ARG", "-o", "-")
            assertEquals(2, code)
            assertTrue("cannot reach" in stderr && "<redacted>" in stderr, stderr)
            assertNoneIn(stderr, url, "PLACEHOLDER_URL", "PLACEHOLDER_QUERY", "PLACEHOLDER_SLASH", "PLACEHOLDER_ARG")
        } finally {
            DriverManager.deregisterDriver(driver)
        }
    }

    // SG_DB_PASSWORD는 프로세스 안에서 바꿀 수 없어 가림 함수로 직접 본다. 접속 문자열 키 이름이
    // 달라도(trustStorePassword) 값만 가리고 진단에 필요한 나머지는 남긴다.
    @Test
    fun scrubbingCoversTheEnvironmentPasswordAndCredentialShapes() {
        val message = "login failed with PLACEHOLDER_ENV at jdbc:oracle:thin:scott/PLACEHOLDER_SLASH@db.invalid:1521/svc;" +
            "trustStorePassword=PLACEHOLDER_TRUST;encrypt=true"
        val scrubbed = scrubSecrets(message, "jdbc:sqlserver://db.invalid", listOf("PLACEHOLDER_ENV"))
        assertNoneIn(scrubbed, "PLACEHOLDER_ENV", "PLACEHOLDER_SLASH", "PLACEHOLDER_TRUST", "scott")
        assertTrue("trustStorePassword=<redacted>;encrypt=true" in scrubbed, scrubbed)
        assertTrue("db.invalid:1521/svc" in scrubbed, scrubbed)
    }

    /** 받은 URL과 자격 증명을 그대로 되풀이하는 드라이버 — 실제 드라이버의 최악의 오류 메시지 모양. */
    private class EchoingDriver(private val message: String) : Driver {
        override fun connect(url: String?, info: Properties?): Connection? =
            if (acceptsURL(url)) throw SQLException(message, "08006") else null
        override fun acceptsURL(url: String?) = url?.startsWith("jdbc:sgecho:") == true
        override fun getPropertyInfo(url: String?, info: Properties?) = emptyArray<DriverPropertyInfo>()
        override fun getMajorVersion() = 1
        override fun getMinorVersion() = 0
        override fun jdbcCompliant() = false
        override fun getParentLogger(): Logger = throw SQLFeatureNotSupportedException("no logger")
    }
}
