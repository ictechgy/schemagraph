#!/usr/bin/env python3
"""폐기용 JDBC 대상에 DB2 LUW·Informix fixture를 적용해 실제 경로를 검증한다.

runner는 데이터베이스를 만들거나 지우지 않는다. 실행 전에 SG_DB2_URL 또는
SG_INFORMIX_URL과 대응하는 외부 JDBC jar를 지정해야 한다. fixture 객체를
지우지 않으므로 대상은 폐기용 데이터베이스여야 한다.
"""

from __future__ import annotations

import argparse
import json
import os
from pathlib import Path
import subprocess
import sys
from urllib.parse import urlsplit


ROOT = Path(__file__).resolve().parents[1]
VERSIONS = ROOT / "Scripts" / "verify-probe-versions.py"
APPLY = ROOT / "Scripts" / "ApplySqlStatements.java"


def redactions(command: list[str]) -> list[str]:
    values: list[str] = []
    for index, argument in enumerate(command[:-1]):
        if argument in {"--url", "--password"}:
            values.append(command[index + 1])
    return values


def scrub(text: str, values: list[str]) -> str:
    result = text
    for value in filter(None, values):
        result = result.replace(value, "<redacted>")
        try:
            parsed = urlsplit(value)
        except ValueError:
            parsed = None
        if parsed and parsed.password:
            result = result.replace(parsed.password, "<redacted-password>")
    return result.strip()


def run(command: list[str], stage: str, expected: int = 0, secrets: list[str] | None = None) -> str:
    try:
        result = subprocess.run(command, cwd=ROOT, capture_output=True, text=True, check=False)
    except OSError as error:
        raise RuntimeError(f"{stage} could not start: {error}") from error
    if result.returncode != expected:
        detail = scrub(result.stderr or result.stdout, redactions(command) + (secrets or []))
        suffix = f": {detail}" if detail else ""
        raise RuntimeError(f"{stage} failed with exit code {result.returncode}{suffix}")
    return result.stdout


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("engine", type=Path, help="schemagraph executable")
    parser.add_argument("probe_jar", type=Path, help="JVM probe fat jar")
    parser.add_argument("--output-dir", type=Path, help="keep fixture documents and graphs in this directory")
    parser.add_argument("--java", default=os.environ.get("JAVA", "java"))
    parser.add_argument("--db2-url", default=os.environ.get("SG_DB2_URL"))
    parser.add_argument("--db2-user", default=os.environ.get("SG_DB2_USER", "db2inst1"))
    parser.add_argument("--db2-password", default=os.environ.get("SG_DB2_PASSWORD", ""))
    parser.add_argument("--db2-jdbc", type=Path,
                        default=Path(os.environ["SG_DB2_JDBC"]) if os.environ.get("SG_DB2_JDBC") else None)
    parser.add_argument("--db2-schema", default=os.environ.get("SG_DB2_SCHEMA", "SGFIX"))
    parser.add_argument("--informix-url", default=os.environ.get("SG_INFORMIX_URL"))
    parser.add_argument("--informix-user", default=os.environ.get("SG_INFORMIX_USER", "informix"))
    parser.add_argument("--informix-password", default=os.environ.get("SG_INFORMIX_PASSWORD", ""))
    parser.add_argument("--informix-jdbc", type=Path,
                        default=Path(os.environ["SG_INFORMIX_JDBC"]) if os.environ.get("SG_INFORMIX_JDBC") else None)
    parser.add_argument("--informix-schema", default=os.environ.get("SG_INFORMIX_SCHEMA", ""))
    parser.add_argument("--require-all", action="store_true",
                        help="fail when one of the two configured targets is unavailable")
    parser.add_argument("--database", choices=("db2", "informix", "all"), default="all",
                        help="fixture target to verify")
    return parser.parse_args()


def jdbc_apply(java: str, jdbc: Path, url: str, user: str, password: str, fixture: Path) -> None:
    run(
        [java, "-cp", os.pathsep.join((str(jdbc), str(ROOT))), str(APPLY), url, user, password, str(fixture)],
        f"apply {fixture.name}",
        secrets=[url, user, password],
    )


def exercise_case(java: str, jdbc: Path, url: str, user: str, password: str,
                  label: str, schema: str, temporary: Path) -> None:
    if label == "db2":
        body = f"""--#SET TERMINATOR @
SET CURRENT SCHEMA {schema}@
VALUES {schema}.add_one(1)@
VALUES {schema}.overloaded(1)@
VALUES {schema}.overloaded('x')@
CALL {schema}.touch_customer(1)@
INSERT INTO {schema}.orders(id, customer_id, total) VALUES (9001, 1, 1.00)@
"""
    else:
        body = """--#SET TERMINATOR @
EXECUTE FUNCTION add_one(1)@
EXECUTE FUNCTION overloaded(1)@
EXECUTE FUNCTION overloaded('x')@
INSERT INTO orders(id, customer_id, total) VALUES (9001, 1, 1.00)@
"""
    exercise = temporary / f"{label}.exercise.sql"
    exercise.write_text(body, encoding="utf-8")
    jdbc_apply(java, jdbc, url, user, password, exercise)


def probe_command(java: str, jar: Path, jdbc: Path, url: str, user: str, password: str,
                  output: Path, schema: str | None) -> list[str]:
    command = [java, "-jar", str(jar), "--url", url, "--user", user, "--driver", str(jdbc), "-o", str(output)]
    if password:
        command += ["--password", password]
    if schema:
        command += ["--schema", schema]
    return command


def assert_graph(graph_path: Path, document_path: Path, label: str, schema_hint: str | None) -> None:
    graph = json.loads(graph_path.read_text(encoding="utf-8"))
    document = json.loads(document_path.read_text(encoding="utf-8"))
    schema = schema_hint or next(
        (schema_doc["name"] for schema_doc in document.get("schemas", [])
         if any(obj.get("name", "").lower() == "customers"
                for obj in schema_doc.get("objects", []))),
        None,
    )
    if not schema:
        raise RuntimeError(f"{label}: no user schema was emitted")
    vertices = {vertex["id"] for vertex in graph.get("vertices", [])}
    edges = {(edge["kind"], edge["from"], edge["to"]) for edge in graph.get("edges", [])}
    def vertex_named(name: str) -> str | None:
        wanted = f"{schema}.{name}".casefold()
        return next((vertex for vertex in vertices if vertex.casefold() == wanted), None)

    object_ids = {name: vertex_named(name) for name in ("customers", "orders", "order_totals", "order_sequence")}
    missing = {name for name, vertex in object_ids.items() if vertex is None}
    if missing:
        raise RuntimeError(f"{label}: missing fixture vertices {sorted(missing)}")
    reads = {(source, target) for kind, source, target in edges if kind == "reads"}
    required_reads = {(object_ids["order_totals"], object_ids["customers"]),
                      (object_ids["order_totals"], object_ids["orders"])}
    if missing := required_reads - reads:
        raise RuntimeError(f"{label}: missing body reads edges {sorted(missing)}")
    if not any(kind == "writes" and source.casefold().startswith(f"{schema}.orders.".casefold())
               and target == object_ids["customers"]
               for kind, source, target in edges):
        raise RuntimeError(f"{label}: trigger write edge was not recovered")
    overloaded = [vertex for vertex in vertices if vertex.casefold().startswith(f"{schema}.overloaded(".casefold())]
    if len(overloaded) < 2:
        raise RuntimeError(f"{label}: overloaded routines were not distinguished: {sorted(overloaded)}")
    external = [vertex for vertex in vertices if vertex.casefold().startswith(f"{schema}.external_marker(".casefold())]
    if not external:
        raise RuntimeError(f"{label}: external routine vertex is missing")
    if any(source in external for _, source, _ in edges):
        raise RuntimeError(f"{label}: external routine unexpectedly produced body edges")
    if any(vertex.lower().startswith(("syscat.", "sysibm.", "informix.systables")) for vertex in vertices):
        raise RuntimeError(f"{label}: system catalog vertex leaked into graph")
    if label == "informix":
        long_body = next((routine.get("body") for schema_doc in document.get("schemas", [])
                          for routine in schema_doc.get("routines", [])
                          if routine.get("name", "").lower() == "long_literal"), None)
        if not long_body or "LONG_FRAGMENT_MARKER" not in long_body or len(long_body) < 300:
            raise RuntimeError(f"{label}: long routine body fragments were not reconstructed")
    if not any((("body" in note.lower() or "bodies" in note.lower()) and "unavailable" in note.lower()) or ("language" in note.lower() and "unsupported" in note.lower()) for note in graph.get("limitations", [])):
        raise RuntimeError(f"{label}: missing external-body limitation")


def verify_case(args: argparse.Namespace, label: str, url: str | None, user: str, password: str,
                jdbc: Path | None, schema: str, fixture: Path, temporary: Path) -> bool:
    if not url or not jdbc:
        if args.require_all and (url or jdbc):
            raise RuntimeError(f"{label}: URL and JDBC jar must be provided together")
        print(f"warning: {label} skipped; provide the URL and external JDBC jar", file=sys.stderr)
        return False
    if not jdbc.is_file():
        raise RuntimeError(f"{label}: JDBC jar does not exist: {jdbc}")
    if not args.probe_jar.is_file() or not args.engine.is_file():
        raise RuntimeError(f"{label}: probe jar and engine must exist")
    jdbc_apply(args.java, jdbc, url, user, password, fixture)
    exercise_case(args.java, jdbc, url, user, password, label, schema, temporary)
    document = temporary / f"{label}.document.json"
    command = probe_command(args.java, args.probe_jar, jdbc, url, user, password, document, schema or None)
    run(command, f"{label} initial probe")
    graph = temporary / f"{label}.graph.json"
    run([str(args.engine), "scan", "--document", str(document), "-o", str(graph)], f"{label} initial scan")
    assert_graph(graph, document, label, schema or None)
    run([
        sys.executable, str(VERSIONS), str(args.engine), str(graph), str(temporary / label), "--", *command
    ], f"{label} v1/v2 JSON/NDJSON parity")
    return True


def main() -> int:
    args = parse_args()
    selected = {"db2", "informix"} if args.database == "all" else {args.database}
    configured = {
        "db2": bool(args.db2_url and args.db2_jdbc),
        "informix": bool(args.informix_url and args.informix_jdbc),
    }
    if args.require_all and not all(configured[name] for name in selected):
        print(f"error: --require-all needs URL and JDBC jar for {', '.join(sorted(selected))}", file=sys.stderr)
        return 2
    temporary = args.output_dir or Path(os.environ.get("TMPDIR", "/tmp")) / f"schemagraph-ibm-fixtures-{os.getpid()}"
    temporary.mkdir(parents=True, exist_ok=bool(args.output_dir))
    try:
        ran = 0
        if "db2" in selected:
            ran += verify_case(args, "db2", args.db2_url, args.db2_user, args.db2_password,
                               args.db2_jdbc, args.db2_schema, ROOT / "Fixtures/db2/basic.sql", temporary)
        if "informix" in selected:
            ran += verify_case(args, "informix", args.informix_url, args.informix_user, args.informix_password,
                               args.informix_jdbc, args.informix_schema, ROOT / "Fixtures/informix/basic.sql", temporary)
        if not ran:
            print("warning: no IBM fixture target was configured", file=sys.stderr)
            return 0
        print(f"IBM fixture verification: {ran} target(s) passed")
        return 0
    except RuntimeError as error:
        print(f"error: {scrub(str(error), [args.db2_password, args.informix_password])}", file=sys.stderr)
        return 1
    finally:
        if not args.output_dir:
            for child in temporary.glob("*"):
                child.unlink(missing_ok=True)
            temporary.rmdir()


if __name__ == "__main__":
    raise SystemExit(main())
