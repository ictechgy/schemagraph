package main

import (
	"database/sql"
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
