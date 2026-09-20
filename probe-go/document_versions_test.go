package main

import (
	"encoding/json"
	"math"
	"reflect"
	"strings"
	"testing"
)

func versionTestDocument() CatalogDocument {
	large := int64(9_007_199_254_740_993)
	return CatalogDocument{
		Version: 1, Dialect: "sqlite", Reader: "fixture-reader",
		Schemas: []SchemaDoc{{
			Name: "main",
			Objects: []ObjectDoc{{
				Name: "items", Kind: "table", Columns: []ColumnDoc{},
				Constraints: []ConstraintDoc{}, Indexes: []IndexDoc{{
					Name: "items_idx", Unique: true, Columns: []string{"id"},
					Usage: &UsageDoc{Reads: large, Writes: large - 1},
				}}, Triggers: []TriggerDoc{},
			}},
			Routines: []RoutineDoc{{
				Name: "member", Kind: "procedure", MemberOf: stringPtr("pkg"),
			}},
		}},
		Limitations: []string{"one limitation"},
	}
}

func TestEncodeDocumentV1AndV2Envelopes(t *testing.T) {
	doc := versionTestDocument()
	v1, err := encodeDocument(doc, 1, "json")
	if err != nil {
		t.Fatal(err)
	}
	v2, err := encodeDocument(doc, 2, "json")
	if err != nil {
		t.Fatal(err)
	}
	var one, two map[string]any
	if err := json.Unmarshal(v1, &one); err != nil {
		t.Fatal(err)
	}
	if err := json.Unmarshal(v2, &two); err != nil {
		t.Fatal(err)
	}
	if one["version"] != float64(1) || one["reader"] != doc.Reader {
		t.Fatalf("v1 envelope = %#v", one)
	}
	if _, ok := one["producer"]; ok {
		t.Fatalf("v1 unexpectedly has producer: %#v", one)
	}
	if two["version"] != float64(2) || two["producer"].(map[string]any)["name"] != doc.Reader {
		t.Fatalf("v2 envelope = %#v", two)
	}
	if _, ok := two["reader"]; ok {
		t.Fatalf("v2 unexpectedly has reader: %#v", two)
	}
	if got := two["required_features"]; !reflect.DeepEqual(got, []any{"package-members-v1", "usage-v1"}) {
		t.Fatalf("v2 features = %#v", got)
	}
	for _, encoded := range [][]byte{v1, v2} {
		if !strings.Contains(string(encoded), "9007199254740993") {
			t.Fatalf("large usage counter was not emitted exactly: %s", encoded)
		}
	}
}

func TestEncodeDocumentNDJSONMatchesJSONPayloads(t *testing.T) {
	doc := versionTestDocument()
	jsonBytes, err := encodeDocument(doc, 2, "json")
	if err != nil {
		t.Fatal(err)
	}
	ndjsonBytes, err := encodeDocument(doc, 2, "ndjson")
	if err != nil {
		t.Fatal(err)
	}
	var jsonValue map[string]any
	if err := json.Unmarshal(jsonBytes, &jsonValue); err != nil {
		t.Fatal(err)
	}
	assembled := map[string]any{}
	var schemas []any
	for _, raw := range strings.Split(strings.TrimSpace(string(ndjsonBytes)), "\n") {
		var record map[string]any
		if err := json.Unmarshal([]byte(raw), &record); err != nil {
			t.Fatal(err)
		}
		switch record["type"] {
		case "document":
			for key, value := range record {
				if key != "type" {
					assembled[key] = value
				}
			}
		case "schema":
			schemas = append(schemas, map[string]any{
				"name": record["name"], "objects": []any{}, "routines": []any{},
			})
		case "object", "routine":
			current := schemas[len(schemas)-1].(map[string]any)
			key := "objects"
			if record["type"] == "routine" {
				key = "routines"
			}
			current[key] = append(current[key].([]any), record["data"])
		case "limitations":
			assembled["limitations"] = record["data"]
		}
	}
	assembled["schemas"] = schemas
	if !reflect.DeepEqual(jsonValue, assembled) {
		t.Fatalf("JSON and NDJSON differ\njson: %#v\nndjson: %#v", jsonValue, assembled)
	}
	if !strings.HasSuffix(string(ndjsonBytes), "\n") || !strings.HasSuffix(string(jsonBytes), "\n") {
		t.Fatal("encoded documents must end with a newline")
	}
}

func TestEncodeDocumentV2EmptyFeaturesAndMetadata(t *testing.T) {
	doc := CatalogDocument{Version: 1, Dialect: "sqlite", Reader: "reader", Schemas: []SchemaDoc{}, Limitations: []string{}}
	encoded, err := encodeDocument(doc, 2, "ndjson")
	if err != nil {
		t.Fatal(err)
	}
	lines := strings.Split(strings.TrimSpace(string(encoded)), "\n")
	var header map[string]any
	if err := json.Unmarshal([]byte(lines[0]), &header); err != nil {
		t.Fatal(err)
	}
	if _, ok := header["schemas"]; ok {
		t.Fatalf("schemas leaked into header: %#v", header)
	}
	if !reflect.DeepEqual(header["required_features"], []any{}) {
		t.Fatalf("empty features = %#v", header["required_features"])
	}
	if !reflect.DeepEqual(header["limitations"], []any{}) {
		t.Fatalf("header limitations = %#v", header["limitations"])
	}
	var trailer map[string]any
	if err := json.Unmarshal([]byte(lines[len(lines)-1]), &trailer); err != nil {
		t.Fatal(err)
	}
	if trailer["type"] != "limitations" || !reflect.DeepEqual(trailer["data"], []any{}) {
		t.Fatalf("trailer = %#v", trailer)
	}
}

func TestEncodeDocumentRejectsUnsupportedAndEncodingErrors(t *testing.T) {
	doc := versionTestDocument()
	for _, test := range []struct {
		version int
		format  string
	}{
		{version: 0, format: "json"}, {version: 3, format: "json"},
		{version: 1, format: "yaml"}, {version: 2, format: ""},
	} {
		if _, err := encodeDocument(doc, test.version, test.format); err == nil {
			t.Fatalf("encodeDocument(%d, %q) unexpectedly succeeded", test.version, test.format)
		}
	}
	doc.Schemas[0].Objects[0].Usage = &UsageDoc{TotalMs: floatPtr(math.NaN())}
	if _, err := encodeDocument(doc, 1, "json"); err == nil {
		t.Fatal("NaN JSON encoding unexpectedly succeeded")
	}
	if _, err := encodeDocument(doc, 1, "ndjson"); err == nil {
		t.Fatal("NaN NDJSON encoding unexpectedly succeeded")
	}
}

func stringPtr(value string) *string { return &value }

func floatPtr(value float64) *float64 { return &value }
