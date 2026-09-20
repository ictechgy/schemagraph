package main

import (
	"bufio"
	"database/sql"
	"encoding/json"
	"io"
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
	if version == 1 {
		return s.line(ndjsonDocumentV1{
			Type: "document", Version: version, Dialect: dialect,
			Reader: reader, Limitations: []string{},
		})
	}
	return s.line(ndjsonDocumentV2{
		Type: "document", Version: version, Dialect: dialect,
		Producer: documentProducer{Name: reader}, RequiredFeatures: features,
		Limitations: []string{},
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
	return s.line(map[string]any{"type": "limitations", "data": limitations})
}

// streamNDJSON은 adapter가 schema 하나를 만든 즉시 방출한다. header에는
// 방언이 수확할 수 있는 필수 기능의 집합을 먼저 선언한다.
func (h *harvester) streamNDJSON(w io.Writer, version int) error {
	if err := validateDocumentVersion(version); err != nil {
		return err
	}
	features := h.streamFeatures()
	stream := newNDJSONStreamWriter(w)
	if err := stream.header(h.dialect, "probe-go", version, features); err != nil {
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
	if err := stream.limitations(sortedDistinct(h.limitations)); err != nil {
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
	return names
}

func (h *harvester) oracleSchema(schema string) SchemaDoc {
	limitationsStart := len(h.limitations)
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
	if len(objects) == 0 && len(routines) > 0 {
		kept := h.limitations[:limitationsStart]
		for _, limitation := range h.limitations[limitationsStart:] {
			if limitation != "카탈로그가 테이블/뷰를 하나도 주지 않았다 — 접근 권한을 확인해라" {
				kept = append(kept, limitation)
			}
		}
		h.limitations = kept
	}
	sd := SchemaDoc{Name: schema, Objects: objects, Routines: routines}
	normalizeSchema(&sd)
	return sd
}

func (h *harvester) streamOracle(stream *ndjsonStreamWriter) error {
	schemas := h.oracleSchemas()
	original := h.schemaFilter
	originalActive := h.activeSchema
	defer func() {
		h.schemaFilter = original
		h.activeSchema = originalActive
	}()
	objects := 0
	for _, schema := range schemas {
		h.schemaFilter = map[string]bool{schema: true}
		h.activeSchema = &schema
		document := h.oracleSchema(schema)
		objects += len(document.Objects)
		if err := stream.schema(document); err != nil {
			return err
		}
	}
	if objects == 0 {
		h.limitations = append(h.limitations, "카탈로그가 테이블/뷰를 하나도 주지 않았다 — 접근 권한을 확인해라")
	}
	return nil
}
