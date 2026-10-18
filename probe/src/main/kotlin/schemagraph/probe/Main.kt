package schemagraph.probe

import java.io.BufferedWriter
import java.io.File
import java.io.IOException
import java.io.PrintStream
import java.net.URLClassLoader
import java.nio.file.Files
import java.nio.file.InvalidPathException
import java.nio.file.Path
import java.sql.Driver
import java.sql.DriverManager
import java.sql.SQLException
import java.util.Properties
import java.util.ServiceLoader
import kotlin.system.exitProcess

// schemagraph-probe — JDBC로 카탈로그를 긁어 catalog document(JSON)를 뱉는
// "멍청한 추출기". 의미 해석은 엔진의 일이라 여기선 옮기기만 한다.
//
//   java -jar schemagraph-probe.jar --url jdbc:h2:file:/tmp/db -o catalog.json
//   schemagraph scan --document catalog.json -o graph.json
//
// 번들 드라이버(pgjdbc·H2·SQLite) 외의 DB는 --driver로 jar을 넘긴다.

private const val USAGE = """
usage: schemagraph-probe --url <jdbc-url> [options]

  --url <url>          JDBC URL (jdbc:postgresql:…, jdbc:h2:…, jdbc:sqlite:…, …)
  --user <name>        DB user (optional)
  --password <pw>      DB password (optional — env var SG_DB_PASSWORD도 가능)
  --driver <paths>     extra driver jars, comma-separated (oracle/mssql/mysql…)
  --driver-class <fqc> driver class name when ServiceLoader can't find it
  --schema <names>     comma-separated schema allowlist (default: non-system all)
  -o, --output <path>  catalog document path (default: catalog.json, '-' stdout)
  --format <json|ndjson>  output format (default: json — ndjson은 행 단위 스트리밍)
  --document-version <1|2>  catalog wire version (default: 1)
  --catalog-dependencies   collect DB dependency catalog facts (PostgreSQL/Oracle/SQL Server)
  --source-id <label>      logical source label for comparing snapshots (never a URL)
  -h, --help           this help
"""

/** fat jar에 번들된 드라이버 — jar 안 서비스 파일은 병합에서 빠질 수 있어
 *  Class.forName으로 정적 초기화(자기 등록)를 명시적으로 일으킨다. */
private val BUNDLED_DRIVERS = listOf(
    "org.postgresql.Driver",
    "org.h2.Driver",
    "org.sqlite.JDBC",
    "com.microsoft.sqlserver.jdbc.SQLServerDriver",
)

fun main(args: Array<String>) {
    runProbe(args)?.let { exitProcess(it) }
}

/**
 * CLI 본체. fatal은 ProbeExit를 던지고 여기서 stderr에 써서 종료 코드 2를 돌려준다. exitProcess
 * 없이 부를 수 있어야 테스트가 실제 오류 경로의 stderr를 확인한다. 정상 수집은 null이라 main이
 * 지금처럼 그냥 반환한다(도움말은 0, 인자 오류는 2로 지금처럼 종료한다).
 */
internal fun runProbe(args: Array<String>, stderr: PrintStream = System.err): Int? = try {
    probe(args, stderr)
} catch (exit: ProbeExit) {
    stderr.println("error: ${exit.message}")
    2
}

private fun probe(args: Array<String>, stderr: PrintStream): Int? {
    val opts = parseArgs(args)
    if (opts.help || opts.url == null) {
        stderr.println(if (opts.url == null && !opts.help) "error: --url이 필요하다\n$USAGE" else USAGE)
        return if (opts.help) 0 else 2
    }
    val url = opts.url!!

    for (name in BUNDLED_DRIVERS) runCatching { Class.forName(name) }
    runCatching { registerDrivers(opts.driverJars, opts.driverClass) }
        .onFailure { fatal("드라이버 로딩 실패: ${it.message}") }

    val dialect = dialectOf(url)
    if (dialect == "sqlite") sqliteTargetError(url)?.let { fatal(it) }
    // 출력 경로는 연결 전에 정한다. SQLite 원본을 가리키면 DB를 열기 전에 끝낸다.
    val target = if (opts.output == "-") null else fileOutput(opts.output, url, dialect)
    val props = connectionProperties(dialect, opts.user, opts.password ?: System.getenv("SG_DB_PASSWORD"))
    // --password가 환경 변수를 이겨도 두 값 모두 비밀이라 오류에서 함께 가린다.
    val secrets = listOfNotNull(opts.password, System.getenv("SG_DB_PASSWORD"))
    if (opts.format == "ndjson") {
        // 진짜 스트리밍 — 문서를 전부 들지 않고 스키마 단위로 수확→방출한다. 파일은 트레일러까지
        // 쓴 임시 파일만 교체해, 끊긴 수집이 이전 스냅샷을 자르지 않게 한다.
        runCatching {
            DriverManager.getConnection(url, props).use { conn ->
                val extractor = Extractor(conn, dialect, opts.schemas, opts.catalogDependencies, opts.sourceId)
                if (target == null) streamDocument(extractor, opts.documentVersion, System.out.bufferedWriter())
                else writeOutput(target) { writer -> streamDocument(extractor, opts.documentVersion, writer) }
            }
        }.getOrElse { fatal("추출 실패: ${connectionFailure(it, url, secrets)}") }
        return null
    }
    if (opts.format != "json") fatal("--format은 json|ndjson 중 하나다: ${opts.format}")
    val doc = runCatching {
        DriverManager.getConnection(url, props).use { conn ->
            Extractor(conn, dialect, opts.schemas, opts.catalogDependencies, opts.sourceId).extract()
        }
    }.getOrElse { fatal("추출 실패: ${connectionFailure(it, url, secrets)}") }

    val json = mapper.writeValueAsString(documentWire(doc, opts.documentVersion))
    if (target == null) println(json)
    else writeOutput(target) { writer -> writer.write("$json\n") }
    return null
}

/** '-'가 아닌 -o를 연결 전에 해석하고 SQLite 원본 별칭을 거절한다. */
private fun fileOutput(output: String, url: String, dialect: String): OutputTarget {
    val (target, aliasError) = try {
        val prepared = prepareOutput(output)
        prepared to if (dialect == "sqlite") sqliteOutputAliasError(url, prepared) else null
    } catch (error: IOException) {
        fatal("invalid output path: $error")
    } catch (error: InvalidPathException) {
        fatal("invalid output path: ${error.reason}; check the path")
    }
    aliasError?.let { fatal("invalid output path: $it") }
    return target
}

/** NDJSON 행을 writer에 쓰고 flush한다. 파일 출력은 writeOutput이 트레일러까지 쓴 뒤에만 교체한다. */
private fun streamDocument(extractor: Extractor, documentVersion: Int, writer: BufferedWriter) {
    extractor.extractStreaming(documentVersion) { line ->
        writer.write(line)
        writer.newLine()
    }
    writer.flush()
}

/**
 * 방언별 연결 속성 — 프로브는 카탈로그만 읽으므로 드라이버가 지원하는 읽기 전용 모드로
 * 연다. 연결 뒤 setReadOnly 힌트는 sqlite-jdbc가 거부하고, pgjdbc 기본값
 * (readOnlyMode=transaction)은 autocommit 세션에 적용하지 않기 때문이다.
 */
internal fun connectionProperties(dialect: String, user: String?, password: String?): Properties {
    val props = Properties()
    user?.let { props["user"] = it }
    password?.let { props["password"] = it }
    when (dialect) {
        // setReadOnly(true)가 SET SESSION CHARACTERISTICS AS TRANSACTION READ ONLY를 보내게 한다.
        "postgres" -> props["readOnlyMode"] = "always"
        // sqlite-jdbc SQLiteConfig는 open_mode 값을 sqlite3_open_v2 플래그로 쓰고 OPEN_URI만
        // 더한다. SQLITE_OPEN_READONLY(1)만 주면 CREATE가 빠져 없는 경로에 빈 DB를 만들지
        // 않고, 연결을 닫을 때 -wal을 원본 파일에 체크포인트하지 않는다. 연결 속성이 URL의
        // 같은 키보다 우선한다. sqlite-jdbc는 runtimeOnly라 SQLiteConfig 대신 문자열 키를 쓴다.
        "sqlite" -> props["open_mode"] = "1"
        // 드라이버 메타데이터 조회(getTables·getIndexInfo 등)까지 왕복당 행 수를 늘린다.
        "oracle" -> props["defaultRowPrefetch"] = ORACLE_FETCH_ROWS.toString()
    }
    return props
}

/**
 * SQLite 대상이 이미 있는 파일인지 연결 전에 확인해 사용자에게 보일 오류 문구를 돌려준다
 * (문제없으면 null). 읽기 전용으로 열면 드라이버도 거부하지만, 경로 오타를 원인과 해결
 * 방향이 담긴 문구로 먼저 알린다. file: URI 파일명은 해석을 SQLite에 맡긴다.
 */
internal fun sqliteTargetError(url: String): String? {
    val target = url.removePrefix("jdbc:sqlite:").substringBefore('?')
    if (target.startsWith("file:")) return null
    if (target.isEmpty() || target.startsWith(":")) {
        return "SQLite probe requires an existing database file; in-memory and resource databases are not supported"
    }
    val path = try {
        Path.of(target)
    } catch (error: InvalidPathException) {
        return "SQLite database path is invalid (${error.reason}); check the path"
    }
    return if (Files.isRegularFile(path)) null
    else "SQLite database file not found or not a regular file; check the path"
}

/** 사용자에게 보일 오류. runProbe가 "error: …"로 쓰고 종료 코드 2로 끝낸다. */
private class ProbeExit(message: String) : RuntimeException(message)

private fun fatal(message: String): Nothing = throw ProbeExit(message)

private const val REDACTED = "<redacted>"

/** jdbc:와 subprotocol만 안내에 싣는다 — 이 모양이 아니면 원문 일부일 수 있어 싣지 않는다. */
private val SUBPROTOCOL = Regex("[A-Za-z][A-Za-z0-9+._-]{0,63}")

/** 드라이버가 URL 일부만 되풀이한 모양: //user:pw@, user/pw@(Oracle thin), (…)password=값. */
private val URL_USERINFO = Regex("//[^/@\\s]+@")
private val SLASH_CREDENTIALS = Regex("[^\\s/:@;=]+/[^\\s/@]*@")
private val PASSWORD_VALUE = Regex("(?i)(password\\s*=\\s*)[^;&\\s]*")

/**
 * 연결·추출 실패를 사용자에게 보일 문구로 바꾼다. DriverManager는 받아 주는 드라이버가 없으면
 * "No suitable driver found for <URL 전체>"(SQLState 08001)를 던지므로, 번들되지 않은 Oracle·MySQL에서
 * --driver를 빠뜨리는 흔한 실수가 URL 속 자격 증명을 로그에 남겼다. 이때는 subprotocol과 --driver를
 * 안내하고, 나머지 메시지는 scrubSecrets로 가린다.
 */
internal fun connectionFailure(error: Throwable, url: String, secrets: List<String>): String {
    if (error is SQLException && error.sqlState == "08001" && error.message.orEmpty().startsWith("No suitable driver")) {
        val subprotocol = url.removePrefix("jdbc:").substringBefore(':')
        return if (SUBPROTOCOL.matches(subprotocol)) "no JDBC driver accepts subprotocol '$subprotocol'; pass its driver jar with --driver"
        else "no JDBC driver accepts this URL; pass its driver jar with --driver"
    }
    return scrubSecrets("${error.message}", url, secrets)
}

/**
 * 드라이버 메시지에서 자격 증명을 가린다. Go 프로브의 "오류에 비밀번호가 든 원문을 싣지 않는다"는
 * 계약과 Scripts/verify-probe-versions.py의 scrub을 프로브 안으로 옮긴 것이다 — 스크립트 밖에서 직접
 * 실행해도 지켜져야 한다. URL·비밀번호 원문을 긴 것부터 지운 뒤, URL 일부만 되풀이한 모양을 가린다.
 */
internal fun scrubSecrets(message: String, url: String, secrets: List<String>): String {
    val literals = (listOf(url, url.removePrefix("jdbc:")) + secrets).filter { it.isNotEmpty() }.sortedByDescending { it.length }
    val withoutLiterals = literals.fold(message) { text, literal -> text.replace(literal, REDACTED) }
    val withoutUserinfo = URL_USERINFO.replace(withoutLiterals, "//$REDACTED@")
    val withoutSlashCredentials = SLASH_CREDENTIALS.replace(withoutUserinfo, "$REDACTED@")
    return PASSWORD_VALUE.replace(withoutSlashCredentials) { it.groupValues[1] + REDACTED }
}

private data class Opts(
    var url: String? = null,
    var user: String? = null,
    var password: String? = null,
    var driverJars: List<String> = emptyList(),
    var driverClass: String? = null,
    var schemas: List<String> = emptyList(),
    var output: String = "catalog.json",
    var format: String = "json",
    var documentVersion: Int = 1,
    var catalogDependencies: Boolean = false,
    var sourceId: String = "",
    var help: Boolean = false,
)

private fun parseArgs(args: Array<String>): Opts {
    val o = Opts()
    var i = 0
    fun next(flag: String): String =
        args.getOrElse(++i) { fatal("error: ${flag}에 값이 없다\n$USAGE") }
    while (i < args.size) {
        when (val a = args[i]) {
            "--url" -> o.url = next(a)
            "--user" -> o.user = next(a)
            "--password" -> o.password = next(a)
            "--driver" -> o.driverJars = next(a).split(',').filter { it.isNotBlank() }
            "--driver-class" -> o.driverClass = next(a)
            "--schema" -> o.schemas = next(a).split(',').filter { it.isNotBlank() }
            "--catalog-dependencies" -> o.catalogDependencies = true
            "--source-id" -> o.sourceId = next(a).also { value -> if (!value.matches(Regex("[A-Za-z0-9_.\\-/]{1,128}"))) fatal("source id must be a logical label, not a connection URL") }
            "-o", "--output" -> o.output = next(a)
            "--format" -> o.format = next(a)
            "--document-version" -> o.documentVersion = next(a).toIntOrNull()?.takeIf { it in 1..2 }
                ?: fatal("Unsupported document version; choose 1 or 2")
            "-h", "--help" -> o.help = true
            else -> fatal("error: 알 수 없는 인자 '$a'\n$USAGE")
        }
        i++
    }
    return o
}

/** jdbc:<subprotocol>:… 에서 방언 이름을 뽑는다. 알 수 없는 방언은 subprotocol
 *  이름 그대로 — 엔진이 GenericDialect로 폴백한다. 오류에는 자격 증명이 들 수 있는 URL
 *  원문을 싣지 않는다(Go 프로브용 postgres://user:pw@… 를 그대로 넘기는 실수가 흔하다). */
private fun dialectOf(url: String): String = when {
    !url.startsWith("jdbc:") -> fatal("not a JDBC URL; expected jdbc:<subprotocol>:…")
    else -> url.removePrefix("jdbc:").substringBefore(':').let {
        when (it) {
            "postgresql" -> "postgres"
            "informix-sqli", "informix" -> "informix"
            "db2" -> "db2"
            else -> it
        }
    }
}

/**
 * --driver로 넘긴 jar을 등록한다. DriverManager는 시스템 클래스로더의
 * ServiceLoader만 보기 때문에 URLClassLoader로 직접 ServiceLoader를 돌리고
 * 각 Driver를 shim으로 감싸 등록한다 — 드라이버 jar을 건드리지 않는 표준 요령.
 */
private fun registerDrivers(jars: List<String>, driverClass: String?) {
    if (jars.isEmpty() && driverClass == null) return
    val urls = jars.map { File(it).toURI().toURL() }.toTypedArray()
    val cl = URLClassLoader(urls, ClassLoader.getPlatformClassLoader())
    var registered = 0
    for (driver in ServiceLoader.load(Driver::class.java, cl)) {
        DriverManager.registerDriver(DriverShim(driver))
        registered++
    }
    driverClass?.let { name ->
        val driver = cl.loadClass(name).getDeclaredConstructor().newInstance() as Driver
        DriverManager.registerDriver(DriverShim(driver))
        registered++
    }
    if (registered == 0) {
        System.err.println("warning: --driver jar에서 Driver 구현을 못 찾았다 — --driver-class를 지정해라")
    }
}

/** DriverManager가 클래스로더가 다른 Driver를 거부하지 않게 감싸는 shim. */
private class DriverShim(private val d: Driver) : Driver {
    override fun connect(url: String?, info: Properties?) = d.connect(url, info)
    override fun acceptsURL(url: String?) = d.acceptsURL(url)
    override fun getPropertyInfo(url: String?, info: Properties?) = d.getPropertyInfo(url, info)
    override fun getMajorVersion() = d.majorVersion
    override fun getMinorVersion() = d.minorVersion
    override fun jdbcCompliant() = d.jdbcCompliant()
    override fun getParentLogger() = d.parentLogger
}
