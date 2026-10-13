#!/usr/bin/env bash
# 포트를 미리 추측하지 않고 Docker가 실제로 예약한 loopback 바인딩만 사용한다.
fixture_port() {
    local binding
    binding=$(docker port "$1" "$2/tcp") || return
    if [[ ! $binding =~ ^127\.0\.0\.1:([1-9][0-9]{0,4})$ ]] ||
        (( BASH_REMATCH[1] > 65535 )); then
        echo "error: expected one IPv4 loopback port for fixture $1:$2; got: $binding" >&2
        return 1
    fi
    printf '%s\n' "${BASH_REMATCH[1]}"
}
