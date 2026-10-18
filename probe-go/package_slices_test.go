package main

import (
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
