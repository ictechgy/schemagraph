#!/usr/bin/env python3
"""초기화용 소켓 서버를 제외하고 소유한 fixture DB의 최종 서버를 기다린다."""
import argparse
import subprocess
import time


def wait_for_mysql(container, client, timeout=180):
    """실패한 기동 뒤 fixture 적용을 막고 Docker 호출도 남은 시간 안에 끝낸다."""
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        try:
            state = subprocess.run(
                ['docker', 'inspect', '--format', '{{.State.Running}}', container],
                capture_output=True, text=True, timeout=min(5, deadline - time.monotonic()))
        except subprocess.TimeoutExpired as error:
            raise RuntimeError('Docker inspection timed out; check Docker availability.') from error
        if state.returncode:
            raise RuntimeError(f'Cannot inspect temporary database {container}; check Docker availability.')
        if state.stdout.strip() != 'true':
            raise RuntimeError(f'Temporary database {container} stopped during startup; check its Docker logs and memory limits.')
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            break
        try:
            query = subprocess.run(
                ['docker', 'exec', container, client, '--no-defaults', '--protocol=TCP',
                 '--host=127.0.0.1', '--port=3306', '--connect-timeout=2', '-uroot',
                 '--database=sgfix', '--execute=SELECT 1'],
                capture_output=True, timeout=min(5, remaining))
        except subprocess.TimeoutExpired:
            # 접속 한 번이 지연돼도 재시도는 전체 기한을 늘리지 않는다.
            query = None
        if query is not None and query.returncode == 0:
            return
        time.sleep(min(1, max(0, deadline - time.monotonic())))
    raise RuntimeError(f'Timed out after {timeout}s waiting for {container} TCP database sgfix; check its Docker logs and startup configuration.')


def main():
    """셸에 실패를 전달해 준비되지 않은 DB에 fixture를 적용하지 않게 한다."""
    parser = argparse.ArgumentParser(description='Wait for a temporary MySQL/MariaDB fixture database over TCP.')
    parser.add_argument('container')
    parser.add_argument('client', choices=('mysql', 'mariadb'))
    parser.add_argument('--timeout', type=int, default=180, help='startup deadline in seconds (default: 180)')
    args = parser.parse_args()
    if args.timeout <= 0:
        parser.error('--timeout must be positive')
    try:
        wait_for_mysql(args.container, args.client, args.timeout)
    except (RuntimeError, OSError) as error:
        parser.exit(1, f'error: {error}\n')


if __name__ == '__main__':
    main()
