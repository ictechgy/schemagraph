package main

import (
	"bytes"
	"database/sql"
	"errors"
	"io"
	"reflect"
	"strings"
	"testing"

	_ "modernc.org/sqlite"
)

func TestSQLiteNDJSONStreamHasBoundedRecordEnvelope(t *testing.T) {
	db, err := sql.Open("sqlite", "file:stream-envelope?mode=memory&cache=shared")
	if err != nil {
		t.Fatal(err)
	}
	defer db.Close()
	db.SetMaxOpenConns(1)
	if _, err := db.Exec(`CREATE TABLE stream_items (id INTEGER PRIMARY KEY, name TEXT)`); err != nil {
		t.Fatal(err)
	}

	h := &harvester{db: db, dialect: "sqlite", schemaFilter: map[string]bool{}}
	var output bytes.Buffer
	if err := h.streamNDJSON(&output, 2); err != nil {
		t.Fatal(err)
	}
	lines := strings.Split(strings.TrimSpace(output.String()), "\n")
	if len(lines) < 3 {
		t.Fatalf("stream produced too few records: %d", len(lines))
	}
	if !strings.Contains(lines[0], `"type":"document"`) || !strings.Contains(lines[0], `"limitations":[]`) {
		t.Fatalf("invalid header: %s", lines[0])
	}
	if strings.Contains(lines[0], `"reader"`) || !strings.Contains(lines[0], `"producer"`) {
		t.Fatalf("v2 metadata envelope is wrong: %s", lines[0])
	}
	if !strings.Contains(lines[len(lines)-1], `"type":"limitations"`) {
		t.Fatalf("missing final limitations trailer: %s", lines[len(lines)-1])
	}
}

func TestNDJSONEmptyLimitationsRemainAnArray(t *testing.T) {
	var output bytes.Buffer
	stream := newNDJSONStreamWriter(&output)
	if err := stream.limitations(nil); err != nil {
		t.Fatal(err)
	}
	if err := stream.flush(); err != nil {
		t.Fatal(err)
	}
	var record struct {
		Type string   `json:"type"`
		Data []string `json:"data"`
	}
	if err := json.Unmarshal(output.Bytes(), &record); err != nil {
		t.Fatal(err)
	}
	if record.Type != "limitations" || record.Data == nil || len(record.Data) != 0 {
		t.Fatalf("empty limitation evidence must be an array: %s", output.String())
	}
}

func TestNDJSONStreamFeatureHeadersUseDialectCapabilities(t *testing.T) {
	tests := []struct {
		dialect string
		want    []string
	}{
		{dialect: "sqlite", want: []string{}},
		{dialect: "sqlserver", want: []string{}},
		{dialect: "postgres", want: []string{"usage-v1"}},
		{dialect: "mysql", want: []string{"usage-v1"}},
		{dialect: "oracle", want: []string{"package-members-v1"}},
	}
	for _, test := range tests {
		t.Run(test.dialect, func(t *testing.T) {
			h := &harvester{dialect: test.dialect}
			if got := h.streamFeatures(); !reflect.DeepEqual(got, test.want) {
				t.Fatalf("features = %#v, want %#v", got, test.want)
			}
		})
	}
}

func TestSQLiteNDJSONStreamPropagatesWriterErrors(t *testing.T) {
	db, err := sql.Open("sqlite", "file:stream-error?mode=memory&cache=shared")
	if err != nil {
		t.Fatal(err)
	}
	defer db.Close()
	db.SetMaxOpenConns(1)
	if _, err := db.Exec(`CREATE TABLE stream_error (id INTEGER PRIMARY KEY)`); err != nil {
		t.Fatal(err)
	}
	h := &harvester{db: db, dialect: "sqlite", schemaFilter: map[string]bool{}}
	if err := h.streamNDJSON(errorWriter{}, 1); !errors.Is(err, errStreamWriter) {
		t.Fatalf("stream error = %v", err)
	}
}

func BenchmarkSQLiteNDJSONStream(b *testing.B) {
	db, err := sql.Open("sqlite", "file:stream-benchmark?mode=memory&cache=shared")
	if err != nil {
		b.Fatal(err)
	}
	defer db.Close()
	db.SetMaxOpenConns(1)
	if _, err := db.Exec(`CREATE TABLE stream_benchmark (id INTEGER PRIMARY KEY, value TEXT)`); err != nil {
		b.Fatal(err)
	}
	h := &harvester{db: db, dialect: "sqlite", schemaFilter: map[string]bool{}}
	b.ReportAllocs()
	for i := 0; i < b.N; i++ {
		h.limitations = nil
		if err := h.streamNDJSON(io.Discard, 1); err != nil {
			b.Fatal(err)
		}
	}
}

var errStreamWriter = errors.New("stream writer failed")

type errorWriter struct{}

func (errorWriter) Write([]byte) (int, error) { return 0, errStreamWriter }
