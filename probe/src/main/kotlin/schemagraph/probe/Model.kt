package schemagraph.probe

import com.fasterxml.jackson.annotation.JsonInclude
import com.fasterxml.jackson.databind.PropertyNamingStrategies
import com.fasterxml.jackson.databind.SerializationFeature
import com.fasterxml.jackson.module.kotlin.jacksonObjectMapper

// catalog document의 Kotlin 측 표현 — engine/source/src/document.rs의 serde
// 계약과 필드 이름이 1:1로 대응해야 한다. 바꾸면 DOCUMENT_VERSION을 올린다.

const val DOCUMENT_VERSION = 1

data class CatalogDocument(
    val version: Int,
    val dialect: String,
    val reader: String,
    val schemas: List<SchemaDoc>,
    val limitations: List<String>,
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
)

// serde_json::to_string_pretty와 같은 모양: snake_case 키, null 키 생략,
// 2칸 들여쓰기. 필드 순서는 data class 선언 순서를 따른다.
val mapper = jacksonObjectMapper().apply {
    propertyNamingStrategy = PropertyNamingStrategies.SNAKE_CASE
    setSerializationInclusion(JsonInclude.Include.NON_NULL)
    enable(SerializationFeature.INDENT_OUTPUT)
}
