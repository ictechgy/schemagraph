package schemagraph.probe

import java.lang.reflect.InvocationTargetException
import java.lang.reflect.Method
import java.lang.reflect.Proxy
import java.sql.Connection
import java.sql.DatabaseMetaData
import java.sql.PreparedStatement
import java.sql.ResultSet
import java.sql.Statement

/** prepareStatement로 만든 조회 하나 — 원문, 바인드 값, fetch 크기를 남겨 방언 SQL을 검사한다. */
internal class PreparedCall(val sql: String) {
    val bindings = sortedMapOf<Int, String>()
    var fetchSize: Int? = null
}

/**
 * Oracle·SQL Server 같은 서버 없이 방언별 카탈로그 SQL과 드라이버 호출 인자를 검사하는 연결.
 *
 * 메타데이터 호출은 [metadataSource](H2 인메모리 DB 등)에 그대로 맡겨 객체 수집 경로를
 * 실제로 돌리고, SQL 실행은 기록만 한 뒤 빈 결과를 돌려준다. [answers]에 있는 SQL은
 * 그 값 한 칸짜리 결과를 돌려준다(세션 상태 조회 흉내).
 */
internal class ScriptedConnection(
    private val metadataSource: Connection,
    private val answers: Map<String, String> = emptyMap(),
) {
    /** createStatement 경로로 실행된 SQL 원문. */
    val executed = mutableListOf<String>()

    /** prepareStatement 경로로 준비된 조회. */
    val prepared = mutableListOf<PreparedCall>()

    /** getIndexInfo 호출 인자(catalog, schema, table, unique, approximate). */
    val indexInfoCalls = mutableListOf<List<Any?>>()

    private var readOnly = false

    private val metadata: DatabaseMetaData = metadataSource.metaData.let { real ->
        proxy(DatabaseMetaData::class.java) { method, args ->
            if (method.name == "getIndexInfo") indexInfoCalls += args.toList()
            invokeOn(real, method, args)
        }
    }

    val connection: Connection = proxy(Connection::class.java) { method, args ->
        when (method.name) {
            "getMetaData" -> metadata
            "createStatement" -> statement()
            "prepareStatement" -> preparedStatement(args[0] as String)
            "setReadOnly" -> { readOnly = args[0] as Boolean; null }
            "isReadOnly" -> readOnly
            "getCatalog" -> "fixture"
            else -> defaultValue(method.returnType)
        }
    }

    private fun statement(): Statement = proxy(Statement::class.java) { method, args ->
        when (method.name) {
            "executeQuery" -> (args[0] as String).let { sql -> executed += sql; rows(answer(sql)) }
            "execute" -> { executed += args[0] as String; false }
            else -> defaultValue(method.returnType)
        }
    }

    private fun preparedStatement(sql: String): PreparedStatement {
        val call = PreparedCall(sql).also { prepared += it }
        return proxy(PreparedStatement::class.java) { method, args ->
            when (method.name) {
                "setString" -> { call.bindings[args[0] as Int] = args[1] as String; null }
                "setFetchSize" -> { call.fetchSize = args[0] as Int; null }
                "executeQuery" -> rows(answer(sql))
                else -> defaultValue(method.returnType)
            }
        }
    }

    private fun answer(sql: String): List<String> = answers[sql]?.let { listOf(it) }.orEmpty()

    private fun rows(values: List<String>): ResultSet {
        var cursor = -1
        return proxy(ResultSet::class.java) { method, _ ->
            when (method.name) {
                "next" -> ++cursor < values.size
                "getString" -> values[cursor]
                else -> defaultValue(method.returnType)
            }
        }
    }
}

/** Proxy 핸들러가 Object 메서드까지 받으므로 identity 의미로 답해 둔다. */
@Suppress("UNCHECKED_CAST")
private fun <T> proxy(type: Class<T>, handler: (Method, Array<Any?>) -> Any?): T =
    Proxy.newProxyInstance(ScriptedConnection::class.java.classLoader, arrayOf(type)) { self, method, args ->
        when (method.name) {
            "toString" -> "scripted ${type.simpleName}"
            "hashCode" -> System.identityHashCode(self)
            "equals" -> self === args?.get(0)
            else -> handler(method, args ?: emptyArray())
        }
    } as T

/** 위임 대상의 SQLException을 감싸지 않고 그대로 던져야 추출기의 limitation 경로가 같다. */
private fun invokeOn(target: Any, method: Method, args: Array<Any?>): Any? =
    try {
        method.invoke(target, *args)
    } catch (error: InvocationTargetException) {
        throw error.targetException
    }

private fun defaultValue(type: Class<*>): Any? = when (type) {
    Boolean::class.javaPrimitiveType -> false
    Int::class.javaPrimitiveType -> 0
    Long::class.javaPrimitiveType -> 0L
    Short::class.javaPrimitiveType -> 0.toShort()
    Byte::class.javaPrimitiveType -> 0.toByte()
    Double::class.javaPrimitiveType -> 0.0
    Float::class.javaPrimitiveType -> 0f
    Char::class.javaPrimitiveType -> 0.toChar()
    else -> null
}
