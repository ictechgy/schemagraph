#!/usr/bin/env python3
"""동결한 DML 기대값을 실제 producer document와 published engine으로 재현한다."""

from __future__ import annotations

import argparse
import copy
from contextlib import contextmanager
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import shutil
import socket
import sqlite3
import subprocess
import tempfile
from types import SimpleNamespace
from typing import Any, Iterator


ROOT = Path(__file__).resolve().parents[1]
CORPUS_PATH = ROOT / "Fixtures/accuracy/dml-cases.json"
ENVIRONMENTS_PATH = ROOT / "Fixtures/accuracy/sqlserver-oracle-environments.json"
FROZEN_CORPUS_SHA256 = "fd129759650617b06c81ab826fb11627f5315dc72ebc1470dbdaabb10bd41d4b"
DEFAULT_PG_BIN = Path("/opt/homebrew/opt/postgresql@16/bin")
DEFAULT_JAVA = Path("/opt/homebrew/opt/openjdk@17/bin/java")


def load_module(name: str, path: Path):
    """기존 정확도 하네스의 순수 helper를 같은 소유권 경계에서 재사용한다."""

    spec = importlib.util.spec_from_file_location(name, path)
    if spec is None or spec.loader is None:
        raise RuntimeError(f"cannot load helper module: {path}")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


DML = load_module("dml_accuracy", ROOT / "Scripts/verify-dml-accuracy.py")
EXT = load_module("extended_accuracy", ROOT / "Scripts/verify-sqlserver-oracle-accuracy.py")


def digest(path: Path) -> str:
    """입력·실행 파일의 immutable 식별자를 만든다."""

    return hashlib.sha256(path.read_bytes()).hexdigest()


def run(command: list[Any], *, env: dict[str, str] | None = None, timeout: int = 120,
        redactions: tuple[str, ...] = ()) -> subprocess.CompletedProcess[str]:
    """자식 명령 실패에 비밀값을 재출력하지 않고 상한을 둔다."""

    values = [str(value) for value in command]
    child_env = env or {key: value for key, value in os.environ.items()
                        if key not in ("JAVA_TOOL_OPTIONS", "JDK_JAVA_OPTIONS", "_JAVA_OPTIONS")}
    try:
        result = subprocess.run(values, capture_output=True, text=True, env=child_env, timeout=timeout)
    except subprocess.TimeoutExpired:
        raise RuntimeError(f"{Path(values[0]).name} timed out after {timeout}s") from None
    if result.returncode:
        detail = result.stderr[-3000:]
        for value in redactions:
            if value:
                detail = detail.replace(value, "<redacted>")
        raise RuntimeError(f"{Path(values[0]).name} failed ({result.returncode}): {detail.strip()}")
    return result


def ensure_file(path: Path, label: str) -> Path:
    """실제 파일만 받아 잘못된 runner skip을 방지한다."""

    if not path.is_file():
        raise RuntimeError(f"{label} is missing or is not a regular file: {path}")
    return path.resolve()


def ensure_new_directory(path: Path) -> Path:
    """첫 baseline을 기존 결과와 섞거나 덮어쓰지 않는다."""

    if path.exists():
        raise RuntimeError(f"baseline output already exists; refusing overwrite: {path}")
    path.mkdir(parents=True)
    return path


def write_json(path: Path, value: Any) -> None:
    """완성된 JSON만 새 baseline 경로에 기록한다."""

    if path.exists() or path.is_symlink():
        raise RuntimeError(f"baseline artifact already exists: {path}")
    temporary = path.with_name(path.name + ".tmp")
    temporary.write_text(json.dumps(value, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    temporary.replace(path)


def no_credentials(path: Path, secrets: tuple[str, ...]) -> None:
    """document·graph artifact에 접속 비밀번호나 URL이 남지 않았는지 확인한다."""

    content = path.read_text(encoding="utf-8")
    for secret in secrets:
        if secret and secret in content:
            raise RuntimeError(f"credential marker found in artifact: {path.name}")


def load_frozen_corpus(path: Path = CORPUS_PATH) -> dict[str, Any]:
    """동결 commit의 기대값과 정확히 같은 byte 범위만 사용한다."""

    actual = digest(path)
    if actual != FROZEN_CORPUS_SHA256:
        raise RuntimeError(f"DML corpus SHA-256 differs from frozen baseline: {actual}")
    return DML.load_corpus(path)


def adapt_database_schema(corpus: dict[str, Any], dialect: str, actual_schema: str) -> dict[str, Any]:
    """실제 접속 사용자의 schema를 기대값의 방언 namespace에 주입한다."""

    if corpus["schemas"][dialect] == actual_schema:
        return corpus
    adapted = copy.deepcopy(corpus)
    original = adapted["schemas"][dialect]
    adapted["schemas"][dialect] = actual_schema
    for relation in adapted["relations"].values():
        if relation.get("temporary"):
            continue
        relation["ids"][dialect] = relation["ids"][dialect].replace(original + ".", actual_schema + ".", 1)
    for case in adapted["cases"]:
        if dialect in case.get("sql_overrides", {}):
            case["sql_overrides"][dialect] = case["sql_overrides"][dialect].replace(original + ".", actual_schema + ".")
    return adapted


def setup_statements(corpus: dict[str, Any], dialect: str) -> list[str]:
    """방언별 합성 base catalog를 만든다. 기대값 SQL은 재작성하지 않는다."""

    if dialect == "sqlserver":
        # SQL Server의 legacy TEXT는 비교/COALESCE가 불가하다. 사례 SQL은
        # 그대로 두고 합성 fixture의 일반 문자 컬럼만 방언에 맞게 선언한다.
        return [statement.replace(" TEXT", " NVARCHAR(120)") if statement.startswith("CREATE TABLE ") else statement
                for statement in corpus["sqlite_setup"]]
    if dialect != "oracle":
        return list(corpus["sqlite_setup"])
    return [
        "CREATE TABLE DML_SOURCE (SOURCE_ID NUMBER PRIMARY KEY, AMOUNT NUMBER NOT NULL, ACTIVE NUMBER NOT NULL, NOTE VARCHAR2(120), GROUP_ID NUMBER NOT NULL)",
        "CREATE TABLE DML_SOURCE_EXTRA (SOURCE_ID NUMBER PRIMARY KEY, MULTIPLIER NUMBER NOT NULL, LABEL VARCHAR2(120) NOT NULL)",
        "CREATE TABLE DML_TARGET (TARGET_ID NUMBER PRIMARY KEY, TARGET_VALUE NUMBER, TARGET_NOTE VARCHAR2(120), GROUP_ID NUMBER, LAST_WRITER VARCHAR2(120))",
        "CREATE TABLE DML_UPDATE_DELTA (TARGET_ID NUMBER PRIMARY KEY, BUMP NUMBER NOT NULL, REPLACEMENT_NOTE VARCHAR2(120), ACTIVE NUMBER NOT NULL)",
        "CREATE TABLE DML_MERGE_SOURCE (TARGET_ID NUMBER PRIMARY KEY, TARGET_VALUE NUMBER NOT NULL, TARGET_NOTE VARCHAR2(120) NOT NULL)",
        "INSERT INTO DML_SOURCE VALUES (1, 5, 1, 'low', 10)",
        "INSERT INTO DML_SOURCE VALUES (2, 12, 1, 'mid', 10)",
        "INSERT INTO DML_SOURCE VALUES (3, 20, 0, 'off', 20)",
        "INSERT INTO DML_SOURCE VALUES (4, 30, 1, 'high', 20)",
        "INSERT INTO DML_SOURCE VALUES (5, 8, 1, NULL, 30)",
        "INSERT INTO DML_SOURCE_EXTRA VALUES (1, 2, 'alpha')",
        "INSERT INTO DML_SOURCE_EXTRA VALUES (2, 3, 'beta')",
        "INSERT INTO DML_SOURCE_EXTRA VALUES (4, 4, 'delta')",
        "INSERT INTO DML_SOURCE_EXTRA VALUES (5, 5, 'epsilon')",
        "INSERT INTO DML_TARGET VALUES (1, 100, 'one', 10, 'seed')",
        "INSERT INTO DML_TARGET VALUES (2, 200, 'two', 10, 'seed')",
        "INSERT INTO DML_TARGET VALUES (3, 300, 'three', 20, 'seed')",
        "INSERT INTO DML_TARGET VALUES (10, 1000, 'ten', 10, 'seed')",
        "INSERT INTO DML_TARGET VALUES (20, 2000, 'twenty', 20, 'seed')",
        "INSERT INTO DML_TARGET VALUES (30, 3000, 'thirty', 30, 'seed')",
        "INSERT INTO DML_UPDATE_DELTA VALUES (10, 7, 'ten+', 1)",
        "INSERT INTO DML_UPDATE_DELTA VALUES (20, -20, 'twenty+', 1)",
        "INSERT INTO DML_UPDATE_DELTA VALUES (30, 100, 'thirty+', 0)",
        "INSERT INTO DML_MERGE_SOURCE VALUES (20, 222, 'merge-existing')",
        "INSERT INTO DML_MERGE_SOURCE VALUES (40, 444, 'merge-new')",
    ]


def target_seed(dialect: str) -> list[str]:
    """각 case를 같은 독립 target 상태에서 실행한다."""

    table = "DML_TARGET" if dialect == "oracle" else "dml_target"
    if dialect == "oracle":
        return [
            f"INSERT INTO {table} VALUES (1, 100, 'one', 10, 'seed')",
            f"INSERT INTO {table} VALUES (2, 200, 'two', 10, 'seed')",
            f"INSERT INTO {table} VALUES (3, 300, 'three', 20, 'seed')",
            f"INSERT INTO {table} VALUES (10, 1000, 'ten', 10, 'seed')",
            f"INSERT INTO {table} VALUES (20, 2000, 'twenty', 20, 'seed')",
            f"INSERT INTO {table} VALUES (30, 3000, 'thirty', 30, 'seed')",
        ]
    return [f"INSERT INTO {table} VALUES (1, 100, 'one', 10, 'seed'), (2, 200, 'two', 10, 'seed'), "
            "(3, 300, 'three', 20, 'seed'), (10, 1000, 'ten', 10, 'seed'), "
            "(20, 2000, 'twenty', 20, 'seed'), (30, 3000, 'thirty', 30, 'seed')"]


def runtime_oracle(case: dict[str, Any], dialect: str) -> dict[str, Any]:
    """native 결과가 없으면 동일한 독립 SQLite/PG row oracle을 재사용한다."""

    for candidate in (dialect, "postgres", "sqlite"):
        value = case.get("runtime", {}).get(candidate)
        if isinstance(value, dict) and value.get("supported"):
            return value
    return {"supported": False, "reason": "no runtime oracle"}


def row_values(rows: list[dict[str, Any]]) -> list[list[Any]]:
    """AccuracySql의 결정적 JSON column order를 row 배열로 투영한다."""

    return [list(row.values()) for row in rows]


def sql_file_directory(corpus: dict[str, Any], dialect: str, directory: Path) -> Path:
    """subject_contract의 SQL-file query source를 exact bytes로 생성한다."""

    for case in DML.eligible_cases(corpus, dialect):
        spec = DML.subject_spec(corpus, dialect, case)
        if spec["mode"] != "sql_files":
            continue
        relative = Path(spec["source"])
        path = directory / relative
        path.parent.mkdir(parents=True, exist_ok=True)
        if path.exists():
            raise RuntimeError(f"duplicate SQL source: {path}")
        path.write_text(DML.case_sql(case, dialect), encoding="utf-8")
    return directory


def wrapper_sql(corpus: dict[str, Any], dialect: str, case: dict[str, Any]) -> str:
    """일반 DML case의 실제 native routine DDL을 만든다."""

    spec = DML.subject_spec(corpus, dialect, case)
    raw = DML.case_sql(case, dialect).rstrip()
    body = raw if raw.endswith(";") else raw + ";"
    if dialect == "postgres":
        return corpus["procedure_wrappers"]["postgres"].format(
            schema=spec["schema"], name=spec["name"], body=raw
        ) + ";"
    if dialect == "sqlserver":
        return f"CREATE OR ALTER PROCEDURE {spec['schema']}.{spec['name']} AS BEGIN {body} END"
    if dialect == "oracle":
        return f"CREATE OR REPLACE PROCEDURE {spec['schema']}.{spec['name']} AS BEGIN {body} END;"
    raise RuntimeError(f"SQLite has no routine wrapper: {case['name']}")


def wrapper_call(dialect: str, case: dict[str, Any], corpus: dict[str, Any]) -> str:
    """routine wrapper를 실제 DB에서 호출하는 문장."""

    spec = DML.subject_spec(corpus, dialect, case)
    if dialect == "postgres":
        return f"SELECT {spec['schema']}.{spec['name']}();"
    if dialect == "sqlserver":
        return f"EXEC {spec['schema']}.{spec['name']}"
    if dialect == "oracle":
        return f"BEGIN {spec['name']}; END;"
    raise RuntimeError(f"SQLite has no routine wrapper: {case['name']}")


def case_query(dialect: str, query: str) -> str:
    """Oracle의 무인용 소문자 이름도 DB 접힘 규칙으로 실행한다."""

    return query


def dynamic_names(corpus: dict[str, Any], dialect: str, case: dict[str, Any]) -> list[str]:
    """case 뒤에 제거할 persistent CTAS/GTT object만 계산한다."""

    names: list[str] = []
    for logical in case.get("writes", {}).get("objects", []):
        relation = corpus["relations"].get(logical, {})
        if relation.get("temporary") or logical in {"source", "extra", "target", "delta", "merge_source"}:
            continue
        names.append(relation["name"].upper() if dialect == "oracle" else relation["name"])
    if dialect == "oracle":
        for logical in case.get("intermediate_writes", {}).get("objects", []):
            names.append(corpus["relations"][logical]["name"].upper())
    return sorted(set(names))


def compare_runtime(case: dict[str, Any], dialect: str, rows: list[list[Any]]) -> dict[str, Any]:
    """실제 runtime rows를 동결 oracle과 비교한다."""

    oracle = runtime_oracle(case, dialect)
    if not oracle.get("supported"):
        return {"status": "skipped", "reason": oracle.get("reason", "no runtime oracle")}
    expected = oracle["rows"]
    if dialect != "sqlite":
        expected = [[None if value is None else str(value) for value in row] for row in expected]
    if rows != expected:
        return {"status": "failed", "actual": rows, "expected": expected}
    return {"status": "passed", "rows": len(rows)}


def create_sqlite_db(corpus: dict[str, Any], path: Path) -> None:
    """native SQLite scan을 위해 persistent CTAS object까지 생성한다."""

    connection = sqlite3.connect(path)
    try:
        for statement in corpus["sqlite_setup"]:
            connection.execute(statement)
        for case in DML.eligible_cases(corpus, "sqlite"):
            connection.executescript(DML.case_sql(case, "sqlite"))
        connection.commit()
    finally:
        connection.close()


class PostgresRunner:
    """임시 PostgreSQL cluster의 같은 psql command 경계를 관리한다."""

    def __init__(self, bindir: Path, work: Path):
        self.bindir = bindir
        self.work = work
        self.data = work / "data"
        self.port = self._port()
        self.env = {key: value for key, value in os.environ.items() if not key.startswith("PG")}
        password_file = work / "empty-pgpass"
        password_file.write_text("")
        password_file.chmod(0o600)
        self.env["PGPASSFILE"] = str(password_file)
        self.started = False

    @staticmethod
    def _port() -> int:
        with socket.socket() as candidate:
            candidate.bind(("127.0.0.1", 0))
            return candidate.getsockname()[1]

    def command(self) -> list[str]:
        return [str(self.bindir / "psql"), "-X", "-q", "-At", "-F", "\t", "-P", "null=\\N",
                "-h", "127.0.0.1", "-p", str(self.port), "-U", "postgres", "-d", "dml_accuracy",
                "-v", "ON_ERROR_STOP=1"]

    def __enter__(self) -> "PostgresRunner":
        for tool in ("initdb", "pg_ctl", "psql"):
            ensure_file(self.bindir / tool, f"PostgreSQL {tool}")
        run([self.bindir / "initdb", "-D", self.data, "-U", "postgres", "-A", "trust", "--no-locale", "--encoding=UTF8"], env=self.env)
        log = self.work / "postgres.log"
        run([self.bindir / "pg_ctl", "-D", self.data, "-l", log, "-o", f"-p {self.port} -k /tmp -h 127.0.0.1", "-w", "start"], env=self.env)
        self.started = True
        run([self.bindir / "psql", "-X", "-q", "-h", "127.0.0.1", "-p", self.port, "-U", "postgres", "-d", "postgres", "-v", "ON_ERROR_STOP=1", "-c", "CREATE DATABASE dml_accuracy"], env=self.env)
        return self

    def __exit__(self, _type, _value, _traceback) -> None:
        if self.started:
            run([self.bindir / "pg_ctl", "-D", self.data, "-m", "fast", "-w", "stop"], env=self.env)

    def script(self, sql: str) -> list[str]:
        path = self.work / "statement.sql"
        path.write_text(sql, encoding="utf-8")
        result = run(self.command() + ["-f", path], env=self.env)
        return [line.split("\t") for line in result.stdout.splitlines() if line != ""]

    def execute(self, statements: list[str]) -> None:
        self.script("\n".join(statement.rstrip(";") + ";" for statement in statements))

    def rows(self, query: str) -> list[list[Any]]:
        values = self.script(query.rstrip(";") + ";")
        return [[None if value == "\\N" else value for value in row] for row in values]

    def case_rows(self, case: dict[str, Any], corpus: dict[str, Any]) -> list[list[Any]]:
        raw = DML.case_sql(case, "postgres").rstrip()
        spec = DML.subject_spec(corpus, "postgres", case)
        body = wrapper_call("postgres", case, corpus) if spec["mode"] == "wrapper" else raw
        if spec["mode"] != "wrapper" and not body.endswith(";"):
            body += ";"
        script = "BEGIN;\n" + body + "\nSELECT '__SG_ROWS__';\n" + runtime_oracle(case, "postgres")["query"] + ";\nROLLBACK;"
        values = self.script(script)
        marker = next((index for index, row in enumerate(values) if row and row[0] == "__SG_ROWS__"), None)
        if marker is None:
            raise RuntimeError(f"PostgreSQL runtime marker missing: {case['name']}")
        return [[None if value == "\\N" else value for value in row] for row in values[marker + 1:]]


@contextmanager
def postgres_cluster(corpus: dict[str, Any], bindir: Path, work: Path) -> Iterator[PostgresRunner]:
    """PostgreSQL runtime·native catalog·scan을 같은 owned cluster에서 수행한다."""

    with PostgresRunner(bindir, work) as runner:
        runner.execute(setup_statements(corpus, "postgres"))
        yield runner


def prepare_postgres(corpus: dict[str, Any], runner: PostgresRunner) -> list[dict[str, Any]]:
    """wrapper를 만들고 모든 runtime case와 persistent CTAS target을 확인한다."""

    cases = DML.eligible_cases(corpus, "postgres")
    for case in cases:
        if DML.subject_spec(corpus, "postgres", case)["mode"] == "wrapper":
            runner.execute([wrapper_sql(corpus, "postgres", case)])
    results = []
    for case in cases:
        oracle = runtime_oracle(case, "postgres")
        if not oracle.get("supported"):
            results.append({"name": case["name"], "status": "skipped", "reason": oracle.get("reason", "no runtime oracle")})
            continue
        rows = runner.case_rows(case, corpus)
        result = compare_runtime(case, "postgres", rows)
        result["name"] = case["name"]
        results.append(result)
    for case_name in ("ctas_filtered", "ctas_joined"):
        case = next(case for case in cases if case["name"] == case_name)
        runner.execute([DML.case_sql(case, "postgres")])
    return results


def prepare_sqlite(corpus: dict[str, Any], work: Path) -> tuple[dict[str, Any], Path]:
    """SQLite runtime oracle와 native reader 입력을 함께 준비한다."""

    validation = DML.validate_sqlite(corpus)
    database = work / "dml.sqlite"
    create_sqlite_db(corpus, database)
    return validation, database


def prepare_external(corpus: dict[str, Any], dialect: str, sql: Any) -> list[dict[str, Any]]:
    """SQL Server/Oracle wrapper 호출과 runtime row를 실제 DB에서 수행한다."""

    sql.execute(setup_statements(corpus, dialect))
    cases = DML.eligible_cases(corpus, dialect)
    for case in cases:
        if DML.subject_spec(corpus, dialect, case)["mode"] == "wrapper":
            sql.execute([wrapper_sql(corpus, dialect, case)])
    results = []
    dropped: set[str] = set()
    for case in cases:
        for name in sorted(dropped):
            sql.execute([f"DROP TABLE {name}"], check=False)
        dropped.clear()
        sql.execute(["DELETE FROM dml_target", *target_seed(dialect)])
        oracle = runtime_oracle(case, dialect)
        if not oracle.get("supported"):
            results.append({"name": case["name"], "status": "skipped", "reason": oracle.get("reason", "no runtime oracle")})
            continue
        spec = DML.subject_spec(corpus, dialect, case)
        if spec["mode"] == "wrapper":
            statements = [wrapper_call(dialect, case, corpus)]
        else:
            raw = DML.case_sql(case, dialect)
            statements = [part.strip() for part in raw.split(";") if part.strip()] if dialect == "oracle" else [raw]
        sql.execute(statements)
        rows = row_values(sql.rows(case_query(dialect, oracle["query"])))
        result = compare_runtime(case, dialect, rows)
        result["name"] = case["name"]
        results.append(result)
        dropped.update(dynamic_names(corpus, dialect, case))
    for case_name in ("ctas_filtered", "ctas_joined"):
        case = next(case for case in cases if case["name"] == case_name)
        sql.execute([DML.case_sql(case, dialect)])
    return results


def score_document(engine: Path, corpus: dict[str, Any], dialect: str, document: Path, graph: Path,
                   sql_dir: Path, schema: str) -> dict[str, Any]:
    """document에 SQL-file subject를 붙인 뒤 published engine graph를 채점한다."""

    attached = graph.with_name(graph.stem + ".document.json")
    run([engine, "scan", "--document", document, "--sql-dir", sql_dir,
         "--query-schema", schema, "--emit-document", attached, "-o", graph])
    catalog = DML.validate_catalog_document(json.loads(attached.read_text(encoding="utf-8")), corpus, dialect)
    if catalog["failures"]:
        raise RuntimeError("catalog preflight failed: " + "; ".join(catalog["failures"]))
    score = DML.evaluate_graph(json.loads(graph.read_text(encoding="utf-8")), corpus, dialect,
                               catalog["subject_records"])
    return {"catalog": catalog, "score": score, "attached_document": str(attached)}


def scan_native(engine: Path, corpus: dict[str, Any], dialect: str, url: str, sql_dir: Path,
                schema: str, output: Path, work: Path) -> dict[str, Any]:
    """SQLite/PostgreSQL native reader document와 graph를 보존한다."""

    output.mkdir(parents=True, exist_ok=True)
    document = output / "native.document.json"
    graph = output / "native.graph.json"
    run([engine, "scan", url, "--sql-dir", sql_dir, "--query-schema", schema,
         "--emit-document", document, "-o", graph])
    catalog = DML.validate_catalog_document(json.loads(document.read_text(encoding="utf-8")), corpus, dialect)
    if catalog["failures"]:
        raise RuntimeError("native catalog preflight failed: " + "; ".join(catalog["failures"]))
    score = DML.evaluate_graph(json.loads(graph.read_text(encoding="utf-8")), corpus, dialect,
                               catalog["subject_records"])
    return {"catalog": catalog, "score": score, "document": str(document), "graph": str(graph)}


def external_documents(engine: Path, corpus: dict[str, Any], dialect: str, sql: Any, server: dict[str, Any],
                        java: Path, classpath: str, go_probe: Path, jdbc_jar: Path, oracle_jar: Path | None,
                        output: Path, work: Path) -> dict[str, Any]:
    """Go/JDBC raw document를 각각 보존하고 같은 SQL-file subject를 붙여 채점한다."""

    args = SimpleNamespace(go_probe=go_probe, jdbc_jar=jdbc_jar, oracle_jar=oracle_jar)
    definition = {"dialect": dialect, "schema": "dbo" if dialect == "sqlserver" else "SGACC"}
    commands, environment, redactions = EXT.producer_commands(args, java, sql, server, definition)
    reports = {}
    for producer, command in commands.items():
        raw = output / f"{producer}.raw.document.json"
        graph = output / f"{producer}.graph.json"
        run(command + ["--document-version", "1", "--format", "json", "-o", raw], env=environment,
            redactions=redactions)
        sql_dir = work / f"sql-files-{producer}"
        sql_file_directory(corpus, dialect, sql_dir)
        scored = score_document(engine, corpus, dialect, raw, graph, sql_dir, definition["schema"])
        reports[producer] = {"raw_document": str(raw), "graph": str(graph), **scored}
        for path in (raw, graph, Path(scored["attached_document"])):
            no_credentials(path, redactions)
    return reports


def native_baseline(engine: Path, corpus: dict[str, Any], dialect: str, pg_bin: Path, output: Path) -> dict[str, Any]:
    """native SQLite/PG runtime과 published engine baseline을 만든다."""

    with tempfile.TemporaryDirectory(prefix=f"schemagraph-dml-{dialect}-") as directory:
        work = Path(directory)
        sql_dir = sql_file_directory(corpus, dialect, work / "sql-files")
        if dialect == "sqlite":
            runtime, database = prepare_sqlite(corpus, work)
            scan = scan_native(engine, corpus, dialect, f"sqlite:{database}", sql_dir, "main", output, work)
        else:
            with postgres_cluster(corpus, pg_bin, work) as runner:
                runtime = {"dialect": "postgres", "cases": prepare_postgres(corpus, runner)}
                scan = scan_native(engine, corpus, dialect, f"postgres://postgres@127.0.0.1:{runner.port}/dml_accuracy", sql_dir, "public", output, work)
    return {"runtime": runtime, **scan}


def external_baseline(engine: Path, corpus: dict[str, Any], dialect: str, environments: Path, java: Path,
                      probe_jar: Path, oracle_jar: Path | None, go_probe: Path, output: Path) -> dict[str, Any]:
    """pinned SQL Server/Oracle container와 두 producer baseline을 만든다."""

    database_corpus = adapt_database_schema(corpus, dialect, "SGACC" if dialect == "oracle" else "dbo")
    settings = json.loads(environments.read_text(encoding="utf-8"))
    with tempfile.TemporaryDirectory(prefix=f"schemagraph-dml-{dialect}-") as directory:
        work = Path(directory)
        classes = work / "classes"
        classes.mkdir()
        javac = java.with_name("javac")
        run([javac, "-d", classes, ROOT / "Scripts/AccuracySql.java"])
        jars = [str(classes), str(probe_jar)] + ([str(oracle_jar)] if oracle_jar else [])
        classpath = os.pathsep.join(jars)
        with EXT.database(dialect, settings, java, classpath, work) as (sql, server):
            runtime = {"dialect": dialect, "cases": prepare_external(database_corpus, dialect, sql)}
            if dialect == "sqlserver":
                runtime["supplemental"] = validate_union_select_into(sql)
            reports = external_documents(engine, database_corpus, dialect, sql, server, java, classpath,
                                         go_probe, probe_jar, oracle_jar, output, work)
    return {"runtime": runtime, "producers": reports,
            "producer_inputs": {
                "go_sha256": digest(go_probe),
                "jdbc_jar_sha256": digest(probe_jar),
                **({"oracle_driver_sha256": digest(oracle_jar)} if oracle_jar else {}),
            },
            "schema_mapping": {"expected": corpus["schemas"][dialect], "actual": database_corpus["schemas"][dialect]}}


def validate_union_select_into(sql: Any) -> dict[str, Any]:
    """동결 코퍼스와 별도로 UNION 목적지 문법과 양쪽 실제 행을 확인한다."""
    sql.execute([
        "SELECT source_id AS id, CAST(amount AS VARCHAR(30)) AS label "
        "INTO dbo.dml_union_check FROM dbo.dml_source WHERE source_id = 1 "
        "UNION ALL SELECT source_id, CAST(multiplier AS VARCHAR(30)) "
        "FROM dbo.dml_source_extra WHERE source_id = 1"
    ])
    rows = row_values(sql.rows("SELECT id, label FROM dbo.dml_union_check ORDER BY id, label"))
    actual = [[str(value) for value in row] for row in rows]
    if actual != [["1", "2"], ["1", "5"]]:
        raise RuntimeError("SQL Server UNION SELECT INTO did not preserve both input rows")
    sql.execute(["DROP TABLE dbo.dml_union_check"])
    return {"name": "union-select-into", "status": "passed", "rows": 2}


def validate_resources(args: argparse.Namespace, databases: set[str]) -> None:
    """full invocation에서 구현되지 않은 runner를 성공 skip하지 않는다."""

    ensure_file(args.engine, "published engine")
    version = run([args.engine, "--version"]).stdout
    if "0.4.3" not in version:
        raise RuntimeError(f"baseline requires published v0.4.3 engine, found: {version.strip()}")
    if databases & {"sqlserver", "oracle"}:
        if args.go_probe is None or args.probe_jar is None:
            raise RuntimeError("SQL Server/Oracle baseline requires --go-probe and --probe-jar")
        ensure_file(args.go_probe, "Go producer")
        ensure_file(args.probe_jar, "JDBC producer jar")
    if "oracle" in databases:
        if args.oracle_jar is None:
            raise RuntimeError("Oracle baseline requires --oracle-jar")
        ensure_file(args.oracle_jar, "Oracle JDBC driver")
    if "postgres" in databases:
        for tool in ("initdb", "pg_ctl", "psql"):
            ensure_file(args.postgres_bin / tool, f"PostgreSQL {tool}")


def main(argv: list[str] | None = None) -> int:
    """실제 runtime·document·scan 결과를 첫 immutable baseline으로 보존한다."""

    parser = argparse.ArgumentParser(description="Build immutable DML producer baselines with published v0.4.3.")
    parser.add_argument("--database", choices=("sqlite", "postgres", "sqlserver", "oracle", "all"), default="all")
    parser.add_argument("--engine", type=Path, required=True)
    parser.add_argument("--go-probe", type=Path)
    parser.add_argument("--probe-jar", type=Path)
    parser.add_argument("--oracle-jar", type=Path)
    parser.add_argument("--java", type=Path, default=DEFAULT_JAVA)
    parser.add_argument("--postgres-bin", type=Path, default=DEFAULT_PG_BIN)
    parser.add_argument("--environments", type=Path, default=ENVIRONMENTS_PATH)
    parser.add_argument("--corpus", type=Path, default=CORPUS_PATH)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--strict", action="store_true", help="accuracy mismatches also fail the command")
    args = parser.parse_args(argv)
    output_started = False
    databases = {"sqlite", "postgres", "sqlserver", "oracle"} if args.database == "all" else {args.database}
    try:
        corpus = load_frozen_corpus(args.corpus)
        validate_resources(args, databases)
        java = None
        if databases & {"sqlserver", "oracle"}:
            ensure_file(args.environments, "database environment manifest")
            java = ensure_file(args.java, "JDK 17 java")
        output = ensure_new_directory(args.output.resolve())
        output_started = True
        manifest = {
            "status": "running",
            "corpus_sha256": digest(args.corpus),
            "engine": {"path": str(args.engine.resolve()), "sha256": digest(args.engine),
                        "version": run([args.engine, "--version"]).stdout.strip()},
            "databases": sorted(databases),
        }
        write_json(output / "manifest.json", manifest)
        results = {}
        for dialect in ("sqlite", "postgres", "sqlserver", "oracle"):
            if dialect not in databases:
                continue
            dialect_output = output / dialect
            dialect_output.mkdir()
            try:
                if dialect in {"sqlite", "postgres"}:
                    producer = native_baseline(args.engine, corpus, dialect, args.postgres_bin, dialect_output / "native")
                else:
                    producer = external_baseline(args.engine, corpus, dialect, args.environments, java,
                                                 args.probe_jar, args.oracle_jar, args.go_probe, dialect_output)
                results[dialect] = producer
                write_json(dialect_output / "baseline.json", producer)
            except (RuntimeError, OSError, ValueError) as error:
                results[dialect] = {"operational_error": str(error)}
                write_json(dialect_output / "failure.json", results[dialect])
        operational_errors = [dialect for dialect, result in results.items() if "operational_error" in result]
        accuracy_failures = []
        for dialect, result in results.items():
            score = result.get("score") or {}
            accuracy_failures.extend(f"{dialect}: {failure}" for failure in score.get("failures", []))
            for producer, entry in (result.get("producers") or {}).items():
                accuracy_failures.extend(f"{dialect}/{producer}: {failure}"
                                         for failure in entry.get("score", {}).get("failures", []))
        final = {"status": "failed" if operational_errors or (args.strict and accuracy_failures) else "baseline-complete",
                 "operational_errors": operational_errors, "accuracy_failures": sorted(accuracy_failures),
                 "results": results}
        write_json(output / "result.json", final)
        return 1 if final["status"] == "failed" else 0
    except (RuntimeError, OSError, ValueError, json.JSONDecodeError) as error:
        print(f"dml producer baseline failed: {error}", file=os.sys.stderr)
        if output_started and args.output.exists() and args.output.is_dir():
            write_json(args.output / "failure.json", {"status": "failed", "error": str(error)})
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
