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

# P1: view 몸체 파싱으로 reads 간선이 만들어져야 한다.
python3 - "$tmp/graph.json" <<'EOF'
import json, sys
edges = {(e["from"], e["to"]) for e in json.load(open(sys.argv[1]))["edges"] if e["kind"] == "reads"}
want = {
    ("main.order_totals", "main.orders"),
    ("main.order_totals", "main.customers"),
    ("main.order_totals", "main.customers.name"),
}
missing = want - edges
if missing:
    sys.exit(f"reads 간선 미검출: {sorted(missing)}")
EOF

# P1: trigger 몸체 파싱 — UPDATE 대상은 writes, NEW.컬럼은 member reads.
python3 - "$tmp/graph.json" <<'EOF'
import json, sys
edges = {(e["kind"], e["from"], e["to"]) for e in json.load(open(sys.argv[1]))["edges"]}
want = {
    ("writes", "main.orders.trg_orders_touch", "main.customers"),
    ("reads", "main.orders.trg_orders_touch", "main.orders.customer_id"),
}
missing = want - edges
if missing:
    sys.exit(f"trigger 간선 미검출: {sorted(missing)}")
EOF

# P1: impact — customers를 바꾸면 view와 trigger가 전이로 깨진다.
"$BIN" impact main.customers --graph "$tmp/graph.json" > "$tmp/impact.json"
grep -q '"main.order_totals"' "$tmp/impact.json" || { echo "impact: order_totals 미검출" >&2; exit 1; }
grep -q '"main.order_items"' "$tmp/impact.json" || { echo "impact: order_items 전이 미검출" >&2; exit 1; }
grep -q '"reads"' "$tmp/impact.json" || { echo "impact: reads 간선 종류 미보고" >&2; exit 1; }

# P1: dead — 아무도 읽지 않는 order_totals view가 후보여야 하고,
# 삭제 판정 금지 계약이 limitations에 실려야 한다.
"$BIN" dead --graph "$tmp/graph.json" > "$tmp/dead.json"
grep -q '"main.order_totals"' "$tmp/dead.json" || { echo "dead: order_totals 후보 미검출" >&2; exit 1; }
grep -q '"noDependents"' "$tmp/dead.json" || { echo "dead: reason 미보고" >&2; exit 1; }
grep -q 'not safe to delete' "$tmp/dead.json" || { echo "dead: 삭제 금지 계약 누락" >&2; exit 1; }
# standalone 테이블과 trigger는 후보가 아니어야 한다.
! grep -q '"main.standalone"' "$tmp/dead.json" || { echo "dead: 테이블 오탐" >&2; exit 1; }
! grep -q '"main.orders.trg_orders_touch"' "$tmp/dead.json" || { echo "dead: trigger 오탐" >&2; exit 1; }

# ── PostgreSQL ──────────────────────────────────────────────────────────
# 네이티브 리더 검증은 실제 서버가 필요하다. SG_PG_URL이 있으면 그 서버를 쓰고
# (fixture를 새로 적용한다 — 검증 DB를 공유하지 마라), 없으면 로컬 postgres
# 바이너리로 임시 인스턴스를 띄운다. 둘 다 없으면 건너뛴다 — PG가 없는
# 환경에서 전체 검증을 실패시키지 않되, 건너뛴 사실은 출력한다.
PGFIX="Fixtures/postgres"
pg_url=""
pg_own=""

if [ -n "${SG_PG_URL:-}" ]; then
    pg_url="$SG_PG_URL"
    # URL에서 psql용 접속 정보를 그대로 쓴다.
    PGBIN="$(dirname "$(command -v psql 2>/dev/null || echo missing)")"
    if [ ! -x "$PGBIN/psql" ]; then
        echo "주의: SG_PG_URL이 있는데 psql이 없어 PG 검증 건너뜀" >&2
        pg_url=""
    fi
else
    # initdb/postgres/psql 바이너리를 찾는다(PATH → homebrew 순).
    PGBIN=""
    for d in "$(dirname "$(command -v initdb 2>/dev/null || echo missing)")" \
             /opt/homebrew/opt/postgresql@*/bin /opt/homebrew/opt/postgresql/bin \
             /usr/local/opt/postgresql@*/bin /usr/lib/postgresql/*/bin; do
        if [ -x "$d/initdb" ] && [ -x "$d/postgres" ] && [ -x "$d/psql" ]; then
            PGBIN="$d"
            break
        fi
    done
fi

if [ -z "$pg_url" ] && [ -n "$PGBIN" ] && [ -x "$PGBIN/initdb" ]; then
    pg_data="$tmp/pgdata"
    pg_port=$((55400 + RANDOM % 90))
    "$PGBIN/initdb" -D "$pg_data" -U postgres --no-instructions >/dev/null 2>&1
    "$PGBIN/pg_ctl" -D "$pg_data" -l "$tmp/pg.log" -o "-p $pg_port -k $tmp" \
        -w start >/dev/null
    pg_own=1
    trap '"$PGBIN/pg_ctl" -D "$pg_data" -m fast stop >/dev/null 2>&1; rm -rf "$tmp"' EXIT
    "$PGBIN/createdb" -h "$tmp" -p "$pg_port" -U postgres sgfix
    pg_url="postgres://postgres@localhost:$pg_port/sgfix"
    PGHOST_TMP="$tmp"
fi

if [ -n "$pg_url" ]; then
    if [ -n "$pg_own" ]; then
        "$PGBIN/psql" -h "$PGHOST_TMP" -p "$pg_port" -U postgres -d sgfix \
            -v ON_ERROR_STOP=1 -qf "$PGFIX/basic.sql"
    else
        psql "$pg_url" -v ON_ERROR_STOP=1 -qf "$PGFIX/basic.sql"
    fi

    "$BIN" scan "$pg_url" -o "$tmp/graph-pg.json"

    if [ -f "$PGFIX/basic.graph.golden.json" ]; then
        diff -u "$PGFIX/basic.graph.golden.json" "$tmp/graph-pg.json"
    else
        echo "주의: PG 골든이 없다. 첫 출력을 검토하고 골든으로 고정해라:" >&2
        echo "  cp $tmp/graph-pg.json $PGFIX/basic.graph.golden.json" >&2
    fi

    # PG 스모크: a↔b 순환, view member reads, EXECUTE FUNCTION calls,
    # plpgsql 미지원 limitation 보고.
    "$BIN" cycles --graph "$tmp/graph-pg.json" --level object > "$tmp/cycles-pg.json"
    grep -q '"public.a"' "$tmp/cycles-pg.json" || { echo "PG cycles: public.a 미검출" >&2; exit 1; }
    grep -q '"selfLoop": true' "$tmp/cycles-pg.json" || { echo "PG cycles: 자기루프 미검출" >&2; exit 1; }

    python3 - "$tmp/graph-pg.json" <<'EOF'
import json, sys
g = json.load(open(sys.argv[1]))
edges = {(e["kind"], e["from"], e["to"]) for e in g["edges"]}
want = {
    ("calls", "public.orders.trg_orders_touch", "public.trg_orders_touch_fn"),
    ("reads", "public.order_totals", "public.orders.id"),
    ("fires", "public.orders.trg_orders_touch", "public.orders"),
}
missing = want - edges
if missing:
    sys.exit(f"PG 간선 미검출: {sorted(missing)}")
lims = g.get("limitations", [])
if not any("plpgsql" in l for l in lims):
    sys.exit(f"plpgsql 미지원 limitation 미보고: {lims}")
EOF
else
    echo "주의: PostgreSQL을 찾지 못해 PG 검증 건너뜀 (SG_PG_URL로 지정 가능)" >&2
fi

echo "verify-fixtures: OK"
