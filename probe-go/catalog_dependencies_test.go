package main

import (
	"bytes"
	"database/sql/driver"
	"encoding/json"
	"math/rand/v2"
	"reflect"
	"slices"
	"sort"
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

// randomCatalogDependencies는 pg_depend 조회 결과처럼 보이는 의존성을 만든다. 이름을 작은 집합에서
// 뽑아 정렬 키 앞부분이 자주 겹치게 하고, NULL 선택 필드와 완전히 같은 행(중복)도 섞는다.
func randomCatalogDependencies(count int, seed uint64) []CatalogDependency {
	random := rand.New(rand.NewPCG(seed, seed^0x5eed))
	pick := func(values ...string) string { return values[random.IntN(len(values))] }
	optional := func(values ...string) *string {
		if random.IntN(3) == 0 {
			return nil
		}
		value := pick(values...)
		return &value
	}
	dependencies := make([]CatalogDependency, 0, count)
	for len(dependencies) < count {
		if len(dependencies) > 0 && random.IntN(8) == 0 {
			dependencies = append(dependencies, dependencies[random.IntN(len(dependencies))])
			continue
		}
		dependencies = append(dependencies, CatalogDependency{
			Source:  CatalogObjectRef{Schema: pick("app", "billing"), Name: pick("orders_v", "orders_mv", "report"), Kind: optional("view", "function"), Signature: optional("", "integer")},
			Target:  CatalogObjectRef{Schema: pick("app", "billing"), Name: pick("orders", "items", "total"), Kind: optional("table", "function"), Signature: optional("integer, text"), Member: optional("id", "amount", "status"), Database: optional("main")},
			Catalog: "pg_depend", DependencyType: pick("n", "a"),
		})
	}
	return dependencies
}

// dependencyRow는 의존성을 readCatalogDependencies가 읽는 열 11개짜리 행으로 되돌린다.
func dependencyRow(dependency CatalogDependency) []driver.Value {
	text := func(value *string) driver.Value {
		if value == nil {
			return nil
		}
		return *value
	}
	source, target := dependency.Source, dependency.Target
	return []driver.Value{source.Schema, source.Name, text(source.Kind), text(source.Signature),
		target.Schema, target.Name, text(target.Kind), text(target.Signature), text(target.Member), text(target.Database),
		dependency.DependencyType}
}

// dependencyReadAllocs는 dependencies를 행으로 돌려주는 가짜 PostgreSQL에서 readCatalogDependencies의
// 실행당 평균 할당 수와 결과를 잰다. 할당 수는 같은 코드·같은 입력에서 늘 같아 시간보다 안정적이다.
func dependencyReadAllocs(t *testing.T, dependencies []CatalogDependency) (float64, []CatalogDependency) {
	t.Helper()
	rows := make([][]driver.Value, 0, len(dependencies))
	for _, dependency := range dependencies {
		rows = append(rows, dependencyRow(dependency))
	}
	db, _ := newScriptedDB(func(query string, _ []driver.NamedValue) scriptedReply {
		if strings.Contains(query, "FROM pg_depend") {
			return scriptedRows(rows...)
		}
		return scriptedReply{}
	})
	defer func() {
		if err := db.Close(); err != nil {
			t.Errorf("close scripted database: %v", err)
		}
	}()
	h := &harvester{db: db, dialect: "postgres", schemaFilter: map[string]bool{}, catalogDependencies: true}
	var got []CatalogDependency
	allocs := testing.AllocsPerRun(3, func() { got = h.readCatalogDependencies("app") })
	if len(h.limitations) > 0 {
		t.Fatalf("scripted dependency read reported limitations: %q", h.limitations)
	}
	return allocs, got
}

// 의존성은 dependencyKey 바이트 순서로 나오고, Go 프로브는 같은 행을 합치지 않는다.
// 정렬 키를 비교할 때마다 새로 만들면 할당 수가 입력 순서를 따라간다. 이미 정렬된 입력은 비교가
// 약 D번이고 섞인 입력은 약 D·log2 D번이다. 키를 원소마다 한 번만 만들면 같은 행 집합의 할당 수는
// 순서와 무관하다. 그래서 시간 대신 두 순서의 할당 수 차이로 반복 계산을 판정한다.
func TestCatalogDependencySortBuildsEachKeyOnce(t *testing.T) {
	dependencies := randomCatalogDependencies(1024, 20261019)
	want := slices.Clone(dependencies)
	sort.SliceStable(want, func(i, j int) bool { return dependencyKey(want[i]) < dependencyKey(want[j]) })
	shuffledAllocs, shuffled := dependencyReadAllocs(t, dependencies)
	presortedAllocs, presorted := dependencyReadAllocs(t, want)
	if !reflect.DeepEqual(shuffled, want) || !reflect.DeepEqual(presorted, want) {
		t.Fatalf("dependencies are not in dependencyKey order with duplicates kept (got %d and %d, want %d)", len(shuffled), len(presorted), len(want))
	}
	if extra := shuffledAllocs - presortedAllocs; extra > float64(len(dependencies))/8 {
		t.Fatalf("sorting %d shuffled dependencies made %.0f more allocations than presorted input (%.0f vs %.0f); sort keys are rebuilt per comparison",
			len(dependencies), extra, shuffledAllocs, presortedAllocs)
	}
}
