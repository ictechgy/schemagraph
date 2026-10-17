package main

import (
	"strings"
	"testing"

	mysqlDriver "github.com/go-sql-driver/mysql"
	goora "github.com/sijms/go-ora/v2"
)

// go-ora가 실제로 해석한 접속 설정 — 프로브가 덧붙인 옵션과 원래 값이 함께 살아 있는지 본다.
func parseOracleDSN(t *testing.T, raw string) (connectionSpec, int, string) {
	t.Helper()
	spec, err := parseConnection(raw)
	if err != nil || spec.dialect != "oracle" {
		t.Fatalf("Oracle URL was rejected: %v", err)
	}
	config, err := goora.ParseConfig(spec.dsn)
	if err != nil {
		t.Fatalf("go-ora could not parse the probe DSN: %v", err)
	}
	return spec, config.PrefetchRows, config.Password
}

func TestOracleConnectionAddsPrefetchRowsWhenMissing(t *testing.T) {
	const raw = "oracle://fixture:PLACEHOLDER@localhost:1521/FREEPDB1"
	spec, prefetch, _ := parseOracleDSN(t, raw)
	if prefetch != 1000 || spec.dsn != raw+"?PREFETCH_ROWS=1000" {
		t.Fatalf("prefetch rows = %d, dsn changed beyond the added option", prefetch)
	}
}

func TestOracleConnectionKeepsUserPrefetchRows(t *testing.T) {
	const raw = "oracle://fixture:PLACEHOLDER@localhost:1521/FREEPDB1?prefetch_rows=50"
	spec, prefetch, _ := parseOracleDSN(t, raw)
	if prefetch != 50 || spec.dsn != raw {
		t.Fatalf("user prefetch rows were overridden: %d", prefetch)
	}
}

func TestOracleConnectionPreservesPasswordAndExistingOptions(t *testing.T) {
	const raw = "oracle://fixture:p%40ss%3Aw%2Frd%3F%23%25%26%3D%2B%20%21@localhost:1521/FREEPDB1?TIMEOUT=30#ignored"
	spec, prefetch, password := parseOracleDSN(t, raw)
	if password != "p@ss:w/rd?#%&=+ !" || prefetch != 1000 {
		t.Fatalf("password or prefetch rows changed (prefetch %d)", prefetch)
	}
	if !strings.HasPrefix(spec.dsn, "oracle://fixture:p%40ss%3Aw%2Frd%3F%23%25%26%3D%2B%20%21@localhost:1521/FREEPDB1?TIMEOUT=30&") {
		t.Fatal("existing URL bytes were rewritten")
	}
}

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

func TestStreamingCatalogQueriesBindActiveSchema(t *testing.T) {
	schema := "SGFIX"
	h := &harvester{activeSchema: &schema}
	query, args := h.mssqlScopedQuery("SELECT * FROM sys.objects WHERE type = 'U' ORDER BY name", "s.name = @schema")
	if !strings.Contains(query, "AND s.name = @schema ORDER BY") || len(args) != 1 {
		t.Fatalf("scoped query = %q args = %#v", query, args)
	}
	query, args = h.oracleScopedQuery("SELECT OWNER FROM ALL_OBJECTS", "OWNER = :schema")
	if !strings.Contains(query, "WHERE OWNER = :schema") || len(args) != 1 {
		t.Fatalf("unfiltered scoped query = %q args = %#v", query, args)
	}
}
