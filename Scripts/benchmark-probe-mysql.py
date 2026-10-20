#!/usr/bin/env python3
"""MySQL·MariaDB 한 스키마에서 Go probe 기준·새 빌드의 수집 시간·메모리·문장 수와 parity를 측정한다.

Fixtures/accuracy/mysql-environments.json에 다이제스트로 고정한 mysql:8.4·mariadb:11.4 컨테이너를 Docker로
띄운다. --latency-ms를 주면 probe와 서버 사이에 지연 relay(Scripts/latency-relay.py)를 두어 원격 DB의 왕복
지연을 흉내 낸다. 문장 수는 서버 상태 변수 Questions의 증가분(클라이언트가 보낸 문장 — prepare·close는 빠진다)이고,
prepare 수(Com_stmt_prepare)도 함께 남긴다.
"""

from __future__ import annotations

import argparse
import contextlib
import hashlib
import importlib.util
import json
import math
import os
from pathlib import Path
import secrets
import subprocess
import sys
import time

HERE = Path(__file__).resolve().parent
ENVIRONMENTS = HERE.parent / "Fixtures" / "accuracy" / "mysql-environments.json"
# 컨테이너가 처음 만드는 DB이자 측정 스키마다(verify-fixtures.sh의 MySQL·MariaDB fixture와 같은 이름).
DATABASE = "sgfix"
# probe는 이 환경 변수에서 비밀번호가 든 URL을 읽는다(--url-env). 명령줄에 비밀번호를 싣지 않는다.
URL_VARIABLE = "SG_BENCH_MYSQL_URL"
STATUS_VARIABLES = ("Questions", "Com_stmt_prepare")


def load(name: str, filename: str):
    """하이픈이 든 같은 디렉터리의 스크립트를 모듈로 불러온다."""
    spec = importlib.util.spec_from_file_location(name, HERE / filename)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


PG = load("probe_postgres_benchmark", "benchmark-probe-postgres.py")


def write_fixture(path: Path, tables: int, columns: int, routines: int) -> dict[str, int]:
    """테이블마다 PK·parent_id 인덱스·앞 테이블 FK를 두고, 파라미터 둘인 procedure를 routines개 만든다.

    돌려주는 값은 probe 문서가 담아야 할 모양이다(스키마 하나, 테이블마다 PK 하나, 첫 테이블 뒤로 FK 하나).
    """
    if columns < 2:
        raise ValueError("columns must be at least 2")
    with path.open("w", encoding="utf-8") as sql:
        for number in range(tables):
            table = f"t{number:05d}"
            fields = ["id BIGINT NOT NULL PRIMARY KEY", "parent_id BIGINT"]
            fields += [f"c{index} INT" for index in range(2, columns)]
            fields.append(f"KEY {table}_parent (parent_id)")
            if number:
                fields.append(f"CONSTRAINT {table}_fk FOREIGN KEY (parent_id) REFERENCES t{number - 1:05d} (id)")
            sql.write(f"CREATE TABLE {table} ({', '.join(fields)});\n")
        for number in range(routines):
            sql.write(f"CREATE PROCEDURE p{number:05d}(IN a INT, IN b VARCHAR(10)) SELECT a, b;\n")
    return dict(schemas=1, objects=tables, routines=routines, columns=tables * columns,
                constraints=tables + max(tables - 1, 0))


def password_environment(password: str) -> dict[str, str]:
    """docker CLI에 줄 환경이다. `-e 이름`만 쓰면 값이 이 환경에서 넘어가 명령줄·프로세스 목록에 남지 않는다."""
    return {**os.environ, "MYSQL_ROOT_PASSWORD": password, "MARIADB_ROOT_PASSWORD": password, "MYSQL_PWD": password}


def client_command(container: str, client: str, *args: str, interactive: bool = False) -> list[str]:
    """컨테이너 안 root 클라이언트 명령이다. 비밀번호는 docker exec가 넘기는 MYSQL_PWD로 읽는다.

    MYSQL_PWD를 컨테이너 환경에 두지 않는 이유: 이미지 초기화 스크립트는 root 비밀번호를 정하기 전에 비밀번호 없이
    클라이언트를 부르는데, 그때 MYSQL_PWD가 있으면 그 값으로 접속하려다 실패한다.
    """
    return ["docker", "exec", *(["-i"] if interactive else []), "-e", "MYSQL_PWD", container, client,
            "--no-defaults", "-uroot", *args]


def wait_ready(container: str, client: str, password: str, timeout: int) -> None:
    """초기화용 소켓 서버가 아니라 최종 서버가 TCP로 측정 DB를 받을 때까지 기다린다(wait-mysql-ready.py와 같은 판정)."""
    deadline = time.monotonic() + timeout
    probe = client_command(container, client, "--protocol=TCP", "--host=127.0.0.1", "--port=3306",
                           "--connect-timeout=2", f"--database={DATABASE}", "--execute=SELECT 1")
    while time.monotonic() < deadline:
        state = subprocess.run(["docker", "inspect", "--format", "{{.State.Running}}", container],
                               capture_output=True, text=True, check=False)
        if state.returncode or state.stdout.strip() != "true":
            raise RuntimeError(f"temporary database {container} stopped during startup; check its Docker logs")
        try:
            ready = subprocess.run(probe, env=password_environment(password), capture_output=True, timeout=5,
                                   check=False).returncode == 0
        except subprocess.TimeoutExpired:
            # 접속 한 번이 늦어도 다시 시도한다. 전체 기한은 늘리지 않는다.
            ready = False
        if ready:
            return
        time.sleep(1)
    raise RuntimeError(f"timed out after {timeout}s waiting for {container}; check its Docker logs and startup settings")


def start_server(kind: str, image: str, client: str, password: str, timeout: int) -> tuple[str, int]:
    """고정 이미지 컨테이너를 loopback에만 열고, 최종 서버가 TCP로 받을 때까지 기다린다. (이름, 포트)를 돌려준다."""
    name = f"sg-bench-{kind}-{os.getpid()}-{secrets.token_hex(3)}"
    prefix = "MYSQL" if kind == "mysql" else "MARIADB"
    PG.checked(["docker", "run", "-d", "--name", name, "-e", f"{prefix}_ROOT_PASSWORD",
                "-e", f"{prefix}_DATABASE={DATABASE}", "-p", "127.0.0.1::3306", image],
               env=password_environment(password))
    try:
        binding = subprocess.run(["docker", "port", name, "3306/tcp"], capture_output=True, text=True, check=True)
        host, _, port = binding.stdout.strip().partition("\n")[0].rpartition(":")
        if host != "127.0.0.1" or not port.isdigit():
            raise RuntimeError(f"expected one IPv4 loopback port for {name}; got {binding.stdout.strip()!r}")
        wait_ready(name, client, password, timeout)
        return name, int(port)
    except Exception:
        stop_server(name)
        raise


def stop_server(name: str) -> None:
    """컨테이너와 익명 볼륨을 지운다. 지우지 못하면 이름과 함께 알린다."""
    result = subprocess.run(["docker", "rm", "-f", "-v", name], capture_output=True, text=True, check=False)
    if result.returncode:
        print(f"warning: could not remove container {name}: {result.stderr.strip()[:300]}", file=sys.stderr)


def server_sql(server: dict, statement: str) -> str:
    """컨테이너 안 클라이언트로 문장 하나를 실행해 탭으로 나뉜 결과를 돌려준다."""
    result = subprocess.run(client_command(server["container"], server["client"], "-N", "-B", "-e", statement),
                            env=password_environment(server["password"]), capture_output=True, text=True, check=False)
    if result.returncode:
        raise RuntimeError(f"{server['client']} statement failed with exit code {result.returncode}: "
                           f"{result.stderr.strip()[:300]}")
    return result.stdout


def parse_status(text: str) -> dict[str, int]:
    """SHOW GLOBAL STATUS 결과(이름\t값 줄)에서 측정하는 변수를 읽는다. 하나라도 없으면 실패한다."""
    values = {}
    for line in text.splitlines():
        name, _, value = line.partition("\t")
        if name in STATUS_VARIABLES:
            values[name] = int(value)
    missing = [name for name in STATUS_VARIABLES if name not in values]
    if missing:
        raise RuntimeError(f"server status lacks {', '.join(missing)}")
    return values


def server_status(server: dict) -> dict[str, int]:
    """측정 문장 수 변수를 읽는다. 이 읽기 자신도 Questions에 잡히므로 쓰는 쪽이 보정한다."""
    names = ", ".join(f"'{name}'" for name in STATUS_VARIABLES)
    return parse_status(server_sql(server, f"SHOW GLOBAL STATUS WHERE Variable_name IN ({names})"))


def status_delta(before: dict[str, int], after: dict[str, int], overhead: dict[str, int]) -> dict[str, int]:
    """두 읽기 사이의 증가분에서 상태 읽기 자신의 몫(overhead)을 뺀다."""
    return {name: after[name] - before[name] - overhead[name] for name in STATUS_VARIABLES}


def collector_command(probe: Path, format_name: str, document: Path) -> list[str]:
    """probe 수집 명령이다. URL(비밀번호 포함)은 URL_VARIABLE 환경 변수로만 넘긴다."""
    return [str(probe), "--url-env", URL_VARIABLE, "--format", format_name, "--document-version", "2",
            "-o", str(document)]


def run_once(context: dict, producer: dict, run_label: str, warmup: bool) -> dict:
    """한 번 수집해 시간·메모리·문장 수를 재고 문서 모양과(엔진을 주면) 그래프 parity를 확인한다."""
    prefix = context["report"] / f"{producer['label']}-{run_label}"
    document = Path(f"{prefix}.document.{context['format']}")
    before = server_status(context["server"])
    metric = PG.measure(collector_command(producer["path"], context["format"], document),
                        Path(f"{prefix}.probe.stderr"), context["probe_env"])
    statements = status_delta(before, server_status(context["server"]), context["overhead"])
    shape = PG.document_shape(document)
    if shape != context["expected"]:
        raise RuntimeError(f"{producer['label']} document shape mismatch: {shape} != {context['expected']}")
    graph = None
    if context["engine"] is not None:
        graph_path = Path(f"{prefix}.graph.json")
        PG.measure([str(context["engine"]), "scan", "--document", str(document), "-o", str(graph_path)],
                   Path(f"{prefix}.engine.stderr"))
        graph = PG.graph_value(graph_path)
        PG.check_graph_parity(context["references"], producer, run_label, graph)
    return {
        "producer": producer["label"], "kind": producer["kind"], "run": run_label, "warmup": warmup,
        "probe": metric, "statements": statements["Questions"], "prepares": statements["Com_stmt_prepare"],
        "document_bytes": document.stat().st_size,
        "document_sha256": hashlib.sha256(document.read_bytes()).hexdigest(), "shape": shape,
        "graph": None if graph is None else {"vertices": len(graph["vertices"]), "edges": len(graph["edges"])},
    }


def measure_producers(context: dict, producers: list[dict], args: argparse.Namespace) -> tuple[list[dict], dict | None]:
    """warm-up 뒤 기준·새 빌드를 번갈아 repeat번 돌린다. --latency-ms면 모든 수집을 지연 relay 뒤에서 한다."""
    results = []
    relay = PG.RELAY.LatencyRelay(context["port"], args.latency_ms) if args.latency_ms > 0 else None
    with relay if relay is not None else contextlib.nullcontext():
        probe_port = relay.port if relay is not None else context["port"]
        password = context["server"]["password"]
        context["probe_env"] = {**os.environ, URL_VARIABLE: f"mysql://root:{password}@127.0.0.1:{probe_port}/{DATABASE}"}
        for warmup in range(args.warmup):
            results += [run_once(context, producer, f"warmup{warmup}", True) for producer in producers]
        for run in range(args.repeat):
            results += [run_once(context, producer, str(run), False) for producer in producers]
        relay_summary = None if relay is None else {"one_way_ms": args.latency_ms, **relay.snapshot()}
    return results, relay_summary


def apply_fixture(server: dict, fixture: Path, report: Path) -> None:
    """fixture SQL을 컨테이너 안 클라이언트로 측정 스키마에 적용한다."""
    with fixture.open("rb") as source, (report / "fixture.stderr").open("wb") as errors:
        PG.checked(client_command(server["container"], server["client"], DATABASE, interactive=True),
                   stdin=source, stderr=errors, env=password_environment(server["password"]))


def run_benchmark(args: argparse.Namespace) -> dict:
    report = args.output.resolve()
    report.mkdir(parents=True, exist_ok=True)
    producers = [{"label": "baseline", "kind": "go", "path": args.baseline_probe.resolve()},
                 {"label": "new", "kind": "go", "path": args.new_probe.resolve()}]
    for producer in producers:
        if not producer["path"].is_file() or not os.access(producer["path"], os.X_OK):
            raise RuntimeError(f"{producer['label']} probe is not executable: {producer['path']}")
    engine = args.engine.resolve() if args.engine is not None else None
    if engine is not None and (not engine.is_file() or not os.access(engine, os.X_OK)):
        raise RuntimeError(f"engine is not executable: {engine}")
    environment = json.loads(ENVIRONMENTS.read_text(encoding="utf-8"))["engines"][args.server]
    fixture = report / "fixture.sql"
    expected = write_fixture(fixture, args.tables, args.columns, args.routines)
    password = secrets.token_hex(16)
    container, port = start_server(args.server, environment["image"], environment["client"], password,
                                   args.startup_timeout)
    server = {"container": container, "client": environment["client"], "password": password}
    try:
        apply_fixture(server, fixture, report)
        first = server_status(server)
        overhead = status_delta(first, server_status(server), dict.fromkeys(STATUS_VARIABLES, 0))
        context = {"report": report, "server": server, "port": port, "format": args.format, "expected": expected,
                   "engine": engine, "references": {}, "overhead": overhead}
        results, relay_summary = measure_producers(context, producers, args)
        return {
            "server": args.server, "image": environment["image"], "tag": environment["tag"],
            "version": server_sql(server, "SELECT VERSION()").strip(),
            "tables": args.tables, "columns": args.columns, "routines": args.routines, "format": args.format,
            "latency_ms": args.latency_ms, "warmup": args.warmup, "repeat": args.repeat,
            "expected_shape": expected, "status_read_overhead": overhead,
            "platform": sys.platform, "machine": os.uname().machine, "python": sys.version.split()[0],
            "relay": relay_summary, "run_order": [producer["label"] for producer in producers] * args.repeat,
            "summary": PG.summarize(results, producers), "results": results,
        }
    finally:
        stop_server(container)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--server", choices=("mysql", "mariadb"), required=True)
    parser.add_argument("--baseline-probe", type=Path, required=True)
    parser.add_argument("--new-probe", type=Path, required=True)
    parser.add_argument("--engine", type=Path, help="schemagraph engine; when given, graphs of every run must match")
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--tables", type=int, default=2000)
    parser.add_argument("--columns", type=int, default=8)
    parser.add_argument("--routines", type=int, default=0)
    parser.add_argument("--format", choices=("json", "ndjson"), default="json")
    parser.add_argument("--latency-ms", type=float, default=0.0,
                        help="one-way delay a local relay adds in each direction; 0 connects directly")
    parser.add_argument("--warmup", type=int, default=0,
                        help="runs per build before the measured runs; recorded but left out of the summary")
    parser.add_argument("--repeat", type=int, default=3)
    parser.add_argument("--startup-timeout", type=int, default=180, help="seconds to wait for the server")
    args = parser.parse_args()
    if args.tables < 1 or args.columns < 2 or args.routines < 0 or args.repeat < 3 or args.warmup < 0:
        parser.error("tables must be positive, columns at least 2, routines and warmup not negative, repeat at least 3")
    if not math.isfinite(args.latency_ms) or args.latency_ms < 0:
        parser.error("--latency-ms must be a finite number of milliseconds, 0 or more")
    if args.startup_timeout <= 0:
        parser.error("--startup-timeout must be positive")
    if not hasattr(os, "wait4"):
        parser.error("this benchmark requires POSIX wait4 resource accounting")
    try:
        report = run_benchmark(args)
        (args.output / "results.json").write_text(json.dumps(report, indent=2) + "\n", encoding="utf-8")
        print(json.dumps({"status": "ok", "output": str(args.output / "results.json")}, sort_keys=True))
        return 0
    except (OSError, RuntimeError, ValueError, subprocess.CalledProcessError) as error:
        print(f"error: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
