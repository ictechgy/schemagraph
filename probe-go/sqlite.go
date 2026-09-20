// SQLite 수확기 — database/sql의 표준 인터페이스만 사용해 카탈로그 원문을
// 옮긴다. 파싱과 그래프 의미론은 엔진의 책임이므로 이 파일은 PRAGMA와
// sqlite_master가 돌려준 값만 document로 조립한다.

package main

import (
	"database/sql"
	"fmt"
	"sort"
	"strconv"
	"strings"
)

type sqliteObject struct {
	name string
	kind string
	body *string
}

// extractSQLite — SQLite의 attached database마다 하나의 document schema를
// 만든다. temp는 연결 세션에만 존재하므로 네이티브 reader와 같이 건너뛴다.
func (h *harvester) extractSQLite() CatalogDocument {
	schemas := h.sqliteSchemas()
	doc := CatalogDocument{
		Version: documentVersion, Dialect: "sqlite", Reader: "probe-go",
		Schemas: []SchemaDoc{}, Limitations: []string{},
	}
	for _, schema := range schemas {
		doc.Schemas = append(doc.Schemas, h.sqliteSchema(schema))
	}
	sort.Slice(doc.Schemas, func(i, j int) bool { return doc.Schemas[i].Name < doc.Schemas[j].Name })
	doc.Limitations = append(doc.Limitations, h.limitations...)
	return doc
}

func (h *harvester) sqliteSchema(schema string) SchemaDoc {
	objects := h.sqliteObjects(schema)
	sd := SchemaDoc{Name: schema, Objects: []ObjectDoc{}, Routines: []RoutineDoc{}}
	for _, raw := range objects {
		columns := h.sqliteColumns(schema, raw.name)
		constraints := h.sqliteConstraints(schema, raw.name)
		indexes := h.sqliteIndexes(schema, raw.name)
		obj := ObjectDoc{
			Name: raw.name, Kind: raw.kind, Columns: columns,
			Constraints: constraints, Indexes: indexes, Triggers: []TriggerDoc{},
			Body: raw.body,
		}
		for i := range obj.Columns {
			obj.Columns[i].PkPosition = sqlitePrimaryPosition(obj.Columns[i])
		}
		sd.Objects = append(sd.Objects, obj)
	}
	h.sqliteTriggers(schema, sd.Objects)
	sort.Slice(sd.Objects, func(i, j int) bool { return sd.Objects[i].Name < sd.Objects[j].Name })
	return sd
}

func (h *harvester) streamSQLite(stream *ndjsonStreamWriter) error {
	for _, schema := range h.sqliteSchemas() {
		if err := stream.schema(h.sqliteSchema(schema)); err != nil {
			return err
		}
		if err := h.streamDependencies(stream, schema); err != nil {
			return err
		}
	}
	return nil
}

// sqliteSchemas — database_list의 attached 순서를 정렬해 결과를 결정적으로 만든다.
func (h *harvester) sqliteSchemas() []string {
	set := map[string]bool{}
	h.bestEffort("sqlite schemas", "PRAGMA database_list", func(rows *sql.Rows) error {
		v, err := sqliteRow(rows)
		if err != nil {
			return err
		}
		name := sqliteString(v, "name")
		if name != "temp" && (len(h.schemaFilter) == 0 || h.schemaFilter[name]) {
			set[name] = true
		}
		return nil
	})
	out := make([]string, 0, len(set))
	for name := range set {
		out = append(out, name)
	}
	sort.Strings(out)
	return out
}

// sqliteObjects — sqlite_master의 table/view만 객체 정점으로 만든다. 인덱스와
// trigger는 각각 PRAGMA와 객체 연결 단계에서 수확한다.
func (h *harvester) sqliteObjects(schema string) []sqliteObject {
	objects := []sqliteObject{}
	inlineConstraints := false
	query := fmt.Sprintf(`SELECT name, type, sql, tbl_name FROM %s.sqlite_master
                         WHERE name NOT LIKE 'sqlite_%%' ORDER BY name`, sqliteQuoteIdent(schema))
	h.bestEffort("sqlite objects", query, func(rows *sql.Rows) error {
		v, err := sqliteRow(rows)
		if err != nil {
			return err
		}
		name, kind := sqliteString(v, "name"), sqliteString(v, "type")
		body := sqliteStringPtr(v, "sql")
		if body != nil {
			lower := strings.ToLower(*body)
			inlineConstraints = inlineConstraints || strings.Contains(lower, "unique") || strings.Contains(lower, "check")
		}
		switch kind {
		case "table", "view":
			objects = append(objects, sqliteObject{
				name: name, kind: kind, body: body,
			})
		}
		return nil
	})
	if inlineConstraints {
		h.catalogIncomplete = true
		h.limitations = append(h.limitations,
			"inline UNIQUE/CHECK constraints are not exposed by the catalog; inline DDL constraint parsing is unsupported")
	}
	sort.Slice(objects, func(i, j int) bool { return objects[i].name < objects[j].name })
	return objects
}

// sqliteColumns — table_xinfo는 generated/hidden 컬럼도 포함하므로 ordinary
// table_info보다 네이티브 reader의 컬럼 집합과 맞는다.
func (h *harvester) sqliteColumns(schema, table string) []ColumnDoc {
	columns := []ColumnDoc{}
	query := fmt.Sprintf("PRAGMA %s.table_xinfo(%s)", sqliteQuoteIdent(schema), sqliteQuoteIdent(table))
	h.bestEffort("sqlite columns", query, func(rows *sql.Rows) error {
		v, err := sqliteRow(rows)
		if err != nil {
			return err
		}
		pk := sqliteInt(v, "pk")
		columns = append(columns, ColumnDoc{
			Name: sqliteString(v, "name"), DataType: sqliteString(v, "type"),
			Nullable: sqliteInt(v, "notnull") == 0 && pk == 0,
			Default:  sqliteStringPtr(v, "dflt_value"), Ordinal: len(columns) + 1,
			PkPosition: int(pk),
		})
		return nil
	})
	return columns
}

// sqliteConstraints — foreign_key_list의 id로 다중 컬럼 FK를 묶고,
// table_xinfo의 pk 위치로 SQLite가 이름을 주지 않는 PK를 복원한다.
func (h *harvester) sqliteConstraints(schema, table string) []ConstraintDoc {
	type fkPart struct {
		seq  int
		from string
		to   string
	}
	groups := map[int][]fkPart{}
	targets := map[int]string{}
	query := fmt.Sprintf("PRAGMA %s.foreign_key_list(%s)", sqliteQuoteIdent(schema), sqliteQuoteIdent(table))
	h.bestEffort("sqlite foreign keys", query, func(rows *sql.Rows) error {
		v, err := sqliteRow(rows)
		if err != nil {
			return err
		}
		id := int(sqliteInt(v, "id"))
		targets[id] = sqliteString(v, "table")
		groups[id] = append(groups[id], fkPart{
			seq: int(sqliteInt(v, "seq")), from: sqliteString(v, "from"), to: sqliteString(v, "to"),
		})
		return nil
	})
	constraints := []ConstraintDoc{}
	ids := make([]int, 0, len(groups))
	for id := range groups {
		ids = append(ids, id)
	}
	sort.Ints(ids)
	for _, id := range ids {
		parts := groups[id]
		sort.Slice(parts, func(i, j int) bool { return parts[i].seq < parts[j].seq })
		columns, referenced := []string{}, []string{}
		for _, part := range parts {
			columns = append(columns, part.from)
			referenced = append(referenced, part.to)
		}
		constraints = append(constraints, ConstraintDoc{
			Name: fmt.Sprintf("%s_fk_%d", table, id), Kind: "fk", Columns: columns,
			Referenced: &ReferencedDoc{Table: targets[id], Columns: referenced},
		})
	}

	pkColumns := []ColumnDoc{}
	for _, column := range h.sqliteColumns(schema, table) {
		if column.PkPosition > 0 {
			pkColumns = append(pkColumns, column)
		}
	}
	sort.Slice(pkColumns, func(i, j int) bool { return pkColumns[i].PkPosition < pkColumns[j].PkPosition })
	if len(pkColumns) > 0 {
		columns := make([]string, 0, len(pkColumns))
		for _, column := range pkColumns {
			columns = append(columns, column.Name)
		}
		constraints = append(constraints, ConstraintDoc{
			Name: table + "_pk", Kind: "pk", Columns: columns,
		})
	}
	sort.Slice(constraints, func(i, j int) bool { return constraints[i].Name < constraints[j].Name })
	return constraints
}

// sqliteIndexes — index_xinfo의 rowid/표현식 항목(cid < 0)은 컬럼 이름이
// 없으므로 정점은 보존하되 컬럼 목록에서는 제외한다.
func (h *harvester) sqliteIndexes(schema, table string) []IndexDoc {
	type indexRow struct {
		name    string
		unique  bool
		partial bool
	}
	indexes := []indexRow{}
	listQuery := fmt.Sprintf("PRAGMA %s.index_list(%s)", sqliteQuoteIdent(schema), sqliteQuoteIdent(table))
	h.bestEffort("sqlite indexes", listQuery, func(rows *sql.Rows) error {
		v, err := sqliteRow(rows)
		if err != nil {
			return err
		}
		name := sqliteString(v, "name")
		if name != "" {
			indexes = append(indexes, indexRow{name: name, unique: sqliteInt(v, "unique") == 1, partial: sqliteInt(v, "partial") == 1})
		}
		return nil
	})
	out := make([]IndexDoc, 0, len(indexes))
	for _, index := range indexes {
		columns := []string{}
		complete := true
		notesBefore := len(h.limitations)
		query := fmt.Sprintf("PRAGMA %s.index_xinfo(%s)", sqliteQuoteIdent(schema), sqliteQuoteIdent(index.name))
		h.bestEffort("sqlite index columns", query, func(rows *sql.Rows) error {
			v, err := sqliteRow(rows)
			if err != nil {
				return err
			}
			if sqliteInt(v, "key") == 0 {
				return nil
			}
			if sqliteInt(v, "cid") >= 0 && sqliteString(v, "name") != "" {
				if name := sqliteString(v, "name"); name != "" {
					columns = append(columns, name)
				}
			} else {
				complete = false
			}
			return nil
		})
		out = append(out, IndexDoc{Name: index.name, Unique: index.unique, Columns: columns, DefinitionComplete: boolValue(complete && len(columns) > 0 && len(h.limitations) == notesBefore), HasPredicate: boolValue(index.partial)})
	}
	sort.Slice(out, func(i, j int) bool { return out[i].Name < out[j].Name })
	return out
}

// sqliteTriggers — master 행은 trigger의 tbl_name만 주므로 이미 수확한
// 객체에 이름으로 귀속한다. 대상이 없으면 유령 정점을 만들지 않고 limitation만 남긴다.
func (h *harvester) sqliteTriggers(schema string, objects []ObjectDoc) {
	byName := map[string]*ObjectDoc{}
	for i := range objects {
		byName[objects[i].Name] = &objects[i]
	}
	query := fmt.Sprintf(`SELECT name, tbl_name, sql FROM %s.sqlite_master
                         WHERE type = 'trigger' AND name NOT LIKE 'sqlite_%%' ORDER BY name`, sqliteQuoteIdent(schema))
	h.bestEffort("sqlite triggers", query, func(rows *sql.Rows) error {
		v, err := sqliteRow(rows)
		if err != nil {
			return err
		}
		table := sqliteString(v, "tbl_name")
		obj := byName[table]
		if obj == nil {
			h.catalogIncomplete = true
			h.limitations = append(h.limitations, fmt.Sprintf("trigger %s.%s targets %s, which is missing from the scanned catalog", schema, sqliteString(v, "name"), table))
			return nil
		}
		obj.Triggers = append(obj.Triggers, TriggerDoc{Name: sqliteString(v, "name"), Body: sqliteStringPtr(v, "sql")})
		return nil
	})
	for _, obj := range objects {
		sort.Slice(obj.Triggers, func(i, j int) bool { return obj.Triggers[i].Name < obj.Triggers[j].Name })
	}
}

// sqliteQuoteIdent는 PRAGMA에도 사용할 수 있는 SQLite 식별자 인용이다.
// 입력을 SQL 조각으로 취급하지 않고 큰따옴표를 두 배로 만들어 주입을 막는다.
func sqliteQuoteIdent(name string) string {
	return `"` + strings.ReplaceAll(name, `"`, `""`) + `"`
}

// sqliteRow는 PRAGMA 버전별 추가 컬럼을 허용하기 위해 열 이름으로 한 행을
// 읽는다. database/sql.Rows는 반드시 이 함수가 끝난 뒤 닫히므로 단일 연결
// 풀에서도 다음 메타데이터 질의가 기다리지 않는다.
func sqliteRow(rows *sql.Rows) (map[string]any, error) {
	columns, err := rows.Columns()
	if err != nil {
		return nil, err
	}
	values := make([]any, len(columns))
	dest := make([]any, len(columns))
	for i := range values {
		dest[i] = &values[i]
	}
	if err := rows.Scan(dest...); err != nil {
		return nil, err
	}
	row := make(map[string]any, len(columns))
	for i, name := range columns {
		row[name] = values[i]
	}
	return row, nil
}

func sqliteValue(row map[string]any, name string) any {
	if value, ok := row[name]; ok {
		return value
	}
	for key, value := range row {
		if strings.EqualFold(key, name) {
			return value
		}
	}
	return nil
}

func sqliteString(row map[string]any, name string) string {
	switch value := sqliteValue(row, name).(type) {
	case string:
		return value
	case []byte:
		return string(value)
	default:
		return ""
	}
}

func sqliteStringPtr(row map[string]any, name string) *string {
	if sqliteValue(row, name) == nil {
		return nil
	}
	value := sqliteString(row, name)
	return &value
}

func sqliteInt(row map[string]any, name string) int64 {
	switch value := sqliteValue(row, name).(type) {
	case int64:
		return value
	case int:
		return int64(value)
	case int32:
		return int64(value)
	case uint64:
		return int64(value)
	case []byte:
		parsed, _ := strconv.ParseInt(string(value), 10, 64)
		return parsed
	case string:
		parsed, _ := strconv.ParseInt(value, 10, 64)
		return parsed
	default:
		return 0
	}
}

func sqlitePrimaryPosition(column ColumnDoc) int {
	return column.PkPosition
}
