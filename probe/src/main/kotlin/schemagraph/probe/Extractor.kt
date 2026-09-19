package schemagraph.probe

import java.sql.Connection
import java.sql.DatabaseMetaData
import java.sql.ResultSet

// 프로브는 "멍청한 추출기"다 — JDBC 메타데이터와 몸체 원문을 옮길 뿐 의미
// 해석은 엔진이 한다. DatabaseMetaData가 표준 API라 어느 드라이버든 기본
// 수확이 나오고, 몸체는 information_schema류 쿼리를 best-effort로 덧붙인다 —
// 실패는 숨기지 않고 limitation으로 신고한다.

/** 시스템 스키마 억제 목록 — 카탈로그가 아니라 운영용이라 의존성 판정의 잡음. */
private val SYSTEM_SCHEMAS = setOf(
    "information_schema", "pg_catalog", "pg_toast", "pg_temp_1",
    "sys", "system", "mysql", "performance_schema", "innodb",
    "xdb", "olapsys", "ordsys", "mdsys", "ctxsys", "wmsys", "dbsnmp",
    "outln", "appqossys", "audsys", "gsmadmin_internal", "lbacsys",
    "remote_scheduler_agent", "dip", "oracle_ocm",
)

private fun ResultSet.strOrNull(col: String): String? =
    try { getString(col) } catch (_: Exception) { null }

/** 프로브 추출기 — Connection 하나를 받아 CatalogDocument를 만든다. */
class Extractor(
    private val conn: Connection,
    private val dialect: String,
    private val schemaFilter: List<String>,
) {
    private val limitations = mutableListOf<String>()
    private val meta: DatabaseMetaData = conn.metaData

    fun extract(): CatalogDocument {
        conn.isReadOnly = true
        val objects = collectObjects()
        val bodies = collectBodies()
        val routines = collectRoutines(bodies)
        // 스키마 목록은 객체와 routine의 합집합 — routine만 있는 스키마도 있다.
        val schemaNames = (objects.keys + routines.keys).sorted()
        val schemas = schemaNames.map { schema ->
            SchemaDoc(
                name = schema,
                objects = objects[schema].orEmpty().map { obj ->
                    obj.copy(
                        body = bodies.views[schema to obj.name],
                        triggers = bodies.triggers[schema to obj.name].orEmpty().sortedBy { it.name },
                    )
                }.sortedBy { it.name },
                routines = routines[schema].orEmpty().sortedWith(
                    compareBy({ it.name }, { it.signature ?: "" })
                ),
            )
        }
        return CatalogDocument(
            version = DOCUMENT_VERSION,
            dialect = dialect,
            reader = "probe-jdbc",
            schemas = schemas,
            limitations = limitations.distinct().sorted(),
        )
    }

    // ---- 객체 수집: TABLE/VIEW 계열만 정점으로, 시스템 테이블은 버린다 ----

    private data class RawObject(
        val schema: String, val catalog: String?, val schemaName: String?,
        val name: String, val kind: String,
    )

    private fun collectObjects(): Map<String, List<ObjectDoc>> {
        val rows = readTables(null, null).ifEmpty {
            discoveredSchemas().flatMap { readTables(null, it) }
        }
        val grouped = linkedMapOf<String, MutableList<ObjectDoc>>()
        for (row in rows) {
            val columns = columnsOf(row)
            val (constraints, pkPos) = constraintsOf(row)
            grouped.getOrPut(row.schema) { mutableListOf() } += ObjectDoc(
                name = row.name,
                kind = row.kind,
                columns = columns.map { it.copy(pkPosition = pkPos[it.name] ?: 0) }
                    .sortedBy { it.ordinal },
                constraints = constraints,
                indexes = indexesOf(row),
                triggers = emptyList(), // 몸체 수집 단계에서 붙는다
            )
        }
        if (grouped.isEmpty()) {
            limitations += "JDBC 메타데이터가 테이블/뷰를 하나도 주지 않았다 — 드라이버 커버리지를 확인해라"
        }
        return grouped
    }

    private fun readTables(catalog: String?, schema: String?): List<RawObject> = runCatching {
        meta.getTables(catalog, schema, null, null).use { rs ->
            buildList {
                while (rs.next()) {
                    val type = rs.strOrNull("TABLE_TYPE").orEmpty().uppercase()
                    val kind = when {
                        type == "TABLE" || type == "BASE TABLE" -> "table"
                        type.contains("MATERIALIZED") && type.contains("VIEW") -> "materialized-view"
                        type.contains("VIEW") -> "view"
                        type == "SYNONYM" -> "synonym"
                        else -> continue // SYSTEM TABLE, GLOBAL TEMPORARY 등
                    }
                    val schemaName = rs.strOrNull("TABLE_SCHEM")
                    val catalogName = rs.strOrNull("TABLE_CAT")
                    val schema = schemaName ?: catalogName ?: continue
                    if (schema.lowercase() in SYSTEM_SCHEMAS) continue
                    if (schemaFilter.isNotEmpty() && schema !in schemaFilter) continue
                    add(RawObject(schema, catalogName, schemaName, rs.getString("TABLE_NAME"), kind))
                }
            }
        }
    }.getOrElse {
        limitations += "getTables 실패: ${it.message}"
        emptyList()
    }

    private fun columnsOf(obj: RawObject): List<ColumnDoc> = runCatching {
        meta.getColumns(obj.catalog, obj.schemaName, obj.name, null).use { rs ->
            buildList {
                while (rs.next()) {
                    add(
                        ColumnDoc(
                            name = rs.getString("COLUMN_NAME"),
                            dataType = rs.strOrNull("TYPE_NAME") ?: "unknown",
                            // NULLABLE: 0=no, 1=yes, 2=알 수 없음 → 모름은 널 가능 쪽으로
                            nullable = rs.getInt("NULLABLE") != DatabaseMetaData.columnNoNulls,
                            default = rs.strOrNull("COLUMN_DEF"),
                            ordinal = rs.getInt("ORDINAL_POSITION"),
                            pkPosition = 0,
                        )
                    )
                }
            }
        }
    }.getOrElse {
        limitations += "${obj.schema}.${obj.name}: getColumns 실패 — ${it.message}"
        emptyList()
    }

    private fun constraintsOf(obj: RawObject): Pair<List<ConstraintDoc>, Map<String, Int>> {
        val out = mutableListOf<ConstraintDoc>()
        val pkPos = mutableMapOf<String, Int>()
        runCatching {
            meta.getPrimaryKeys(obj.catalog, obj.schemaName, obj.name).use { rs ->
                val cols = mutableListOf<Pair<Int, String>>()
                var pkName: String? = null
                while (rs.next()) {
                    cols += rs.getInt("KEY_SEQ") to rs.getString("COLUMN_NAME")
                    pkName = pkName ?: rs.strOrNull("PK_NAME")
                }
                if (cols.isNotEmpty()) {
                    val ordered = cols.sortedBy { it.first }.map { it.second }
                    ordered.forEachIndexed { i, c -> pkPos[c] = i + 1 }
                    out += ConstraintDoc(pkName ?: "${obj.name}_pk", "pk", ordered)
                }
            }
        }.onFailure { limitations += "${obj.schema}.${obj.name}: getPrimaryKeys 실패 — ${it.message}" }

        runCatching {
            meta.getImportedKeys(obj.catalog, obj.schemaName, obj.name).use { rs ->
                data class FkRow(
                    val seq: Int, val fkCol: String,
                    val pkSchema: String?, val pkTable: String, val pkCol: String,
                )
                val groups = linkedMapOf<String, MutableList<FkRow>>()
                var unnamed = 0
                while (rs.next()) {
                    val name = rs.strOrNull("FK_NAME") ?: "${obj.name}_fk_${unnamed++}"
                    groups.getOrPut(name) { mutableListOf() } += FkRow(
                        seq = rs.getInt("KEY_SEQ"),
                        fkCol = rs.getString("FKCOLUMN_NAME"),
                        pkSchema = rs.strOrNull("PKTABLE_SCHEM") ?: rs.strOrNull("PKTABLE_CAT"),
                        pkTable = rs.getString("PKTABLE_NAME"),
                        pkCol = rs.getString("PKCOLUMN_NAME"),
                    )
                }
                for ((name, rows) in groups) {
                    val ordered = rows.sortedBy { it.seq }
                    out += ConstraintDoc(
                        name = name,
                        kind = "fk",
                        columns = ordered.map { it.fkCol },
                        referenced = ReferencedDoc(
                            schema = ordered.first().pkSchema,
                            table = ordered.first().pkTable,
                            columns = ordered.map { it.pkCol },
                        ),
                    )
                }
            }
        }.onFailure { limitations += "${obj.schema}.${obj.name}: getImportedKeys 실패 — ${it.message}" }

        return out.sortedBy { it.name } to pkPos
    }

    private fun indexesOf(obj: RawObject): List<IndexDoc> = runCatching {
        meta.getIndexInfo(obj.catalog, obj.schemaName, obj.name, false, false).use { rs ->
            val groups = linkedMapOf<String, Pair<Boolean, MutableList<Pair<Int, String>>>>()
            while (rs.next()) {
                if (rs.getShort("TYPE") == DatabaseMetaData.tableIndexStatistic.toShort()) continue
                val name = rs.strOrNull("INDEX_NAME") ?: continue
                val col = rs.strOrNull("COLUMN_NAME") ?: continue
                val g = groups.getOrPut(name) { (!rs.getBoolean("NON_UNIQUE")) to mutableListOf() }
                g.second += rs.getInt("ORDINAL_POSITION") to col
            }
            groups.map { (name, g) ->
                IndexDoc(name, g.first, g.second.sortedBy { it.first }.map { it.second })
            }.sortedBy { it.name }
        }
    }.getOrElse {
        limitations += "${obj.schema}.${obj.name}: getIndexInfo 실패 — ${it.message}"
        emptyList()
    }

    private fun discoveredSchemas(): List<String> = runCatching {
        meta.schemas.use { rs ->
            buildList {
                while (rs.next()) add(rs.getString("TABLE_SCHEM"))
            }.filter { it.lowercase() !in SYSTEM_SCHEMAS }
        }
    }.getOrElse { emptyList() }

    // ---- 몸체 수집: 표준 information_schema, 방언별 대체 테이블 ----

    private data class BodyHarvest(
        val views: Map<Pair<String, String>, String>,
        val triggers: Map<Pair<String, String>, List<TriggerDoc>>,
        /** (스키마, specific_name, doc) — 오버로드 구분엔 ROUTINE_NAME이 아니라
         *  SPECIFIC_NAME이 필요하다. */
        val routines: List<Triple<String, String, RoutineDoc>>,
        /** (스키마, specific_name) → 시그니처 문자열 */
        val routineParams: Map<Pair<String, String>, String>,
    )

    private fun collectBodies(): BodyHarvest {
        val views = mutableMapOf<Pair<String, String>, String>()
        val triggers = mutableMapOf<Pair<String, String>, MutableList<TriggerDoc>>()
        val routines = mutableListOf<Triple<String, String, RoutineDoc>>()
        val params = mutableMapOf<Pair<String, String>, String>()

        if (dialect == "oracle") {
            // Oracle엔 INFORMATION_SCHEMA가 없다 — ALL_* 딕셔너리로 대체.
            bestEffort("views", "SELECT OWNER, VIEW_NAME, TEXT FROM ALL_VIEWS") { rs ->
                views[rs.getString(1) to rs.getString(2)] = rs.getString(3) ?: return@bestEffort
            }
            bestEffort("triggers",
                "SELECT OWNER, TABLE_NAME, TRIGGER_NAME, TRIGGER_BODY FROM ALL_TRIGGERS") { rs ->
                val key = rs.getString(1) to rs.getString(2)
                triggers.getOrPut(key) { mutableListOf() } +=
                    TriggerDoc(rs.getString(3), rs.getString(4))
            }
        } else {
            bestEffort("views",
                "SELECT TABLE_SCHEMA, TABLE_NAME, VIEW_DEFINITION FROM INFORMATION_SCHEMA.VIEWS") { rs ->
                views[rs.getString(1) to rs.getString(2)] = rs.getString(3) ?: return@bestEffort
            }
            bestEffort("triggers",
                "SELECT TRIGGER_SCHEMA, EVENT_OBJECT_TABLE, TRIGGER_NAME, ACTION_STATEMENT " +
                    "FROM INFORMATION_SCHEMA.TRIGGERS") { rs ->
                val key = rs.getString(1) to rs.getString(2)
                triggers.getOrPut(key) { mutableListOf() } +=
                    TriggerDoc(rs.getString(3), rs.getString(4))
            }
            bestEffort("routines",
                "SELECT ROUTINE_SCHEMA, ROUTINE_NAME, SPECIFIC_NAME, ROUTINE_TYPE, " +
                    "ROUTINE_DEFINITION, EXTERNAL_LANGUAGE FROM INFORMATION_SCHEMA.ROUTINES") { rs ->
                routines += Triple(
                    rs.getString(1),
                    rs.getString(3),
                    RoutineDoc(
                        name = rs.getString(2),
                        kind = when (rs.getString(4).uppercase()) {
                            "PROCEDURE" -> "procedure"
                            else -> "function"
                        },
                        language = rs.getString(6)?.lowercase(),
                        body = rs.getString(5),
                    ),
                )
            }
            bestEffort("routine parameters",
                "SELECT SPECIFIC_SCHEMA, SPECIFIC_NAME, DATA_TYPE, ORDINAL_POSITION " +
                    "FROM INFORMATION_SCHEMA.PARAMETERS WHERE PARAMETER_MODE = 'IN' " +
                    "ORDER BY ORDINAL_POSITION") { rs ->
                val key = rs.getString(1) to rs.getString(2)
                params.merge(key, rs.getString(3).lowercase()) { a, b -> "$a, $b" }
            }
        }

        // 스키마 필터 — INFORMATION_SCHEMA 쿼리엔 필터를 못 넣어 수확 후 거른다.
        // 시스템 스키마도 여기서 거른다(MySQL의 sys는 information_schema에서도 온다).
        fun keep(schema: String) =
            schema.lowercase() !in SYSTEM_SCHEMAS && (schemaFilter.isEmpty() || schema in schemaFilter)
        views.keys.removeIf { !keep(it.first) }
        triggers.keys.removeIf { !keep(it.first) }
        routines.removeIf { !keep(it.first) }
        params.keys.removeIf { !keep(it.first) }
        return BodyHarvest(views, triggers, routines, params)
    }

    /** routine을 전역으로 수집해 행이 보고한 스키마로 그룹화한다 —
     *  MySQL처럼 catalog을 스키마로 쓰는 드라이버는 schemaPattern을 무시하고
     *  전 DB를 돌려주므로, 요청한 스키마가 아니라 행의 스키마를 믿어야 한다. */
    private fun collectRoutines(bodies: BodyHarvest): Map<String, List<RoutineDoc>> {
        val byKey = linkedMapOf<Pair<String, String>, RoutineDoc>()

        fun merge(schema: String, name: String, kind: String, language: String?, body: String?, specific: String?) {
            // 키는 specific_name 우선 — 오버로드는 ROUTINE_NAME이 같아도 다르다.
            // meta 행이 먼저 이름키로 들어갔으면 특정키로 옮겨 흡수한다.
            val key = schema to (specific ?: name)
            val stale = if (key != schema to name) byKey.remove(schema to name) else null
            val prior = byKey[key] ?: stale
            val sig = bodies.routineParams[key] ?: bodies.routineParams[schema to name]
            byKey[key] = RoutineDoc(
                name = name,
                kind = prior?.kind ?: kind,
                language = prior?.language ?: language,
                body = prior?.body ?: body,
                signature = prior?.signature ?: sig,
            )
        }

        fun accept(schema: String?): String? =
            schema?.takeIf {
                it.lowercase() !in SYSTEM_SCHEMAS &&
                    (schemaFilter.isEmpty() || it in schemaFilter)
            }

        runCatching {
            meta.getFunctions(null, null, null).use { rs ->
                while (rs.next()) {
                    accept(rs.strOrNull("FUNCTION_SCHEM") ?: rs.strOrNull("FUNCTION_CAT"))
                        ?.let { merge(it, rs.getString("FUNCTION_NAME"), "function", null, null, null) }
                }
            }
        }.onFailure { limitations += "getFunctions 실패: ${it.message}" }

        runCatching {
            meta.getProcedures(null, null, null).use { rs ->
                while (rs.next()) {
                    accept(rs.strOrNull("PROCEDURE_SCHEM") ?: rs.strOrNull("PROCEDURE_CAT"))
                        ?.let { merge(it, rs.getString("PROCEDURE_NAME"), "procedure", null, null, null) }
                }
            }
        }.onFailure { limitations += "getProcedures 실패: ${it.message}" }

        // information_schema 수확분 — 이미 수확 단계에서 스키마·specific_name이 붙어 있다.
        for ((rschema, specific, doc) in bodies.routines) {
            merge(rschema, doc.name, doc.kind, doc.language, doc.body, specific)
        }

        return byKey.entries.groupBy({ it.key.first }, { it.value })
    }

    /** best-effort 쿼리 — 실패를 limitation으로 변환해 숨기지 않는다. */
    private fun bestEffort(label: String, sql: String, row: (ResultSet) -> Unit) {
        runCatching {
            conn.createStatement().use { st ->
                st.executeQuery(sql).use { rs -> while (rs.next()) row(rs) }
            }
        }.onFailure {
            // 한 줄로 — 줄바꿈이 들어가면 limitation 텍스트가 지저분해진다.
            val msg = it.message?.replace(Regex("\\s+"), " ")
            limitations += "$label 원문 미수확(${it.javaClass.simpleName}: $msg) — 간선이 빠질 수 있다"
        }
    }
}
