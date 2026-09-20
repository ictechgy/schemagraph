#!/usr/bin/env python3
"""생산자의 버전·전송 형식에 따라 그래프 의미가 달라지지 않는지 확인한다."""

from __future__ import annotations

import copy
import json
import subprocess
import sys
from pathlib import Path
from urllib.parse import urlsplit


FORMATS = ("json", "ndjson")
VERSIONS = (1, 2)
MISSING = object()


def sensitive_values(command: list[str]) -> list[str]:
    """원문 명령을 출력하지 않고 오류에서 가릴 접속 인자만 모은다."""
    values: list[str] = []
    for index, argument in enumerate(command[:-1]):
        if argument in ("--url", "--password"):
            values.append(command[index + 1])
    return values


def scrub(text: str, values: list[str]) -> str:
    """접속 URL과 비밀번호가 검증 실패 로그에 남지 않게 한다."""
    result = text
    for value in filter(None, values):
        result = result.replace(value, "<redacted>")
        candidates = [value]
        if value.startswith("jdbc:"):
            candidates.append(value.removeprefix("jdbc:"))
        for candidate in candidates:
            try:
                parsed = urlsplit(candidate)
            except ValueError:
                continue
            if parsed.password:
                result = result.replace(parsed.password, "<redacted>")
                if parsed.username:
                    result = result.replace(
                        f"{parsed.username}:{parsed.password}", "<redacted-credentials>"
                    )
    return result.strip()


def run_process(command: list[str], stage: str, redactions: list[str]) -> None:
    """자식 프로세스가 실패해도 생산자의 인증 정보는 진단에서 제외한다."""
    try:
        completed = subprocess.run(command, capture_output=True, text=True, check=False)
    except OSError as error:
        raise RuntimeError(f"{stage} could not start: {scrub(str(error), redactions)}") from error
    if completed.returncode == 0:
        return
    detail = scrub(completed.stderr, redactions) or scrub(completed.stdout, redactions)
    suffix = f": {detail}" if detail else ""
    raise RuntimeError(f"{stage} failed with exit code {completed.returncode}{suffix}")


def load_graph(path: Path, label: str) -> dict:
    try:
        with path.open(encoding="utf-8") as stream:
            value = json.load(stream)
    except (OSError, json.JSONDecodeError) as error:
        raise RuntimeError(f"{label} is not valid JSON: {error}") from error
    if not isinstance(value, dict):
        raise RuntimeError(f"{label} must be a JSON object")
    if not isinstance(value.get("vertices"), list) or not isinstance(value.get("edges"), list):
        raise RuntimeError(f"{label} must contain vertices and edges arrays")
    return value


def normalize_usage(graph: dict, label: str) -> dict:
    """관측값의 변동만 지우고 미수집과 관측된 0의 구분은 보존한다."""
    value = copy.deepcopy(graph)
    for vertex_number, vertex in enumerate(value["vertices"]):
        if not isinstance(vertex, dict):
            raise RuntimeError(f"{label} vertex {vertex_number} is not an object")
        usage = vertex.get("usage", MISSING)
        if usage is MISSING:
            continue
        if not isinstance(usage, dict):
            raise RuntimeError(f"{label} vertex {vertex_number} has non-object usage")
        for counter in ("reads", "writes"):
            if counter in usage:
                number = usage[counter]
                if isinstance(number, bool) or not isinstance(number, int) or number < 0:
                    raise RuntimeError(
                        f"{label} vertex {vertex_number} usage.{counter} must be a nonnegative integer"
                    )
        vertex["usage"] = {key: "<value>" for key in sorted(usage)}
    return value


def first_difference(left: object, right: object, path: str = "graph") -> str:
    if type(left) is not type(right):
        return f"{path}: types differ ({type(left).__name__} vs {type(right).__name__})"
    if isinstance(left, dict):
        for key in sorted(set(left) | set(right)):
            if key not in left:
                return f"{path}.{key}: missing from left"
            if key not in right:
                return f"{path}.{key}: missing from right"
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
    return "" if left == right else f"{path}: {left!r} != {right!r}"


def compare_native(native: dict, producer_graph: dict, label: str) -> None:
    """reader별 한계 문구 차이는 허용하되 그래프 구조와 근거는 비교한다."""
    for field in ("vertices", "edges"):
        if native[field] != producer_graph[field]:
            difference = first_difference(native[field], producer_graph[field], field)
            raise RuntimeError(f"{label} differs from native reference: {difference}")


def output_paths(prefix: Path, version: int, output_format: str) -> tuple[Path, Path]:
    stem = f"{prefix}.v{version}.{output_format}"
    return Path(f"{stem}.document"), Path(f"{stem}.graph.json")



def verify(
    engine: Path,
    reference_path: Path,
    output_prefix: Path,
    producer_command: list[str],
) -> None:
    if not producer_command:
        raise RuntimeError("producer command after -- is required")
    if not engine.is_file() or not engine.stat().st_mode & 0o111:
        raise RuntimeError(f"engine binary is not executable: {engine}")
    reference = normalize_usage(load_graph(reference_path, "reference graph"), "reference graph")
    output_prefix.parent.mkdir(parents=True, exist_ok=True)
    redactions = sensitive_values(producer_command)
    baseline: dict | None = None

    for version in VERSIONS:
        for output_format in FORMATS:
            document_path, graph_path = output_paths(output_prefix, version, output_format)
            command = producer_command + [
                "--document-version",
                str(version),
                "--format",
                output_format,
                "-o",
                str(document_path),
            ]
            run_process(command, f"producer v{version}/{output_format}", redactions)
            run_process(
                [
                    str(engine),
                    "scan",
                    "--document",
                    str(document_path),
                    "-o",
                    str(graph_path),
                ],
                f"engine scan v{version}/{output_format}",
                redactions,
            )
            graph = normalize_usage(
                load_graph(graph_path, f"producer graph v{version}/{output_format}"),
                f"producer graph v{version}/{output_format}",
            )
            compare_native(reference, graph, f"producer v{version}/{output_format}")
            if baseline is None:
                baseline = graph
            elif baseline != graph:
                difference = first_difference(baseline, graph)
                raise RuntimeError(
                    f"producer graph v{version}/{output_format} differs from v1/json baseline: {difference}"
                )


def main(arguments: list[str]) -> int:
    try:
        separator = arguments.index("--")
    except ValueError:
        print(
            "usage: verify-probe-versions.py ENGINE REFERENCE_GRAPH OUTPUT_PREFIX -- PRODUCER_COMMAND [ARGS...]",
            file=sys.stderr,
        )
        return 2
    if separator != 3 or len(arguments) == separator + 1:
        print(
            "usage: verify-probe-versions.py ENGINE REFERENCE_GRAPH OUTPUT_PREFIX -- PRODUCER_COMMAND [ARGS...]",
            file=sys.stderr,
        )
        return 2
    try:
        verify(
            Path(arguments[0]),
            Path(arguments[1]),
            Path(arguments[2]),
            arguments[separator + 1 :],
        )
    except RuntimeError as error:
        print(f"error: {error}", file=sys.stderr)
        return 1
    print("producer v1/v2 JSON/NDJSON parity: ok")
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
