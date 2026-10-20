package main

import (
	"database/sql/driver"
	"encoding/json"
	"fmt"
	"reflect"
	"strings"
	"testing"
)

// mariadbCollationJoin은 information_schema 이름을 대소문자 무시로 비교하는 MariaDB다. P와 p는 대소문자만 다른
// 테이블이고, 대소문자만 다른 스키마 APP에도 P가 있다. 제약 조인을 쓰면 세 테이블의 PRIMARY 키 컬럼이 P·p의 PRIMARY
// 모두와 짝지어진다. 제약을 테이블마다 조인으로 묻는 질의(인자 둘: 스키마, 테이블), 스키마 단위로 조인하는 질의(인자
// 하나, 맨 앞 열이 제약 테이블), 제약 행과 키 컬럼 행을 따로 묻는 질의(키 컬럼에 APP의 행도 섞여 온다)에 모두 답해,
// 수집 방식과 관계없이 같은 서버를 흉내 낸다.
func mariadbCollationJoin(query string, args []driver.NamedValue) scriptedReply {
	keys := [][3]string{{"app", "P", "a"}, {"app", "p", "b"}, {"APP", "P", "c"}}
	switch {
	case strings.Contains(query, "information_schema.schemata"):
		return scriptedRows([]driver.Value{"app"})
	case strings.Contains(query, "FROM information_schema.tables"):
		return scriptedRows([]driver.Value{"P", "BASE TABLE"}, []driver.Value{"p", "BASE TABLE"})
	case !strings.Contains(query, "LEFT JOIN") && strings.Contains(query, "FROM information_schema.table_constraints"):
		return scriptedRows([]driver.Value{"P", "PRIMARY", "PRIMARY KEY"}, []driver.Value{"p", "PRIMARY", "PRIMARY KEY"})
	case strings.Contains(query, "FROM information_schema.key_column_usage") && !strings.Contains(query, "'PRIMARY'"):
		var rows [][]driver.Value
		for _, key := range keys {
			rows = append(rows, []driver.Value{key[0], key[1], "PRIMARY", key[2], nil, nil, nil})
		}
		return scriptedRows(rows...)
	case strings.Contains(query, "FROM information_schema.table_constraints"):
		var rows [][]driver.Value
		for _, table := range []string{"P", "p"} {
			if len(args) == 2 && args[1].Value != table {
				continue
			}
			for _, key := range keys {
				row := []driver.Value{"PRIMARY", "PRIMARY KEY", key[0], key[1], "PRIMARY", key[2], nil, nil, nil}
				if len(args) == 1 {
					row = append([]driver.Value{table}, row...)
				}
				rows = append(rows, row)
			}
		}
		return scriptedRows(rows...)
	}
	return scriptedReply{}
}

// 제약 조인이 이름을 콜레이션으로 비교해 대소문자만 다른 테이블·스키마의 키 컬럼을 짝지어도, 제약에는 그 테이블
// 자신의 키 컬럼만 남는다(네이티브 reader와 같다). 예전에는 P와 p의 PRIMARY가 a·b·c를 정해지지 않은 순서로 가졌다.
func TestMySQLConstraintsKeepOnlyTheirOwnKeyColumns(t *testing.T) {
	db, _ := newScriptedDB(mariadbCollationJoin)
	defer closeScriptedDB(t, db)
	h := &harvester{db: db, dialect: "mysql", schemaFilter: map[string]bool{"app": true}}
	doc := h.extract()
	if len(doc.Schemas) != 1 {
		t.Fatalf("collected %d schemas, want app only: %q", len(doc.Schemas), doc.Limitations)
	}
	got := map[string][]string{}
	for _, object := range doc.Schemas[0].Objects {
		for _, constraint := range object.Constraints {
			got[object.Name+"."+constraint.Name] = constraint.Columns
		}
	}
	if want := map[string][]string{"P.PRIMARY": {"a"}, "p.PRIMARY": {"b"}}; !reflect.DeepEqual(got, want) {
		t.Fatalf("constraint columns = %v, want %v", got, want)
	}
}

// mariadbSharedConstraintName은 테이블 c에 이름이 같은 UNIQUE와 FK(dup, 두 컬럼)가 있는 MariaDB 11.4다. 키 컬럼 행은
// 키 순서마다 두 제약의 것이 하나씩 있다. 테이블 하나를 묻는 서버 조인은 그 행들을 제약 행마다 짝지어 키 순서로
// 돌려주고, 같은 키 순서 안에서는 FK 행을 먼저 낸다(실제 MariaDB 11.4.13의 순서). 스키마 단위 제약 행 질의는 UNIQUE를
// 먼저 낸다. 테이블 plain의 FK는 이름이 겹치지 않는다.
func mariadbSharedConstraintName(query string, args []driver.NamedValue) scriptedReply {
	unique := func(column string) []driver.Value { return []driver.Value{"app", "c", "dup", column, nil, nil, nil} }
	foreign := func(column, target string) []driver.Value {
		return []driver.Value{"app", "c", "dup", column, "app", "p", target}
	}
	switch {
	case strings.Contains(query, "information_schema.schemata"):
		return scriptedRows([]driver.Value{"app"})
	case strings.Contains(query, "FROM information_schema.tables"):
		return scriptedRows([]driver.Value{"c", "BASE TABLE"}, []driver.Value{"plain", "BASE TABLE"})
	case strings.Contains(query, "LEFT JOIN"):
		if len(args) != 2 || args[1].Value != "c" {
			return scriptedReply{err: fmt.Errorf("unexpected per-table constraint join for %v", args)}
		}
		var rows [][]driver.Value
		for _, keys := range [][2][]driver.Value{{foreign("a", "x"), unique("a")}, {foreign("b", "y"), unique("b")}} {
			for _, kind := range []string{"FOREIGN KEY", "UNIQUE"} {
				for _, key := range keys {
					rows = append(rows, append([]driver.Value{"dup", kind}, key...))
				}
			}
		}
		return scriptedRows(rows...)
	case strings.Contains(query, "FROM information_schema.table_constraints"):
		return scriptedRows([]driver.Value{"c", "dup", "UNIQUE"}, []driver.Value{"c", "dup", "FOREIGN KEY"},
			[]driver.Value{"plain", "fk_plain", "FOREIGN KEY"})
	case strings.Contains(query, "FROM information_schema.key_column_usage") && !strings.Contains(query, "'PRIMARY'"):
		return scriptedRows(unique("a"), foreign("a", "x"), unique("b"), foreign("b", "y"),
			[]driver.Value{"app", "plain", "fk_plain", "a", "app", "p", "x"},
			[]driver.Value{"app", "plain", "fk_plain", "b", "app", "p", "y"})
	}
	return scriptedReply{}
}

// 한 테이블에 이름이 같은 제약 행이 둘 이상이면(같은 이름의 UNIQUE와 FK) 예전 probe는 서버 조인 결과를 이름으로 한 제약에
// 합쳤고, 그 제약의 종류(첫 행)와 컬럼 순서는 서버 조인의 순서를 따랐다. 제약 행과 키 컬럼 행을 따로 읽어 이으면 컬럼
// 순서가 바뀌고, MariaDB 11.4에서는 FK가 UNIQUE가 되어 그래프의 참조 간선까지 사라졌다. 그런 테이블만 예전 조인으로 읽는다.
func TestMySQLConstraintsWithSharedNamesKeepTheServerJoinOrder(t *testing.T) {
	db, server := newScriptedDB(mariadbSharedConstraintName)
	defer closeScriptedDB(t, db)
	h := &harvester{db: db, dialect: "mysql", schemaFilter: map[string]bool{"app": true}}
	objects := h.mysqlObjects("app")
	h.mysqlRelationDetails("app", objects)
	if len(h.limitations) != 0 {
		t.Fatalf("limitations = %q", h.limitations)
	}
	got := map[string][]ConstraintDoc{}
	for _, object := range objects {
		got[object.Name] = object.Constraints
	}
	app := "app"
	want := map[string][]ConstraintDoc{
		"c": {{Name: "dup", Kind: "fk", Columns: []string{"a", "a", "a", "a", "b", "b", "b", "b"},
			Referenced: &ReferencedDoc{Schema: &app, Table: "p", Columns: []string{"x", "y"}}}},
		"plain": {{Name: "fk_plain", Kind: "fk", Columns: []string{"a", "b"},
			Referenced: &ReferencedDoc{Schema: &app, Table: "p", Columns: []string{"x", "y"}}}},
	}
	if !reflect.DeepEqual(got, want) {
		encoded, err := json.Marshal(got)
		if err != nil {
			t.Fatalf("constraints differ and cannot be shown: %v", err)
		}
		t.Fatalf("constraints = %s, want c.dup kind fk with [a a a a b b b b] and plain.fk_plain [a b]", encoded)
	}
	joins := 0
	for _, call := range server.recorded() {
		if strings.Contains(call.query, "LEFT JOIN") {
			joins++
		}
	}
	if joins != 1 {
		t.Fatalf("per-table constraint joins = %d, want one for c", joins)
	}
}
