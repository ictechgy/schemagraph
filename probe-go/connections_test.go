package main

import (
	"strings"
	"testing"

	mysqlDriver "github.com/go-sql-driver/mysql"
)

func TestConnectionURLsPreserveDatabaseAndEscaping(t *testing.T) {
	spec, err := parseConnection("mysql://fixture:p%40ss@[::1]:3307/demo%20db?tls=false")
	if err != nil {
		t.Fatal(err)
	}
	config, err := mysqlDriver.ParseDSN(spec.dsn)
	if err != nil || config.DBName != "demo db" || config.Passwd != "p@ss" || config.Addr != "[::1]:3307" || spec.schema != "demo db" {
		t.Fatal("MySQL URL components were not preserved")
	}
	spec, err = parseConnection("mssql://fixture@localhost/demo")
	if err != nil || !strings.Contains(spec.dsn, "database=demo") || spec.dialect != "sqlserver" {
		t.Fatal("SQL Server database alias was not normalized")
	}
}

func TestSQLiteConnectionAlwaysUsesReadOnlyFile(t *testing.T) {
	for _, raw := range []string{"sqlite:demo.db", "sqlite:///tmp/demo.db?mode=rwc", "file:/tmp/demo.db"} {
		spec, err := parseConnection(raw)
		if err != nil || !strings.Contains(spec.dsn, "mode=ro") || strings.Contains(spec.dsn, "mode=rwc") {
			t.Fatalf("SQLite connection was not read-only: %v", err)
		}
	}
	if _, err := parseConnection("sqlite::memory:"); err == nil {
		t.Fatal("in-memory URL cannot identify an existing database")
	}
}

func TestConnectionErrorsDoNotEchoCredentials(t *testing.T) {
	_, err := parseConnection("postgres://fixture:private%xx@localhost/db")
	if err == nil || strings.Contains(err.Error(), "private") {
		t.Fatal("invalid URL error leaked credentials or accepted malformed encoding")
	}
}
