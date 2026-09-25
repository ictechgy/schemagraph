package main

import "testing"

// PK 순서는 컬럼 선언 순서가 아니라 제약의 키 순서를 따른다 — 엔진 native와 같은 규칙이다.
func TestPrimaryKeyPositionsFollowConstraintKeyOrder(t *testing.T) {
	obj := ObjectDoc{
		Columns:     []ColumnDoc{{Name: "a"}, {Name: "b"}, {Name: "c"}},
		Constraints: []ConstraintDoc{{Name: "t_pkey", Kind: "pk", Columns: []string{"b", "a"}}},
	}
	applyPrimaryKeyPositions(&obj)
	got := []int{obj.Columns[0].PkPosition, obj.Columns[1].PkPosition, obj.Columns[2].PkPosition}
	want := []int{2, 1, 0}
	for i := range want {
		if got[i] != want[i] {
			t.Fatalf("pk positions = %v, want %v", got, want)
		}
	}
}
