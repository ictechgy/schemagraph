package main

import (
	"context"
	"database/sql"
	"database/sql/driver"
	"errors"
	"io"
	"os"
	"strings"
	"sync"
	"testing"

	_ "modernc.org/sqlite"
)

func TestSQLiteAdapterExtractsCatalogOnSingleConnection(t *testing.T) {
	db, err := sql.Open("sqlite", "file:sqlite-adapter-test?mode=memory&cache=shared")
	if err != nil {
		t.Fatal(err)
	}
	defer db.Close()
	db.SetMaxOpenConns(1)
	for _, statement := range []string{
		"PRAGMA foreign_keys = ON",
		`CREATE TABLE "parent" ("id" INTEGER PRIMARY KEY, "name" TEXT NOT NULL)`,
		`CREATE TABLE "child" ("id" INTEGER PRIMARY KEY, "parent_id" INTEGER REFERENCES "parent"("id"), UNIQUE("parent_id"))`,
		`CREATE INDEX "child_parent_idx" ON "child" ("parent_id")`,
		`CREATE VIEW "child_view" AS SELECT "id", "parent_id" FROM "child"`,
		`CREATE TRIGGER "child_touch" AFTER INSERT ON "child" BEGIN UPDATE "parent" SET "name" = "name" WHERE "id" = NEW."parent_id"; END`,
	} {
		if _, err := db.Exec(statement); err != nil {
			t.Fatalf("execute %q: %v", statement, err)
		}
	}

	doc := (&harvester{db: db, dialect: "sqlite", schemaFilter: map[string]bool{}}).extractSQLite()
	if len(doc.Schemas) != 1 || doc.Schemas[0].Name != "main" {
		t.Fatalf("schemas = %#v", doc.Schemas)
	}
	child := sqliteTestObject(t, doc.Schemas[0].Objects, "child")
	if child.Body == nil || len(child.Columns) != 2 || len(child.Indexes) != 2 {
		t.Fatalf("child catalog = %#v", child)
	}
	if len(child.Constraints) != 2 || child.Constraints[0].Kind != "fk" {
		t.Fatalf("child constraints = %#v", child.Constraints)
	}
	if len(child.Triggers) != 1 {
		t.Fatalf("child triggers = %#v", child.Triggers)
	}
	view := sqliteTestObject(t, doc.Schemas[0].Objects, "child_view")
	if view.Kind != "view" || view.Body == nil {
		t.Fatalf("view = %#v", view)
	}
}

func sqliteTestObject(t *testing.T, objects []ObjectDoc, name string) ObjectDoc {
	t.Helper()
	for _, object := range objects {
		if object.Name == name {
			return object
		}
	}
	t.Fatalf("object %q not found", name)
	return ObjectDoc{}
}

func TestSQLitePrimaryKeyNullability(t *testing.T) {
	db, err := sql.Open("sqlite", ":memory:")
	if err != nil {
		t.Fatal(err)
	}
	defer db.Close()
	db.SetMaxOpenConns(1)
	fixture, err := os.ReadFile("../Fixtures/sqlite-nullability.sql")
	if err != nil {
		t.Fatal(err)
	}
	if _, err := db.Exec(string(fixture)); err != nil {
		t.Fatal(err)
	}
	h := &harvester{db: db, dialect: "sqlite", schemaFilter: map[string]bool{}}
	doc := h.extractSQLite()
	if len(doc.Schemas) != 1 || len(doc.Schemas[0].Objects) != 9 {
		t.Fatalf("unexpected catalog: %#v", doc)
	}
	for _, object := range doc.Schemas[0].Objects {
		for _, column := range object.Columns {
			if column.Nullable != strings.HasPrefix(object.Name, "nullable_") || column.PkPosition == 0 {
				t.Errorf("%s.%s incorrect nullability/PK: %#v", object.Name, column.Name, column)
			}
		}
	}
}

// PK 인덱스 수집 실패는 rowid 별칭이라는 긍정 증거가 아니다.
var registerSQLiteNullabilityDriver sync.Once

func TestSQLiteIndexFailureKeepsPrimaryKeyNullable(t *testing.T) {
	registerSQLiteNullabilityDriver.Do(func() { sql.Register("sqlite-nullability-index-failure", sqliteNullabilityDriver{}) })
	db, err := sql.Open("sqlite-nullability-index-failure", "")
	if err != nil {
		t.Fatal(err)
	}
	defer db.Close()
	h := &harvester{db: db}
	columns := h.sqliteColumns("main", "items")
	if len(columns) != 1 || !columns[0].Nullable || !h.catalogIncomplete || len(h.limitations) != 1 {
		t.Fatalf("incomplete metadata was treated as a rowid alias: %#v, %#v", columns, h.limitations)
	}
}

type sqliteNullabilityDriver struct{}

func (sqliteNullabilityDriver) Open(string) (driver.Conn, error) { return sqliteNullabilityConn{}, nil }

type sqliteNullabilityConn struct{}

func (sqliteNullabilityConn) Prepare(string) (driver.Stmt, error) {
	return nil, errors.New("unsupported prepare")
}
func (sqliteNullabilityConn) Close() error { return nil }
func (sqliteNullabilityConn) Begin() (driver.Tx, error) {
	return nil, errors.New("unsupported transaction")
}
func (sqliteNullabilityConn) QueryContext(_ context.Context, query string, _ []driver.NamedValue) (driver.Rows, error) {
	if strings.Contains(query, "index_list") {
		return nil, errors.New("injected index metadata failure")
	}
	return &sqliteNullabilityRows{}, nil
}

type sqliteNullabilityRows struct{ sent bool }

func (*sqliteNullabilityRows) Columns() []string {
	return []string{"cid", "name", "type", "notnull", "dflt_value", "pk", "hidden"}
}
func (*sqliteNullabilityRows) Close() error { return nil }
func (rows *sqliteNullabilityRows) Next(values []driver.Value) error {
	if rows.sent {
		return io.EOF
	}
	copy(values, []driver.Value{int64(0), "id", "INTEGER", int64(0), nil, int64(1), int64(0)})
	rows.sent = true
	return nil
}
