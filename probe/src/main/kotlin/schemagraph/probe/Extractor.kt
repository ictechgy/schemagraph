package schemagraph.probe

import java.sql.Connection
import java.sql.DatabaseMetaData
import java.sql.ResultSet

// 프로브는 "멍청한 추출기"다 — JDBC 메타데이터와 몸체 원문을 옮길 뿐 의미
// 해석은 엔진이 한다. DatabaseMetaData가 표준 API라 어느 드라이버든 기본
// 수확이 나오고, 몸체는 information_schema류 쿼리를 best-effort로 덧붙인다 —
// 실패는 숨기지 않고 limitation으로 신고한다.

/** 시스템 스키마 억제 목록 — 카탈로그가 아니라 운영용이라 의존성 판정의 잡음.
 *  "public"은 여기 두지 않는다 — H2의 기본 스키마가 PUBLIC이라 전부 걸러진다.
 *  Oracle의 PUBLIC 슈도스키마는 isSystem에서 방언 한정으로 억제한다. */
private val SYSTEM_SCHEMAS = setOf(
    "information_schema", "pg_catalog", "pg_toast", "pg_temp_1",
    "sys", "system", "mysql", "performance_schema", "innodb",
    "xdb", "olapsys", "ordsys", "mdsys", "ctxsys", "wmsys", "dbsnmp",
    "outln", "appqossys", "audsys", "gsmadmin_internal", "lbacsys",
    "remote_scheduler_agent", "dip", "oracle_ocm",
    // Database Vault — 23ai+ 기본 탑재라 F$ 계열 함수가 잡음으로 온다.
    "dvsys", "dvf",
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

    /** 시스템 스키마 억제 — Oracle의 PUBLIC은 수만 시노님 슈도스키마라 방언
     *  한정으로 억제한다(H2의 PUBLIC은 사용자 기본 스키마라 억제하면 안 된다). */
    private fun isSystem(schema: String): Boolean {
        val s = schema.lowercase()
        return s in SYSTEM_SCHEMAS || (dialect == "oracle" && s == "public")
    }

    fun extract(): CatalogDocument {
        // 읽기 전용 힌트 — sqlite-jdbc처럼 연결 후 변경을 거부하는 드라이버는
        // 건너뛴다(프로브는 어차피 SELECT만 친다).
        runCatching { conn.isReadOnly = true }
        val objects = collectObjects()
        val bodies = collectBodies()
        val routines = collectRoutines(bodies)
        val usage = collectUsage()
        // 스키마 목록은 객체와 routine의 합집합 — routine만 있는 스키마도 있다.
        val schemaNames = (objects.keys + routines.keys).sorted()
        val schemas = schemaNames.map { schema ->
            SchemaDoc(
                name = schema,
                objects = objects[schema].orEmpty().map { obj ->
                    obj.copy(
                        body = bodies.views[schema to obj.name],
                        triggers = bodies.triggers[schema to obj.name].orEmpty().sortedBy { it.name },
                        usage = usage.tables[schema to obj.name],
                        indexes = obj.indexes.map { idx ->
                            idx.copy(usage = usage.indexes[Triple(schema, obj.name, idx.name)])
                        },
                    )
                }.sortedBy { it.name },
                routines = run {
                    val rts = routines[schema].orEmpty()
                    val nameCounts = rts.groupingBy { it.name }.eachCount()
                    // funcname엔 시그니처가 없다 — 같은 이름의 오버로드가 있으면
                    // 어느 것의 calls인지 모르므로 미귀속(네이티브와 같은 규칙).
                    val ambiguous = rts.count {
                        nameCounts[it.name] != 1 &&
                            usage.functions.containsKey(schema to it.name)
                    }
                    if (ambiguous > 0) {
                        limitations += "$schema: 오버로드된 routine ${ambiguous}개는 " +
                            "funcname만으로 귀속 못 해 usage 미수집"
                    }
                    rts.map { rt ->
                        rt.copy(
                            usage = usage.functions[schema to rt.name]
                                .takeIf { nameCounts[rt.name] == 1 }
                        )
                    }.sortedWith(compareBy({ it.name }, { it.signature ?: "" }))
                },
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
                // view에 getIndexInfo를 부르면 Oracle이 ORA-20000(DBMS_STATS)을
                // 던진다 — 인덱스는 테이블·물화뷰에만 있는 게 보통이라 거기만 수확.
                indexes = if (row.kind == "table" || row.kind == "materialized-view")
                    indexesOf(row) else emptyList(),
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
                    // sqlite-jdbc는 둘 다 null로 온다 — SQLite의 유일 스키마는 main이다.
                    val schema = schemaName ?: catalogName
                        ?: if (dialect == "sqlite") "main" else continue
                    if (isSystem(schema)) continue
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
                    val fkName: String?, val seq: Int, val fkCol: String,
                    val pkSchema: String?, val pkTable: String, val pkCol: String,
                )
                val groups = linkedMapOf<String, MutableList<FkRow>>()
                var unnamed = 0
                while (rs.next()) {
                    val fkName = rs.strOrNull("FK_NAME")?.takeIf { it.isNotBlank() }
                    val pkTable = rs.getString("PKTABLE_NAME")
                    // sqlite-jdbc 등은 FK_NAME이 빈 문자열이고 KEY_SEQ도 전부 1이라
                    // 행만으로는 FK를 구분할 수 없다 — 같은 대상 테이블로 가는
                    // 컬럼들을 한 FK로 묶는 게 다중 컬럼 FK 복원에도 최선이다.
                    val gkey = fkName ?: "${obj.name}@$pkTable"
                    groups.getOrPut(gkey) { mutableListOf() } += FkRow(
                        fkName = fkName,
                        seq = rs.getInt("KEY_SEQ"),
                        fkCol = rs.getString("FKCOLUMN_NAME"),
                        pkSchema = rs.strOrNull("PKTABLE_SCHEM") ?: rs.strOrNull("PKTABLE_CAT"),
                        pkTable = pkTable,
                        pkCol = rs.getString("PKCOLUMN_NAME"),
                    )
                }
                for ((_, rows) in groups) {
                    val ordered = rows.sortedBy { it.seq }
                    out += ConstraintDoc(
                        name = ordered.first().fkName ?: "${obj.name}_fk_${unnamed++}",
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
            }.filter { !isSystem(it) }
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

        when (dialect) {
            "oracle" -> {
                // Oracle엔 INFORMATION_SCHEMA가 없다 — ALL_* 딕셔너리로 대체.
                // TEXT/TRIGGER_BODY/QUERY는 LONG이라 스트림 주의가 필요하지만
                // ojdbc는 getString으로 읽어준다.
                bestEffort("views", "SELECT OWNER, VIEW_NAME, TEXT FROM ALL_VIEWS") { rs ->
                    views[rs.getString(1) to rs.getString(2)] = rs.getString(3) ?: return@bestEffort
                }
                bestEffort("materialized views",
                    "SELECT OWNER, MVIEW_NAME, QUERY FROM ALL_MVIEWS") { rs ->
                    views[rs.getString(1) to rs.getString(2)] = rs.getString(3) ?: return@bestEffort
                }
                bestEffort("triggers",
                    "SELECT OWNER, TABLE_NAME, TRIGGER_NAME, TRIGGER_BODY FROM ALL_TRIGGERS") { rs ->
                    val key = rs.getString(1) to rs.getString(2)
                    triggers.getOrPut(key) { mutableListOf() } +=
                        TriggerDoc(rs.getString(3), rs.getString(4))
                }
                // routine — ALL_OBJECTS가 kind를, ALL_SOURCE가 LINE순 몸체를 준다.
                // PACKAGE BODY의 멤버는 OBJECT_NAME이 패키지라 routine 귀속이
                // 다르다 — 독립 routine만 수확한다(PROCEDURE/FUNCTION).
                val oracleKinds = mutableMapOf<Pair<String, String>, String>()
                val oracleBodies = mutableMapOf<Pair<String, String>, StringBuilder>()
                bestEffort("routines",
                    "SELECT o.OWNER, o.OBJECT_NAME, o.OBJECT_TYPE, s.LINE, s.TEXT " +
                        "FROM ALL_OBJECTS o " +
                        "JOIN ALL_SOURCE s ON s.OWNER = o.OWNER " +
                        "  AND s.NAME = o.OBJECT_NAME AND s.TYPE = o.OBJECT_TYPE " +
                        "WHERE o.OBJECT_TYPE IN ('PROCEDURE','FUNCTION') " +
                        "ORDER BY o.OWNER, o.OBJECT_NAME, s.LINE") { rs ->
                    val key = rs.getString(1) to rs.getString(2)
                    oracleKinds[key] = rs.getString(3)
                    oracleBodies.getOrPut(key) { StringBuilder() }.append(rs.getString(5))
                }
                for ((key, body) in oracleBodies) {
                    routines += Triple(key.first, key.second, RoutineDoc(
                        name = key.second,
                        kind = if (oracleKinds[key] == "PROCEDURE") "procedure" else "function",
                        // PL/SQL — plpgsql과 같은 계약이라 엔진이 미지원 한계로 보고한다.
                        language = "plsql",
                        body = body.toString(),
                    ))
                }
                // 파라미터 — POSITION=0은 반환값이라 제외(MySQL ordinal=0 함정과 같다).
                bestEffort("routine parameters",
                    "SELECT OWNER, OBJECT_NAME, DATA_TYPE FROM ALL_ARGUMENTS " +
                        "WHERE POSITION > 0 AND PACKAGE_NAME IS NULL " +
                        "ORDER BY OWNER, OBJECT_NAME, SEQUENCE") { rs ->
                    params.merge(rs.getString(1) to rs.getString(2),
                        rs.getString(3).lowercase()) { a, b -> "$a, $b" }
                }
            }
            "sqlite" -> {
                // SQLite도 INFORMATION_SCHEMA가 없다 — sqlite_master가 원문을 가진다.
                // 네이티브 reader가 쓰는 것과 같은 소스라 몸체 패리티가 맞는다.
                bestEffort("views",
                    "SELECT name, sql FROM sqlite_master WHERE type = 'view'") { rs ->
                    views["main" to rs.getString(1)] = rs.getString(2) ?: return@bestEffort
                }
                bestEffort("triggers",
                    "SELECT name, tbl_name, sql FROM sqlite_master WHERE type = 'trigger'") { rs ->
                    triggers.getOrPut("main" to rs.getString(2)) { mutableListOf() } +=
                        TriggerDoc(rs.getString(1), rs.getString(3))
                }
            }
            "sqlserver" -> {
                // MSSQL의 INFORMATION_SCHEMA는 몸체를 4000자에서 자른다 —
                // sys.sql_modules.definition이 nvarchar(max)라 온전한 원문이다.
                bestEffort("views",
                    "SELECT SCHEMA_NAME(o.schema_id), o.name, m.definition " +
                        "FROM sys.views o " +
                        "JOIN sys.sql_modules m ON m.object_id = o.object_id") { rs ->
                    views[rs.getString(1) to rs.getString(2)] =
                        rs.getString(3) ?: return@bestEffort
                }
                bestEffort("triggers",
                    "SELECT SCHEMA_NAME(p.schema_id), p.name, t.name, m.definition " +
                        "FROM sys.triggers t " +
                        "JOIN sys.objects p ON p.object_id = t.parent_id " +
                        "JOIN sys.sql_modules m ON m.object_id = t.object_id " +
                        "WHERE t.parent_class = 1") { rs ->
                    triggers.getOrPut(rs.getString(1) to rs.getString(2)) { mutableListOf() } +=
                        TriggerDoc(rs.getString(3), rs.getString(4))
                }
                bestEffort("routines",
                    "SELECT SCHEMA_NAME(o.schema_id), o.name, o.type, m.definition " +
                        "FROM sys.objects o " +
                        "JOIN sys.sql_modules m ON m.object_id = o.object_id " +
                        "WHERE o.type IN ('P','FN','IF','TF')") { rs ->
                    val name = rs.getString(2)
                    routines += Triple(
                        rs.getString(1),
                        name, // T-SQL은 오버로드가 없어 specific=name
                        RoutineDoc(
                            name = name,
                            // type은 char(2)라 'P '처럼 후행 공백이 온다.
                            kind = if (rs.getString(3).trim() == "P") "procedure" else "function",
                            // T-SQL 본문 — 엔진이 BEGIN..END 껍질을 벗겨 파싱한다.
                            language = "sql",
                            body = rs.getString(4),
                        ),
                    )
                }
                bestEffort("routine parameters",
                    "SELECT SCHEMA_NAME(o.schema_id), o.name, TYPE_NAME(p.user_type_id) " +
                        "FROM sys.parameters p " +
                        "JOIN sys.objects o ON o.object_id = p.object_id " +
                        "WHERE o.type IN ('P','FN','IF','TF') AND p.parameter_id > 0 " +
                        "ORDER BY o.name, p.parameter_id") { rs ->
                    // parameter_id=0은 반환값 — MySQL의 ordinal=0과 같은 함정.
                    params.merge(rs.getString(1) to rs.getString(2),
                        rs.getString(3).lowercase()) { a, b -> "$a, $b" }
                }
            }
            else -> {
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
                // PG의 집계 함수는 ROUTINE_TYPE이 NULL이다 — null-safe로 받지
                // 않으면 NPE가 나서 이후 행 전부의 수확이 끊긴다.
                val rtype = rs.getString(4) ?: return@bestEffort
                routines += Triple(
                    rs.getString(1),
                    rs.getString(3),
                    RoutineDoc(
                        name = rs.getString(2),
                        kind = when (rtype.uppercase()) {
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
        }

        // 스키마 필터 — INFORMATION_SCHEMA 쿼리엔 필터를 못 넣어 수확 후 거른다.
        // 시스템 스키마도 여기서 거른다(MySQL의 sys는 information_schema에서도 온다).
        fun keep(schema: String) =
            !isSystem(schema) && (schemaFilter.isEmpty() || schema in schemaFilter)
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
            // MSSQL의 JDBC 메타는 이름을 `touch_customer;1`(numbered procedure
            // 명명)로 돌려준다 — sys.objects의 이름과 맞추려면 ;N을 떼야 한다.
            val normName = if (dialect == "sqlserver") name.substringBefore(';') else name
            // 키는 specific_name 우선 — 오버로드는 ROUTINE_NAME이 같아도 다르다.
            // meta 행이 먼저 이름키로 들어갔으면 특정키로 옮겨 흡수한다.
            val key = schema to (specific ?: normName)
            val stale = if (key != schema to normName) byKey.remove(schema to normName) else null
            val prior = byKey[key] ?: stale
            val sig = bodies.routineParams[key] ?: bodies.routineParams[schema to normName]
            byKey[key] = RoutineDoc(
                name = normName,
                kind = prior?.kind ?: kind,
                language = prior?.language ?: language,
                body = prior?.body ?: body,
                signature = prior?.signature ?: sig,
            )
        }

        fun accept(schema: String?): String? =
            schema?.takeIf {
                !isSystem(it) &&
                    (schemaFilter.isEmpty() || it in schemaFilter)
            }

        // SQLite엔 routine 개념이 없다 — getFunctions는 NPE를 던지는 드라이버라
        // 무의미한 limitation만 남으므로 아예 건너뛴다. MSSQL도 건너뛴다 —
        // getFunctions가 프로시저까지 함수로 돌려주고 이름에 ;N 접미사를 붙여
        // sys.objects 수확과 충돌·오분류만 만든다(kind는 sys.objects가 권위).
        // Oracle도 건너뛴다 — ALL_OBJECTS+ALL_SOURCE가 kind·몸체를 권위 있게
        // 주는데, getProcedures는 패키지 routine까지 섞어 귀속을 흐린다.
        if (dialect != "sqlite" && dialect != "sqlserver" && dialect != "oracle") {
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
        }

        // information_schema 수확분 — 이미 수확 단계에서 스키마·specific_name이 붙어 있다.
        for ((rschema, specific, doc) in bodies.routines) {
            merge(rschema, doc.name, doc.kind, doc.language, doc.body, specific)
        }

        return byKey.entries.groupBy({ it.key.first }, { it.value })
    }

    // ---- 사용 통계: 방언별 통계 뷰, 없는 방언은 빈 수확(0이 아니라 미수집) ----

    private data class UsageHarvest(
        val tables: Map<Pair<String, String>, UsageDoc>,
        val indexes: Map<Triple<String, String, String>, UsageDoc>,
        val functions: Map<Pair<String, String>, UsageDoc>,
    )

    /** 통계는 since 이후만 유효하다는 게 계약의 핵심 — 쿼리가 지원되는 방언만
     *  수확하고, 실패는 limitation으로 신고해 0으로 오독되지 않게 한다. */
    private fun collectUsage(): UsageHarvest {
        val tables = mutableMapOf<Pair<String, String>, UsageDoc>()
        val indexes = mutableMapOf<Triple<String, String, String>, UsageDoc>()
        val functions = mutableMapOf<Pair<String, String>, UsageDoc>()
        when (dialect) {
            "postgres" -> {
                // 테이블별 리셋 시각은 없다 — pg_stat_database.stats_reset이
                // 모든 카운터의 공통 하한(pg_stat_reset은 DB 전체를 리셋).
                // 리셋된 적 없는 클러스터에선 NULL이라 서버 기동 시각으로 폴백.
                val since = runCatching {
                    conn.createStatement().use { st ->
                        st.executeQuery(
                            "SELECT COALESCE(stats_reset::text, pg_postmaster_start_time()::text) " +
                                "FROM pg_stat_database WHERE datname = current_database()"
                        ).use { rs -> if (rs.next()) rs.getString(1) else null }
                    }
                }.getOrNull()
                bestEffort("table stats",
                    "SELECT schemaname, relname, " +
                        "seq_tup_read + COALESCE(idx_tup_fetch,0), " +
                        "n_tup_ins + n_tup_upd + n_tup_del " +
                        "FROM pg_stat_user_tables") { rs ->
                    tables[rs.getString(1) to rs.getString(2)] =
                        UsageDoc(since, rs.getLong(3), rs.getLong(4))
                }
                bestEffort("index stats",
                    "SELECT schemaname, relname, indexrelname, idx_scan " +
                        "FROM pg_stat_user_indexes") { rs ->
                    indexes[Triple(rs.getString(1), rs.getString(2), rs.getString(3))] =
                        UsageDoc(since, rs.getLong(4), 0)
                }
                // routine 호출 수 — calls를 reads에 싣는다(단위는 kind별로 다르다는
                // 계약). funcname엔 시그니처가 없어 오버로드 귀속은 extract()에서
                // 이름 유일성으로 판별한다(네이티브 reader와 같은 규칙).
                // track_functions=none이면 뷰가 0행(0 호출이 아니라 미수집)이라
                // 비어 있는 이유를 limitation으로 남긴다.
                val tracking = runCatching {
                    conn.createStatement().use { st ->
                        st.executeQuery("SELECT current_setting('track_functions')")
                            .use { rs -> if (rs.next()) rs.getString(1) else null }
                    }
                }.getOrNull()
                if (tracking == "none") {
                    limitations += "track_functions=none — routine usage 미수집(함수 통계 비활성)"
                } else {
                    bestEffort("function stats",
                        "SELECT schemaname, funcname, calls, total_time, self_time " +
                            "FROM pg_stat_user_functions") { rs ->
                        // track_calls는 track_functions와 별개 스위치 — 꺼져 있으면
                        // 시간 컬럼이 NULL이라 getObject+cast로 받는다.
                        functions[rs.getString(1) to rs.getString(2)] =
                            UsageDoc(
                                since, rs.getLong(3), 0,
                                rs.getObject(4) as? Double,
                                rs.getObject(5) as? Double,
                            )
                    }
                }
            }
            "mysql" -> {
                // MariaDB는 performance_schema=OFF가 기본값 — sys 통계 뷰는
                // 쿼리는 되지만 0행이라, 꺼져 있으면 그 이유를 limitation으로
                // 남기고 수확을 건너뛴다(0행을 "관측된 0"으로 오독하지 않기 위해).
                val pfs = runCatching {
                    conn.createStatement().use { st ->
                        st.executeQuery("SELECT CAST(@@performance_schema AS CHAR)")
                            .use { rs -> if (rs.next()) rs.getString(1) else null }
                    }
                }.getOrNull()
                if (pfs == "0" || pfs == "OFF") {
                    limitations += "performance_schema=OFF — usage 미수집(통계 비활성, MariaDB 기본값)"
                } else {
                    // performance_schema·sys는 재시작에 리셋된다 — uptime 역산이 since.
                    val since = runCatching {
                        conn.createStatement().use { st ->
                            st.executeQuery(
                                "SELECT NOW() - INTERVAL VARIABLE_VALUE SECOND " +
                                    "FROM performance_schema.global_status " +
                                    "WHERE VARIABLE_NAME='Uptime'"
                            ).use { rs -> if (rs.next()) rs.getString(1) else null }
                        }
                    }.getOrNull()
                    bestEffort("table stats",
                        "SELECT table_schema, table_name, rows_fetched, " +
                            "rows_inserted + rows_updated + rows_deleted " +
                            "FROM sys.schema_table_statistics") { rs ->
                        tables[rs.getString(1) to rs.getString(2)] =
                            UsageDoc(since, rs.getLong(3), rs.getLong(4))
                    }
                    // 재시작 이후 한 번도 안 쓰인 인덱스 명단 — 명단에 없는 것은
                    // 사용량이 모르는 것이지 0이 아니라서, 명단의 것만 0으로 싣는다.
                    bestEffort("unused indexes",
                        "SELECT object_schema, object_name, index_name " +
                            "FROM sys.schema_unused_indexes") { rs ->
                        indexes[Triple(rs.getString(1), rs.getString(2), rs.getString(3))] =
                            UsageDoc(since, 0, 0)
                    }
                }
            }
        }
        fun keep(schema: String) =
            !isSystem(schema) && (schemaFilter.isEmpty() || schema in schemaFilter)
        tables.keys.removeIf { !keep(it.first) }
        indexes.keys.removeIf { !keep(it.first) }
        functions.keys.removeIf { !keep(it.first) }
        return UsageHarvest(tables, indexes, functions)
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
