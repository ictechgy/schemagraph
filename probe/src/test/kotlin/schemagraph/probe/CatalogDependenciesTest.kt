package schemagraph.probe

import kotlin.random.Random
import kotlin.test.Test
import kotlin.test.assertEquals
import kotlin.test.assertFalse
import kotlin.test.assertNotNull
import kotlin.test.assertTrue

class CatalogDependenciesTest {
    /** pg_depend 조회 결과처럼 보이는 의존성. 이름을 작은 집합에서 뽑아 직렬화 앞부분이 자주 겹치게
     *  하고, null 선택 필드와 완전히 같은 값(중복)도 섞는다. Go 프로브 테스트의 생성기와 같은 모양이다. */
    private fun randomDependencies(count: Int, seed: Int): List<CatalogDependency> {
        val random = Random(seed)
        fun pick(vararg values: String) = values[random.nextInt(values.size)]
        fun optional(vararg values: String) = if (random.nextInt(3) == 0) null else pick(*values)
        val dependencies = mutableListOf<CatalogDependency>()
        while (dependencies.size < count) {
            if (dependencies.isNotEmpty() && random.nextInt(8) == 0) {
                dependencies += dependencies[random.nextInt(dependencies.size)]
                continue
            }
            dependencies += CatalogDependency(
                source = CatalogObjectRef(
                    schema = pick("app", "billing"),
                    name = pick("orders_v", "orders_mv", "report"),
                    kind = optional("view", "function"),
                    signature = optional("", "integer"),
                ),
                target = CatalogObjectRef(
                    schema = pick("app", "billing"),
                    name = pick("orders", "items", "total"),
                    kind = optional("table", "function"),
                    member = optional("id", "amount", "status"),
                    signature = optional("integer, text"),
                    database = optional("main"),
                ),
                catalog = "pg_depend",
                dependencyType = pick("n", "a"),
            )
        }
        return dependencies
    }

    /** 문서·NDJSON의 의존성 순서는 중복을 뺀 뒤 한 줄 직렬화 문자열 순서다. sortedBy는 비교마다
     *  selector를 두 번 불러, 섞인 입력에서는 직렬화가 약 2·D·log2 D번 일어났다. 같은 순서를 내면서
     *  서로 다른 의존성마다 직렬화를 한 번만 하는지 호출 수로 확인한다. */
    @Test
    fun dependencyOrderSerializesEachDistinctDependencyOnce() {
        val dependencies = randomDependencies(1024, 20261019)
        val expected = dependencies.distinct().sortedBy { lineMapper.writeValueAsString(it) }
        var serializations = 0
        val sorted = distinctSortedDependencies(dependencies) {
            serializations++
            lineMapper.writeValueAsString(it)
        }
        assertEquals(expected, sorted)
        assertEquals(expected, distinctSortedDependencies(dependencies))
        assertEquals(expected.size, serializations, "sort keys must be serialized once per distinct dependency")
    }

    private fun document(): CatalogDocument {
        return CatalogDocument(
            version = 1,
            dialect = "postgres",
            reader = "probe-jdbc",
            schemas = listOf(
                SchemaDoc(
                    name = "app",
                    objects = listOf(
                        ObjectDoc(
                            name = "consumer",
                            kind = "table",
                            columns = emptyList(),
                            constraints = emptyList(),
                            indexes = emptyList(),
                            triggers = emptyList(),
                        ),
                    ),
                    routines = emptyList(),
                ),
            ),
            limitations = emptyList(),
            context = CollectionContext(
                sourceId = "app-prod",
                database = "local-db",
                schemaFilter = listOf("app", "reporting"),
                catalogComplete = true,
            ),
            dependencies = listOf(
                CatalogDependency(
                    source = CatalogObjectRef(schema = "app", name = "consumer", kind = "table"),
                    target = CatalogObjectRef(
                        schema = "app",
                        name = "worker",
                        kind = "function",
                        member = "run",
                        signature = "integer",
                        database = "local-db",
                    ),
                    catalog = "pg_depend",
                    dependencyType = "NORMAL",
                ),
            ),
        )
    }

    @Test
    fun jsonWirePreservesDependencyIdentityAndCollectionContext() {
        val doc = document()
        val v1 = documentWire(doc, 1)
        assertEquals("probe-jdbc", v1["reader"].asText())
        assertFalse(v1.has("producer"))
        assertEquals("app-prod", v1["context"]["source_id"].asText())
        assertEquals("local-db", v1["context"]["database"].asText())
        assertEquals("NORMAL", v1["dependencies"][0]["dependency_type"].asText())
        assertEquals("run", v1["dependencies"][0]["target"]["member"].asText())
        assertEquals("integer", v1["dependencies"][0]["target"]["signature"].asText())

        val v2 = documentWire(doc, 2)
        assertFalse(v2.has("reader"))
        assertEquals("probe-jdbc", v2["producer"]["name"].asText())
        assertEquals(listOf("catalog-dependencies-v1"), v2["required_features"].map { it.asText() })
        assertNotNull(v2["context"])
    }

    @Test
    fun emptyOptionalIdentityFieldsAreOmittedAndDependencyFeatureIsExplicit() {
        val empty = CatalogDependency(
            source = CatalogObjectRef(schema = "app", name = "consumer"),
            target = CatalogObjectRef(schema = "app", name = "worker"),
            catalog = "fixture",
            dependencyType = "NORMAL",
        )
        val encoded = mapper.writeValueAsString(empty)
        assertTrue(encoded.contains("\"source\""))
        assertTrue(encoded.contains("\"dependency_type\""))
        assertFalse(encoded.contains("\"member\""))
        assertFalse(encoded.contains("\"signature\""))
        assertFalse(encoded.contains("\"database\""))
        val noDependencies = document().copy(dependencies = emptyList())
        val v2 = documentWire(noDependencies, 2)
        assertEquals(emptyList<String>(), v2["required_features"].map { it.asText() })
    }

    @Test
    fun streamingV2HeaderDeclaresCatalogDependenciesOnlyWhenEnabled() {
        val without = streamingHeader(2, "postgres", catalogDependencies = false)
        assertEquals(listOf("usage-v1"), without["required_features"].map { it.asText() })
        assertFalse(without.has("reader"))

        val with = streamingHeader(2, "postgres", catalogDependencies = true)
        assertEquals(
            listOf("catalog-dependencies-v1", "usage-v1"),
            with["required_features"].map { it.asText() },
        )
        assertEquals("probe-jdbc", with["producer"]["name"].asText())

        val v1 = streamingHeader(1, "postgres", catalogDependencies = true)
        assertEquals("probe-jdbc", v1["reader"].asText())
        assertFalse(v1.has("required_features"))
    }
}
