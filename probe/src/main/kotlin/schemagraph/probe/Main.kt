package schemagraph.probe

import java.io.File
import java.net.URLClassLoader
import java.sql.Driver
import java.sql.DriverManager
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
    val opts = parseArgs(args)
    if (opts.help || opts.url == null) {
        System.err.println(if (opts.url == null && !opts.help) "error: --url이 필요하다\n$USAGE" else USAGE)
        exitProcess(if (opts.help) 0 else 2)
    }
    val url = opts.url!!

    for (name in BUNDLED_DRIVERS) runCatching { Class.forName(name) }
    runCatching { registerDrivers(opts.driverJars, opts.driverClass) }
        .onFailure { fatal("드라이버 로딩 실패: ${it.message}") }

    val props = Properties()
    opts.user?.let { props["user"] = it }
    (opts.password ?: System.getenv("SG_DB_PASSWORD"))?.let { props["password"] = it }

    val dialect = dialectOf(url)
    val doc = runCatching {
        DriverManager.getConnection(url, props).use { conn ->
            Extractor(conn, dialect, opts.schemas).extract()
        }
    }.getOrElse { fatal("추출 실패: ${it.message}") }

    val json = when (opts.format) {
        "json" -> mapper.writeValueAsString(doc)
        "ndjson" -> toNdjson(doc)
        else -> fatal("--format은 json|ndjson 중 하나다: ${opts.format}")
    }
    if (opts.output == "-") println(json)
    else File(opts.output).writeText("$json\n")
}

private fun fatal(message: String): Nothing {
    System.err.println("error: $message")
    exitProcess(2)
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
            "-o", "--output" -> o.output = next(a)
            "--format" -> o.format = next(a)
            "-h", "--help" -> o.help = true
            else -> fatal("error: 알 수 없는 인자 '$a'\n$USAGE")
        }
        i++
    }
    return o
}

/** jdbc:<subprotocol>:… 에서 방언 이름을 뽑는다. 알 수 없는 방언은 subprotocol
 *  이름 그대로 — 엔진이 GenericDialect로 폴백한다. */
private fun dialectOf(url: String): String = when {
    !url.startsWith("jdbc:") -> fatal("JDBC URL이 아니다: $url")
    else -> url.removePrefix("jdbc:").substringBefore(':').let {
        when (it) {
            "postgresql" -> "postgres"
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
