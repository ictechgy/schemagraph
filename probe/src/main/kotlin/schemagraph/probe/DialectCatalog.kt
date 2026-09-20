package schemagraph.probe

import java.sql.Connection
import java.sql.ResultSet

internal data class DialectObject(
    val name: String,
    val kind: String,
)

internal data class DialectBodyHarvest(
    val views: Map<Pair<String, String>, String>,
    val triggers: Map<Pair<String, String>, List<TriggerDoc>>,
    val routines: List<Triple<String, String, RoutineDoc>>,
    val routineParams: Map<Pair<String, String>, String>,
)

private fun dialectQuery(
    conn: Connection,
    label: String,
    sql: String,
    schema: String,
    limitations: MutableList<String>,
    row: (ResultSet) -> Unit,
) : Boolean {
    return runCatching {
        conn.prepareStatement(sql).use { statement ->
            statement.setString(1, schema)
            statement.executeQuery().use { result ->
                while (result.next()) row(result)
            }
        }
    }.onFailure {
        limitations += "$label unavailable for schema $schema (${it.javaClass.simpleName}: ${it.message})"
    }.isSuccess
}

internal fun db2Objects(conn: Connection, schema: String, limitations: MutableList<String>): List<DialectObject> {
    val objects = mutableListOf<DialectObject>()
    dialectQuery(
        conn,
        "Db2 objects",
        """
            SELECT TABNAME, TYPE
            FROM SYSCAT.TABLES
            WHERE TABSCHEMA = ? AND TYPE IN ('T', 'U', 'V', 'W', 'S', 'A', 'N')
            ORDER BY TABNAME
        """.trimIndent(),
        schema,
        limitations,
    ) { rs ->
        val kind = when (rs.getString(2)) {
            "V", "W" -> "view"
            "S" -> "materialized-view"
            "A", "N" -> "synonym"
            else -> "table"
        }
        objects += DialectObject(rs.getString(1), kind)
    }
    dialectQuery(
        conn,
        "Db2 sequences",
        "SELECT SEQNAME FROM SYSCAT.SEQUENCES WHERE SEQSCHEMA = ? ORDER BY SEQNAME",
        schema,
        limitations,
    ) { rs -> objects += DialectObject(rs.getString(1), "sequence") }
    return objects.distinctBy { it.name to it.kind }.sortedWith(compareBy({ it.name }, { it.kind }))
}

internal fun db2Bodies(conn: Connection, schema: String, limitations: MutableList<String>): DialectBodyHarvest {
    val views = mutableMapOf<Pair<String, String>, String>()
    val triggers = mutableMapOf<Pair<String, String>, MutableList<TriggerDoc>>()
    val routines = mutableListOf<Triple<String, String, RoutineDoc>>()
    val params = mutableMapOf<Pair<String, String>, String>()

    dialectQuery(
        conn,
        "Db2 views",
        "SELECT VIEWNAME, TEXT FROM SYSCAT.VIEWS WHERE VIEWSCHEMA = ? ORDER BY VIEWNAME",
        schema,
        limitations,
    ) { rs -> rs.getString(2)?.let { views[schema to rs.getString(1)] = it } }
    dialectQuery(
        conn,
        "Db2 triggers",
        "SELECT TRIGNAME, TABNAME, TEXT FROM SYSCAT.TRIGGERS WHERE TRIGSCHEMA = ? ORDER BY TABNAME, TRIGNAME",
        schema,
        limitations,
    ) { rs ->
        triggers.getOrPut(schema to rs.getString(2)) { mutableListOf() } +=
            TriggerDoc(rs.getString(1), rs.getString(3))
    }

    val rawRoutines = mutableListOf<Db2RoutineRow>()
    dialectQuery(
        conn,
        "Db2 routines",
        """
            SELECT ROUTINENAME, SPECIFICNAME, ROUTINETYPE, LANGUAGE, TEXT, ORIGIN
            FROM SYSCAT.ROUTINES
            WHERE ROUTINESCHEMA = ? AND ROUTINETYPE IN ('F', 'P')
            ORDER BY ROUTINENAME, SPECIFICNAME
        """.trimIndent(),
        schema,
        limitations,
    ) { rs ->
        rawRoutines += Db2RoutineRow(
            name = rs.getString(1),
            specific = rs.getString(2),
            type = rs.getString(3),
            language = rs.getString(4)?.trim()?.lowercase()?.takeIf { it.isNotEmpty() },
            body = rs.getString(5),
            origin = rs.getString(6),
        )
    }
    dialectQuery(
        conn,
        "Db2 routine parameters",
        """
            SELECT SPECIFICNAME, ROWTYPE, ORDINAL, TYPENAME
            FROM SYSCAT.ROUTINEPARMS
            WHERE ROUTINESCHEMA = ? AND ROWTYPE IN ('B', 'O', 'P') AND ORDINAL > 0
            ORDER BY SPECIFICNAME, ORDINAL
        """.trimIndent(),
        schema,
        limitations,
    ) { rs ->
        params.merge(schema to rs.getString(1), rs.getString(4)) { old, next -> "$old,$next" }
    }
    var unavailableBodies = 0
    for (routine in rawRoutines) {
        if (routine.body == null) unavailableBodies++
        routines += Triple(
            schema,
            routine.specific,
            db2RoutineDocument(routine, params[schema to routine.specific]),
        )
    }
    if (unavailableBodies > 0) {
        limitations += "$schema: $unavailableBodies Db2 routine bodies unavailable (external or protected routine text is NULL)"
    }
    return DialectBodyHarvest(views, triggers, routines, params)
}

internal data class Db2RoutineRow(
    val name: String,
    val specific: String,
    val type: String,
    val language: String?,
    val body: String?,
    val origin: String?,
)

internal fun informixObjects(conn: Connection, schema: String, limitations: MutableList<String>): List<DialectObject> {
    val objects = mutableListOf<DialectObject>()
    dialectQuery(
        conn,
        "Informix objects",
        """
            SELECT tabname, tabtype
            FROM systables
            WHERE owner = ? AND tabid >= 100 AND tabtype IN ('T', 'E', 'V', 'Q', 'P', 'S')
            ORDER BY tabname
        """.trimIndent(),
        schema,
        limitations,
    ) { rs ->
        val kind = when (rs.getString(2)) {
            "V" -> "view"
            "Q" -> "sequence"
            "P", "S" -> "synonym"
            else -> "table"
        }
        objects += DialectObject(rs.getString(1), kind)
    }
    return objects.distinctBy { it.name to it.kind }.sortedWith(compareBy({ it.name }, { it.kind }))
}

internal fun informixBodies(conn: Connection, schema: String, limitations: MutableList<String>): DialectBodyHarvest {
    val views = mutableMapOf<Pair<String, String>, String>()
    val triggers = mutableMapOf<Pair<String, String>, MutableList<TriggerDoc>>()
    val routines = mutableListOf<Triple<String, String, RoutineDoc>>()
    val params = mutableMapOf<Pair<String, String>, String>()

    val viewLines = mutableMapOf<String, MutableList<Pair<Int, String>>>()
    dialectQuery(
        conn,
        "Informix views",
        """
            SELECT t.tabname, v.seqno, v.viewtext
            FROM systables t JOIN sysviews v ON v.tabid = t.tabid
            WHERE t.owner = ? ORDER BY t.tabname, v.seqno
        """.trimIndent(),
        schema,
        limitations,
    ) { rs -> viewLines.getOrPut(rs.getString(1)) { mutableListOf() } += rs.getInt(2) to rs.getString(3) }
    for ((name, lines) in viewLines) {
        // SYSVIEWS의 viewtext는 이미 원문 조각이다 — seqno 순으로 이어 붙인다.
        views[schema to name] = concatenateInformixFragments(lines)
    }

    val triggerChunks = mutableMapOf<Pair<String, String>, MutableList<InformixChunk>>()
    dialectQuery(
        conn,
        "Informix triggers",
        """
            SELECT t.trigname, s.tabname, b.datakey, b.seqno, b.data
            FROM systriggers t
            JOIN systables s ON s.tabid = t.tabid
            JOIN systrigbody b ON b.trigid = t.trigid
            WHERE t.owner = ? AND b.datakey IN ('D', 'A')
            ORDER BY t.trigname, t.tabid, b.datakey, b.seqno
        """.trimIndent(),
        schema,
        limitations,
    ) { rs ->
        triggerChunks.getOrPut(rs.getString(1) to rs.getString(2)) { mutableListOf() } +=
            InformixChunk(rs.getString(3), rs.getInt(4), rs.getString(5))
    }
    for ((key, chunks) in triggerChunks) {
        val body = assembleInformixFragments(chunks)
        triggers.getOrPut(schema to key.second) { mutableListOf() } += TriggerDoc(key.first, body)
    }

    val routineRows = mutableListOf<InformixRoutineRow>()
    dialectQuery(
        conn,
        "Informix routines",
        """
            SELECT p.procid, p.procname, p.specificname, p.isproc, p.langid,
                   l.langname
            FROM sysprocedures p LEFT JOIN sysroutinelangs l ON l.langid = p.langid
            WHERE p.owner = ? AND p.internal = 'f' ORDER BY p.procname, p.procid
        """.trimIndent(),
        schema,
        limitations,
    ) { rs ->
        routineRows += InformixRoutineRow(
            procid = rs.getInt(1),
            name = rs.getString(2),
            specific = rs.getString(3)?.trim().takeUnless { it.isNullOrEmpty() } ?: "procid:${rs.getInt(1)}",
            isProcedure = rs.getString(4).equals("t", ignoreCase = true),
            language = rs.getString(6)?.trim()?.lowercase()?.takeIf { it.isNotEmpty() },
            signature = null,
        )
    }
    val signatureByProcid = mutableMapOf<Int, String>()
    val signaturesRead = dialectQuery(
        conn,
        "Informix routine signatures",
        """
            SELECT p.procid, CAST(p.paramtypes AS LVARCHAR(32739))
            FROM sysprocedures p
            WHERE p.owner = ? AND p.internal = 'f'
            ORDER BY p.procid
        """.trimIndent(),
        schema,
        limitations,
    ) { rs ->
        rs.getString(2)?.trim()?.takeIf { it.isNotEmpty() }?.let { signatureByProcid[rs.getInt(1)] = it }
    }
    if (!signaturesRead) {
        // RTNPARAMTYPES는 opaque라 일부 JDBC 드라이버가 getString을 거부한다.
        // core routine 행은 보존하고 signature만 limitation으로 남긴다.
        limitations += "$schema: Informix RTNPARAMTYPES signature cast unavailable; routine signatures omitted"
    }
    for (index in routineRows.indices) {
        val routine = routineRows[index]
        routineRows[index] = routine.copy(signature = signatureByProcid[routine.procid])
    }
    val bodyLines = mutableMapOf<Int, MutableList<Pair<Int, String>>>()
    dialectQuery(
        conn,
        "Informix routine bodies",
        """
            SELECT p.procid, b.seqno, b.data
            FROM sysprocedures p JOIN sysprocbody b ON b.procid = p.procid
            WHERE p.owner = ? AND p.internal = 'f' AND b.datakey = 'T' ORDER BY p.procid, b.seqno
        """.trimIndent(),
        schema,
        limitations,
    ) { rs -> bodyLines.getOrPut(rs.getInt(1)) { mutableListOf() } += rs.getInt(2) to rs.getString(3) }
    var unavailableBodies = 0
    for (routine in routineRows) {
        // SYSPROCBODY의 data는 고정 길이 원문 조각일 수 있어 임의의 줄바꿈을
        // 넣으면 문자열·식별자가 깨진다. seqno 순으로 그대로 이어 붙인다.
        val body = bodyLines[routine.procid]?.let(::concatenateInformixFragments)
        if (body == null) unavailableBodies++
        routines += Triple(
            schema,
            routine.specific,
            informixRoutineDocument(routine, body),
        )
    }
    if (unavailableBodies > 0) {
        limitations += "$schema: $unavailableBodies Informix routine bodies unavailable (external or missing SYSPROCBODY text)"
    }
    return DialectBodyHarvest(views, triggers, routines, params)
}

internal data class InformixChunk(val key: String, val seq: Int, val data: String)

internal data class InformixRoutineRow(
    val procid: Int,
    val name: String,
    val specific: String,
    val isProcedure: Boolean,
    val language: String?,
    val signature: String?,
)

internal fun concatenateInformixFragments(fragments: List<Pair<Int, String>>): String =
    fragments.sortedBy { it.first }.joinToString(separator = "") { it.second }

internal fun assembleInformixFragments(chunks: List<InformixChunk>): String {
    val header = concatenateInformixFragments(chunks.filter { it.key == "D" }.map { it.seq to it.data })
    val action = concatenateInformixFragments(chunks.filter { it.key == "A" }.map { it.seq to it.data })
    return when {
        header.isEmpty() -> action
        action.isEmpty() -> header
        else -> "$header\n$action"
    }
}

internal fun db2RoutineDocument(row: Db2RoutineRow, signature: String?): RoutineDoc =
    RoutineDoc(
        name = row.name,
        kind = if (row.type == "P") "procedure" else "function",
        language = row.language,
        body = row.body,
        signature = signature,
    )

internal fun informixRoutineDocument(row: InformixRoutineRow, body: String?): RoutineDoc =
    RoutineDoc(
        name = row.name,
        kind = if (row.isProcedure) "procedure" else "function",
        language = row.language,
        body = body,
        signature = row.signature,
    )
