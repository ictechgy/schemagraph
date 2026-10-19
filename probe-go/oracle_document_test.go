package main

import (
	"bytes"
	"encoding/json"
	"reflect"
	"strings"
	"testing"
)

// JSON 모드도 스트리밍처럼 스키마마다 OWNER를 바인딩해야 한다. 바인딩하지 않으면 --schema와
// 무관하게 SYS·PUBLIC 사전 전체를 받아 클라이언트에서 버린다. 두 형식은 같은 경로로 수확해
// 같은 문서를 내야 한다.
func TestOracleJSONCollectionBindsOwnerPerSchema(t *testing.T) {
	catalog := oracleFixtureCatalog(40)
	db, server := newScriptedDB(catalog.reply)
	defer db.Close()
	doc := (&harvester{db: db, dialect: "oracle", schemaFilter: map[string]bool{}}).extract()
	for _, call := range server.recorded() {
		if !strings.Contains(call.query, "ALL_") || strings.Contains(call.query, "DISTINCT OWNER") {
			continue
		}
		if owner, bound := namedArg(call.args, "schema"); !bound || owner != "APP" || !strings.Contains(call.query, ":schema") {
			t.Errorf("catalog query was not bound to the collected owner: %.120s", oneLine(call.query))
		}
	}
	if catalog.sent["SYS"] != 0 {
		t.Errorf("JSON collection transferred %d SYS dictionary rows only to discard them", catalog.sent["SYS"])
	}
	encoded, err := encodeDocument(doc, 1, "json")
	if err != nil {
		t.Fatal(err)
	}
	var jsonValue map[string]any
	if err := json.Unmarshal(encoded, &jsonValue); err != nil {
		t.Fatal(err)
	}
	var output bytes.Buffer
	if err := (&harvester{db: db, dialect: "oracle", schemaFilter: map[string]bool{}}).streamNDJSON(&output, 1); err != nil {
		t.Fatal(err)
	}
	if assembled := assembleNDJSON(t, output.Bytes()); !reflect.DeepEqual(jsonValue, assembled) {
		t.Fatalf("JSON and NDJSON documents differ\njson:   %s\nndjson: %s", encoded, output.Bytes())
	}
}

// assembleNDJSON은 NDJSON 레코드를 JSON 문서 모양으로 다시 모은다. limitation과 context는
// 트레일러 값이 최종값이다(헤더는 수집 전이라 catalog_complete=false다).
func assembleNDJSON(t *testing.T, encoded []byte) map[string]any {
	t.Helper()
	assembled := map[string]any{}
	schemas := []any{}
	for _, record := range decodeLines(t, encoded) {
		switch record["type"] {
		case "document":
			for key, value := range record {
				if key != "type" {
					assembled[key] = value
				}
			}
		case "schema":
			schemas = append(schemas, map[string]any{"name": record["name"], "objects": []any{}, "routines": []any{}})
		case "object", "routine":
			current := schemas[len(schemas)-1].(map[string]any)
			key := record["type"].(string) + "s"
			current[key] = append(current[key].([]any), record["data"])
		case "limitations":
			assembled["limitations"] = record["data"]
			assembled["context"] = record["context"]
		}
	}
	assembled["schemas"] = schemas
	return assembled
}

func TestOracleObjectIndexesUseEmptyArrayWhenNoIndexesExist(t *testing.T) {
	object := ObjectDoc{
		Name: "ACC_GENRE_AUDIT", Kind: "table",
		Indexes: oracleObjectIndexes("table", nil),
	}
	encoded, err := json.Marshal(object)
	if err != nil {
		t.Fatal(err)
	}
	if !strings.Contains(string(encoded), `"indexes":[]`) {
		t.Fatalf("object indexes were not encoded as an empty array: %s", encoded)
	}
}

func TestOracleObjectIndexesStayEmptyForObjectsWithoutIndexes(t *testing.T) {
	if got := oracleObjectIndexes("view", nil); got == nil || len(got) != 0 {
		t.Fatalf("view indexes = %#v, want non-nil empty slice", got)
	}
}
