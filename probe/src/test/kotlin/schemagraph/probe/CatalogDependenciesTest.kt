package schemagraph.probe

import kotlin.test.Test
import kotlin.test.assertEquals
import kotlin.test.assertFalse
import kotlin.test.assertNotNull
import kotlin.test.assertTrue

class CatalogDependenciesTest {
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
