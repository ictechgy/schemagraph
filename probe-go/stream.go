package main

import (
	"bufio"
	"database/sql"
	"encoding/json"
	"io"
	"sort"
	"strings"
)

// ndjsonStreamWriter는 한 레코드만 encoder에 넘긴다. document 전체나
// 직렬화된 바이트 전체를 버퍼링하지 않아 출력 버퍼는 한 레코드만 보관한다.
// 수집된 스키마 자체는 방출이 끝날 때까지 메모리에 남는다.
type ndjsonStreamWriter struct {
	output  *bufio.Writer
	encoder *json.Encoder
}

func newNDJSONStreamWriter(w io.Writer) *ndjsonStreamWriter {
	output := bufio.NewWriterSize(w, 64*1024)
	return &ndjsonStreamWriter{output: output, encoder: json.NewEncoder(output)}
}

func (s *ndjsonStreamWriter) line(value any) error {
	return s.encoder.Encode(value)
}

func (s *ndjsonStreamWriter) flush() error {
	return s.output.Flush()
}

func (s *ndjsonStreamWriter) header(dialect, reader string, version int, features []string) error {
	return s.headerWithContext(dialect, reader, version, features, nil)
}

func (s *ndjsonStreamWriter) headerWithContext(dialect, reader string, version int, features []string, context *CollectionContext) error {
	if version == 1 {
		return s.line(ndjsonDocumentV1{
			Type: "document", Version: version, Dialect: dialect,
			Reader: reader, Limitations: []string{},
			Context: context,
		})
	}
	return s.line(ndjsonDocumentV2{
		Type: "document", Version: version, Dialect: dialect,
		Producer: documentProducer{Name: reader}, RequiredFeatures: features,
		Limitations: []string{},
		Context:     context,
	})
}

func (s *ndjsonStreamWriter) schema(schema SchemaDoc) error {
	if err := s.line(map[string]any{"type": "schema", "name": schema.Name}); err != nil {
		return err
	}
	for _, object := range schema.Objects {
		if err := s.line(map[string]any{
			"type": "object", "schema": schema.Name, "data": object,
		}); err != nil {
			return err
		}
	}
	for _, routine := range schema.Routines {
		if err := s.line(map[string]any{
			"type": "routine", "schema": schema.Name, "data": routine,
		}); err != nil {
			return err
		}
	}
	return nil
}

func (s *ndjsonStreamWriter) limitations(limitations []string) error {
	return s.footer(limitations, nil)
}

func (s *ndjsonStreamWriter) footer(limitations []string, context *CollectionContext) error {
	if limitations == nil {
		limitations = []string{}
	}
	record := map[string]any{"type": "limitations", "data": limitations}
	if context != nil {
		record["context"] = context
	}
	return s.line(record)
}

// streamNDJSON은 adapter가 schema 하나를 만든 즉시 방출한다. header에는
// 방언이 수확할 수 있는 필수 기능의 집합을 먼저 선언한다.
func (h *harvester) streamNDJSON(w io.Writer, version int) error {
	if err := validateDocumentVersion(version); err != nil {
		return err
	}
	features := h.streamFeatures()
	stream := newNDJSONStreamWriter(w)
	if err := stream.headerWithContext(h.dialect, "probe-go", version, features, h.collectionContext(false)); err != nil {
		return err
	}
	switch h.dialect {
	case "postgres":
		if err := h.streamPostgres(stream); err != nil {
			return err
		}
	case "mysql":
		if err := h.streamMySQL(stream); err != nil {
			return err
		}
	case "sqlite":
		if err := h.streamSQLite(stream); err != nil {
			return err
		}
	case "sqlserver":
		if err := h.streamMSSQL(stream); err != nil {
			return err
		}
	default:
		if err := h.streamOracle(stream); err != nil {
			return err
		}
	}
	if err := stream.footer(sortedDistinct(h.limitations), h.collectionContext(true)); err != nil {
		return err
	}
	return stream.flush()
}

func (h *harvester) streamFeatures() []string {
	usage, members := false, false
	switch h.dialect {
	case "postgres":
		usage = true
	case "mysql":
		usage = true
	case "oracle":
		members = true
	}
	features := make([]string, 0, 2)
	if h.catalogDependencies {
		features = append(features, "catalog-dependencies-v1")
	}
	if members {
		features = append(features, "package-members-v1")
	}
	if usage {
		features = append(features, "usage-v1")
	}
	return features
}

func (h *harvester) scopedCatalogQuery(query, predicate, parameter string) (string, []any) {
	if h.activeSchema == nil {
		return query, nil
	}
	upper := strings.ToUpper(query)
	insertAt := strings.Index(upper, "ORDER BY")
	if insertAt < 0 {
		insertAt = len(query)
	}
	joiner := " WHERE "
	if strings.Contains(upper, "WHERE") {
		joiner = " AND "
	}
	query = query[:insertAt] + joiner + predicate + " " + query[insertAt:]
	return query, []any{sql.Named(parameter, *h.activeSchema)}
}

func (h *harvester) mssqlScopedQuery(query, predicate string) (string, []any) {
	return h.scopedCatalogQuery(query, predicate, "schema")
}

func (h *harvester) oracleScopedQuery(query, predicate string) (string, []any) {
	return h.scopedCatalogQuery(query, predicate, "schema")
}

func (h *harvester) oracleSchemas() []string {
	names := []string{}
	h.bestEffort("schemas", `SELECT DISTINCT OWNER FROM ALL_OBJECTS
		WHERE OBJECT_TYPE IN ('TABLE','VIEW','MATERIALIZED VIEW','SYNONYM',
		'PROCEDURE','FUNCTION','PACKAGE','PACKAGE BODY') ORDER BY OWNER`, func(rs *sql.Rows) error {
		var name string
		if err := rs.Scan(&name); err != nil {
			return err
		}
		if h.keepSchema(name) {
			names = append(names, name)
		}
		return nil
	})
	h.checkRequestedSchemas(names)
	return names
}

// oracleSchema — 소유자 하나를 수확한다. 소유자 목록(oracleSchemas, ALL_OBJECTS)에는 전역 임시
// 테이블이나 객체 테이블처럼 collectObjects(ALL_TABLES WHERE TEMPORARY = 'N')가 받지 않는
// 'TABLE'만 가진 소유자도 오른다. 그래서 객체도 routine도 없는 스키마는 권한 공백의 증거가 아니라
// 존재하지만 빈 스키마다. 다른 방언과 JDBC 프로브처럼 limitation 없이 완전한 수집으로 남기고,
// 권한 공백 신호는 수집 전체에 테이블·뷰가 없을 때(checkOracleObjects)만 낸다.
func (h *harvester) oracleSchema(schema string) SchemaDoc {
	objects := h.collectObjects()[schema]
	views := h.collectViews()
	triggers := h.collectTriggers()
	for i := range objects {
		key := schema + "." + objects[i].Name
		if body, ok := views[key]; ok {
			objects[i].Body = &body
		}
		objects[i].Triggers = triggers[key]
		if objects[i].Triggers == nil {
			objects[i].Triggers = []TriggerDoc{}
		}
	}
	routines := h.collectRoutines()[schema]
	sd := SchemaDoc{Name: schema, Objects: objects, Routines: routines}
	normalizeSchema(&sd)
	return sd
}

// oracleScopedSchema — 질의의 OWNER를 이 스키마로 바인딩해 수확하고 원래 범위로 되돌린다.
// JSON과 NDJSON이 이 함수로 수확해 같은 질의와 같은 완전성 규칙을 쓴다.
func (h *harvester) oracleScopedSchema(schema string) SchemaDoc {
	original, originalActive := h.schemaFilter, h.activeSchema
	defer func() { h.schemaFilter, h.activeSchema = original, originalActive }()
	h.schemaFilter = map[string]bool{schema: true}
	h.activeSchema = &schema
	return h.oracleSchema(schema)
}

// checkOracleObjects — 수집한 스키마 전체에 테이블·뷰가 없으면 권한 공백으로 신고한다.
func (h *harvester) checkOracleObjects(objects int) {
	if objects == 0 {
		h.markIncomplete(oracleNoObjectsLimitation)
	}
}

// extractOracle — JSON 모드도 스키마마다 OWNER를 바인딩해 수확한다. 바인딩 없이 ALL_*를
// 읽으면 --schema와 무관하게 SYS·PUBLIC 사전까지 받아 클라이언트에서 버린다.
func (h *harvester) extractOracle() CatalogDocument {
	doc := CatalogDocument{
		Version: documentVersion, Dialect: h.dialect, Reader: "probe-go",
		Schemas: []SchemaDoc{}, Limitations: []string{},
	}
	objects := 0
	for _, schema := range h.oracleSchemas() {
		document := h.oracleScopedSchema(schema)
		objects += len(document.Objects)
		doc.Schemas = append(doc.Schemas, document)
	}
	h.checkOracleObjects(objects)
	sort.Slice(doc.Schemas, func(i, j int) bool { return doc.Schemas[i].Name < doc.Schemas[j].Name })
	doc.Limitations = append(doc.Limitations, h.limitations...)
	return doc
}

func (h *harvester) streamOracle(stream *ndjsonStreamWriter) error {
	objects := 0
	for _, schema := range h.oracleSchemas() {
		document := h.oracleScopedSchema(schema)
		objects += len(document.Objects)
		if err := stream.schema(document); err != nil {
			return err
		}
		if err := h.streamDependencies(stream, schema); err != nil {
			return err
		}
	}
	h.checkOracleObjects(objects)
	return nil
}
