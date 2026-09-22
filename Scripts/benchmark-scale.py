#!/usr/bin/env python3
"""bounded graph scale, resident MCP query, 실제 취소 단계를 측정한다."""

from __future__ import annotations

import argparse
from dataclasses import dataclass
from datetime import datetime, timezone
import errno
import hashlib
import json
import os
from pathlib import Path
import platform
import selectors
import signal
import statistics
import subprocess
import sys
import tempfile
import time
from typing import BinaryIO, Iterable


MIB = 1024 * 1024
REPORT_ROOT = Path.home() / "Library/Application Support/schemagraph/verification/followups-20260922/performance"
ACK = b"Cancellation requested; press Ctrl+C again to exit immediately.\n"
DEFAULT_TIMEOUT = 120.0
PROCESS_POLL_SECONDS = 0.001
ACTIVE_CPU_PROGRESS_SECONDS = 0.01
ACTIVE_PROOF_TIMEOUT = 0.2
ACTIVE_PROOF_POLL_SECONDS = 0.002
MAX_TIMEOUT = 600.0
MAX_OBJECTS = 50_000
MAX_DENSE_VERTICES = 5_000
MAX_DENSE_EDGES = 2_500_000
MAX_ENGINE_EXAMINED_EDGES = 1_000_000
DEFAULT_DENSE_VERTICES = [1_000, 2_000]


class BenchmarkError(RuntimeError):
    """측정 입력·프로세스·출력 검증 실패를 하나의 오류로 표현한다."""


@dataclass(frozen=True)
class Measurement:
    """wait4가 한 자식 프로세스에서 관측한 wall time과 최대 RSS다."""

    seconds: float
    peak_rss_bytes: int


def fail(message: str) -> None:
    """검증 실패를 호출자에게 영어 오류 하나로 전달한다."""
    raise BenchmarkError(message)


def sha256(path: Path) -> str:
    """큰 입력을 한 번에 메모리에 복제하지 않고 해시한다."""
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def json_line(value: object) -> str:
    """생성기와 MCP 프레임이 공유하는 결정적 JSON 표현을 만든다."""
    return json.dumps(value, sort_keys=True, separators=(",", ":"))


def json_value_sha256(value: object) -> str:
    """보존한 report section의 의미론적 동일성을 canonical JSON으로 기록한다."""
    return hashlib.sha256(json_line(value).encode("utf-8")).hexdigest()


def object_record(number: int, width: int) -> dict[str, object]:
    """PK/FK와 인덱스를 가진 같은 schema의 table 하나를 만든다."""
    name = f"table_{number:06d}"
    columns = [
        {
            "name": f"column_{column:03d}",
            "data_type": "INTEGER",
            "nullable": column != 0,
            "ordinal": column + 1,
            "pk_position": int(column == 0),
        }
        for column in range(width)
    ]
    constraints: list[dict[str, object]] = [
        {"name": f"{name}_pk", "kind": "pk", "columns": ["column_000"]}
    ]
    if number:
        constraints.append(
            {
                "name": f"{name}_fk",
                "kind": "fk",
                "columns": ["column_001"],
                "referenced": {"table": f"table_{number - 1:06d}", "columns": ["column_000"]},
            }
        )
    return {
        "name": name,
        "kind": "table",
        "columns": columns,
        "constraints": constraints,
        "indexes": [{"name": f"{name}_idx", "unique": False, "columns": ["column_001"]}],
        "triggers": [],
    }


def catalog_header() -> dict[str, object]:
    """엔진이 문서 형식을 확인할 수 있는 결정적 v2 header를 만든다."""
    return {
        "dialect": "sqlite",
        "producer": {"name": "schemagraph-scale-validation"},
        "required_features": [],
        "version": 2,
    }


def write_large_schema(work: Path, objects: int, columns: int) -> dict[str, Path]:
    """JSON과 NDJSON에 같은 단일 대형 schema를 스트리밍으로 쓴다."""
    schema = "large"
    json_path = work / "large-schema.json"
    ndjson_path = work / "large-schema.ndjson"
    header = catalog_header()
    with json_path.open("w", encoding="utf-8", newline="\n") as document:
        document.write("{")
        document.write(json_line(header)[1:-1])
        document.write(',"limitations":[],"schemas":[{"name":"large","objects":[')
        for number in range(objects):
            if number:
                document.write(",")
            document.write(json_line(object_record(number, columns)))
        document.write('],"routines":[]}]}' + "\n")
    with ndjson_path.open("w", encoding="utf-8", newline="\n") as stream:
        stream.write(json_line({"type": "document", "limitations": [], **header}) + "\n")
        stream.write(json_line({"name": schema, "type": "schema"}) + "\n")
        for number in range(objects):
            stream.write(
                json_line(
                    {"data": object_record(number, columns), "schema": schema, "type": "object"}
                )
                + "\n"
            )
        stream.write(json_line({"data": [], "type": "limitations"}) + "\n")
    return {"json": json_path, "ndjson": ndjson_path}


def expected_large_topology(
    objects: int, columns: int
) -> tuple[set[tuple[str, str]], set[tuple[str, str, str]], dict[str, object]]:
    """생성기 의미론에서 필요한 vertex/edge 집합과 독립 count를 계산한다."""
    vertices: set[tuple[str, str]] = {("large", "schema")}
    edges: set[tuple[str, str, str]] = set()
    vertex_kind_counts = {
        "schema": 1,
        "table": objects,
        "column": objects * columns,
        "index": objects,
        "constraint": objects * 2 - 1,
    }
    edge_kind_counts = {"contains": objects * columns + objects * 4 - 1, "references": (objects - 1) * 2}
    for number in range(objects):
        table = f"large.table_{number:06d}"
        vertices.add((table, "table"))
        edges.add(("large", "contains", table))
        for column in range(columns):
            column_id = f"{table}.column_{column:03d}"
            vertices.add((column_id, "column"))
            edges.add((table, "contains", column_id))
        index_id = f"{table}.table_{number:06d}_idx"
        pk_id = f"{table}.table_{number:06d}_pk"
        vertices.update(((index_id, "index"), (pk_id, "constraint")))
        edges.update(((table, "contains", index_id), (table, "contains", pk_id)))
        if number:
            previous = f"large.table_{number - 1:06d}"
            fk_id = f"{table}.table_{number:06d}_fk"
            vertices.add((fk_id, "constraint"))
            edges.update(
                (
                    (table, "contains", fk_id),
                    (table, "references", previous),
                    (f"{table}.column_001", "references", f"{previous}.column_000"),
                )
            )
    expected = {
        "vertices": len(vertices),
        "edges": len(edges),
        "vertex_kind_counts": vertex_kind_counts,
        "edge_kind_counts": edge_kind_counts,
    }
    if len(vertices) != objects * (columns + 4) or len(edges) != columns * objects + 6 * objects - 3:
        fail("large-schema topology formula disagrees with generated semantics")
    return vertices, edges, expected


def write_dense_graph(
    work: Path, vertices: int, degree: int, *, max_edges: int
) -> Path:
    """중앙 root와 반수준 반복 간선을 가진 bounded dense graph를 쓴다."""
    if vertices < 2 or degree < 1 or degree >= vertices:
        fail("dense graph requires 2 <= vertices and 1 <= degree < vertices")
    edge_count = vertices * (degree + 1)
    if edge_count > max_edges:
        fail(f"dense graph has {edge_count} directed edges, above the {max_edges}-edge bound")
    path = work / f"dense-v{vertices}-d{degree}.graph.json"
    root = {"id": "main.root", "kind": "table", "level": "object", "name": "root", "schema": "main"}

    def node(number: int) -> dict[str, str]:
        name = f"node_{number:06d}"
        return {"id": f"main.{name}", "kind": "view", "level": "object", "name": name, "schema": "main"}

    with path.open("w", encoding="utf-8", newline="\n") as stream:
        stream.write('{"edges":[\n')
        first = True
        for number in range(vertices):
            identifier = node(number)["id"]
            edges = [{"from": identifier, "kind": "reads", "to": "main.root"}]
            edges.extend(
                {
                    "from": identifier,
                    "kind": "reads",
                    "to": f"main.node_{(number - offset) % vertices:06d}",
                }
                for offset in range(1, degree + 1)
            )
            for edge in edges:
                if not first:
                    stream.write(",\n")
                stream.write(json_line(edge))
                first = False
        stream.write('],"limitations":[],"version":2,"vertices":[\n')
        stream.write(json_line(root))
        for number in range(vertices):
            stream.write(",\n" + json_line(node(number)))
        stream.write("]}\n")
    return path


def dense_workloads(vertices: list[int], degree: int | None, max_edges: int) -> list[dict[str, int | str]]:
    """두 개 이상의 dense scale에 대해 degree·edge cap을 먼저 확정한다."""
    if len(vertices) < 2:
        fail("scale validation requires at least two dense workload sizes")
    workloads: list[dict[str, int | str]] = []
    labels: set[str] = set()
    for count in vertices:
        selected_degree = (
            min(count - 1, MAX_ENGINE_EXAMINED_EDGES // count - 1)
            if degree is None
            else degree
        )
        if count < 2 or selected_degree < 1 or selected_degree >= count:
            fail("dense workload requires 2 <= vertices and 1 <= degree < vertices")
        edges = count * (selected_degree + 1)
        if edges > max_edges:
            fail(f"dense workload has {edges} directed edges, above the {max_edges}-edge bound")
        label = f"v{count}-d{selected_degree}"
        if label in labels:
            fail(f"duplicate dense workload {label}")
        labels.add(label)
        workloads.append({"label": label, "vertices": count, "degree": selected_degree, "edges": edges})
    return workloads


def expected_dense_impact(
    vertices: int,
    degree: int,
    max_examined_edges: int = MAX_ENGINE_EXAMINED_EDGES,
) -> dict[str, object]:
    """dense root impact의 독립적인 complete/result-limit 기대값을 만든다."""
    graph_edges = vertices * (degree + 1)
    examined_edges = min(graph_edges, max_examined_edges)
    edge_limited = graph_edges > max_examined_edges
    return {
        "complete": not edge_limited,
        "truncated": True,
        "visited": vertices + 1,
        "examined_edges": examined_edges,
        "truncation_reasons": ["edge-limit", "result-limit"] if edge_limited else ["result-limit"],
        "reported_impacted": 1,
        "impacted_ids": ["main.node_000000"],
        "distances": [1],
        "edge_kinds": [["reads"]],
        "subject": "main.root",
    }


def expected_dense_full_impact(
    vertices: int,
    degree: int,
    max_examined_edges: int = MAX_ENGINE_EXAMINED_EDGES,
) -> dict[str, object]:
    """edge cap 안의 dense impact가 모든 정점을 반환해야 하는 기대값을 만든다."""
    graph_edges = vertices * (degree + 1)
    if graph_edges > max_examined_edges:
        fail("full dense impact validation requires the entire graph within the edge cap")
    impacted_ids = [f"main.node_{number:06d}" for number in range(vertices)]
    return {
        "complete": True,
        "truncated": False,
        "visited": vertices + 1,
        "examined_edges": graph_edges,
        "truncation_reasons": [],
        "reported_impacted": vertices,
        "impacted_ids": impacted_ids,
        "distances": [1] * vertices,
        "edge_kinds": [["reads"] for _number in range(vertices)],
        "subject": "main.root",
    }


def graph_facts(
    path: Path,
    *,
    expected_vertices: set[tuple[str, str]] | None = None,
    expected_edges: set[tuple[str, str, str]] | None = None,
) -> dict[str, object]:
    """graph JSON의 endpoint·중복 id와 독립 topology를 검증한다."""
    try:
        value = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as error:
        fail(f"{path.name} is not valid graph JSON: {error}")
    if not isinstance(value, dict) or value.get("version") != 2:
        fail(f"{path.name} is not a graph v2 object")
    vertices = value.get("vertices")
    edges = value.get("edges")
    if not isinstance(vertices, list) or not isinstance(edges, list):
        fail(f"{path.name} lacks vertices/edges arrays")
    ids = [item.get("id") for item in vertices if isinstance(item, dict)]
    if len(ids) != len(vertices) or any(not isinstance(item, str) for item in ids):
        fail(f"{path.name} contains a malformed vertex")
    if len(set(ids)) != len(ids):
        fail(f"{path.name} contains duplicate vertex ids")
    endpoints = set(ids)
    vertex_pairs: set[tuple[str, str]] = set()
    vertex_kind_counts: dict[str, int] = {}
    for item in vertices:
        if not isinstance(item, dict) or not isinstance(item.get("kind"), str):
            fail(f"{path.name} contains a vertex without a kind")
        pair = (item["id"], item["kind"])
        vertex_pairs.add(pair)
        vertex_kind_counts[item["kind"]] = vertex_kind_counts.get(item["kind"], 0) + 1
    dangling = 0
    edge_triples: set[tuple[str, str, str]] = set()
    edge_kind_counts: dict[str, int] = {}
    for edge in edges:
        if (
            not isinstance(edge, dict)
            or not isinstance(edge.get("from"), str)
            or not isinstance(edge.get("to"), str)
            or not isinstance(edge.get("kind"), str)
        ):
            fail(f"{path.name} contains a malformed edge")
        if edge["from"] not in endpoints or edge["to"] not in endpoints:
            dangling += 1
        edge_triples.add((edge["from"], edge["kind"], edge["to"]))
        edge_kind_counts[edge["kind"]] = edge_kind_counts.get(edge["kind"], 0) + 1
    if dangling:
        fail(f"{path.name} contains {dangling} dangling edges")
    limitations = value.get("limitations", [])
    if not isinstance(limitations, list) or any(not isinstance(item, str) for item in limitations):
        fail(f"{path.name} contains malformed limitations")
    if expected_vertices is not None:
        missing = expected_vertices - vertex_pairs
        extra = vertex_pairs - expected_vertices
        if missing or extra or len(vertices) != len(expected_vertices):
            fail(
                f"{path.name} vertex topology mismatch: missing={sorted(missing)[:3]} "
                f"extra={sorted(extra)[:3]} actual={len(vertices)} expected={len(expected_vertices)}"
            )
    if expected_edges is not None:
        missing = expected_edges - edge_triples
        extra = edge_triples - expected_edges
        if missing or extra or len(edges) != len(expected_edges):
            fail(
                f"{path.name} edge topology mismatch: missing={sorted(missing)[:3]} "
                f"extra={sorted(extra)[:3]} actual={len(edges)} expected={len(expected_edges)}"
            )
    return {
        "vertices": len(vertices),
        "edges": len(edges),
        "limitations": len(limitations),
        "dangling_edges": dangling,
        "root_present": "main.root" in endpoints,
        "vertex_kind_counts": dict(sorted(vertex_kind_counts.items())),
        "edge_kind_counts": dict(sorted(edge_kind_counts.items())),
    }


def bytes_to_mib(value: int) -> float:
    """사람이 읽는 report에 사용할 MiB 수를 반올림한다."""
    return round(value / MIB, 3)


def rss_multiplier() -> int:
    """플랫폼별 ru_maxrss 단위를 bytes로 변환할 배수를 고른다."""
    return 1 if sys.platform == "darwin" else 1024


def stderr_tail(path: Path, limit: int = 1200) -> str:
    """실패 원인에 필요한 마지막 stderr만 읽고 민감한 전체 로그는 보존하지 않는다."""
    try:
        value = path.read_bytes()[-limit:]
    except OSError:
        return ""
    return value.decode("utf-8", errors="replace").strip()


def process_cpu_seconds(pid: int) -> float | None:
    """idle 전후 CPU time을 읽어 MCP worker가 실제로 실행됐는지 보조 확인한다."""
    result = subprocess.run(
        ["ps", "-p", str(pid), "-o", "cputime="],
        capture_output=True,
        text=True,
        check=False,
    )
    if result.returncode != 0:
        return None
    value = result.stdout.strip()
    if not value:
        return None
    try:
        days = 0.0
        if "-" in value:
            day_text, value = value.split("-", 1)
            days = float(day_text)
        parts = [float(part) for part in value.split(":")]
        if len(parts) == 3:
            hours, minutes, seconds = parts
        elif len(parts) == 2:
            hours = 0.0
            minutes, seconds = parts
        else:
            return None
        return days * 86400.0 + hours * 3600.0 + minutes * 60.0 + seconds
    except ValueError:
        return None


def reap_with_usage(process: subprocess.Popen[bytes], deadline: float) -> tuple[int, int]:
    """wait4 polling으로 timeout과 자식별 peak RSS를 함께 관리한다."""
    while True:
        waited, status, usage = os.wait4(process.pid, os.WNOHANG)
        if waited:
            return os.waitstatus_to_exitcode(status), usage.ru_maxrss * rss_multiplier()
        if time.monotonic() >= deadline:
            try:
                os.kill(process.pid, signal.SIGTERM)
            except ProcessLookupError:
                pass
            kill_deadline = time.monotonic() + 2.0
            while time.monotonic() < kill_deadline:
                waited, status, usage = os.wait4(process.pid, os.WNOHANG)
                if waited:
                    fail("process exceeded its timeout")
                time.sleep(0.01)
            try:
                os.kill(process.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            os.wait4(process.pid, 0)
            fail("process exceeded its timeout and was killed")
        time.sleep(min(PROCESS_POLL_SECONDS, max(0.0001, deadline - time.monotonic())))


def run_measured(
    command: list[str],
    *,
    timeout: float,
    stderr_path: Path,
    stdout_path: Path | None = None,
) -> Measurement:
    """하나의 bounded child를 실행하고 wall/RSS를 측정한다."""
    stderr_path.parent.mkdir(parents=True, exist_ok=True)
    stdout: int | BinaryIO = subprocess.DEVNULL
    opened_stdout: BinaryIO | None = None
    if stdout_path is not None:
        opened_stdout = stdout_path.open("wb")
        stdout = opened_stdout
    started = time.perf_counter()
    with stderr_path.open("wb") as errors:
        process = subprocess.Popen(command, stdout=stdout, stderr=errors)
        try:
            returncode, peak_rss = reap_with_usage(process, started + timeout)
        finally:
            if opened_stdout is not None:
                opened_stdout.close()
    seconds = time.perf_counter() - started
    if returncode != 0:
        detail = stderr_tail(stderr_path)
        suffix = f": {detail}" if detail else ""
        fail(f"command failed with exit code {returncode}: {Path(command[0]).name}{suffix}")
    return Measurement(seconds=seconds, peak_rss_bytes=peak_rss)


def percentile(values: list[float], fraction: float) -> float:
    """nearest-rank p95를 작은 표본에서도 결정적으로 계산한다."""
    ordered = sorted(values)
    index = max(0, min(len(ordered) - 1, int((len(ordered) * fraction + 0.999999) - 1)))
    return ordered[index]


def summarize(samples: Iterable[Measurement]) -> dict[str, object]:
    """시간 p50/p95/max와 RSS p50/max를 같은 표본에서 요약한다."""
    values = list(samples)
    if not values:
        fail("cannot summarize an empty measurement set")
    seconds = [item.seconds for item in values]
    rss = [item.peak_rss_bytes for item in values]
    return {
        "count": len(values),
        "seconds": {
            "median": round(statistics.median(seconds), 6),
            "p95": round(percentile(seconds, 0.95), 6),
            "max": round(max(seconds), 6),
        },
        "peak_rss_bytes": {
            "median": int(statistics.median(rss)),
            "max": int(max(rss)),
            "median_mib": bytes_to_mib(int(statistics.median(rss))),
            "max_mib": bytes_to_mib(int(max(rss))),
        },
    }


def summarize_values(values: Iterable[float]) -> dict[str, object]:
    """RSS가 없는 취소 latency 표본에도 p50/p95/max를 적용한다."""
    samples = list(values)
    if not samples:
        fail("cannot summarize an empty latency set")
    return {
        "count": len(samples),
        "median": round(statistics.median(samples), 6),
        "p95": round(percentile(samples, 0.95), 6),
        "max": round(max(samples), 6),
    }


def check_rss(measurement: Measurement, max_rss_mib: int, label: str) -> None:
    """report가 선언한 RSS 상한을 넘은 측정을 조용히 성공으로 두지 않는다."""
    if measurement.peak_rss_bytes > max_rss_mib * MIB:
        fail(f"{label} peak RSS exceeded the configured {max_rss_mib} MiB bound")


def check_input_size(paths: Iterable[Path], max_input_bytes: int) -> int:
    """생성 파일 합계를 측정 전에 검사해 입력 크기를 bounded 상태로 둔다."""
    total = sum(path.stat().st_size for path in paths)
    if total > max_input_bytes:
        fail(f"generated inputs total {total} bytes, above the {max_input_bytes}-byte bound")
    return total


def run_large_schema(
    engine: Path,
    work: Path,
    *,
    inputs: dict[str, Path],
    objects: int,
    columns: int,
    repeat: int,
    timeout: float,
    max_rss_mib: int,
    max_input_bytes: int,
) -> dict[str, object]:
    """같은 대형 schema의 JSON/NDJSON scan을 반복하고 graph parity를 검증한다."""
    check_input_size(inputs.values(), max_input_bytes)
    expected_vertices, expected_edges, expected_summary = expected_large_topology(objects, columns)
    format_reports: dict[str, object] = {}
    expected_hash: str | None = None
    expected_facts: dict[str, object] | None = None
    for format_name, input_path in inputs.items():
        measurements: list[Measurement] = []
        runs: list[dict[str, object]] = []
        input_hash = sha256(input_path)
        for run in range(repeat):
            output = work / f"large-{format_name}-{run}.graph.json"
            measurement = run_measured(
                [str(engine), "scan", "--document", str(input_path), "-o", str(output)],
                timeout=timeout,
                stderr_path=work / f"large-{format_name}-{run}.stderr",
            )
            check_rss(measurement, max_rss_mib, f"large {format_name} run {run}")
            output_hash = sha256(output)
            facts = graph_facts(
                output,
                expected_vertices=expected_vertices,
                expected_edges=expected_edges,
            )
            if expected_hash is None:
                expected_hash, expected_facts = output_hash, facts
            elif output_hash != expected_hash or facts != expected_facts:
                fail(f"large schema graph changed between formats or repeats ({format_name}, run {run})")
            measurements.append(measurement)
            runs.append(
                {
                    "run": run,
                    "output_bytes": output.stat().st_size,
                    "output_sha256": output_hash,
                    "facts": facts,
                    "seconds": round(measurement.seconds, 6),
                    "peak_rss_bytes": measurement.peak_rss_bytes,
                }
            )
        format_reports[format_name] = {
            "input_bytes": input_path.stat().st_size,
            "input_sha256": input_hash,
            "runs": runs,
            "summary": summarize(measurements),
        }
    return {
        "objects": objects,
        "columns": columns,
        "input_total_bytes": check_input_size(inputs.values(), max_input_bytes),
        "graph_sha256": expected_hash,
        "graph_facts": expected_facts,
        "expected_graph_facts": expected_summary,
        "formats": format_reports,
    }


def process_output(path: Path) -> dict[str, object]:
    """CLI JSON 결과를 읽어 cancellation/complete 계약 검증에 사용한다."""
    try:
        value = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as error:
        fail(f"CLI did not emit one JSON object: {error}")
    if not isinstance(value, dict):
        fail("CLI output is not a JSON object")
    return value


def assert_impact(
    value: dict[str, object],
    *,
    expected: dict[str, object] | None = None,
    allow_cancelled: bool = False,
) -> dict[str, object]:
    """CLI/MCP impact 결과에서 독립적으로 계산한 탐색 사실을 검증한다."""
    impacted = value.get("impacted")
    if not isinstance(impacted, list) or not isinstance(value.get("visited"), int):
        fail("impact output lacks impacted/visited facts")
    reasons = value.get("truncationReasons")
    if not isinstance(reasons, list) or any(not isinstance(item, str) for item in reasons):
        fail("impact output has malformed truncationReasons")
    if allow_cancelled:
        if "cancelled" not in reasons or value.get("complete") is not False:
            fail("cancelled impact did not report complete=false and cancelled")
    elif not isinstance(value.get("complete"), bool) or not isinstance(
        value.get("truncated"), bool
    ):
        fail(f"reference impact has invalid completion facts: {value}")
    impacted_ids: list[str] = []
    distances: list[int] = []
    edge_kinds: list[list[str]] = []
    for item in impacted:
        if (
            not isinstance(item, dict)
            or not isinstance(item.get("id"), str)
            or not isinstance(item.get("distance"), int)
            or not isinstance(item.get("edges"), list)
            or any(not isinstance(kind, str) for kind in item["edges"])
        ):
            fail("impact output contains malformed neighbor facts")
        impacted_ids.append(item["id"])
        distances.append(item["distance"])
        edge_kinds.append(item["edges"])
    subject = value.get("subject")
    subject_id = subject.get("id") if isinstance(subject, dict) else None
    facts = {
        "complete": value.get("complete"),
        "truncated": value.get("truncated"),
        "visited": value["visited"],
        "examined_edges": value.get("examinedEdges"),
        "truncation_reasons": reasons,
        "reported_impacted": len(impacted),
        "impacted_ids": impacted_ids,
        "distances": distances,
        "edge_kinds": edge_kinds,
        "subject": subject_id,
    }
    if expected is not None and facts != expected:
        fail(f"impact topology facts mismatch: actual={facts} expected={expected}")
    return facts


def run_cli_queries(
    engine: Path,
    graph: Path,
    *,
    vertices: int,
    degree: int,
    repeat: int,
    timeout: float,
    max_rss_mib: int,
    work: Path,
) -> dict[str, object]:
    """프로세스 재기동을 포함한 CLI impact 비용을 반복 측정한다."""
    measurements: list[Measurement] = []
    runs: list[dict[str, object]] = []
    expected = expected_dense_impact(vertices, degree)
    command = [
        str(engine),
        "impact",
        "main.root",
        "--graph",
        str(graph),
        "--max",
        "1",
        "--max-visited",
        str(vertices + 1),
        "--max-examined-edges",
        str(expected["examined_edges"]),
    ]
    expected_hash: str | None = None
    expected_facts: dict[str, object] | None = None
    for run in range(repeat):
        output = work / f"cli-impact-{run}.json"
        measurement = run_measured(
            command,
            timeout=timeout,
            stderr_path=work / f"cli-impact-{run}.stderr",
            stdout_path=output,
        )
        check_rss(measurement, max_rss_mib, f"CLI impact run {run}")
        value = process_output(output)
        facts = assert_impact(value, expected=expected)
        output_hash = sha256(output)
        if expected_hash is None:
            expected_hash, expected_facts = output_hash, facts
        elif output_hash != expected_hash or facts != expected_facts:
            fail(f"CLI impact output changed on repeat {run}")
        measurements.append(measurement)
        runs.append(
            {
                "run": run,
                "output_sha256": output_hash,
                "facts": facts,
                "seconds": round(measurement.seconds, 6),
                "peak_rss_bytes": measurement.peak_rss_bytes,
            }
        )
    return {"runs": runs, "summary": summarize(measurements), "output_sha256": expected_hash, "facts": expected_facts}


class McpSession:
    """MCP stdio session의 startup, response matching, bounded cleanup을 소유한다."""

    def __init__(self, engine: Path, graph: Path, timeout: float):
        self.timeout = timeout
        self.process = subprocess.Popen(
            [str(engine), "serve", "--graph", str(graph)],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
        )
        assert self.process.stdin is not None
        assert self.process.stdout is not None
        assert self.process.stderr is not None
        self.selector = selectors.DefaultSelector()
        self.selector.register(self.process.stdout, selectors.EVENT_READ, "stdout")
        self.selector.register(self.process.stderr, selectors.EVENT_READ, "stderr")
        self.buffer = bytearray()
        self.stderr = bytearray()
        self.pending: dict[object, list[dict[str, object]]] = {}

    def close(self) -> Measurement:
        """stdin을 닫고 수락한 요청을 drain한 뒤 wait4로 RSS를 얻는다."""
        if self.process.stdin is not None and not self.process.stdin.closed:
            self.process.stdin.close()
        started = time.perf_counter()
        try:
            returncode, peak_rss = reap_with_usage(self.process, started + self.timeout)
        finally:
            self.selector.close()
            if self.process.stdout is not None:
                self.process.stdout.close()
            if self.process.stderr is not None:
                self.process.stderr.close()
        if returncode != 0:
            fail(f"MCP server exited with code {returncode}: {bytes(self.stderr).decode(errors='replace')[-800:]}")
        return Measurement(seconds=time.perf_counter() - started, peak_rss_bytes=peak_rss)

    def terminate(self) -> None:
        """예외 뒤에 남은 MCP child를 bounded하게 정리한다."""
        if self.process.poll() is None:
            self.process.terminate()
            try:
                self.process.wait(timeout=2)
            except subprocess.TimeoutExpired:
                self.process.kill()
                self.process.wait()
        self.selector.close()

    def send(self, value: dict[str, object]) -> float:
        """JSON-RPC 한 줄을 flush하고 전송 monotonic 시각을 반환한다."""
        assert self.process.stdin is not None
        try:
            self.process.stdin.write((json_line(value) + "\n").encode())
            self.process.stdin.flush()
        except OSError as error:
            fail(f"could not write MCP request: {error}")
        return time.monotonic()

    def _read_events(self, end: float) -> list[dict[str, object]]:
        """stdout/stderr를 읽어 JSON-RPC response를 한 번에 반환한다."""
        messages: list[dict[str, object]] = []
        while time.monotonic() < end:
            events = self.selector.select(max(0.0, min(end - time.monotonic(), 0.05)))
            if not events:
                if self.process.poll() is not None:
                    fail(f"MCP server exited before response: {bytes(self.stderr)!r}")
                continue
            for key, _mask in events:
                chunk = os.read(key.fileobj.fileno(), 65536)
                if not chunk:
                    if key.data == "stdout":
                        fail(f"MCP stdout closed before response: {bytes(self.stderr)!r}")
                    self.selector.unregister(key.fileobj)
                    continue
                if key.data == "stderr":
                    self.stderr.extend(chunk)
                    del self.stderr[:-4096]
                    continue
                self.buffer.extend(chunk)
                while b"\n" in self.buffer:
                    raw, _separator, rest = bytes(self.buffer).partition(b"\n")
                    self.buffer = bytearray(rest)
                    try:
                        message = json.loads(raw.decode("utf-8"))
                    except (UnicodeDecodeError, json.JSONDecodeError) as error:
                        fail(f"MCP emitted invalid JSON: {error}")
                    if isinstance(message, dict) and "id" in message:
                        messages.append(message)
            if messages:
                return messages
        return messages

    def stash(self, messages: Iterable[dict[str, object]]) -> None:
        """현재 요청이 아닌 JSON-RPC 응답을 다음 id 대기로 보존한다."""
        for message in messages:
            self.pending.setdefault(message.get("id"), []).append(message)

    def poll_messages(self, timeout: float) -> list[dict[str, object]]:
        """active proof 중 도착한 응답을 읽고 id별 대기열에도 보존한다."""
        messages = self._read_events(time.monotonic() + timeout)
        self.stash(messages)
        return messages

    def wait_for(self, request_id: object, timeout: float | None = None) -> tuple[dict[str, object], float]:
        """id가 같은 응답을 deadline 안에 찾아 response 시각과 반환한다."""
        queued = self.pending.get(request_id)
        if queued:
            return queued.pop(0), time.monotonic()
        end = time.monotonic() + (self.timeout if timeout is None else timeout)
        while time.monotonic() < end:
            for message in self._read_events(end):
                if message.get("id") == request_id:
                    return message, time.monotonic()
                self.stash([message])
        fail(f"timed out waiting for MCP response id {request_id!r}")

    def try_wait_for(self, request_id: object, timeout: float) -> tuple[dict[str, object], float] | None:
        """짧은 proof window에서 응답이 이미 왔는지 검사한다."""
        queued = self.pending.get(request_id)
        if queued:
            return queued.pop(0), time.monotonic()
        end = time.monotonic() + timeout
        while time.monotonic() < end:
            for message in self._read_events(end):
                if message.get("id") == request_id:
                    return message, time.monotonic()
                self.stash([message])
        return None


def initialize_mcp(session: McpSession) -> float:
    """initialize 응답을 확인해 이후 측정이 graph load 뒤 시작되게 한다."""
    sent = session.send(
        {
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-11-25",
                "capabilities": {},
                "clientInfo": {"name": "benchmark-scale", "version": "1"},
            },
        }
    )
    response, received = session.wait_for(1)
    if "error" in response or "result" not in response:
        fail(f"MCP initialize failed: {response}")
    session.send({"jsonrpc": "2.0", "method": "notifications/initialized"})
    return received - sent


def mcp_impact_call(
    session: McpSession,
    request_id: int,
    vertices: int,
    degree: int,
    *,
    expected: dict[str, object] | None = None,
    max_results: int = 1,
) -> tuple[float, dict[str, object]]:
    """impact request를 보내고 structuredContent의 complete 사실을 확인한다."""
    sent = session.send(
        {
            "jsonrpc": "2.0",
            "id": request_id,
            "method": "tools/call",
            "params": {
                "name": "impact",
                "arguments": {
                    "name": "main.root",
                    "max": max_results,
                    "maxVisited": vertices + 1,
                    "maxExaminedEdges": expected["examined_edges"]
                    if expected is not None
                    else vertices * (degree + 1),
                },
            },
        }
    )
    response, received = session.wait_for(request_id)
    result = response.get("result")
    if "error" in response or not isinstance(result, dict) or result.get("isError") is not False:
        fail(f"MCP impact failed: {response}")
    structured = result.get("structuredContent")
    if not isinstance(structured, dict):
        fail(f"MCP impact omitted structuredContent: {response}")
    facts = assert_impact(structured, expected=expected)
    return received - sent, facts


def validate_full_mcp_impact(
    engine: Path,
    graph: Path,
    *,
    vertices: int,
    degree: int,
    timeout: float,
) -> dict[str, object]:
    """취소 시간 밖에서 dense impact의 전체 id 집합을 한 번 검증한다."""
    session = McpSession(engine, graph, timeout)
    expected = expected_dense_full_impact(vertices, degree)
    try:
        initialize_mcp(session)
        seconds, facts = mcp_impact_call(
            session,
            19_999,
            vertices,
            degree,
            expected=expected,
            max_results=vertices,
        )
        close_measurement = session.close()
        return {
            "outside_cancellation_timing": True,
            "seconds": round(seconds, 6),
            "facts": facts,
            "peak_rss_bytes": close_measurement.peak_rss_bytes,
        }
    except Exception:
        session.terminate()
        raise


def observe_active_request(
    session: McpSession,
    active_id: int,
    cpu_before: float | None,
) -> tuple[float, float, float]:
    """응답 전 실제 process CPU 진행을 기다려 active-request 경계를 증명한다."""
    if cpu_before is None:
        fail("process CPU time is unavailable; cannot prove MCP active-request work")
    started = time.monotonic()
    deadline = started + min(session.timeout, ACTIVE_PROOF_TIMEOUT)
    cpu_after = cpu_before
    while time.monotonic() < deadline:
        remaining = deadline - time.monotonic()
        session.poll_messages(min(ACTIVE_PROOF_POLL_SECONDS, max(0.0, remaining)))
        if session.pending.get(active_id):
            fail("MCP active request completed before CPU progress proof")
        observed = process_cpu_seconds(session.process.pid)
        if observed is None:
            fail("process CPU time became unavailable during MCP active-request proof")
        cpu_after = observed
        cpu_delta = cpu_after - cpu_before
        if cpu_delta + 1e-9 >= ACTIVE_CPU_PROGRESS_SECONDS:
            session.poll_messages(ACTIVE_PROOF_POLL_SECONDS)
            if session.pending.get(active_id):
                fail("MCP active request completed before cancellation")
            return cpu_after, time.monotonic() - started, cpu_delta
    fail(
        "MCP active-request proof observed no required CPU progress "
        f"({cpu_after - cpu_before:.6f}s < {ACTIVE_CPU_PROGRESS_SECONDS:.6f}s)"
    )


def assert_worker_bound_followup_request(
    request: dict[str, object],
) -> dict[str, object]:
    """worker-unblock 증거가 reader thread의 control 응답을 쓰지 못하게 한다."""
    if request.get("method") == "ping":
        fail("ping bypasses the analysis worker and cannot prove worker unblock")
    params = request.get("params")
    if (
        request.get("method") != "tools/call"
        or not isinstance(params, dict)
        or params.get("name") != "impact"
    ):
        fail("worker follow-up proof requires an impact tools/call request")
    return request


def mcp_worker_followup_call(
    session: McpSession,
    request_id: int,
) -> tuple[float, float, dict[str, object]]:
    """analysis worker를 거치는 missing impact의 정확한 응답을 검증한다."""
    missing_name = "main.__benchmark_missing__"
    request = assert_worker_bound_followup_request(
        {
            "jsonrpc": "2.0",
            "id": request_id,
            "method": "tools/call",
            "params": {
                "name": "impact",
                "arguments": {"name": missing_name},
            },
        }
    )
    sent = session.send(request)
    response, received = session.wait_for(request_id)
    result = response.get("result")
    expected = {
        "candidates": [],
        "found": False,
        "limitations": [],
        "name": missing_name,
    }
    if (
        "error" in response
        or not isinstance(result, dict)
        or result.get("isError") is not True
        or result.get("structuredContent") != expected
    ):
        fail(f"MCP worker follow-up did not return the expected not-found result: {response}")
    return sent, received, expected


def run_mcp_queries(
    engine: Path,
    graph: Path,
    *,
    vertices: int,
    degree: int,
    repeat: int,
    timeout: float,
    max_rss_mib: int,
) -> dict[str, object]:
    """한 resident MCP process에서 startup과 request latency를 분리 측정한다."""
    session = McpSession(engine, graph, timeout)
    expected = expected_dense_impact(vertices, degree)
    request_seconds: list[float] = []
    responses: list[dict[str, object]] = []
    expected_facts: dict[str, object] | None = None
    startup_seconds = None
    try:
        startup_seconds = initialize_mcp(session)
        for request_id in range(2, repeat + 2):
            seconds, facts = mcp_impact_call(
                session, request_id, vertices, degree, expected=expected
            )
            if expected_facts is None:
                expected_facts = facts
            elif facts != expected_facts:
                fail(f"MCP impact facts changed on repeat {request_id}")
            request_seconds.append(seconds)
            responses.append({"request_id": request_id, "seconds": round(seconds, 6), "facts": facts})
        close_measurement = session.close()
    except Exception:
        session.terminate()
        raise
    check_rss(close_measurement, max_rss_mib, "resident MCP process")
    samples = [Measurement(seconds=item, peak_rss_bytes=close_measurement.peak_rss_bytes) for item in request_seconds]
    return {
        "startup_seconds": round(startup_seconds, 6),
        "requests": responses,
        "facts": expected_facts,
        "expected_facts": expected,
        "request_summary": summarize(samples),
        "process_peak_rss_bytes": close_measurement.peak_rss_bytes,
    }


def open_fifo_writer(process: subprocess.Popen[bytes], path: Path, timeout: float) -> int:
    """reader가 FIFO를 열 때까지 bounded nonblocking writer open을 반복한다."""
    end = time.monotonic() + timeout
    while time.monotonic() < end:
        try:
            descriptor = os.open(path, os.O_WRONLY | os.O_NONBLOCK)
            os.set_blocking(descriptor, True)
            return descriptor
        except OSError as error:
            if error.errno != errno.ENXIO:
                raise
            if process.poll() is not None:
                fail(f"engine exited {process.returncode} before opening the graph FIFO")
            time.sleep(0.01)
    fail("timed out waiting for the engine to open the graph FIFO")


def wait_for_ack(
    process: subprocess.Popen[bytes], started_at: float, timeout: float
) -> float:
    """첫 SIGINT의 stderr ACK를 읽고 caller와 같은 축의 수신 시각을 반환한다."""
    assert process.stderr is not None
    selector = selectors.DefaultSelector()
    selector.register(process.stderr, selectors.EVENT_READ)
    pending = bytearray()
    deadline = started_at + timeout
    try:
        while time.monotonic() < deadline:
            if b"\n" in pending:
                line, _separator, _rest = bytes(pending).partition(b"\n")
                actual = line + b"\n"
                if actual != ACK:
                    fail(f"unexpected CLI cancellation stderr: {actual!r}")
                return time.monotonic()
            events = selector.select(min(0.05, max(0.0, deadline - time.monotonic())))
            if not events:
                continue
            chunk = os.read(process.stderr.fileno(), 4096)
            if not chunk:
                fail("CLI closed stderr before cancellation acknowledgement")
            pending.extend(chunk)
        fail("timed out waiting for CLI cancellation acknowledgement")
    finally:
        selector.close()


def copy_file_to_fd(source: Path, descriptor: int) -> None:
    """대형 FIFO payload를 일정한 chunk로 보내 부모 메모리 복제를 피한다."""
    with source.open("rb") as stream:
        while True:
            chunk = stream.read(1024 * 1024)
            if not chunk:
                return
            offset = 0
            while offset < len(chunk):
                offset += os.write(descriptor, chunk[offset:])


def run_cli_input_cancellation_once(
    engine: Path,
    graph: Path,
    *,
    timeout: float,
    work: Path,
    run: int,
) -> dict[str, object]:
    """FIFO bytes/EOF 전 input read에서 SIGINT를 보내 load/input phase를 증명한다."""
    fifo = work / f"cancel-{run}.graph.fifo"
    os.mkfifo(fifo)
    process = subprocess.Popen(
        [str(engine), "impact", "main.root", "--graph", str(fifo), "--max", "1"],
        stdin=subprocess.DEVNULL,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    )
    writer = -1
    try:
        writer = open_fifo_writer(process, fifo, timeout)
        sent = time.monotonic()
        os.kill(process.pid, signal.SIGINT)
        ack_received = wait_for_ack(process, sent, timeout)
        copy_file_to_fd(graph, writer)
        os.close(writer)
        writer = -1
        try:
            stdout, stderr = process.communicate(timeout=timeout)
        except subprocess.TimeoutExpired:
            process.kill()
            process.communicate()
            fail("CLI did not exit after input cancellation")
        if process.returncode != 130:
            detail = (stderr or stdout).decode("utf-8", errors="replace")[-800:]
            fail(f"CLI input cancellation exited {process.returncode}: {detail}")
        try:
            value = json.loads(stdout.decode("utf-8"))
        except (UnicodeDecodeError, json.JSONDecodeError) as error:
            fail(f"CLI input cancellation did not emit JSON: {error}")
        if not isinstance(value, dict):
            fail("CLI input cancellation output is not an object")
        facts = assert_impact(value, allow_cancelled=True)
        return {
            "run": run,
            "phase": "load-input",
            "signal_to_ack_seconds": round(ack_received - sent, 6),
            "signal_to_process_exit_seconds": round(time.monotonic() - sent, 6),
            "input_written_after_signal": True,
            "traversal_started_before_signal": False,
            "facts": facts,
        }
    finally:
        if writer >= 0:
            os.close(writer)
        if process.poll() is None:
            process.kill()
            process.wait()


def run_cli_input_cancellation(
    engine: Path,
    graph: Path,
    *,
    repeat: int,
    timeout: float,
    work: Path,
) -> dict[str, object]:
    """CLI 입력 취소를 반복해 ACK/exit tail을 요약한다."""
    runs = [
        run_cli_input_cancellation_once(engine, graph, timeout=timeout, work=work, run=run)
        for run in range(repeat)
    ]
    return {
        "runs": runs,
        "summary": {
            "signal_to_ack_seconds": summarize_values(
                [float(item["signal_to_ack_seconds"]) for item in runs]
            ),
            "signal_to_process_exit_seconds": summarize_values(
                [float(item["signal_to_process_exit_seconds"]) for item in runs]
            ),
        },
    }


def run_mcp_active_cancellation_once(
    engine: Path,
    graph: Path,
    *,
    vertices: int,
    degree: int,
    timeout: float,
    reference_seconds: float,
    run: int,
) -> dict[str, object]:
    """idle worker의 active CPU 진행 뒤 취소하고 worker-bound call로 복귀를 증명한다."""
    session = McpSession(engine, graph, timeout)
    active_id = 20_001 + (run * 3)
    followup_id = active_id + 1
    try:
        initialize_mcp(session)
        preflight_id = 20_000
        expected = expected_dense_impact(vertices, degree)
        preflight_seconds, preflight_facts = mcp_impact_call(
            session, preflight_id, vertices, degree, expected=expected
        )
        if reference_seconds < 0.01:
            fail("reference traversal is too short to establish an in-flight cancellation window")
        cpu_before = process_cpu_seconds(session.process.pid)
        active_sent = session.send(
            {
                "jsonrpc": "2.0",
                "id": active_id,
                "method": "tools/call",
                "params": {
                    "name": "impact",
                    "arguments": {
                        "name": "main.root",
                        "max": expected["visited"] - 1,
                        "maxVisited": expected["visited"],
                        "maxExaminedEdges": expected["examined_edges"],
                    },
                },
            }
        )
        cpu_after, active_observation, cpu_delta = observe_active_request(
            session, active_id, cpu_before
        )
        cancel_started = time.monotonic()
        cancel_written = session.send(
            {
                "jsonrpc": "2.0",
                "method": "notifications/cancelled",
                "params": {"requestId": active_id, "reason": "benchmark"},
            }
        )
        followup_sent, followup_received, followup_facts = mcp_worker_followup_call(
            session, followup_id
        )
        active_response = session.try_wait_for(active_id, 0.05)
        if active_response is not None:
            fail("canceled MCP active request unexpectedly returned a response")
        close_measurement = session.close()
        return {
            "run": run,
            "phase": "active-request",
            "preflight_seconds": round(preflight_seconds, 6),
            "preflight_facts": preflight_facts,
            "reference_seconds": round(reference_seconds, 6),
            "process_cpu_before_seconds": cpu_before,
            "process_cpu_after_seconds": cpu_after,
            "process_cpu_delta_seconds": round(cpu_delta, 6),
            "active_request_was_only_tool_call_before_cancel": True,
            "active_request_submitted_before_followup": active_sent <= followup_sent,
            "worker_followup_submitted_after_cancel": cancel_written <= followup_sent,
            "cpu_progress_observed": True,
            "cpu_progress_required_seconds": ACTIVE_CPU_PROGRESS_SECONDS,
            "active_observation_seconds": round(active_observation, 6),
            "active_response_before_cancel": False,
            "cancel_notification_write_seconds": round(cancel_written - cancel_started, 6),
            "followup_worker_request": "impact-not-found",
            "followup_worker_facts": followup_facts,
            "followup_worker_request_seconds": round(
                followup_received - followup_sent, 6
            ),
            "cancel_to_followup_worker_seconds": round(
                followup_received - cancel_started, 6
            ),
            "cancelled_request_response": "suppressed",
            "in_flight_proof": (
                "idle preflight completed; process CPU advanced after the active request; "
                "no active response preceded cancellation; a worker-bound impact call followed"
            ),
            "peak_rss_bytes": close_measurement.peak_rss_bytes,
        }
    except Exception:
        session.terminate()
        raise


def run_mcp_active_cancellation(
    engine: Path,
    graph: Path,
    *,
    vertices: int,
    degree: int,
    repeat: int,
    timeout: float,
    reference_seconds: float,
) -> dict[str, object]:
    """MCP active 취소를 fresh resident process에서 반복해 latency tail을 요약한다."""
    full_validation = validate_full_mcp_impact(
        engine,
        graph,
        vertices=vertices,
        degree=degree,
        timeout=timeout,
    )
    runs = [
        run_mcp_active_cancellation_once(
            engine,
            graph,
            vertices=vertices,
            degree=degree,
            timeout=timeout,
            reference_seconds=reference_seconds,
            run=run,
        )
        for run in range(repeat)
    ]
    return {
        "full_result_validation": full_validation,
        "runs": runs,
        "summary": {
            "cancel_to_followup_worker_seconds": summarize_values(
                [float(item["cancel_to_followup_worker_seconds"]) for item in runs]
            ),
            "peak_rss_bytes": {
                "median": int(statistics.median(int(item["peak_rss_bytes"]) for item in runs)),
                "max": max(int(item["peak_rss_bytes"]) for item in runs),
                "median_mib": bytes_to_mib(
                    int(statistics.median(int(item["peak_rss_bytes"]) for item in runs))
                ),
                "max_mib": bytes_to_mib(max(int(item["peak_rss_bytes"]) for item in runs)),
            },
        },
    }


def bounded_args() -> argparse.Namespace:
    """CLI 인자를 생성하고 입력·반복·시간 상한을 초기에 검증한다."""
    parser = argparse.ArgumentParser(description="Measure bounded graph scale and cancellation phases.")
    parser.add_argument("--engine", type=Path, help="path to an executable schemagraph binary")
    parser.add_argument("--output", type=Path, default=REPORT_ROOT, help="external report directory")
    parser.add_argument("--large-objects", type=int, default=10_000, help="tables in the single large schema")
    parser.add_argument("--columns", type=int, default=8, help="columns per generated table")
    parser.add_argument(
        "--dense-vertices",
        type=int,
        nargs="+",
        default=DEFAULT_DENSE_VERTICES,
        help="one or more dense graph node counts",
    )
    parser.add_argument(
        "--dense-degree",
        type=int,
        default=None,
        help="repeated dependency edges per node; default is about half the graph",
    )
    parser.add_argument("--repeat", type=int, default=5, help="measurement repeats; at least 3")
    parser.add_argument("--timeout-seconds", type=float, default=DEFAULT_TIMEOUT, help="per-process/request timeout")
    parser.add_argument("--max-input-mib", type=int, default=512, help="maximum generated input total")
    parser.add_argument("--max-rss-mib", type=int, default=2048, help="maximum observed child peak RSS")
    parser.add_argument(
        "--replace-cancellation-in",
        type=Path,
        help="replace only cancellation measurements in an existing results.json",
    )
    parser.add_argument(
        "--unconfirmed-report-snapshot",
        type=Path,
        help="preserved pre-replacement report whose hash must match the input report",
    )
    parser.add_argument(
        "--unconfirmed-tool-snapshot",
        type=Path,
        help="preserved pre-replacement benchmark script recorded as unconfirmed provenance",
    )
    parser.add_argument("--smoke", action="store_true", help="validate generators and invariants without an engine")
    args = parser.parse_args()
    if args.large_objects < 1 or args.large_objects > MAX_OBJECTS:
        parser.error(f"--large-objects must be between 1 and {MAX_OBJECTS}")
    if args.columns < 2 or args.columns > 64:
        parser.error("--columns must be between 2 and 64")
    if len(args.dense_vertices) < 2 or len(args.dense_vertices) > 4:
        parser.error("--dense-vertices requires between two and four sizes")
    if any(count < 2 or count > MAX_DENSE_VERTICES for count in args.dense_vertices):
        parser.error(f"--dense-vertices values must be between 2 and {MAX_DENSE_VERTICES}")
    if args.dense_degree is not None and args.dense_degree < 1:
        parser.error("--dense-degree must be positive when provided")
    try:
        args.dense_workloads = dense_workloads(
            args.dense_vertices, args.dense_degree, MAX_DENSE_EDGES
        )
    except BenchmarkError as error:
        parser.error(str(error))
    if args.repeat < 3 or args.repeat > 30:
        parser.error("--repeat must be between 3 and 30")
    if args.timeout_seconds <= 0 or args.timeout_seconds > MAX_TIMEOUT:
        parser.error(f"--timeout-seconds must be between 0 and {MAX_TIMEOUT}")
    if args.max_input_mib < 1 or args.max_input_mib > 4096:
        parser.error("--max-input-mib must be between 1 and 4096")
    if args.max_rss_mib < 64 or args.max_rss_mib > 8192:
        parser.error("--max-rss-mib must be between 64 and 8192")
    replacement_snapshots = (
        args.unconfirmed_report_snapshot,
        args.unconfirmed_tool_snapshot,
    )
    if args.replace_cancellation_in is not None and any(
        snapshot is None for snapshot in replacement_snapshots
    ):
        parser.error(
            "--replace-cancellation-in requires --unconfirmed-report-snapshot "
            "and --unconfirmed-tool-snapshot"
        )
    if args.replace_cancellation_in is None and any(
        snapshot is not None for snapshot in replacement_snapshots
    ):
        parser.error("unconfirmed snapshots require --replace-cancellation-in")
    if args.smoke and args.replace_cancellation_in is not None:
        parser.error("--smoke cannot replace cancellation measurements")
    if not args.smoke and args.engine is None:
        parser.error("--engine is required unless --smoke is used")
    return args


def run_smoke(args: argparse.Namespace) -> dict[str, object]:
    """CI가 빠르게 실행할 생성기·shape·determinism invariant를 검사한다."""
    with tempfile.TemporaryDirectory(prefix="schemagraph-scale-smoke-") as directory:
        work = Path(directory)
        inputs = write_large_schema(work, 7, 3)
        graph = write_dense_graph(work, 11, 3, max_edges=MAX_DENSE_EDGES)
        facts = graph_facts(graph)
        if (
            facts["vertices"] != 12
            or facts["edges"] != 44
            or facts["dangling_edges"] != 0
            or facts["vertex_kind_counts"] != {"table": 1, "view": 11}
            or facts["edge_kind_counts"] != {"reads": 44}
        ):
            fail("dense graph smoke facts changed")
        if check_input_size(inputs.values(), 2 * MIB) <= 0:
            fail("catalog smoke inputs were empty")
        if sha256(inputs["json"]) == sha256(inputs["ndjson"]):
            fail("JSON and NDJSON smoke inputs unexpectedly share a hash")
        return {"status": "ok", "catalog_formats": sorted(inputs), "dense_graph_facts": facts}


def load_report(path: Path) -> dict[str, object]:
    """cancellation-only 재측정에 사용할 기존 report를 검증해 읽는다."""
    try:
        value = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as error:
        fail(f"could not read existing benchmark report {path}: {error}")
    if not isinstance(value, dict) or value.get("tool") != "benchmark-scale":
        fail(f"existing report is not a benchmark-scale report: {path}")
    return value


def report_dense_workload(
    report: dict[str, object], label: str
) -> tuple[dict[str, object], int, int, float]:
    """기존 report에서 cancellation workload와 reference 시간을 꺼낸다."""
    dense_graph = report.get("dense_graph")
    dense = dense_graph.get(label) if isinstance(dense_graph, dict) else None
    if not isinstance(dense, dict):
        fail(f"existing report lacks dense workload {label}")
    nodes = dense.get("nodes")
    degree = dense.get("degree")
    mcp = dense.get("mcp")
    summary = mcp.get("request_summary") if isinstance(mcp, dict) else None
    seconds = summary.get("seconds") if isinstance(summary, dict) else None
    reference = seconds.get("median") if isinstance(seconds, dict) else None
    if (
        not isinstance(nodes, int)
        or not isinstance(degree, int)
        or not isinstance(reference, (int, float))
    ):
        fail(f"existing report has malformed dense workload {label}")
    return dense, nodes, degree, float(reference)


def run_cancellation_replacement(args: argparse.Namespace) -> dict[str, object]:
    """검증된 scale 결과를 보존하고 CLI/MCP cancellation만 다시 측정한다."""
    engine = args.engine.resolve()
    if not engine.is_file() or not os.access(engine, os.X_OK):
        fail(f"engine is not an executable file: {engine}")
    report_path = args.replace_cancellation_in.resolve()
    report_snapshot = args.unconfirmed_report_snapshot.resolve()
    tool_snapshot = args.unconfirmed_tool_snapshot.resolve()
    for snapshot in (report_snapshot, tool_snapshot):
        if not snapshot.is_file():
            fail(f"unconfirmed snapshot does not exist: {snapshot}")
    if sha256(report_path) != sha256(report_snapshot):
        fail("existing report changed after the unconfirmed snapshot was preserved")
    report = load_report(report_path)
    engine_info = report.get("engine")
    if not isinstance(engine_info, dict) or engine_info.get("sha256") != sha256(engine):
        fail("replacement engine hash does not match the existing report")
    limits = report.get("limits")
    if not isinstance(limits, dict) or limits.get("repeat") != args.repeat:
        fail("replacement repeat count must match the existing report")
    cancellation = report.get("cancellation")
    label = cancellation.get("workload") if isinstance(cancellation, dict) else None
    if not isinstance(label, str):
        fail("existing report lacks a cancellation workload")
    dense, vertices, degree, reference_seconds = report_dense_workload(report, label)
    retained_hashes = {
        key: json_value_sha256(report[key])
        for key in ("large_schema", "dense_graph")
        if key in report
    }
    current_tool = Path(__file__).resolve()
    with tempfile.TemporaryDirectory(prefix="schemagraph-cancellation-replacement-") as directory:
        work = Path(directory)
        graph = write_dense_graph(work, vertices, degree, max_edges=MAX_DENSE_EDGES)
        check_input_size([graph], args.max_input_mib * MIB)
        if dense.get("input_sha256") != sha256(graph):
            fail("regenerated cancellation graph hash differs from the retained dense workload")
        if dense.get("facts") != graph_facts(graph):
            fail("regenerated cancellation graph facts differ from the retained dense workload")
        measured = {
            "workload": label,
            "cli": run_cli_input_cancellation(
                engine,
                graph,
                repeat=args.repeat,
                timeout=args.timeout_seconds,
                work=work,
            ),
            "mcp": run_mcp_active_cancellation(
                engine,
                graph,
                vertices=vertices,
                degree=degree,
                repeat=args.repeat,
                timeout=args.timeout_seconds,
                reference_seconds=reference_seconds,
            ),
            "provenance": {
                "mode": "cancellation-only replacement",
                "measured_utc": datetime.now(timezone.utc).isoformat(),
                "engine_sha256": sha256(engine),
                "tool_path": str(current_tool),
                "tool_sha256": sha256(current_tool),
                "retained_section_sha256": retained_hashes,
                "unconfirmed_report_snapshot": {
                    "path": str(report_snapshot),
                    "sha256": sha256(report_snapshot),
                },
                "unconfirmed_tool_snapshot": {
                    "path": str(tool_snapshot),
                    "sha256": sha256(tool_snapshot),
                },
                "unconfirmed_reason": (
                    "the prior MCP samples observed no CPU progress and used a reader-thread "
                    "ping, while the prior CLI ACK clock began after signal delivery"
                ),
                "clock_boundaries": {
                    "cli_signal_to_ack": "immediately before os.kill to ACK line observed",
                    "mcp_cancel_to_followup_worker": (
                        "immediately before cancellation notification write to worker-bound "
                        "impact response observed"
                    ),
                    "retained_mcp_queries": "post-request-flush to response observed",
                },
                "worker_dispatch_evidence": (
                    "engine/cli/src/mcp.rs replies to ping in the reader path and submits "
                    "tools/call jobs to worker_loop"
                ),
            },
        }
    report["cancellation"] = measured
    for key, expected_hash in retained_hashes.items():
        if json_value_sha256(report[key]) != expected_hash:
            fail(f"cancellation replacement changed retained report section {key}")
    temporary = report_path.with_name(f".{report_path.name}.replacement.tmp")
    temporary.write_text(json.dumps(report, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    os.replace(temporary, report_path)
    return {
        "status": "ok",
        "report": str(report_path),
        "engine_sha256": engine_info["sha256"],
        "mode": "cancellation-only replacement",
    }


def run(args: argparse.Namespace) -> dict[str, object]:
    """전체 측정을 외부 report 하나로 묶고 temporary work를 자동 정리한다."""
    if args.smoke:
        return run_smoke(args)
    if args.replace_cancellation_in is not None:
        return run_cancellation_replacement(args)
    engine = args.engine.resolve()
    if not engine.is_file() or not os.access(engine, os.X_OK):
        fail(f"engine is not an executable file: {engine}")
    if not hasattr(os, "wait4") or os.name != "posix":
        fail("scale measurement requires POSIX wait4 resource accounting")
    output = args.output.resolve()
    output.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="schemagraph-scale-") as directory:
        work = Path(directory)
        large_inputs = write_large_schema(work, args.large_objects, args.columns)
        dense_paths: dict[str, Path] = {}
        for workload in args.dense_workloads:
            label = str(workload["label"])
            dense_work = work / label
            dense_work.mkdir()
            dense_paths[label] = write_dense_graph(
                dense_work,
                int(workload["vertices"]),
                int(workload["degree"]),
                max_edges=MAX_DENSE_EDGES,
            )
        all_inputs = list(large_inputs.values()) + list(dense_paths.values())
        total_input = check_input_size(all_inputs, args.max_input_mib * MIB)
        large = run_large_schema(
            engine,
            work,
            inputs=large_inputs,
            objects=args.large_objects,
            columns=args.columns,
            repeat=args.repeat,
            timeout=args.timeout_seconds,
            max_rss_mib=args.max_rss_mib,
            max_input_bytes=args.max_input_mib * MIB,
        )
        dense: dict[str, object] = {}
        for workload in args.dense_workloads:
            label = str(workload["label"])
            vertices = int(workload["vertices"])
            degree = int(workload["degree"])
            edge_count = int(workload["edges"])
            dense_graph = dense_paths[label]
            facts = graph_facts(dense_graph)
            expected_facts = expected_dense_impact(vertices, degree)
            expected_graph_facts = {
                "vertices": vertices + 1,
                "edges": edge_count,
                "vertex_kind_counts": {"table": 1, "view": vertices},
                "edge_kind_counts": {"reads": edge_count},
            }
            for key, expected_value in expected_graph_facts.items():
                if facts[key] != expected_value:
                    fail(f"{label} generated graph fact {key}={facts[key]!r}, expected {expected_value!r}")
            cli = run_cli_queries(
                engine,
                dense_graph,
                vertices=vertices,
                degree=degree,
                repeat=args.repeat,
                timeout=args.timeout_seconds,
                max_rss_mib=args.max_rss_mib,
                work=work,
            )
            mcp = run_mcp_queries(
                engine,
                dense_graph,
                vertices=vertices,
                degree=degree,
                repeat=args.repeat,
                timeout=args.timeout_seconds,
                max_rss_mib=args.max_rss_mib,
            )
            if cli["facts"] != mcp["facts"] or cli["facts"] != expected_facts:
                fail(f"{label} CLI/MCP impact facts diverged")
            dense[label] = {
                "nodes": vertices,
                "graph_vertices": vertices + 1,
                "degree": degree,
                "directed_edges": edge_count,
                "directed_density": round(edge_count / ((vertices + 1) * vertices), 6),
                "node_dependency_density": round(degree / (vertices - 1), 6),
                "input_bytes": dense_graph.stat().st_size,
                "input_sha256": sha256(dense_graph),
                "facts": facts,
                "expected_impact_facts": expected_facts,
                "cli": cli,
                "mcp": mcp,
            }
        largest = max(args.dense_workloads, key=lambda item: int(item["vertices"]))
        largest_label = str(largest["label"])
        largest_dense = dense[largest_label]
        mcp_reference = largest_dense["mcp"]["request_summary"]["seconds"]["median"]
        cancellation = {
            "workload": largest_label,
            "cli": run_cli_input_cancellation(
                engine,
                dense_paths[largest_label],
                repeat=args.repeat,
                timeout=args.timeout_seconds,
                work=work,
            ),
            "mcp": run_mcp_active_cancellation(
                engine,
                dense_paths[largest_label],
                vertices=int(largest["vertices"]),
                degree=int(largest["degree"]),
                repeat=args.repeat,
                timeout=args.timeout_seconds,
                reference_seconds=float(mcp_reference),
            ),
        }
        report: dict[str, object] = {
            "schema": 1,
            "tool": "benchmark-scale",
            "benchmark_tool_sha256": sha256(Path(__file__).resolve()),
            "process_exit_poll_seconds": PROCESS_POLL_SECONDS,
            "created_utc": datetime.now(timezone.utc).isoformat(),
            "platform": {
                "system": platform.system(),
                "release": platform.release(),
                "machine": platform.machine(),
                "python": platform.python_version(),
            },
            "engine": {"path": str(engine), "sha256": sha256(engine)},
            "limits": {
                "repeat": args.repeat,
                "timeout_seconds": args.timeout_seconds,
                "max_input_mib": args.max_input_mib,
                "max_rss_mib": args.max_rss_mib,
                "generated_input_total_bytes": total_input,
            },
            "large_schema": large,
            "dense_graph": dense,
            "cancellation": cancellation,
        }
    report_path = output / "results.json"
    report_path.write_text(json.dumps(report, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    return {"status": "ok", "report": str(report_path), "engine_sha256": report["engine"]["sha256"]}


def main() -> int:
    """CLI 진입점에서 오류를 CI 로그가 읽을 수 있는 한 줄로 출력한다."""
    try:
        args = bounded_args()
        result = run(args)
    except (BenchmarkError, OSError, ValueError) as error:
        print(f"error: {error}", file=sys.stderr)
        return 1
    print(json.dumps(result, sort_keys=True))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
