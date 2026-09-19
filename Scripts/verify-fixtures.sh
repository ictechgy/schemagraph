#!/usr/bin/env bash
# 실제 DB fixture로 양방향 검증한다 — 단위 테스트가 통과한 뒤 도구가 실제로
# 발견하는 결함이 여기서 드러난다(계열 전통).
#
# 사용법:
#   Scripts/verify-fixtures.sh [바이너리 경로]
# 인자가 없으면 target/debug/schemagraph를 쓴다. 먼저 `cargo build`를 돌려라 —
# 릴리스 바이너리가 낡아 있으면 방금 고친 것이 반영되지 않은 결과가 나온다.

set -euo pipefail
cd "$(dirname "$0")/.."

FIX="Fixtures/sqlite"
BIN="${1:-engine/target/debug/schemagraph}"

if [ ! -x "$BIN" ]; then
    echo "error: 바이너리가 없다: $BIN — 먼저 (cd engine && cargo build)" >&2
    exit 2
fi

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

sqlite3 "$tmp/basic.db" < "$FIX/basic.sql"

"$BIN" scan "sqlite:$tmp/basic.db" -o "$tmp/graph.json"

# 골든과 비교 — 출력이 결정적이어야 diff가 성립한다.
if [ -f "$FIX/basic.graph.golden.json" ]; then
    diff -u "$FIX/basic.graph.golden.json" "$tmp/graph.json"
else
    echo "주의: 골든이 없다. 첫 출력을 검토하고 골든으로 고정해라:" >&2
    echo "  cp $tmp/graph.json $FIX/basic.graph.golden.json" >&2
    cp "$tmp/graph.json" "$tmp/graph.review.json"
fi

# 스모크: cycles가 a<->b와 loop_self를 잡고, query가 orders->customers를 보는가.
"$BIN" cycles --graph "$tmp/graph.json" --level object > "$tmp/cycles.json"
grep -q '"main.a"' "$tmp/cycles.json" || { echo "cycles: main.a 순환 미검출" >&2; exit 1; }
grep -q '"main.b"' "$tmp/cycles.json" || { echo "cycles: main.b 순환 미검출" >&2; exit 1; }
grep -q '"selfLoop": true' "$tmp/cycles.json" || { echo "cycles: loop_self 자기루프 미검출" >&2; exit 1; }

"$BIN" query main.orders --graph "$tmp/graph.json" --depth 1 > "$tmp/query.json"
grep -q '"main.customers"' "$tmp/query.json" || { echo "query: orders->customers 미검출" >&2; exit 1; }
grep -q '"main.order_items"' "$tmp/query.json" || { echo "query: order_items->orders 미검출" >&2; exit 1; }

# notFound는 종료 코드 1 + limitations을 실어야 한다.
set +e
"$BIN" query no_such_table --graph "$tmp/graph.json" > "$tmp/notfound.json"
code=$?
set -e
[ "$code" -eq 1 ] || { echo "notFound 종료 코드가 1이 아니다: $code" >&2; exit 1; }
grep -q '"found": false' "$tmp/notfound.json" || { echo "notFound 보고 형식 이상" >&2; exit 1; }

# limitations에 view 미파싱이 실려 있어야 한다(fixture에 view가 있으므로).
grep -q 'view 본문' "$tmp/graph.json" || { echo "limitations: view 미파싱 미보고" >&2; exit 1; }

echo "verify-fixtures: OK"
