package schemagraph.probe

import java.nio.file.Files
import java.nio.file.Path
import java.sql.Connection
import java.sql.DatabaseMetaData
import java.sql.DriverManager
import java.sql.Statement
import java.util.Locale
import kotlin.test.Test
import kotlin.test.assertEquals
import kotlin.test.assertTrue

/**
 * SQLite 제약 수집이 sqlite-jdbc의 getPrimaryKeys·getImportedKeys를 부르지 않으면서 그
 * 메타데이터와 같은 문서를 내는지 본다. 드라이버는 호출마다 sqlite_schema 전체를 훑고 FK
 * 행마다 한 번 더 훑어, 테이블 수의 제곱으로 느려진다(PERFORMANCE.md의 JDBC SQLite 절).
 */
class SQLiteConstraintsTest {
    /**
     * 실제 sqlite-jdbc 연결에 위임하며 드라이버 키 메타데이터 호출과 실행한 SQL을 기록한다.
     * [driverVersion]을 주면 그 값을 드라이버 버전으로 보고해 드라이버 메타데이터 경로를 강제한다.
     */
    private class RecordingConnection(private val real: Connection, private val driverVersion: String? = null) {
        /** getPrimaryKeys·getImportedKeys 호출 — "메서드:테이블" 꼴. */
        val keyMetadataCalls = mutableListOf<String>()

        /** createStatement로 실행한 SQL 원문. */
        val statements = mutableListOf<String>()

        private val metadata: DatabaseMetaData = real.metaData.let { target ->
            proxy(DatabaseMetaData::class.java) { method, args ->
                if (method.name == "getPrimaryKeys" || method.name == "getImportedKeys") {
                    keyMetadataCalls += "${method.name}:${args[2]}"
                }
                if (method.name == "getDriverVersion" && driverVersion != null) driverVersion
                else invokeOn(target, method, args)
            }
        }

        val connection: Connection = proxy(Connection::class.java) { method, args ->
            when (method.name) {
                "getMetaData" -> metadata
                "createStatement" -> recording(invokeOn(real, method, args) as Statement)
                else -> invokeOn(real, method, args)
            }
        }

        private fun recording(target: Statement): Statement = proxy(Statement::class.java) { method, args ->
            if (method.name == "executeQuery" || method.name == "execute") statements += args[0] as String
            invokeOn(target, method, args)
        }
    }

    /**
     * 드라이버와 대조할 스키마 하나. [driverPath]는 드라이버가 예외를 던져 메타데이터를 그대로 불러야 하는 호출이다.
     * [encoding]은 스크립트를 적용한 뒤의 `PRAGMA encoding` 값이다 — 인코딩이 안 바뀌면 정렬 사례가 UTF-8과 같아져 헛돈다.
     */
    private class KeyCase(
        val label: String, val script: String, val driverPath: Set<String> = emptySet(), val encoding: String = "UTF-8",
    )

    /** 앞 테이블을 참조하는 테이블 [count]개 — 이름 있는 FK, 이름 없는 FK, 컬럼 생략 참조를 섞는다. */
    private fun chain(count: Int): String = buildString {
        appendLine("CREATE TABLE t0 (id INTEGER PRIMARY KEY);")
        for (index in 1 until count) {
            val reference = when (index % 3) {
                0 -> "prev INTEGER, CONSTRAINT fk_$index FOREIGN KEY (prev) REFERENCES t${index - 1}(id)"
                1 -> "prev INTEGER REFERENCES t${index - 1}(id)"
                else -> "prev INTEGER REFERENCES t${index - 1}"
            }
            appendLine("CREATE TABLE t$index (id INTEGER PRIMARY KEY, $reference);")
        }
    }

    /** 드라이버의 UNION 결과문은 500항까지만 된다(번들 SQLite MAX_COMPOUND_SELECT). */
    private fun wide(): String = buildString {
        appendLine("CREATE TABLE p (id INTEGER PRIMARY KEY);")
        for (count in listOf(500, 501)) {
            appendLine("CREATE TABLE w$count (${(0 until count).joinToString { "c$it INTEGER REFERENCES p(id)" }});")
            val keys = (0 until count).joinToString { "k$it" }
            appendLine("CREATE TABLE pk$count ($keys, PRIMARY KEY ($keys));")
        }
    }

    /**
     * ORDER BY PKTABLE_NAME은 DB 인코딩 바이트의 BINARY 비교다 — UTF-16LE는 코드 포인트 순서와 다르고,
     * BMP 밖 글자(😀)는 UTF-8에서 전각 ｚ 뒤지만 Java String 비교(UTF-16 코드 단위)에서는 앞이다.
     */
    private fun encoded(encoding: String?): String = listOfNotNull(
        encoding?.let { "PRAGMA encoding = '$it';" },
        """CREATE TABLE "é" (id INTEGER PRIMARY KEY);""",
        """CREATE TABLE "ā" (id INTEGER PRIMARY KEY);""",
        "CREATE TABLE z (id INTEGER PRIMARY KEY);",
        """CREATE TABLE "😀" (id INTEGER PRIMARY KEY);""",
        """CREATE TABLE "ｚ" (id INTEGER PRIMARY KEY);""",
        """CREATE TABLE multi (a INTEGER REFERENCES "ā"(id), b INTEGER REFERENCES "é"(id), """ +
            """c INTEGER REFERENCES z(id), d INTEGER REFERENCES "ā"(id), """ +
            """e INTEGER REFERENCES "😀"(id), f INTEGER REFERENCES "ｚ"(id));""",
    ).joinToString("\n")

    private val cases = listOf(
        KeyCase("named and unnamed keys", """
            CREATE TABLE parent (id INTEGER PRIMARY KEY, code TEXT UNIQUE);
            CREATE TABLE named_pk (a TEXT, b INTEGER, CONSTRAINT pk_named PRIMARY KEY (a, b));
            CREATE TABLE child (
                id INTEGER PRIMARY KEY,
                parent_id INTEGER CONSTRAINT fk_inline REFERENCES parent(id),
                code TEXT, a TEXT, b INTEGER,
                CONSTRAINT fk_code FOREIGN KEY (code) REFERENCES parent(code),
                FOREIGN KEY (a, b) REFERENCES named_pk(a, b)
            );
            CREATE TABLE twice (x INTEGER REFERENCES parent(id), y INTEGER REFERENCES parent(id));
            CREATE TABLE twice_named (
                x INTEGER, y INTEGER,
                CONSTRAINT fk_x FOREIGN KEY (x) REFERENCES parent(id),
                CONSTRAINT fk_y FOREIGN KEY (y) REFERENCES parent(id)
            );
            CREATE TABLE implicit_ref (p INTEGER REFERENCES parent, a TEXT, b INTEGER, FOREIGN KEY (a, b) REFERENCES named_pk);
            CREATE TABLE q (a, b, PRIMARY KEY (a, b));
            CREATE TABLE ties (x REFERENCES parent(id), y REFERENCES parent(id), z REFERENCES q, w, v, FOREIGN KEY (w, v) REFERENCES q);
            CREATE TABLE misaligned (
                a INTEGER, b INTEGER, c INTEGER,
                CONSTRAINT first_fk FOREIGN KEY (a) REFERENCES parent(id),
                FOREIGN KEY (b) REFERENCES q(a),
                CONSTRAINT "bad-name" FOREIGN KEY (c) REFERENCES parent(id)
            );
            CREATE TABLE same_names (
                a INTEGER, b INTEGER,
                CONSTRAINT same FOREIGN KEY (a) REFERENCES parent(id),
                CONSTRAINT same FOREIGN KEY (b) REFERENCES q(a)
            );
        """.trimIndent()),
        KeyCase("quoted and mixed-case names", """
            CREATE TABLE "Mixed Parent" ("Id" INTEGER, [Name] TEXT, `Kind` TEXT, CONSTRAINT "PK Mixed" PRIMARY KEY ("Id", [Name]));
            CREATE TABLE "Child""Q" ("Ref" INTEGER, "Nm" TEXT, CONSTRAINT "fk_Mixed" FOREIGN KEY ("Ref", "Nm") REFERENCES "Mixed Parent"("Id", [Name]));
            CREATE TABLE lower_ref (r INTEGER PRIMARY KEY REFERENCES "MIXED PARENT"("ID"), s TEXT REFERENCES "mixed parent");
            CREATE TABLE "Ärger" (id INTEGER PRIMARY KEY, x INTEGER, CONSTRAINT fk_one FOREIGN KEY (id) REFERENCES lower_ref(r), CONSTRAINT fk_two FOREIGN KEY (x) REFERENCES "Mixed Parent"("Id"));
            CREATE TABLE "ärger" (id INTEGER PRIMARY KEY REFERENCES lower_ref(r));
            CREATE TABLE "İndex" (id INTEGER PRIMARY KEY, CONSTRAINT fk_dot FOREIGN KEY (id) REFERENCES lower_ref(r));
            CREATE TABLE UPPER_T (id INTEGER PRIMARY KEY, CONSTRAINT fk_upper FOREIGN KEY (id) REFERENCES lower_ref);
            CREATE TABLE PARENT (ID INTEGER PRIMARY KEY);
            CREATE TABLE ITEMS (ID INTEGER PRIMARY KEY, P INTEGER, CONSTRAINT FK_ITEMS FOREIGN KEY (P) REFERENCES PARENT(ID));
            CREATE TABLE INDEXED (ID INTEGER PRIMARY KEY, CONSTRAINT FK_INDEXED FOREIGN KEY (ID) REFERENCES PARENT);
        """.trimIndent()),
        // 드라이버 getImportedKeys는 결과문의 FKTABLE_NAME을 escape 없이 넣는다 — 작은따옴표가 든 이름은
        // 문이 깨지고(o'neil, it's), 짝이 맞으면 다른 문자열이 된다(a''b). 그 테이블의 FK는 드라이버에 맡긴다.
        KeyCase("single quotes in names", """
            CREATE TABLE "o'neil" (id INTEGER, code TEXT, CONSTRAINT "pk'quoted" PRIMARY KEY (id));
            CREATE TABLE "it's" (a INTEGER REFERENCES "o'neil"(id), b INTEGER REFERENCES "o'neil", c TEXT,
                CONSTRAINT fk_code FOREIGN KEY (c) REFERENCES "o'neil"(code));
            CREATE TABLE "a''b" (id INTEGER PRIMARY KEY REFERENCES "o'neil"(id));
            CREATE TABLE quoted_target (r INTEGER REFERENCES "o'neil"(id), s INTEGER REFERENCES "o'neil", t INTEGER REFERENCES "a''b");
            CREATE TABLE plain ("col'q" INTEGER PRIMARY KEY);
            CREATE TABLE to_plain (z INTEGER REFERENCES plain);
        """.trimIndent(), setOf("getImportedKeys:o'neil", "getImportedKeys:it's", "getImportedKeys:a''b")),
        KeyCase("virtual tables and their shadow tables", """
            CREATE VIRTUAL TABLE docs USING fts5(body);
            CREATE VIRTUAL TABLE rt USING rtree(id, min_x, max_x);
            CREATE TABLE doc_notes (id INTEGER PRIMARY KEY, rt_id INTEGER REFERENCES rt_rowid);
        """.trimIndent()),
        KeyCase("rowid and without rowid tables", """
            CREATE TABLE ipk (id INTEGER PRIMARY KEY, v TEXT);
            CREATE TABLE ipk_desc (id INTEGER PRIMARY KEY DESC);
            CREATE TABLE table_desc (id INTEGER, PRIMARY KEY (id DESC));
            CREATE TABLE collated (a TEXT, b TEXT, PRIMARY KEY (a COLLATE NOCASE, b));
            CREATE TABLE wr (a TEXT, b INTEGER, c INTEGER REFERENCES ipk(id), PRIMARY KEY (b, a)) WITHOUT ROWID;
            CREATE TABLE strict_t (id TEXT PRIMARY KEY, r INTEGER REFERENCES wr) STRICT;
        """.trimIndent()),
        KeyCase("comments and line breaks", """
            CREATE TABLE target (id INTEGER PRIMARY KEY);
            CREATE TABLE commented (
                -- PRIMARY KEY (ghost) 주석 속 PK
                id INTEGER /* CONSTRAINT fake FOREIGN KEY (id) */ PRIMARY KEY,
                p INTEGER,
                CONSTRAINT
                    fk_split
                FOREIGN    KEY
                (p) REFERENCES target (id)
            );
            CREATE TABLE weird_pk_name (a TEXT CONSTRAINT nn NOT NULL, b TEXT, PRIMARY KEY (a, b));
            CREATE TABLE empty_list (a TEXT, CHECK (a <> 'PRIMARY KEY ()'));
            CREATE TABLE double_quoted (`"q"` TEXT PRIMARY KEY);
            CREATE TABLE spaced (" s " TEXT PRIMARY KEY);
        """.trimIndent()),
        KeyCase("views and triggers", """
            CREATE TABLE base (id INTEGER PRIMARY KEY, a TEXT);
            CREATE VIEW v_pk AS SELECT id, a FROM base WHERE a <> 'PRIMARY KEY (a)';
            CREATE VIEW v_plain AS SELECT id FROM base;
            CREATE TRIGGER late AFTER INSERT ON base BEGIN SELECT 1; END;
            CREATE TABLE late (id INTEGER PRIMARY KEY, CONSTRAINT fk_late FOREIGN KEY (id) REFERENCES base(id));
            CREATE TABLE gone (x INTEGER);
            CREATE VIEW v_broken AS SELECT x FROM gone;
            DROP TABLE gone;
        """.trimIndent(), setOf("getPrimaryKeys:v_broken")),
        KeyCase("driver failures stay on the driver path", """
            CREATE TABLE ipk (id INTEGER PRIMARY KEY);
            CREATE TABLE " " (id INTEGER PRIMARY KEY, p INTEGER REFERENCES ipk(id));
            CREATE TABLE nopk (a TEXT);
            CREATE TABLE to_nopk (x INTEGER REFERENCES nopk);
            CREATE TABLE to_missing (x INTEGER REFERENCES nowhere);
            CREATE TABLE empty_pk (a TEXT, b TEXT, CHECK (a <> 'PRIMARY KEY (,)'));
            CREATE TABLE odd_ref_a (a INTEGER REFERENCES sqlite_master(rowid));
            CREATE TABLE odd_ref_b (b INTEGER REFERENCES SQLITE_MASTER);
        """.trimIndent(), setOf(
            "getPrimaryKeys: ", "getImportedKeys: ", "getImportedKeys:to_nopk", "getImportedKeys:to_missing",
            "getPrimaryKeys:empty_pk", "getImportedKeys:odd_ref_b",
        )),
        KeyCase("wide keys", wide(), setOf("getImportedKeys:w501", "getPrimaryKeys:pk501")),
        KeyCase("attached and temporary schemas", """
            ATTACH ':memory:' AS aux;
            CREATE TABLE aux.only_aux (id INTEGER PRIMARY KEY, k TEXT);
            CREATE TABLE aux.shared (id INTEGER PRIMARY KEY, z TEXT);
            CREATE TABLE shared (id INTEGER PRIMARY KEY, q TEXT, CONSTRAINT fk_shared FOREIGN KEY (q) REFERENCES only_aux(k));
            CREATE TABLE refs_aux (a INTEGER REFERENCES only_aux(id), b INTEGER REFERENCES shared(id));
            CREATE TABLE refs_aux_implicit (a INTEGER REFERENCES only_aux);
            CREATE TEMP TABLE shadow_target (id INTEGER PRIMARY KEY);
            CREATE TABLE shadowed (id INTEGER PRIMARY KEY, w INTEGER REFERENCES refs_aux(a));
            CREATE TEMP TABLE shadowed (id INTEGER, t INTEGER REFERENCES shadow_target(id));
        """.trimIndent(), setOf("getImportedKeys:refs_aux_implicit")),
        KeyCase("UTF-8 ordering", encoded(null)),
        KeyCase("UTF-16le ordering", encoded("UTF-16le"), encoding = "UTF-16le"),
        KeyCase("UTF-16be ordering", encoded("UTF-16be"), encoding = "UTF-16be"),
    )

    /** 저장소 fixture도 같은 대조를 거친다 — 교차 생산자 패리티가 쓰는 이름 있는 제약 fixture를 포함한다. */
    private fun fixtureCases(): List<KeyCase> =
        listOf("../Fixtures/sqlite/basic.sql", "../Fixtures/sqlite/named-constraints.sql", "../Fixtures/sqlite-nullability.sql")
            .map { KeyCase(it, Files.readString(Path.of(it))) }

    /** NDJSON 스트리밍 경로도 같은 constraintsOf를 타는지 행 단위로 본다. */
    private fun streamed(connection: Connection): List<String> =
        buildList { Extractor(connection, "sqlite", emptyList()).extractStreaming(2) { add(it) } }

    /** 객체·limitation 단위로 어긋남을 적는다 — 한 사례의 첫 차이에서 멈추지 않게 모은다. */
    private fun differences(label: String, actual: CatalogDocument, expected: CatalogDocument): List<String> {
        if (mapper.writeValueAsString(actual) == mapper.writeValueAsString(expected)) return emptyList()
        val expectedObjects = expected.schemas.flatMap { it.objects }.associateBy { it.name }
        val found = actual.schemas.flatMap { it.objects }.mapNotNull { obj ->
            val other = expectedObjects[obj.name]
            if (obj == other) null
            else "$label/${obj.name}: replicated ${obj.constraints} ${obj.columns.map { it.name to it.pkPosition }} " +
                "driver ${other?.constraints} ${other?.columns?.map { it.name to it.pkPosition }}"
        }.toMutableList()
        if (actual.limitations != expected.limitations) found += "$label: limitations ${actual.limitations} driver ${expected.limitations}"
        return found.ifEmpty { listOf("$label: documents differ outside objects and limitations") }
    }

    /** 같은 연결에서 옮긴 경로와 드라이버 경로의 JSON·NDJSON 문서, 드라이버 호출 범위를 대조한다. */
    private fun compare(case: KeyCase): List<String> = DriverManager.getConnection("jdbc:sqlite::memory:").use { real ->
        real.createStatement().use { it.executeUpdate(case.script) }
        val encoding = real.createStatement().use { statement ->
            statement.executeQuery("PRAGMA encoding").use { rows -> rows.next(); rows.getString(1) }
        }
        val replicated = RecordingConnection(real)
        val driver = RecordingConnection(real, driverVersion = "driver-path")
        val actual = Extractor(replicated.connection, "sqlite", emptyList()).extract()
        val expected = Extractor(driver.connection, "sqlite", emptyList()).extract()
        val calls = replicated.keyMetadataCalls.toSet()
        buildList {
            if (encoding != case.encoding) add("${case.label}: database encoding $encoding expected ${case.encoding}")
            if (actual.schemas.none { it.objects.isNotEmpty() }) add("${case.label}: no objects were collected")
            addAll(differences(case.label, actual, expected))
            if (calls != case.driverPath) add("${case.label}: driver key metadata ${calls.sorted()} expected ${case.driverPath.sorted()}")
            if (streamed(replicated.connection) != streamed(driver.connection)) add("${case.label}: NDJSON records differ")
        }
    }

    @Test
    fun sqliteKeysSkipDriverMetadataAndReadTheSchemaTextOnce() {
        DriverManager.getConnection("jdbc:sqlite::memory:").use { real ->
            real.createStatement().use { it.executeUpdate(chain(120)) }
            val recording = RecordingConnection(real)
            val objects = Extractor(recording.connection, "sqlite", emptyList()).extract().schemas.single().objects
            assertEquals(120, objects.size)
            assertEquals(119, objects.sumOf { obj -> obj.constraints.count { it.kind == "fk" } })
            // 번들 드라이버를 올리면 SQLiteConstraints.kt의 규칙을 새 드라이버 소스와 대조한 뒤 버전 상수를 바꾼다.
            assertEquals(emptyList(), recording.keyMetadataCalls, "sqlite-jdbc ${real.metaData.driverVersion}")
            val schemaReads = recording.statements.filter { "sqlite_schema" in it }
            assertEquals(1, schemaReads.size, "$schemaReads")
        }
    }

    @Test
    fun replicatedKeysMatchDriverMetadata() {
        val mismatches = (cases + fixtureCases()).flatMap(::compare)
        assertTrue(mismatches.isEmpty(), mismatches.joinToString("\n"))
    }

    @Test
    fun replicatedKeysFollowTheDriverUnderTheTurkishLocale() {
        val original = Locale.getDefault()
        val mismatches: List<String>
        val names: List<String>
        try {
            // 드라이버는 FK 이름을 붙일 때 테이블 이름을 기본 로캘로 소문자화한다(ITEMS → ıtems).
            // 넓은 표 사례는 이름이 ASCII 소문자라 로캘과 무관해 뺀다.
            Locale.setDefault(Locale.forLanguageTag("tr-TR"))
            mismatches = cases.filter { it.label != "wide keys" }.flatMap(::compare)
            names = DriverManager.getConnection("jdbc:sqlite::memory:").use { real ->
                real.createStatement().use { it.executeUpdate(cases[1].script) }
                Extractor(real, "sqlite", emptyList()).extract().schemas.single().objects
                    .first { it.name == "ITEMS" }.constraints.map { it.name }
            }
        } finally {
            Locale.setDefault(original)
        }
        assertTrue(mismatches.isEmpty(), mismatches.joinToString("\n"))
        assertEquals(listOf("ITEMS_fk_0", "ITEMS_pk"), names)
    }
}
