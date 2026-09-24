package main

import (
	"database/sql"
	"fmt"
	"sort"
	"strings"
)

// extractPostgres — PostgreSQL 카탈로그를 원문 그대로 읽어 document로 만든다.
// SQL 본문 해석과 그래프 생성은 엔진의 책임이므로 이 함수는 메타데이터만
// 수확한다.
func (h *harvester) extractPostgres() CatalogDocument {
	schemas := h.postgresSchemas()
	doc := CatalogDocument{
		Version: documentVersion, Dialect: "postgres", Reader: "probe-go",
		Schemas: []SchemaDoc{}, Limitations: []string{},
	}
	for _, schema := range schemas {
		doc.Schemas = append(doc.Schemas, h.postgresSchema(schema))
	}
	doc.Limitations = append([]string{}, h.limitations...)
	return doc
}

func (h *harvester) postgresSchema(schema string) SchemaDoc {
	objects := h.postgresObjects(schema)
	for i := range objects {
		obj := &objects[i]
		obj.Columns = h.postgresColumns(schema, obj.Name)
		obj.Constraints = h.postgresConstraints(schema, obj.Name)
		obj.Indexes = h.postgresIndexes(schema, obj.Name)
		obj.Triggers = h.postgresTriggers(schema, obj.Name)
	}
	h.postgresVisibility(schema, objects)
	h.postgresObjectUsage(schema, objects)
	routines := h.postgresRoutines(schema)
	h.postgresRoutineUsage(schema, routines)
	sd := SchemaDoc{Name: schema, Objects: objects, Routines: routines}
	normalizeSchema(&sd)
	return sd
}

// 권한 필터로 사라진 카탈로그 행 수를 독립 pg_catalog 목록과 대조한다.
func (h *harvester) postgresVisibility(schema string, objects []ObjectDoc) {
	raw, err := catalogQueries.ReadFile("sql/visibility-postgres.sql")
	if err != nil {
		h.catalogIncomplete = true
		h.limitations = append(h.limitations, "bundled PostgreSQL visibility query is missing; rebuild the probe")
		return
	}
	known := make(map[string]map[string]bool, len(objects))
	for _, object := range objects {
		columns := map[string]bool{}
		for _, column := range object.Columns {
			columns[column.Name] = true
		}
		known[object.Name] = columns
	}
	missingRelations := map[string]bool{}
	missingColumns := 0
	h.pgRows("catalog visibility", strings.ReplaceAll(string(raw), ":schema", "$1"), []any{schema}, func(rows *sql.Rows) error {
		var relation string
		var column sql.NullString
		if err := rows.Scan(&relation, &column); err != nil {
			return err
		}
		columns, found := known[relation]
		if !found {
			missingRelations[relation] = true
		}
		if column.Valid && !columns[column.String] {
			missingColumns++
		}
		return nil
	})
	if len(missingRelations) > 0 || missingColumns > 0 {
		h.catalogIncomplete = true
		h.limitations = append(h.limitations, fmt.Sprintf("%s: catalog visibility check found %d uncollected relations and %d uncollected columns; verify metadata permissions and reader coverage", schema, len(missingRelations), missingColumns))
	}
}

func (h *harvester) streamPostgres(stream *ndjsonStreamWriter) error {
	for _, schema := range h.postgresSchemas() {
		if err := stream.schema(h.postgresSchema(schema)); err != nil {
			return err
		}
		if err := h.streamDependencies(stream, schema); err != nil {
			return err
		}
	}
	return nil
}

// postgresSchemaAllowed — PostgreSQL의 system namespace 필터와 사용자가 준
// 정확한 schema allowlist를 함께 적용한다. Oracle의 대문자 목록은 섞지 않는다.
func (h *harvester) postgresSchemaAllowed(schema string) bool {
	return !strings.HasPrefix(schema, "pg_") && schema != "information_schema" &&
		(len(h.schemaFilter) == 0 || h.schemaFilter[schema])
}

func (h *harvester) postgresSchemas() []string {
	rows, err := h.db.Query(`
		SELECT nspname
		FROM pg_namespace
		WHERE nspname NOT LIKE 'pg\_%' AND nspname <> 'information_schema'
		ORDER BY nspname`)
	if err != nil {
		h.catalogIncomplete = true
		h.limitations = append(h.limitations, fmt.Sprintf("schemas unavailable — %s", oneLine(err.Error())))
		return []string{}
	}
	defer rows.Close()
	var out []string
	for rows.Next() {
		var schema string
		if err := rows.Scan(&schema); err != nil {
			h.catalogIncomplete = true
			h.limitations = append(h.limitations, fmt.Sprintf("schema row read failed — %s", oneLine(err.Error())))
			break
		}
		if h.postgresSchemaAllowed(schema) {
			out = append(out, schema)
		}
	}
	if err := rows.Err(); err != nil {
		h.catalogIncomplete = true
		h.limitations = append(h.limitations, fmt.Sprintf("schemas unavailable — %s", oneLine(err.Error())))
	}
	found := map[string]bool{}
	for _, schema := range out {
		found[schema] = true
	}
	for schema := range h.schemaFilter {
		if !found[schema] {
			h.catalogIncomplete = true
			h.limitations = append(h.limitations, fmt.Sprintf("requested schema '%s' was not collected; verify existence and metadata permissions", schema))
		}
	}
	return out
}

func (h *harvester) postgresObjects(schema string) []ObjectDoc {
	objects := []ObjectDoc{}
	h.pgRows("objects", `
		SELECT table_name, table_type
		FROM information_schema.tables WHERE table_schema = $1
		ORDER BY table_name`, []any{schema}, func(rows *sql.Rows) error {
		var name, rawKind string
		if err := rows.Scan(&name, &rawKind); err != nil {
			return err
		}
		kind := rawKind
		switch rawKind {
		case "BASE TABLE", "FOREIGN":
			kind = "table"
		case "VIEW":
			kind = "view"
		}
		objects = append(objects, newObject(name, kind))
		return nil
	})

	views := map[string]*string{}
	h.pgRows("views", `SELECT viewname, definition FROM pg_views WHERE schemaname = $1`, []any{schema}, func(rows *sql.Rows) error {
		var name string
		var body sql.NullString
		if err := rows.Scan(&name, &body); err != nil {
			return err
		}
		if body.Valid {
			v := body.String
			views[name] = &v
		}
		return nil
	})
	for i := range objects {
		if body := views[objects[i].Name]; body != nil {
			objects[i].Body = body
		}
	}
	h.pgRows("materialized views", `SELECT matviewname, definition FROM pg_matviews WHERE schemaname = $1`, []any{schema}, func(rows *sql.Rows) error {
		var name string
		var body sql.NullString
		if err := rows.Scan(&name, &body); err != nil {
			return err
		}
		objects = append(objects, newObjectWithBody(name, "materialized-view", ns(body)))
		return nil
	})
	h.pgRows("sequences", `SELECT sequencename FROM pg_sequences WHERE schemaname = $1`, []any{schema}, func(rows *sql.Rows) error {
		var name string
		if err := rows.Scan(&name); err != nil {
			return err
		}
		objects = append(objects, newObject(name, "sequence"))
		return nil
	})
	return objects
}

// postgresColumns는 엔진과 같은 SQL 파일로 테이블·뷰·materialized view 컬럼을 읽는다.
func (h *harvester) postgresColumns(schema, table string) []ColumnDoc {
	columns := []ColumnDoc{}
	raw, err := catalogQueries.ReadFile("sql/columns-postgres.sql")
	if err != nil {
		h.catalogIncomplete = true
		h.limitations = append(h.limitations, "bundled PostgreSQL column query is missing; rebuild the probe")
		return columns
	}
	query := strings.NewReplacer(":schema", "$1", ":table", "$2").Replace(string(raw))
	h.pgRows("columns", query, []any{schema, table}, func(rows *sql.Rows) error {
		var name, dataType, nullable string
		var def sql.NullString
		var ordinal int
		if err := rows.Scan(&name, &dataType, &nullable, &def, &ordinal); err != nil {
			return err
		}
		columns = append(columns, ColumnDoc{Name: name, DataType: dataType, Nullable: nullable == "YES", Default: ns(def), Ordinal: ordinal})
		return nil
	})
	return columns
}

type postgresConstraintRow struct {
	name, kind, column             string
	refColumn, refTable, refSchema sql.NullString
	position                       int
}

func (h *harvester) postgresConstraints(schema, table string) []ConstraintDoc {
	byName := map[string]*ConstraintDoc{}
	order := []string{}
	h.pgRows("constraints", `
		SELECT con.conname, con.contype::text, local_attr.attname,
		       ref_attr.attname, ref_rel.relname, ref_ns.nspname, local_cols.ord
		FROM pg_constraint con
		JOIN pg_class rel ON rel.oid = con.conrelid
		JOIN pg_namespace ns ON ns.oid = rel.relnamespace
		JOIN LATERAL unnest(con.conkey) WITH ORDINALITY AS local_cols(attnum, ord) ON true
		JOIN pg_attribute local_attr
		  ON local_attr.attrelid = rel.oid AND local_attr.attnum = local_cols.attnum
		LEFT JOIN pg_class ref_rel ON ref_rel.oid = con.confrelid
		LEFT JOIN pg_namespace ref_ns ON ref_ns.oid = ref_rel.relnamespace
		LEFT JOIN LATERAL unnest(con.confkey) WITH ORDINALITY AS ref_cols(attnum, ord)
		  ON ref_cols.ord = local_cols.ord
		LEFT JOIN pg_attribute ref_attr
		  ON ref_attr.attrelid = ref_rel.oid AND ref_attr.attnum = ref_cols.attnum
		WHERE ns.nspname = $1 AND rel.relname = $2 AND con.contype IN ('p','f','u')
		ORDER BY con.conname, local_cols.ord`, []any{schema, table}, func(rows *sql.Rows) error {
		var r postgresConstraintRow
		if err := rows.Scan(&r.name, &r.kind, &r.column, &r.refColumn, &r.refTable, &r.refSchema, &r.position); err != nil {
			return err
		}
		constraint := byName[r.name]
		if constraint == nil {
			constraint = &ConstraintDoc{Name: r.name, Kind: postgresConstraintKind(r.kind), Columns: []string{}}
			byName[r.name] = constraint
			order = append(order, r.name)
		}
		constraint.Columns = append(constraint.Columns, r.column)
		if r.kind == "f" && r.refTable.Valid {
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

func postgresConstraintKind(kind string) string {
	switch kind {
	case "p":
		return "pk"
	case "f":
		return "fk"
	default:
		return "unique"
	}
}

func (h *harvester) postgresIndexes(schema, table string) []IndexDoc {
	byName := map[string]*IndexDoc{}
	order := []string{}
	h.pgRows("indexes", `
		SELECT cls.relname, idx.indisunique, attr.attname, index_cols.ord,
		       idx.indisvalid AND idx.indisready, pg_get_expr(idx.indpred, idx.indrelid)
		FROM pg_index idx
		JOIN pg_class cls ON cls.oid = idx.indexrelid
		JOIN pg_class tbl ON tbl.oid = idx.indrelid
		JOIN pg_namespace ns ON ns.oid = tbl.relnamespace
		JOIN LATERAL unnest(idx.indkey) WITH ORDINALITY AS index_cols(attnum, ord) ON index_cols.ord <= idx.indnkeyatts
		LEFT JOIN pg_attribute attr ON attr.attrelid = tbl.oid AND attr.attnum = index_cols.attnum
		WHERE ns.nspname = $1 AND tbl.relname = $2 AND NOT idx.indisprimary
		ORDER BY cls.relname, index_cols.ord`, []any{schema, table}, func(rows *sql.Rows) error {
		var name string
		var unique bool
		var column sql.NullString
		var ordinal int
		var ready bool
		var predicate sql.NullString
		if err := rows.Scan(&name, &unique, &column, &ordinal, &ready, &predicate); err != nil {
			return err
		}
		idx := byName[name]
		if idx == nil {
			idx = &IndexDoc{Name: name, Unique: unique, Columns: []string{}, DefinitionComplete: boolValue(ready), HasPredicate: boolValue(predicate.Valid), Predicate: ns(predicate)}
			byName[name] = idx
			order = append(order, name)
		}
		if column.Valid {
			idx.Columns = append(idx.Columns, column.String)
		} else {
			idx.DefinitionComplete = boolValue(false)
		}
		return nil
	})
	out := make([]IndexDoc, 0, len(order))
	for _, name := range order {
		out = append(out, *byName[name])
	}
	return out
}

func (h *harvester) postgresTriggers(schema, table string) []TriggerDoc {
	triggers := []TriggerDoc{}
	h.pgRows("triggers", `
		SELECT tg.tgname, pg_get_triggerdef(tg.oid)
		FROM pg_trigger tg
		JOIN pg_class rel ON rel.oid = tg.tgrelid
		JOIN pg_namespace ns ON ns.oid = rel.relnamespace
		WHERE NOT tg.tgisinternal AND ns.nspname = $1 AND rel.relname = $2
		ORDER BY tg.tgname`, []any{schema, table}, func(rows *sql.Rows) error {
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

func (h *harvester) postgresRoutines(schema string) []RoutineDoc {
	routines := []RoutineDoc{}
	h.pgRows("routines", `
		SELECT p.proname, l.lanname, p.prokind::text,
		       pg_get_function_identity_arguments(p.oid),
		       CASE WHEN p.prokind = 'a' THEN NULL ELSE pg_get_functiondef(p.oid) END
		FROM pg_proc p
		JOIN pg_namespace ns ON ns.oid = p.pronamespace
		JOIN pg_language l ON l.oid = p.prolang
		WHERE ns.nspname = $1 AND p.prokind IN ('f', 'p', 'a', 'w')
		ORDER BY p.proname, pg_get_function_identity_arguments(p.oid), p.oid`, []any{schema}, func(rows *sql.Rows) error {
		var name, language, prokind, signature string
		var body sql.NullString
		if err := rows.Scan(&name, &language, &prokind, &signature, &body); err != nil {
			return err
		}
		kind := ""
		switch prokind {
		case "f", "a", "w":
			kind = "function"
		case "p":
			kind = "procedure"
		default:
			return nil
		}
		lang := language
		sig := signature
		routines = append(routines, RoutineDoc{Name: name, Kind: kind, Language: &lang, Body: ns(body), Signature: &sig})
		return nil
	})
	return routines
}

func (h *harvester) postgresObjectUsage(schema string, objects []ObjectDoc) {
	var since sql.NullString
	if err := h.db.QueryRow(`SELECT COALESCE(stats_reset::text, pg_postmaster_start_time()::text)
		FROM pg_stat_database WHERE datname = current_database()`).Scan(&since); err != nil {
		h.limitations = append(h.limitations, fmt.Sprintf("%s: pg stats reset time unavailable — %s", schema, oneLine(err.Error())))
	}
	// schema 전체를 한 번에 읽어 같은 통계 조회를 객체마다 반복하지 않는다.
	tableReads, tableWrites := h.postgresTableUsage(schema)
	indexReads := h.postgresIndexUsage(schema)
	for i := range objects {
		obj := &objects[i]
		if usage, ok := tableReads[obj.Name]; ok {
			obj.Usage = &UsageDoc{Since: ns(since), Reads: maxInt64(usage), Writes: maxInt64(tableWrites[obj.Name])}
		}
		for j := range obj.Indexes {
			if reads, ok := indexReads[obj.Name+"\x00"+obj.Indexes[j].Name]; ok {
				obj.Indexes[j].Usage = &UsageDoc{Since: ns(since), Reads: maxInt64(reads)}
			}
		}
	}
}

func (h *harvester) postgresTableUsage(schema string) (map[string]int64, map[string]int64) {
	reads, writes := map[string]int64{}, map[string]int64{}
	rows, err := h.db.Query(`SELECT relname, seq_tup_read + COALESCE(idx_tup_fetch, 0),
		n_tup_ins + n_tup_upd + n_tup_del FROM pg_stat_user_tables WHERE schemaname = $1`, schema)
	if err != nil {
		h.limitations = append(h.limitations, fmt.Sprintf("pg_stat_user_tables unavailable; no usage evidence — %s", oneLine(err.Error())))
		return reads, writes
	}
	defer rows.Close()
	for rows.Next() {
		var name string
		var read, write int64
		if err := rows.Scan(&name, &read, &write); err != nil {
			h.limitations = append(h.limitations, fmt.Sprintf("pg_stat_user_tables row read failed — %s", oneLine(err.Error())))
			return reads, writes
		}
		reads[name], writes[name] = read, write
	}
	if err := rows.Err(); err != nil {
		h.limitations = append(h.limitations, fmt.Sprintf("pg_stat_user_tables unavailable — %s", oneLine(err.Error())))
	}
	return reads, writes
}

func (h *harvester) postgresIndexUsage(schema string) map[string]int64 {
	out := map[string]int64{}
	rows, err := h.db.Query(`SELECT relname, indexrelname, idx_scan FROM pg_stat_user_indexes WHERE schemaname = $1`, schema)
	if err != nil {
		h.limitations = append(h.limitations, fmt.Sprintf("pg_stat_user_indexes unavailable; no index usage evidence — %s", oneLine(err.Error())))
		return out
	}
	defer rows.Close()
	for rows.Next() {
		var table, index string
		var reads int64
		if err := rows.Scan(&table, &index, &reads); err != nil {
			h.limitations = append(h.limitations, fmt.Sprintf("pg_stat_user_indexes row read failed — %s", oneLine(err.Error())))
			return out
		}
		out[table+"\x00"+index] = reads
	}
	if err := rows.Err(); err != nil {
		h.limitations = append(h.limitations, fmt.Sprintf("pg_stat_user_indexes unavailable — %s", oneLine(err.Error())))
	}
	return out
}

func (h *harvester) postgresRoutineUsage(schema string, routines []RoutineDoc) {
	var since sql.NullString
	if err := h.db.QueryRow(`SELECT COALESCE(stats_reset::text, pg_postmaster_start_time()::text)
		FROM pg_stat_database WHERE datname = current_database()`).Scan(&since); err != nil {
		h.limitations = append(h.limitations, fmt.Sprintf("%s: pg stats reset time unavailable — %s", schema, oneLine(err.Error())))
	}
	var tracking string
	if err := h.db.QueryRow(`SELECT current_setting('track_functions')`).Scan(&tracking); err != nil {
		h.limitations = append(h.limitations, fmt.Sprintf("%s: failed to inspect track_functions — %s", schema, oneLine(err.Error())))
		return
	}
	if tracking == "none" {
		h.limitations = append(h.limitations, "track_functions=none — routine usage unavailable because function statistics are disabled")
		return
	}
	rows, err := h.db.Query(`SELECT funcname, calls, total_time, self_time FROM pg_stat_user_functions WHERE schemaname = $1`, schema)
	if err != nil {
		h.limitations = append(h.limitations, fmt.Sprintf("pg_stat_user_functions unavailable; no routine usage evidence — %s", oneLine(err.Error())))
		return
	}
	defer rows.Close()
	ambiguous := 0
	for rows.Next() {
		var name string
		var calls int64
		var total, self sql.NullFloat64
		if err := rows.Scan(&name, &calls, &total, &self); err != nil {
			h.limitations = append(h.limitations, fmt.Sprintf("pg_stat_user_functions row read failed — %s", oneLine(err.Error())))
			return
		}
		hits := []int{}
		for i := range routines {
			if routines[i].Name == name {
				hits = append(hits, i)
			}
		}
		if len(hits) == 1 {
			routines[hits[0]].Usage = &UsageDoc{Since: ns(since), Reads: maxInt64(calls), TotalMs: nullFloat(total), SelfMs: nullFloat(self)}
		} else if len(hits) > 1 {
			ambiguous++
		}
	}
	if err := rows.Err(); err != nil {
		h.limitations = append(h.limitations, fmt.Sprintf("pg_stat_user_functions unavailable — %s", oneLine(err.Error())))
	}
	if ambiguous > 0 {
		h.limitations = append(h.limitations, fmt.Sprintf("%s: usage unavailable for %d overloaded routines because funcname is ambiguous", schema, ambiguous))
	}
}

// pgRows는 보조 조회 실패를 빈 결과로 위장하지 않고 limitation에 남긴다.
func (h *harvester) pgRows(label, query string, args []any, scan func(*sql.Rows) error) {
	rows, err := h.db.Query(query, args...)
	if err != nil {
		h.catalogIncomplete = true
		h.limitations = append(h.limitations, fmt.Sprintf("%s unavailable — %s", label, oneLine(err.Error())))
		return
	}
	defer rows.Close()
	for rows.Next() {
		if err := scan(rows); err != nil {
			h.catalogIncomplete = true
			h.limitations = append(h.limitations, fmt.Sprintf("%s row read failed — %s", label, oneLine(err.Error())))
			return
		}
	}
	if err := rows.Err(); err != nil {
		h.catalogIncomplete = true
		h.limitations = append(h.limitations, fmt.Sprintf("%s unavailable — %s", label, oneLine(err.Error())))
	}
}

func newObject(name, kind string) ObjectDoc {
	return newObjectWithBody(name, kind, nil)
}

func newObjectWithBody(name, kind string, body *string) ObjectDoc {
	return ObjectDoc{Name: name, Kind: kind, Columns: []ColumnDoc{}, Constraints: []ConstraintDoc{}, Indexes: []IndexDoc{}, Triggers: []TriggerDoc{}, Body: body}
}

func normalizeSchema(schema *SchemaDoc) {
	sort.Slice(schema.Objects, func(i, j int) bool {
		if schema.Objects[i].Name != schema.Objects[j].Name {
			return schema.Objects[i].Name < schema.Objects[j].Name
		}
		return schema.Objects[i].Kind < schema.Objects[j].Kind
	})
	sort.Slice(schema.Routines, func(i, j int) bool {
		if schema.Routines[i].Name != schema.Routines[j].Name {
			return schema.Routines[i].Name < schema.Routines[j].Name
		}
		var a, b string
		if schema.Routines[i].Signature != nil {
			a = *schema.Routines[i].Signature
		}
		if schema.Routines[j].Signature != nil {
			b = *schema.Routines[j].Signature
		}
		if a != b {
			return a < b
		}
		return schema.Routines[i].Kind < schema.Routines[j].Kind
	})
	for i := range schema.Objects {
		obj := &schema.Objects[i]
		sort.Slice(obj.Columns, func(a, b int) bool { return obj.Columns[a].Ordinal < obj.Columns[b].Ordinal })
		sort.Slice(obj.Constraints, func(a, b int) bool { return obj.Constraints[a].Name < obj.Constraints[b].Name })
		sort.Slice(obj.Indexes, func(a, b int) bool { return obj.Indexes[a].Name < obj.Indexes[b].Name })
		sort.Slice(obj.Triggers, func(a, b int) bool { return obj.Triggers[a].Name < obj.Triggers[b].Name })
		if obj.Columns == nil {
			obj.Columns = []ColumnDoc{}
		}
		if obj.Constraints == nil {
			obj.Constraints = []ConstraintDoc{}
		}
		if obj.Indexes == nil {
			obj.Indexes = []IndexDoc{}
		}
		if obj.Triggers == nil {
			obj.Triggers = []TriggerDoc{}
		}
	}
	if schema.Objects == nil {
		schema.Objects = []ObjectDoc{}
	}
	if schema.Routines == nil {
		schema.Routines = []RoutineDoc{}
	}
}

func maxInt64(value int64) int64 {
	if value < 0 {
		return 0
	}
	return value
}

func nullFloat(value sql.NullFloat64) *float64 {
	if !value.Valid {
		return nil
	}
	v := value.Float64
	return &v
}
