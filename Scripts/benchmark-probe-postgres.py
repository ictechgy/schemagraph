#!/usr/bin/env python3
"""PostgreSQL 다중 schema에서 Go probe NDJSON 메모리와 parity를 측정한다."""

from __future__ import annotations

import argparse
import copy
import getpass
import json
import os
from pathlib import Path
import shutil
import socket
import subprocess
import sys
import tempfile
import time


MISSING = object()


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


def measure(command: list[str], error_path: Path) -> dict[str, int | float]:
    """wait4로 자식 하나의 wall time과 peak RSS를 측정한다."""
    start = time.perf_counter()
    with error_path.open("wb") as errors:
        process = subprocess.Popen(command, stdout=subprocess.DEVNULL, stderr=errors)
        _, status, usage = os.wait4(process.pid, 0)
        process.returncode = os.waitstatus_to_exitcode(status)
    if process.returncode:
        raise RuntimeError(f"benchmark process failed with exit code {process.returncode}")
    multiplier = 1 if sys.platform == "darwin" else 1024
    return {
        "seconds": round(time.perf_counter() - start, 6),
        "peak_rss_bytes": usage.ru_maxrss * multiplier,
    }


def reserve_port() -> int:
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as probe:
        probe.bind(("127.0.0.1", 0))
        return int(probe.getsockname()[1])


def write_fixture(path: Path, schemas: int, tables: int, columns: int) -> tuple[int, int]:
    """실제 PK/FK와 모든 컬럼을 포함한 결정적 DDL을 쓴다."""
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


def document_shape(path: Path) -> dict[str, int]:
    schemas = objects = routines = columns = constraints = 0
    with path.open(encoding="utf-8") as source:
        first = source.readline()
        if not first:
            raise RuntimeError("probe document is empty")
        if json.loads(first).get("type") == "document":
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


def start_postgres(pg_bin: Path, root: Path, report: Path) -> tuple[Path, int, str]:
    data = Path(tempfile.mkdtemp(prefix="sg-roadmap-pg-"))
    port = reserve_port()
    user = getpass.getuser()
    try:
        checked([str(pg_bin / "initdb"), "-D", str(data), "-A", "trust", "--no-instructions"])
        log = report / "postgres.log"
        checked(
            [
                str(pg_bin / "pg_ctl"),
                "-D",
                str(data),
                "-l",
                str(log),
                "-o",
                f"-p {port} -k {data}",
                "-w",
                "start",
            ]
        )
        checked(
            [
                str(pg_bin / "psql"),
                "-h",
                str(data),
                "-p",
                str(port),
                "-U",
                user,
                "-d",
                "postgres",
                "-v",
                "ON_ERROR_STOP=1",
                "-c",
                "CREATE DATABASE sgbench",
            ]
        )
        return data, port, user
    except Exception:
        stop_postgres(pg_bin, data)
        raise


def stop_postgres(pg_bin: Path, data: Path) -> None:
    subprocess.run(
        [str(pg_bin / "pg_ctl"), "-D", str(data), "-m", "fast", "-w", "stop"],
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
        check=False,
    )
    shutil.rmtree(data, ignore_errors=True)


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
    data = None
    results = []
    reference_graph = None
    try:
        data, port, user = start_postgres(pg_bin, Path.cwd(), report)
        fixture = report / "fixture.sql"
        expected_shape = write_fixture(fixture, args.schemas, args.tables_per_schema, args.columns)
        checked(
            [
                str(pg_bin / "psql"), "-h", str(data), "-p", str(port), "-U", user,
                "-d", "sgbench", "-v", "ON_ERROR_STOP=1", "-f", str(fixture),
            ],
            stderr=(report / "fixture.stderr").open("wb"),
        )
        url = f"postgres://{user}@localhost:{port}/sgbench?sslmode=disable"
        producers = (("baseline", baseline), ("streaming", new_probe))
        for run in range(args.repeat):
            for label, probe in producers:
                prefix = report / f"{label}-{run}"
                document = Path(f"{prefix}.document.ndjson")
                graph_path = Path(f"{prefix}.graph.json")
                metric = measure(
                    [str(probe), "--url", url, "--format", "ndjson", "--document-version", "2", "-o", str(document)],
                    Path(f"{prefix}.probe.stderr"),
                )
                engine_metric = measure(
                    [str(engine), "scan", "--document", str(document), "-o", str(graph_path)],
                    Path(f"{prefix}.engine.stderr"),
                )
                shape = document_shape(document)
                expected = dict(
                    schemas=args.schemas + 1,
                    objects=args.schemas * args.tables_per_schema,
                    routines=0,
                    columns=expected_shape[0],
                    constraints=expected_shape[1],
                )
                if shape != expected:
                    raise RuntimeError(f"{label} document shape mismatch: {shape} != {expected}")
                graph = graph_value(graph_path)
                if reference_graph is None:
                    reference_graph = graph
                else:
                    difference = first_difference(reference_graph, graph)
                    if difference:
                        raise RuntimeError(f"graph parity mismatch for {label} run {run}: {difference}")
                results.append({
                    "producer": label,
                    "run": run,
                    "probe": metric,
                    "engine": engine_metric,
                    "document_bytes": document.stat().st_size,
                    "graph_bytes": graph_path.stat().st_size,
                    "shape": shape,
                    "graph": {"vertices": len(graph.get("vertices", [])), "edges": len(graph.get("edges", []))},
                })
        return {
            "schemas": args.schemas,
            "tables_per_schema": args.tables_per_schema,
            "columns": args.columns,
            "repeat": args.repeat,
            "expected_shape": expected,
            "platform": sys.platform,
            "machine": os.uname().machine,
            "python": sys.version.split()[0],
            "go": subprocess.run(["go", "version"], capture_output=True, text=True, check=False).stdout.strip(),
            "postgres": subprocess.run([str(pg_bin / "postgres"), "--version"], capture_output=True, text=True, check=False).stdout.strip(),
            "run_order": ["baseline", "streaming"] * args.repeat,
            "results": results,
        }
    finally:
        if data is not None:
            stop_postgres(pg_bin, data)


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
    args = parser.parse_args()
    if args.schemas < 1 or args.tables_per_schema < 1 or args.columns < 2 or args.repeat < 3:
        parser.error("schemas/tables-per-schema must be positive, columns at least 2, repeat at least 3")
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
