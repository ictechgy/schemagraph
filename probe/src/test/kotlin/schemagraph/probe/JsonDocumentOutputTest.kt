package schemagraph.probe

import java.io.Writer
import java.nio.file.Files
import kotlin.test.Test
import kotlin.test.assertContentEquals
import kotlin.test.assertTrue

class JsonDocumentOutputTest {
    /** 인코딩 경계를 모두 지나는 문서: 한국어, BMP 밖 글자, 짝 없는 서로게이트, 제어 문자, 버퍼보다 긴 원문. */
    private fun document(tables: Int, bodyRepeat: Int = 9_000): CatalogDocument {
        val body = "SELECT 1 -- 고객 설명 🙂 \"따옴표\" \\ \t\u0001\n" + "가🙂".repeat(bodyRepeat) + "\uD800 끝"
        val view = ObjectDoc("고객_보기🙂", "view", emptyList(), emptyList(), emptyList(), emptyList(), body = body)
        val tableDocs = (1..tables).map { index ->
            ObjectDoc(
                "accounts_$index", "table",
                (1..10).map { ordinal ->
                    ColumnDoc("설명_$ordinal", "TEXT", nullable = true, default = "'기본값 🙂'", ordinal = ordinal, pkPosition = 0)
                },
                emptyList(), emptyList(), emptyList(),
            )
        }
        return CatalogDocument(
            version = 1, dialect = "sqlite", reader = "probe-jdbc",
            schemas = listOf(SchemaDoc("main", tableDocs + view, emptyList())),
            limitations = listOf("한국어 limitation 🙂"),
            context = CollectionContext(sourceId = "json-bytes", catalogComplete = true),
        )
    }

    // 문자열을 만들지 않고 임시 파일 스트림에 바로 써도, 이전 경로(writeValueAsString 뒤
    // File.writeText("$json\n"))와 들여쓰기·끝 개행·인코딩까지 바이트가 같아야 한다. OutputStream으로
    // 쓰면 Jackson이 BMP 밖 글자를 \uXXXX 쌍으로 바꿔 여기서 갈라진다.
    @Test
    fun streamedJsonMatchesThePreviousStringOutputByteForByte() {
        val directory = Files.createTempDirectory("schemagraph-json-")
        try {
            for (version in 1..2) {
                val wire = documentWire(document(tables = 3), version)
                val previous = directory.resolve("previous-v$version.json")
                previous.toFile().writeText(mapper.writeValueAsString(wire) + "\n")
                val streamed = directory.resolve("streamed-v$version.json")
                writeOutput(prepareOutput(streamed.toString())) { writer -> writeDocumentJson(writer, wire) }
                assertContentEquals(Files.readAllBytes(previous), Files.readAllBytes(streamed), "document version $version")
            }
        } finally {
            directory.toFile().deleteRecursively()
        }
    }

    // JSON 모드는 문서 전체를 문자열로 만들고("$json\n"으로 한 벌 더) 한 번에 썼다. 큰 카탈로그에서는
    // 문서 크기의 몇 배가 직렬화 단계에 한꺼번에 힙에 올랐다. 직렬화기가 문서를 버퍼 크기 덩어리로
    // 흘려 써야 한다 — writer가 한 번에 받는 덩어리가 문서보다 훨씬 작아야 한다.
    @Test
    fun documentJsonIsStreamedInBoundedChunks() {
        val wire = documentWire(document(tables = 500, bodyRepeat = 100_000), 2)
        val writer = ChunkRecordingWriter()
        writeDocumentJson(writer, wire)
        assertTrue(writer.total >= 1_000_000, "document too small to tell streaming apart: ${writer.total} chars")
        assertTrue(
            writer.largestChunk <= CHUNK_LIMIT,
            "largest single write was ${writer.largestChunk} of ${writer.total} chars; the document was materialized before writing",
        )
    }

    private companion object {
        /** Jackson 출력 버퍼(수천 자)보다 넉넉하고 테스트 문서(100만 자 이상)보다 훨씬 작은 상한. */
        const val CHUNK_LIMIT = 64 * 1024
    }

    /** 받은 글자 수와 한 번에 받은 가장 큰 덩어리만 센다. write(String)도 기본 구현이 이 함수로 온다. */
    private class ChunkRecordingWriter : Writer() {
        var total = 0L
        var largestChunk = 0

        override fun write(cbuf: CharArray, off: Int, len: Int) {
            total += len
            largestChunk = maxOf(largestChunk, len)
        }

        override fun flush() = Unit

        override fun close() = Unit
    }
}
