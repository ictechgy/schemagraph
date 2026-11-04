package main

import (
	"database/sql"
	"sort"
	"strings"
	"testing"

	_ "modernc.org/sqlite"
)

// sqliteNamesTestDoc는 문장을 적용한 메모리 DB를 Go 프로브로 읽는다.
func sqliteNamesTestDoc(t *testing.T, statements ...string) (*harvester, CatalogDocument) {
	t.Helper()
	db, err := sql.Open("sqlite", ":memory:")
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { db.Close() })
	db.SetMaxOpenConns(1)
	for _, statement := range statements {
		if _, err := db.Exec(statement); err != nil {
			t.Fatalf("execute %q: %v", statement, err)
		}
	}
	h := &harvester{db: db, dialect: "sqlite", schemaFilter: map[string]bool{}}
	return h, h.extractSQLite()
}

// `sqlite_` 예약 접두만 내부 객체다. LIKE의 `_` 와일드카드가 사용자 객체를
// 지우던 회귀를 막는다.
func TestSQLiteKeepsNamesThatOnlyResembleTheReservedPrefix(t *testing.T) {
	h, doc := sqliteNamesTestDoc(t,
		"CREATE TABLE users(id INTEGER PRIMARY KEY)",
		"CREATE TABLE sqlitex_users(id INTEGER PRIMARY KEY)",
		"CREATE TABLE SQLiteVersion(id INTEGER PRIMARY KEY)",
		"CREATE TABLE auto(id INTEGER PRIMARY KEY AUTOINCREMENT)",
		"CREATE TRIGGER sqlitex_trg AFTER INSERT ON users BEGIN SELECT 1; END",
	)
	names := []string{}
	for _, object := range doc.Schemas[0].Objects {
		names = append(names, object.Name)
	}
	sort.Strings(names)
	if strings.Join(names, ",") != "SQLiteVersion,auto,sqlitex_users,users" {
		t.Fatalf("objects = %v", names)
	}
	if users := sqliteTestObject(t, doc.Schemas[0].Objects, "users"); len(users.Triggers) != 1 {
		t.Fatalf("users triggers = %#v", users.Triggers)
	}
	if h.catalogIncomplete || len(h.limitations) != 0 {
		t.Fatalf("incomplete=%v limitations=%v", h.catalogIncomplete, h.limitations)
	}
}

// tbl_name은 DDL 철자 그대로다. SQLite 식별자는 ASCII 대소문자를 구분하지 않는다.
func TestSQLiteTriggerTargetMatchesTableCaseInsensitively(t *testing.T) {
	h, doc := sqliteNamesTestDoc(t,
		"CREATE TABLE orders(id INTEGER PRIMARY KEY)",
		"CREATE TRIGGER t_upper AFTER INSERT ON ORDERS BEGIN SELECT 1; END",
	)
	if orders := sqliteTestObject(t, doc.Schemas[0].Objects, "orders"); len(orders.Triggers) != 1 {
		t.Fatalf("orders triggers = %#v limitations=%v", orders.Triggers, h.limitations)
	}
	if h.catalogIncomplete || len(h.limitations) != 0 {
		t.Fatalf("incomplete=%v limitations=%v", h.catalogIncomplete, h.limitations)
	}
}

// 인라인 제약 한계는 키워드 토큰에만 붙고 카탈로그 완전성을 낮추지 않는다.
func TestSQLiteInlineConstraintNoticeNeedsKeywordAndKeepsCatalogComplete(t *testing.T) {
	h, _ := sqliteNamesTestDoc(t,
		`CREATE TABLE files(id INTEGER PRIMARY KEY, checksum TEXT, "check" TEXT, uniqueness TEXT DEFAULT 'unique check')`,
		"CREATE VIEW v AS SELECT checksum FROM files -- unique check",
	)
	if h.catalogIncomplete || len(h.limitations) != 0 {
		t.Fatalf("substring-only DDL: incomplete=%v limitations=%v", h.catalogIncomplete, h.limitations)
	}
	for _, ddl := range []string{
		"CREATE TABLE t(id INTEGER PRIMARY KEY, email TEXT UNIQUE)",
		"CREATE TABLE t(id INTEGER PRIMARY KEY, n INTEGER CHECK (n > 0))",
		"CREATE TABLE t(id INTEGER PRIMARY KEY, n INTEGER, CONSTRAINT c CHECK(n>0))",
	} {
		h, _ := sqliteNamesTestDoc(t, ddl)
		if h.catalogIncomplete || len(h.limitations) != 1 || !strings.Contains(h.limitations[0], "inline UNIQUE/CHECK") {
			t.Fatalf("%s: incomplete=%v limitations=%v", ddl, h.catalogIncomplete, h.limitations)
		}
	}
}

// 키워드 토큰 판정은 인용·주석·문자열을 건너뛴다.
func TestSQLiteDDLWordsSkipQuotesAndComments(t *testing.T) {
	cases := map[string]bool{
		"CREATE TABLE a(x UNIQUE)":                       true,
		"create table a(x int check(x>0))":               true,
		"CREATE TABLE a(checksum TEXT)":                  false,
		`CREATE TABLE a("unique" TEXT, [check] TEXT)`:    false,
		"CREATE TABLE a(x TEXT DEFAULT 'unique''check')": false,
		"CREATE TABLE a(x TEXT) -- unique\n":             false,
		"CREATE TABLE a(x TEXT /* check */)":             false,
		"CREATE TABLE a(`check` TEXT)":                   false,
	}
	for ddl, want := range cases {
		if got := sqliteDeclaresInlineConstraint(ddl); got != want {
			t.Errorf("%q: got %v want %v", ddl, got, want)
		}
	}
}
