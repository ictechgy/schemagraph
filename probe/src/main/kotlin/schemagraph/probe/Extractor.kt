package schemagraph.probe

import com.fasterxml.jackson.databind.JsonNode
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

private val DB2_SYSTEM_SCHEMAS = setOf(
    "sysibm", "syscat", "sysstat", "sysibmadm", "systools", "nullid", "sqlj",
)

private val INFORMIX_SYSTEM_SCHEMAS = setOf(
    "sysmaster", "sysutils", "sysadmin", "sysuser", "syscdr", "syscdcv1",
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
        val dialectSystem = when (dialect) {
            "db2" -> s in DB2_SYSTEM_SCHEMAS
            "informix" -> s in INFORMIX_SYSTEM_SCHEMAS
            else -> false
        }
        return s in SYSTEM_SCHEMAS || dialectSystem || (dialect == "oracle" && s == "public")
    }

    fun extract(): CatalogDocument {
        // 읽기 전용 힌트 — sqlite-jdbc처럼 연결 후 변경을 거부하는 드라이버는
        // 건너뛴다(프로브는 어차피 SELECT만 친다).
        runCatching { conn.isReadOnly = true }
        val usage = collectUsage()
        val schemas = mutableListOf<SchemaDoc>()
        for (schema in candidateSchemas()) {
            extractSchema(schema, usage)?.let { schemas += it }
        }
        if (schemas.none { it.objects.isNotEmpty() }) {
            limitations += "JDBC 메타데이터가 테이블/뷰를 하나도 주지 않았다 — 드라이버 커버리지를 확인해라"
        }
        return CatalogDocument(
            version = DOCUMENT_VERSION,
            dialect = dialect,
            reader = "probe-jdbc",
            schemas = schemas,
            limitations = limitations.distinct().sorted(),
        )
    }

    /**
     * NDJSON 스트리밍 — 문서 전체를 메모리에 들지 않고 스키마 단위로
     * 수확→방출한다. 레코드 레이아웃은 engine/source/src/ndjson.rs와 약속이다:
     * document 헤더 → 스키마마다 schema 행 + object·routine 행 → 끝에
     * limitations 행. 헤더의 limitations는 비워 두고 트레일러가 진짜 목록을
     * 싣는다 — 스트리밍은 마지막까지 무슨 한계가 나올지 모르기 때문이다.
     */
    fun extractStreaming(documentVersion: Int = 1, emit: (String) -> Unit) {
        runCatching { conn.isReadOnly = true }
        fun rec(type: String, vararg fields: Pair<String, Any?>) {
            val node = lineMapper.createObjectNode()
            node.put("type", type)
            for ((k, v) in fields) node.set<JsonNode>(k, lineMapper.valueToTree(v))
            emit(lineMapper.writeValueAsString(node))
        }
        emit(lineMapper.writeValueAsString(streamingHeader(documentVersion, dialect)))
        val usage = collectUsage()
        var sawObjects = false
        for (schema in candidateSchemas()) {
            val sd = extractSchema(schema, usage) ?: continue
            sawObjects = sawObjects || sd.objects.isNotEmpty()
            rec("schema", "name" to sd.name)
            for (obj in sd.objects) rec("object", "schema" to sd.name, "data" to obj)
            for (rt in sd.routines) rec("routine", "schema" to sd.name, "data" to rt)
        }
        if (!sawObjects) {
            limitations += "JDBC 메타데이터가 테이블/뷰를 하나도 주지 않았다 — 드라이버 커버리지를 확인해라"
        }
        rec("limitations", "data" to limitations.distinct().sorted())
    }

    /** 스키마 하나분의 수확 — 스트리밍 방출의 버퍼 단위. 내용이 없는
     *  스키마는 null이다(빈 스키마 정점을 만들지 않는다 — 옛 schemaNames가
     *  객체·routine의 합집합이던 계약과 같다). */
    private fun extractSchema(schema: String, usage: UsageHarvest): SchemaDoc? {
        val bodies = collectBodies(schema)
        val objects = rawObjectsOf(schema).map { raw ->
            val columns = columnsOf(raw)
            val (constraints, pkPos) = constraintsOf(raw)
            ObjectDoc(
                name = raw.name,
                kind = raw.kind,
                columns = columns.map { it.copy(pkPosition = pkPos[it.name] ?: 0) }
                    .sortedBy { it.ordinal },
                constraints = constraints,
                // view에 getIndexInfo를 부르면 Oracle이 ORA-20000(DBMS_STATS)을
                // 던진다 — 인덱스는 테이블·물화뷰에만 있는 게 보통이라 거기만 수확.
                indexes = if (raw.kind == "table" || raw.kind == "materialized-view")
                    indexesOf(raw).map { idx ->
                        idx.copy(usage = usage.indexes[Triple(schema, raw.name, idx.name)])
                    }
                else emptyList(),
                triggers = bodies.triggers[schema to raw.name].orEmpty().sortedBy { it.name },
                body = bodies.views[schema to raw.name],
                usage = usage.tables[schema to raw.name],
            )
        }.sortedBy { it.name }
        val routines = routinesOf(schema, bodies, usage)
        if (objects.isEmpty() && routines.isEmpty()) return null
        return SchemaDoc(name = schema, objects = objects, routines = routines)
    }

    /** 방출할 스키마 후보 — getSchemas가 비는 드라이버(MySQL 계열: 스키마가
     *  카탈로그)는 카탈로그 목록과 전역 테이블 스캔의 합집합으로 채운다. */
    private var schemaViaCatalog = false

    private fun candidateSchemas(): List<String> {
        val discovered = discoveredSchemas()
        // getSchemas가 일부를 빠뜨리는 드라이버가 있어 테이블 행이 보고한
        // 스키마도 후보에 합산한다 — 옛 전역 수집과 같은 커버리지 보장.
        val fromTables = readTables(null, null).map { it.schema }
        if (discovered.isEmpty()) {
            // 스키마가 카탈로그인 계열(MySQL)은 카탈로그 목록도 후보다.
            schemaViaCatalog = true
            val catalogs = runCatching {
                meta.catalogs.use { rs -> buildList { while (rs.next()) add(rs.getString(1)) } }
            }.getOrElse { emptyList() }
            return (catalogs + fromTables)
                .filter { !isSystem(it) && (schemaFilter.isEmpty() || it in schemaFilter) }
                .distinct().sorted()
        }
        return (discovered + fromTables)
            .filter { !isSystem(it) && (schemaFilter.isEmpty() || it in schemaFilter) }
            .distinct().sorted()
    }

    /** 스키마 한정 객체 목록 — 카탈로그가 스키마인 드라이버는 catalog
     *  인자로, 나머지는 schemaPattern으로 긁는다. */
    private fun rawObjectsOf(schema: String): List<RawObject> = when (dialect) {
        "db2" -> db2Objects(conn, schema, limitations).map {
            RawObject(schema, null, schema, it.name, it.kind)
        }
        "informix" -> informixObjects(conn, schema, limitations).map {
            RawObject(schema, null, schema, it.name, it.kind)
        }
        else -> (if (schemaViaCatalog) readTables(schema, null) else readTables(null, schema))
            .filter { it.schema == schema }
    }

    // ---- 객체 수집: TABLE/VIEW 계열만 정점으로, 시스템 테이블은 버린다 ----

    private data class RawObject(
        val schema: String, val catalog: String?, val schemaName: String?,
        val name: String, val kind: String,
    )

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

    /** SQL 리터럴 인용 — 스키마 이름은 서버가 준 값이라 위험이 없지만
     *  작은따옴표는 이스케이프한다. */
    private fun q(s: String): String = "'" + s.replace("'", "''") + "'"

    private fun collectBodies(schema: String): BodyHarvest {
        val views = mutableMapOf<Pair<String, String>, String>()
        val triggers = mutableMapOf<Pair<String, String>, MutableList<TriggerDoc>>()
        val routines = mutableListOf<Triple<String, String, RoutineDoc>>()
        val params = mutableMapOf<Pair<String, String>, String>()
        val sq = q(schema)

        if (dialect == "db2") {
            val specialized = db2Bodies(conn, schema, limitations)
            return BodyHarvest(
                specialized.views,
                specialized.triggers,
                specialized.routines,
                specialized.routineParams,
            )
        }
        if (dialect == "informix") {
            val specialized = informixBodies(conn, schema, limitations)
            return BodyHarvest(
                specialized.views,
                specialized.triggers,
                specialized.routines,
                specialized.routineParams,
            )
        }

        when (dialect) {
            "oracle" -> {
                // Oracle엔 INFORMATION_SCHEMA가 없다 — ALL_* 딕셔너리로 대체.
                // TEXT/TRIGGER_BODY/QUERY는 LONG이라 스트림 주의가 필요하지만
                // ojdbc는 getString으로 읽어준다.
                bestEffort("views",
                    "SELECT OWNER, VIEW_NAME, TEXT FROM ALL_VIEWS WHERE OWNER = $sq") { rs ->
                    views[rs.getString(1) to rs.getString(2)] = rs.getString(3) ?: return@bestEffort
                }
                bestEffort("materialized views",
                    "SELECT OWNER, MVIEW_NAME, QUERY FROM ALL_MVIEWS WHERE OWNER = $sq") { rs ->
                    views[rs.getString(1) to rs.getString(2)] = rs.getString(3) ?: return@bestEffort
                }
                bestEffort("triggers",
                    "SELECT OWNER, TABLE_NAME, TRIGGER_NAME, TRIGGER_BODY FROM ALL_TRIGGERS " +
                        "WHERE OWNER = $sq") { rs ->
                    val key = rs.getString(1) to rs.getString(2)
                    triggers.getOrPut(key) { mutableListOf() } +=
                        TriggerDoc(rs.getString(3), rs.getString(4))
                }
                // routine — ALL_OBJECTS가 kind를, ALL_SOURCE가 LINE순 몸체를 준다.
                // PACKAGE는 스펙과 BODY를 따로 보관한다 — 멤버 몸체는 BODY
                // 텍스트를 멤버 헤더로 나눠 귀속하고, 경계를 못 찾은 멤버는
                // body 없이 limitation으로 센다(추측 귀속 금지).
                val oracleBodies = mutableMapOf<Triple<String, String, String>, StringBuilder>()
                bestEffort("routines",
                    "SELECT o.OWNER, o.OBJECT_NAME, o.OBJECT_TYPE, s.LINE, s.TEXT " +
                        "FROM ALL_OBJECTS o " +
                        "JOIN ALL_SOURCE s ON s.OWNER = o.OWNER " +
                        "  AND s.NAME = o.OBJECT_NAME AND s.TYPE = o.OBJECT_TYPE " +
                        "WHERE o.OBJECT_TYPE IN ('PROCEDURE','FUNCTION','PACKAGE','PACKAGE BODY') " +
                        "  AND o.OWNER = $sq " +
                        "ORDER BY o.OWNER, o.OBJECT_NAME, s.LINE") { rs ->
                    val key = Triple(rs.getString(1), rs.getString(2), rs.getString(3))
                    oracleBodies.getOrPut(key) { StringBuilder() }.append(rs.getString(5))
                }
                // 패키지 멤버 목록 — ALL_PROCEDURES가 카탈로그상 멤버를
                // SUBPROGRAM_ID(선언 순)로 준다. OVERLOAD는 같은 이름의
                // 오버로드를 구분한다(NULL이면 비오버로드).
                val pkgMembers = mutableMapOf<Pair<String, String>, MutableList<Pair<String, String?>>>()
                bestEffort("package members",
                    "SELECT OWNER, OBJECT_NAME, PROCEDURE_NAME, OVERLOAD " +
                        "FROM ALL_PROCEDURES WHERE PROCEDURE_NAME IS NOT NULL " +
                        "AND OWNER = $sq " +
                        "ORDER BY OWNER, OBJECT_NAME, SUBPROGRAM_ID") { rs ->
                    pkgMembers.getOrPut(rs.getString(1) to rs.getString(2)) { mutableListOf() } +=
                        (rs.getString(3) to rs.getString(4))
                }
                // 멤버 시그니처와 kind — PACKAGE_NAME이 있는 인자 행.
                // POSITION=0은 반환값이라 함수의 표시다.
                val memberParams = mutableMapOf<Triple<String, String, String>, String>()
                val memberFunctions = mutableSetOf<Triple<String, String, String>>()
                bestEffort("package member arguments",
                    "SELECT OWNER, PACKAGE_NAME, OBJECT_NAME, DATA_TYPE, POSITION, OVERLOAD " +
                        "FROM ALL_ARGUMENTS WHERE PACKAGE_NAME IS NOT NULL " +
                        "AND OWNER = $sq " +
                        "ORDER BY OWNER, PACKAGE_NAME, OBJECT_NAME, OVERLOAD, SEQUENCE") { rs ->
                    val key = Triple(rs.getString(1), rs.getString(2),
                        rs.getString(3) + (rs.getString(6)?.let { "#$it" } ?: ""))
                    if (rs.getInt(5) == 0) memberFunctions += key
                    else memberParams.merge(key, rs.getString(4).lowercase()) { a, b -> "$a, $b" }
                }
                val specBodies = mutableMapOf<Pair<String, String>, String>()
                val implBodies = mutableMapOf<Pair<String, String>, String>()
                for ((key, body) in oracleBodies) {
                    when (key.third) {
                        "PACKAGE" -> specBodies[key.first to key.second] = body.toString()
                        "PACKAGE BODY" -> implBodies[key.first to key.second] = body.toString()
                        else -> routines += Triple(key.first, key.second, RoutineDoc(
                            name = key.second,
                            kind = key.third.lowercase(),
                            // PL/SQL — plpgsql과 같은 계약이라 엔진이 문장 추출로 파싱한다.
                            language = "plsql",
                            body = body.toString(),
                        ))
                    }
                }
                for (pkg in (specBodies.keys + implBodies.keys).sortedBy { it.second }) {
                    val (owner, pname) = pkg
                    val spec = specBodies[pkg]
                    val impl = implBodies[pkg]
                    val members = if (impl != null) pkgMembers[pkg].orEmpty() else emptyList()
                    val memberDocs = mutableListOf<Triple<String, String, RoutineDoc>>()
                    if (impl != null && members.isNotEmpty()) {
                        val slices = slicePackageBodyLexical(impl, members.map { it.first }.toSet())
                        val seen = mutableMapOf<String, Int>()
                        for ((mname, overload) in members) {
                            // 같은 이름의 n번째 오버로드는 본문의 n번째 헤더와 짝짓는다.
                            val nth = seen.merge(mname, 1) { a, b -> a + b }!! - 1
                            val ov = overload?.let { "#$it" } ?: ""
                            // specific 키는 `pkg.m#ov` — 같은 이름의 독립 routine이나
                            // 오버로드된 형제 멤버와 병합 키가 충돌하지 않게 한다.
                            memberDocs += Triple(owner, "$pname.$mname$ov", RoutineDoc(
                                name = mname,
                                kind = if (Triple(owner, pname, "$mname$ov") in memberFunctions)
                                    "function" else "procedure",
                                language = "plsql",
                                body = slices["$mname#$nth"],
                                signature = memberParams[Triple(owner, pname, "$mname$ov")],
                                memberOf = pname,
                            ))
                            if (slices["$mname#$nth"] == null) {
                                limitations += "$owner.$pname.$mname: 패키지 본문에서 멤버 경계를 " +
                                    "못 찾음 — 해당 멤버의 몸체 간선 없음"
                            }
                        }
                    }
                    // 멤버를 냈으면 패키지 몸체는 스펙이 대표다 — 실행 문장은
                    // 멤버 몸체에 있다. 못 냈으면 옛 동작(BODY 통째 귀속)을 유지.
                    routines += Triple(owner, pname, RoutineDoc(
                        name = pname,
                        kind = "package",
                        language = "plsql",
                        body = if (memberDocs.isNotEmpty()) spec ?: impl else impl ?: spec,
                    ))
                    routines += memberDocs
                }
                // 파라미터 — POSITION=0은 반환값이라 제외(MySQL ordinal=0 함정과 같다).
                bestEffort("routine parameters",
                    "SELECT OWNER, OBJECT_NAME, DATA_TYPE FROM ALL_ARGUMENTS " +
                        "WHERE POSITION > 0 AND PACKAGE_NAME IS NULL " +
                        "AND OWNER = $sq " +
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
                        "JOIN sys.sql_modules m ON m.object_id = o.object_id " +
                        "WHERE SCHEMA_NAME(o.schema_id) = $sq") { rs ->
                    views[rs.getString(1) to rs.getString(2)] =
                        rs.getString(3) ?: return@bestEffort
                }
                bestEffort("triggers",
                    "SELECT SCHEMA_NAME(p.schema_id), p.name, t.name, m.definition " +
                        "FROM sys.triggers t " +
                        "JOIN sys.objects p ON p.object_id = t.parent_id " +
                        "JOIN sys.sql_modules m ON m.object_id = t.object_id " +
                        "WHERE t.parent_class = 1 AND SCHEMA_NAME(p.schema_id) = $sq") { rs ->
                    triggers.getOrPut(rs.getString(1) to rs.getString(2)) { mutableListOf() } +=
                        TriggerDoc(rs.getString(3), rs.getString(4))
                }
                bestEffort("routines",
                    "SELECT SCHEMA_NAME(o.schema_id), o.name, o.type, m.definition " +
                        "FROM sys.objects o " +
                        "JOIN sys.sql_modules m ON m.object_id = o.object_id " +
                        "WHERE o.type IN ('P','FN','IF','TF') " +
                        "  AND SCHEMA_NAME(o.schema_id) = $sq") { rs ->
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
                        "  AND SCHEMA_NAME(o.schema_id) = $sq " +
                        "ORDER BY o.name, p.parameter_id") { rs ->
                    // parameter_id=0은 반환값 — MySQL의 ordinal=0과 같은 함정.
                    params.merge(rs.getString(1) to rs.getString(2),
                        rs.getString(3).lowercase()) { a, b -> "$a, $b" }
                }
            }
            else -> {
            bestEffort("views",
                "SELECT TABLE_SCHEMA, TABLE_NAME, VIEW_DEFINITION FROM INFORMATION_SCHEMA.VIEWS " +
                    "WHERE TABLE_SCHEMA = $sq") { rs ->
                views[rs.getString(1) to rs.getString(2)] = rs.getString(3) ?: return@bestEffort
            }
            bestEffort("triggers",
                "SELECT TRIGGER_SCHEMA, EVENT_OBJECT_TABLE, TRIGGER_NAME, ACTION_STATEMENT " +
                    "FROM INFORMATION_SCHEMA.TRIGGERS WHERE TRIGGER_SCHEMA = $sq") { rs ->
                val key = rs.getString(1) to rs.getString(2)
                triggers.getOrPut(key) { mutableListOf() } +=
                    TriggerDoc(rs.getString(3), rs.getString(4))
            }
            bestEffort("routines",
                "SELECT ROUTINE_SCHEMA, ROUTINE_NAME, SPECIFIC_NAME, ROUTINE_TYPE, " +
                    "ROUTINE_DEFINITION, EXTERNAL_LANGUAGE FROM INFORMATION_SCHEMA.ROUTINES " +
                    "WHERE ROUTINE_SCHEMA = $sq") { rs ->
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
                    "AND SPECIFIC_SCHEMA = $sq " +
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

    /** getFunctions/getProcedures가 주는 (스키마, 이름, kind) — 몸체 없는
     *  메타 행이라 한 번만 긁어 스키마별로 재사용한다.
     *
     *  SQLite엔 routine 개념이 없다 — getFunctions는 NPE를 던지는 드라이버라
     *  무의미한 limitation만 남으므로 아예 건너뛴다. MSSQL도 건너뛴다 —
     *  getFunctions가 프로시저까지 함수로 돌려주고 이름에 ;N 접미사를 붙여
     *  sys.objects 수확과 충돌·오분류만 만든다(kind는 sys.objects가 권위).
     *  Oracle도 건너뛴다 — ALL_OBJECTS+ALL_SOURCE가 kind·몸체를 권위 있게
     *  주는데, getProcedures는 패키지 routine까지 섞어 귀속을 흐린다. */
    private data class MetaRoutine(val schema: String, val name: String, val kind: String)

    private val metaRoutineRows: List<MetaRoutine> by lazy {
        val rows = mutableListOf<MetaRoutine>()
        fun accept(schema: String?): String? =
            schema?.takeIf {
                !isSystem(it) && (schemaFilter.isEmpty() || it in schemaFilter)
            }
        if (dialect != "sqlite" && dialect != "sqlserver" && dialect != "oracle" &&
            dialect != "db2" && dialect != "informix") {
            runCatching {
                meta.getFunctions(null, null, null).use { rs ->
                    while (rs.next()) {
                        accept(rs.strOrNull("FUNCTION_SCHEM") ?: rs.strOrNull("FUNCTION_CAT"))
                            ?.let { rows += MetaRoutine(it, rs.getString("FUNCTION_NAME"), "function") }
                    }
                }
            }.onFailure { limitations += "getFunctions 실패: ${it.message}" }
            runCatching {
                meta.getProcedures(null, null, null).use { rs ->
                    while (rs.next()) {
                        accept(rs.strOrNull("PROCEDURE_SCHEM") ?: rs.strOrNull("PROCEDURE_CAT"))
                            ?.let { rows += MetaRoutine(it, rs.getString("PROCEDURE_NAME"), "procedure") }
                    }
                }
            }.onFailure { limitations += "getProcedures 실패: ${it.message}" }
        }
        rows
    }

    /** 스키마 하나의 routine — 메타 행과 몸체 수확분을 키로 병합하고
     *  usage를 귀속한다(오버로드는 funcname만으로 구분 못 해 미수집). */
    private fun routinesOf(schema: String, bodies: BodyHarvest, usage: UsageHarvest): List<RoutineDoc> {
        val byKey = linkedMapOf<Pair<String, String>, RoutineDoc>()

        fun merge(name: String, kind: String, language: String?, body: String?, specific: String?,
                  signature: String? = null, memberOf: String? = null) {
            // MSSQL의 JDBC 메타는 이름을 `touch_customer;1`(numbered procedure
            // 명명)로 돌려준다 — sys.objects의 이름과 맞추려면 ;N을 떼야 한다.
            val normName = if (dialect == "sqlserver") name.substringBefore(';') else name
            // 키는 specific_name 우선 — 오버로드는 ROUTINE_NAME이 같아도 다르다.
            // meta 행이 먼저 이름키로 들어갔으면 특정키로 옮겨 흡수한다.
            val key = schema to (specific ?: normName)
            // 멤버 행의 키는 `pkg.m` 형태라 이름키 흡수를 하면 같은 이름의
            // 독립 routine이 통째로 멤버 문서에 흡수된다 — 멤버는 흡수 안 한다.
            val stale = if (memberOf == null && key != schema to normName)
                byKey.remove(schema to normName) else null
            val prior = byKey[key] ?: stale
            val sig = bodies.routineParams[key] ?: bodies.routineParams[schema to normName]
            byKey[key] = RoutineDoc(
                name = normName,
                kind = prior?.kind ?: kind,
                language = prior?.language ?: language,
                body = prior?.body ?: body,
                signature = prior?.signature ?: signature ?: sig,
                // memberOf·signature는 수확 단계에서만 온다 — 재조립 때 떨구면
                // 멤버 귀속과 오버로드 구분이 함께 사라진다.
                memberOf = prior?.memberOf ?: memberOf,
            )
        }

        for (row in metaRoutineRows) {
            if (row.schema == schema) merge(row.name, row.kind, null, null, null)
        }
        // information_schema 수확분 — 이미 수확 단계에서 스키마·specific_name이 붙어 있다.
        for ((rschema, specific, doc) in bodies.routines) {
            if (rschema != schema) continue
            merge(doc.name, doc.kind, doc.language, doc.body, specific,
                signature = doc.signature, memberOf = doc.memberOf)
        }

        val rts = byKey.values.toList()
        val nameCounts = rts.groupingBy { it.name }.eachCount()
        // funcname엔 시그니처가 없다 — 같은 이름의 오버로드가 있으면
        // 어느 것의 calls인지 모르므로 미귀속(네이티브와 같은 규칙).
        val ambiguous = rts.count {
            nameCounts[it.name] != 1 && usage.functions.containsKey(schema to it.name)
        }
        if (ambiguous > 0) {
            limitations += "$schema: 오버로드된 routine ${ambiguous}개는 " +
                "funcname만으로 귀속 못 해 usage 미수집"
        }
        return rts.map { rt ->
            rt.copy(usage = usage.functions[schema to rt.name].takeIf { nameCounts[rt.name] == 1 })
        }.sortedWith(compareBy({ it.name }, { it.signature ?: "" }))
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
