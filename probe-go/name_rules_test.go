package main

import (
	"database/sql/driver"
	"errors"
	"strings"
	"testing"
)

// 서버가 알려 준 대소문자 규칙 원값을 문맥에 싣는다. 판정은 엔진이 한다.
func TestCollectionContextCarriesServerNameRules(t *testing.T) {
	cases := []struct {
		dialect, answerFor string
		answer             driver.Value
		wantLower          *int64
		wantCollation      *string
	}{
		{"mysql", "lower_case_table_names", int64(1), ptrInt64(1), nil},
		{"mysql", "lower_case_table_names", int64(7), nil, nil},
		{"sqlserver", "DATABASEPROPERTYEX", "SQL_Latin1_General_CP1_CI_AS", nil, ptrString("SQL_Latin1_General_CP1_CI_AS")},
		{"postgres", "", nil, nil, nil},
	}
	for _, c := range cases {
		db, _ := newScriptedDB(func(query string, _ []driver.NamedValue) scriptedReply {
			if c.answerFor != "" && strings.Contains(query, c.answerFor) {
				return scriptedRows([]driver.Value{c.answer})
			}
			return scriptedRows([]driver.Value{"app"})
		})
		h := &harvester{db: db, dialect: c.dialect, schemaFilter: map[string]bool{}}
		context := h.collectionContext(true)
		if !equalInt(context.LowerCaseTableNames, c.wantLower) || !equalString(context.Collation, c.wantCollation) {
			t.Errorf("%s: lower=%v collation=%v", c.dialect, context.LowerCaseTableNames, context.Collation)
		}
		db.Close()
	}
}

// 규칙을 읽지 못해도 수집은 계속되고 값만 비운다. 엔진은 그때 정확 비교를 유지한다.
func TestCollectionContextOmitsNameRulesWhenUnreadable(t *testing.T) {
	db, _ := newScriptedDB(func(query string, _ []driver.NamedValue) scriptedReply {
		if strings.Contains(query, "lower_case_table_names") {
			return scriptedReply{err: errors.New("denied")}
		}
		return scriptedRows([]driver.Value{"app"})
	})
	defer db.Close()
	h := &harvester{db: db, dialect: "mysql", schemaFilter: map[string]bool{}}
	context := h.collectionContext(true)
	if context.LowerCaseTableNames != nil || !context.CatalogComplete {
		t.Fatalf("context = %#v", context)
	}
}

func ptrInt64(value int64) *int64 { return &value }

func ptrString(value string) *string { return &value }

func equalInt(a, b *int64) bool { return (a == nil) == (b == nil) && (a == nil || *a == *b) }

func equalString(a, b *string) bool { return (a == nil) == (b == nil) && (a == nil || *a == *b) }
