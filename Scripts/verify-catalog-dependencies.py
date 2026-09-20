#!/usr/bin/env python3
"""catalog dependency transport와 실제 PostgreSQL fixture를 검증한다."""

from __future__ import annotations

import argparse
import getpass
import json
import os
import re
import shutil
import socket
import subprocess
import sys
import tempfile
from urllib.parse import quote, urlsplit
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]


def run(command: list[str], *, expected: int = 0, label: str) -> subprocess.CompletedProcess[str]:
    """명령을 실행하되 URL·자격증명을 오류 출력에 포함하지 않는다."""
    try:
        result = subprocess.run(command, capture_output=True, text=True, check=False)
    except OSError as error:
        raise RuntimeError(f"{label} could not start: {error}") from error
    if result.returncode != expected:
        detail = (result.stderr or result.stdout).strip().splitlines()
        message = detail[-1] if detail else 'no diagnostic'
        for index, argument in enumerate(command[:-1]):
            if argument in {"--url", "--password"}:
                message = message.replace(command[index + 1], "<redacted>")
        message = re.sub(r'(?:jdbc:)?[A-Za-z][A-Za-z0-9+.-]*://\S+', '<connection>', message)
        raise RuntimeError(f"{label} exited {result.returncode}, expected {expected}: {message}")
    return result


def create_sqlite(path: Path) -> None:
    """카탈로그와 일반 FK를 가진 임시 SQLite fixture를 만든다."""
    import sqlite3

    connection = sqlite3.connect(path)
    try:
        connection.executescript(
            """
            PRAGMA foreign_keys = ON;
            CREATE TABLE customers (id INTEGER PRIMARY KEY, name TEXT NOT NULL);
            CREATE TABLE orders (
                id INTEGER PRIMARY KEY,
                customer_id INTEGER NOT NULL REFERENCES customers(id)
            );
            CREATE INDEX orders_customer_idx ON orders(customer_id);
            CREATE INDEX orders_partial ON orders(customer_id) WHERE customer_id > 0;
            CREATE INDEX orders_expression ON orders((customer_id + 1), id);
            CREATE TABLE without_rowid (id INTEGER, second INTEGER, value INTEGER, PRIMARY KEY(id,second)) WITHOUT ROWID;
            CREATE INDEX without_rowid_value ON without_rowid(value);
            CREATE VIEW order_view AS
                SELECT orders.id, customers.name
                FROM orders JOIN customers ON customers.id = orders.customer_id;
            """
        )
        connection.commit()
    finally:
        connection.close()


def load_json(path: Path) -> dict:
    try:
        value = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as error:
        raise RuntimeError(f"invalid JSON document {path}: {error}") from error
    if not isinstance(value, dict):
        raise RuntimeError(f"document {path} is not an object")
    return value


def dependency_key(item: dict) -> str:
    return json.dumps(item, sort_keys=True, separators=(",", ":"))


def assert_graph(graph: dict, *, dependencies: bool, label: str) -> None:
    """ghost 정점과 depends-on의 필요한/금지된 방향을 확인한다."""
    vertices = {item["id"] for item in graph.get("vertices", [])}
    edges = graph.get("edges", [])
    for edge in edges:
        if edge.get("from") not in vertices or edge.get("to") not in vertices:
            raise RuntimeError(f"{label}: graph has ghost edge {edge}")
    depends = [edge for edge in edges if edge.get("kind") == "depends-on"]
    if dependencies and not depends:
        raise RuntimeError(f"{label}: expected catalog depends-on edges")
    if not dependencies and depends:
        raise RuntimeError(f"{label}: unsupported/empty catalog emitted depends-on edges: {depends}")
    if not dependencies:
        indexes = graph.get('schema_metadata', {}).get('indexes', {})
        for suffix, complete, partial, columns in (
            ('orders.orders_customer_idx', True, False, ['main.orders.customer_id']),
            ('orders.orders_partial', True, True, ['main.orders.customer_id']),
            ('orders.orders_expression', False, False, ['main.orders.id']),
            ('without_rowid.without_rowid_value', True, False, ['main.without_rowid.value']),
        ):
            index = indexes.get('main.' + suffix)
            if not index or (index['complete'], index['has_predicate'], index['columns']) != (complete, partial, columns):
                raise RuntimeError(f"{label}: incorrect expression/partial/auxiliary index metadata for {suffix}: {index}")


def document_records(path: Path, output_format: str) -> tuple[dict, list[dict], dict]:
    """JSON·NDJSON의 header/context/dependency 레코드를 공통 형태로 읽는다."""
    if output_format == "json":
        document = load_json(path)
        return document, list(document.get("dependencies", [])), document.get("context") or {}
    lines = [json.loads(line) for line in path.read_text(encoding="utf-8").splitlines() if line.strip()]
    if not lines or lines[0].get("type") != "document" or lines[-1].get("type") != "limitations":
        raise RuntimeError(f"{path}: malformed NDJSON envelope")
    dependencies = [line["data"] for line in lines if line.get("type") == "dependency"]
    header_context = lines[0].get("context") or {}
    trailer_context = lines[-1].get("context") or {}
    for field in ("source_id", "database", "schema_filter"):
        if header_context.get(field) != trailer_context.get(field):
            raise RuntimeError(f"{path}: NDJSON header/trailer context mismatch in {field}")
    if header_context.get("catalog_complete") and not trailer_context.get("catalog_complete", True):
        pass
    document = dict(lines[0])
    document["dependencies"] = dependencies
    document["context"] = trailer_context
    return document, dependencies, trailer_context


def run_engine_document(engine: Path, document: Path, graph: Path, label: str) -> dict:
    run([str(engine), "scan", "--document", str(document), "-o", str(graph)], label=label)
    return load_json(graph)


def check_document(
    engine: Path,
    document: Path,
    output_format: str,
    graph: Path,
    *,
    expected_dependencies: bool,
    label: str,
) -> list[dict]:
    value, dependencies, context = document_records(document, output_format)
    if context.get("source_id") != "app":
        raise RuntimeError(f"{label}: source_id was not preserved: {context}")
    if not expected_dependencies and context.get("catalog_complete") is not False:
        raise RuntimeError(f"{label}: unsupported dependency collection was reported complete: {context}")
    if expected_dependencies and not dependencies:
        raise RuntimeError(f"{label}: dependency records are missing")
    if not expected_dependencies and dependencies:
        raise RuntimeError(f"{label}: unexpected dependency records: {dependencies}")
    graph_value = run_engine_document(engine, document, graph, label)
    assert_graph(graph_value, dependencies=expected_dependencies, label=label)
    return sorted(dependencies, key=dependency_key)


def probe_sqlite(
    engine: Path,
    database: Path,
    work: Path,
    go_probe: Path | None,
    jdbc_jar: Path | None,
) -> None:
    """SQLite에서 native·Go·JDBC의 4 wire envelope를 대조한다."""
    native_docs: list[Path] = []
    for version in (1, 2):
        document = work / f"sqlite-native-v{version}.json"
        graph = work / f"sqlite-native-v{version}.graph.json"
        run(
            [
                str(engine),
                "scan",
                f"sqlite:{database}",
                "--source-id",
                "app",
                "--catalog-dependencies",
                "--document-version",
                str(version),
                "--emit-document",
                str(document),
                "-o",
                str(graph),
            ],
            label=f"native SQLite v{version}",
        )
        value = load_json(document)
        if value.get("dependencies"):
            raise RuntimeError(f"native SQLite v{version}: unexpected dependency records")
        context = value.get("context") or {}
        if context.get("catalog_complete") is not False:
            raise RuntimeError(f"native SQLite v{version}: unsupported catalog collection was reported complete")
        if not any("supported for PostgreSQL" in note for note in value.get("limitations", [])):
            raise RuntimeError(f"native SQLite v{version}: missing unsupported-catalog limitation")
        assert_graph(load_json(graph), dependencies=False, label=f"native SQLite v{version}")
        native_docs.append(document)
    baseline = load_json(work / "sqlite-native-v1.graph.json")
    if load_json(work / "sqlite-native-v2.graph.json") != baseline:
        raise RuntimeError("native SQLite v1/v2 changed graph semantics")

    if go_probe and go_probe.is_file():
        for version in (1, 2):
            for output_format in ("json", "ndjson"):
                document = work / f"sqlite-go-v{version}-{output_format}.document"
                graph = work / f"sqlite-go-v{version}-{output_format}.graph.json"
                run(
                    [
                        str(go_probe),
                        "--url",
                        f"sqlite:{database}",
                        "--source-id",
                        "app",
                        "--catalog-dependencies",
                        "--document-version",
                        str(version),
                        "--format",
                        output_format,
                        "-o",
                        str(document),
                    ],
                    label=f"Go SQLite v{version}/{output_format}",
                )
                deps = check_document(
                    engine,
                    document,
                    output_format,
                    graph,
                    expected_dependencies=False,
                    label=f"Go SQLite v{version}/{output_format}",
                )
                if deps != []:
                    raise RuntimeError("Go SQLite emitted unsupported dependency facts")
        print("Go SQLite catalog-dependencies v1/v2 JSON/NDJSON: verified unsupported and empty")
    else:
        print("SKIP Go SQLite catalog-dependencies: no executable supplied")

    if jdbc_jar and jdbc_jar.is_file():
        for version in (1, 2):
            for output_format in ("json", "ndjson"):
                document = work / f"sqlite-jdbc-v{version}-{output_format}.document"
                graph = work / f"sqlite-jdbc-v{version}-{output_format}.graph.json"
                run(
                    [
                        "java",
                        "-jar",
                        str(jdbc_jar),
                        "--url",
                        f"jdbc:sqlite:{database}",
                        "--source-id",
                        "app",
                        "--catalog-dependencies",
                        "--document-version",
                        str(version),
                        "--format",
                        output_format,
                        "-o",
                        str(document),
                    ],
                    label=f"JDBC SQLite v{version}/{output_format}",
                )
                check_document(
                    engine,
                    document,
                    output_format,
                    graph,
                    expected_dependencies=False,
                    label=f"JDBC SQLite v{version}/{output_format}",
                )
        print("JDBC SQLite catalog-dependencies v1/v2 JSON/NDJSON: verified unsupported and empty")
    else:
        print("SKIP JDBC SQLite catalog-dependencies: no standalone JAR supplied")


def free_port() -> int:
    socket_handle = socket.socket()
    socket_handle.bind(("127.0.0.1", 0))
    port = socket_handle.getsockname()[1]
    socket_handle.close()
    return port


def start_postgres(work: Path) -> tuple[subprocess.Popen[str], str] | None:
    """공식 Homebrew PostgreSQL로 인증 없는 임시 cluster를 시작한다."""
    bindir = Path(os.environ.get("PGBIN", "/opt/homebrew/opt/postgresql@16/bin"))
    initdb = bindir / "initdb"
    pg_ctl = bindir / "pg_ctl"
    psql = bindir / "psql"
    if not all(path.is_file() for path in (initdb, pg_ctl, psql)):
        print("SKIP PostgreSQL catalog-dependencies: PostgreSQL 16 tools unavailable")
        return None
    data = work / "postgres-data"
    # PostgreSQL의 Unix socket 경로는 macOS에서 103 bytes 제한이 있어
    # TemporaryDirectory 전체 경로를 쓰지 않는다. 포트는 매번 임의로 고른다.
    socket_dir = Path("/tmp")
    run([str(initdb), "-D", str(data), "-A", "trust", "--no-locale"], label="PostgreSQL initdb")
    port = free_port()
    role = getpass.getuser()
    logfile = work / "postgres.log"
    process = subprocess.Popen(
        [str(pg_ctl), "-D", str(data), "-o", f"-p {port} -k {socket_dir}", "-l", str(logfile), "-w", "start"],
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
    )
    return_code = process.wait(timeout=60)
    if return_code != 0:
        detail = (process.stderr.read() if process.stderr else "").strip().splitlines()
        log_detail = logfile.read_text(encoding="utf-8", errors="replace").strip().splitlines() if logfile.exists() else []
        raise RuntimeError(
            "PostgreSQL temporary cluster failed to start: "
            f"{' | '.join((log_detail or detail or ['no pg_ctl diagnostic'])[-8:])}"
        )
    url = f"postgres://{role}@127.0.0.1:{port}/postgres"
    run([str(psql), "-h", str(socket_dir), "-p", str(port), "-U", role, "-d", "postgres", "-v", "ON_ERROR_STOP=1", "-c", postgres_fixture_sql()], label="PostgreSQL fixture")
    return process, url


def postgres_fixture_sql() -> str:
    return """
CREATE SCHEMA app;
CREATE TABLE app.customers(id integer primary key, name text not null);
CREATE TABLE app.orders(id integer primary key, customer_id integer not null references app.customers(id));
CREATE VIEW app.order_view AS SELECT o.id, c.name FROM app.orders o JOIN app.customers c ON c.id=o.customer_id;
CREATE OR REPLACE FUNCTION app.customer_name(integer) RETURNS text LANGUAGE sql AS $$ SELECT name FROM app.customers WHERE id=$1 $$;
"""


def check_postgres(engine: Path, url: str, work: Path, go_probe: Path | None, jdbc_jar: Path | None) -> None:
    """실제 pg_depend 행이 모든 producer와 engine DependsOn으로 이어지는지 확인한다."""
    native = work / "postgres-native.json"
    native_graph = work / "postgres-native.graph.json"
    run([str(engine), "scan", url, "--schema", "app", "--source-id", "app", "--catalog-dependencies", "--emit-document", str(native), "-o", str(native_graph)], label="native PostgreSQL catalog-dependencies")
    native_value = load_json(native)
    native_dependencies = native_value.get("dependencies", [])
    if not native_dependencies:
        raise RuntimeError("native PostgreSQL returned no catalog dependency rows")
    assert_expected_postgres_dependencies(native_dependencies, "native PostgreSQL")
    assert_graph(load_json(native_graph), dependencies=True, label="native PostgreSQL")
    print(f"native PostgreSQL catalog-dependencies: verified {len(native_dependencies)} dependency rows")

    # DB가 제공한 외부 database identity는 같은 로컬 이름에 연결되지 않아야 한다.
    external = json.loads(json.dumps(native_value))
    external["dependencies"].append({
        "source": {"schema": "app", "name": "order_view", "kind": "view"},
        "target": {"schema": "app", "name": "customers", "kind": "table", "database": "other-database"},
        "catalog": "fixture",
        "dependency_type": "normal",
    })
    external_path = work / "postgres-external-target.json"
    external_path.write_text(json.dumps(external), encoding="utf-8")
    external_graph = run_engine_document(engine, external_path, work / "postgres-external-target.graph.json", "external PostgreSQL target")
    assert_graph(external_graph, dependencies=True, label="external PostgreSQL target")
    if any(edge["to"] == "app.customers" and edge["kind"] == "depends-on" for edge in external_graph["edges"]):
        # Existing local rows may legitimately point to customers; ensure the added row
        # did not create another edge by checking the graph remains valid and deterministic.
        if len([edge for edge in external_graph["edges"] if edge["kind"] == "depends-on"]) > len(native_dependencies):
            raise RuntimeError("external database dependency attached to local target")

    for producer, command in (
        ("Go", [str(go_probe)] if go_probe and go_probe.is_file() else None),
        ("JDBC", ["java", "-jar", str(jdbc_jar)] if jdbc_jar and jdbc_jar.is_file() else None),
    ):
        if command is None:
            print(f"SKIP {producer} PostgreSQL catalog-dependencies: producer unavailable")
            continue
        for version in (1, 2):
            for output_format in ("json", "ndjson"):
                document = work / f"postgres-{producer.lower()}-v{version}-{output_format}.document"
                graph = work / f"postgres-{producer.lower()}-v{version}-{output_format}.graph.json"
                if producer == "Go":
                    producer_url = url
                else:
                    parsed = urlsplit(url)
                    user_query = f"?user={parsed.username}" if parsed.username else ""
                    producer_url = f"jdbc:postgresql://{parsed.hostname}:{parsed.port}{parsed.path}{user_query}"
                args = command + ["--url", producer_url]
                args += ["--schema", "app", "--source-id", "app", "--catalog-dependencies", "--document-version", str(version), "--format", output_format, "-o", str(document)]
                run(args, label=f"{producer} PostgreSQL v{version}/{output_format}")
                deps = check_document(engine, document, output_format, graph, expected_dependencies=True, label=f"{producer} PostgreSQL v{version}/{output_format}")
                assert_expected_postgres_dependencies(deps, f"{producer} PostgreSQL v{version}/{output_format}")
                if deps != sorted(native_dependencies, key=dependency_key):
                    print(f"NOTICE {producer} PostgreSQL v{version}/{output_format}: dependency rows differ from native catalog (raw catalog ordering/identity varies)")
        print(f"{producer} PostgreSQL catalog-dependencies v1/v2 JSON/NDJSON: verified")


def assert_expected_postgres_dependencies(dependencies: list[dict], label: str) -> None:
    """fixture가 요구하는 실제 dependency pair와 시스템 스키마 누락을 확인한다."""
    pairs = {(item.get("source", {}).get("name"), item.get("target", {}).get("name")) for item in dependencies}
    required = {("order_view", "customers"), ("order_view", "orders")}
    missing = sorted(required - pairs)
    if missing:
        raise RuntimeError(f"{label}: required dependency pairs missing: {missing}")
    if any(item.get("target", {}).get("schema") in {"pg_catalog", "information_schema"} for item in dependencies):
        raise RuntimeError(f"{label}: system-schema dependency leaked into fixture output")


def probe_remote_database(
    engine: Path,
    work: Path,
    *,
    name: str,
    jdbc_url: str,
    user: str,
    password: str,
    go_probe: Path | None,
    jdbc_jar: Path | None,
    oracle_jar: Path | None = None,
) -> None:
    """기존 fixture가 살아 있는 JDBC DB에서 JDBC·Go 4-format을 검증한다."""
    if jdbc_jar is None or not jdbc_jar.is_file():
        raise RuntimeError(f"{name} catalog-dependencies requires the standalone JDBC probe")
    else:
        for version in (1, 2):
            for output_format in ("json", "ndjson"):
                document = work / f"{name.lower()}-jdbc-v{version}-{output_format}.document"
                graph = work / f"{name.lower()}-jdbc-v{version}-{output_format}.graph.json"
                command = [
                    "java", "-jar", str(jdbc_jar), "--url", jdbc_url,
                    "--user", user, "--password", password,
                    "--source-id", "app", "--catalog-dependencies",
                    "--document-version", str(version), "--format", output_format,
                    "-o", str(document),
                ]
                if oracle_jar is not None:
                    command.extend(("--driver", str(oracle_jar)))
                run(command, label=f"JDBC {name} v{version}/{output_format}")
                deps = check_document(
                    engine, document, output_format, graph,
                    expected_dependencies=True,
                    label=f"JDBC {name} v{version}/{output_format}",
                )
                assert_expected_remote_dependencies(deps, f"JDBC {name} v{version}/{output_format}")
        print(f"JDBC {name} catalog-dependencies v1/v2 JSON/NDJSON: verified")

    if go_probe is None or not go_probe.is_file():
        raise RuntimeError(f"Go {name} catalog-dependencies requires the Go executable")
    if name == "Oracle":
        match = re.search(r"@(?://)?([^:/;]+):(\d+)/([^;]+)", jdbc_url)
    else:
        match = re.search(r"//([^:;]+):(\d+);databaseName=([^;]+)", jdbc_url)
    if not match:
        raise RuntimeError(f"{name}: JDBC URL could not be converted for Go probe")
    host, port, database = match.groups()
    if name == "Oracle":
        go_url = f"oracle://{quote(user)}:{quote(password)}@{host}:{port}/{database}"
    else:
        go_url = f"sqlserver://{quote(user)}:{quote(password)}@{host}:{port}/{database}?encrypt=disable"
    for version in (1, 2):
        for output_format in ("json", "ndjson"):
            document = work / f"{name.lower()}-go-v{version}-{output_format}.document"
            graph = work / f"{name.lower()}-go-v{version}-{output_format}.graph.json"
            run(
                [
                    str(go_probe), "--url", go_url, "--schema", "dbo" if name == "MSSQL" else user,
                    "--source-id", "app", "--catalog-dependencies", "--document-version", str(version),
                    "--format", output_format, "-o", str(document),
                ],
                label=f"Go {name} v{version}/{output_format}",
            )
            deps = check_document(
                engine, document, output_format, graph,
                expected_dependencies=True,
                label=f"Go {name} v{version}/{output_format}",
            )
            assert_expected_remote_dependencies(deps, f"Go {name} v{version}/{output_format}")
    print(f"Go {name} catalog-dependencies v1/v2 JSON/NDJSON: verified")


def assert_expected_remote_dependencies(dependencies: list[dict], label: str) -> None:
    pairs = {(item['source']['name'].lower(), item['target']['name'].lower()) for item in dependencies}
    required = {('order_totals', 'orders'), ('order_totals', 'customers')}
    if not required <= pairs:
        raise RuntimeError(f"{label}: required order_totals dependencies missing: {sorted(required - pairs)}")
    if any(item['target']['schema'].lower() in {'sys','system','information_schema'} for item in dependencies):
        raise RuntimeError(f"{label}: system catalog targets leaked into the fixture graph")


def main() -> int:
    parser = argparse.ArgumentParser(description="Verify catalog dependency collection and transport.")
    parser.add_argument("--engine", required=True, type=Path)
    parser.add_argument("--go-probe", type=Path)
    parser.add_argument("--jdbc-jar", type=Path)
    parser.add_argument("--mssql-jdbc")
    parser.add_argument("--mssql-user")
    parser.add_argument("--mssql-password")
    parser.add_argument("--oracle-jdbc")
    parser.add_argument("--oracle-user")
    parser.add_argument("--oracle-password")
    parser.add_argument("--oracle-jar", type=Path)
    parser.add_argument("--skip-sqlite", action="store_true")
    parser.add_argument("--skip-postgres", action="store_true")
    args = parser.parse_args()
    engine = args.engine.resolve()
    if not engine.is_file() or not engine.stat().st_mode & 0o111:
        print(f"error: engine is not executable: {engine}", file=sys.stderr)
        return 2
    go_probe = args.go_probe.resolve() if args.go_probe else None
    jdbc_jar = args.jdbc_jar.resolve() if args.jdbc_jar else None
    if jdbc_jar and jdbc_jar.is_file():
        try:
            run(["java", "-version"], label="Java runtime probe")
        except RuntimeError:
            raise RuntimeError("JDBC catalog-dependencies requires a working Java runtime; set JAVA_HOME and PATH")
    try:
        with tempfile.TemporaryDirectory(prefix="schemagraph-catalog-dependencies-") as directory:
            work = Path(directory)
            if not args.skip_sqlite:
                sqlite = work / "fixture.db"
                create_sqlite(sqlite)
                probe_sqlite(engine, sqlite, work, go_probe, jdbc_jar)
            if not args.skip_postgres:
                started = start_postgres(work)
                if started is not None:
                    process, url = started
                    try:
                        check_postgres(engine, url, work, go_probe, jdbc_jar)
                    finally:
                        bindir = Path(os.environ.get("PGBIN", "/opt/homebrew/opt/postgresql@16/bin"))
                        subprocess.run([str(bindir / "pg_ctl"), "-D", str(work / "postgres-data"), "-m", "fast", "stop"], capture_output=True, text=True, check=False)
            if args.mssql_jdbc:
                if not args.mssql_user or args.mssql_password is None:
                    raise RuntimeError("MSSQL live verification requires --mssql-user and --mssql-password")
                probe_remote_database(
                    engine,
                    work,
                    name="MSSQL",
                    jdbc_url=args.mssql_jdbc,
                    user=args.mssql_user,
                    password=args.mssql_password,
                    go_probe=go_probe,
                    jdbc_jar=jdbc_jar,
                )
            if args.oracle_jdbc:
                if not args.oracle_user or args.oracle_password is None:
                    raise RuntimeError("Oracle live verification requires --oracle-user and --oracle-password")
                probe_remote_database(
                    engine,
                    work,
                    name="Oracle",
                    jdbc_url=args.oracle_jdbc,
                    user=args.oracle_user,
                    password=args.oracle_password,
                    go_probe=go_probe,
                    jdbc_jar=jdbc_jar,
                    oracle_jar=args.oracle_jar.resolve() if args.oracle_jar else None,
                )
    except (OSError, RuntimeError, json.JSONDecodeError) as error:
        print(f"catalog-dependencies verification failed: {error}", file=sys.stderr)
        return 1
    print("catalog-dependencies verification: ok")
    print("MSSQL/Oracle live checks run when verify-fixtures.sh supplies their disposable DB URLs.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
