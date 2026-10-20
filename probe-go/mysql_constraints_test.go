package main

import (
	"database/sql/driver"
	"reflect"
	"strings"
	"testing"
)

// mariadbCollationJoin은 제약 조인이 이름을 대소문자 무시로 비교하는 MariaDB다. P와 p는 대소문자만 다른 테이블이고,
// 대소문자만 다른 스키마 APP에도 P가 있다. 세 테이블의 PRIMARY 키 컬럼이 P·p의 PRIMARY 모두와 짝지어진다. 제약을
// 테이블마다 묻는 질의(인자 둘: 스키마, 테이블)와 스키마 단위로 묻는 질의(인자 하나, 맨 앞 열이 제약 테이블)에 모두
// 답해, 수집 방식과 관계없이 같은 서버 응답을 흉내 낸다.
func mariadbCollationJoin(query string, args []driver.NamedValue) scriptedReply {
	switch {
	case strings.Contains(query, "information_schema.schemata"):
		return scriptedRows([]driver.Value{"app"})
	case strings.Contains(query, "FROM information_schema.tables"):
		return scriptedRows([]driver.Value{"P", "BASE TABLE"}, []driver.Value{"p", "BASE TABLE"})
	case strings.Contains(query, "FROM information_schema.table_constraints"):
		keys := [][3]string{{"app", "P", "a"}, {"app", "p", "b"}, {"APP", "P", "c"}}
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
