package main

import (
	"database/sql/driver"
	"encoding/json"
	"os"
	"strings"
	"testing"
)

func TestSlicePackageBodyLexicalIgnoresNonCodeText(t *testing.T) {
	impl := `CREATE OR REPLACE PACKAGE BODY p AS
  -- PROCEDURE Fake IS BEGIN NULL; END Fake;
  /* FUNCTION FakeBlock IS BEGIN NULL; END; */
  v VARCHAR2(100) := 'PROCEDURE FakeString';
  v2 VARCHAR2(100) := q'« FUNCTION FakeQuote IS BEGIN NULL; END; »';
  PROCEDURE "Do_Work" IS
  BEGIN
    NULL;
  END "Do_Work";
END p;`

	got := slicePackageBodyLexical(impl, map[string]bool{"DO_WORK": true})
	body, ok := got["DO_WORK#0"]
	if !ok {
		t.Fatalf("missing quoted member: %#v", got)
	}
	if !strings.Contains(body, `PROCEDURE "Do_Work"`) || strings.Contains(body, "FakeQuote") {
		t.Fatalf("unexpected slice: %q", body)
	}
}

func TestSlicePackageBodyLexicalSkipsNestedCollision(t *testing.T) {
	impl := `PACKAGE BODY p AS
  PROCEDURE outer IS
    PROCEDURE same_name IS
    BEGIN
      NULL;
    END same_name;
  BEGIN
    NULL;
  END outer;
  FUNCTION same_name RETURN NUMBER IS
  BEGIN
    RETURN 1;
  END;
END p;`

	got := slicePackageBodyLexical(impl, map[string]bool{"OUTER": true, "SAME_NAME": true})
	if len(got) != 2 {
		t.Fatalf("got %d slices: %#v", len(got), got)
	}
	if strings.Contains(got["OUTER#0"], "FUNCTION same_name") {
		t.Fatalf("outer slice crossed public boundary: %q", got["OUTER#0"])
	}
	if !strings.Contains(got["OUTER#0"], "PROCEDURE same_name") {
		t.Fatalf("nested local routine was dropped: %q", got["OUTER#0"])
	}
	if !strings.Contains(got["SAME_NAME#0"], "RETURN 1") {
		t.Fatalf("public same-name member was not selected: %q", got["SAME_NAME#0"])
	}
}

func TestSlicePackageBodyLexicalStopsBeforePrivateAndInitialization(t *testing.T) {
	impl := `PACKAGE BODY p AS
  PROCEDURE public_member IS
  BEGIN
    NULL;
  END public_member;
  PROCEDURE private_member IS
  BEGIN
    INSERT INTO audit_log VALUES (1);
  END private_member;
BEGIN
  INSERT INTO package_log VALUES (2);
END p;`

	got := slicePackageBodyLexical(impl, map[string]bool{"PUBLIC_MEMBER": true})
	body := got["PUBLIC_MEMBER#0"]
	if strings.Contains(body, "private_member") || strings.Contains(body, "package_log") {
		t.Fatalf("following private/init code was attributed to public member: %q", body)
	}
	if !strings.HasSuffix(body, "END public_member;") {
		t.Fatalf("slice does not end at the member terminator: %q", body)
	}
}

func TestSlicePackageBodyLexicalOverloadsAndNamedEnds(t *testing.T) {
	impl := `PACKAGE BODY p AS
  PROCEDURE work(a NUMBER) IS
  BEGIN
    NULL;
  END work;
  PROCEDURE work(a VARCHAR2) IS
  BEGIN
    NULL;
  END;
  BEGIN
    NULL;
  END;
END p;`

	got := slicePackageBodyLexical(impl, map[string]bool{"work": true})
	if len(got) != 2 || !strings.Contains(got["work#0"], "a NUMBER") || !strings.Contains(got["work#1"], "a VARCHAR2") {
		t.Fatalf("overload slices mismatch: %#v", got)
	}
}

func TestSlicePackageBodyLexicalUncertainBoundaryIsMissing(t *testing.T) {
	impl := `PACKAGE BODY p AS
  PROCEDURE broken IS
  BEGIN
    NULL;
  -- missing END means the boundary is uncertain
`
	got := slicePackageBodyLexical(impl, map[string]bool{"broken": true})
	if len(got) != 0 {
		t.Fatalf("uncertain routine should be omitted: %#v", got)
	}
}

// 중첩 정의 256단(패키지 멤버 포함)까지는 경계를 찾고, 257단부터는 경계 없음이다.
// Kotlin PackageBodiesTest와 같은 상한이다. 컴파일되는 PL/SQL은 블록 중첩 255단에서 멈춘다.
func TestSlicePackageBodyLexicalNestingDepthLimit(t *testing.T) {
	for _, tc := range []struct {
		depth  int
		slices int
	}{{depth: 256, slices: 1}, {depth: 257, slices: 0}} {
		got := slicePackageBodyLexical(nestedPackageBody(tc.depth), map[string]bool{"M": true})
		if len(got) != tc.slices {
			t.Fatalf("depth %d: got %d slices, want %d", tc.depth, len(got), tc.slices)
		}
	}
}

// nestedPackageBody는 m 안에 m을 depth단 정의하고 안쪽부터 모두 닫은 PACKAGE BODY다.
func nestedPackageBody(depth int) string {
	return "PACKAGE BODY p AS\n" + strings.Repeat("PROCEDURE m IS\n", depth) +
		strings.Repeat("BEGIN NULL; END m;\n", depth) + "END p;"
}

// Oracle 수확 경로가 귀속 규칙을 그대로 쓰는지 본다. 비공개 오버로드가 먼저 정의된 LOG는
// 몸체 없이 limitation을 받고, 경계가 분명한 PING은 제 몸체를 받는다. Kotlin
// PackageBodiesTest의 같은 이름 사례와 같은 기대값이다.
func TestOraclePackageMemberAmbiguityBecomesLimitation(t *testing.T) {
	catalog := oracleFixtureCatalog(0)
	body := "PACKAGE BODY UTIL AS\n" +
		"  PROCEDURE log(msg VARCHAR2, lvl NUMBER) IS BEGIN INSERT INTO debug_log VALUES (msg, lvl); END log;\n" +
		"  PROCEDURE log(msg VARCHAR2) IS BEGIN INSERT INTO audit_log VALUES (msg); END log;\n" +
		"  PROCEDURE ping IS BEGIN NULL; END ping;\n" +
		"END UTIL;"
	catalog.routines = append(catalog.routines,
		[]driver.Value{"APP", "UTIL", "PACKAGE", int64(1), "PACKAGE UTIL AS PROCEDURE log(msg VARCHAR2); PROCEDURE ping; END UTIL;"},
		[]driver.Value{"APP", "UTIL", "PACKAGE BODY", int64(1), body})
	catalog.details["FROM ALL_PROCEDURES"] = [][]driver.Value{{"APP", "UTIL", "LOG", nil}, {"APP", "UTIL", "PING", nil}}
	catalog.details["PACKAGE_NAME IS NOT NULL"] = [][]driver.Value{{"APP", "UTIL", "LOG", "VARCHAR2", int64(1), nil}}
	db, _ := newScriptedDB(catalog.reply)
	defer db.Close()

	doc := (&harvester{db: db, dialect: "oracle", schemaFilter: map[string]bool{}}).extract()

	members := map[string]RoutineDoc{}
	for _, schema := range doc.Schemas {
		for _, routine := range schema.Routines {
			if routine.MemberOf != nil {
				members[routine.Name] = routine
			}
		}
	}
	if body := members["LOG"].Body; body != nil {
		t.Errorf("LOG received a guessed body: %q", *body)
	}
	if body := members["PING"].Body; body == nil || *body != "PROCEDURE ping IS BEGIN NULL; END ping;" {
		t.Errorf("PING body = %v", body)
	}
	want := "APP.UTIL.LOG: package body member boundary ambiguous (2 headers vs 1 catalog members); member body edges are omitted"
	found := false
	for _, limitation := range doc.Limitations {
		found = found || limitation == want
	}
	if !found {
		t.Errorf("missing %q in %q", want, doc.Limitations)
	}
}

// sharedPackageBodyCases는 Kotlin PackageBodiesTest와 함께 읽는 귀속 기대값이다. 두 프로브가 같은
// 본문에서 같은 멤버 몸체와 limitation을 내야 수집기에 따라 그래프가 갈라지지 않는다.
type sharedPackageBodyCases struct {
	Cases []struct {
		Name   string `json:"name"`
		Body   string `json:"body"`
		Repeat *struct {
			Text  string `json:"text"`
			Count int    `json:"count"`
		} `json:"repeat"`
		Members []struct {
			Name      string `json:"name"`
			Function  bool   `json:"function"`
			Signature string `json:"signature"`
		} `json:"members"`
		Expected []struct {
			Body       *string `json:"body"`
			Limitation *string `json:"limitation"`
		} `json:"expected"`
	} `json:"cases"`
}

func TestAttributePackageMembersMatchesSharedCases(t *testing.T) {
	raw, err := os.ReadFile("../Fixtures/oracle-package-bodies.json")
	if err != nil {
		t.Fatal(err)
	}
	var fixture sharedPackageBodyCases
	if err := json.Unmarshal(raw, &fixture); err != nil {
		t.Fatal(err)
	}
	for _, tc := range fixture.Cases {
		impl := tc.Body
		if tc.Repeat != nil {
			impl += strings.Repeat(tc.Repeat.Text, tc.Repeat.Count)
		}
		members := make([]packageMember, 0, len(tc.Members))
		for _, m := range tc.Members {
			members = append(members, packageMember{name: m.Name, function: m.Function, signature: m.Signature})
		}
		got := attributePackageMembers(impl, members)
		if len(got) != len(tc.Expected) {
			t.Fatalf("%s: got %d results for %d expectations", tc.Name, len(got), len(tc.Expected))
		}
		for i, want := range tc.Expected {
			switch {
			case (want.Body == nil) == (want.Limitation == nil):
				t.Fatalf("%s: expectation %d needs exactly one of body or limitation", tc.Name, i)
			case want.Body != nil && (!got[i].found || got[i].body != *want.Body):
				t.Errorf("%s: member %d = %+v, want body %q", tc.Name, i, got[i], *want.Body)
			case want.Limitation != nil && (got[i].found || !strings.Contains(got[i].reason, *want.Limitation)):
				t.Errorf("%s: member %d = %+v, want limitation containing %q", tc.Name, i, got[i], *want.Limitation)
			}
		}
	}
}
