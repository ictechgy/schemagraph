#!/usr/bin/env bash
# 소유한 폐기용 PG만 관측한다. 진단 오류는 원래 JDBC 결과를 바꾸지 않는다.

# 성공/실패 양쪽을 기록해 준비 조회 자체가 수집의 관문이 되지 않게 한다.
fixture_pg_readiness() {
    local status=0
    printf 'endpoint: %s:%s\n' "$1" "$2"
    "$PGBIN/pg_isready" -h "$1" -p "$2" -U postgres -d sgfix -t 2 || status=$?
    printf 'pg_isready exit: %s\n' "$status"
}

# 서버 종료와 주소 계열별 접속 차이를 추측 대신 실패 시점의 증거로 남긴다.
fixture_pg_failure() {
    local directory="$1" port="$2" status=0
    printf 'JDBC exit: %s\n' "$3"
    "$PGBIN/pg_ctl" -D "$pg_data" status || status=$?
    printf 'pg_ctl status exit: %s\n' "$status"
    fixture_pg_readiness "$PGHOST_TMP" "$port"
    fixture_pg_readiness 127.0.0.1 "$port"
    fixture_pg_readiness ::1 "$port"
    # 임시 디렉터리의 EXIT 정리 전에 합성 fixture 로그의 마지막 64 KiB만 남긴다.
    tail -c 65536 "$tmp/pg.log" > "$directory/postgres-server.log"
}

# 외부 URL은 그대로 실행하고, 소유 서버만 원래 JDBC 종료 코드를 보존하며 관측한다.
fixture_postgres_jdbc() {
    local host="$1" port="$2" directory="${SG_FIXTURE_ARTIFACTS:-$tmp}" status=0 recorded=""
    shift 2
    if [ -z "${pg_own:-}" ]; then
        "$@"
        return
    fi
    # 정확히 같은 TCP endpoint의 기동 상태를 남기되 실패해도 수집은 한 번 실행한다.
    if mkdir -p "$directory" && fixture_pg_readiness "$host" "$port" \
        > "$directory/postgres-jdbc-status.log" 2>&1; then
        recorded=1
    else
        echo "warning: could not record owned PostgreSQL readiness" >&2
    fi
    "$@" || status=$?
    if [ "$status" -ne 0 ] && [ -n "$recorded" ]; then
        fixture_pg_failure "$directory" "$port" "$status" \
            >> "$directory/postgres-jdbc-status.log" 2>&1 \
            || echo "warning: could not preserve all owned PostgreSQL diagnostics" >&2
    fi
    return "$status"
}
