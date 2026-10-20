package schemagraph.probe

import java.sql.Connection
import java.sql.DatabaseMetaData
import java.sql.DriverManager
import java.sql.ResultSet
import java.sql.SQLException
import kotlin.test.Test
import kotlin.test.assertEquals
import kotlin.test.assertTrue

/**
 * PostgreSQL을 pgjdbc로 읽을 때 컬럼·키·인덱스를 스키마마다 한 번씩 읽는지, 그렇게 나눈 결과가 객체마다
 * 묻던 결과와 같은지 본다. 객체마다 물으면 고정 연결 하나에서 직렬 왕복이 객체 수의 네 배가 되어 원격 DB의
 * 수집 시간이 왕복 지연에 비례한다(PERFORMANCE.md의 원격 PostgreSQL 절).
 *
 * 메타데이터는 H2가 답한다. 추출기는 드라이버 이름이 pgjdbc일 때만 스키마 단위로 읽으므로 그 이름을
 * 보고하고, H2가 거부하는 table=null 키 조회는 pgjdbc처럼 스키마 전체를 돌려주도록 테이블마다 물은 행을
 * 이어 붙인다. PostgreSQL 전용 SQL(인덱스 카탈로그·가시성·몸체)은 ScriptedConnection이 기록만 한다.
 */
class PostgresSchemaMetadataTest {
    private fun h2(vararg statements: String): Connection = DriverManager.getConnection("jdbc:h2:mem:").also { conn ->
        conn.createStatement().use { statement -> statements.forEach { statement.execute(it) } }
    }

    /** 앞 테이블을 참조하는 테이블 [count]개 — 테이블마다 PK·FK·보조 인덱스가 있다. */
    private fun chain(count: Int): Array<String> = buildList {
        add("CREATE SCHEMA SGFIX")
        for (index in 0 until count) {
            val reference = if (index == 0) "" else ", CONSTRAINT T${index}_FK FOREIGN KEY (PREV) REFERENCES SGFIX.T${index - 1} (ID)"
            add("CREATE TABLE SGFIX.T$index (ID INT PRIMARY KEY, PREV INT, NOTE VARCHAR(20)$reference)")
            add("CREATE INDEX T${index}_PREV ON SGFIX.T$index (PREV)")
        }
    }.toTypedArray()

    /** pgjdbc처럼 보고하는 연결. [failColumns]면 스키마 전체(table=null) getColumns가 실패한다. */
    private fun pgjdbcLike(conn: Connection, failColumns: Boolean = false): ScriptedConnection {
        val overrides = mutableMapOf<String, (DatabaseMetaData, Array<Any?>) -> Any?>(
            "getDriverName" to { _, _ -> PGJDBC_DRIVER_NAME },
            "getPrimaryKeys" to { real, args -> schemaWideKeys(real, args) { table -> real.getPrimaryKeys(args[0] as String?, args[1] as String?, table) } },
            "getImportedKeys" to { real, args -> schemaWideKeys(real, args) { table -> real.getImportedKeys(args[0] as String?, args[1] as String?, table) } },
        )
        if (failColumns) overrides["getColumns"] = { _, args ->
            if (args[2] == null) throw SQLException("injected schema-wide getColumns failure")
            error("per-object getColumns must not be called on the schema-wide path")
        }
        return ScriptedConnection(conn, metadataOverrides = overrides)
    }

    /** table이 null이면 스키마의 모든 관계에 [keys]를 물어 행을 이어 붙인다 — pgjdbc의 null 처리 흉내. */
    private fun schemaWideKeys(real: DatabaseMetaData, args: Array<Any?>, keys: (String) -> ResultSet): ResultSet {
        val tables = (args[2] as String?)?.let { listOf(it) }
            ?: real.getTables(args[0] as String?, args[1] as String?, null, null).use { rs ->
                buildList { while (rs.next()) add(rs.getString("TABLE_NAME")) }
            }
        return labelledRows(tables.flatMap { table -> keys(table).use { readRows(it) } })
    }

    /** 결과의 모든 행을 열 이름 → 값으로 읽는다. */
    private fun readRows(rs: ResultSet): List<Map<String, Any?>> = buildList {
        val labels = (1..rs.metaData.columnCount).map { rs.metaData.getColumnLabel(it) }
        while (rs.next()) add(labels.associateWith { rs.getObject(it) })
    }

    /** 열 이름으로 읽는 결과. 추출기가 키 행에서 쓰는 getString·getInt만 답한다. */
    private fun labelledRows(rows: List<Map<String, Any?>>): ResultSet {
        var cursor = -1
        return proxy(ResultSet::class.java) { method, args ->
            when (method.name) {
                "next" -> ++cursor < rows.size
                "getString" -> rows[cursor][args[0] as String]?.toString()
                "getInt" -> (rows[cursor][args[0] as String] as Number?)?.toInt() ?: 0
                "close" -> null
                else -> error("labelled rows do not answer ResultSet.${method.name}")
            }
        }
    }

    private fun collect(scripted: ScriptedConnection) = Extractor(scripted.connection, "postgres", listOf("SGFIX")).extract()

    private fun objectsOf(document: CatalogDocument) = document.schemas.single().objects.associateBy { it.name }

    /** 테이블 [count]개 스키마를 pgjdbc처럼 수집하며 메타데이터 호출·준비 SQL·실행 SQL 수를 센다. */
    private fun callCounts(count: Int): Map<String, Int> = h2(*chain(count)).use { conn ->
        val scripted = pgjdbcLike(conn)
        assertEquals(count, collect(scripted).schemas.single().objects.size)
        scripted.metadataCalls.groupingBy { it }.eachCount() +
            mapOf("prepared" to scripted.prepared.size, "executed" to scripted.executed.size)
    }

    @Test
    fun schemaMetadataCallsDoNotGrowWithTheTableCount() {
        // 객체마다 getColumns·getPrimaryKeys·getImportedKeys·인덱스 SQL을 부르면 테이블 하나에 왕복 넷이 는다.
        assertEquals(callCounts(1), callCounts(40))
    }

    @Test
    fun schemaWideRowsMatchPerObjectMetadata() {
        h2(
            *chain(4),
            "CREATE TABLE SGFIX.\"Mixed Case\" (B VARCHAR(5), A INT, C INT, PRIMARY KEY (A, B))",
            "CREATE TABLE SGFIX.PAIR_REF (ID INT PRIMARY KEY, A INT, B VARCHAR(5), " +
                "CONSTRAINT PAIR_FK FOREIGN KEY (A, B) REFERENCES SGFIX.\"Mixed Case\" (A, B))",
            "CREATE TABLE SGFIX.NO_KEYS (V INT)",
            "CREATE VIEW SGFIX.CHAIN_VIEW AS SELECT ID, PREV FROM SGFIX.T1",
        ).use { conn ->
            val perObjectCalls = ScriptedConnection(conn)
            val perObject = collect(perObjectCalls)
            val schemaWideCalls = pgjdbcLike(conn)
            val schemaWide = collect(schemaWideCalls)
            assertEquals(perObject, schemaWide)
            // 두 경로가 실제로 다르게 물었고, 비교한 문서에 다중 컬럼 키와 키 없는 관계가 들어 있다.
            assertEquals(8, perObjectCalls.metadataCalls.count { it == "getPrimaryKeys" })
            assertEquals(1, schemaWideCalls.metadataCalls.count { it == "getPrimaryKeys" })
            val objects = objectsOf(schemaWide)
            assertEquals(listOf("B" to 2, "A" to 1, "C" to 0), objects.getValue("Mixed Case").columns.map { it.name to it.pkPosition })
            val pairKey = objects.getValue("PAIR_REF").constraints.single { it.kind == "fk" }
            assertEquals(listOf("A", "B") to listOf("A", "B"), pairKey.columns to pairKey.referenced?.columns)
            assertEquals(emptyList(), objects.getValue("NO_KEYS").constraints)
            assertEquals(listOf("fk", "pk"), objects.getValue("T2").constraints.map { it.kind }.sorted())
        }
    }

    @Test
    fun schemaWideColumnsAreSplitByExactTableName() {
        h2("CREATE SCHEMA SGFIX", "CREATE TABLE SGFIX.A_B (ONLY_IN_A_B INT)", "CREATE TABLE SGFIX.A1B (ONLY_IN_A1B INT)").use { conn ->
            // getColumns의 테이블 인자는 LIKE 패턴이라, 객체마다 물으면 A_B가 A1B의 컬럼까지 잡는다.
            val perObject = objectsOf(collect(ScriptedConnection(conn)))
            assertEquals(setOf("ONLY_IN_A_B", "ONLY_IN_A1B"), perObject.getValue("A_B").columns.map { it.name }.toSet())
            val schemaWide = objectsOf(collect(pgjdbcLike(conn)))
            assertEquals(listOf("ONLY_IN_A_B"), schemaWide.getValue("A_B").columns.map { it.name })
            assertEquals(listOf("ONLY_IN_A1B"), schemaWide.getValue("A1B").columns.map { it.name })
        }
    }

    @Test
    fun schemaWideFailureIsReportedPerObjectLikeBefore() {
        h2(*chain(3)).use { conn ->
            val scripted = pgjdbcLike(conn, failColumns = true)
            val document = collect(scripted)
            // 한 번만 묻고, 그 실패를 객체마다 예전 문구로 남긴다. 키는 따로 읽어 그대로 남는다.
            assertEquals(1, scripted.metadataCalls.count { it == "getColumns" })
            val expected = (0 until 3).map { "SGFIX.T$it: getColumns 실패 — injected schema-wide getColumns failure" }
            assertTrue(document.limitations.containsAll(expected), "limitations = ${document.limitations}")
            val objects = document.schemas.single().objects
            assertTrue(objects.all { it.columns.isEmpty() && it.constraints.isNotEmpty() }, "objects = $objects")
            assertEquals(false, document.context?.catalogComplete)
        }
    }
}
