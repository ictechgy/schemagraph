#!/usr/bin/env python3
"""동일 카탈로그의 CLI 시간·최대 RSS와 출력 해시를 재현 가능하게 측정한다."""

import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import sqlite3
import subprocess
import sys
import time


def object_record(number, width):
    """실제 컬럼·PK·FK가 있는 결정적 입력으로 그래프 구성 비용을 포함한다."""
    name = f"table_{number:06d}"
    columns = [dict(name=f"column_{column:03d}", data_type="INTEGER", nullable=column != 0,
                    ordinal=column + 1, pk_position=int(column == 0)) for column in range(width)]
    constraints = [dict(name=f"{name}_pk", kind="pk", columns=["column_000"])]
    if number:
        constraints.append(dict(name=f"{name}_fk", kind="fk", columns=["column_001"],
                                referenced=dict(table=f"table_{number - 1:06d}", columns=["column_000"])))
    return dict(name=name, kind="table", columns=columns, constraints=constraints,
                indexes=[dict(name=f"{name}_idx", unique=False, columns=["column_001"])], triggers=[])


def write_catalogs(directory, count, width, schema_count):
    """입력 생성기는 레코드를 바로 써서 측정 대상과 별도로 큰 버퍼를 만들지 않는다."""
    plain, lines = directory / "catalog.json", directory / "catalog.ndjson"
    header = dict(version=2, dialect="sqlite", producer=dict(name="benchmark"), required_features=[])
    with plain.open("w") as one, lines.open("w") as many:
        one.write(json.dumps(header)[:-1] + ', "schemas": [')
        many.write(json.dumps(dict(type="document", **header, limitations=[])) + "\n")
        start = 0
        for schema_index in range(min(schema_count, count)):
            size = count // min(schema_count, count) + int(schema_index < count % min(schema_count, count))
            schema = f"schema_{schema_index:04d}"
            if schema_index:
                one.write(",")
            one.write(json.dumps(dict(name=schema))[:-1] + ', "objects": [')
            many.write(json.dumps(dict(type="schema", name=schema)) + "\n")
            for number in range(size):
                record = object_record(number, width)
                if number:
                    one.write(",")
                one.write(json.dumps(record))
                many.write(json.dumps(dict(type="object", schema=schema, data=record)) + "\n")
            one.write('], "routines": []}')
            start += size
        assert start == count
        one.write('], "limitations": []}\n')
        many.write(json.dumps(dict(type="limitations", data=[])) + "\n")
    return plain, lines


def checksum(path):
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def measure(command, error_path):
    """wait4의 프로세스별 최고 RSS를 써서 이전 실행의 누적 최고치를 피한다."""
    start = time.perf_counter()
    with error_path.open("wb") as errors:
        process = subprocess.Popen(command, stdout=subprocess.DEVNULL, stderr=errors)
        _, status, usage = os.wait4(process.pid, 0)
        process.returncode = os.waitstatus_to_exitcode(status)
    if process.returncode:
        raise RuntimeError(f"Benchmark process failed ({process.returncode}); inspect {error_path}")
    multiplier = 1 if sys.platform == "darwin" else 1024
    return dict(seconds=round(time.perf_counter() - start, 6), peak_rss_bytes=usage.ru_maxrss * multiplier)


def benchmark(args):
    output = args.output.resolve()
    output.mkdir(parents=True, exist_ok=True)
    results = []
    for count in args.objects:
        directory = output / str(count)
        directory.mkdir(exist_ok=True)
        paths = write_catalogs(directory, count, args.columns, args.schemas)
        expected = None
        for path in paths:
            for run in range(args.repeat):
                graph = directory / f"{path.suffix[1:]}-{run}.graph.json"
                result = measure([str(args.engine.resolve()), "scan", "--document", str(path), "-o", str(graph)],
                                 graph.with_suffix(".stderr"))
                digest = checksum(graph)
                if expected is None:
                    expected = digest
                if digest != expected:
                    raise RuntimeError("JSON/NDJSON or repeated runs changed graph bytes")
                result.update(component="engine", objects=count, format=path.suffix[1:], run=run,
                              input_bytes=path.stat().st_size, output_bytes=graph.stat().st_size, sha256=digest)
                results.append(result)
                print(json.dumps(result), flush=True)
        if args.probe:
            database = directory / "large.sqlite"
            with sqlite3.connect(database) as connection:
                for number in range(count):
                    connection.execute(f'CREATE TABLE IF NOT EXISTS "table_{number:06d}" (id INTEGER PRIMARY KEY, value TEXT)')
            for format_name in ["json", "ndjson"]:
                catalog = directory / f"probe-{format_name}.document"
                for run in range(args.repeat):
                    result = measure([str(args.probe.resolve()), "--url", "sqlite:" + str(database),
                                      "--document-version", "2", "--format", format_name, "-o", str(catalog)],
                                     directory / f"probe-{format_name}.stderr")
                    result.update(component="probe-go", objects=count, format=format_name, run=run,
                                  output_bytes=catalog.stat().st_size, sha256=checksum(catalog))
                    results.append(result)
                    print(json.dumps(result), flush=True)
    report = dict(label=args.label, platform=platform.platform(), machine=platform.machine(),
                  engine=str(args.engine.resolve()), objects=args.objects, columns=args.columns,
                  schemas=args.schemas, repeat=args.repeat, results=results)
    (output / "results.json").write_text(json.dumps(report, indent=2) + "\n")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--engine", type=Path, required=True)
    parser.add_argument("--probe", type=Path)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--label", default="working-tree")
    parser.add_argument("--objects", type=int, nargs="+", default=[1000, 10000])
    parser.add_argument("--columns", type=int, default=8)
    parser.add_argument("--schemas", type=int, default=20)
    parser.add_argument("--repeat", type=int, default=3)
    args = parser.parse_args()
    if min(args.objects) < 1 or args.columns < 2 or args.schemas < 1 or args.repeat < 1:
        parser.error("objects/schemas/repeat must be positive and columns must be at least 2")
    if not hasattr(os, "wait4"):
        parser.error("This benchmark requires a POSIX platform with wait4 resource accounting")
    benchmark(args)
