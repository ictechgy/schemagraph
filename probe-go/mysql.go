package main

import (
	"database/sql"
	"fmt"
	"strings"
)

// extractMySQL — MySQL/MariaDB information_schema를 읽어 원문 catalog
// document를 만든다. MariaDB의 SEQUENCE와 MySQL의 functional index처럼
// 방언별로 노출되는 항목도 가능한 한 같은 wire 계약으로 보존한다.
func (h *harvester) extractMySQL() CatalogDocument {
	schemas := h.mysqlSchemas()
	objects := map[string][]ObjectDoc{}
	routines := map[string][]RoutineDoc{}
	for _, schema := range schemas {
		objects[schema] = h.mysqlObjects(schema)
		for i := range objects[schema] {
			obj := &objects[schema][i]
			obj.Columns = h.mysqlColumns(schema, obj.Name)
			obj.Constraints = h.mysqlConstraints(schema, obj.Name)
			obj.Indexes = h.mysqlIndexes(schema, obj.Name)
			obj.Triggers = h.mysqlTriggers(schema, obj.Name)
		}
		h.mysqlObjectUsage(schema, objects[schema])
		routines[schema] = h.mysqlRoutines(schema)
	}

	doc := CatalogDocument{
		Version: documentVersion, Dialect: "mysql", Reader: "probe-go",
		Schemas: []SchemaDoc{}, Limitations: append([]string{}, h.limitations...),
	}
	for _, schema := range schemas {
		sd := SchemaDoc{Name: schema, Objects: objects[schema], Routines: routines[schema]}
		normalizeSchema(&sd)
		doc.Schemas = append(doc.Schemas, sd)
	}
	return doc
}

// mysqlSchemaAllowed — information_schema의 시스템 schema는 기본 수확에서
// 제외하지만 명시된 schemaFilter는 그대로 존중한다. Oracle 전용 대문자
// systemSchemas 목록을 재사용하지 않는 이유는 MySQL 이름이 대소문자와
// 설치 설정에 따라 달라질 수 있기 때문이다.
func (h *harvester) mysqlSchemaAllowed(schema string) bool {
	if len(h.schemaFilter) > 0 {
		return h.schemaFilter[schema]
	}
	switch schema {
	case "mysql", "sys", "information_schema", "performance_schema":
		return false
	default:
		return true
	}
}

func (h *harvester) mysqlSchemas() []string {
	rows, err := h.db.Query(`SELECT CAST(schema_name AS CHAR) FROM information_schema.schemata ORDER BY schema_name`)
	if err != nil {
		h.limitations = append(h.limitations, fmt.Sprintf("schemas unavailable — %s", oneLine(err.Error())))
		return []string{}
	}
	defer rows.Close()
	var out []string
	for rows.Next() {
		var schema string
		if err := rows.Scan(&schema); err != nil {
			h.limitations = append(h.limitations, fmt.Sprintf("schema row read failed — %s", oneLine(err.Error())))
			break
		}
		if h.mysqlSchemaAllowed(schema) {
			out = append(out, schema)
		}
	}
	if err := rows.Err(); err != nil {
		h.limitations = append(h.limitations, fmt.Sprintf("schemas unavailable — %s", oneLine(err.Error())))
	}
	return out
}

func (h *harvester) mysqlObjects(schema string) []ObjectDoc {
	objects := []ObjectDoc{}
	h.mysqlRows("objects", `
		SELECT CAST(table_name AS CHAR), CAST(table_type AS CHAR)
		FROM information_schema.tables WHERE table_schema = ? ORDER BY table_name`, []any{schema}, func(rows *sql.Rows) error {
		var name, rawKind string
		if err := rows.Scan(&name, &rawKind); err != nil {
			return err
		}
		kind := rawKind
		switch rawKind {
		case "BASE TABLE", "SYSTEM VERSIONED":
			kind = "table"
		case "VIEW":
			kind = "view"
		case "SEQUENCE":
			kind = "sequence"
		}
		objects = append(objects, newObject(name, kind))
		return nil
	})

	h.mysqlRows("views", `
		SELECT CAST(table_name AS CHAR), CAST(view_definition AS CHAR)
		FROM information_schema.views WHERE table_schema = ?`, []any{schema}, func(rows *sql.Rows) error {
		var name string
		var body sql.NullString
		if err := rows.Scan(&name, &body); err != nil {
			return err
		}
		for i := range objects {
			if objects[i].Name == name {
				objects[i].Body = ns(body)
			}
		}
		return nil
	})
	return objects
}

func (h *harvester) mysqlColumns(schema, table string) []ColumnDoc {
	columns := []ColumnDoc{}
	pkPositions := map[string]int{}
	h.mysqlRows("primary key columns", `
		SELECT CAST(column_name AS CHAR), CAST(ordinal_position AS SIGNED)
		FROM information_schema.key_column_usage
		WHERE table_schema = ? AND table_name = ? AND constraint_name = 'PRIMARY'
		ORDER BY ordinal_position`, []any{schema, table}, func(rows *sql.Rows) error {
		var name string
		var position int
		if err := rows.Scan(&name, &position); err != nil {
			return err
		}
		pkPositions[name] = position
		return nil
	})
	h.mysqlRows("columns", `
		SELECT CAST(column_name AS CHAR), CAST(data_type AS CHAR), CAST(is_nullable AS CHAR),
		       CAST(column_default AS CHAR), CAST(ordinal_position AS SIGNED)
		FROM information_schema.columns
		WHERE table_schema = ? AND table_name = ? ORDER BY ordinal_position`, []any{schema, table}, func(rows *sql.Rows) error {
		var name, dataType, nullable string
		var def sql.NullString
		var ordinal int
		if err := rows.Scan(&name, &dataType, &nullable, &def, &ordinal); err != nil {
			return err
		}
		columns = append(columns, ColumnDoc{Name: name, DataType: dataType, Nullable: nullable == "YES", Default: ns(def), Ordinal: ordinal, PkPosition: pkPositions[name]})
		return nil
	})
	return columns
}

type mysqlConstraintRow struct {
	name, kind, column             sql.NullString
	refSchema, refTable, refColumn sql.NullString
}

func (h *harvester) mysqlConstraints(schema, table string) []ConstraintDoc {
	byName := map[string]*ConstraintDoc{}
	order := []string{}
	h.mysqlRows("constraints", `
		SELECT CAST(tc.constraint_name AS CHAR), CAST(tc.constraint_type AS CHAR),
		       CAST(kcu.column_name AS CHAR), CAST(kcu.referenced_table_schema AS CHAR),
		       CAST(kcu.referenced_table_name AS CHAR), CAST(kcu.referenced_column_name AS CHAR)
		FROM information_schema.table_constraints tc
		LEFT JOIN information_schema.key_column_usage kcu
		  ON kcu.constraint_schema = tc.constraint_schema
		 AND kcu.table_schema = tc.table_schema AND kcu.table_name = tc.table_name
		 AND kcu.constraint_name = tc.constraint_name
		WHERE tc.table_schema = ? AND tc.table_name = ?
		  AND tc.constraint_type IN ('PRIMARY KEY','FOREIGN KEY','UNIQUE','CHECK')
		ORDER BY tc.constraint_name, kcu.ordinal_position`, []any{schema, table}, func(rows *sql.Rows) error {
		var r mysqlConstraintRow
		if err := rows.Scan(&r.name, &r.kind, &r.column, &r.refSchema, &r.refTable, &r.refColumn); err != nil {
			return err
		}
		if !r.name.Valid || !r.kind.Valid {
			return nil
		}
		constraint := byName[r.name.String]
		if constraint == nil {
			constraint = &ConstraintDoc{Name: r.name.String, Kind: mysqlConstraintKind(r.kind.String), Columns: []string{}}
			byName[r.name.String] = constraint
			order = append(order, r.name.String)
		}
		if r.column.Valid {
			constraint.Columns = append(constraint.Columns, r.column.String)
		}
		if r.kind.String == "FOREIGN KEY" && r.refTable.Valid {
			if constraint.Referenced == nil {
				constraint.Referenced = &ReferencedDoc{Table: r.refTable.String, Columns: []string{}}
				if r.refSchema.Valid {
					v := r.refSchema.String
					constraint.Referenced.Schema = &v
				}
			}
			if r.refColumn.Valid {
				constraint.Referenced.Columns = append(constraint.Referenced.Columns, r.refColumn.String)
			}
		}
		return nil
	})
	out := make([]ConstraintDoc, 0, len(order))
	for _, name := range order {
		out = append(out, *byName[name])
	}
	return out
}

func mysqlConstraintKind(kind string) string {
	switch kind {
	case "PRIMARY KEY":
		return "pk"
	case "FOREIGN KEY":
		return "fk"
	case "UNIQUE":
		return "unique"
	default:
		return "check"
	}
}

func (h *harvester) mysqlIndexes(schema, table string) []IndexDoc {
	byName := map[string]*IndexDoc{}
	order := []string{}
	expressionColumns := 0
	h.mysqlRows("indexes", `
		SELECT CAST(index_name AS CHAR), CAST(non_unique AS SIGNED), CAST(column_name AS CHAR)
		FROM information_schema.statistics
		WHERE table_schema = ? AND table_name = ? AND index_name <> 'PRIMARY'
		ORDER BY index_name, seq_in_index`, []any{schema, table}, func(rows *sql.Rows) error {
		var name string
		var nonUnique int64
		var column sql.NullString
		if err := rows.Scan(&name, &nonUnique, &column); err != nil {
			return err
		}
		idx := byName[name]
		if idx == nil {
			idx = &IndexDoc{Name: name, Unique: nonUnique == 0, Columns: []string{}}
			byName[name] = idx
			order = append(order, name)
		}
		if column.Valid {
			idx.Columns = append(idx.Columns, column.String)
		} else {
			expressionColumns++
		}
		return nil
	})
	if expressionColumns > 0 {
		h.limitations = append(h.limitations, fmt.Sprintf("%s.%s: omitted %d functional index columns because they have no names", schema, table, expressionColumns))
	}
	out := make([]IndexDoc, 0, len(order))
	for _, name := range order {
		out = append(out, *byName[name])
	}
	return out
}

func (h *harvester) mysqlTriggers(schema, table string) []TriggerDoc {
	triggers := []TriggerDoc{}
	h.mysqlRows("triggers", `
		SELECT CAST(trigger_name AS CHAR), CAST(action_statement AS CHAR)
		FROM information_schema.triggers WHERE trigger_schema = ? AND event_object_table = ?
		ORDER BY trigger_name`, []any{schema, table}, func(rows *sql.Rows) error {
		var name string
		var body sql.NullString
		if err := rows.Scan(&name, &body); err != nil {
			return err
		}
		triggers = append(triggers, TriggerDoc{Name: name, Body: ns(body)})
		return nil
	})
	return triggers
}

func (h *harvester) mysqlRoutines(schema string) []RoutineDoc {
	routines := []RoutineDoc{}
	seen := map[string]bool{}
	nullBodies := 0
	type rawRoutine struct {
		name string
		kind sql.NullString
		body sql.NullString
	}
	raw := []rawRoutine{}
	h.mysqlRows("routines", `
		SELECT CAST(routine_name AS CHAR), CAST(routine_type AS CHAR), CAST(routine_definition AS CHAR)
		FROM information_schema.routines WHERE routine_schema = ?
		ORDER BY routine_name, routine_type`, []any{schema}, func(rows *sql.Rows) error {
		var item rawRoutine
		if err := rows.Scan(&item.name, &item.kind, &item.body); err != nil {
			return err
		}
		raw = append(raw, item)
		return nil
	})
	for _, item := range raw {
		if !item.kind.Valid {
			h.limitations = append(h.limitations, fmt.Sprintf("%s.%s: ROUTINE_TYPE is NULL; routine kind cannot be determined", schema, item.name))
			continue
		}
		if !item.body.Valid {
			nullBodies++
		}
		signature := h.mysqlRoutineSignature(schema, item.name)
		name := item.name
		id := name
		if signature != nil && *signature != "" {
			id += "(" + *signature + ")"
		}
		if seen[id] {
			h.limitations = append(h.limitations, fmt.Sprintf("%s.%s: function and procedure share a vertex id and will be merged", schema, name))
		}
		seen[id] = true
		kind := "function"
		if item.kind.String == "PROCEDURE" {
			kind = "procedure"
		}
		language := "sql"
		routines = append(routines, RoutineDoc{Name: name, Kind: kind, Language: &language, Body: ns(item.body), Signature: signature})
	}
	if nullBodies > 0 {
		h.limitations = append(h.limitations, fmt.Sprintf("%s: %d routine bodies were unavailable (ROUTINE_DEFINITION is NULL; check permissions)", schema, nullBodies))
	}
	return routines
}

func (h *harvester) mysqlRoutineSignature(schema, name string) *string {
	rows, err := h.db.Query(`
		SELECT CAST(dtd_identifier AS CHAR)
		FROM information_schema.parameters
		WHERE specific_schema = ? AND specific_name = ? AND ordinal_position > 0
		ORDER BY ordinal_position`, schema, name)
	if err != nil {
		h.limitations = append(h.limitations, fmt.Sprintf("%s.%s: routine parameters unavailable — %s", schema, name, oneLine(err.Error())))
		return nil
	}
	defer rows.Close()
	var types []string
	for rows.Next() {
		var typ sql.NullString
		if err := rows.Scan(&typ); err != nil {
			h.limitations = append(h.limitations, fmt.Sprintf("%s.%s: routine parameter row read failed — %s", schema, name, oneLine(err.Error())))
			return nil
		}
		if typ.Valid {
			types = append(types, typ.String)
		}
	}
	if err := rows.Err(); err != nil {
		h.limitations = append(h.limitations, fmt.Sprintf("%s.%s: routine parameters unavailable — %s", schema, name, oneLine(err.Error())))
		return nil
	}
	if len(types) == 0 {
		return nil
	}
	signature := strings.Join(types, ",")
	return &signature
}

func (h *harvester) mysqlObjectUsage(schema string, objects []ObjectDoc) {
	var pfs string
	if err := h.db.QueryRow(`SELECT CAST(@@performance_schema AS CHAR)`).Scan(&pfs); err != nil {
		h.limitations = append(h.limitations, fmt.Sprintf("%s: failed to inspect performance_schema — %s", schema, oneLine(err.Error())))
		return
	}
	if pfs == "0" || strings.EqualFold(pfs, "OFF") {
		h.limitations = append(h.limitations, "performance_schema=OFF — usage unavailable because statistics are disabled (MariaDB default)")
		return
	}
	var since sql.NullString
	if err := h.db.QueryRow(`SELECT CAST(NOW() - INTERVAL VARIABLE_VALUE SECOND AS CHAR)
		FROM performance_schema.global_status WHERE VARIABLE_NAME = 'Uptime'`).Scan(&since); err != nil {
		h.limitations = append(h.limitations, fmt.Sprintf("%s: performance_schema Uptime unavailable — %s", schema, oneLine(err.Error())))
	}
	tableRows, err := h.db.Query(`SELECT CAST(table_name AS CHAR), CAST(rows_fetched AS SIGNED),
		CAST(rows_inserted + rows_updated + rows_deleted AS SIGNED)
		FROM sys.schema_table_statistics WHERE table_schema = ?`, schema)
	if err != nil {
		h.limitations = append(h.limitations, fmt.Sprintf("%s: sys.schema_table_statistics unavailable; no usage evidence — %s", schema, oneLine(err.Error())))
	} else {
		for tableRows.Next() {
			var table string
			var reads, writes int64
			if err := tableRows.Scan(&table, &reads, &writes); err != nil {
				h.limitations = append(h.limitations, fmt.Sprintf("%s: sys.schema_table_statistics row read failed — %s", schema, oneLine(err.Error())))
				break
			}
			for i := range objects {
				if objects[i].Name == table {
					objects[i].Usage = &UsageDoc{Since: ns(since), Reads: maxInt64(reads), Writes: maxInt64(writes)}
				}
			}
		}
		if err := tableRows.Err(); err != nil {
			h.limitations = append(h.limitations, fmt.Sprintf("%s: sys.schema_table_statistics unavailable — %s", schema, oneLine(err.Error())))
		}
		_ = tableRows.Close()
	}
	rows, err := h.db.Query(`SELECT CAST(object_name AS CHAR), CAST(index_name AS CHAR)
		FROM sys.schema_unused_indexes WHERE object_schema = ?`, schema)
	if err != nil {
		h.limitations = append(h.limitations, fmt.Sprintf("%s: sys.schema_unused_indexes unavailable; no index usage evidence — %s", schema, oneLine(err.Error())))
		return
	}
	defer rows.Close()
	for rows.Next() {
		var table, index string
		if err := rows.Scan(&table, &index); err != nil {
			h.limitations = append(h.limitations, fmt.Sprintf("%s: sys.schema_unused_indexes row read failed — %s", schema, oneLine(err.Error())))
			return
		}
		for i := range objects {
			if objects[i].Name != table {
				continue
			}
			for j := range objects[i].Indexes {
				if objects[i].Indexes[j].Name == index {
					objects[i].Indexes[j].Usage = &UsageDoc{Since: ns(since), Reads: 0, Writes: 0}
				}
			}
		}
	}
	if err := rows.Err(); err != nil {
		h.limitations = append(h.limitations, fmt.Sprintf("%s: sys.schema_unused_indexes unavailable — %s", schema, oneLine(err.Error())))
	}
}

// mysqlRows는 metadata query 실패를 빈 성공으로 숨기지 않고 limitation으로
// 기록한다. 개별 테이블의 선택적 metadata는 그 테이블만 비워 진행한다.
func (h *harvester) mysqlRows(label, query string, args []any, scan func(*sql.Rows) error) {
	rows, err := h.db.Query(query, args...)
	if err != nil {
		h.limitations = append(h.limitations, fmt.Sprintf("%s unavailable — %s", label, oneLine(err.Error())))
		return
	}
	defer rows.Close()
	for rows.Next() {
		if err := scan(rows); err != nil {
			h.limitations = append(h.limitations, fmt.Sprintf("%s row read failed — %s", label, oneLine(err.Error())))
			return
		}
	}
	if err := rows.Err(); err != nil {
		h.limitations = append(h.limitations, fmt.Sprintf("%s unavailable — %s", label, oneLine(err.Error())))
	}
}
