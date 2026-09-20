package schemagraph.probe

import com.fasterxml.jackson.databind.node.ObjectNode

/** 필드 변경은 전송 경계에서만 처리해 추출기 내부 모델과 v1 호환성을 유지한다. */
internal fun documentWire(doc: CatalogDocument, version: Int): ObjectNode {
    require(version in 1..2) { "Unsupported catalog version; choose 1 or 2" }
    require(version == 1 || doc.reader.isNotEmpty()) { "Catalog v2 requires a nonempty producer name" }
    val node = mapper.valueToTree<ObjectNode>(doc)
    node.put("version", version)
    if (version == 2) {
        node.remove("reader")
        node.set<ObjectNode>("producer", mapper.createObjectNode().put("name", doc.reader))
        node.set<com.fasterxml.jackson.databind.JsonNode>("required_features", mapper.valueToTree(documentFeatures(doc)))
    }
    return node
}

/** 필수 기능은 실제 레코드의 의미를 기준으로 선언해 미지원 소비자의 무시를 막는다. */
private fun documentFeatures(doc: CatalogDocument): List<String> {
    val features = sortedSetOf<String>()
    if (doc.dependencies.isNotEmpty()) features += "catalog-dependencies-v1"
    for (schema in doc.schemas) {
        if (schema.objects.any { it.usage != null || it.indexes.any { index -> index.usage != null } }
            || schema.routines.any { it.usage != null }) features += "usage-v1"
        if (schema.routines.any { it.memberOf != null }) features += "package-members-v1"
    }
    return features.toList()
}

/** 스트리밍은 레코드를 보기 전에 헤더를 내보내므로 방언이 수확할 수 있는 기능을 선언한다. */
internal fun streamingHeader(version: Int, dialect: String, catalogDependencies: Boolean = false): ObjectNode {
    require(version in 1..2) { "Unsupported catalog version; choose 1 or 2" }
    val node = lineMapper.createObjectNode()
        .put("type", "document").put("version", version).put("dialect", dialect)
    node.set<com.fasterxml.jackson.databind.JsonNode>("limitations", lineMapper.valueToTree(emptyList<String>()))
    if (version == 1) {
        node.put("reader", "probe-jdbc")
    } else {
        node.set<ObjectNode>("producer", lineMapper.createObjectNode().put("name", "probe-jdbc"))
        val features = (when (dialect) {
            "postgres", "mysql", "mariadb" -> listOf("usage-v1")
            "oracle" -> listOf("package-members-v1")
            else -> emptyList()
        } + if (catalogDependencies) listOf("catalog-dependencies-v1") else emptyList()).sorted()
        node.set<com.fasterxml.jackson.databind.JsonNode>("required_features", lineMapper.valueToTree(features))
    }
    return node
}
