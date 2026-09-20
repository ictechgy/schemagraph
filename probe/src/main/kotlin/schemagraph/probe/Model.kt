package schemagraph.probe

import com.fasterxml.jackson.annotation.JsonInclude
import com.fasterxml.jackson.databind.PropertyNamingStrategies
import com.fasterxml.jackson.databind.SerializationFeature
import com.fasterxml.jackson.module.kotlin.jacksonObjectMapper

// catalog document의 Kotlin 측 표현 — engine/source/src/document.rs의 serde
// 내부 모델은 v1을 유지하고, 전송 버전의 필드 변경은 DocumentVersions에서 처리한다.

const val DOCUMENT_VERSION = 1

data class CatalogDocument(
    val version: Int,
    val dialect: String,
    val reader: String,
    val schemas: List<SchemaDoc>,
    val limitations: List<String>,
    val context: CollectionContext? = null,
    @get:JsonInclude(JsonInclude.Include.NON_EMPTY)
    val dependencies: List<CatalogDependency> = emptyList(),
)

/** 논리적인 수집 범위만 기록하며 연결 URL과 인증정보는 포함하지 않는다. */
data class CollectionContext(
    val sourceId: String,
    val database: String? = null,
    val schemaFilter: List<String>? = null,
    val catalogComplete: Boolean,
)

/** DB가 제공한 이름을 보존하며 실제 그래프 id 해석은 엔진에 맡긴다. */
data class CatalogObjectRef(
    val schema: String,
    val name: String,
    val kind: String? = null,
    val member: String? = null,
    val signature: String? = null,
    val database: String? = null,
)

/** 카탈로그의 참조 사실을 그대로 전달한다. 읽기/쓰기 간선은 생성하지 않는다. */
data class CatalogDependency(
    val source: CatalogObjectRef,
    val target: CatalogObjectRef,
    val catalog: String,
    val dependencyType: String,
)

data class SchemaDoc(
    val name: String,
    val objects: List<ObjectDoc>,
    val routines: List<RoutineDoc>,
)

data class ObjectDoc(
    val name: String,
    val kind: String,
    val columns: List<ColumnDoc>,
    val constraints: List<ConstraintDoc>,
    val indexes: List<IndexDoc>,
    val triggers: List<TriggerDoc>,
    val body: String? = null,
    val usage: UsageDoc? = null,
)

data class ColumnDoc(
    val name: String,
    val dataType: String,
    val nullable: Boolean,
    val default: String? = null,
    val ordinal: Int,
    val pkPosition: Int,
)

data class ConstraintDoc(
    val name: String,
    val kind: String,
    val columns: List<String>,
    val referenced: ReferencedDoc? = null,
)

data class ReferencedDoc(
    val schema: String? = null,
    val table: String,
    val columns: List<String>,
)

data class IndexDoc(
    val name: String,
    val unique: Boolean,
    val columns: List<String>,
    val usage: UsageDoc? = null,
    val definitionComplete: Boolean? = null,
    val hasPredicate: Boolean? = null,
    val predicate: String? = null,
)

/**
 * 사용 통계 — since 이후만 유효한 관측량. document.rs의 UsageDoc과
 * 필드명이 같아야 한다(additive 계약이라 버전은 안 올린다).
 */
data class UsageDoc(
    val since: String? = null,
    val reads: Long,
    val writes: Long,
    // routine 누적/자기 실행 시간 ms — routine 정점에만 온다(미지원 방언은 null).
    val totalMs: Double? = null,
    val selfMs: Double? = null,
)

data class TriggerDoc(
    val name: String,
    val body: String? = null,
)

data class RoutineDoc(
    val name: String,
    val kind: String,
    val language: String? = null,
    val body: String? = null,
    val signature: String? = null,
    val usage: UsageDoc? = null,
    /** 패키지 멤버면 부모 패키지 이름 — 독립 routine이면 null. */
    val memberOf: String? = null,
    /** 외부 SQL 생산자가 제공할 때만 상대 경로를 옮긴다. */
    val source: String? = null,
)

// serde_json::to_string_pretty와 같은 모양: snake_case 키, null 키 생략,
// 2칸 들여쓰기. 필드 순서는 data class 선언 순서를 따른다.
val mapper = jacksonObjectMapper().apply {
    propertyNamingStrategy = PropertyNamingStrategies.SNAKE_CASE
    setSerializationInclusion(JsonInclude.Include.NON_NULL)
    enable(SerializationFeature.INDENT_OUTPUT)
}

// NDJSON 행은 반드시 한 줄 — 들여쓰기 없는 별도 매퍼를 쓴다.
// 스트리밍 방출은 Extractor.extractStreaming이 한다 — 행 레이아웃은
// engine/source/src/ndjson.rs와 약속이다(document 헤더 → schema 행 +
// object·routine 행 → limitations 트레일러).
val lineMapper = jacksonObjectMapper().apply {
    propertyNamingStrategy = PropertyNamingStrategies.SNAKE_CASE
    setSerializationInclusion(JsonInclude.Include.NON_NULL)
}
