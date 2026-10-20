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
	doc := CatalogDocument{
		Version: documentVersion, Dialect: "mysql", Reader: "probe-go",
		Schemas: []SchemaDoc{}, Limitations: []string{},
	}
	for _, schema := range schemas {
		doc.Schemas = append(doc.Schemas, h.mysqlSchema(schema))
	}
	doc.Limitations = append([]string{}, h.limitations...)
	return doc
}

func (h *harvester) mysqlSchema(schema string) SchemaDoc {
	objects := h.mysqlObjects(schema)
	h.mysqlRelationDetails(schema, objects)
	h.mysqlObjectUsage(schema, objects)
	sd := SchemaDoc{Name: schema, Objects: objects, Routines: h.mysqlRoutines(schema)}
	normalizeSchema(&sd)
	return sd
}

func (h *harvester) streamMySQL(stream *ndjsonStreamWriter) error {
	for _, schema := range h.mysqlSchemas() {
		if err := stream.schema(h.mysqlSchema(schema)); err != nil {
			return err
		}
		if err := h.streamDependencies(stream, schema); err != nil {
			return err
		}
	}
	return nil
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

// mysqlSchemas — 목록을 끝까지 읽지 못하면 스키마가 빠졌을 수 있으므로 미완전으로 표시한다.
// 실패 처리와 요청 스키마 대조는 postgresSchemas와 같다.
func (h *harvester) mysqlSchemas() []string {
	rows, err := h.db.Query(`SELECT CAST(schema_name AS CHAR) FROM information_schema.schemata ORDER BY schema_name`)
	if err != nil {
		h.markIncomplete(fmt.Sprintf("schemas unavailable — %s", oneLine(err.Error())))
		return []string{}
	}
	defer rows.Close()
	var out []string
	for rows.Next() {
		var schema string
		if err := rows.Scan(&schema); err != nil {
			h.markIncomplete(fmt.Sprintf("schema row read failed — %s", oneLine(err.Error())))
			break
		}
		if h.mysqlSchemaAllowed(schema) {
			out = append(out, schema)
		}
	}
	if err := rows.Err(); err != nil {
		h.markIncomplete(fmt.Sprintf("schemas unavailable — %s", oneLine(err.Error())))
	}
	h.checkRequestedSchemas(out)
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

	positions := objectPositions(objects)
	h.mysqlRows("views", `
		SELECT CAST(table_name AS CHAR), CAST(view_definition AS CHAR)
		FROM information_schema.views WHERE table_schema = ?`, []any{schema}, func(rows *sql.Rows) error {
		var name string
		var body sql.NullString
		if err := rows.Scan(&name, &body); err != nil {
			return err
		}
		for _, i := range positions[name] {
			objects[i].Body = ns(body)
		}
		return nil
	})
	return objects
}

// objectPositions는 이름마다 objects 안의 위치를 모은다. 뷰 본문·사용 통계 행을 이름으로 귀속할
// 때 행마다 objects 전체를 훑으면 스키마마다 O(행 수 × 객체 수)가 된다. 같은 이름이 여러 번 있으면
// 예전 선형 탐색처럼 그 모두에 귀속하도록 위치를 전부 담는다.
func objectPositions(objects []ObjectDoc) map[string][]int {
	positions := make(map[string][]int, len(objects))
	for i := range objects {
		positions[objects[i].Name] = append(positions[objects[i].Name], i)
	}
	return positions
}

// indexPosition은 objects[object].Indexes[index]를 가리킨다.
type indexPosition struct{ object, index int }

// indexPositions는 "테이블\x00인덱스" 키마다 인덱스 위치를 모은다. MySQL 식별자에는 NUL이 없어
// 두 이름의 경계가 섞이지 않는다(postgresObjectUsage의 인덱스 키와 같은 규칙).
func indexPositions(objects []ObjectDoc) map[string][]indexPosition {
	positions := map[string][]indexPosition{}
	for i := range objects {
		for j := range objects[i].Indexes {
			key := objects[i].Name + "\x00" + objects[i].Indexes[j].Name
			positions[key] = append(positions[key], indexPosition{object: i, index: j})
		}
	}
	return positions
}

// mysqlRelationDetails는 컬럼·PK 위치·제약·키 컬럼·인덱스·트리거를 스키마마다 한 번씩 읽어 테이블 이름으로 객체에
// 나눠 단다. 객체마다 다섯 번씩 물으면 고정 연결 하나에서 직렬 왕복이 객체 수에 비례한다. go-sql-driver/mysql은 interpolateParams가
// 꺼져 있으면 인자 있는 질의를 prepare와 execute 두 번 왕복으로 보내, 원격 DB에서는 테이블당 약 10왕복이었다.
// 컬럼·PK 위치·인덱스·트리거를 테이블마다 묻던 table_name = ? 조건은 그 이름의 테이블 행만 돌려줬다
// (lower_case_table_names=0인 MySQL 8.4와 MariaDB 11.4에서 대소문자·악센트만 다른 T·t, cafe·café로 확인). 그래서 정확한
// 이름으로 나눈 결과가 테이블마다 묻던 결과와 같다. 제약은 mysqlSchemaConstraints를 본다. 객체가 없는 스키마에는 묻지
// 않는다 — 예전에도 질의가 없었으니 그 스키마에서 실패 limitation이 새로 생기지 않는다.
func (h *harvester) mysqlRelationDetails(schema string, objects []ObjectDoc) {
	if len(objects) == 0 {
		return
	}
	columns := h.mysqlSchemaColumns(schema)
	constraints := h.mysqlSchemaConstraints(schema)
	indexes, expressionColumns := h.mysqlSchemaIndexes(schema)
	triggers := h.mysqlSchemaTriggers(schema)
	for i := range objects {
		obj := &objects[i]
		obj.Columns, obj.Constraints = nonNil(columns[obj.Name]), nonNil(constraints[obj.Name])
		obj.Indexes, obj.Triggers = nonNil(indexes[obj.Name]), nonNil(triggers[obj.Name])
		if count := expressionColumns[obj.Name]; count > 0 {
			h.limitations = append(h.limitations, fmt.Sprintf("%s.%s: omitted %d functional index columns because they have no names", schema, obj.Name, count))
		}
	}
}

// mysqlPrimaryKeyPositions는 테이블마다 PK 컬럼의 PK 안 위치(1부터)를 읽는다. 끝까지 읽지 못하면 모든 위치를 비운다
// (일부 테이블에만 위치가 남지 않게).
func (h *harvester) mysqlPrimaryKeyPositions(schema string) map[string]map[string]int {
	positions := map[string]map[string]int{}
	complete := h.mysqlRows("primary key columns", `
		SELECT CAST(table_name AS CHAR), CAST(column_name AS CHAR), CAST(ordinal_position AS SIGNED)
		FROM information_schema.key_column_usage
		WHERE table_schema = ? AND constraint_name = 'PRIMARY'
		ORDER BY table_name, ordinal_position`, []any{schema}, func(rows *sql.Rows) error {
		var table, name string
		var position int
		if err := rows.Scan(&table, &name, &position); err != nil {
			return err
		}
		if positions[table] == nil {
			positions[table] = map[string]int{}
		}
		positions[table][name] = position
		return nil
	})
	if !complete {
		return map[string]map[string]int{}
	}
	return positions
}

// mysqlSchemaColumns는 스키마의 컬럼을 테이블마다 ordinal_position 순으로 읽어 나누고, PK 컬럼에 PK 안 위치를 단다.
// 컬럼을 끝까지 읽지 못하면 모든 테이블의 컬럼을 비운다(일부 컬럼만 남지 않게).
func (h *harvester) mysqlSchemaColumns(schema string) map[string][]ColumnDoc {
	pkPositions := h.mysqlPrimaryKeyPositions(schema)
	columns := map[string][]ColumnDoc{}
	complete := h.mysqlRows("columns", `
		SELECT CAST(table_name AS CHAR), CAST(column_name AS CHAR), CAST(data_type AS CHAR), CAST(is_nullable AS CHAR),
		       CAST(column_default AS CHAR), CAST(ordinal_position AS SIGNED)
		FROM information_schema.columns
		WHERE table_schema = ? ORDER BY table_name, ordinal_position`, []any{schema}, func(rows *sql.Rows) error {
		var table, name, dataType, nullable string
		var def sql.NullString
		var ordinal int
		if err := rows.Scan(&table, &name, &dataType, &nullable, &def, &ordinal); err != nil {
			return err
		}
		columns[table] = append(columns[table], ColumnDoc{Name: name, DataType: dataType, Nullable: nullable == "YES", Default: ns(def), Ordinal: ordinal, PkPosition: pkPositions[table][name]})
		return nil
	})
	if !complete {
		return map[string][]ColumnDoc{}
	}
	return columns
}

// mysqlSchemaConstraints는 PK·FK·UNIQUE·CHECK 제약을 스키마마다 한 번 읽어 테이블 이름별로 나눈다. 테이블 안의
// 순서는 테이블마다 읽던 때와 같은 (제약 이름, 키 순서)다.
//
// 제약 행(table_constraints)과 키 컬럼 행(key_column_usage)은 따로 읽어, 스키마·테이블·제약 이름이 정확히 같은 것끼리
// 여기서 잇는다. 서버에서 LEFT JOIN으로 이으면 MariaDB 11.4는 두 information_schema 표를 인덱스 없이 블록 중첩
// 반복(BNL)으로 이어, 4,000테이블 스키마에서 약 8,000행끼리 6,400만 번 비교하는 데 4초쯤 걸렸고 테이블 수의 제곱으로
// 늘었다. 결과는 서버 조인에 키 컬럼 소유자 검사를 더하던 앞 방식과 같다. 정확히 같은 이름은 콜레이션 비교로도 같고,
// 한 제약 안의 키 순서는 ordinal_position으로 정해진다. 서버 조인은 MariaDB에서 대소문자·악센트만 다른 테이블·스키마의
// 같은 이름 제약(PRIMARY, 이름이 같은 UNIQUE)끼리도 짝지었지만, 그런 행은 소유자 검사에서 버리던 것이다. 네이티브
// reader(engine/source/src/mysql.rs)도 제약마다 키 컬럼을 정확한 테이블·제약 이름으로 물어 같은 결과를 낸다.
// 키 컬럼이나 제약 행을 끝까지 읽지 못하면 일부 컬럼만 가진 제약이나 일부 제약만 내지 않도록 그 스키마의 제약을 모두
// 비운다(limitation은 남는다). 이름이 같은 제약 행이 둘 이상인 테이블만 예외로, mysqlSharedNameConstraints가 예전
// 서버 조인으로 다시 읽는다.
func (h *harvester) mysqlSchemaConstraints(schema string) map[string][]ConstraintDoc {
	keys, complete := h.mysqlSchemaKeyColumns(schema)
	if !complete {
		return map[string][]ConstraintDoc{}
	}
	groups := newRelationGroups[ConstraintDoc]()
	rowsPerName := map[string]int{}
	sharedTables := map[string]bool{}
	shared := []string{}
	complete = h.mysqlRows("constraints", `
		SELECT CAST(table_name AS CHAR), CAST(constraint_name AS CHAR), CAST(constraint_type AS CHAR)
		FROM information_schema.table_constraints
		WHERE table_schema = ? AND constraint_type IN ('PRIMARY KEY','FOREIGN KEY','UNIQUE','CHECK')
		ORDER BY table_name, constraint_name`, []any{schema}, func(rows *sql.Rows) error {
		var table string
		var name, kind sql.NullString
		if err := rows.Scan(&table, &name, &kind); err != nil {
			return err
		}
		if !name.Valid || !kind.Valid {
			return nil
		}
		owner := mysqlKeyColumnOwner(table, name.String)
		if rowsPerName[owner]++; rowsPerName[owner] > 1 && !sharedTables[table] {
			sharedTables[table] = true
			shared = append(shared, table)
		}
		constraint := groups.entry(table, name.String, func() ConstraintDoc {
			return ConstraintDoc{Name: name.String, Kind: mysqlConstraintKind(kind.String), Columns: []string{}}
		})
		for _, key := range keys[owner] {
			addMySQLKeyColumn(constraint, kind.String, key)
		}
		return nil
	})
	if !complete {
		return map[string][]ConstraintDoc{}
	}
	constraints := groups.lists()
	for _, table := range shared {
		constraints[table] = h.mysqlSharedNameConstraints(schema, table)
	}
	return constraints
}

// mysqlSharedNameConstraints는 이름이 같은 제약 행이 둘 이상인 테이블(같은 이름의 UNIQUE와 FK, MySQL은 UNIQUE와
// CHECK도)의 제약을 예전처럼 테이블 하나의 서버 조인으로 읽는다. 예전 probe는 이런 제약을 이름으로 하나로 합쳤고,
// 조인은 그 이름의 키 컬럼 행을 제약 행마다 짝지어 키 순서로 정렬했다. 그래서 합친 제약의 종류(첫 행)와 컬럼 순서는
// 서버의 동점 순서를 따르고, 따로 읽어 잇는 방식으로는 같게 만들 수 없다(MariaDB 11.4에서는 FK가 UNIQUE가 되어
// 참조 간선이 사라졌다). 이런 테이블은 드물어 테이블마다 질의를 하나 더 보낸다. 키 컬럼은 정확히 같은 이름의 행만
// 쓰고(다른 테이블·스키마 행 제외, mysqlSchemaKeyColumns와 같다), 끝까지 읽지 못하면 이 테이블의 제약을 비운다.
func (h *harvester) mysqlSharedNameConstraints(schema, table string) []ConstraintDoc {
	groups := newRelationGroups[ConstraintDoc]()
	complete := h.mysqlRows("constraints", `
		SELECT CAST(tc.constraint_name AS CHAR), CAST(tc.constraint_type AS CHAR),
		       CAST(kcu.table_schema AS CHAR), CAST(kcu.table_name AS CHAR), CAST(kcu.constraint_name AS CHAR),
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
		var name, kind, ownerSchema, ownerTable, ownerConstraint sql.NullString
		var key mysqlKeyColumn
		if err := rows.Scan(&name, &kind, &ownerSchema, &ownerTable, &ownerConstraint,
			&key.column, &key.refSchema, &key.refTable, &key.refColumn); err != nil {
			return err
		}
		if !name.Valid || !kind.Valid {
			return nil
		}
		constraint := groups.entry(table, name.String, func() ConstraintDoc {
			return ConstraintDoc{Name: name.String, Kind: mysqlConstraintKind(kind.String), Columns: []string{}}
		})
		// 짝이 없는 행(CHECK)은 키 컬럼 열이 모두 NULL이라 더할 것이 없다.
		paired := ownerSchema.Valid || ownerTable.Valid || ownerConstraint.Valid
		if paired && (ownerSchema.String != schema || ownerTable.String != table || ownerConstraint.String != name.String) {
			return nil
		}
		addMySQLKeyColumn(constraint, kind.String, key)
		return nil
	})
	if !complete {
		return []ConstraintDoc{}
	}
	return groups.lists()[table]
}

// mysqlKeyColumn은 key_column_usage 행 하나의 키 컬럼과 FK 대상이다.
type mysqlKeyColumn struct {
	column, refSchema, refTable, refColumn sql.NullString
}

// mysqlKeyColumnOwner는 키 컬럼 행을 제약에 잇는 키("테이블\x00제약")다. MySQL 식별자에는 NUL이 없어 두 이름의
// 경계가 섞이지 않는다(indexPositions와 같은 규칙).
func mysqlKeyColumnOwner(table, constraint string) string {
	return table + "\x00" + constraint
}

// mysqlSchemaKeyColumns는 스키마의 키 컬럼 행을 (테이블, 제약)마다 ordinal_position 순으로 모은다. 스키마 이름이
// 정확히 같은 행만 쓴다. MariaDB는 information_schema 이름을 대소문자 무시로 비교하므로 대소문자만 다른 스키마의 행이
// 섞여 올 수 있다. 끝까지 읽었는지도 돌려준다.
func (h *harvester) mysqlSchemaKeyColumns(schema string) (map[string][]mysqlKeyColumn, bool) {
	keys := map[string][]mysqlKeyColumn{}
	complete := h.mysqlRows("constraints", `
		SELECT CAST(table_schema AS CHAR), CAST(table_name AS CHAR), CAST(constraint_name AS CHAR),
		       CAST(column_name AS CHAR), CAST(referenced_table_schema AS CHAR),
		       CAST(referenced_table_name AS CHAR), CAST(referenced_column_name AS CHAR)
		FROM information_schema.key_column_usage
		WHERE table_schema = ?
		ORDER BY table_name, constraint_name, ordinal_position`, []any{schema}, func(rows *sql.Rows) error {
		var owner, table, constraint string
		var key mysqlKeyColumn
		if err := rows.Scan(&owner, &table, &constraint, &key.column, &key.refSchema, &key.refTable, &key.refColumn); err != nil {
			return err
		}
		if owner == schema {
			keys[mysqlKeyColumnOwner(table, constraint)] = append(keys[mysqlKeyColumnOwner(table, constraint)], key)
		}
		return nil
	})
	return keys, complete
}

// addMySQLKeyColumn은 제약에 키 컬럼 하나와, FK면 대상 테이블·컬럼을 덧붙인다. CHECK 제약은 키 컬럼 행이 없다.
func addMySQLKeyColumn(constraint *ConstraintDoc, kind string, key mysqlKeyColumn) {
	if key.column.Valid {
		constraint.Columns = append(constraint.Columns, key.column.String)
	}
	if kind != "FOREIGN KEY" || !key.refTable.Valid {
		return
	}
	if constraint.Referenced == nil {
		constraint.Referenced = &ReferencedDoc{Table: key.refTable.String, Columns: []string{}}
		if key.refSchema.Valid {
			v := key.refSchema.String
			constraint.Referenced.Schema = &v
		}
	}
	if key.refColumn.Valid {
		constraint.Referenced.Columns = append(constraint.Referenced.Columns, key.refColumn.String)
	}
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

// mysqlSchemaIndexes는 PRIMARY가 아닌 인덱스를 스키마마다 한 번 읽어 테이블 이름별로 나눈다. 테이블 안의 순서는
// 테이블마다 읽던 때와 같은 (인덱스 이름, 키 순서)다. 이름 없는 키 컬럼(MySQL functional index)은 컬럼에 넣지 않고
// 정의를 불완전으로 표시하며, 그 수를 테이블마다 돌려준다 — limitation은 객체 목록에 있는 테이블에만 남긴다.
// 끝까지 읽지 못하면 모든 테이블의 인덱스와 그 수를 비운다(일부 인덱스·키 컬럼만 남지 않게).
func (h *harvester) mysqlSchemaIndexes(schema string) (map[string][]IndexDoc, map[string]int) {
	groups := newRelationGroups[IndexDoc]()
	expressionColumns := map[string]int{}
	complete := h.mysqlRows("indexes", `
		SELECT CAST(table_name AS CHAR), CAST(index_name AS CHAR), CAST(non_unique AS SIGNED), CAST(column_name AS CHAR)
		FROM information_schema.statistics
		WHERE table_schema = ? AND index_name <> 'PRIMARY'
		ORDER BY table_name, index_name, seq_in_index`, []any{schema}, func(rows *sql.Rows) error {
		var table, name string
		var nonUnique int64
		var column sql.NullString
		if err := rows.Scan(&table, &name, &nonUnique, &column); err != nil {
			return err
		}
		idx := groups.entry(table, name, func() IndexDoc {
			return IndexDoc{Name: name, Unique: nonUnique == 0, Columns: []string{}, DefinitionComplete: boolValue(true), HasPredicate: boolValue(false)}
		})
		if column.Valid {
			idx.Columns = append(idx.Columns, column.String)
		} else {
			expressionColumns[table]++
			idx.DefinitionComplete = boolValue(false)
		}
		return nil
	})
	if !complete {
		return map[string][]IndexDoc{}, map[string]int{}
	}
	return groups.lists(), expressionColumns
}

// mysqlSchemaTriggers는 트리거를 스키마마다 한 번 읽어 대상 테이블 이름별로 나눈다. MySQL 트리거는 대상 테이블과 같은
// 스키마에만 있으므로 trigger_schema로 고른다(테이블마다 읽던 때와 같은 조건). 끝까지 읽지 못하면 모든 트리거를 비운다.
func (h *harvester) mysqlSchemaTriggers(schema string) map[string][]TriggerDoc {
	triggers := map[string][]TriggerDoc{}
	complete := h.mysqlRows("triggers", `
		SELECT CAST(event_object_table AS CHAR), CAST(trigger_name AS CHAR), CAST(action_statement AS CHAR)
		FROM information_schema.triggers WHERE trigger_schema = ?
		ORDER BY event_object_table, trigger_name`, []any{schema}, func(rows *sql.Rows) error {
		var table, name string
		var body sql.NullString
		if err := rows.Scan(&table, &name, &body); err != nil {
			return err
		}
		triggers[table] = append(triggers[table], TriggerDoc{Name: name, Body: ns(body)})
		return nil
	})
	if !complete {
		return map[string][]TriggerDoc{}
	}
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
	names := []string{}
	for _, item := range raw {
		if item.kind.Valid {
			names = append(names, item.name)
		}
	}
	parameters := h.mysqlRoutineParameters(schema, names)
	for _, item := range raw {
		if !item.kind.Valid {
			h.limitations = append(h.limitations, fmt.Sprintf("%s.%s: ROUTINE_TYPE is NULL; routine kind cannot be determined", schema, item.name))
			continue
		}
		if !item.body.Valid {
			nullBodies++
		}
		signature := parameters.signature(h, schema, item.name)
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

// mysqlParameterBatch는 스키마 routine의 파라미터 형을 한 번 읽은 결과다.
//
// routine마다 specific_name = ?로 물으면 서버가 routine 이름 콜레이션(대소문자·악센트 무시)으로 비교해, 이름이
// 대소문자만 다른 function과 procedure가 서로의 파라미터를 함께 받았다. 이름이 같은 function과 procedure의 행은
// ordinal_position이 같으면 순서가 정해지지 않는다. 정확한 이름으로 묶으면 이런 이름에서만 결과가 달라질 수 있으므로,
// 그런 이름은 예전처럼 routine마다 따로 묻는다(mysqlRoutineSignature). 나머지 routine은 묶음에서 찾는다. 네이티브
// reader도 routine마다 같은 질의를 보내므로, 이렇게 해야 두 생산자의 시그니처가 계속 같다.
type mysqlParameterBatch struct {
	// types는 정확한 specific_name마다 ordinal_position 순으로 모은 dtd_identifier다(NULL은 빼고, 예전과 같다).
	types map[string][]string
	// separate는 예전처럼 따로 물을 routine 이름이다. everyName이면 모든 routine을 따로 묻는다.
	separate  map[string]bool
	everyName bool
	// failure는 묶음 조회가 실패했을 때 routine마다 남길 예전 limitation의 꼬리다("routine parameters unavailable — …").
	failure string
}

// mysqlRoutineParameters는 routine names의 파라미터 형을 스키마마다 한 번 읽는다. routine이 없으면 묻지 않는다 —
// 예전에도 질의가 없었다.
func (h *harvester) mysqlRoutineParameters(schema string, names []string) mysqlParameterBatch {
	batch := mysqlParameterBatch{types: map[string][]string{}}
	if len(names) == 0 {
		return batch
	}
	rows, err := h.db.Query(`
		SELECT CAST(specific_name AS CHAR), CAST(dtd_identifier AS CHAR)
		FROM information_schema.parameters
		WHERE specific_schema = ? AND ordinal_position > 0
		ORDER BY specific_name, ordinal_position`, schema)
	if err != nil {
		batch.failure = "routine parameters unavailable — " + oneLine(err.Error())
		return batch
	}
	defer rows.Close()
	specificNames := map[string]bool{}
	for rows.Next() {
		var name string
		var typ sql.NullString
		if err := rows.Scan(&name, &typ); err != nil {
			batch.failure = "routine parameter row read failed — " + oneLine(err.Error())
			return batch
		}
		specificNames[name] = true
		if typ.Valid {
			batch.types[name] = append(batch.types[name], typ.String)
		}
	}
	if err := rows.Err(); err != nil {
		batch.failure = "routine parameters unavailable — " + oneLine(err.Error())
		return batch
	}
	batch.separate, batch.everyName = mysqlCollationSensitiveNames(names, specificNames)
	return batch
}

// signature는 routine name의 파라미터 형을 쉼표로 이은 시그니처다. 파라미터가 없으면 nil이다. 묶음 조회가 실패했으면
// 예전에 routine마다 실패했을 때와 같은 limitation을 남긴다.
func (b mysqlParameterBatch) signature(h *harvester, schema, name string) *string {
	if b.failure != "" {
		h.limitations = append(h.limitations, fmt.Sprintf("%s.%s: %s", schema, name, b.failure))
		return nil
	}
	if b.everyName || b.separate[name] {
		return h.mysqlRoutineSignature(schema, name)
	}
	return joinedSignature(b.types[name])
}

// mysqlCollationSensitiveNames는 정확한 이름으로 묶으면 예전 콜레이션 비교와 결과가 달라질 수 있는 routine 이름을 고른다.
// 출력 가능한 ASCII 이름끼리의 콜레이션 비교는 대소문자 무시 비교와 같다. 그래서 대소문자 무시로 겹치는 이름이 routine
// 둘 이상이거나 철자가 둘 이상이면 따로 묻는다. 그 밖의 이름(ASCII가 아니거나 공백으로 끝나는 이름)이 하나라도 있으면
// 악센트 무시·끝 공백 무시 규칙을 옮길 수 없으므로 모든 routine을 따로 묻는다.
func mysqlCollationSensitiveNames(routines []string, specificNames map[string]bool) (map[string]bool, bool) {
	routineCount := map[string]int{}
	spellings := map[string]map[string]bool{}
	add := func(name string) bool {
		if !plainASCIIName(name) {
			return false
		}
		folded := strings.ToLower(name)
		if spellings[folded] == nil {
			spellings[folded] = map[string]bool{}
		}
		spellings[folded][name] = true
		return true
	}
	for _, name := range routines {
		if !add(name) {
			return nil, true
		}
		routineCount[strings.ToLower(name)]++
	}
	for name := range specificNames {
		if !add(name) {
			return nil, true
		}
	}
	separate := map[string]bool{}
	for _, name := range routines {
		folded := strings.ToLower(name)
		if routineCount[folded] > 1 || len(spellings[folded]) > 1 {
			separate[name] = true
		}
	}
	return separate, false
}

// plainASCIIName은 이름이 출력 가능한 ASCII(공백~물결표)로만 되어 있고 공백으로 끝나지 않는지 본다.
func plainASCIIName(name string) bool {
	if name == "" || name[len(name)-1] == ' ' {
		return false
	}
	for i := 0; i < len(name); i++ {
		if name[i] < ' ' || name[i] > '~' {
			return false
		}
	}
	return true
}

// joinedSignature는 파라미터 형을 쉼표로 잇는다. 형이 하나도 없으면 시그니처가 없다(nil).
func joinedSignature(types []string) *string {
	if len(types) == 0 {
		return nil
	}
	signature := strings.Join(types, ",")
	return &signature
}

// mysqlRoutineSignature는 routine 하나의 파라미터 형을 따로 묻는다. 비교는 서버 콜레이션을 따른다.
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
	return joinedSignature(types)
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
		positions := objectPositions(objects)
		for tableRows.Next() {
			var table string
			var reads, writes int64
			if err := tableRows.Scan(&table, &reads, &writes); err != nil {
				h.limitations = append(h.limitations, fmt.Sprintf("%s: sys.schema_table_statistics row read failed — %s", schema, oneLine(err.Error())))
				break
			}
			for _, i := range positions[table] {
				objects[i].Usage = &UsageDoc{Since: ns(since), Reads: maxInt64(reads), Writes: maxInt64(writes)}
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
	indexes := indexPositions(objects)
	for rows.Next() {
		var table, index string
		if err := rows.Scan(&table, &index); err != nil {
			h.limitations = append(h.limitations, fmt.Sprintf("%s: sys.schema_unused_indexes row read failed — %s", schema, oneLine(err.Error())))
			return
		}
		for _, position := range indexes[table+"\x00"+index] {
			objects[position.object].Indexes[position.index].Usage = &UsageDoc{Since: ns(since), Reads: 0, Writes: 0}
		}
	}
	if err := rows.Err(); err != nil {
		h.limitations = append(h.limitations, fmt.Sprintf("%s: sys.schema_unused_indexes unavailable — %s", schema, oneLine(err.Error())))
	}
}

// mysqlRows는 metadata query 실패를 빈 성공으로 숨기지 않고 limitation으로
// 기록한다. 끝까지 읽었으면 true다. 스키마 단위 세부 조회(컬럼·PK 위치·제약·키 컬럼·인덱스·트리거)는 false면
// 그때까지 읽은 행도 버리고 그 부분을 스키마의 모든 객체에서 비운다 — 커서가 도중에 끊기면 어느 객체까지
// 채워지는지가 실패 시점에 따라 달라지기 때문이다. 객체 목록·뷰 본문·routine 같은 나머지 호출은 이 값을 보지 않는다.
func (h *harvester) mysqlRows(label, query string, args []any, scan func(*sql.Rows) error) bool {
	rows, err := h.db.Query(query, args...)
	if err != nil {
		h.catalogIncomplete = true
		h.limitations = append(h.limitations, fmt.Sprintf("%s unavailable — %s", label, oneLine(err.Error())))
		return false
	}
	defer rows.Close()
	for rows.Next() {
		if err := scan(rows); err != nil {
			h.catalogIncomplete = true
			h.limitations = append(h.limitations, fmt.Sprintf("%s row read failed — %s", label, oneLine(err.Error())))
			return false
		}
	}
	if err := rows.Err(); err != nil {
		h.catalogIncomplete = true
		h.limitations = append(h.limitations, fmt.Sprintf("%s unavailable — %s", label, oneLine(err.Error())))
		return false
	}
	return true
}
