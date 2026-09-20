package main

import (
	"bytes"
	"encoding/json"
	"reflect"
	"strings"
	"testing"
)

func dependencyTestDocument() CatalogDocument {
	database := "local-db"
	filter := []string{"app", "reporting"}
	kind := "table"
	targetKind := "function"
	signature := "integer"
	member := "run"
	return CatalogDocument{
		Version: 1, Dialect: "postgres", Reader: "probe-go",
		Context: &CollectionContext{
			SourceID: "app-prod", Database: &database, SchemaFilter: &filter, CatalogComplete: true,
		},
		Schemas:     []SchemaDoc{{Name: "app", Objects: []ObjectDoc{{Name: "consumer", Kind: "table"}}, Routines: []RoutineDoc{}}},
		Limitations: []string{},
		Dependencies: []CatalogDependency{{
			Source:  CatalogObjectRef{Schema: "app", Name: "consumer", Kind: &kind},
			Target:  CatalogObjectRef{Schema: "app", Name: "worker", Kind: &targetKind, Member: &member, Signature: &signature, Database: &database},
			Catalog: "pg_depend", DependencyType: "NORMAL",
		}},
	}
}

func decodeLines(t *testing.T, encoded []byte) []map[string]any {
	t.Helper()
	lines := strings.Split(strings.TrimSpace(string(encoded)), "\n")
	decoded := make([]map[string]any, 0, len(lines))
	for _, line := range lines {
		var value map[string]any
		if err := json.Unmarshal([]byte(line), &value); err != nil {
			t.Fatalf("invalid NDJSON line %q: %v", line, err)
		}
		decoded = append(decoded, value)
	}
	return decoded
}

func TestCatalogDependencyJSONEnvelopesDeclareV1AndV2Features(t *testing.T) {
	doc := dependencyTestDocument()
	for _, version := range []int{1, 2} {
		encoded, err := encodeDocument(doc, version, "json")
		if err != nil {
			t.Fatalf("v%d JSON: %v", version, err)
		}
		var value map[string]any
		if err := json.Unmarshal(encoded, &value); err != nil {
			t.Fatal(err)
		}
		if len(value["dependencies"].([]any)) != 1 || value["context"].(map[string]any)["source_id"] != "app-prod" {
			t.Fatalf("v%d lost dependency/context: %#v", version, value)
		}
		if version == 1 {
			if value["reader"] != doc.Reader || value["producer"] != nil {
				t.Fatalf("v1 envelope = %#v", value)
			}
		} else {
			if value["reader"] != nil || value["producer"].(map[string]any)["name"] != doc.Reader {
				t.Fatalf("v2 envelope = %#v", value)
			}
			if !reflect.DeepEqual(value["required_features"], []any{"catalog-dependencies-v1"}) {
				t.Fatalf("v2 required features = %#v", value["required_features"])
			}
		}
	}
}

func TestCatalogDependencyNDJSONHeaderDependencyAndTrailerContext(t *testing.T) {
	doc := dependencyTestDocument()
	for _, version := range []int{1, 2} {
		encoded, err := encodeDocument(doc, version, "ndjson")
		if err != nil {
			t.Fatalf("v%d NDJSON: %v", version, err)
		}
		lines := decodeLines(t, encoded)
		if lines[0]["type"] != "document" || lines[len(lines)-1]["type"] != "limitations" {
			t.Fatalf("v%d envelope records = %#v", version, lines)
		}
		dependencyCount := 0
		for _, line := range lines {
			if line["type"] == "dependency" {
				dependencyCount++
				data := line["data"].(map[string]any)
				if data["dependency_type"] != "NORMAL" || data["target"].(map[string]any)["database"] != "local-db" {
					t.Fatalf("dependency payload lost identity: %#v", data)
				}
			}
		}
		if dependencyCount != 1 {
			t.Fatalf("v%d dependency records = %d", version, dependencyCount)
		}
		if version == 2 && !reflect.DeepEqual(lines[0]["required_features"], []any{"catalog-dependencies-v1"}) {
			t.Fatalf("v2 NDJSON features = %#v", lines[0]["required_features"])
		}
	}

	var output bytes.Buffer
	stream := newNDJSONStreamWriter(&output)
	context := doc.Context
	if err := stream.headerWithContext("postgres", "probe-go", 2, []string{"catalog-dependencies-v1"}, context); err != nil {
		t.Fatal(err)
	}
	if err := stream.line(map[string]any{"type": "dependency", "data": doc.Dependencies[0]}); err != nil {
		t.Fatal(err)
	}
	incomplete := *context
	incomplete.CatalogComplete = false
	if err := stream.footer([]string{"permission gap"}, &incomplete); err != nil {
		t.Fatal(err)
	}
	if err := stream.flush(); err != nil {
		t.Fatal(err)
	}
	lines := decodeLines(t, output.Bytes())
	if lines[len(lines)-1]["context"].(map[string]any)["catalog_complete"] != false {
		t.Fatalf("trailer context did not expose incomplete collection: %#v", lines[len(lines)-1])
	}
}

func TestCatalogDependencySourceAndFilterMetadataAreValidated(t *testing.T) {
	if validSourceID("") || validSourceID("postgres://user:password@host/db") || validSourceID(strings.Repeat("x", 129)) {
		t.Fatal("invalid source id accepted")
	}
	for _, value := range []string{"app-prod", "app/reporting", "A_1.2"} {
		if !validSourceID(value) {
			t.Fatalf("valid source id rejected: %q", value)
		}
	}
	h := &harvester{dialect: "sqlite", catalogDependencies: true}
	if got := h.streamFeatures(); !reflect.DeepEqual(got, []string{"catalog-dependencies-v1"}) {
		t.Fatalf("catalog dependency feature = %#v", got)
	}
}
