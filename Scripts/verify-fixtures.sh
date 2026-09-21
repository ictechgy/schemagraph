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

# 프로브는 한 번 빌드해 전 DB/전송 버전 조합에서 같은 바이너리를 사용한다.
GO_PROBE=""

# CI 실패 뒤에도 실제 그래프를 검토할 수 있게 알려진 fixture 출력만 보관한다.
record_fixture_graph() {
    if [ -n "${SG_FIXTURE_ARTIFACTS:-}" ]; then
        mkdir -p "$SG_FIXTURE_ARTIFACTS"
        cp "$2" "$SG_FIXTURE_ARTIFACTS/$1.graph.json"
    fi
}
if command -v go >/dev/null 2>&1; then
    GO_PROBE="$tmp/schemagraph-probe-go"
    (cd probe-go && CGO_ENABLED=0 go build -o "$GO_PROBE" .)
else
    echo "주의: go가 없어 Go 프로브 전체 검증 건너뜀" >&2
fi

verify_go_versions() {
    if [ -n "$GO_PROBE" ]; then
        python3 Scripts/verify-probe-versions.py "$BIN" "$2" "$tmp/go-$3" \
            -- "$GO_PROBE" --url "$1"
    fi
}

verify_jdbc_versions() {
    local reference="$1" label="$2"
    shift 2
    python3 Scripts/verify-probe-versions.py "$BIN" "$reference" "$tmp/jdbc-$label" \
        -- "$JAVABIN" -jar "$JAR" "$@"
}

# usage 통계는 환경 의존이다(시각·누적 카운트) — 골든 비교 전에 값을
# 정규화해 "usage가 있었다" 사실만 비교한다. 골든도 이 형태로 저장한다.
norm_usage() {
    python3 - "$1" <<'EOF'
import json, sys
g = json.load(open(sys.argv[1]))
for v in g.get("vertices", []):
    if isinstance(v.get("usage"), dict):
        v["usage"] = {k: "<value>" for k in v["usage"]}
print(json.dumps(g, indent=2, sort_keys=True))
EOF
}

sqlite3 "$tmp/basic.db" < "$FIX/basic.sql"

"$BIN" scan "sqlite:$tmp/basic.db" -o "$tmp/graph.json" --emit-document "$tmp/sqlite-document.json"
record_fixture_graph sqlite "$tmp/graph.json"
python3 Scripts/verify-document-versions.py "$BIN" "$tmp/sqlite-document.json" "$tmp/protocol-sqlite"
verify_go_versions "sqlite:$tmp/basic.db" "$tmp/graph.json" sqlite

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

# rules — 의도적 위반 1건이 보고되고 strict가 종료 코드 1을 낸다.
"$BIN" rules --graph "$tmp/graph.json" --config Fixtures/rules.toml > "$tmp/rules.json"
python3 - "$tmp/rules.json" <<'EOF'
import json, sys
r = json.load(open(sys.argv[1]))
assert r["checked"] == 3, f"checked: {r['checked']}"
assert len(r["violations"]) == 1, f"violations: {r['violations']}"
v = r["violations"][0]
assert v["edge"]["from"] == "main.order_items" and v["edge"]["to"] == "main.orders", v
EOF
set +e
"$BIN" rules --graph "$tmp/graph.json" --config Fixtures/rules.toml --strict >/dev/null
code=$?
set -e
[ "$code" -eq 1 ] || { echo "rules --strict 종료 코드가 1이 아니다: $code" >&2; exit 1; }

# --inferred: shipments.customer_id는 선언 FK가 없어 이름 규칙으로 추정된다.
# 기본 스캔에는 없어야 하고, 의존성 질의(impact)에는 섞이면 안 된다.
"$BIN" scan "sqlite:$tmp/basic.db" --inferred -o "$tmp/graph-inf.json"
python3 - "$tmp/graph-inf.json" <<'EOF'
import json, sys
g = json.load(open(sys.argv[1]))
edges = {(e["kind"], e["from"], e["to"]) for e in g["edges"]}
if ("inferred", "main.shipments", "main.customers") not in edges:
    sys.exit("--inferred: shipments->customers 추정 간선 미검출")
# 선언 FK가 있는 customer_id(orders)는 추정하지 않는다.
if any(k == "inferred" and f == "main.orders" for k, f, t in edges):
    sys.exit("--inferred: 선언 FK 컬럼을 추정했다")
EOF
"$BIN" impact main.customers --graph "$tmp/graph-inf.json" > "$tmp/impact-inf.json"
! grep -q '"main.shipments"' "$tmp/impact-inf.json" \
    || { echo "--inferred: inferred 간선이 impact에 섞였다" >&2; exit 1; }

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

    "$BIN" scan "$pg_url" -o "$tmp/graph-pg.json" --emit-document "$tmp/pg-document.json"
    record_fixture_graph postgres "$tmp/graph-pg.json"
    python3 Scripts/verify-dynamic-sql.py "$tmp/graph-pg.json" postgres
    python3 Scripts/verify-document-versions.py "$BIN" "$tmp/pg-document.json" "$tmp/protocol-pg"
    verify_go_versions "$pg_url" "$tmp/graph-pg.json" postgres

    if [ -f "$PGFIX/basic.graph.golden.json" ]; then
        # usage 값(시각·카운트)은 환경 의존 — 정규화 후 비교한다.
        diff -u <(norm_usage "$PGFIX/basic.graph.golden.json") \
                <(norm_usage "$tmp/graph-pg.json")
    else
        echo "주의: PG 골든이 없다. 첫 출력을 검토하고 골든으로 고정해라:" >&2
        echo "  norm_usage $tmp/graph-pg.json > $PGFIX/basic.graph.golden.json" >&2
    fi

    # P3: stats — pg_stat 사용량이 since와 함께 수확돼야 한다.
    "$BIN" stats --graph "$tmp/graph-pg.json" > "$tmp/stats-pg.json"
    python3 - "$tmp/stats-pg.json" <<'EOF'
import json, sys
r = json.load(open(sys.argv[1]))
stats = {s["id"]: s for s in r["stats"]}
assert r["totals"]["observed"] >= 1, f"관측 정점 없음: {r}"
orders = stats.get("public.orders")
assert orders is not None, f"public.orders usage 없음: {sorted(stats)}"
assert isinstance(orders["reads"], int) and orders["reads"] >= 0, orders
assert "since" in orders, f"since 없음 — 유효 구간을 알 수 없다: {orders}"
EOF

    # PG 스모크: a↔b 순환, view member reads, EXECUTE FUNCTION calls,
    # plpgsql 몸체 추출 간선.
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
    # plpgsql 몸체의 UPDATE는 문장 추출로 writes 간선이 돼야 한다.
    ("writes", "public.trg_orders_touch_fn", "public.customers"),
}
missing = want - edges
if missing:
    sys.exit(f"PG 간선 미검출: {sorted(missing)}")
EOF

    # rules — 같은 규칙 파일이 PG 그래프에서도 의도적 위반을 잡아야 한다.
    "$BIN" rules --graph "$tmp/graph-pg.json" --config Fixtures/rules.toml \
        | grep -q '"public.order_items"' \
        || { echo "PG rules: 의도적 위반 미검출" >&2; exit 1; }
else
    echo "주의: PostgreSQL을 찾지 못해 PG 검증 건너뜀 (SG_PG_URL로 지정 가능)" >&2
fi

# ── MySQL ───────────────────────────────────────────────────────────────
# SG_MYSQL_URL이 있으면 그 서버를 쓴다 — fixture를 새로 적용하니 폐기용
# DB만 지정해라. 적용은 SG_MYSQL_CONTAINER가 있으면 docker exec로,
# 없으면 로컬 mysql 클라이언트로 한다. URL이 없으면 docker로 임시
# 인스턴스를 띄운다. 둘 다 없으면 건너뛰고 안내한다.
MYFIX="Fixtures/mysql"
my_url=""
my_container=""
my_own=""

if [ -n "${SG_MYSQL_URL:-}" ]; then
    my_url="$SG_MYSQL_URL"
    my_container="${SG_MYSQL_CONTAINER:-}"
    if [ -z "$my_container" ] && ! command -v mysql >/dev/null; then
        echo "주의: SG_MYSQL_URL이 있는데 mysql 클라이언트가 없어 MySQL 검증 건너뜀" >&2
        echo "  (SG_MYSQL_CONTAINER로 서버 컨테이너를 지정하면 docker exec로 적용한다)" >&2
        my_url=""
    fi
elif command -v docker >/dev/null && docker info >/dev/null 2>&1; then
    my_container="sg-verify-mysql-$$"
    my_port=$((33400 + RANDOM % 90))
    docker run -d --name "$my_container" \
        -e MYSQL_ALLOW_EMPTY_PASSWORD=1 -e MYSQL_DATABASE=sgfix \
        -p "$my_port":3306 mysql:8.4 >/dev/null
    my_own=1
    trap '[ -n "${pg_own:-}" ] && "${PGBIN:-true}/pg_ctl" -D "${pg_data:-}" -m fast stop >/dev/null 2>&1; [ -n "${my_own:-}" ] && docker rm -f "${my_container:-}" >/dev/null 2>&1; rm -rf "$tmp"' EXIT
    # 초기화 서버는 소켓만 열기 때문에 최종 서버의 TCP와 DB를 함께 확인한다.
    python3 Scripts/wait-mysql-ready.py "$my_container" mysql
    docker exec -i "$my_container" mysql -uroot sgfix < "$MYFIX/basic.sql"
    my_url="mysql://root@localhost:$my_port/sgfix"
fi

if [ -n "$my_url" ]; then
    if [ -z "$my_own" ]; then
        # 외부 서버 — fixture를 새로 적용한다(폐기용 DB 계약).
        if [ -n "$my_container" ]; then
            docker exec -i "$my_container" mysql -uroot sgfix < "$MYFIX/basic.sql"
        else
            # mysql://user[:pass]@host[:port]/db 를 클라이언트 인자로 푼다.
            if [[ "$my_url" =~ mysql://([^:/@]+)(:([^@]*))?@([^:/]+)(:([0-9]+))?/([^?]+) ]]; then
                MYSQL_PWD="${BASH_REMATCH[3]}" mysql -h "${BASH_REMATCH[4]}" \
                    -P "${BASH_REMATCH[6]:-3306}" -u "${BASH_REMATCH[1]}" \
                    "${BASH_REMATCH[7]}" < "$MYFIX/basic.sql"
            else
                echo "주의: SG_MYSQL_URL 형식을 못 풀어 fixture 적용 건너뜀" >&2
            fi
        fi
    fi

    "$BIN" scan "$my_url" -o "$tmp/graph-my.json"
    record_fixture_graph mysql "$tmp/graph-my.json"
    verify_go_versions "$my_url" "$tmp/graph-my.json" mysql

    if [ -f "$MYFIX/basic.graph.golden.json" ]; then
        # usage 값(시각·카운트)은 환경 의존 — 정규화 후 비교한다.
        diff -u <(norm_usage "$MYFIX/basic.graph.golden.json") \
                <(norm_usage "$tmp/graph-my.json")
    else
        echo "주의: MySQL 골든이 없다. 첫 출력을 검토하고 골든으로 고정해라:" >&2
        echo "  norm_usage $tmp/graph-my.json > $MYFIX/basic.graph.golden.json" >&2
    fi

    # P3: stats — sys 스키마 통계가 since와 함께 수확돼야 한다.
    "$BIN" stats --graph "$tmp/graph-my.json" > "$tmp/stats-my.json"
    python3 - "$tmp/stats-my.json" <<'EOF'
import json, sys
r = json.load(open(sys.argv[1]))
stats = {s["id"]: s for s in r["stats"]}
assert r["totals"]["observed"] >= 1, f"관측 정점 없음: {r}"
orders = stats.get("sgfix.orders")
assert orders is not None, f"sgfix.orders usage 없음: {sorted(stats)}"
assert isinstance(orders["reads"], int) and orders["reads"] >= 0, orders
EOF

    # MySQL 스모크: a↔b 순환, view member reads, trigger writes+fires,
    # procedure의 writes.
    "$BIN" cycles --graph "$tmp/graph-my.json" --level object > "$tmp/cycles-my.json"
    grep -q '"sgfix.a"' "$tmp/cycles-my.json" || { echo "MySQL cycles: sgfix.a 미검출" >&2; exit 1; }
    grep -q '"selfLoop": true' "$tmp/cycles-my.json" || { echo "MySQL cycles: 자기루프 미검출" >&2; exit 1; }

    python3 - "$tmp/graph-my.json" <<'EOF'
import json, sys
g = json.load(open(sys.argv[1]))
edges = {(e["kind"], e["from"], e["to"]) for e in g["edges"]}
want = {
    ("writes", "sgfix.orders.trg_orders_touch", "sgfix.customers"),
    ("reads", "sgfix.orders.trg_orders_touch", "sgfix.orders.customer_id"),
    ("fires", "sgfix.orders.trg_orders_touch", "sgfix.orders"),
    ("writes", "sgfix.touch_customer(int)", "sgfix.customers"),
    ("reads", "sgfix.order_totals", "sgfix.orders.id"),
}
missing = want - edges
if missing:
    sys.exit(f"MySQL 간선 미검출: {sorted(missing)}")
EOF

    # rules — 같은 규칙 파일이 MySQL 그래프에서도 의도적 위반을 잡아야 한다.
    "$BIN" rules --graph "$tmp/graph-my.json" --config Fixtures/rules.toml \
        | grep -q '"sgfix.order_items"' \
        || { echo "MySQL rules: 의도적 위반 미검출" >&2; exit 1; }
else
    echo "주의: MySQL을 찾지 못해 MySQL 검증 건너뜀 (SG_MYSQL_URL 또는 docker)" >&2
fi

# ── MariaDB ─────────────────────────────────────────────────────────────
# MySQL 리더의 MariaDB 호환 검증 — 같은 fixture를 쓰고, 온전한 스캔의 그래프는
# MySQL 골든과 동일해야 한다. SG_MARIADB_URL이 있으면 그 서버를 쓰고
# (fixture를 새로 적용한다 — 폐기용 DB만), 없으면 docker로 띄운다.
# MariaDB는 performance_schema=OFF가 기본값이라, 소유 컨테이너는 두 단계로
# 띄워 OFF limitation 경로와 ON 수확 경로를 둘 다 본다(런타임 전환 불가라
# 컨테이너 재생성이 필요하다).
maria_url=""
maria_container=""
maria_own=""
maria_port=""

maria_up() {  # $*=추가 mysqld 옵션
    docker rm -f "$maria_container" >/dev/null 2>&1 || true
    docker run -d --name "$maria_container" \
        -e MARIADB_ALLOW_EMPTY_ROOT_PASSWORD=1 -e MARIADB_DATABASE=sgfix \
        -p "$maria_port":3306 mariadb:11.4 "$@" >/dev/null
    python3 Scripts/wait-mysql-ready.py "$maria_container" mariadb
    docker exec -i "$maria_container" mariadb -uroot sgfix < "$MYFIX/basic.sql"
}

if [ -n "${SG_MARIADB_URL:-}" ]; then
    maria_url="$SG_MARIADB_URL"
    if ! command -v mysql >/dev/null && ! command -v mariadb >/dev/null; then
        echo "주의: SG_MARIADB_URL이 있는데 mysql/mariadb 클라이언트가 없어 MariaDB 검증 건너뜀" >&2
        maria_url=""
    fi
elif command -v docker >/dev/null && docker info >/dev/null 2>&1; then
    maria_container="sg-verify-mariadb-$$"
    maria_port=$((33500 + RANDOM % 90))
    maria_own=1
    trap '[ -n "${pg_own:-}" ] && "${PGBIN:-true}/pg_ctl" -D "${pg_data:-}" -m fast stop >/dev/null 2>&1; [ -n "${my_own:-}" ] && docker rm -f "${my_container:-}" >/dev/null 2>&1; [ -n "${maria_own:-}" ] && docker rm -f "${maria_container:-}" >/dev/null 2>&1; rm -rf "$tmp"' EXIT

    # 1단계: 기본값(PFS OFF) — 통계 미수집이 limitation으로 보고돼야 하고
    # usage가 붙은 정점이 없어야 한다(0행을 관측된 0으로 읽지 않는 계약).
    maria_up
    maria_url="mysql://root@localhost:$maria_port/sgfix"
    "$BIN" scan "$maria_url" -o "$tmp/graph-maria-off.json"
    verify_go_versions "$maria_url" "$tmp/graph-maria-off.json" mariadb-off
    python3 - "$tmp/graph-maria-off.json" <<'EOF'
import json, sys
g = json.load(open(sys.argv[1]))
lims = g.get("limitations", [])
if not any("performance_schema=OFF" in l for l in lims):
    sys.exit(f"MariaDB: performance_schema=OFF limitation 미보고: {lims}")
used = [v["id"] for v in g["vertices"] if v.get("usage")]
if used:
    sys.exit(f"MariaDB: PFS OFF인데 usage 부착: {used[:5]}")
EOF

    # 2단계: PFS ON — 수확 경로까지 검증한다.
    maria_up --performance-schema=ON
else
    :
fi

if [ -n "$maria_url" ]; then
    if [ -z "$maria_own" ]; then
        # 외부 서버 — fixture를 새로 적용한다(폐기용 DB 계약). mysql과 같은
        # 와이어 프로토콜이라 같은 클라이언트로 적용한다.
        if [[ "$maria_url" =~ mysql://([^:/@]+)(:([^@]*))?@([^:/]+)(:([0-9]+))?/([^?]+) ]]; then
            MARIA_CLI="$(command -v mariadb || command -v mysql)"
            MYSQL_PWD="${BASH_REMATCH[3]}" "$MARIA_CLI" -h "${BASH_REMATCH[4]}" \
                -P "${BASH_REMATCH[6]:-3306}" -u "${BASH_REMATCH[1]}" \
                "${BASH_REMATCH[7]}" < "$MYFIX/basic.sql"
        else
            echo "주의: SG_MARIADB_URL 형식을 못 풀어 fixture 적용 건너뜀" >&2
        fi
    fi

    "$BIN" scan "$maria_url" -o "$tmp/graph-maria.json"
    record_fixture_graph mariadb "$tmp/graph-maria.json"
    verify_go_versions "$maria_url" "$tmp/graph-maria.json" mariadb-on

    # 골든은 MariaDB 전용이다 — MySQL과 구조는 같아도 시그니처 표기
    # (int(11))와 sys 뷰 범위가 다르다.
    MARIAFIX="Fixtures/mariadb"
    if [ -f "$MARIAFIX/basic.graph.golden.json" ]; then
        # usage 값(시각·카운트)은 환경 의존 — 정규화 후 비교한다.
        diff -u <(norm_usage "$MARIAFIX/basic.graph.golden.json") \
                <(norm_usage "$tmp/graph-maria.json")
    else
        echo "주의: MariaDB 골든이 없다. 첫 출력을 검토하고 골든으로 고정해라:" >&2
        echo "  norm_usage $tmp/graph-maria.json > $MARIAFIX/basic.graph.golden.json" >&2
    fi

    python3 - "$tmp/graph-maria.json" <<'EOF'
import json, sys
g = json.load(open(sys.argv[1]))
edges = {(e["kind"], e["from"], e["to"]) for e in g["edges"]}
want = {
    ("writes", "sgfix.orders.trg_orders_touch", "sgfix.customers"),
    ("fires", "sgfix.orders.trg_orders_touch", "sgfix.orders"),
    ("reads", "sgfix.order_totals", "sgfix.orders"),
}
missing = want - edges
if missing:
    sys.exit(f"MariaDB 간선 미검출: {sorted(missing)}")
# routine 시그니처는 방언 표기를 따른다 — MariaDB는 int(11)이라
# 정확한 id 대신 접두로 본다.
if not any(k == "writes" and f.startswith("sgfix.touch_customer(")
           and t == "sgfix.customers" for k, f, t in edges):
    sys.exit("MariaDB: touch_customer writes 간선 미검출")
verts = {v["id"] for v in g["vertices"]}
if "sgfix.order_items.order_id@index" not in verts:
    sys.exit("MariaDB: 충돌 분리 정점 order_id@index 없음")
lims = g.get("limitations", [])
pfs_off = any("performance_schema=OFF" in l for l in lims)
used = {v["id"] for v in g["vertices"] if v.get("usage")}
if pfs_off:
    # 외부 서버가 OFF면 미수집이 정직한 결과다 — usage가 없어야 한다.
    if used:
        sys.exit(f"MariaDB: PFS OFF인데 usage 부착: {sorted(used)[:5]}")
elif "sgfix.orders" not in used:
    sys.exit(f"MariaDB: PFS ON인데 sgfix.orders usage 없음: {sorted(used)[:10]}")
EOF
else
    echo "주의: MariaDB를 찾지 못해 MariaDB 검증 건너뜀 (SG_MARIADB_URL 또는 docker)" >&2
fi

# ── JDBC probe ──────────────────────────────────────────────────────────
# probe는 java + fat jar이 필요하다. macOS /usr/bin/java는 스텁이라 실기동을
# 확인하고, 없으면 homebrew openjdk를 본다. jar이 없고 gradle이 있으면 빌드.
JAVABIN=""
for j in "$(command -v java 2>/dev/null || true)" \
         /opt/homebrew/opt/openjdk/bin/java \
         /usr/local/opt/openjdk/bin/java \
         "$HOME/.sdkman/candidates/java/current/bin/java"; do
    if [ -n "$j" ] && [ -x "$j" ] && "$j" -version >/dev/null 2>&1; then
        JAVABIN="$j"
        break
    fi
done

JAR="${SG_PROBE_JAR:-probe/build/libs/schemagraph-probe-all.jar}"
if [ -n "${SG_PROBE_JAR:-}" ] && [ ! -f "$JAR" ]; then
    echo "error: SG_PROBE_JAR does not point to an existing probe jar" >&2
    exit 1
fi
if [ -n "$JAVABIN" ] && [ ! -f "$JAR" ] && command -v gradle >/dev/null; then
    JAVA_HOME="$(cd "$(dirname "$JAVABIN")/.." && pwd)" \
        gradle -p probe shadowJar --console=plain -q \
        || echo "주의: probe 빌드 실패 — probe 검증 건너뜀" >&2
fi

if [ -n "$JAVABIN" ] && [ -f "$JAR" ]; then
    # H2 — 번들 드라이버 + 임베디드라 서버 없이 document→graph end-to-end가 돈다.
    "$JAVABIN" -jar "$JAR" \
        --url "jdbc:h2:file:$tmp/sgfix-h2;INIT=RUNSCRIPT FROM '$PWD/Fixtures/h2/basic.sql'" \
        -o "$tmp/probe-doc.json"
    "$BIN" scan --document "$tmp/probe-doc.json" -o "$tmp/probe-graph.json"
    verify_jdbc_versions "$tmp/probe-graph.json" h2 --url "jdbc:h2:file:$tmp/sgfix-h2"

    "$BIN" cycles --graph "$tmp/probe-graph.json" > "$tmp/cycles-h2.json"
    grep -q '"PUBLIC.LOOP_A"' "$tmp/cycles-h2.json" \
        || { echo "probe(H2): LOOP_A 순환 미검출" >&2; exit 1; }
    grep -q '"selfLoop": true' "$tmp/cycles-h2.json" \
        || { echo "probe(H2): 자기루프 미검출" >&2; exit 1; }

    python3 - "$tmp/probe-graph.json" <<'EOF'
import json, sys
g = json.load(open(sys.argv[1]))
edges = {(e["kind"], e["from"], e["to"]) for e in g["edges"]}
want = {
    ("references", "PUBLIC.ORDERS", "PUBLIC.CUSTOMERS"),
    ("reads", "PUBLIC.ORDER_TOTALS", "PUBLIC.ORDERS"),
    ("reads", "PUBLIC.ORDER_TOTALS", "PUBLIC.CUSTOMERS.ID"),
}
missing = want - edges
if missing:
    sys.exit(f"probe(H2) 간선 미검출: {sorted(missing)}")
EOF

    # SQLite — sqlite-jdbc도 번들이다. 스키마 귀속(TABLE_SCHEM이 null → main)과
    # sqlite_master 몸체 수확이 네이티브와 같은 간선을 내야 한다.
    sqlite3 "$tmp/probe-sqlite.db" < "$FIX/basic.sql"
    "$JAVABIN" -jar "$JAR" --url "jdbc:sqlite:$tmp/probe-sqlite.db" \
        -o "$tmp/probe-sqlite-doc.json"
    "$BIN" scan --document "$tmp/probe-sqlite-doc.json" -o "$tmp/probe-sqlite-graph.json"
    verify_jdbc_versions "$tmp/probe-sqlite-graph.json" sqlite --url "jdbc:sqlite:$tmp/probe-sqlite.db"
    python3 - "$tmp/graph.json" "$tmp/probe-sqlite-graph.json" <<'EOF'
import json, sys
def load(p):
    g = json.load(open(p))
    return ({v["id"] for v in g["vertices"]},
            {(e["kind"], e["from"], e["to"]) for e in g["edges"]})
nv, ne = load(sys.argv[1])
pv, pe = load(sys.argv[2])
if nv != pv or ne != pe:
    sys.exit(f"probe(SQLite) 패리티 불일치 — verts only-native: {sorted(nv-pv)} "
             f"only-probe: {sorted(pv-nv)} | edges only-native: {sorted(ne-pe)} "
             f"only-probe: {sorted(pe-ne)}")
EOF

    # NDJSON 전송 — 같은 DB를 --format ndjson으로 긁으면 엔진이 행 단위
    # document를 자동 감지해 같은 그래프를 내야 한다.
    "$JAVABIN" -jar "$JAR" --url "jdbc:sqlite:$tmp/probe-sqlite.db" \
        -o "$tmp/probe-sqlite-doc.ndjson" --format ndjson
    "$BIN" scan --document "$tmp/probe-sqlite-doc.ndjson" -o "$tmp/probe-sqlite-ndjson-graph.json"
    python3 - "$tmp/graph.json" "$tmp/probe-sqlite-ndjson-graph.json" <<'EOF'
import json, sys
def load(p):
    g = json.load(open(p))
    return ({v["id"] for v in g["vertices"]},
            {(e["kind"], e["from"], e["to"]) for e in g["edges"]})
nv, ne = load(sys.argv[1])
pv, pe = load(sys.argv[2])
if nv != pv or ne != pe:
    sys.exit(f"probe(SQLite/NDJSON) 패리티 불일치 — verts diff: {sorted(nv^pv)} "
             f"edges diff: {sorted(ne^pe)}")
EOF

    # PG parity — pgjdbc가 번들이라 PG가 떠 있으면 같은 DB를 JDBC로도 읽어
    # 네이티브와 같은 판정 간선이 나오는지 확인한다.
    if [ -n "${pg_url:-}" ]; then
        if [[ "$pg_url" =~ (postgres|postgresql)://([^:/@]+)(:([^@]*))?@([^:/]+)(:([0-9]+))?/([^?]+) ]]; then
            jdbc_pg="jdbc:postgresql://${BASH_REMATCH[5]}:${BASH_REMATCH[7]:-5432}/${BASH_REMATCH[8]}"
            probe_pg_args=(--url "$jdbc_pg" --user "${BASH_REMATCH[2]}")
            [ -n "${BASH_REMATCH[4]:-}" ] && probe_pg_args+=(--password "${BASH_REMATCH[4]}")
            "$JAVABIN" -jar "$JAR" "${probe_pg_args[@]}" -o "$tmp/probe-pg-doc.json"
            "$BIN" scan --document "$tmp/probe-pg-doc.json" -o "$tmp/probe-pg-graph.json"
            verify_jdbc_versions "$tmp/probe-pg-graph.json" postgres "${probe_pg_args[@]}"
            python3 Scripts/verify-dynamic-sql.py "$tmp/probe-pg-graph.json" postgres
            python3 - "$tmp/probe-pg-graph.json" <<'EOF'
import json, sys
g = json.load(open(sys.argv[1]))
edges = {(e["kind"], e["from"], e["to"]) for e in g["edges"]}
want = {
    ("calls", "public.orders.trg_orders_touch", "public.trg_orders_touch_fn"),
    ("fires", "public.orders.trg_orders_touch", "public.orders"),
    ("reads", "public.order_totals", "public.orders"),
}
missing = want - edges
if missing:
    sys.exit(f"probe(PG) 간선 미검출: {sorted(missing)}")
EOF
        else
            echo "주의: SG_PG_URL 형식을 못 풀어 probe-PG 패리티 건너뜀" >&2
        fi
    fi

    # MySQL probe — 드라이버는 GPL이라 번들하지 않는다. SG_MYSQL_JAR로 jar을
    # 지정하면 --driver 경로와 MySQL 수확을 함께 검증한다.
    if [ -n "${my_url:-}" ] && [ -n "${SG_MYSQL_JAR:-}" ] && [ -f "${SG_MYSQL_JAR:-}" ]; then
        if [[ "$my_url" =~ mysql://([^:/@]+)(:([^@]*))?@([^:/]+)(:([0-9]+))?/([^?]+) ]]; then
            jdbc_my="jdbc:mysql://${BASH_REMATCH[4]}:${BASH_REMATCH[6]:-3306}/${BASH_REMATCH[7]}"
            probe_mysql_args=(--url "$jdbc_my" --user "${BASH_REMATCH[1]}" --driver "$SG_MYSQL_JAR")
            [ -n "${BASH_REMATCH[3]:-}" ] && probe_mysql_args+=(--password "${BASH_REMATCH[3]}")
            "$JAVABIN" -jar "$JAR" "${probe_mysql_args[@]}" -o "$tmp/probe-my-doc.json"
            "$BIN" scan --document "$tmp/probe-my-doc.json" -o "$tmp/probe-my-graph.json"
            verify_jdbc_versions "$tmp/probe-my-graph.json" mysql "${probe_mysql_args[@]}"
            python3 - "$tmp/probe-my-graph.json" <<'EOF'
import json, sys
g = json.load(open(sys.argv[1]))
edges = {(e["kind"], e["from"], e["to"]) for e in g["edges"]}
want = {
    ("references", "sgfix.order_items", "sgfix.orders"),
    ("fires", "sgfix.orders.trg_orders_touch", "sgfix.orders"),
    ("writes", "sgfix.touch_customer(int)", "sgfix.customers"),
    ("reads", "sgfix.order_totals", "sgfix.orders"),
}
missing = want - edges
if missing:
    sys.exit(f"probe(MySQL) 간선 미검출: {sorted(missing)}")
EOF
        fi
    fi

    # MariaDB probe — mysql 드라이버로 같은 와이어 프로토콜을 탄다.
    if [ -n "${maria_url:-}" ] && [ -n "${SG_MYSQL_JAR:-}" ] && [ -f "${SG_MYSQL_JAR:-}" ]; then
        if [[ "$maria_url" =~ mysql://([^:/@]+)(:([^@]*))?@([^:/]+)(:([0-9]+))?/([^?]+) ]]; then
            jdbc_maria="jdbc:mysql://${BASH_REMATCH[4]}:${BASH_REMATCH[6]:-3306}/${BASH_REMATCH[7]}"
            probe_maria_args=(--url "$jdbc_maria" --user "${BASH_REMATCH[1]}" --driver "$SG_MYSQL_JAR")
            [ -n "${BASH_REMATCH[3]:-}" ] && probe_maria_args+=(--password "${BASH_REMATCH[3]}")
            "$JAVABIN" -jar "$JAR" "${probe_maria_args[@]}" -o "$tmp/probe-maria-doc.json"
            "$BIN" scan --document "$tmp/probe-maria-doc.json" -o "$tmp/probe-maria-graph.json"
            verify_jdbc_versions "$tmp/probe-maria-graph.json" mariadb "${probe_maria_args[@]}"
            python3 - "$tmp/probe-maria-graph.json" <<'EOF'
import json, sys
g = json.load(open(sys.argv[1]))
edges = {(e["kind"], e["from"], e["to"]) for e in g["edges"]}
want = {
    ("references", "sgfix.order_items", "sgfix.orders"),
    ("fires", "sgfix.orders.trg_orders_touch", "sgfix.orders"),
}
missing = want - edges
if missing:
    sys.exit(f"probe(MariaDB) 간선 미검출: {sorted(missing)}")
# MariaDB는 파라미터 표기가 int(11) — 정확한 id 대신 접두로 본다.
if not any(k == "writes" and f.startswith("sgfix.touch_customer(")
           and t == "sgfix.customers" for k, f, t in edges):
    sys.exit("probe(MariaDB): touch_customer writes 간선 미검출")
EOF
        fi
    fi

    # mysql/mariadb/PG가 필요한 구간은 여기까지다 — 소유 리소스를 여기서
    # 내린다. MSSQL·Oracle과 동시 기동하면 4GiB급 docker VM에서 OOM-kill이
    # 난다(실제로 oracle 컨테이너가 OOMKilled로 죽은 적이 있다).
    if [ -n "${my_own:-}" ]; then
        docker rm -f "$my_container" >/dev/null 2>&1 || true
        my_own=""
    fi
    if [ -n "${maria_own:-}" ]; then
        docker rm -f "$maria_container" >/dev/null 2>&1 || true
        maria_own=""
    fi
    if [ -n "${pg_own:-}" ]; then
        "${PGBIN:-true}/pg_ctl" -D "${pg_data:-}" -m fast stop >/dev/null 2>&1 || true
        pg_own=""
    fi

    # ── MSSQL probe ─────────────────────────────────────────────────────
    # 네이티브 리더가 없어 프로브가 유일한 경로다 — mssql-jdbc는 MIT라 번들.
    # SG_MSSQL_URL(jdbc:sqlserver://…;databaseName=<폐기용DB>)이 있으면 그
    # 서버에 fixture를 적용해 쓰고, 없으면 docker로 Azure SQL Edge를 띄운다
    # (arm64/amd64 모두 돈다 — 공식 SQL Server 이미지는 ARM QEMU에서 죽는다).
    # fixture 적용은 sqlcmd가 없는 이미지라 Scripts/ApplySql.java로 한다.
    mssql_jdbc=""
    mssql_container=""
    ms_user="${SG_MSSQL_USER:-sa}"
    ms_pass="${SG_MSSQL_PASSWORD:-Strong!Passw0rd}"

    if [ -n "${SG_MSSQL_URL:-}" ]; then
        mssql_jdbc="$SG_MSSQL_URL"
    elif command -v docker >/dev/null && docker info >/dev/null 2>&1; then
        mssql_container="sg-verify-mssql-$$"
        ms_port=$((33600 + RANDOM % 90))
        docker run -d --name "$mssql_container" \
            -e ACCEPT_EULA=Y -e MSSQL_SA_PASSWORD="$ms_pass" \
            -p "$ms_port":1433 mcr.microsoft.com/azure-sql-edge:latest >/dev/null
        trap '[ -n "${pg_own:-}" ] && "${PGBIN:-true}/pg_ctl" -D "${pg_data:-}" -m fast stop >/dev/null 2>&1; [ -n "${my_own:-}" ] && docker rm -f "${my_container:-}" >/dev/null 2>&1; [ -n "${maria_own:-}" ] && docker rm -f "${maria_container:-}" >/dev/null 2>&1; docker rm -f "${mssql_container:-}" >/dev/null 2>&1; rm -rf "$tmp"' EXIT

        # 기동 대기 — sqlcmd가 없어 SELECT 1 한 장으로 tcp가 열릴 때까지 본다.
        # ApplySql은 ServiceLoader로 드라이버를 찾으므로 fat jar을 -cp로 준다
        # (프로브 자체는 BUNDLED_DRIVERS 명시 로딩이라 -jar만으로 동작한다).
        # Azure SQL Edge는 arm64에서도 수 분 걸릴 수 있어 여유를 둔다.
        echo "SELECT 1" > "$tmp/ms-ping.sql"
        ms_ready=""
        for _ in $(seq 1 120); do
            if "$JAVABIN" -cp "$JAR" "$PWD/Scripts/ApplySql.java" \
                "jdbc:sqlserver://localhost:$ms_port;encrypt=false" \
                "$ms_user" "$ms_pass" "$tmp/ms-ping.sql" >/dev/null 2>&1; then
                ms_ready=1
                break
            fi
            sleep 3
        done
        if [ -z "$ms_ready" ]; then
            echo "주의: MSSQL 컨테이너가 기동하지 않아 probe-MSSQL 검증 건너뜀" >&2
        else
            # 폐기용 DB를 만들고 fixture를 적용한다.
            echo "IF DB_ID('sgfix') IS NULL CREATE DATABASE sgfix;" > "$tmp/ms-ddl.sql"
            "$JAVABIN" -cp "$JAR" "$PWD/Scripts/ApplySql.java" \
                "jdbc:sqlserver://localhost:$ms_port;encrypt=false" \
                "$ms_user" "$ms_pass" "$tmp/ms-ddl.sql"
            mssql_jdbc="jdbc:sqlserver://localhost:$ms_port;databaseName=sgfix;encrypt=false"
        fi
    fi

    if [ -n "$mssql_jdbc" ]; then
        # 외부 서버든 소유 컨테이너든 대상 DB에 fixture를 적용한다(폐기용 DB
        # 계약 — 재실행하면 CREATE가 충돌해 실패하니 검증 전용 서버만 지정).
        "$JAVABIN" -cp "$JAR" "$PWD/Scripts/ApplySql.java" \
            "$mssql_jdbc" "$ms_user" "$ms_pass" "$PWD/Fixtures/mssql/basic.sql"

        "$JAVABIN" -jar "$JAR" --url "$mssql_jdbc" \
            --user "$ms_user" --password "$ms_pass" -o "$tmp/probe-ms-doc.json"
        "$BIN" scan --document "$tmp/probe-ms-doc.json" -o "$tmp/probe-ms-graph.json"
        verify_jdbc_versions "$tmp/probe-ms-graph.json" sqlserver --url "$mssql_jdbc" --user "$ms_user" --password "$ms_pass"
        python3 Scripts/verify-dynamic-sql.py "$tmp/probe-ms-graph.json" sqlserver
        python3 - "$tmp/probe-ms-graph.json" <<'EOF'
import json, sys
g = json.load(open(sys.argv[1]))
edges = {(e["kind"], e["from"], e["to"]) for e in g["edges"]}
want = {
    ("references", "dbo.order_items", "dbo.orders"),
    ("fires", "dbo.orders.trg_orders_touch", "dbo.orders"),
    ("writes", "dbo.orders.trg_orders_touch", "dbo.customers"),
    ("writes", "dbo.touch_customer(int)", "dbo.customers"),
    # T-SQL 절차형 구문 — IF/TRY/CATCH/SET @v=/EXEC/WHILE/CURSOR 안의 문장.
    ("reads", "dbo.touch_customer(int)", "dbo.customers"),
    ("writes", "dbo.touch_customer(int)", "dbo.tickets"),
    ("calls", "dbo.touch_customer(int)", "dbo.audit_orders(int)"),
    ("writes", "dbo.audit_orders(int)", "dbo.tickets"),
    ("reads", "dbo.drain_orders", "dbo.customers"),
    ("writes", "dbo.drain_orders", "dbo.orders"),
    ("reads", "dbo.order_totals", "dbo.orders"),
    ("reads", "dbo.order_count", "dbo.orders"),
}
missing = want - edges
if missing:
    sys.exit(f"probe(MSSQL) 간선 미검출: {sorted(missing)}")
# numbered procedure 접미사(;0/;1)는 정규화돼야 한다 — 유령 routine 금지.
ghosts = [v["id"] for v in g["vertices"] if ";" in v["id"]]
if ghosts:
    sys.exit(f"probe(MSSQL): ;N 유령 정점: {ghosts}")
EOF

        # Go 프로브(probe-go) — JVM 없는 경로가 JVM 프로브와 같은 그래프를
        # 내는지 패리티로 검증한다(Oracle 섹션과 같은 선택 검증).
        if command -v go >/dev/null 2>&1 && \
            [[ "$mssql_jdbc" =~ jdbc:sqlserver://([^;:]+):([0-9]+)\;databaseName=([^;]+) ]]; then
            ms_go_url="sqlserver://${ms_user}:${ms_pass}@${BASH_REMATCH[1]}:${BASH_REMATCH[2]}/${BASH_REMATCH[3]}"
            # Azure SQL Edge 계열은 Go TLS가 서버 인증서를 못 읽는다(negative
            # serial) — JDBC의 encrypt=false와 같은 의도로 평문 접속한다.
            [[ "$mssql_jdbc" =~ encrypt=false ]] && ms_go_url+="?encrypt=disable"
            [ -x "$tmp/schemagraph-probe-go" ] || \
                (cd "$PWD/probe-go" && go build -o "$tmp/schemagraph-probe-go" .) || \
                { echo "probe-go 빌드 실패" >&2; exit 1; }
            "$tmp/schemagraph-probe-go" --url "$ms_go_url" -o "$tmp/probe-go-ms-doc.json"
            "$BIN" scan --document "$tmp/probe-go-ms-doc.json" -o "$tmp/probe-go-ms-graph.json"
            verify_go_versions "$ms_go_url" "$tmp/probe-go-ms-graph.json" sqlserver
            python3 - "$tmp/probe-ms-graph.json" "$tmp/probe-go-ms-graph.json" <<'EOF'
import json, sys
def load(p):
    g = json.load(open(p))
    return ({v["id"] for v in g["vertices"]},
            {(e["kind"], e["from"], e["to"]) for e in g["edges"]})
jv, je = load(sys.argv[1])
gv, ge = load(sys.argv[2])
if jv != gv or je != ge:
    sys.exit(f"probe-go(MSSQL) 패리티 불일치 — verts diff: {sorted(jv^gv)} "
             f"edges diff: {sorted(je^ge)}")
EOF
        else
            echo "주의: go가 없거나 MSSQL URL을 변환 못 해 probe-go(MSSQL) 검증 건너뜀" >&2
        fi
    else
        echo "주의: MSSQL을 찾지 못해 probe-MSSQL 검증 건너뜀 (SG_MSSQL_URL 또는 docker)" >&2
    fi

    if [ -n "$mssql_jdbc" ]; then
        python3 Scripts/verify-catalog-dependencies.py \
            --engine "$BIN" --go-probe "$tmp/schemagraph-probe-go" --jdbc-jar "$JAR" \
            --skip-sqlite --skip-postgres \
            --mssql-jdbc "$mssql_jdbc" --mssql-user "$ms_user" --mssql-password "$ms_pass"
    fi

    # MSSQL도 여기서 끝 — Oracle(2GB+)과 동시 기동하지 않게 소유 컨테이너를 내린다.
    if [ -n "$mssql_container" ]; then
        docker rm -f "$mssql_container" >/dev/null 2>&1 || true
        mssql_container=""
    fi

    # ── Oracle probe ────────────────────────────────────────────────────
    # ojdbc는 OTN 계열 라이선스라 번들하지 않는다 — SG_ORACLE_JAR로 jar을
    # 지정해야 한다. SG_ORACLE_URL이 있으면 그 서버를 쓰고(폐기용 스키마
    # 계정), 없으면 docker로 gvenzl/oracle-free를 띄운다 — XE엔 ARM 빌드가
    # 없고 23ai Free는 arm64/amd64 모두 돈다. 접속은 APP_USER 스키마로 한다.
    oracle_jdbc=""
    oracle_user="${SG_ORACLE_USER:-sgfix}"
    oracle_pass="${SG_ORACLE_PASSWORD:-Strong!Passw0rd}"
    OJAR="${SG_ORACLE_JAR:-}"

    if [ -z "$OJAR" ] || [ ! -f "$OJAR" ]; then
        echo "주의: SG_ORACLE_JAR가 없어 probe-Oracle 검증 건너뜀 (ojdbc jar 경로)" >&2
    elif [ -n "${SG_ORACLE_URL:-}" ]; then
        oracle_jdbc="$SG_ORACLE_URL"
    elif command -v docker >/dev/null && docker info >/dev/null 2>&1; then
        oracle_container="sg-verify-oracle-$$"
        or_port=$((33700 + RANDOM % 90))
        docker run -d --name "$oracle_container" \
            -e ORACLE_PASSWORD="$oracle_pass" \
            -e APP_USER="$oracle_user" -e APP_USER_PASSWORD="$oracle_pass" \
            -p "$or_port":1521 gvenzl/oracle-free:slim >/dev/null
        trap '[ -n "${pg_own:-}" ] && "${PGBIN:-true}/pg_ctl" -D "${pg_data:-}" -m fast stop >/dev/null 2>&1; [ -n "${my_own:-}" ] && docker rm -f "${my_container:-}" >/dev/null 2>&1; [ -n "${maria_own:-}" ] && docker rm -f "${maria_container:-}" >/dev/null 2>&1; [ -n "${mssql_container:-}" ] && docker rm -f "${mssql_container:-}" >/dev/null 2>&1; docker rm -f "${oracle_container:-}" >/dev/null 2>&1; rm -rf "$tmp"' EXIT

        # 기동 대기 — 첫 기동은 DB 초기화라 몇 분 걸릴 수 있다.
        echo "SELECT 1 FROM DUAL" > "$tmp/or-ping.sql"
        or_ready=""
        for _ in $(seq 1 90); do
            if "$JAVABIN" -cp "$OJAR" "$PWD/Scripts/ApplySql.java" \
                "jdbc:oracle:thin:@localhost:$or_port/FREEPDB1" \
                "$oracle_user" "$oracle_pass" "$tmp/or-ping.sql" >/dev/null 2>&1; then
                or_ready=1
                break
            fi
            sleep 3
        done
        if [ -z "$or_ready" ]; then
            echo "주의: Oracle 컨테이너가 기동하지 않아 probe-Oracle 검증 건너뜀" >&2
        else
            oracle_jdbc="jdbc:oracle:thin:@localhost:$or_port/FREEPDB1"
        fi
    fi

    if [ -n "$oracle_jdbc" ]; then
        # 폐기용 스키마 계약 — fixture를 새로 적용한다.
        "$JAVABIN" -cp "$OJAR" "$PWD/Scripts/ApplySql.java" \
            "$oracle_jdbc" "$oracle_user" "$oracle_pass" \
            "$PWD/Fixtures/oracle/basic.sql"

        "$JAVABIN" -jar "$JAR" --url "$oracle_jdbc" \
            --user "$oracle_user" --password "$oracle_pass" \
            --driver "$OJAR" -o "$tmp/probe-or-doc.json"
        "$BIN" scan --document "$tmp/probe-or-doc.json" -o "$tmp/probe-or-graph.json"
        verify_jdbc_versions "$tmp/probe-or-graph.json" oracle --url "$oracle_jdbc" --user "$oracle_user" --password "$oracle_pass" --driver "$OJAR"
        python3 Scripts/verify-document-versions.py "$BIN" "$tmp/probe-or-doc.json" "$tmp/protocol-oracle"
        python3 Scripts/verify-dynamic-sql.py "$tmp/probe-or-graph.json" oracle
        python3 - "$tmp/probe-or-graph.json" <<'EOF'
import json, sys
g = json.load(open(sys.argv[1]))
# 스키마는 ORDERS 정점에서 유도한다 — Oracle은 미인용 식별자를 대문자로
# 접으므로 SGFIX가 되고, 외부 서버는 접속 계정 스키마를 따른다.
schema = next(v["id"].split(".")[0] for v in g["vertices"]
            if v["id"].endswith(".ORDERS"))
edges = {(e["kind"], e["from"], e["to"]) for e in g["edges"]}
want = {
    ("references", f"{schema}.ORDER_ITEMS", f"{schema}.ORDERS"),
    ("fires", f"{schema}.ORDERS.TRG_ORDERS_TOUCH", f"{schema}.ORDERS"),
    ("writes", f"{schema}.ORDERS.TRG_ORDERS_TOUCH", f"{schema}.CUSTOMERS"),
    ("reads", f"{schema}.ORDER_TOTALS", f"{schema}.ORDERS"),
    # 대소문자 접힘 해석 — 몸체의 소문자 참조가 대문자 멤버까지 닿아야 한다.
    ("reads", f"{schema}.ORDER_TOTALS", f"{schema}.ORDERS.ID"),
}
missing = want - edges
if missing:
    sys.exit(f"probe(Oracle) 간선 미검출: {sorted(missing)}")
verts = {v["id"] for v in g["vertices"]}
if f"{schema}.ORDER_COUNT" not in verts:
    sys.exit(f"probe(Oracle): ORDER_COUNT routine 정점 없음")
if not any(v.startswith(f"{schema}.TOUCH_CUSTOMER(") for v in verts):
    sys.exit(f"probe(Oracle): TOUCH_CUSTOMER routine 정점 없음")
# plsql 몸체는 문장 추출로 간선이 돼야 한다 — TOUCH_CUSTOMER의 UPDATE와
# ORDER_COUNT의 SELECT .. INTO 둘 다 살아 있어야 한다.
if not any(k == "writes" and f.startswith(f"{schema}.TOUCH_CUSTOMER(")
           and t == f"{schema}.CUSTOMERS" for k, f, t in edges):
    sys.exit("probe(Oracle): TOUCH_CUSTOMER의 plsql 몸체 writes 미검출")
if ("reads", f"{schema}.ORDER_COUNT", f"{schema}.ORDERS") not in edges:
    sys.exit("probe(Oracle): ORDER_COUNT의 SELECT .. INTO reads 미검출")
# PACKAGE BODY — 멤버는 schema.pkg.member 정점이 되고 몸체 간선은
# 멤버에 귀속된다. 시그니처가 붙을 수 있어 정점 id는 접두로 본다.
if f"{schema}.ORDER_OPS" not in verts:
    sys.exit("probe(Oracle): ORDER_OPS 패키지 정점 없음")
if not any(k == "contains" and f == f"{schema}.ORDER_OPS"
           and t.startswith(f"{schema}.ORDER_OPS.TOUCH") for k, f, t in edges):
    sys.exit("probe(Oracle): 패키지→멤버 contains 미검출")
if not any(k == "writes" and f.startswith(f"{schema}.ORDER_OPS.TOUCH")
           and t == f"{schema}.CUSTOMERS" for k, f, t in edges):
    sys.exit("probe(Oracle): TOUCH 멤버 UPDATE 귀속 미검출")
if not any(k == "reads" and f.startswith(f"{schema}.ORDER_OPS.COUNT_ALL")
           and t == f"{schema}.ORDERS" for k, f, t in edges):
    sys.exit("probe(Oracle): COUNT_ALL 멤버 SELECT 귀속 미검출")
# pkg.member 꼴 호출 — refresh가 order_ops.count_all()을 부른다.
if not any(k == "calls" and f.startswith(f"{schema}.ORDER_OPS.REFRESH")
           and t.startswith(f"{schema}.ORDER_OPS.COUNT_ALL") for k, f, t in edges):
    sys.exit("probe(Oracle): pkg.member 호출 해석 미검출")
if not any(k == "writes" and f.startswith(f"{schema}.ORDER_OPS.REFRESH")
           and t == f"{schema}.TICKETS" for k, f, t in edges):
    sys.exit("probe(Oracle): REFRESH 멤버 UPDATE 귀속 미검출")
EOF

        # Go 프로브(probe-go) — JVM 없는 경로가 JVM 프로브와 같은 그래프를
        # 내는지 패리티로 검증한다. go 도구체인이 없거나 thin URL을
        # oracle:// 형태로 못 바꾸면 건너뛴다(선택 검증).
        if command -v go >/dev/null 2>&1 && \
            [[ "$oracle_jdbc" =~ jdbc:oracle:thin:@(//)?([^:/]+):([0-9]+)/(.+) ]]; then
            go_url="oracle://${oracle_user}:${oracle_pass}@${BASH_REMATCH[2]}:${BASH_REMATCH[3]}/${BASH_REMATCH[4]}"
            (cd "$PWD/probe-go" && CGO_ENABLED=0 go build -o "$tmp/schemagraph-probe-go" .) || \
                { echo "probe-go 빌드 실패" >&2; exit 1; }
            "$tmp/schemagraph-probe-go" --url "$go_url" -o "$tmp/probe-go-doc.json"
            "$BIN" scan --document "$tmp/probe-go-doc.json" -o "$tmp/probe-go-graph.json"
            verify_go_versions "$go_url" "$tmp/probe-go-graph.json" oracle
            python3 - "$tmp/probe-or-graph.json" "$tmp/probe-go-graph.json" <<'EOF'
import json, sys
def load(p):
    g = json.load(open(p))
    return ({v["id"] for v in g["vertices"]},
            {(e["kind"], e["from"], e["to"]) for e in g["edges"]})
jv, je = load(sys.argv[1])
gv, ge = load(sys.argv[2])
if jv != gv or je != ge:
    sys.exit(f"probe-go(Oracle) 패리티 불일치 — verts diff: {sorted(jv^gv)} "
             f"edges diff: {sorted(je^ge)}")
EOF
        else
            echo "주의: go가 없거나 Oracle URL을 변환 못 해 probe-go 검증 건너뜀" >&2
        fi
    else
        [ -n "$OJAR" ] && [ -f "$OJAR" ] && \
            echo "주의: Oracle을 찾지 못해 probe-Oracle 검증 건너뜀 (SG_ORACLE_URL 또는 docker)" >&2
    fi
    if [ -n "$oracle_jdbc" ]; then
        python3 Scripts/verify-catalog-dependencies.py \
            --engine "$BIN" --go-probe "$tmp/schemagraph-probe-go" --jdbc-jar "$JAR" \
            --oracle-jar "$OJAR" --skip-sqlite --skip-postgres \
            --oracle-jdbc "$oracle_jdbc" --oracle-user "$oracle_user" --oracle-password "$oracle_pass"
    fi
else
    echo "주의: java/probe jar이 없어 probe 검증 건너뜀 (brew openjdk 또는 gradle shadowJar)" >&2
fi

echo "verify-fixtures: OK"
