package main

import (
	"bytes"
	"database/sql"
	"database/sql/driver"
	"errors"
	"slices"
	"strings"
	"testing"

	_ "modernc.org/sqlite"
)

// emptyOracleCatalog는 Oracle 카탈로그가 테이블·뷰를 하나도 주지 않았을 때의 기존 limitation 원문이다.
// 기존 문서와 바이트가 같아야 하므로 프로브 상수를 재사용하지 않고 원문으로 단언한다.
const emptyOracleCatalog = "카탈로그가 테이블/뷰를 하나도 주지 않았다 — 접근 권한을 확인해라"

// collectionVerdict는 문서가 스스로 선언한 완전성과 그 근거(limitation)다.
type collectionVerdict struct {
	complete    bool
	limitations []string
}

// collectBothFormats는 같은 설정의 수확기로 JSON 문서와 NDJSON 트레일러를 만들어 각 판정을 돌려준다.
// 두 형식은 같은 collectionContext를 쓰므로, 한쪽만 고치면 이 비교에서 갈린다.
func collectBothFormats(t *testing.T, newHarvester func() *harvester) map[string]collectionVerdict {
	t.Helper()
	doc := newHarvester().extract()
	verdicts := map[string]collectionVerdict{
		"json": {complete: doc.Context.CatalogComplete, limitations: doc.Limitations},
	}
	var output bytes.Buffer
	if err := newHarvester().streamNDJSON(&output, 1); err != nil {
		t.Fatal(err)
	}
	lines := decodeLines(t, output.Bytes())
	trailer := lines[len(lines)-1]
	ndjson := collectionVerdict{complete: trailer["context"].(map[string]any)["catalog_complete"].(bool)}
	for _, limitation := range trailer["data"].([]any) {
		ndjson.limitations = append(ndjson.limitations, limitation.(string))
	}
	verdicts["ndjson"] = ndjson
	return verdicts
}

func requestedSchemaLimitation(schema string) string {
	return "requested schema '" + schema + "' was not collected; verify existence and metadata permissions"
}

// --schema 오타로 빈 카탈로그를 "완전"이라고 내면, review가 빈 스냅샷끼리 비교해 변경 0건으로 통과시킨다.
func TestSQLiteRequestedSchemaTypoIsIncompleteInJSONAndNDJSON(t *testing.T) {
	db, err := sql.Open("sqlite", "file:requested-schema?mode=memory&cache=shared")
	if err != nil {
		t.Fatal(err)
	}
	defer db.Close()
	db.SetMaxOpenConns(1)
	if _, err := db.Exec(`CREATE TABLE items (id INTEGER PRIMARY KEY, name TEXT)`); err != nil {
		t.Fatal(err)
	}
	for _, test := range []struct {
		schema   string
		complete bool
		want     []string
	}{
		{schema: "mian", complete: false, want: []string{requestedSchemaLimitation("mian")}},
		{schema: "main", complete: true},
	} {
		verdicts := collectBothFormats(t, func() *harvester {
			return &harvester{db: db, dialect: "sqlite", schemaFilter: map[string]bool{test.schema: true}}
		})
		for format, verdict := range verdicts {
			if verdict.complete != test.complete || !slices.Equal(verdict.limitations, test.want) {
				t.Errorf("--schema %s %s: complete=%v limitations=%q", test.schema, format, verdict.complete, verdict.limitations)
			}
		}
	}
}

// 요청한 스키마가 방언의 스키마 목록에 없으면 JSON·NDJSON 모두 limitation과 미완전을 낸다.
// 목록에 있으면(대조군) 완전 판정을 지키고 같은 limitation을 만들지 않는다. Oracle은 이름을
// 대소문자까지 정확히 비교하므로 소문자 app은 APP를 찾지 못한다.
func TestRequestedSchemaAbsentFromDiscoveryIsIncomplete(t *testing.T) {
	oracle := &oracleCatalog{owners: []string{"APP"}, objects: [][]driver.Value{{"APP", "ORDERS", "table"}}}
	for _, test := range []struct {
		dialect, present, absent string
		reply                    func(string, []driver.NamedValue) scriptedReply
	}{
		{dialect: "mysql", present: "app", absent: "reporting", reply: mysqlDiscoveryReply},
		{dialect: "oracle", present: "APP", absent: "app", reply: oracle.reply},
		{dialect: "sqlserver", present: "dbo", absent: "Sales", reply: mssqlDiscoveryReply},
	} {
		for _, schema := range []string{test.present, test.absent} {
			db, _ := newScriptedDB(test.reply)
			verdicts := collectBothFormats(t, func() *harvester {
				return &harvester{db: db, dialect: test.dialect, schemaFilter: map[string]bool{schema: true}}
			})
			if err := db.Close(); err != nil {
				t.Fatal(err)
			}
			missing := schema == test.absent
			for format, verdict := range verdicts {
				reported := slices.Contains(verdict.limitations, requestedSchemaLimitation(schema))
				if reported != missing || verdict.complete == missing {
					t.Errorf("%s --schema %s %s: complete=%v limitations=%q", test.dialect, schema, format, verdict.complete, verdict.limitations)
				}
			}
		}
	}
}

// MySQL 스키마 목록의 조회·행 읽기·커서 실패는 limitation과 함께 미완전으로 표시한다. 표시하지
// 않으면 "스키마 목록 없음" limitation과 catalog_complete=true가 한 문서에 함께 실린다.
func TestMySQLSchemaListFailuresMarkCatalogIncomplete(t *testing.T) {
	for _, test := range []struct {
		name  string
		reply scriptedReply
		want  string
	}{
		{name: "query", reply: scriptedReply{err: errors.New("injected list failure")}, want: "schemas unavailable — injected list failure"},
		{name: "row", reply: scriptedRows([]driver.Value{nil}), want: "schema row read failed — "},
		{name: "cursor", reply: scriptedReply{rowsErr: errors.New("injected cursor failure")}, want: "schemas unavailable — injected cursor failure"},
	} {
		t.Run(test.name, func(t *testing.T) {
			db, _ := newScriptedDB(func(query string, args []driver.NamedValue) scriptedReply {
				if strings.Contains(query, "information_schema.schemata") {
					return test.reply
				}
				return mysqlDiscoveryReply(query, args)
			})
			defer db.Close()
			verdicts := collectBothFormats(t, func() *harvester {
				return &harvester{db: db, dialect: "mysql", schemaFilter: map[string]bool{}}
			})
			for format, verdict := range verdicts {
				failed := slices.ContainsFunc(verdict.limitations, func(limitation string) bool {
					return strings.HasPrefix(limitation, test.want)
				})
				if verdict.complete || !failed {
					t.Errorf("%s: complete=%v limitations=%q", format, verdict.complete, verdict.limitations)
				}
			}
		})
	}
}

// Oracle 카탈로그가 테이블·뷰를 하나도 주지 않으면 limitation만 남기지 않고 미완전으로 표시한다.
// routine만 있는 스키마는 정상이므로, 다른 스키마에 객체가 있으면 완전 판정을 지킨다.
func TestOracleEmptyCatalogIsIncompleteButRoutineOnlySchemaIsNot(t *testing.T) {
	for _, test := range []struct {
		name     string
		catalog  *oracleCatalog
		complete bool
	}{
		{name: "no objects", catalog: &oracleCatalog{owners: []string{"APP"}}},
		{name: "routine-only schema", complete: true, catalog: &oracleCatalog{
			owners:   []string{"API", "APP"},
			objects:  [][]driver.Value{{"APP", "ORDERS", "table"}},
			routines: [][]driver.Value{{"API", "PING", "PROCEDURE", int64(1), "PROCEDURE PING IS BEGIN NULL; END;"}},
		}},
	} {
		db, _ := newScriptedDB(test.catalog.reply)
		verdicts := collectBothFormats(t, func() *harvester {
			return &harvester{db: db, dialect: "oracle", schemaFilter: map[string]bool{}}
		})
		if err := db.Close(); err != nil {
			t.Fatal(err)
		}
		for format, verdict := range verdicts {
			reported := slices.Contains(verdict.limitations, emptyOracleCatalog)
			if verdict.complete != test.complete || reported == test.complete {
				t.Errorf("%s %s: complete=%v limitations=%q", test.name, format, verdict.complete, verdict.limitations)
			}
		}
	}
}

// mysqlDiscoveryReply는 기본 DB가 app이고 schemata에 app만 보이는 MySQL이다. 나머지 조회는 빈 결과다.
func mysqlDiscoveryReply(query string, _ []driver.NamedValue) scriptedReply {
	switch {
	case strings.Contains(query, "DATABASE()"), strings.Contains(query, "information_schema.schemata"):
		return scriptedRows([]driver.Value{"app"})
	}
	return scriptedReply{}
}

// mssqlDiscoveryReply는 dbo에 테이블 하나가 보이는 SQL Server다. 나머지 조회는 빈 결과다.
func mssqlDiscoveryReply(query string, _ []driver.NamedValue) scriptedReply {
	switch {
	case strings.Contains(query, "DB_NAME()"):
		return scriptedRows([]driver.Value{"app"})
	case strings.Contains(query, "SELECT DISTINCT s.name"):
		return scriptedRows([]driver.Value{"dbo"})
	case strings.Contains(query, "SELECT s.name, o.name, o.type"):
		return scriptedRows([]driver.Value{"dbo", "orders", "U"})
	}
	return scriptedReply{}
}

// oracleCatalog는 OWNER가 첫 열인 Oracle 사전 행을 돌려준다. OWNER가 :schema로 바인딩되면
// 서버처럼 그 소유자의 행만 보낸다. 정하지 않은 사전 조회는 빈 결과다.
type oracleCatalog struct {
	owners   []string
	objects  [][]driver.Value // OWNER, 이름, kind
	routines [][]driver.Value // OWNER, 이름, OBJECT_TYPE, LINE, TEXT
	// details는 그 밖의 사전 조회를 질의에 든 고유 문자열로 찾아 돌려줄 행이다.
	details map[string][][]driver.Value
	// sent는 소유자별로 클라이언트에 보낸 사전 행 수다. nil이면 세지 않는다.
	sent map[string]int
}

func (c *oracleCatalog) reply(query string, args []driver.NamedValue) scriptedReply {
	switch {
	case strings.Contains(query, "SYS_CONTEXT"):
		return scriptedRows([]driver.Value{"FREEPDB1"})
	case strings.Contains(query, "SELECT DISTINCT OWNER FROM ALL_OBJECTS"):
		owners := make([][]driver.Value, 0, len(c.owners))
		for _, owner := range c.owners {
			owners = append(owners, []driver.Value{owner})
		}
		return scriptedRows(owners...)
	case strings.Contains(query, "'table' FROM ALL_TABLES"):
		return c.owned(c.objects, args)
	case strings.Contains(query, "JOIN ALL_SOURCE"):
		return c.owned(c.routines, args)
	}
	for marker, rows := range c.details {
		if strings.Contains(query, marker) {
			return c.owned(rows, args)
		}
	}
	return scriptedReply{}
}

// owned는 바인딩된 소유자의 행만 남긴다. 바인딩이 없으면 보이는 사전 전체를 보낸다.
func (c *oracleCatalog) owned(rows [][]driver.Value, args []driver.NamedValue) scriptedReply {
	owner, bound := namedArg(args, "schema")
	kept := [][]driver.Value{}
	for _, row := range rows {
		if !bound || row[0] == owner {
			kept = append(kept, row)
			if c.sent != nil {
				c.sent[row[0].(string)]++
			}
		}
	}
	return scriptedRows(kept...)
}

// oracleFixtureCatalog는 작은 APP 스키마와 수확 대상이 아닌 SYS 사전 행을 함께 가진 Oracle이다.
// sysLines는 SYS 패키지 원문 줄 수다. OWNER 바인딩이 없으면 그만큼 버릴 행을 전송한다.
// 표지 문자열은 질의마다 하나에만 맞는다(PACKAGE_NAME IS NULL은 IS NOT NULL에 들지 않는다).
func oracleFixtureCatalog(sysLines int) *oracleCatalog {
	source := [][]driver.Value{{"APP", "PING", "PROCEDURE", int64(1), "PROCEDURE PING(n NUMBER) IS BEGIN NULL; END;"}}
	for line := 1; line <= sysLines; line++ {
		source = append(source, []driver.Value{"SYS", "DBMS_FIXTURE", "PACKAGE", int64(line), "-- system package line\n"})
	}
	return &oracleCatalog{
		owners: []string{"APP", "SYS"},
		objects: [][]driver.Value{
			{"APP", "ORDERS", "table"}, {"APP", "ORDER_TOTALS", "view"},
			{"SYS", "DUAL", "table"}, {"SYS", "ALL_FIXTURES", "view"},
		},
		routines: source,
		details: map[string][][]driver.Value{
			"NULLABLE, COLUMN_ID": {
				{"APP", "ORDERS", "ID", "NUMBER", "N", int64(1)}, {"APP", "ORDERS", "TOTAL", "NUMBER", "Y", int64(2)},
				{"APP", "ORDER_TOTALS", "TOTAL", "NUMBER", "Y", int64(1)}, {"SYS", "DUAL", "DUMMY", "VARCHAR2", "Y", int64(1)},
			},
			"DATA_DEFAULT FROM ALL_TAB_COLUMNS": {{"APP", "ORDERS", "TOTAL", "0"}, {"SYS", "DUAL", "DUMMY", nil}},
			"CONSTRAINT_TYPE = 'P'":             {{"APP", "ORDERS", "ORDERS_PK", "ID", int64(1)}},
			"FROM ALL_INDEXES":                  {{"APP", "ORDERS", "ORDERS_PK", "UNIQUE", "ID", int64(1)}},
			"VIEW_NAME, TEXT FROM ALL_VIEWS": {
				{"APP", "ORDER_TOTALS", "SELECT TOTAL FROM ORDERS"}, {"SYS", "ALL_FIXTURES", "SELECT DUMMY FROM DUAL"},
			},
			"FROM ALL_TRIGGERS":    {{"APP", "ORDERS", "ORDERS_AUDIT", "BEGIN NULL; END;"}},
			"PACKAGE_NAME IS NULL": {{"APP", "PING", "NUMBER"}},
		},
		sent: map[string]int{},
	}
}
