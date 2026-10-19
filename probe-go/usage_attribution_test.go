package main

import (
	"database/sql/driver"
	"fmt"
	"math/rand/v2"
	"reflect"
	"slices"
	"strings"
	"testing"
)

// 사용 통계·뷰 본문을 객체·routine에 귀속하는 결과를 예전 선형 탐색과 대조한다. linear…는 예전
// 루프를 옮긴 기준 구현이다. 입력에는 같은 이름의 객체, 같은 행의 반복, 카탈로그에 없는 이름의
// 통계 행, NULL 본문, 음수 카운터, 오버로드된 routine을 섞는다.

const attributionSince = "2026-10-01 00:00:00"

// mysqlAttributionInput은 가짜 MySQL의 객체 목록·뷰 본문·사용 통계 조회가 돌려줄 행이다.
type mysqlAttributionInput struct {
	objects [][]driver.Value    // table_name, table_type
	indexes map[string][]string // 객체 이름 → 인덱스 이름. mysqlIndexes 조회 대신 테스트가 채운다.
	views   [][]driver.Value    // table_name, view_definition
	stats   [][]driver.Value    // table_name, rows_fetched, rows_written
	unused  [][]driver.Value    // object_name, index_name
}

func (in mysqlAttributionInput) reply(query string, _ []driver.NamedValue) scriptedReply {
	switch {
	case strings.Contains(query, "FROM information_schema.tables"):
		return scriptedRows(in.objects...)
	case strings.Contains(query, "FROM information_schema.views"):
		return scriptedRows(in.views...)
	case strings.Contains(query, "@@performance_schema"):
		return scriptedRows([]driver.Value{"1"})
	case strings.Contains(query, "VARIABLE_NAME = 'Uptime'"):
		return scriptedRows([]driver.Value{attributionSince})
	case strings.Contains(query, "sys.schema_table_statistics"):
		return scriptedRows(in.stats...)
	case strings.Contains(query, "sys.schema_unused_indexes"):
		return scriptedRows(in.unused...)
	}
	return scriptedReply{}
}

// collectMySQLAttribution은 가짜 MySQL에서 객체를 읽고 인덱스를 채운 뒤 사용 통계를 귀속한다.
func collectMySQLAttribution(t *testing.T, in mysqlAttributionInput) ([]ObjectDoc, []string) {
	t.Helper()
	db, _ := newScriptedDB(in.reply)
	defer func() {
		if err := db.Close(); err != nil {
			t.Errorf("close scripted database: %v", err)
		}
	}()
	h := &harvester{db: db, dialect: "mysql", schemaFilter: map[string]bool{}}
	objects := h.mysqlObjects("app")
	for i := range objects {
		for _, name := range in.indexes[objects[i].Name] {
			objects[i].Indexes = append(objects[i].Indexes, IndexDoc{Name: name, Columns: []string{}})
		}
	}
	h.mysqlObjectUsage("app", objects)
	return objects, h.limitations
}

func textValue(value driver.Value) *string {
	if value == nil {
		return nil
	}
	text := value.(string)
	return &text
}

// linearMySQLAttribution은 예전 귀속 루프다. 본문·통계가 없는 객체 목록에 같은 행을 차례로 귀속한다.
func linearMySQLAttribution(objects []ObjectDoc, in mysqlAttributionInput) []ObjectDoc {
	since := attributionSince
	for _, row := range in.views {
		for i := range objects {
			if objects[i].Name == row[0].(string) {
				objects[i].Body = textValue(row[1])
			}
		}
	}
	for _, row := range in.stats {
		for i := range objects {
			if objects[i].Name == row[0].(string) {
				objects[i].Usage = &UsageDoc{Since: &since, Reads: maxInt64(row[1].(int64)), Writes: maxInt64(row[2].(int64))}
			}
		}
	}
	for _, row := range in.unused {
		for i := range objects {
			if objects[i].Name != row[0].(string) {
				continue
			}
			for j := range objects[i].Indexes {
				if objects[i].Indexes[j].Name == row[1].(string) {
					objects[i].Indexes[j].Usage = &UsageDoc{Since: &since}
				}
			}
		}
	}
	return objects
}

// fixedMySQLAttributionInput에는 같은 이름의 객체 두 개(예전 루프는 둘 다에 귀속했다), 반복된
// 뷰·통계 행(마지막 행이 남는다), 카탈로그에 없는 테이블·인덱스 행, NULL 본문, 음수 카운터가 있다.
func fixedMySQLAttributionInput() mysqlAttributionInput {
	return mysqlAttributionInput{
		objects: [][]driver.Value{{"orders", "BASE TABLE"}, {"orders_v", "VIEW"}, {"twin", "BASE TABLE"}, {"twin", "VIEW"}, {"quiet", "BASE TABLE"}},
		indexes: map[string][]string{"orders": {"orders_customer_ix", "orders_code_uq"}, "twin": {"twin_ix"}},
		views:   [][]driver.Value{{"orders_v", "select 1"}, {"twin", "select 2"}, {"missing_v", "select 3"}, {"orders_v", "select 4"}, {"quiet", nil}},
		stats:   [][]driver.Value{{"orders", int64(10), int64(2)}, {"twin", int64(5), int64(-1)}, {"missing", int64(1), int64(1)}, {"orders", int64(11), int64(3)}},
		unused:  [][]driver.Value{{"orders", "orders_code_uq"}, {"twin", "twin_ix"}, {"orders", "missing_ix"}, {"missing", "orders_code_uq"}},
	}
}

// randomMySQLAttributionInput은 이름 열두 개에서 뽑아 같은 이름과 없는 이름이 자주 나오게 한다.
func randomMySQLAttributionInput(seed uint64) mysqlAttributionInput {
	random := rand.New(rand.NewPCG(seed, 2026))
	name := func() string { return fmt.Sprintf("t%d", random.IntN(12)) }
	index := func() string { return fmt.Sprintf("ix%d", random.IntN(4)) }
	in := mysqlAttributionInput{indexes: map[string][]string{}}
	for range 1 + random.IntN(10) {
		object := name()
		in.objects = append(in.objects, []driver.Value{object, []string{"BASE TABLE", "VIEW"}[random.IntN(2)]})
		for range random.IntN(3) {
			in.indexes[object] = append(in.indexes[object], index())
		}
	}
	for range random.IntN(8) {
		body := driver.Value(fmt.Sprintf("select %d", random.IntN(100)))
		if random.IntN(4) == 0 {
			body = nil
		}
		in.views = append(in.views, []driver.Value{name(), body})
	}
	for range random.IntN(10) {
		in.stats = append(in.stats, []driver.Value{name(), int64(random.IntN(50) - 5), int64(random.IntN(50) - 5)})
	}
	for range random.IntN(10) {
		in.unused = append(in.unused, []driver.Value{name(), index()})
	}
	return in
}

func TestMySQLUsageAttributionMatchesLinearScan(t *testing.T) {
	inputs := []mysqlAttributionInput{fixedMySQLAttributionInput()}
	for seed := uint64(1); seed <= 64; seed++ {
		inputs = append(inputs, randomMySQLAttributionInput(seed))
	}
	for i, in := range inputs {
		got, limitations := collectMySQLAttribution(t, in)
		bare, _ := collectMySQLAttribution(t, mysqlAttributionInput{objects: in.objects, indexes: in.indexes})
		want := linearMySQLAttribution(bare, in)
		if !reflect.DeepEqual(got, want) {
			t.Fatalf("input %d: attribution differs from the linear scan\n got: %+v\nwant: %+v", i, got, want)
		}
		if len(limitations) > 0 {
			t.Fatalf("input %d: unexpected limitations %q", i, limitations)
		}
	}
}

// collectPostgresRoutineUsage는 가짜 PostgreSQL의 함수 통계(funcname, calls, total_time, self_time)를 귀속한다.
func collectPostgresRoutineUsage(t *testing.T, routines []RoutineDoc, stats [][]driver.Value) []string {
	t.Helper()
	db, _ := newScriptedDB(func(query string, _ []driver.NamedValue) scriptedReply {
		switch {
		case strings.Contains(query, "stats_reset"):
			return scriptedRows([]driver.Value{attributionSince})
		case strings.Contains(query, "track_functions"):
			return scriptedRows([]driver.Value{"all"})
		case strings.Contains(query, "FROM pg_stat_user_functions"):
			return scriptedRows(stats...)
		}
		return scriptedReply{}
	})
	defer func() {
		if err := db.Close(); err != nil {
			t.Errorf("close scripted database: %v", err)
		}
	}()
	h := &harvester{db: db, dialect: "postgres", schemaFilter: map[string]bool{}}
	h.postgresRoutineUsage("app", routines)
	return h.limitations
}

func floatValue(value driver.Value) *float64 {
	if value == nil {
		return nil
	}
	number := value.(float64)
	return &number
}

// linearPostgresRoutineUsage는 예전 귀속 루프다. funcname과 이름이 같은 routine이 하나면 귀속하고,
// 오버로드로 여럿이면 그 통계 행을 모호하다고 센다.
func linearPostgresRoutineUsage(routines []RoutineDoc, stats [][]driver.Value) []string {
	since := attributionSince
	ambiguous := 0
	for _, row := range stats {
		hits := []int{}
		for i := range routines {
			if routines[i].Name == row[0].(string) {
				hits = append(hits, i)
			}
		}
		if len(hits) == 1 {
			routines[hits[0]].Usage = &UsageDoc{Since: &since, Reads: maxInt64(row[1].(int64)), TotalMs: floatValue(row[2]), SelfMs: floatValue(row[3])}
		} else if len(hits) > 1 {
			ambiguous++
		}
	}
	if ambiguous == 0 {
		return nil
	}
	return []string{fmt.Sprintf("app: usage unavailable for %d overloaded routines because funcname is ambiguous", ambiguous)}
}

func routine(name, signature string) RoutineDoc {
	language := "sql"
	return RoutineDoc{Name: name, Kind: "function", Language: &language, Signature: &signature}
}

func TestPostgresRoutineUsageAttributionMatchesLinearScan(t *testing.T) {
	type input struct {
		routines []RoutineDoc
		stats    [][]driver.Value
	}
	// 오버로드 f 두 개, 통계 행이 두 번 온 g(마지막 행이 남는다), 카탈로그에 없는 missing, 시간 열이 NULL인 h.
	inputs := []input{{
		routines: []RoutineDoc{routine("f", "integer"), routine("f", "text"), routine("g", ""), routine("h", "integer")},
		stats: [][]driver.Value{{"f", int64(3), 1.5, 1.0}, {"g", int64(2), 0.5, 0.25}, {"missing", int64(9), 9.0, 9.0},
			{"g", int64(7), 2.0, 1.0}, {"f", int64(4), 1.0, 1.0}, {"h", int64(-1), nil, nil}},
	}}
	for seed := uint64(1); seed <= 64; seed++ {
		random := rand.New(rand.NewPCG(seed, 1019))
		var in input
		for range random.IntN(10) {
			in.routines = append(in.routines, routine(fmt.Sprintf("r%d", random.IntN(8)), fmt.Sprintf("arg%d", random.IntN(3))))
		}
		for range random.IntN(12) {
			in.stats = append(in.stats, []driver.Value{fmt.Sprintf("r%d", random.IntN(10)), int64(random.IntN(20) - 2), float64(random.IntN(9)), nil})
		}
		inputs = append(inputs, in)
	}
	for i, in := range inputs {
		got, want := slices.Clone(in.routines), slices.Clone(in.routines)
		limitations := collectPostgresRoutineUsage(t, got, in.stats)
		wantLimitations := linearPostgresRoutineUsage(want, in.stats)
		if !reflect.DeepEqual(got, want) || !slices.Equal(limitations, wantLimitations) {
			t.Fatalf("input %d: attribution differs from the linear scan\n got: %+v %q\nwant: %+v %q", i, got, limitations, want, wantLimitations)
		}
	}
}
