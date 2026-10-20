package main

import (
	"bytes"
	"database/sql/driver"
	"encoding/json"
	"fmt"
	"reflect"
	"slices"
	"strings"
	"testing"
)

// mysqlTablesReply는 app 스키마에 테이블·procedure가 count개씩 보이고 procedure마다 파라미터가 하나인 MySQL이다.
// 나머지 조회는 빈 결과다.
func mysqlTablesReply(count int) func(string, []driver.NamedValue) scriptedReply {
	return func(query string, _ []driver.NamedValue) scriptedReply {
		switch {
		case strings.Contains(query, "DATABASE()"), strings.Contains(query, "information_schema.schemata"):
			return scriptedRows([]driver.Value{"app"})
		case strings.Contains(query, "FROM information_schema.tables"):
			rows := make([][]driver.Value, 0, count)
			for i := range count {
				rows = append(rows, []driver.Value{fmt.Sprintf("t%03d", i), "BASE TABLE"})
			}
			return scriptedRows(rows...)
		case strings.Contains(query, "FROM information_schema.routines"):
			rows := make([][]driver.Value, 0, count)
			for i := range count {
				rows = append(rows, []driver.Value{fmt.Sprintf("p%03d", i), "PROCEDURE", "SELECT 1"})
			}
			return scriptedRows(rows...)
		case strings.Contains(query, "FROM information_schema.parameters"):
			rows := make([][]driver.Value, 0, count)
			for i := range count {
				rows = append(rows, []driver.Value{fmt.Sprintf("p%03d", i), "int"})
			}
			return scriptedRows(rows...)
		}
		return scriptedReply{}
	}
}

// mysqlQueryCount는 테이블·procedure가 count개씩인 스키마를 JSON과 NDJSON으로 한 번씩 수집해 형식별 질의 수를 센다.
func mysqlQueryCount(t *testing.T, count int) map[string]int {
	t.Helper()
	counts := map[string]int{}
	for _, format := range []string{"json", "ndjson"} {
		db, server := newScriptedDB(mysqlTablesReply(count))
		h := &harvester{db: db, dialect: "mysql", schemaFilter: map[string]bool{"app": true}}
		if format == "json" {
			doc := h.extract()
			if len(doc.Schemas) != 1 || len(doc.Schemas[0].Objects) != count || len(doc.Schemas[0].Routines) != count {
				t.Fatalf("json collected %d schemas, want one with %d objects and routines", len(doc.Schemas), count)
			}
		} else if err := h.streamNDJSON(&bytes.Buffer{}, 2); err != nil {
			t.Fatal(err)
		}
		counts[format] = len(server.recorded())
		closeScriptedDB(t, db)
	}
	return counts
}

// 객체마다 컬럼·PK 위치·제약·인덱스·트리거를, routine마다 파라미터를 물으면 고정 연결 하나에서 왕복이 객체 수에
// 비례한다(go-sql-driver/mysql은 인자 있는 질의마다 prepare·execute 두 번 왕복이다). 스키마당 질의 수는 객체·routine
// 수와 무관해야 한다.
func TestMySQLQueriesPerSchemaDoNotGrowWithObjectCount(t *testing.T) {
	small, large := mysqlQueryCount(t, 1), mysqlQueryCount(t, 40)
	if !reflect.DeepEqual(small, large) {
		t.Fatalf("queries per schema grew with the object count: 1 object %v, 40 objects %v", small, large)
	}
}

// mysqlCatalogReply는 스키마 단위 조회가 여러 테이블의 행을 섞어 돌려주는 MySQL이다. Orders와 orders는 대소문자만
// 다른 별개 테이블이고(lower_case_table_names=0), ghost는 객체 목록에 없는 테이블이다. 키 컬럼 조회에는 MariaDB처럼
// 대소문자만 다른 스키마 APP의 행과 대소문자만 다른 제약 이름의 행이 섞여 온다.
func mysqlCatalogReply(query string, _ []driver.NamedValue) scriptedReply {
	switch {
	case strings.Contains(query, "information_schema.schemata"):
		return scriptedRows([]driver.Value{"app"})
	case strings.Contains(query, "FROM information_schema.tables"):
		return scriptedRows([]driver.Value{"Orders", "BASE TABLE"}, []driver.Value{"order_totals", "VIEW"},
			[]driver.Value{"orders", "BASE TABLE"}, []driver.Value{"orders_seq", "SEQUENCE"})
	case strings.Contains(query, "FROM information_schema.key_column_usage") && strings.Contains(query, "'PRIMARY'"):
		return scriptedRows([]driver.Value{"Orders", "ID", int64(1)}, []driver.Value{"ghost", "id", int64(1)},
			[]driver.Value{"orders", "region", int64(1)}, []driver.Value{"orders", "id", int64(2)})
	case strings.Contains(query, "FROM information_schema.columns"):
		return scriptedRows(
			[]driver.Value{"Orders", "ID", "int", "NO", nil, int64(1)},
			[]driver.Value{"ghost", "id", "int", "NO", nil, int64(1)},
			[]driver.Value{"order_totals", "total", "decimal", "YES", nil, int64(1)},
			[]driver.Value{"orders", "id", "bigint", "NO", nil, int64(1)},
			[]driver.Value{"orders", "region", "varchar", "NO", "'eu'", int64(2)},
			[]driver.Value{"orders", "customer_id", "bigint", "YES", nil, int64(3)},
		)
	case strings.Contains(query, "FROM information_schema.key_column_usage"):
		// 키 컬럼 행의 스키마·테이블·제약 이름, 키 컬럼, FK 대상 스키마·테이블·컬럼 순이다(table_name, constraint_name,
		// ordinal_position 순이며, 대소문자 무시 정렬이라 Orders와 orders의 행이 번갈아 온다).
		return scriptedRows(
			[]driver.Value{"app", "ghost", "PRIMARY", "id", nil, nil, nil},
			[]driver.Value{"app", "Orders", "PRIMARY", "ID", nil, nil, nil},
			[]driver.Value{"app", "orders", "PRIMARY", "region", nil, nil, nil},
			[]driver.Value{"APP", "orders", "PRIMARY", "zone", nil, nil, nil},
			[]driver.Value{"app", "orders", "PRIMARY", "id", nil, nil, nil},
			[]driver.Value{"app", "orders", "orders_pair_fk", "region", "app", "Orders", "ID"},
			[]driver.Value{"app", "orders", "ORDERS_PAIR_FK", "zone", "app", "Orders", "ZONE"},
			[]driver.Value{"app", "orders", "orders_pair_fk", "customer_id", "app", "Orders", "CUSTOMER"},
			[]driver.Value{"app", "orders", "orders_uq", "id", nil, nil, nil},
		)
	case strings.Contains(query, "FROM information_schema.table_constraints"):
		// 제약 테이블·이름·종류 순이다. 이름이 NULL인 행은 버린다.
		return scriptedRows(
			[]driver.Value{"ghost", "PRIMARY", "PRIMARY KEY"},
			[]driver.Value{"Orders", "PRIMARY", "PRIMARY KEY"},
			[]driver.Value{"orders", "PRIMARY", "PRIMARY KEY"},
			[]driver.Value{"orders", "orders_check", "CHECK"},
			[]driver.Value{"orders", "orders_pair_fk", "FOREIGN KEY"},
			[]driver.Value{"orders", nil, "UNIQUE"},
		)
	case strings.Contains(query, "FROM information_schema.statistics"):
		return scriptedRows(
			[]driver.Value{"ghost", "ghost_expr", int64(1), nil},
			[]driver.Value{"orders", "orders_customer_idx", int64(1), "customer_id"},
			[]driver.Value{"Orders", "Orders_u", int64(0), "ID"},
			[]driver.Value{"orders", "orders_expr", int64(1), nil},
			[]driver.Value{"orders", "orders_expr", int64(1), "region"},
			[]driver.Value{"orders", "orders_expr", int64(1), nil},
		)
	case strings.Contains(query, "FROM information_schema.triggers"):
		return scriptedRows([]driver.Value{"orders", "orders_audit", "BEGIN END"}, []driver.Value{"ghost", "ghost_audit", nil})
	}
	return scriptedReply{}
}

// mysqlObjectsByName은 객체를 이름으로 찾게 한다.
func mysqlObjectsByName(objects []ObjectDoc) map[string]ObjectDoc {
	out := map[string]ObjectDoc{}
	for _, object := range objects {
		out[object.Name] = object
	}
	return out
}

// 스키마 단위 조회의 행은 정확한 테이블 이름으로 나뉜다. 테이블 안의 키 순서는 테이블마다 읽던 때와 같고, 행이 없는
// 객체(뷰의 제약, 시퀀스)도 빈 배열을 낸다. 제약 조인이 이름의 대소문자만 다른 테이블·스키마·제약의 키 컬럼을 짝지어도
// 그 컬럼은 제약에 넣지 않는다. 이름 없는 키 컬럼 limitation은 객체 목록에 있는 테이블에만 한 번 남는다.
func TestMySQLSchemaRowsAreSplitByExactTableName(t *testing.T) {
	db, _ := newScriptedDB(mysqlCatalogReply)
	defer closeScriptedDB(t, db)
	h := &harvester{db: db, dialect: "mysql", schemaFilter: map[string]bool{"app": true}}
	objects := h.mysqlObjects("app")
	h.mysqlRelationDetails("app", objects)
	want := map[string]ObjectDoc{
		"Orders": {
			Name: "Orders", Kind: "table",
			Columns:     []ColumnDoc{{Name: "ID", DataType: "int", Ordinal: 1, PkPosition: 1}},
			Constraints: []ConstraintDoc{{Name: "PRIMARY", Kind: "pk", Columns: []string{"ID"}}},
			Indexes: []IndexDoc{{Name: "Orders_u", Unique: true, Columns: []string{"ID"},
				DefinitionComplete: boolValue(true), HasPredicate: boolValue(false)}},
			Triggers: []TriggerDoc{},
		},
		"order_totals": {
			Name: "order_totals", Kind: "view",
			Columns:     []ColumnDoc{{Name: "total", DataType: "decimal", Nullable: true, Ordinal: 1}},
			Constraints: []ConstraintDoc{}, Indexes: []IndexDoc{}, Triggers: []TriggerDoc{},
		},
		"orders": {
			Name: "orders", Kind: "table",
			Columns: []ColumnDoc{
				{Name: "id", DataType: "bigint", Ordinal: 1, PkPosition: 2},
				{Name: "region", DataType: "varchar", Default: stringPtr("'eu'"), Ordinal: 2, PkPosition: 1},
				{Name: "customer_id", DataType: "bigint", Nullable: true, Ordinal: 3},
			},
			Constraints: []ConstraintDoc{
				{Name: "PRIMARY", Kind: "pk", Columns: []string{"region", "id"}},
				{Name: "orders_check", Kind: "check", Columns: []string{}},
				{Name: "orders_pair_fk", Kind: "fk", Columns: []string{"region", "customer_id"},
					Referenced: &ReferencedDoc{Schema: stringPtr("app"), Table: "Orders", Columns: []string{"ID", "CUSTOMER"}}},
			},
			Indexes: []IndexDoc{
				{Name: "orders_customer_idx", Columns: []string{"customer_id"}, DefinitionComplete: boolValue(true), HasPredicate: boolValue(false)},
				{Name: "orders_expr", Columns: []string{"region"}, DefinitionComplete: boolValue(false), HasPredicate: boolValue(false)},
			},
			Triggers: []TriggerDoc{{Name: "orders_audit", Body: stringPtr("BEGIN END")}},
		},
		"orders_seq": {
			Name: "orders_seq", Kind: "sequence",
			Columns: []ColumnDoc{}, Constraints: []ConstraintDoc{}, Indexes: []IndexDoc{}, Triggers: []TriggerDoc{},
		},
	}
	if got := mysqlObjectsByName(objects); !reflect.DeepEqual(got, want) {
		encoded, err := json.MarshalIndent(got, "", " ")
		if err != nil {
			t.Fatalf("split objects differ and cannot be shown: %v", err)
		}
		t.Fatalf("split objects differ:\n%s", encoded)
	}
	wantLimitations := []string{"app.orders: omitted 2 functional index columns because they have no names"}
	if !slices.Equal(h.limitations, wantLimitations) || h.catalogIncomplete {
		t.Fatalf("limitations = %q (incomplete %v), want %q", h.limitations, h.catalogIncomplete, wantLimitations)
	}
	sequence, err := json.Marshal(mysqlObjectsByName(objects)["orders_seq"])
	if err != nil {
		t.Fatal(err)
	}
	if !bytes.Contains(sequence, []byte(`"columns":[],"constraints":[],"indexes":[],"triggers":[]`)) {
		t.Fatalf("an object without rows must encode empty arrays, not null: %s", sequence)
	}
}

// isMySQLKeyColumnQuery는 제약의 키 컬럼을 읽는 질의인지 본다(PK 위치 질의도 key_column_usage를 읽는다).
func isMySQLKeyColumnQuery(query string) bool {
	return strings.Contains(query, "FROM information_schema.key_column_usage") && !strings.Contains(query, "'PRIMARY'")
}

// 스키마 단위 조회 하나가 실패하면 limitation은 테이블마다가 아니라 스키마에 한 번 남고 미완전으로 표시된다. 문구는
// 테이블마다 읽던 때와 같다(그때도 테이블 이름 없이 남아 문서에서 하나로 합쳐졌다). 제약은 제약 행과 키 컬럼 행을
// 따로 읽는데, 어느 쪽이 실패해도 문구는 하나("constraints …")이고, 키 컬럼 일부만 가진 제약은 내지 않는다.
func TestMySQLSchemaBatchFailureIsReportedOncePerSchema(t *testing.T) {
	failure := fmt.Errorf("injected constraint failure")
	tests := []struct {
		name  string
		reply func(string, []driver.NamedValue) scriptedReply
	}{
		{name: "constraint rows", reply: func(query string, args []driver.NamedValue) scriptedReply {
			if strings.Contains(query, "FROM information_schema.table_constraints") {
				return scriptedReply{err: failure}
			}
			return mysqlCatalogReply(query, args)
		}},
		{name: "key column rows", reply: func(query string, args []driver.NamedValue) scriptedReply {
			if isMySQLKeyColumnQuery(query) {
				return scriptedReply{err: failure}
			}
			return mysqlCatalogReply(query, args)
		}},
		{name: "key column cursor", reply: func(query string, args []driver.NamedValue) scriptedReply {
			reply := mysqlCatalogReply(query, args)
			if isMySQLKeyColumnQuery(query) {
				reply.rowsErr = failure
			}
			return reply
		}},
	}
	for _, test := range tests {
		t.Run(test.name, func(t *testing.T) {
			db, _ := newScriptedDB(test.reply)
			defer closeScriptedDB(t, db)
			h := &harvester{db: db, dialect: "mysql", schemaFilter: map[string]bool{"app": true}}
			objects := h.mysqlObjects("app")
			h.mysqlRelationDetails("app", objects)
			failures := 0
			for _, limitation := range h.limitations {
				if limitation == "constraints unavailable — injected constraint failure" {
					failures++
				}
			}
			if failures != 1 || !h.catalogIncomplete {
				t.Fatalf("constraint failure must be reported once and mark the catalog incomplete: %q", h.limitations)
			}
			for _, object := range objects {
				if object.Constraints == nil || len(object.Constraints) != 0 {
					t.Fatalf("%s: constraints = %#v, want an empty slice", object.Name, object.Constraints)
				}
			}
			if columns := mysqlObjectsByName(objects)["orders"].Columns; len(columns) != 3 {
				t.Fatalf("other batch queries must still fill the schema: %#v", columns)
			}
		})
	}
}

// 제약 행과 키 컬럼 행은 서버에서 잇지 않는다. MariaDB 11.4는 두 information_schema 표의 JOIN을 인덱스 없는 블록
// 중첩 반복으로 실행해, 그 질의 하나가 테이블 수의 제곱으로 느려졌다(4,000테이블에서 약 4초).
func TestMySQLCatalogQueriesDoNotJoinInformationSchemaTables(t *testing.T) {
	db, server := newScriptedDB(mysqlTablesReply(3))
	defer closeScriptedDB(t, db)
	h := &harvester{db: db, dialect: "mysql", schemaFilter: map[string]bool{"app": true}}
	h.extract()
	for _, call := range server.recorded() {
		if strings.Count(call.query, "information_schema.") > 1 {
			t.Fatalf("a catalog query joins information_schema tables on the server:\n%s", call.query)
		}
	}
}

// 객체가 없는 스키마에는 테이블 세부 조회를 보내지 않는다(예전에도 객체마다만 물었다).
func TestMySQLSchemaWithoutObjectsSendsNoDetailQueries(t *testing.T) {
	db, server := newScriptedDB(nil)
	defer closeScriptedDB(t, db)
	h := &harvester{db: db, dialect: "mysql", schemaFilter: map[string]bool{"app": true}}
	h.mysqlRelationDetails("app", []ObjectDoc{})
	if calls := server.recorded(); len(calls) != 0 {
		t.Fatalf("an empty schema sent %d detail queries", len(calls))
	}
}

// mysqlRoutineCatalog는 routine 목록, 스키마 단위 파라미터 행, routine 하나를 따로 묻는 질의의 응답을 정한다.
// 따로 묻는 질의는 서버처럼 이름을 대소문자 무시로 비교해, 대소문자만 다른 routine의 파라미터도 함께 돌려준다.
type mysqlRoutineCatalog struct {
	routines   [][]driver.Value // routine_name, routine_type, routine_definition
	parameters [][]driver.Value // specific_name, dtd_identifier (specific_name, ordinal_position 순)
	batchErr   error
}

func (c mysqlRoutineCatalog) reply(query string, args []driver.NamedValue) scriptedReply {
	switch {
	case strings.Contains(query, "FROM information_schema.routines"):
		return scriptedRows(c.routines...)
	case strings.Contains(query, "ORDER BY specific_name, ordinal_position"):
		if c.batchErr != nil {
			return scriptedReply{err: c.batchErr}
		}
		return scriptedRows(c.parameters...)
	case strings.Contains(query, "specific_name = ?"):
		var rows [][]driver.Value
		for _, row := range c.parameters {
			if strings.EqualFold(row[0].(string), args[1].Value.(string)) {
				rows = append(rows, []driver.Value{row[1]})
			}
		}
		return scriptedReply{columns: []string{"dtd_identifier"}, rows: rows}
	}
	return scriptedReply{}
}

// separateParameterQueries는 routine 하나를 따로 물은 이름을 질의 순서대로 모은다.
func separateParameterQueries(calls []scriptedCall) []string {
	var names []string
	for _, call := range calls {
		if strings.Contains(call.query, "specific_name = ?") {
			names = append(names, call.args[1].Value.(string))
		}
	}
	return names
}

// collectMySQLRoutines는 가짜 MySQL에서 routine을 읽어 이름·종류별 시그니처와, 따로 물은 이름을 돌려준다.
func collectMySQLRoutines(t *testing.T, catalog mysqlRoutineCatalog) (map[string]string, []string, []string) {
	t.Helper()
	db, server := newScriptedDB(catalog.reply)
	defer closeScriptedDB(t, db)
	h := &harvester{db: db, dialect: "mysql", schemaFilter: map[string]bool{"app": true}}
	signatures := map[string]string{}
	for _, routine := range h.mysqlRoutines("app") {
		signature := "<none>"
		if routine.Signature != nil {
			signature = *routine.Signature
		}
		signatures[routine.Name+" "+routine.Kind] = signature
	}
	return signatures, separateParameterQueries(server.recorded()), h.limitations
}

// 파라미터는 스키마마다 한 번 읽어 정확한 이름으로 찾는다. 이름이 같은 function·procedure와 대소문자만 다른 이름은
// 예전처럼 따로 물어, 서버 콜레이션 비교로 합쳐지던 결과(같은 순서 포함)를 그대로 둔다.
func TestMySQLRoutineParametersAreReadOncePerSchema(t *testing.T) {
	catalog := mysqlRoutineCatalog{
		routines: [][]driver.Value{
			{"Mixed", "FUNCTION", "RETURN 1"}, {"add_item", "PROCEDURE", "SELECT 1"}, {"count_items", "FUNCTION", "RETURN 1"},
			{"dup", "FUNCTION", "RETURN 1"}, {"dup", "PROCEDURE", "SELECT 1"}, {"mixed", "PROCEDURE", "SELECT 1"},
			{"no_params", "PROCEDURE", "SELECT 1"},
		},
		parameters: [][]driver.Value{
			{"Mixed", "int"}, {"add_item", "int"}, {"add_item", "varchar(10)"}, {"count_items", nil},
			{"dup", "int"}, {"dup", "date"}, {"mixed", "text"},
		},
	}
	signatures, separate, limitations := collectMySQLRoutines(t, catalog)
	want := map[string]string{
		"Mixed function": "int,text", "add_item procedure": "int,varchar(10)", "count_items function": "<none>",
		"dup function": "int,date", "dup procedure": "int,date", "mixed procedure": "int,text", "no_params procedure": "<none>",
	}
	if !reflect.DeepEqual(signatures, want) {
		t.Fatalf("signatures = %v, want %v", signatures, want)
	}
	if wantSeparate := []string{"Mixed", "dup", "dup", "mixed"}; !slices.Equal(separate, wantSeparate) {
		t.Fatalf("separately queried routines = %q, want %q", separate, wantSeparate)
	}
	if !slices.Equal(limitations, []string{"app.dup: function and procedure share a vertex id and will be merged"}) {
		t.Fatalf("limitations = %q", limitations)
	}
}

// ASCII가 아닌 routine 이름이 있으면 악센트 무시 비교를 옮길 수 없으므로, 그 스키마의 모든 routine을 따로 묻는다.
func TestMySQLRoutineParametersWithNonASCIINamesAreQueriedSeparately(t *testing.T) {
	catalog := mysqlRoutineCatalog{
		routines:   [][]driver.Value{{"café", "FUNCTION", "RETURN 1"}, {"plain", "PROCEDURE", "SELECT 1"}},
		parameters: [][]driver.Value{{"café", "int"}, {"plain", "date"}},
	}
	signatures, separate, _ := collectMySQLRoutines(t, catalog)
	if want := map[string]string{"café function": "int", "plain procedure": "date"}; !reflect.DeepEqual(signatures, want) {
		t.Fatalf("signatures = %v, want %v", signatures, want)
	}
	if !slices.Equal(separate, []string{"café", "plain"}) {
		t.Fatalf("separately queried routines = %q", separate)
	}
}

// 스키마 단위 파라미터 조회가 실패하면 routine마다 예전에 따로 묻다 실패했을 때와 같은 limitation을 남기고, 시그니처
// 없이 계속한다. 따로 묻는 질의로 다시 시도하지 않는다.
func TestMySQLRoutineParameterBatchFailureKeepsPerRoutineWording(t *testing.T) {
	catalog := mysqlRoutineCatalog{
		routines:   [][]driver.Value{{"add_item", "PROCEDURE", "SELECT 1"}, {"count_items", "FUNCTION", "RETURN 1"}},
		parameters: [][]driver.Value{{"add_item", "int"}},
		batchErr:   fmt.Errorf("injected\nparameter failure"),
	}
	signatures, separate, limitations := collectMySQLRoutines(t, catalog)
	if want := map[string]string{"add_item procedure": "<none>", "count_items function": "<none>"}; !reflect.DeepEqual(signatures, want) {
		t.Fatalf("signatures = %v, want %v", signatures, want)
	}
	want := []string{
		"app.add_item: routine parameters unavailable — injected parameter failure",
		"app.count_items: routine parameters unavailable — injected parameter failure",
	}
	if len(separate) != 0 || !slices.Equal(limitations, want) {
		t.Fatalf("separate=%q limitations=%q, want no separate queries and %q", separate, limitations, want)
	}
}

// routine이 없거나 모두 ROUTINE_TYPE이 NULL이면 파라미터를 묻지 않는다(예전에도 묻지 않았다).
func TestMySQLRoutineParametersAreNotQueriedWithoutRoutines(t *testing.T) {
	db, server := newScriptedDB(mysqlRoutineCatalog{routines: [][]driver.Value{{"lost", nil, nil}}}.reply)
	defer closeScriptedDB(t, db)
	h := &harvester{db: db, dialect: "mysql", schemaFilter: map[string]bool{"app": true}}
	h.mysqlRoutines("app")
	for _, call := range server.recorded() {
		if strings.Contains(call.query, "information_schema.parameters") {
			t.Fatalf("parameters were queried without routines: %s", call.query)
		}
	}
}

func TestMySQLCollationSensitiveNames(t *testing.T) {
	tests := []struct {
		name          string
		routines      []string
		specificNames []string
		separate      []string
		everyName     bool
	}{
		{name: "distinct ascii", routines: []string{"a", "b"}, specificNames: []string{"a", "b"}},
		{name: "same name twice", routines: []string{"f", "f"}, specificNames: []string{"f"}, separate: []string{"f"}},
		{name: "case variants", routines: []string{"F", "f", "g"}, specificNames: []string{"F", "f"}, separate: []string{"F", "f"}},
		{name: "variant only in parameters", routines: []string{"f"}, specificNames: []string{"F"}, separate: []string{"f"}},
		{name: "non-ascii routine", routines: []string{"é"}, everyName: true},
		{name: "non-ascii parameter owner", routines: []string{"e"}, specificNames: []string{"é"}, everyName: true},
		{name: "trailing space", routines: []string{"f "}, everyName: true},
		{name: "control character", routines: []string{"f\t"}, everyName: true},
	}
	for _, test := range tests {
		t.Run(test.name, func(t *testing.T) {
			specificNames := map[string]bool{}
			for _, name := range test.specificNames {
				specificNames[name] = true
			}
			separate, everyName := mysqlCollationSensitiveNames(test.routines, specificNames)
			var got []string
			for name := range separate {
				got = append(got, name)
			}
			slices.Sort(got)
			if everyName != test.everyName || !slices.Equal(got, test.separate) {
				t.Fatalf("separate=%q everyName=%v, want %q %v", got, everyName, test.separate, test.everyName)
			}
		})
	}
}
