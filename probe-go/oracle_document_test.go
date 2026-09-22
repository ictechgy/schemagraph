package main

import (
	"encoding/json"
	"strings"
	"testing"
)

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
