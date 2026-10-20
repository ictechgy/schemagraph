#!/usr/bin/env python3
"""PostgreSQL 다중 schema에서 Go probe(선택하면 JDBC probe도)의 수집 시간·메모리·문장 수와 parity를 측정한다.

--latency-ms를 주면 probe와 서버 사이에 지연 relay(Scripts/latency-relay.py)를 두어 원격 DB의 왕복
지연을 흉내 낸다. 문장 수는 측정 DB의 pg_stat_database.xact_commit 증가분이다.
"""

from __future__ import annotations

import argparse
import contextlib
import copy
import getpass
import hashlib
import importlib.util
import json
import math
import os
from pathlib import Path
import secrets
import shutil
import socket
import statistics
import subprocess
import sys
import tempfile
import time

SPEC = importlib.util.spec_from_file_location("latency_relay", Path(__file__).with_name("latency-relay.py"))
RELAY = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(RELAY)

MISSING = object()
# 측정 DB. 문장 수는 이 DB의 xact_commit만 세고, 카운터를 읽는 psql은 postgres DB에 붙는다.
DATABASE = "sgbench"
# 측정 뒤 카운터가 멈추기를 기다리는 한도(초). 백엔드는 끝날 때 통계를 넘긴다.
STATEMENT_SETTLE_SECONDS = 60


def checked(command: list[str], *, cwd: Path | None = None, stdin=None, stderr=None, env=None) -> None:
    """외부 명령 실패를 argv 전체나 접속정보 없이 보고한다."""
    result = subprocess.run(
        command,
        cwd=cwd,
        stdin=stdin,
        stderr=stderr,
        env=env,
        stdout=subprocess.DEVNULL,
        text=True,
        check=False,
    )
    if result.returncode:
        raise RuntimeError(f"command failed with exit code {result.returncode}: {Path(command[0]).name}")


def measure(command: list[str], error_path: Path, env: dict[str, str] | None = None) -> dict[str, int | float]:
    """wait4로 자식 하나의 wall time과 peak RSS를 측정한다."""
    start = time.perf_counter()
    with error_path.open("wb") as errors:
        process = subprocess.Popen(command, stdout=subprocess.DEVNULL, stderr=errors, env=env)
        _, status, usage = os.wait4(process.pid, 0)
        process.returncode = os.waitstatus_to_exitcode(status)
    if process.returncode:
        raise RuntimeError(f"benchmark process failed with exit code {process.returncode}; see {error_path}")
    multiplier = 1 if sys.platform == "darwin" else 1024
    return {
        "seconds": round(time.perf_counter() - start, 6),
        "peak_rss_bytes": usage.ru_maxrss * multiplier,
    }


def reserve_port() -> int:
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as probe:
        probe.bind(("127.0.0.1", 0))
        return int(probe.getsockname()[1])


def write_fixture(path: Path, schemas: int, tables: int, columns: int, indexes: bool = False) -> tuple[int, int]:
    """실제 PK/FK와 모든 컬럼을 포함한 결정적 DDL을 쓴다. indexes면 테이블마다 parent_id 인덱스를 더한다."""
    if columns < 2:
        raise ValueError("columns must be at least 2")
    total_constraints = 0
    total_columns = 0
    with path.open("w", encoding="utf-8") as sql:
        previous = None
        for schema_number in range(schemas):
            schema = f"bench_{schema_number:03d}"
            sql.write(f'CREATE SCHEMA "{schema}";\n')
            for table_number in range(tables):
                table = f"table_{table_number:04d}"
                qualified = f'"{schema}"."{table}"'
                fields = ["id BIGINT NOT NULL PRIMARY KEY", "parent_id BIGINT"]
                fields.extend(f"column_{i:02d} INTEGER" for i in range(2, columns))
                if previous is not None:
                    fields.append(f"CONSTRAINT \"{table}_fk\" FOREIGN KEY (parent_id) REFERENCES {previous}(id)")
                    total_constraints += 1
                total_constraints += 1
                total_columns += columns
                sql.write(f"CREATE TABLE {qualified} ({', '.join(fields)});\n")
                if indexes:
                    sql.write(f'CREATE INDEX "{table}_parent" ON {qualified} (parent_id);\n')
                previous = f"{qualified}"
    return total_columns, total_constraints


def normalize_usage(graph: dict) -> dict:
    """usage의 시각·카운트 값은 지우고 key 존재와 graph 구조는 유지한다."""
    value = copy.deepcopy(graph)
    for vertex in value.get("vertices", []):
        usage = vertex.get("usage", MISSING)
        if usage is MISSING:
            continue
        if not isinstance(usage, dict):
            raise RuntimeError("graph usage must be an object")
        for key in ("reads", "writes"):
            if key in usage and (isinstance(usage[key], bool) or not isinstance(usage[key], int) or usage[key] < 0):
                raise RuntimeError(f"graph usage.{key} must be a nonnegative integer")
        vertex["usage"] = {key: "<value>" for key in sorted(usage)}
    return value


def load_json(path: Path, label: str) -> dict:
    try:
        with path.open(encoding="utf-8") as source:
            value = json.load(source)
    except (OSError, json.JSONDecodeError) as error:
        raise RuntimeError(f"{label} is not valid JSON: {error}") from error
    if not isinstance(value, dict):
        raise RuntimeError(f"{label} must be a JSON object")
    return value


def ndjson_header(line: str) -> bool:
    """첫 줄이 NDJSON 머리 레코드인지 본다. 들여쓴 JSON 문서의 첫 줄(`{`)은 그 자체로 JSON 값이 아니다."""
    try:
        record = json.loads(line)
    except json.JSONDecodeError:
        return False
    return isinstance(record, dict) and record.get("type") == "document"


def document_shape(path: Path) -> dict[str, int]:
    schemas = objects = routines = columns = constraints = 0
    with path.open(encoding="utf-8") as source:
        first = source.readline()
        if not first:
            raise RuntimeError("probe document is empty")
        if ndjson_header(first):
            for line in source:
                record = json.loads(line)
                if record.get("type") == "schema":
                    schemas += 1
                elif record.get("type") == "object":
                    obj = record.get("data", {})
                    objects += 1
                    columns += len(obj.get("columns", []))
                    constraints += len(obj.get("constraints", []))
                elif record.get("type") == "routine":
                    routines += 1
        else:
            document = json.loads(first + source.read())
            schema_values = document.get("schemas")
            if not isinstance(schema_values, list):
                raise RuntimeError("probe document has no schemas array")
            schemas = len(schema_values)
            for schema in schema_values:
                objects += len(schema.get("objects", []))
                routines += len(schema.get("routines", []))
                for obj in schema.get("objects", []):
                    columns += len(obj.get("columns", []))
                    constraints += len(obj.get("constraints", []))
    return dict(schemas=schemas, objects=objects, routines=routines, columns=columns, constraints=constraints)


def graph_value(path: Path) -> dict:
    graph = load_json(path, "engine graph")
    if not isinstance(graph.get("vertices"), list) or not isinstance(graph.get("edges"), list):
        raise RuntimeError("engine graph lacks vertices or edges arrays")
    return normalize_usage(graph)


def first_difference(left: object, right: object, path: str = "graph") -> str:
    if type(left) is not type(right):
        return f"{path}: types differ"
    if isinstance(left, dict):
        for key in sorted(set(left) | set(right)):
            if key not in left or key not in right:
                return f"{path}.{key}: field presence differs"
            difference = first_difference(left[key], right[key], f"{path}.{key}")
            if difference:
                return difference
        return ""
    if isinstance(left, list):
        if len(left) != len(right):
            return f"{path}: lengths differ ({len(left)} vs {len(right)})"
        for index, (a, b) in enumerate(zip(left, right)):
            difference = first_difference(a, b, f"{path}[{index}]")
            if difference:
                return difference
        return ""
    return "" if left == right else f"{path}: values differ"


def private_file(path: Path, text: str) -> Path:
    """비밀번호를 담는 파일을 처음부터 0600으로 만든다(만든 뒤 chmod하면 잠깐 열린다)."""
    with os.fdopen(os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600), "w") as stream:
        stream.write(text)
    return path


def start_postgres(pg_bin: Path, report: Path) -> tuple[Path, int, str, str, dict[str, str]]:
    """측정용 임시 cluster를 띄우고 (cluster 디렉터리, 포트, 사용자, 비밀번호, 접속 환경)을 돌려준다.

    같은 호스트의 다른 계정이 loopback TCP로 슈퍼유저에 접속하지 못하게 실행마다 만든 비밀번호(scram)로만
    받는다. Unix socket은 끈다: macOS처럼 TMPDIR이 긴 곳에서는 socket 경로가 103바이트 제한을 넘어 기동이
    실패한다. 0700 cluster 디렉터리에 data와 0600 비밀번호 파일을 두고, psql과 Go probe는 env의
    PGPASSFILE에서 비밀번호를 읽는다. autovacuum과 병렬 질의를 끈다 — 자동 vacuum과 병렬 작업자의
    트랜잭션도 xact_commit에 잡혀, 켜 두면 문장 수가 probe가 보낸 autocommit 문장 수와 달라진다.
    """
    cluster = Path(tempfile.mkdtemp(prefix="sg-roadmap-pg-"))
    port = reserve_port()
    user = getpass.getuser()
    password = secrets.token_hex(16)
    # 셸의 PGPASSWORD는 PGPASSFILE보다 먼저 쓰여 새 비밀번호를 가리므로 뺀다.
    env = {key: value for key, value in os.environ.items() if key != "PGPASSWORD"}
    try:
        initial = private_file(cluster / "initdb-password", password + "\n")
        # 비밀번호는 이 cluster 포트에만 준다. --latency-ms의 relay 포트 줄은 relay가 열린 뒤 allow_relay_port가 더한다.
        env["PGPASSFILE"] = str(private_file(cluster / "pgpass", f"*:{port}:*:*:{password}\n"))
        checked([str(pg_bin / "initdb"), "-D", str(cluster / "data"), "--auth-local=trust",
                 "--auth-host=scram-sha-256", f"--pwfile={initial}", "--no-instructions"])
        settings = f"-p {port} -k '' -c listen_addresses=127.0.0.1 -c autovacuum=off -c max_parallel_workers_per_gather=0"
        checked([str(pg_bin / "pg_ctl"), "-D", str(cluster / "data"), "-l", str(report / "postgres.log"),
                 "-o", settings, "-w", "start"])
        checked(psql_command(pg_bin, port, user, "postgres", "-c", f"CREATE DATABASE {DATABASE}"), env=env)
        return cluster, port, user, password, env
    except Exception:
        stop_postgres(pg_bin, cluster)
        raise


def allow_relay_port(passfile: str, relay_port: int, password: str) -> None:
    """지연 relay 포트로 붙는 Go probe도 PGPASSFILE에서 비밀번호를 찾게 그 포트 줄을 더한다.

    포트를 *로 두면 같은 호스트의 다른 포트에서 받는 서버에도 이 비밀번호를 내주므로, 실제로 붙는 포트만
    하나씩 연다. 0600으로 만든 파일에 이어 쓰므로 권한은 그대로다.
    """
    with open(passfile, "a", encoding="utf-8") as stream:
        stream.write(f"*:{relay_port}:*:*:{password}\n")


def psql_command(pg_bin: Path, port: int, user: str, database: str, *args: str) -> list[str]:
    """loopback TCP로 붙는 psql 명령을 만든다. 비밀번호는 env의 PGPASSFILE에서 읽는다."""
    return [str(pg_bin / "psql"), "-X", "-q", "-h", "127.0.0.1", "-p", str(port), "-U", user, "-d", database,
            "-v", "ON_ERROR_STOP=1", *args]


def database_activity(pg_bin: Path, port: int, user: str, env: dict[str, str]) -> tuple[int, int]:
    """측정 DB의 (열린 세션 수, xact_commit)을 읽는다. 측정 DB의 카운터를 늘리지 않게 postgres DB에서 묻는다."""
    query = (f"SELECT (SELECT count(*) FROM pg_stat_activity WHERE datname = '{DATABASE}'), "
             f"(SELECT xact_commit FROM pg_stat_database WHERE datname = '{DATABASE}')")
    result = subprocess.run(psql_command(pg_bin, port, user, "postgres", "-A", "-t", "-F", ",", "-c", query),
                            env=env, capture_output=True, text=True, check=False)
    if result.returncode:
        raise RuntimeError(f"statement counter query failed with exit code {result.returncode}: {result.stderr.strip()[:300]}")
    sessions, commits = result.stdout.strip().split(",")
    return int(sessions), int(commits)


def committed_statements(pg_bin: Path, port: int, user: str, env: dict[str, str]) -> int:
    """측정 DB의 세션이 모두 끝나고 xact_commit이 두 번 연달아 같을 때의 값을 돌려준다.

    probe 백엔드는 끝날 때 통계를 넘기므로, 세션이 남아 있거나 값이 바뀌는 동안은 기다린다.
    """
    previous = None
    deadline = time.monotonic() + STATEMENT_SETTLE_SECONDS
    while time.monotonic() < deadline:
        sessions, commits = database_activity(pg_bin, port, user, env)
        if sessions == 0 and commits == previous:
            return commits
        previous = commits if sessions == 0 else None
        time.sleep(0.2)
    raise RuntimeError(f"{DATABASE} statement counter did not settle within {STATEMENT_SETTLE_SECONDS} s; "
                       "check for other sessions on the benchmark database")


def stop_postgres(pg_bin: Path, cluster: Path) -> None:
    subprocess.run(
        [str(pg_bin / "pg_ctl"), "-D", str(cluster / "data"), "-m", "fast", "-w", "stop"],
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
        check=False,
    )
    shutil.rmtree(cluster, ignore_errors=True)


def collector_command(producer: dict, port: int, user: str, format_name: str, document: Path,
                      java: Path | None) -> list[str]:
    """producer 하나의 수집 명령을 만든다. port는 서버나, 지연 relay를 두면 relay의 포트다."""
    tail = ["--format", format_name, "--document-version", "2", "-o", str(document)]
    if producer["kind"] == "jdbc":
        return [str(java), "-jar", str(producer["path"]), "--url",
                f"jdbc:postgresql://127.0.0.1:{port}/{DATABASE}?sslmode=disable", "--user", user, *tail]
    return [str(producer["path"]), "--url", f"postgres://{user}@127.0.0.1:{port}/{DATABASE}?sslmode=disable", *tail]


def benchmark_producers(args: argparse.Namespace, baseline: Path, new_probe: Path) -> list[dict]:
    """번갈아 돌릴 producer 목록이다. JDBC JAR 두 개를 주면 Go 쌍 뒤에 JDBC 쌍을 더한다."""
    producers = [{"label": "baseline", "kind": "go", "path": baseline},
                 {"label": "streaming", "kind": "go", "path": new_probe}]
    if args.baseline_jar is not None:
        for label, jar in (("jdbc-baseline", args.baseline_jar), ("jdbc-new", args.new_jar)):
            if not jar.resolve().is_file():
                raise RuntimeError(f"{label} JAR does not exist: {jar}")
            producers.append({"label": label, "kind": "jdbc", "path": jar.resolve()})
    return producers


def check_graph_parity(references: dict[str, dict], producer: dict, run_label: str, graph: dict) -> None:
    """같은 종류(Go, JDBC)의 모든 실행이 첫 실행과 같은 그래프를 내는지 본다(usage 값은 정규화한다)."""
    reference = references.setdefault(producer["kind"], graph)
    difference = "" if reference is graph else first_difference(reference, graph)
    if difference:
        raise RuntimeError(f"graph parity mismatch for {producer['label']} run {run_label}: {difference}")


def run_once(context: dict, producer: dict, probe_port: int, run_label: str, warmup: bool) -> dict:
    """한 번 수집해 시간·메모리·문장 수를 재고, 문서 모양과 그래프 parity를 확인한다."""
    prefix = context["report"] / f"{producer['label']}-{run_label}"
    document = Path(f"{prefix}.document.{context['format']}")
    graph_path = Path(f"{prefix}.graph.json")
    counter = (context["pg_bin"], context["port"], context["user"], context["env"])
    command = collector_command(producer, probe_port, context["user"], context["format"], document, context["java"])
    before = committed_statements(*counter)
    metric = measure(command, Path(f"{prefix}.probe.stderr"), context["probe_env"][producer["kind"]])
    statements = committed_statements(*counter) - before
    engine_metric = measure([str(context["engine"]), "scan", "--document", str(document), "-o", str(graph_path)],
                            Path(f"{prefix}.engine.stderr"))
    shape = document_shape(document)
    expected = context["expected"][producer["kind"]]
    if shape != expected:
        raise RuntimeError(f"{producer['label']} document shape mismatch: {shape} != {expected}")
    graph = graph_value(graph_path)
    check_graph_parity(context["references"], producer, run_label, graph)
    return {
        "producer": producer["label"], "kind": producer["kind"], "run": run_label, "warmup": warmup,
        "probe": metric, "engine": engine_metric, "statements": statements,
        "document_bytes": document.stat().st_size,
        "document_sha256": hashlib.sha256(document.read_bytes()).hexdigest(),
        "graph_bytes": graph_path.stat().st_size, "shape": shape,
        "graph": {"vertices": len(graph.get("vertices", [])), "edges": len(graph.get("edges", []))},
    }


def summarize(results: list[dict], producers: list[dict]) -> dict:
    """producer마다 warm-up을 뺀 실행의 중앙값과, 실행마다 같아야 하는 문장 수·문서 해시의 집합을 모은다."""
    summary = {}
    for producer in producers:
        samples = [result for result in results if result["producer"] == producer["label"]]
        measured = [result for result in samples if not result["warmup"]]
        summary[producer["label"]] = {
            "kind": producer["kind"],
            "median_seconds": round(statistics.median(result["probe"]["seconds"] for result in measured), 3),
            "median_peak_rss_bytes": statistics.median(result["probe"]["peak_rss_bytes"] for result in measured),
            "statements": sorted({result["statements"] for result in measured}),
            "document_sha256": sorted({result["document_sha256"] for result in samples}),
        }
    return summary


def tool_version(command: list[str]) -> str:
    """버전 명령의 첫 줄(stdout, 없으면 stderr)을 기록용으로 읽는다."""
    result = subprocess.run(command, capture_output=True, text=True, check=False)
    lines = (result.stdout or result.stderr).strip().splitlines()
    return lines[0] if lines else f"unknown (exit code {result.returncode})"


def measure_producers(context: dict, producers: list[dict], args: argparse.Namespace) -> tuple[list[dict], dict | None]:
    """warm-up 뒤 producer들을 번갈아 repeat번 돌린다. --latency-ms면 모든 수집을 지연 relay 뒤에서 한다."""
    results = []
    relay = RELAY.LatencyRelay(context["port"], args.latency_ms) if args.latency_ms > 0 else None
    with relay if relay is not None else contextlib.nullcontext():
        probe_port = relay.port if relay is not None else context["port"]
        if relay is not None:
            allow_relay_port(context["env"]["PGPASSFILE"], relay.port, context["password"])
        for warmup in range(args.warmup):
            results += [run_once(context, producer, probe_port, f"warmup{warmup}", True) for producer in producers]
        for run in range(args.repeat):
            results += [run_once(context, producer, probe_port, str(run), False) for producer in producers]
        relay_summary = None if relay is None else {"one_way_ms": args.latency_ms, **relay.snapshot()}
    return results, relay_summary


def run_benchmark(args: argparse.Namespace) -> dict:
    report = args.output.resolve()
    report.mkdir(parents=True, exist_ok=True)
    baseline = args.baseline_probe.resolve()
    new_probe = args.new_probe.resolve()
    engine = args.engine.resolve()
    if not baseline.is_file() or not os.access(baseline, os.X_OK):
        raise RuntimeError(f"baseline probe is not executable: {baseline}")
    if not engine.is_file() or not os.access(engine, os.X_OK):
        raise RuntimeError(f"engine is not executable: {engine}")
    producers = benchmark_producers(args, baseline, new_probe)
    new_probe.parent.mkdir(parents=True, exist_ok=True)
    build_env = os.environ.copy()
    build_env["CGO_ENABLED"] = "0"
    with (report / "go-build.stderr").open("wb") as build_errors:
        checked(
            ["go", "build", "-o", str(new_probe), "."],
            cwd=args.probe_dir.resolve(),
            stderr=build_errors,
            env=build_env,
        )

    pg_bin = args.pg_bin.resolve()
    cluster = None
    try:
        cluster, port, user, password, env = start_postgres(pg_bin, report)
        fixture = report / "fixture.sql"
        expected_shape = write_fixture(fixture, args.schemas, args.tables_per_schema, args.columns, args.indexes)
        with (report / "fixture.stderr").open("wb") as fixture_errors:
            checked(psql_command(pg_bin, port, user, DATABASE, "-f", str(fixture)), stderr=fixture_errors, env=env)
        expected = dict(
            schemas=args.schemas + 1,
            objects=args.schemas * args.tables_per_schema,
            routines=0,
            columns=expected_shape[0],
            constraints=expected_shape[1],
        )
        context = {
            "report": report, "pg_bin": pg_bin, "port": port, "user": user, "password": password, "env": env,
            "engine": engine,
            "java": args.java.resolve() if args.java is not None else None, "format": args.format,
            # Go probe는 빈 public 스키마도 싣고, JDBC probe는 객체·routine이 없는 스키마를 싣지 않는다.
            "expected": {"go": expected, "jdbc": {**expected, "schemas": args.schemas}}, "references": {},
            # JDBC probe는 PGPASSFILE을 읽지 않으므로 SG_DB_PASSWORD로 받는다.
            "probe_env": {"go": env, "jdbc": {**env, "SG_DB_PASSWORD": password}},
        }
        results, relay_summary = measure_producers(context, producers, args)
        return {
            "schemas": args.schemas,
            "tables_per_schema": args.tables_per_schema,
            "columns": args.columns,
            "indexes": args.indexes,
            "format": args.format,
            "latency_ms": args.latency_ms,
            "warmup": args.warmup,
            "repeat": args.repeat,
            "expected_shape": expected,
            "platform": sys.platform,
            "machine": os.uname().machine,
            "python": sys.version.split()[0],
            "go": tool_version(["go", "version"]),
            "java": tool_version([str(context["java"]), "-version"]) if context["java"] is not None else None,
            "postgres": tool_version([str(pg_bin / "postgres"), "--version"]),
            "server_settings": {"autovacuum": "off", "max_parallel_workers_per_gather": 0},
            "relay": relay_summary,
            "run_order": [producer["label"] for producer in producers] * args.repeat,
            "summary": summarize(results, producers),
            "results": results,
        }
    finally:
        if cluster is not None:
            stop_postgres(pg_bin, cluster)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--baseline-probe", type=Path, default=Path("engine/target/roadmap-validation/sg-roadmap-probe-go"))
    parser.add_argument("--new-probe", type=Path, default=Path("engine/target/scaling-validation/probe-after"))
    parser.add_argument("--probe-dir", type=Path, default=Path("probe-go"))
    parser.add_argument("--engine", type=Path, default=Path("engine/target/scaling-validation/engine-build/release/schemagraph"))
    parser.add_argument("--pg-bin", type=Path, default=Path("/opt/homebrew/opt/postgresql@16/bin"))
    parser.add_argument("--output", type=Path, default=Path("engine/target/scaling-validation/pg-benchmark"))
    parser.add_argument("--schemas", type=int, default=20)
    parser.add_argument("--tables-per-schema", type=int, default=200)
    parser.add_argument("--columns", type=int, default=8)
    parser.add_argument("--repeat", type=int, default=3)
    parser.add_argument("--warmup", type=int, default=0,
                        help="runs per producer before the measured runs; recorded but left out of the summary")
    parser.add_argument("--format", choices=("ndjson", "json"), default="ndjson")
    parser.add_argument("--indexes", action="store_true", help="add one parent_id index to every table")
    parser.add_argument("--latency-ms", type=float, default=0.0,
                        help="one-way delay a local relay adds in each direction; 0 connects directly")
    parser.add_argument("--baseline-jar", type=Path, help="previous JDBC probe JAR; requires --new-jar and --java")
    parser.add_argument("--new-jar", type=Path, help="changed JDBC probe JAR")
    parser.add_argument("--java", type=Path, help="java executable for the JDBC probe")
    args = parser.parse_args()
    if args.schemas < 1 or args.tables_per_schema < 1 or args.columns < 2 or args.repeat < 3:
        parser.error("schemas/tables-per-schema must be positive, columns at least 2, repeat at least 3")
    if args.warmup < 0:
        parser.error("--warmup must not be negative")
    if not math.isfinite(args.latency_ms) or args.latency_ms < 0:
        parser.error("--latency-ms must be a finite number of milliseconds, 0 or more")
    if (args.baseline_jar is None) != (args.new_jar is None):
        parser.error("--baseline-jar and --new-jar must be given together")
    if args.baseline_jar is not None and args.java is None:
        parser.error("--java is required with the JDBC JARs")
    if not hasattr(os, "wait4"):
        parser.error("this benchmark requires POSIX wait4 resource accounting")
    try:
        report = run_benchmark(args)
        args.output.mkdir(parents=True, exist_ok=True)
        (args.output / "results.json").write_text(json.dumps(report, indent=2) + "\n", encoding="utf-8")
        print(json.dumps({"status": "ok", "output": str(args.output / "results.json")}, sort_keys=True))
        return 0
    except (OSError, RuntimeError, ValueError) as error:
        print(f"error: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
