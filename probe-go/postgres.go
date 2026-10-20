package main

import (
	"database/sql"
	"errors"
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
	h.postgresRelationDetails(schema, objects)
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
	h.checkRequestedSchemas(out)
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

// applyPrimaryKeyPositions는 PK 제약의 키 순서를 컬럼의 PkPosition에 옮긴다.
// 0으로 남으면 엔진이 "PK 없음"과 "못 읽음"을 구분하지 못한다 — 엔진 native와 같은 규칙이다.
func applyPrimaryKeyPositions(obj *ObjectDoc) {
	for _, constraint := range obj.Constraints {
		if constraint.Kind != "pk" {
			continue
		}
		for i := range obj.Columns {
			for position, name := range constraint.Columns {
				if name == obj.Columns[i].Name {
					obj.Columns[i].PkPosition = position + 1
				}
			}
		}
		return
	}
}

// postgresRelationDetails는 컬럼·제약·인덱스·트리거를 스키마마다 한 번씩 읽어 관계 이름으로 객체에 나눠 단다.
// 객체마다 네 번씩 물으면 고정 연결 하나에서 직렬 왕복이 객체 수의 네 배가 되고, 원격 DB에서는 수집 시간이
// 왕복 지연에 비례한다(2 ms RTT에서 2,000테이블이 8,036문장, 22.6 s). 객체가 없는 스키마에는 묻지 않는다 —
// 예전에도 질의가 없었으니 그 스키마에서 실패 limitation이 새로 생기지 않는다.
func (h *harvester) postgresRelationDetails(schema string, objects []ObjectDoc) {
	if len(objects) == 0 {
		return
	}
	columns := h.postgresRelationColumns(schema)
	constraints := h.postgresSchemaConstraints(schema)
	indexes := h.postgresSchemaIndexes(schema)
	triggers := h.postgresSchemaTriggers(schema)
	for i := range objects {
		obj := &objects[i]
		obj.Columns, obj.Constraints = nonNil(columns[obj.Name]), nonNil(constraints[obj.Name])
		applyPrimaryKeyPositions(obj)
		obj.Indexes, obj.Triggers = nonNil(indexes[obj.Name]), nonNil(triggers[obj.Name])
	}
}

// nonNil은 nil 슬라이스를 빈 슬라이스로 바꾼다. 스키마 단위 조회에 행이 없는 객체(뷰의 제약, 시퀀스)도
// JSON에서 null이 아니라 []여야 문서 바이트가 객체마다 읽던 때와 같다.
func nonNil[T any](values []T) []T {
	if values == nil {
		return []T{}
	}
	return values
}

// relationGroups는 스키마 단위 조회의 행을 관계 이름별로, 관계 안에서는 항목(제약·인덱스) 이름별로 모은다.
// 항목은 처음 나온 순서를 지킨다. 관계 안의 행 순서는 테이블마다 읽던 질의의 ORDER BY 순서와 같다.
type relationGroups[T any] struct {
	items map[string]map[string]*T
	order map[string][]string
}

func newRelationGroups[T any]() *relationGroups[T] {
	return &relationGroups[T]{items: map[string]map[string]*T{}, order: map[string][]string{}}
}

// entry는 관계 relation의 항목 name을 돌려준다. 처음 보는 항목이면 create로 만들고 순서에 더한다.
func (g *relationGroups[T]) entry(relation, name string, create func() T) *T {
	items := g.items[relation]
	if items == nil {
		items = map[string]*T{}
		g.items[relation] = items
	}
	if item := items[name]; item != nil {
		return item
	}
	item := create()
	items[name] = &item
	g.order[relation] = append(g.order[relation], name)
	return &item
}

// lists는 관계마다 항목을 처음 나온 순서의 슬라이스로 돌려준다.
func (g *relationGroups[T]) lists() map[string][]T {
	out := make(map[string][]T, len(g.order))
	for relation, names := range g.order {
		list := make([]T, 0, len(names))
		for _, name := range names {
			list = append(list, *g.items[relation][name])
		}
		out[relation] = list
	}
	return out
}

// postgresSchemaColumnSubstitutions는 관계 하나를 읽는 정본 컬럼 질의(sql/columns-postgres.sql)를 스키마
// 전체를 읽는 질의로 바꾸는 치환이다. 정본은 엔진 native reader와 같은 파일임을 CI가 cmp로 확인하므로,
// 프로브 전용 사본을 두면 두 벌이 갈라질 수 있어 실행 시 파생한다. 관계 이름 열을 더하고, 관계 조건 두
// 곳을 빼고, 관계 이름으로 먼저 정렬한다.
var postgresSchemaColumnSubstitutions = [][2]string{
	{"SELECT column_name::text AS column_name,", "SELECT table_name::text AS relation_name, column_name::text AS column_name,"},
	{"WHERE table_schema = :schema AND table_name = :table", "WHERE table_schema = :schema"},
	{"SELECT a.attname::text,", "SELECT c.relname::text, a.attname::text,"},
	{"WHERE n.nspname = :schema AND c.relname = :table AND c.relkind = 'm'", "WHERE n.nspname = :schema AND c.relkind = 'm'"},
	{"ORDER BY ordinal_position", "ORDER BY relation_name, ordinal_position"},
}

// postgresSchemaColumnQuery는 정본 컬럼 질의에서 스키마 단위 질의를 만든다. 치환 원문이 없거나 여러 번
// 맞으면(정본이 바뀐 빌드) 엉뚱한 곳을 바꾼 질의를 보내지 않도록 오류를 낸다.
func postgresSchemaColumnQuery(canonical string) (string, error) {
	query := canonical
	for _, substitution := range postgresSchemaColumnSubstitutions {
		if count := strings.Count(query, substitution[0]); count != 1 {
			return "", fmt.Errorf("expected %q once, found it %d times", substitution[0], count)
		}
		query = strings.Replace(query, substitution[0], substitution[1], 1)
	}
	if strings.Contains(query, ":table") {
		return "", errors.New("a :table filter remains")
	}
	return strings.ReplaceAll(query, ":schema", "$1"), nil
}

// postgresRelationColumns는 엔진과 같은 정본 컬럼 질의 파일로 스키마의 컬럼을 관계별로 읽는다.
func (h *harvester) postgresRelationColumns(schema string) map[string][]ColumnDoc {
	raw, err := catalogQueries.ReadFile("sql/columns-postgres.sql")
	if err != nil {
		h.markIncomplete("bundled PostgreSQL column query is missing; rebuild the probe")
		return map[string][]ColumnDoc{}
	}
	return h.postgresSchemaColumns(schema, string(raw))
}

// postgresSchemaColumns는 테이블·뷰·materialized view 컬럼을 스키마마다 한 번 읽어 관계 이름별로 나눈다.
// 관계 안의 순서는 정본과 같은 ordinal_position이다.
func (h *harvester) postgresSchemaColumns(schema, canonical string) map[string][]ColumnDoc {
	columns := map[string][]ColumnDoc{}
	query, err := postgresSchemaColumnQuery(canonical)
	if err != nil {
		h.markIncomplete(fmt.Sprintf("bundled PostgreSQL column query could not be adapted to schema-wide reads (%s); rebuild the probe", err))
		return columns
	}
	h.pgRows("columns", query, []any{schema}, func(rows *sql.Rows) error {
		var relation, name, dataType, nullable string
		var def sql.NullString
		var ordinal int
		if err := rows.Scan(&relation, &name, &dataType, &nullable, &def, &ordinal); err != nil {
			return err
		}
		columns[relation] = append(columns[relation], ColumnDoc{Name: name, DataType: dataType, Nullable: nullable == "YES", Default: ns(def), Ordinal: ordinal})
		return nil
	})
	return columns
}

type postgresConstraintRow struct {
	name, kind, column             string
	refColumn, refTable, refSchema sql.NullString
	position                       int
}

// postgresSchemaConstraints는 PK·FK·UNIQUE 제약을 스키마마다 한 번 읽어 관계 이름별로 나눈다. 관계 안의
// 순서는 테이블마다 읽던 때와 같은 (제약 이름, 키 순서)다.
func (h *harvester) postgresSchemaConstraints(schema string) map[string][]ConstraintDoc {
	groups := newRelationGroups[ConstraintDoc]()
	h.pgRows("constraints", `
		SELECT rel.relname, con.conname, con.contype::text, local_attr.attname,
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
		WHERE ns.nspname = $1 AND con.contype IN ('p','f','u')
		ORDER BY rel.relname, con.conname, local_cols.ord`, []any{schema}, func(rows *sql.Rows) error {
		var relation string
		var r postgresConstraintRow
		if err := rows.Scan(&relation, &r.name, &r.kind, &r.column, &r.refColumn, &r.refTable, &r.refSchema, &r.position); err != nil {
			return err
		}
		constraint := groups.entry(relation, r.name, func() ConstraintDoc {
			return ConstraintDoc{Name: r.name, Kind: postgresConstraintKind(r.kind), Columns: []string{}}
		})
		addPostgresConstraintRow(constraint, r)
		return nil
	})
	return groups.lists()
}

// addPostgresConstraintRow는 제약 행 하나의 키 컬럼과, FK면 대상 테이블·컬럼을 덧붙인다.
func addPostgresConstraintRow(constraint *ConstraintDoc, r postgresConstraintRow) {
	constraint.Columns = append(constraint.Columns, r.column)
	if r.kind != "f" || !r.refTable.Valid {
		return
	}
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

// postgresSchemaIndexes는 PK가 아닌 인덱스를 스키마마다 한 번 읽어 테이블 이름별로 나눈다. 테이블 안의
// 순서는 테이블마다 읽던 때와 같은 (인덱스 이름, 키 순서)다. 식 키는 컬럼이 없어 정의가 불완전하다.
func (h *harvester) postgresSchemaIndexes(schema string) map[string][]IndexDoc {
	groups := newRelationGroups[IndexDoc]()
	h.pgRows("indexes", `
		SELECT tbl.relname, cls.relname, idx.indisunique, attr.attname, index_cols.ord,
		       idx.indisvalid AND idx.indisready, pg_get_expr(idx.indpred, idx.indrelid)
		FROM pg_index idx
		JOIN pg_class cls ON cls.oid = idx.indexrelid
		JOIN pg_class tbl ON tbl.oid = idx.indrelid
		JOIN pg_namespace ns ON ns.oid = tbl.relnamespace
		JOIN LATERAL unnest(idx.indkey) WITH ORDINALITY AS index_cols(attnum, ord) ON index_cols.ord <= idx.indnkeyatts
		LEFT JOIN pg_attribute attr ON attr.attrelid = tbl.oid AND attr.attnum = index_cols.attnum
		WHERE ns.nspname = $1 AND NOT idx.indisprimary
		ORDER BY tbl.relname, cls.relname, index_cols.ord`, []any{schema}, func(rows *sql.Rows) error {
		var relation, name string
		var unique, ready bool
		var column, predicate sql.NullString
		var ordinal int
		if err := rows.Scan(&relation, &name, &unique, &column, &ordinal, &ready, &predicate); err != nil {
			return err
		}
		idx := groups.entry(relation, name, func() IndexDoc {
			return IndexDoc{Name: name, Unique: unique, Columns: []string{}, DefinitionComplete: boolValue(ready), HasPredicate: boolValue(predicate.Valid), Predicate: ns(predicate)}
		})
		if column.Valid {
			idx.Columns = append(idx.Columns, column.String)
		} else {
			idx.DefinitionComplete = boolValue(false)
		}
		return nil
	})
	return groups.lists()
}

// postgresSchemaTriggers는 내부용이 아닌 트리거를 스키마마다 한 번 읽어 관계 이름별로 나눈다.
func (h *harvester) postgresSchemaTriggers(schema string) map[string][]TriggerDoc {
	triggers := map[string][]TriggerDoc{}
	h.pgRows("triggers", `
		SELECT rel.relname, tg.tgname, pg_get_triggerdef(tg.oid)
		FROM pg_trigger tg
		JOIN pg_class rel ON rel.oid = tg.tgrelid
		JOIN pg_namespace ns ON ns.oid = rel.relnamespace
		WHERE NOT tg.tgisinternal AND ns.nspname = $1
		ORDER BY rel.relname, tg.tgname`, []any{schema}, func(rows *sql.Rows) error {
		var relation, name string
		var body sql.NullString
		if err := rows.Scan(&relation, &name, &body); err != nil {
			return err
		}
		triggers[relation] = append(triggers[relation], TriggerDoc{Name: name, Body: ns(body)})
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
	// track_counts=off면 모든 카운터가 0인 행이 나온다 — 관측된 0이 아니므로 싣지 않는다.
	if !h.postgresTrackCounts(schema) {
		return
	}
	// schema 전체를 한 번에 읽어 같은 통계 조회를 객체마다 반복하지 않는다.
	tableReads, tableWrites, tableScans := h.postgresTableUsage(schema)
	indexReads := h.postgresIndexUsage(schema)
	for i := range objects {
		obj := &objects[i]
		if usage, ok := tableReads[obj.Name]; ok {
			scans := maxInt64(tableScans[obj.Name])
			obj.Usage = &UsageDoc{Since: ns(since), Reads: maxInt64(usage), Writes: maxInt64(tableWrites[obj.Name]), Scans: &scans}
		}
		for j := range obj.Indexes {
			if reads, ok := indexReads[obj.Name+"\x00"+obj.Indexes[j].Name]; ok {
				obj.Indexes[j].Usage = &UsageDoc{Since: ns(since), Reads: maxInt64(reads)}
			}
		}
	}
}

// postgresTrackCounts는 track_counts가 켜져 있는지 본다. 꺼져 있거나 확인하지 못하면 limitation을 남기고 거짓이다.
func (h *harvester) postgresTrackCounts(schema string) bool {
	var tracking string
	if err := h.db.QueryRow(`SELECT current_setting('track_counts')`).Scan(&tracking); err != nil {
		h.limitations = append(h.limitations, fmt.Sprintf("%s: failed to inspect track_counts; no table/index usage — %s", schema, oneLine(err.Error())))
		return false
	}
	if tracking != "on" {
		h.limitations = append(h.limitations, "track_counts=off — table/index usage unavailable because counters are disabled (zero rows are not observations)")
		return false
	}
	return true
}

// postgresTableUsage는 테이블 읽기(튜플)·쓰기·스캔 횟수를 읽는다.
// 파티션 부모(relkind p)는 스캔이 리프 파티션에 세어져 항상 0이라 제외한다.
func (h *harvester) postgresTableUsage(schema string) (map[string]int64, map[string]int64, map[string]int64) {
	reads, writes, scans := map[string]int64{}, map[string]int64{}, map[string]int64{}
	rows, err := h.db.Query(`SELECT s.relname, s.seq_tup_read + COALESCE(s.idx_tup_fetch, 0),
		s.n_tup_ins + s.n_tup_upd + s.n_tup_del, s.seq_scan + COALESCE(s.idx_scan, 0)
		FROM pg_stat_user_tables s JOIN pg_class c ON c.oid = s.relid
		WHERE s.schemaname = $1 AND c.relkind <> 'p'`, schema)
	if err != nil {
		h.limitations = append(h.limitations, fmt.Sprintf("pg_stat_user_tables unavailable; no usage evidence — %s", oneLine(err.Error())))
		return reads, writes, scans
	}
	defer rows.Close()
	for rows.Next() {
		var name string
		var read, write, scan int64
		if err := rows.Scan(&name, &read, &write, &scan); err != nil {
			h.limitations = append(h.limitations, fmt.Sprintf("pg_stat_user_tables row read failed — %s", oneLine(err.Error())))
			return reads, writes, scans
		}
		reads[name], writes[name], scans[name] = read, write, scan
	}
	if err := rows.Err(); err != nil {
		h.limitations = append(h.limitations, fmt.Sprintf("pg_stat_user_tables unavailable — %s", oneLine(err.Error())))
	}
	return reads, writes, scans
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
	positions := routinePositions(routines)
	ambiguous := 0
	for rows.Next() {
		var name string
		var calls int64
		var total, self sql.NullFloat64
		if err := rows.Scan(&name, &calls, &total, &self); err != nil {
			h.limitations = append(h.limitations, fmt.Sprintf("pg_stat_user_functions row read failed — %s", oneLine(err.Error())))
			return
		}
		hits := positions[name]
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

// routinePositions는 이름마다 routines 안의 위치를 모은다. pg_stat_user_functions의 funcname에는
// 시그니처가 없어서, 위치가 하나면 귀속하고 오버로드로 여럿이면 모호하다고 센다. 통계 행마다
// routines 전체를 훑으면 스키마마다 O(행 수 × routine 수)가 된다.
func routinePositions(routines []RoutineDoc) map[string][]int {
	positions := make(map[string][]int, len(routines))
	for i := range routines {
		positions[routines[i].Name] = append(positions[routines[i].Name], i)
	}
	return positions
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
