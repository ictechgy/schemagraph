package schemagraph.probe

import kotlin.test.Test
import kotlin.test.assertEquals
import kotlin.test.assertNull

class DialectCatalogTest {
    @Test
    fun informixFragmentsRestoreHeaderBeforeActionInSequenceOrder() {
        val body = assembleInformixFragments(
            listOf(
                InformixChunk("A", 2, "INSERT INTO audit_log VALUES (1);"),
                InformixChunk("D", 1, "CREATE TRIGGER trg AFTER INSERT ON orders"),
                InformixChunk("A", 1, "FOR EACH ROW"),
            ),
        )
        assertEquals(
            "CREATE TRIGGER trg AFTER INSERT ON orders\nFOR EACH ROWINSERT INTO audit_log VALUES (1);",
            body,
        )
    }

    @Test
    fun fixedSizeRoutineFragmentsDoNotGainNewlinesInsideTokens() {
        val first = "LONG_FRAGMENT_MARKER_" + "x".repeat(238)
        val second = "y".repeat(256) + "_tail"
        assertEquals(first + second, concatenateInformixFragments(listOf(1 to first, 2 to second)))
    }

    @Test
    fun db2SpecificNamesKeepOverloadedSignaturesAndExternalBodiesRaw() {
        val integer = db2RoutineDocument(
            Db2RoutineRow("overloaded", "overloaded_i", "F", "sql", "CREATE FUNCTION overloaded(INTEGER)", "N"),
            "INTEGER",
        )
        val text = db2RoutineDocument(
            Db2RoutineRow("overloaded", "overloaded_s", "F", "sql", "CREATE FUNCTION overloaded(VARCHAR)", "N"),
            "VARCHAR",
        )
        val external = db2RoutineDocument(
            Db2RoutineRow("external_marker", "external_marker", "F", "c", null, "E"),
            "INTEGER",
        )
        assertEquals("INTEGER", integer.signature)
        assertEquals("VARCHAR", text.signature)
        assertEquals("sql", integer.language)
        assertEquals("c", external.language)
        assertNull(external.body)
    }

    @Test
    fun informixExternalRoutineHasNoInventedBody() {
        val external = informixRoutineDocument(
            InformixRoutineRow(101, "external_marker", "external_marker", false, "c", "INTEGER"),
            null,
        )
        assertEquals("function", external.kind)
        assertEquals("c", external.language)
        assertEquals("INTEGER", external.signature)
        assertNull(external.body)
    }
}
