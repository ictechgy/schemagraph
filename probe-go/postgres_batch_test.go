package main

import (
	"bytes"
	"database/sql"
	"database/sql/driver"
	"encoding/json"
	"fmt"
	"reflect"
	"strings"
	"testing"
)

// closeScriptedDB는 가짜 서버 풀을 닫고, 닫기 실패를 테스트 오류로 남긴다.
func closeScriptedDB(t *testing.T, db *sql.DB) {
	t.Helper()
	if err := db.Close(); err != nil {
		t.Errorf("close scripted database: %v", err)
	}
}

// postgresTablesReply는 app 스키마에 테이블 count개가 보이고 나머지 조회는 빈 결과인 PostgreSQL이다.
func postgresTablesReply(count int) func(string, []driver.NamedValue) scriptedReply {
	return func(query string, _ []driver.NamedValue) scriptedReply {
		switch {
		case strings.Contains(query, "FROM pg_namespace"):
			return scriptedRows([]driver.Value{"app"})
		case strings.Contains(query, "FROM information_schema.tables"):
			rows := make([][]driver.Value, 0, count)
			for i := range count {
				rows = append(rows, []driver.Value{fmt.Sprintf("t%03d", i), "BASE TABLE"})
			}
			return scriptedRows(rows...)
		}
		return scriptedReply{}
	}
}

// postgresQueryCount는 테이블 count개 스키마를 JSON과 NDJSON으로 한 번씩 수집해 형식별로 서버에 닿은 질의 수를 센다.
func postgresQueryCount(t *testing.T, count int) map[string]int {
	t.Helper()
	counts := map[string]int{}
	for _, format := range []string{"json", "ndjson"} {
		db, server := newScriptedDB(postgresTablesReply(count))
		h := &harvester{db: db, dialect: "postgres", schemaFilter: map[string]bool{"app": true}}
		if format == "json" {
			doc := h.extract()
			if len(doc.Schemas) != 1 || len(doc.Schemas[0].Objects) != count {
				t.Fatalf("json collected %d schemas, want one with %d objects", len(doc.Schemas), count)
			}
		} else if err := h.streamNDJSON(&bytes.Buffer{}, 2); err != nil {
			t.Fatal(err)
		}
		counts[format] = len(server.recorded())
		if err := db.Close(); err != nil {
			t.Fatal(err)
		}
	}
	return counts
}

// 객체마다 컬럼·제약·인덱스·트리거 질의를 보내면 고정 연결 하나에서 왕복이 테이블 수의 네 배가 된다.
// 원격 DB(왕복 2 ms)에서 2,000테이블이 8,036문장, 22.6 s였다. 스키마당 질의 수는 객체 수와 무관해야 한다.
func TestPostgresQueriesPerSchemaDoNotGrowWithObjectCount(t *testing.T) {
	small, large := postgresQueryCount(t, 1), postgresQueryCount(t, 40)
	if !reflect.DeepEqual(small, large) {
		t.Fatalf("queries per schema grew with the object count: 1 table %v, 40 tables %v", small, large)
	}
}

// postgresCatalogReply는 스키마 단위 조회 넷이 여러 관계의 행을 섞어 돌려주는 PostgreSQL이다.
// 관계 순서와 섞인 행으로 나누기가 결과 순서에 기대지 않는지 본다. ghost는 객체 목록에 없는 관계다.
func postgresCatalogReply(query string, _ []driver.NamedValue) scriptedReply {
	switch {
	case strings.Contains(query, "FROM pg_namespace"):
		return scriptedRows([]driver.Value{"app"})
	case strings.Contains(query, "FROM information_schema.tables"):
		return scriptedRows([]driver.Value{"orders", "BASE TABLE"}, []driver.Value{"customers", "BASE TABLE"}, []driver.Value{"order_totals", "VIEW"})
	case strings.Contains(query, "FROM pg_sequences"):
		return scriptedRows([]driver.Value{"orders_id_seq"})
	case strings.Contains(query, "information_schema.columns"):
		return scriptedRows(
			[]driver.Value{"customers", "id", "bigint", "NO", nil, int64(1)},
			[]driver.Value{"customers", "name", "text", "YES", "'n/a'::text", int64(2)},
			[]driver.Value{"ghost", "id", "bigint", "NO", nil, int64(1)},
			[]driver.Value{"order_totals", "total", "numeric", "YES", nil, int64(1)},
			[]driver.Value{"orders", "id", "bigint", "NO", "nextval('app.orders_id_seq'::regclass)", int64(1)},
			[]driver.Value{"orders", "customer_id", "bigint", "YES", nil, int64(2)},
			[]driver.Value{"orders", "region", "text", "NO", nil, int64(3)},
		)
	case strings.Contains(query, "FROM pg_constraint con"):
		return scriptedRows(
			[]driver.Value{"customers", "customers_pkey", "p", "id", nil, nil, nil, int64(1)},
			[]driver.Value{"orders", "orders_customer_fk", "f", "customer_id", "id", "customers", "app", int64(1)},
			[]driver.Value{"ghost", "ghost_pkey", "p", "id", nil, nil, nil, int64(1)},
			[]driver.Value{"orders", "orders_pkey", "p", "region", nil, nil, nil, int64(1)},
			[]driver.Value{"orders", "orders_pkey", "p", "id", nil, nil, nil, int64(2)},
		)
	case strings.Contains(query, "FROM pg_index idx"):
		return scriptedRows(
			[]driver.Value{"orders", "orders_customer_idx", false, "customer_id", int64(1), true, nil},
			[]driver.Value{"customers", "customers_name_key", true, "name", int64(1), true, "(name <> ''::text)"},
			[]driver.Value{"orders", "orders_customer_idx", false, nil, int64(2), true, nil},
		)
	case strings.Contains(query, "FROM pg_trigger tg"):
		return scriptedRows([]driver.Value{"orders", "orders_audit", "CREATE TRIGGER orders_audit AFTER INSERT ON app.orders FOR EACH ROW EXECUTE FUNCTION app.audit()"})
	}
	return scriptedReply{}
}

// 스키마 단위 조회의 행은 정확한 관계 이름으로 나뉜다. 관계 안의 순서는 테이블마다 읽던 때와 같고,
// 행이 없는 객체(뷰의 제약, 시퀀스)는 nil이 아니라 빈 슬라이스라 JSON이 null 대신 []를 낸다.
func TestPostgresSchemaRowsAreSplitByExactRelationName(t *testing.T) {
	db, _ := newScriptedDB(postgresCatalogReply)
	defer closeScriptedDB(t, db)
	h := &harvester{db: db, dialect: "postgres", schemaFilter: map[string]bool{"app": true}}
	schema := h.postgresSchema("app")
	for _, limitation := range h.limitations {
		for _, label := range []string{"columns", "constraints", "indexes", "triggers"} {
			if strings.HasPrefix(limitation, label+" ") {
				t.Fatalf("batch query %s failed: %q", label, limitation)
			}
		}
	}
	objects := map[string]ObjectDoc{}
	for _, object := range schema.Objects {
		objects[object.Name] = object
	}
	want := map[string]ObjectDoc{
		"customers": {
			Name: "customers", Kind: "table",
			Columns: []ColumnDoc{
				{Name: "id", DataType: "bigint", Nullable: false, Ordinal: 1, PkPosition: 1},
				{Name: "name", DataType: "text", Nullable: true, Default: stringPtr("'n/a'::text"), Ordinal: 2},
			},
			Constraints: []ConstraintDoc{{Name: "customers_pkey", Kind: "pk", Columns: []string{"id"}}},
			Indexes: []IndexDoc{{Name: "customers_name_key", Unique: true, Columns: []string{"name"},
				DefinitionComplete: boolValue(true), HasPredicate: boolValue(true), Predicate: stringPtr("(name <> ''::text)")}},
			Triggers: []TriggerDoc{},
		},
		"order_totals": {
			Name: "order_totals", Kind: "view",
			Columns:     []ColumnDoc{{Name: "total", DataType: "numeric", Nullable: true, Ordinal: 1}},
			Constraints: []ConstraintDoc{}, Indexes: []IndexDoc{}, Triggers: []TriggerDoc{},
		},
		"orders": {
			Name: "orders", Kind: "table",
			Columns: []ColumnDoc{
				{Name: "id", DataType: "bigint", Nullable: false, Default: stringPtr("nextval('app.orders_id_seq'::regclass)"), Ordinal: 1, PkPosition: 2},
				{Name: "customer_id", DataType: "bigint", Nullable: true, Ordinal: 2},
				{Name: "region", DataType: "text", Nullable: false, Ordinal: 3, PkPosition: 1},
			},
			Constraints: []ConstraintDoc{
				{Name: "orders_customer_fk", Kind: "fk", Columns: []string{"customer_id"},
					Referenced: &ReferencedDoc{Schema: stringPtr("app"), Table: "customers", Columns: []string{"id"}}},
				{Name: "orders_pkey", Kind: "pk", Columns: []string{"region", "id"}},
			},
			Indexes: []IndexDoc{{Name: "orders_customer_idx", Unique: false, Columns: []string{"customer_id"},
				DefinitionComplete: boolValue(false), HasPredicate: boolValue(false)}},
			Triggers: []TriggerDoc{{Name: "orders_audit", Body: stringPtr("CREATE TRIGGER orders_audit AFTER INSERT ON app.orders FOR EACH ROW EXECUTE FUNCTION app.audit()")}},
		},
		"orders_id_seq": {
			Name: "orders_id_seq", Kind: "sequence",
			Columns: []ColumnDoc{}, Constraints: []ConstraintDoc{}, Indexes: []IndexDoc{}, Triggers: []TriggerDoc{},
		},
	}
	if !reflect.DeepEqual(objects, want) {
		got, err := json.MarshalIndent(objects, "", " ")
		if err != nil {
			t.Fatalf("split objects differ and cannot be shown: %v", err)
		}
		t.Fatalf("split objects differ:\n%s", got)
	}
	sequence, err := json.Marshal(objects["orders_id_seq"])
	if err != nil {
		t.Fatal(err)
	}
	if !bytes.Contains(sequence, []byte(`"columns":[],"constraints":[],"indexes":[],"triggers":[]`)) {
		t.Fatalf("an object without rows must encode empty arrays, not null: %s", sequence)
	}
}

// 스키마 단위 조회 하나가 실패하면 limitation은 테이블마다가 아니라 스키마에 한 번 남고, 미완전으로 표시된다.
// 그 스키마의 객체는 그 항목만 빈 배열로 남는다(테이블마다 읽던 때 한 테이블이 실패한 것과 같은 모양).
func TestPostgresSchemaBatchFailureIsReportedOncePerSchema(t *testing.T) {
	db, _ := newScriptedDB(func(query string, args []driver.NamedValue) scriptedReply {
		if strings.Contains(query, "FROM pg_constraint con") {
			return scriptedReply{err: fmt.Errorf("injected constraint failure")}
		}
		return postgresCatalogReply(query, args)
	})
	defer closeScriptedDB(t, db)
	h := &harvester{db: db, dialect: "postgres", schemaFilter: map[string]bool{"app": true}}
	schema := h.postgresSchema("app")
	failures := 0
	for _, limitation := range h.limitations {
		if limitation == "constraints unavailable — injected constraint failure" {
			failures++
		}
	}
	if failures != 1 || !h.catalogIncomplete {
		t.Fatalf("constraint failure must be reported once and mark the catalog incomplete: %q", h.limitations)
	}
	for _, object := range schema.Objects {
		if object.Constraints == nil || len(object.Constraints) != 0 {
			t.Fatalf("%s: constraints = %#v, want an empty slice", object.Name, object.Constraints)
		}
		if object.Name == "orders" && len(object.Columns) != 3 {
			t.Fatalf("other batch queries must still fill the schema: %#v", object.Columns)
		}
	}
}

// 스키마 단위 세부 조회의 커서가 도중에 실패하면(rows.Err) 그때까지 읽은 행도 쓰지 않고 그 부분을 스키마의 모든
// 객체에서 비운다. 남기면 어느 객체까지, 다중 컬럼 제약·인덱스는 어느 키 컬럼까지 채워지는지가 실패 시점에 따라
// 달라진다. limitation은 예전 문구로 한 번 남고 미완전으로 표시된다.
func TestPostgresSchemaBatchCursorFailureLeavesThatPartEmpty(t *testing.T) {
	failure := fmt.Errorf("injected cursor failure")
	tests := []struct {
		label, marker string
		empty         func(ObjectDoc) bool
	}{
		{"columns", "information_schema.columns", func(object ObjectDoc) bool { return len(object.Columns) == 0 }},
		{"constraints", "FROM pg_constraint con", func(object ObjectDoc) bool { return len(object.Constraints) == 0 }},
		{"indexes", "FROM pg_index idx", func(object ObjectDoc) bool { return len(object.Indexes) == 0 }},
		{"triggers", "FROM pg_trigger tg", func(object ObjectDoc) bool { return len(object.Triggers) == 0 }},
	}
	for _, test := range tests {
		t.Run(test.label, func(t *testing.T) {
			db, _ := newScriptedDB(func(query string, args []driver.NamedValue) scriptedReply {
				reply := postgresCatalogReply(query, args)
				if strings.Contains(query, test.marker) {
					reply.rowsErr = failure
				}
				return reply
			})
			defer closeScriptedDB(t, db)
			h := &harvester{db: db, dialect: "postgres", schemaFilter: map[string]bool{"app": true}}
			schema := h.postgresSchema("app")
			failures := 0
			for _, limitation := range h.limitations {
				if limitation == test.label+" unavailable — injected cursor failure" {
					failures++
				}
			}
			if failures != 1 || !h.catalogIncomplete {
				t.Fatalf("%s failure must be reported once and mark the catalog incomplete: %q", test.label, h.limitations)
			}
			for _, object := range schema.Objects {
				if !test.empty(object) {
					t.Fatalf("%s: %s kept rows read before the cursor failed: %#v", object.Name, test.label, object)
				}
			}
		})
	}
}

// 컬럼 조회는 엔진과 cmp로 같은 정본(columns-postgres.sql)에서 실행 시 파생한다. 정본이 바뀌어
// 치환 원문이 맞지 않으면 이 테스트가 먼저 실패해야 한다.
func TestPostgresSchemaColumnQueryDerivesFromTheSharedCanonicalQuery(t *testing.T) {
	raw, err := catalogQueries.ReadFile("sql/columns-postgres.sql")
	if err != nil {
		t.Fatal(err)
	}
	query, err := postgresSchemaColumnQuery(string(raw))
	if err != nil {
		t.Fatal(err)
	}
	for _, removed := range []string{":table", ":schema", "table_name = ", "c.relname = "} {
		if strings.Contains(query, removed) {
			t.Fatalf("derived query still contains %q:\n%s", removed, query)
		}
	}
	for _, added := range []string{
		"SELECT table_name::text AS relation_name, column_name::text AS column_name,",
		"SELECT c.relname::text, a.attname::text,",
		"WHERE table_schema = $1\n",
		"WHERE n.nspname = $1 AND c.relkind = 'm'",
		"ORDER BY relation_name, ordinal_position",
	} {
		if strings.Count(query, added) != 1 {
			t.Fatalf("derived query lacks %q exactly once:\n%s", added, query)
		}
	}
}

// 치환 원문이 없거나 두 번 이상 맞으면 엉뚱한 곳을 바꾼 질의를 보내지 않고 오류를 낸다.
func TestPostgresSchemaColumnQueryRejectsUnexpectedCanonicalText(t *testing.T) {
	raw, err := catalogQueries.ReadFile("sql/columns-postgres.sql")
	if err != nil {
		t.Fatal(err)
	}
	canonical := string(raw)
	for name, text := range map[string]string{
		"missing":   strings.Replace(canonical, "ORDER BY ordinal_position", "ORDER BY 5", 1),
		"duplicate": canonical + "\n-- ORDER BY ordinal_position\n",
	} {
		if query, err := postgresSchemaColumnQuery(text); err == nil {
			t.Errorf("%s canonical text was accepted:\n%s", name, query)
		}
	}
}

// 정본을 스키마 단위로 바꾸지 못하면(여기서는 빈 정본) 질의를 보내지 않고, limitation과 미완전 표시를
// 한 번 남긴다. 컬럼은 빈 배열이 된다.
func TestPostgresUnadaptableColumnQueryIsReported(t *testing.T) {
	db, server := newScriptedDB(postgresCatalogReply)
	defer closeScriptedDB(t, db)
	h := &harvester{db: db, dialect: "postgres", schemaFilter: map[string]bool{"app": true}}
	columns := h.postgresSchemaColumns("app", "")
	if len(columns) != 0 || !h.catalogIncomplete || len(h.limitations) != 1 ||
		!strings.HasPrefix(h.limitations[0], "bundled PostgreSQL column query could not be adapted") {
		t.Fatalf("columns=%v incomplete=%v limitations=%q", columns, h.catalogIncomplete, h.limitations)
	}
	if calls := server.recorded(); len(calls) != 0 {
		t.Fatalf("an unadapted column query must not reach the server: %d queries", len(calls))
	}
}
