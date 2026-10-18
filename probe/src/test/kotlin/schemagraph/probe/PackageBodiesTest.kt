package schemagraph.probe

import java.sql.DriverManager
import kotlin.test.Test
import kotlin.test.assertEquals
import kotlin.test.assertNotNull
import kotlin.test.assertNull
import kotlin.test.assertTrue

/** probe-go/package_slices_test.go와 같은 사례 — 두 프로브는 같은 본문을 같은 멤버 조각으로 나눠야 한다. */
class PackageBodiesTest {
    @Test
    fun ignoresHeadersInsideCommentsStringsAndQQuotes() {
        val impl = """
            CREATE OR REPLACE PACKAGE BODY p AS
              -- PROCEDURE Fake IS BEGIN NULL; END Fake;
              /* FUNCTION FakeBlock IS BEGIN NULL; END; */
              v VARCHAR2(100) := 'PROCEDURE FakeString';
              v2 VARCHAR2(100) := q'« FUNCTION FakeQuote IS BEGIN NULL; END; »';
              PROCEDURE "Do_Work" IS
              BEGIN
                NULL;
              END "Do_Work";
            END p;
        """.trimIndent()

        val body = assertNotNull(slicePackageBodyLexical(impl, setOf("DO_WORK"))["DO_WORK#0"])
        assertTrue("PROCEDURE \"Do_Work\"" in body && "FakeQuote" !in body, "unexpected slice: $body")
    }

    @Test
    fun skipsLocalSubprogramWithThePublicName() {
        val impl = """
            PACKAGE BODY p AS
              PROCEDURE outer IS
                PROCEDURE same_name IS
                BEGIN
                  NULL;
                END same_name;
              BEGIN
                NULL;
              END outer;
              FUNCTION same_name RETURN NUMBER IS
              BEGIN
                RETURN 1;
              END;
            END p;
        """.trimIndent()

        val got = slicePackageBodyLexical(impl, setOf("OUTER", "SAME_NAME"))
        assertEquals(2, got.size, "slices: $got")
        val outer = got.getValue("OUTER#0")
        assertTrue("FUNCTION same_name" !in outer, "outer slice crossed public boundary: $outer")
        assertTrue("PROCEDURE same_name" in outer, "nested local routine was dropped: $outer")
        assertTrue("RETURN 1" in got.getValue("SAME_NAME#0"), "public same-name member was not selected: $got")
    }

    @Test
    fun stopsBeforePrivateMemberAndInitializationBlock() {
        val impl = """
            PACKAGE BODY p AS
              PROCEDURE public_member IS
              BEGIN
                NULL;
              END public_member;
              PROCEDURE private_member IS
              BEGIN
                INSERT INTO audit_log VALUES (1);
              END private_member;
            BEGIN
              INSERT INTO package_log VALUES (2);
            END p;
        """.trimIndent()

        val body = slicePackageBodyLexical(impl, setOf("PUBLIC_MEMBER"))["PUBLIC_MEMBER#0"].orEmpty()
        assertTrue("private_member" !in body && "package_log" !in body,
            "following private/init code was attributed to public member: $body")
        assertTrue(body.endsWith("END public_member;"), "slice does not end at the member terminator: $body")
    }

    @Test
    fun slicesOverloadsAndNamedEnds() {
        val impl = """
            PACKAGE BODY p AS
              PROCEDURE work(a NUMBER) IS
              BEGIN
                NULL;
              END work;
              PROCEDURE work(a VARCHAR2) IS
              BEGIN
                NULL;
              END;
              BEGIN
                NULL;
              END;
            END p;
        """.trimIndent()

        val got = slicePackageBodyLexical(impl, setOf("work"))
        assertEquals(2, got.size, "overload slices: $got")
        assertTrue("a NUMBER" in got.getValue("work#0") && "a VARCHAR2" in got.getValue("work#1"), "overload slices: $got")
    }

    @Test
    fun uncertainBoundaryIsMissing() {
        val impl = """
            PACKAGE BODY p AS
              PROCEDURE broken IS
              BEGIN
                NULL;
              -- missing END means the boundary is uncertain
        """.trimIndent()

        assertEquals(emptyMap(), slicePackageBodyLexical(impl, setOf("broken")))
    }

    /** 중첩 정의 256단(패키지 멤버 포함)까지는 경계를 찾고 257단부터는 경계 없음이다 — Go와 같은 상한. */
    @Test
    fun nestingDepthLimitKeepsCompilableBodiesAndRejectsDeeperOnes() {
        assertEquals(1, slicePackageBodyLexical(nestedPackageBody(256), setOf("M")).size)
        assertEquals(emptyMap(), slicePackageBodyLexical(nestedPackageBody(257), setOf("M")))
    }

    /** 닫히지 않는 중첩 헤더 10만 단도 스택을 쓰지 않고 경계 없음으로 끝난다. */
    @Test
    fun deepUnterminatedNestingIsMissingWithoutStackOverflow() {
        val impl = "PACKAGE BODY p AS\n" + "PROCEDURE m IS\n".repeat(100_000)

        assertEquals(emptyMap(), slicePackageBodyLexical(impl, setOf("M")))
    }

    /** 조작된 PACKAGE BODY 하나가 Oracle 수집 전체를 끝내지 않고 그 멤버의 limitation으로만 남는다. */
    @Test
    fun hostilePackageBodyLeavesOnlyAMemberLimitation() {
        DriverManager.getConnection("jdbc:h2:mem:").use { h2 ->
            h2.createStatement().use { it.execute("CREATE SCHEMA SGFIX") }
            val hostile = "PACKAGE BODY HOSTILE AS\n" + "PROCEDURE m IS\n".repeat(100_000)
            val ping = "PROCEDURE ping IS BEGIN UPDATE tickets SET note = 'x'; END;"
            val scripted = ScriptedConnection(h2, tables = mapOf(
                "JOIN ALL_SOURCE" to listOf(
                    listOf("SGFIX", "HOSTILE", "PACKAGE BODY", 1, hostile),
                    listOf("SGFIX", "PING", "PROCEDURE", 1, ping),
                ),
                "FROM ALL_PROCEDURES" to listOf(listOf("SGFIX", "HOSTILE", "M", null)),
            ))

            val document = Extractor(scripted.connection, "oracle", listOf("SGFIX")).extract()

            val routines = document.schemas.single { it.name == "SGFIX" }.routines
            assertNull(routines.single { it.memberOf == "HOSTILE" }.body)
            assertEquals(ping, routines.single { it.name == "PING" }.body)
            assertTrue(document.limitations.any { it.startsWith("SGFIX.HOSTILE.M: ") }, "${document.limitations}")
        }
    }

    /** m 안에 m을 depth단 정의하고 안쪽부터 모두 닫은 PACKAGE BODY. */
    private fun nestedPackageBody(depth: Int): String =
        "PACKAGE BODY p AS\n" + "PROCEDURE m IS\n".repeat(depth) + "BEGIN NULL; END m;\n".repeat(depth) + "END p;"
}
