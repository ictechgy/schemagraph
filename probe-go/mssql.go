// MSSQL 수확 — probe/Extractor.kt의 "sqlserver" 분기와 1:1 대응.
// Kotlin은 객체·컬럼·제약을 JDBC 메타로 긁는데, 여기선 같은 의미를
// sys.* 카탈로그 쿼리로 직접 얻는다(JDBC 메타 대응표):
//   getTables       → sys.objects(type U/V/SN, is_ms_shipped=0)
//   getColumns      → sys.columns + sys.types + sys.default_constraints
//   getPrimaryKeys  → sys.key_constraints(PK) + sys.index_columns
//   getImportedKeys → sys.foreign_keys + sys.foreign_key_columns
//   getIndexInfo    → sys.indexes + sys.index_columns
// 몸체(뷰·트리거·routine)는 Kotlin이 이미 sys.sql_modules로 수확한다 —
// INFORMATION_SCHEMA는 정의를 4000자에서 자르기 때문이다. 같은 쿼리를 쓴다.
// usage는 JVM 프로브도 mssql에서 수확하지 않으므로 여기도 안 한다.

package main

import (
	"database/sql"
	"sort"
	"strings"
)

// extractMSSQL — 스키마 목록은 객체와 routine의 합집합(Extractor.kt와 같은
// 규칙: routine만 있는 스키마도 있다).
func (h *harvester) extractMSSQL() CatalogDocument {
	objects := h.mssqlObjects()
	columns := h.mssqlColumns()
	pkPos, pkCons := h.mssqlPrimaryKeys()
	fks := h.mssqlForeignKeys()
	indexes := h.mssqlIndexes()
	views := h.mssqlViews()
	triggers := h.mssqlTriggers()
	routines := h.mssqlRoutines()

	schemaSet := map[string]bool{}
	for s := range objects {
		schemaSet[s] = true
	}
	for s := range routines {
		schemaSet[s] = true
	}
	schemas := make([]string, 0, len(schemaSet))
	for s := range schemaSet {
		schemas = append(schemas, s)
	}
	sort.Strings(schemas)

	doc := CatalogDocument{
		Version: documentVersion, Dialect: "sqlserver", Reader: "probe-go",
		Schemas:     []SchemaDoc{},
		Limitations: append([]string{}, h.limitations...),
	}
	for _, schema := range schemas {
		sd := SchemaDoc{Name: schema, Objects: []ObjectDoc{}, Routines: []RoutineDoc{}}
		for _, obj := range objects[schema] {
			k := schema + "." + obj.Name
			obj.Columns = columns[k]
			for i := range obj.Columns {
				obj.Columns[i].PkPosition = pkPos[k][obj.Columns[i].Name]
			}
			if obj.Columns == nil {
				obj.Columns = []ColumnDoc{}
			}
			obj.Constraints = append(pkCons[k], fks[k]...)
			if obj.Constraints == nil {
				obj.Constraints = []ConstraintDoc{}
			}
			sort.Slice(obj.Constraints, func(i, j int) bool {
				return obj.Constraints[i].Name < obj.Constraints[j].Name
			})
			// 인덱스는 테이블에만 — 뷰의 getIndexInfo는 방언마다 잡음이 달라
			// Kotlin도 table/materialized-view에만 붙인다.
			if obj.Kind == "table" {
				obj.Indexes = indexes[k]
			}
			if obj.Indexes == nil {
				obj.Indexes = []IndexDoc{}
			}
			if b, ok := views[k]; ok {
				obj.Body = &b
			}
			obj.Triggers = triggers[k]
			if obj.Triggers == nil {
				obj.Triggers = []TriggerDoc{}
			}
			sd.Objects = append(sd.Objects, obj)
		}
		rts := routines[schema]
		sort.Slice(rts, func(i, j int) bool {
			if rts[i].Name != rts[j].Name {
				return rts[i].Name < rts[j].Name
			}
			var a, b string
			if rts[i].Signature != nil {
				a = *rts[i].Signature
			}
			if rts[j].Signature != nil {
				b = *rts[j].Signature
			}
			return a < b
		})
		sd.Routines = rts
		if sd.Routines == nil {
			sd.Routines = []RoutineDoc{}
		}
		doc.Schemas = append(doc.Schemas, sd)
	}
	return doc
}

// mssqlObjects — sys.objects의 사용자 객체. is_ms_shipped=0이 시스템 객체를
// 걸러 JDBC getTables의 TABLE_TYPE 필터와 같은 멤버를 낸다.
func (h *harvester) mssqlObjects() map[string][]ObjectDoc {
	out := map[string][]ObjectDoc{}
	h.bestEffort("objects",
		`SELECT s.name, o.name, o.type
		 FROM sys.objects o JOIN sys.schemas s ON s.schema_id = o.schema_id
		 WHERE o.type IN ('U','V','SN') AND o.is_ms_shipped = 0
		 ORDER BY s.name, o.name`,
		func(rs *sql.Rows) error {
			var schema, name, typ string
			if err := rs.Scan(&schema, &name, &typ); err != nil {
				return err
			}
			if !h.keepSchema(schema) {
				return nil
			}
			kind := "table"
			switch strings.TrimSpace(typ) {
			case "V":
				kind = "view"
			case "SN":
				kind = "synonym"
			}
			out[schema] = append(out[schema], ObjectDoc{Name: name, Kind: kind})
			return nil
		})
	return out
}

// mssqlColumns — column_id가 JDBC ORDINAL_POSITION이다.
// 기본값은 sys.default_constraints.definition(`(0)`처럼 괄호를 포함한 원문)
// — JDBC COLUMN_DEF와 같은 역할이다.
func (h *harvester) mssqlColumns() map[string][]ColumnDoc {
	out := map[string][]ColumnDoc{}
	h.bestEffort("columns",
		`SELECT s.name, o.name, c.name, t.name, c.is_nullable, dc.definition, c.column_id
		 FROM sys.columns c
		 JOIN sys.objects o ON o.object_id = c.object_id
		 JOIN sys.schemas s ON s.schema_id = o.schema_id
		 JOIN sys.types t ON t.user_type_id = c.user_type_id
		 LEFT JOIN sys.default_constraints dc
		   ON dc.parent_object_id = c.object_id AND dc.parent_column_id = c.column_id
		 WHERE o.type IN ('U','V','SN') AND o.is_ms_shipped = 0
		 ORDER BY s.name, o.name, c.column_id`,
		func(rs *sql.Rows) error {
			var schema, obj, name, dtype string
			var nullable bool
			var def sql.NullString
			var ord int
			if err := rs.Scan(&schema, &obj, &name, &dtype, &nullable, &def, &ord); err != nil {
				return err
			}
			k := schema + "." + obj
			out[k] = append(out[k], ColumnDoc{
				Name: name, DataType: dtype, Nullable: nullable,
				Default: ns(def), Ordinal: ord,
			})
			return nil
		})
	return out
}

// mssqlPrimaryKeys — key_ordinal이 JDBC KEY_SEQ다. 반환: (컬럼→pkPosition,
// 객체→pk 제약). 컬럼 순서는 다른 제약과 달리 정점 id에 들어가므로 두 채널을
// 같이 돌려준다(Oracle collectPrimaryKeys와 같은 형태).
func (h *harvester) mssqlPrimaryKeys() (map[string]map[string]int, map[string][]ConstraintDoc) {
	pos := map[string]map[string]int{}
	cons := map[string][]ConstraintDoc{}
	h.bestEffort("primary keys",
		`SELECT s.name, o.name, kc.name, c.name, ic.key_ordinal
		 FROM sys.key_constraints kc
		 JOIN sys.objects o ON o.object_id = kc.parent_object_id
		 JOIN sys.schemas s ON s.schema_id = o.schema_id
		 JOIN sys.index_columns ic
		   ON ic.object_id = kc.parent_object_id AND ic.index_id = kc.unique_index_id
		 JOIN sys.columns c ON c.object_id = ic.object_id AND c.column_id = ic.column_id
		 WHERE kc.type = 'PK' AND o.is_ms_shipped = 0
		 ORDER BY s.name, o.name, ic.key_ordinal`,
		func(rs *sql.Rows) error {
			var schema, obj, name, col string
			var ord int
			if err := rs.Scan(&schema, &obj, &name, &col, &ord); err != nil {
				return err
			}
			k := schema + "." + obj
			if pos[k] == nil {
				pos[k] = map[string]int{}
			}
			pos[k][col] = ord
			// 제약은 첫 컬럼 행에서 한 번만 만든다 — 컬럼은 누적한다.
			n := len(cons[k])
			if n == 0 || cons[k][n-1].Name != name {
				cons[k] = append(cons[k], ConstraintDoc{Name: name, Kind: "pk"})
			}
			cons[k][len(cons[k])-1].Columns = append(cons[k][len(cons[k])-1].Columns, col)
			return nil
		})
	return pos, cons
}

// mssqlForeignKeys — constraint_column_id가 JDBC KEY_SEQ다. 참조 스키마는
// referenced_object_id의 스키마라 크로스 스키마 FK도 원래 소유자를 가리킨다.
func (h *harvester) mssqlForeignKeys() map[string][]ConstraintDoc {
	out := map[string][]ConstraintDoc{}
	h.bestEffort("foreign keys",
		`SELECT s.name, o.name, fk.name, fc.name, rs.name, ro.name, rc.name,
		        fkc.constraint_column_id
		 FROM sys.foreign_keys fk
		 JOIN sys.foreign_key_columns fkc ON fkc.constraint_object_id = fk.object_id
		 JOIN sys.objects o ON o.object_id = fk.parent_object_id
		 JOIN sys.schemas s ON s.schema_id = o.schema_id
		 JOIN sys.columns fc
		   ON fc.object_id = fkc.parent_object_id AND fc.column_id = fkc.parent_column_id
		 JOIN sys.objects ro ON ro.object_id = fkc.referenced_object_id
		 JOIN sys.schemas rs ON rs.schema_id = ro.schema_id
		 JOIN sys.columns rc
		   ON rc.object_id = fkc.referenced_object_id AND rc.column_id = fkc.referenced_column_id
		 WHERE o.is_ms_shipped = 0
		 ORDER BY s.name, o.name, fk.name, fkc.constraint_column_id`,
		func(rs *sql.Rows) error {
			var schema, obj, name, col, rschema, rtable, rcol string
			var ord int
			if err := rs.Scan(&schema, &obj, &name, &col, &rschema, &rtable, &rcol, &ord); err != nil {
				return err
			}
			k := schema + "." + obj
			n := len(out[k])
			if n == 0 || out[k][n-1].Name != name {
				out[k] = append(out[k], ConstraintDoc{
					Name: name, Kind: "fk",
					Referenced: &ReferencedDoc{Schema: &rschema, Table: rtable},
				})
			}
			c := &out[k][len(out[k])-1]
			c.Columns = append(c.Columns, col)
			c.Referenced.Columns = append(c.Referenced.Columns, rcol)
			return nil
		})
	return out
}

// mssqlIndexes — key_ordinal이 JDBC ORDINAL_POSITION이다. type>0은 힙을
// 빼고(JDBC의 tableIndexStatistic 스킵과 같은 역할), is_included_column=0은
// INCLUDE 컬럼을 뺀다 — JDBC getIndexInfo가 돌려주는 건 키 컬럼뿐이다.
func (h *harvester) mssqlIndexes() map[string][]IndexDoc {
	out := map[string][]IndexDoc{}
	h.bestEffort("indexes",
		`SELECT s.name, o.name, i.name, i.is_unique, c.name, ic.key_ordinal
		 FROM sys.indexes i
		 JOIN sys.objects o ON o.object_id = i.object_id
		 JOIN sys.schemas s ON s.schema_id = o.schema_id
		 JOIN sys.index_columns ic ON ic.object_id = i.object_id AND ic.index_id = i.index_id
		 JOIN sys.columns c ON c.object_id = ic.object_id AND c.column_id = ic.column_id
		 WHERE i.type > 0 AND i.is_hypothetical = 0 AND i.name IS NOT NULL
		   AND ic.is_included_column = 0 AND o.is_ms_shipped = 0
		 ORDER BY s.name, o.name, i.name, ic.key_ordinal`,
		func(rs *sql.Rows) error {
			var schema, obj, name, col string
			var uniq bool
			var ord int
			if err := rs.Scan(&schema, &obj, &name, &uniq, &col, &ord); err != nil {
				return err
			}
			k := schema + "." + obj
			n := len(out[k])
			if n == 0 || out[k][n-1].Name != name {
				out[k] = append(out[k], IndexDoc{Name: name, Unique: uniq})
			}
			out[k][len(out[k])-1].Columns = append(out[k][len(out[k])-1].Columns, col)
			return nil
		})
	return out
}

// mssqlViews — sys.sql_modules.definition이 nvarchar(max)라 온전한 원문이다
// (INFORMATION_SCHEMA.VIEWS는 4000자에서 잘린다 — Kotlin과 같은 이유).
func (h *harvester) mssqlViews() map[string]string {
	out := map[string]string{}
	h.bestEffort("views",
		`SELECT SCHEMA_NAME(o.schema_id), o.name, m.definition
		 FROM sys.views o
		 JOIN sys.sql_modules m ON m.object_id = o.object_id`,
		func(rs *sql.Rows) error {
			var schema, name, def string
			if err := rs.Scan(&schema, &name, &def); err != nil {
				return err
			}
			out[schema+"."+name] = def
			return nil
		})
	return out
}

// mssqlTriggers — parent_class=1은 테이블/뷰 트리거만(0은 DDL 트리거).
func (h *harvester) mssqlTriggers() map[string][]TriggerDoc {
	out := map[string][]TriggerDoc{}
	h.bestEffort("triggers",
		`SELECT SCHEMA_NAME(p.schema_id), p.name, t.name, m.definition
		 FROM sys.triggers t
		 JOIN sys.objects p ON p.object_id = t.parent_id
		 JOIN sys.sql_modules m ON m.object_id = t.object_id
		 WHERE t.parent_class = 1`,
		func(rs *sql.Rows) error {
			var schema, obj, name, def string
			if err := rs.Scan(&schema, &obj, &name, &def); err != nil {
				return err
			}
			k := schema + "." + obj
			out[k] = append(out[k], TriggerDoc{Name: name, Body: &def})
			return nil
		})
	return out
}

// mssqlRoutines — sys.objects의 P/FN/IF/TF가 routine이다. kind는 type이
// 'P'면 procedure, 나머지(FN/IF/TF)는 function — Kotlin과 같은 매핑이다.
// language는 "sql" — 엔진이 MsSqlDialect로 파싱한다.
func (h *harvester) mssqlRoutines() map[string][]RoutineDoc {
	bodies := map[string][]RoutineDoc{}
	h.bestEffort("routines",
		`SELECT SCHEMA_NAME(o.schema_id), o.name, o.type, m.definition
		 FROM sys.objects o
		 JOIN sys.sql_modules m ON m.object_id = o.object_id
		 WHERE o.type IN ('P','FN','IF','TF')`,
		func(rs *sql.Rows) error {
			var schema, name, typ, def string
			if err := rs.Scan(&schema, &name, &typ, &def); err != nil {
				return err
			}
			if !h.keepSchema(schema) {
				return nil
			}
			lang := "sql"
			kind := "function"
			if strings.TrimSpace(typ) == "P" {
				kind = "procedure"
			}
			bodies[schema] = append(bodies[schema], RoutineDoc{
				Name: name, Kind: kind, Language: &lang, Body: &def,
			})
			return nil
		})

	params := map[string]string{}
	h.bestEffort("routine parameters",
		`SELECT SCHEMA_NAME(o.schema_id), o.name, TYPE_NAME(p.user_type_id)
		 FROM sys.parameters p
		 JOIN sys.objects o ON o.object_id = p.object_id
		 WHERE o.type IN ('P','FN','IF','TF') AND p.parameter_id > 0
		 ORDER BY o.name, p.parameter_id`,
		func(rs *sql.Rows) error {
			var schema, name, dtype string
			if err := rs.Scan(&schema, &name, &dtype); err != nil {
				return err
			}
			k := schema + "." + name
			if cur := params[k]; cur != "" {
				params[k] = cur + ", " + strings.ToLower(dtype)
			} else {
				params[k] = strings.ToLower(dtype)
			}
			return nil
		})

	for schema, rts := range bodies {
		for i := range rts {
			if s := params[schema+"."+rts[i].Name]; s != "" {
				rts[i].Signature = &s
			}
		}
	}
	return bodies
}
