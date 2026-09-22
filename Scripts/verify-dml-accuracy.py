#!/usr/bin/env python3
"""그래프 분석기를 실행하기 전에 작성한 DML 코퍼스를 검증한다.

첫 단계는 시험할 SQL 엔진만 실행한다. 두 번째 단계는 이미 만들어진
catalog/graph를 채점한다. 두 단계의 입력을 분리해 분석기 출력으로 기대값을
만드는 순환을 막는다.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import socket
import sqlite3
import subprocess
import tempfile
from typing import Any, Iterable


ROOT = Path(__file__).resolve().parents[1]
CORPUS = ROOT / "Fixtures" / "accuracy" / "dml-cases.json"
RELATION_KINDS = {
    "table",
    "view",
    "materialized-view",
    "materialized_view",
    "foreign-table",
    "foreign_table",
    "partitioned-table",
    "partitioned_table",
}
NAME_RE = re.compile(r"^[a-z][a-z0-9_]{0,47}$")


class CorpusError(RuntimeError):
    """작성한 코퍼스의 구조가 검증 범위에 안전하지 않을 때 발생한다."""


def load_corpus(path: Path = CORPUS) -> dict[str, Any]:
    """DB나 분석기를 건드리기 전에 기대 사실을 읽고 검증한다."""

    try:
        corpus = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as error:
        raise CorpusError(f"cannot read DML corpus: {error}") from error
    validate_corpus(corpus)
    return corpus


def validate_corpus(corpus: dict[str, Any]) -> None:
    """중복·미매핑·모호한 작성 참조를 거부한다."""

    if corpus.get("version") != 1:
        raise CorpusError("unsupported DML corpus version")
    dialects = ("sqlite", "postgres", "sqlserver", "oracle")
    schemas = corpus.get("schemas")
    if not isinstance(schemas, dict) or set(dialects) - set(schemas):
        raise CorpusError("corpus must name every supported dialect schema")
    contract = corpus.get("subject_contract")
    if not isinstance(contract, dict):
        raise CorpusError("corpus has no subject contract")
    if not isinstance(contract.get("sql_file_source_template"), str):
        raise CorpusError("subject contract needs a SQL-file source template")
    if set(contract.get("sql_file_dialects", [])) - set(dialects):
        raise CorpusError("subject contract names an unknown SQL-file dialect")
    if not isinstance(contract.get("standalone_wrapper_modes"), list):
        raise CorpusError("subject contract needs standalone wrapper modes")
    for dialect in dialects:
        subject = contract.get("dialects", {}).get(dialect)
        allowed_kinds = {"query"} if dialect == "sqlite" else {"function", "procedure"}
        if not isinstance(subject, dict) or subject.get("kind") not in allowed_kinds:
            raise CorpusError(f"subject contract has no routine kind for {dialect}")
    relations = corpus.get("relations")
    if not isinstance(relations, dict) or not relations:
        raise CorpusError("corpus has no relation catalog")
    ids_by_dialect: dict[str, set[str]] = {dialect: set() for dialect in dialects}
    names: set[str] = set()
    for logical, relation in relations.items():
        if not NAME_RE.fullmatch(logical):
            raise CorpusError(f"invalid logical relation name: {logical}")
        if logical in names:
            raise CorpusError(f"duplicate logical relation: {logical}")
        names.add(logical)
        if not isinstance(relation.get("name"), str) or not relation["name"]:
            raise CorpusError(f"relation {logical} has no physical name")
        columns = relation.get("columns")
        if not isinstance(columns, list) or not columns or len(columns) != len(set(columns)):
            raise CorpusError(f"relation {logical} has invalid columns")
        if any(not isinstance(column, str) or not column for column in columns):
            raise CorpusError(f"relation {logical} has an invalid column name")
        if relation.get("temporary"):
            if relation.get("logical_only") is not True or relation.get("ids") is not None:
                raise CorpusError(f"temporary relation {logical} must remain a logical symbol")
            continue
        for dialect in dialects:
            identifier = relation.get("ids", {}).get(dialect)
            if not isinstance(identifier, str) or not identifier:
                raise CorpusError(f"relation {logical} has no {dialect} catalog ID")
            if identifier in ids_by_dialect[dialect]:
                raise CorpusError(f"duplicate {dialect} catalog ID: {identifier}")
            ids_by_dialect[dialect].add(identifier)
    cases = corpus.get("cases")
    if not isinstance(cases, list) or not 12 <= len(cases) <= 20:
        raise CorpusError("DML corpus must contain 12-20 cases")
    case_names: set[str] = set()
    for case in cases:
        validate_case(corpus, case, case_names, dialects)
    if len(case_names) != len(cases):
        raise CorpusError("case names must be unique")


def validate_case(
    corpus: dict[str, Any],
    case: dict[str, Any],
    case_names: set[str],
    dialects: Iterable[str],
) -> None:
    """SQL 의미를 추측하지 않고 한 사례의 구조만 검증한다."""

    name = case.get("name")
    if not isinstance(name, str) or not NAME_RE.fullmatch(name) or name in case_names:
        raise CorpusError(f"invalid or duplicate case name: {name!r}")
    case_names.add(name)
    if case.get("kind") != "procedure":
        raise CorpusError(f"{name}: DML cases must use procedure subjects")
    if case.get("state") not in {"complete", "partial", "unsupported"}:
        raise CorpusError(f"{name}: invalid expected analysis state")
    if case.get("fact_state", "complete") not in {"complete", "partial", "unsupported"}:
        raise CorpusError(f"{name}: invalid fact state")
    body = case.get("body")
    if not isinstance(body, str) or not body.strip():
        raise CorpusError(f"{name}: missing dialect-neutral SQL body")
    raw_supported = case.get("supported_dialects", list(dialects))
    if not isinstance(raw_supported, list) or not raw_supported:
        raise CorpusError(f"{name}: supported_dialects must be a nonempty list")
    supported = set(raw_supported)
    unknown = supported - set(dialects)
    if unknown:
        raise CorpusError(f"{name}: unknown dialects: {sorted(unknown)}")
    if "sqlite" in supported and case.get("runtime", {}).get("sqlite", {}).get("supported") is not True:
        raise CorpusError(f"{name}: SQLite-supported case needs an executable runtime oracle")
    facts = ("reads", "writes")
    for direction in facts:
        value = case.get(direction)
        if not isinstance(value, dict):
            raise CorpusError(f"{name}: missing {direction} fact object")
        for field in ("objects", "columns"):
            if not isinstance(value.get(field), list) or len(value[field]) != len(set(value[field])):
                raise CorpusError(f"{name}: invalid {direction}.{field}")
            for reference in value[field]:
                validate_reference(corpus, reference, name, allow_column=(field == "columns"))
    lineage = case.get("lineage")
    if not isinstance(lineage, dict):
        raise CorpusError(f"{name}: missing lineage map")
    for target, sources in lineage.items():
        validate_reference(corpus, target, name, allow_column=True)
        if not isinstance(sources, list) or len(sources) != len(set(sources)):
            raise CorpusError(f"{name}: invalid lineage sources for {target}")
        for source in sources:
            validate_reference(corpus, source, name, allow_column=True)
    intermediate = case.get("intermediate_writes")
    if intermediate is not None:
        if not isinstance(intermediate, dict):
            raise CorpusError(f"{name}: invalid intermediate_writes")
        for field in ("objects", "columns"):
            for reference in intermediate.get(field, []):
                validate_reference(corpus, reference, name, allow_column=(field == "columns"))
    limitations = set(case.get("analysis_limitations", []))
    self_sources = {
        source
        for target, sources in lineage.items()
        for source in sources
        if source == target
    }
    if self_sources and "SG_TEMPORAL_SELF_LINEAGE" not in limitations:
        raise CorpusError(f"{name}: temporal self-lineage needs an explicit limitation")
    if "SG_TEMPORAL_SELF_LINEAGE" in limitations and case["state"] != "partial":
        raise CorpusError(f"{name}: temporal self-lineage must expect partial analysis")
    overrides = case.get("sql_overrides", {})
    if not isinstance(overrides, dict) or not set(overrides) <= set(dialects):
        raise CorpusError(f"{name}: invalid SQL dialect overrides")
    for dialect in supported:
        sql = overrides.get(dialect, body)
        if not isinstance(sql, str) or not sql.strip():
            raise CorpusError(f"{name}: empty SQL for {dialect}")
    for dialect, runtime in case.get("runtime", {}).items():
        if dialect not in dialects or not isinstance(runtime, dict):
            raise CorpusError(f"{name}: invalid runtime dialect {dialect}")
        if runtime.get("supported") and (not isinstance(runtime.get("query"), str) or not isinstance(runtime.get("rows"), list)):
            raise CorpusError(f"{name}: runtime oracle needs query and rows")


def validate_reference(corpus: dict[str, Any], reference: Any, case_name: str, *, allow_column: bool) -> None:
    """작성한 relation.column 논리 표기가 실제 catalog 기호인지 확인한다."""

    if not isinstance(reference, str):
        raise CorpusError(f"{case_name}: fact reference must be a string")
    pieces = reference.split(".")
    if len(pieces) == 1 and not allow_column:
        logical = pieces[0]
        if logical not in corpus["relations"]:
            raise CorpusError(f"{case_name}: unknown relation {reference}")
        return
    if len(pieces) != 2 or pieces[0] not in corpus["relations"]:
        raise CorpusError(f"{case_name}: invalid fact reference {reference}")
    if not allow_column or pieces[1] not in corpus["relations"][pieces[0]]["columns"]:
        raise CorpusError(f"{case_name}: unknown column reference {reference}")


def relation_id(corpus: dict[str, Any], dialect: str, logical: str) -> str:
    """논리 relation을 코퍼스가 예약한 정확한 catalog ID로 바꾼다."""

    if corpus["relations"].get(logical, {}).get("temporary"):
        raise CorpusError(f"temporary relation {logical} has no persistent graph ID")
    try:
        return corpus["relations"][logical]["ids"][dialect]
    except KeyError as error:
        raise CorpusError(f"no {dialect} catalog ID for {logical}") from error


def query_name(relative: str) -> str:
    """engine/source sql_files.rs의 byte-hex 이름 규칙과 맞춘다."""

    return "query_" + "".join(f"{byte:02x}" for byte in relative.encode("utf-8"))


def body_hash(body: str) -> str:
    """parser scope::body_hash와 같은 sha256 접두사 hash를 만든다."""

    return "sha256:" + hashlib.sha256(body.encode("utf-8")).hexdigest()


def eligible_cases(corpus: dict[str, Any], dialect: str) -> list[dict[str, Any]]:
    """runtime·catalog·graph 채점 전에 방언 필터를 적용한다."""

    return [
        case for case in corpus["cases"]
        if dialect in set(case.get("supported_dialects", ("sqlite", "postgres", "sqlserver", "oracle")))
    ]


def subject_spec(corpus: dict[str, Any], dialect: str, case: dict[str, Any]) -> dict[str, Any]:
    """한 사례와 방언에서 document가 실제로 가질 subject를 기술한다."""

    contract = corpus["subject_contract"]
    standalone = case.get("wrapper_mode") in set(contract["standalone_wrapper_modes"])
    query_subject = dialect in set(contract["sql_file_dialects"]) or standalone
    if query_subject:
        source = contract["sql_file_source_template"].format(name=case["name"])
        return {
            "schema": corpus["schemas"][dialect],
            "name": query_name(source),
            "kind": "query",
            "source": source,
            "expected_body_hash": body_hash(case_sql(case, dialect)),
            "mode": "sql_files",
        }
    name = f"acc_{case['name']}"
    if dialect == "oracle":
        name = name.upper()
    return {
        "schema": corpus["schemas"][dialect],
        "name": name,
        "kind": contract["dialects"][dialect]["kind"],
        "source": None,
        "expected_body_hash": None,
        "mode": "wrapper",
    }


def subject_id(spec: dict[str, Any]) -> str:
    """subject 명세에서 정확한 graph ID를 만든다."""

    return f"{spec['schema']}.{spec['name']}"


def column_id(corpus: dict[str, Any], dialect: str, reference: str) -> str:
    """이름을 추측하지 않고 논리 relation.column을 ID로 바꾼다."""

    relation, column = reference.split(".", 1)
    if dialect == "oracle":
        column = column.upper()
    return f"{relation_id(corpus, dialect, relation)}.{column}"


def expected_facts(corpus: dict[str, Any], case: dict[str, Any], dialect: str) -> dict[str, Any]:
    """한 방언의 catalog ID 집합으로 기대 사실을 구체화한다."""

    def refs(values: list[str], columns: bool) -> set[str]:
        return {column_id(corpus, dialect, value) if columns else relation_id(corpus, dialect, value) for value in values}

    return {
        "object_reads": refs(case["reads"]["objects"], False),
        "column_reads": refs(case["reads"]["columns"], True),
        "object_writes": refs(case["writes"]["objects"], False),
        "column_writes": refs(case["writes"]["columns"], True),
        "lineage": {
            (column_id(corpus, dialect, target), column_id(corpus, dialect, source))
            for target, sources in case["lineage"].items()
            for source in sources
        },
        "intermediate_writes": {
            "objects": set(case.get("intermediate_writes", {}).get("objects", [])),
            "columns": set(case.get("intermediate_writes", {}).get("columns", [])),
        },
    }


def case_sql(case: dict[str, Any], dialect: str) -> str:
    """공통 body를 기본으로 방언별 작성 SQL을 반환한다."""

    return case.get("sql_overrides", {}).get(dialect, case["body"])


def _json_value(value: Any) -> Any:
    """SQLite 값을 runtime oracle의 JSON scalar로 바꾼다."""

    if value is None or isinstance(value, (str, int, float, bool)):
        return value
    return str(value)


def validate_sqlite(corpus: dict[str, Any], selected: set[str] | None = None) -> dict[str, Any]:
    """SQLite가 지원하는 각 사례를 새 메모리 DB에서 실행한다."""

    report: dict[str, Any] = {
        "dialect": "sqlite",
        "phase": "database-validation-only",
        "analyzer_evaluated": False,
        "cases": [],
        "passed": 0,
        "skipped": 0,
        "failed": 0,
    }
    for case in corpus["cases"]:
        if selected and case["name"] not in selected:
            continue
        runtime = case.get("runtime", {}).get("sqlite", {})
        if "sqlite" not in set(case.get("supported_dialects", ("sqlite", "postgres", "sqlserver", "oracle"))) or not runtime.get("supported"):
            report["skipped"] += 1
            report["cases"].append({"name": case["name"], "status": "skipped", "reason": runtime.get("reason", "dialect is unsupported")})
            continue
        result = {"name": case["name"], "status": "passed", "statement_sha256": _sha256(case_sql(case, "sqlite"))}
        connection = sqlite3.connect(":memory:")
        try:
            for statement in corpus["sqlite_setup"]:
                connection.execute(statement)
            connection.executescript(case_sql(case, "sqlite"))
            actual = [[_json_value(value) for value in row] for row in connection.execute(runtime["query"])]
            expected = runtime["rows"]
            if actual != expected:
                raise CorpusError(f"runtime rows differ: actual={actual!r}, expected={expected!r}")
            result["rows"] = len(actual)
            report["passed"] += 1
        except (sqlite3.Error, CorpusError) as error:
            result["status"] = "failed"
            result["error"] = str(error)
            report["failed"] += 1
        finally:
            connection.close()
        report["cases"].append(result)
    if report["failed"]:
        report["status"] = "failed"
    else:
        report["status"] = "passed"
    return report


def _runtime_oracle(case: dict[str, Any], dialect: str) -> dict[str, Any]:
    """native runtime oracle가 있으면 쓰고 없으면 공통 row oracle로 보완한다."""

    native = case.get("runtime", {}).get(dialect)
    if isinstance(native, dict):
        return native
    shared = case.get("runtime", {}).get("sqlite", {})
    if shared.get("supported"):
        return {"supported": True, "query": shared["query"], "rows": shared["rows"], "source": "shared-sqlite-oracle"}
    return {"supported": False, "reason": shared.get("reason", "no native runtime oracle")}


def _pg_run(command: list[str], *, env: dict[str, str], timeout: float = 120) -> subprocess.CompletedProcess[str]:
    """호출자의 PostgreSQL 설정을 물려받지 않고 유틸리티를 실행한다."""

    return subprocess.run(command, capture_output=True, text=True, env=env, timeout=timeout)


def validate_postgres(
    corpus: dict[str, Any],
    selected: set[str] | None = None,
    bindir: Path = Path("/opt/homebrew/opt/postgresql@16/bin"),
) -> dict[str, Any]:
    """소유한 임시 cluster에서 PostgreSQL 지원 사례를 모두 검증한다."""

    report: dict[str, Any] = {
        "dialect": "postgres",
        "phase": "database-validation-only",
        "analyzer_evaluated": False,
        "cases": [],
        "passed": 0,
        "skipped": 0,
        "failed": 0,
        "server_version": None,
    }
    tools = {name: bindir / name for name in ("initdb", "pg_ctl", "psql")}
    missing = [str(path) for path in tools.values() if not path.is_file()]
    if missing:
        report["status"] = "failed"
        report["error"] = "PostgreSQL 16 tools are required: " + ", ".join(missing)
        report["failed"] = 1
        return report
    cases = [case for case in corpus["cases"] if not selected or case["name"] in selected]
    with tempfile.TemporaryDirectory(prefix="schemagraph-dml-postgres-") as directory:
        work = Path(directory)
        data = work / "data"
        socket_dir = Path("/tmp")
        env = {key: value for key, value in os.environ.items() if not key.startswith("PG")}
        password_file = work / "empty-pgpass"
        password_file.write_text("")
        password_file.chmod(0o600)
        env["PGPASSFILE"] = str(password_file)
        init = _pg_run([str(tools["initdb"]), "-D", str(data), "-U", "postgres", "-A", "trust", "--no-locale", "--encoding=UTF8"], env=env)
        if init.returncode:
            report["status"] = "failed"
            report["error"] = f"initdb failed: {init.stderr[-2000:]}"
            report["failed"] = 1
            return report
        with socket.socket() as candidate:
            candidate.bind(("127.0.0.1", 0))
            port = candidate.getsockname()[1]
        log = work / "postgres.log"
        started = _pg_run([str(tools["pg_ctl"]), "-D", str(data), "-l", str(log), "-o", f"-p {port} -k {socket_dir} -h 127.0.0.1", "-w", "start"], env=env)
        if started.returncode:
            report["status"] = "failed"
            report["error"] = f"temporary PostgreSQL failed to start: {log.read_text(errors='replace')[-2000:]}"
            report["failed"] = 1
            return report
        admin = [str(tools["psql"]), "-X", "-q", "-h", "127.0.0.1", "-p", str(port), "-U", "postgres", "-d", "postgres", "-v", "ON_ERROR_STOP=1"]
        try:
            created = _pg_run(admin + ["-c", "CREATE DATABASE dml_accuracy"], env=env)
            if created.returncode:
                report["status"] = "failed"
                report["error"] = f"temporary PostgreSQL database creation failed: {created.stderr[-2000:]}"
                report["failed"] = 1
            else:
                version = _pg_run(admin + ["-At", "-c", "SELECT version()"], env=env)
                report["server_version"] = version.stdout.strip()
                database = [str(tools["psql"]), "-X", "-q", "-At", "-F", "\t", "-P", "null=\\N", "-h", "127.0.0.1", "-p", str(port), "-U", "postgres", "-d", "dml_accuracy", "-v", "ON_ERROR_STOP=1"]
                for case in cases:
                    if "postgres" not in set(case.get("supported_dialects", ("sqlite", "postgres", "sqlserver", "oracle"))):
                        report["skipped"] += 1
                        report["cases"].append({"name": case["name"], "status": "skipped", "reason": "dialect is unsupported"})
                        continue
                    runtime = _runtime_oracle(case, "postgres")
                    if not runtime.get("supported"):
                        report["skipped"] += 1
                        report["cases"].append({"name": case["name"], "status": "skipped", "reason": runtime.get("reason", "no PostgreSQL runtime oracle")})
                        continue
                    setup = corpus.get("postgres_setup", corpus["sqlite_setup"])
                    script = work / f"{case['name']}.sql"
                    raw_body = case_sql(case, "postgres")
                    spec = subject_spec(corpus, "postgres", case)
                    wrapper = spec["mode"] == "wrapper"
                    body = raw_body.rstrip()
                    if not wrapper and not body.endswith(";"):
                        body += ";"
                    wrapper_sql = ""
                    wrapper_probe = ""
                    call_sql = ""
                    if wrapper:
                        wrapper_sql = corpus["procedure_wrappers"]["postgres"].format(
                            schema=spec["schema"], name=spec["name"], body=raw_body
                        ) + ";"
                        call_sql = f"SELECT {spec['schema']}.{spec['name']}();"
                        wrapper_probe = (
                            "SELECT '__SG_WRAPPER__', encode(convert_to(p.prosrc, 'UTF8'), 'hex') "
                            "FROM pg_proc p JOIN pg_namespace n ON n.oid = p.pronamespace "
                            f"WHERE n.nspname = '{spec['schema']}' AND p.proname = '{spec['name']}' "
                            "AND pg_get_function_identity_arguments(p.oid) = '';"
                        )
                    else:
                        wrapper_probe = ""
                    script.write_text(
                        "BEGIN;\nDROP SCHEMA public CASCADE;\nCREATE SCHEMA public;\n"
                        + "\n".join(statement + ";" for statement in setup)
                        + "\n"
                        + (wrapper_sql + "\n" + call_sql + "\n" if wrapper else body + "\n")
                        + wrapper_probe
                        + ("\nSELECT '__SG_ROWS__';\n" if wrapper else "SELECT '__SG_ROWS__';\n")
                        + runtime["query"]
                        + ";\nROLLBACK;\n",
                        encoding="utf-8",
                    )
                    result = _pg_run(database + ["-f", str(script)], env=env)
                    item = {
                        "name": case["name"],
                        "status": "passed",
                        "statement_sha256": _sha256(raw_body),
                        "execution_mode": "wrapper" if wrapper else "sql-only",
                        "subject_kind": spec["kind"],
                        "subject_name": spec["name"],
                    }
                    if spec["source"] is not None:
                        item["source"] = spec["source"]
                    if result.returncode:
                        item["status"] = "failed"
                        item["error"] = result.stderr[-2000:]
                        report["failed"] += 1
                    else:
                        lines = [line for line in result.stdout.splitlines() if line != ""]
                        rows_start = next((index + 1 for index, line in enumerate(lines) if line.split("\t")[0] == "__SG_ROWS__"), None)
                        actual = [line.split("\t") for line in lines[rows_start:]] if rows_start is not None else []
                        if wrapper:
                            wrapper_line = next((line.split("\t") for line in lines if line.split("\t")[0] == "__SG_WRAPPER__"), None)
                            expected_prosrc = wrapper_sql.split("$$", 2)[1]
                            item["wrapper_prosrc_sha256"] = body_hash(expected_prosrc)
                            if wrapper_line is not None and len(wrapper_line) == 2:
                                item["observed_wrapper_prosrc_sha256"] = body_hash(bytes.fromhex(wrapper_line[1]).decode("utf-8"))
                            if wrapper_line is None or len(wrapper_line) != 2 or wrapper_line[1] != expected_prosrc.encode("utf-8").hex():
                                item["status"] = "failed"
                                item["error"] = "PostgreSQL pg_proc.prosrc differs from the authored wrapper body"
                                report["failed"] += 1
                                report["cases"].append(item)
                                continue
                        expected = [["\\N" if value is None else str(value) for value in row] for row in runtime["rows"]]
                        if actual != expected:
                            item["status"] = "failed"
                            item["error"] = f"runtime rows differ: actual={actual!r}, expected={expected!r}"
                            report["failed"] += 1
                        else:
                            item["rows"] = len(actual)
                            report["passed"] += 1
                    report["cases"].append(item)
        finally:
            _pg_run([str(tools["pg_ctl"]), "-D", str(data), "-m", "fast", "-w", "stop"], env=env)
    report["status"] = "failed" if report["failed"] else "passed"
    return report


def _sha256(value: str) -> str:
    import hashlib

    return hashlib.sha256(value.encode("utf-8")).hexdigest()


def _file_sha256(path: Path) -> str:
    """동결 사례 파일의 byte hash로 보고 범위가 바뀌지 않았음을 남긴다."""

    import hashlib

    return hashlib.sha256(path.read_bytes()).hexdigest()


def _score(expected: set[str], actual: set[str]) -> dict[str, Any]:
    """누락과 예기치 않은 값을 숨기지 않고 정확한 사실 집합을 비교한다."""

    missing = sorted(expected - actual)
    unexpected = sorted(actual - expected)
    return {"expected": len(expected), "matched": len(expected & actual), "missing": missing, "unexpected": unexpected}


def _analysis_item(graph: dict[str, Any], subject: str) -> dict[str, Any] | None:
    return next((item for item in graph.get("analysis", []) if item.get("id") == subject), None)


def _subject_id(corpus: dict[str, Any], dialect: str, case: dict[str, Any]) -> str:
    return subject_id(subject_spec(corpus, dialect, case))


def validate_catalog_document(document: dict[str, Any], corpus: dict[str, Any], dialect: str) -> dict[str, Any]:
    """graph 채점 전에 raw subject metadata를 확인한다."""

    if not isinstance(document.get("schemas"), list):
        raise CorpusError("catalog document needs a schemas array")
    relation_ids: set[str] = set()
    relation_kinds: dict[str, str] = {}
    column_ids: set[str] = set()
    subjects: dict[str, list[dict[str, Any]]] = {}
    for schema in document["schemas"]:
        if not isinstance(schema, dict) or not isinstance(schema.get("name"), str):
            raise CorpusError("catalog document has an invalid schema")
        schema_name = schema["name"]
        for obj in schema.get("objects", []):
            if not isinstance(obj, dict) or not isinstance(obj.get("name"), str):
                continue
            object_id = f"{schema_name}.{obj['name']}"
            if obj.get("kind") in RELATION_KINDS:
                relation_ids.add(object_id)
                relation_kinds[object_id] = obj.get("kind")
                for column in obj.get("columns", []):
                    if isinstance(column, dict) and isinstance(column.get("name"), str):
                        column_ids.add(f"{object_id}.{column['name']}")
            for routine in obj.get("routines", []):
                if isinstance(routine, dict) and isinstance(routine.get("name"), str):
                    record = _subject_record(schema_name, routine)
                    subjects.setdefault(record["id"], []).append(record)
        for routine in schema.get("routines", []):
            if isinstance(routine, dict) and isinstance(routine.get("name"), str):
                record = _subject_record(schema_name, routine)
                subjects.setdefault(record["id"], []).append(record)
    expected_relations = {
        relation_id(corpus, dialect, logical)
        for case in eligible_cases(corpus, dialect)
        for direction in ("reads", "writes")
        for logical in case[direction]["objects"]
    }
    expected_columns = {
        column_id(corpus, dialect, reference)
        for case in eligible_cases(corpus, dialect)
        for direction in ("reads", "writes")
        for reference in case[direction]["columns"]
    }
    missing_relations = sorted(expected_relations - relation_ids)
    wrong_relation_kinds = sorted(
        f"{relation}: expected table, found {relation_kinds[relation]}"
        for relation in expected_relations
        if relation in relation_kinds and relation_kinds[relation] != "table"
    )
    missing_columns = sorted(expected_columns - column_ids)
    missing_subjects: list[str] = []
    wrong_kinds: list[str] = []
    wrong_sources: list[str] = []
    wrong_bodies: list[str] = []
    duplicate_body_hashes: list[str] = []
    selected_subjects: dict[str, dict[str, Any]] = {}
    body_hash_owners: dict[str, str] = {}
    for case in eligible_cases(corpus, dialect):
        spec = subject_spec(corpus, dialect, case)
        expected_id = subject_id(spec)
        records = subjects.get(expected_id, [])
        if len(records) != 1:
            missing_subjects.append(expected_id)
            continue
        actual = records[0]
        selected_subjects[case["name"]] = {**actual, "expected": spec}
        if not actual.get("body_hash"):
            wrong_bodies.append(f"{expected_id}: subject body is missing")
        else:
            prior = body_hash_owners.setdefault(actual["body_hash"], case["name"])
            if prior != case["name"]:
                duplicate_body_hashes.append(f"{actual['body_hash']}: {prior}, {case['name']}")
        if actual["kind"] != spec["kind"]:
            wrong_kinds.append(f"{expected_id}: expected {spec['kind']}, found {actual['kind']}")
        if spec["mode"] == "sql_files":
            if actual.get("source") != spec["source"]:
                wrong_sources.append(f"{expected_id}: expected source {spec['source']!r}, found {actual.get('source')!r}")
            if actual.get("body_hash") != spec["expected_body_hash"]:
                wrong_bodies.append(f"{expected_id}: SQL-file body hash differs")
        elif actual.get("source") is not None:
            wrong_sources.append(f"{expected_id}: wrapper subject unexpectedly has source {actual['source']!r}")
    return {
        "relations": {"expected": len(expected_relations), "missing": missing_relations},
        "columns": {"expected": len(expected_columns), "missing": missing_columns},
        "subjects": {"expected": len(eligible_cases(corpus, dialect)), "missing": sorted(missing_subjects)},
        "subject_records": selected_subjects,
        "duplicate_body_hashes": sorted(duplicate_body_hashes),
        "failures": [
            *(f"missing catalog relation: {value}" for value in missing_relations),
            *(f"missing catalog column: {value}" for value in missing_columns),
            *(f"missing catalog subject: {value}" for value in sorted(missing_subjects)),
            *wrong_relation_kinds,
            *wrong_kinds,
            *wrong_sources,
            *wrong_bodies,
            *(f"duplicate subject body hash: {value}" for value in sorted(duplicate_body_hashes)),
        ],
    }


def _subject_record(schema: str, routine: dict[str, Any]) -> dict[str, Any]:
    """body byte를 바꾸지 않고 raw routine 행을 정규화한다."""

    name = routine["name"]
    signature = routine.get("signature")
    if signature:
        name = f"{name}({signature})"
    body = routine.get("body")
    return {
        "id": f"{schema}.{name}",
        "name": routine["name"],
        "kind": routine.get("kind"),
        "source": routine.get("source"),
        "body_hash": body_hash(body) if isinstance(body, str) else None,
        "body_present": isinstance(body, str),
    }


def _derived_subject_records(corpus: dict[str, Any], dialect: str) -> dict[str, dict[str, Any]]:
    """raw catalog가 없는 self-test에서만 사용할 subject map을 만든다."""

    records = {}
    for case in eligible_cases(corpus, dialect):
        spec = subject_spec(corpus, dialect, case)
        records[case["name"]] = {
            "id": subject_id(spec),
            "kind": spec["kind"],
            "source": spec["source"],
            "body_hash": spec["expected_body_hash"] or body_hash(case_sql(case, dialect)),
            "expected": spec,
        }
    return records


def evaluate_graph(
    graph: dict[str, Any],
    corpus: dict[str, Any],
    dialect: str,
    subject_records: dict[str, dict[str, Any]] | None = None,
) -> dict[str, Any]:
    """graph 사실과 owner 귀속 origin을 동결 기대값에 대조한다."""

    if dialect in {"postgres", "sqlserver", "oracle"} and subject_records is None:
        raise CorpusError("wrapper-dialect graph scoring requires raw catalog subject records")
    vertices = graph.get("vertices")
    edges = graph.get("edges")
    if not isinstance(vertices, list) or not isinstance(edges, list):
        raise CorpusError("graph needs vertices and edges arrays")
    if any(not isinstance(vertex, dict) for vertex in vertices):
        raise CorpusError("graph vertices must be objects")
    if any(not isinstance(edge, dict) for edge in edges):
        raise CorpusError("graph edges must be objects")
    vertex_map = {vertex.get("id"): vertex for vertex in vertices}
    failures: list[str] = []
    if any(vertex.get("id") is None for vertex in vertices):
        failures.append("null graph vertex identity")
    if len(vertex_map) != len(vertices):
        failures.append("duplicate graph vertex identities")
    if any(edge.get("from") is None or edge.get("to") is None for edge in edges):
        failures.append("null graph edge endpoint")
    ghosts = [edge for edge in edges if edge.get("from") not in vertex_map or edge.get("to") not in vertex_map]
    if ghosts:
        failures.append("phantom graph endpoints")
    origins = graph.get("origins", [])
    if not isinstance(origins, list):
        raise CorpusError("graph origins must be an array")
    graph_edges = {(edge.get("from"), edge.get("to"), edge.get("kind")) for edge in edges}
    origin_ghosts = [origin for origin in origins if (origin.get("from"), origin.get("to"), origin.get("kind")) not in graph_edges]
    if origin_ghosts:
        failures.append("phantom graph origins")
    records = subject_records if subject_records is not None else _derived_subject_records(corpus, dialect)
    cases: list[dict[str, Any]] = []
    false_complete_cases: list[str] = []
    all_expected_lineage: set[tuple[str, str]] = set()
    all_expected_outputs: set[str] = set()
    actual_global_lineage: set[tuple[str, str]] = set()
    for case in eligible_cases(corpus, dialect):
        facts = expected_facts(corpus, case, dialect)
        all_expected_lineage.update(pair for pair in facts["lineage"] if pair[0] != pair[1])
        # 작성 값이 상수여도 destination column은 범위에 남긴다. 그렇지 않으면
        # last_writer에 붙은 extra derives-from 간선이 global 검사에서 사라진다.
        all_expected_outputs.update(facts["column_writes"])
    for edge in edges:
        pair = (edge.get("from"), edge.get("to"))
        if edge.get("kind") == "derives-from" and pair[0] in all_expected_outputs and pair[1] in vertex_map:
            actual_global_lineage.add(pair)
    for case in eligible_cases(corpus, dialect):
        facts = expected_facts(corpus, case, dialect)
        record = records.get(case["name"])
        spec = subject_spec(corpus, dialect, case)
        subject = record.get("id") if record else subject_id(spec)
        if not record:
            failures.append(f"{case['name']}: missing subject metadata")
            continue
        if subject not in vertex_map:
            failures.append(f"{subject}: missing subject vertex")
            continue
        subject_vertex = vertex_map[subject]
        outgoing = [edge for edge in edges if edge.get("from") == subject]
        kind_errors = []
        if subject_vertex.get("kind") != spec["kind"]:
            kind_errors.append(f"{subject}: expected kind {spec['kind']}, found {subject_vertex.get('kind')}")
        for endpoint in facts["object_reads"] | facts["object_writes"]:
            if endpoint in vertex_map and vertex_map[endpoint].get("kind") != "table":
                kind_errors.append(f"{endpoint}: expected kind table, found {vertex_map[endpoint].get('kind')}")
        for endpoint in facts["column_reads"] | facts["column_writes"]:
            if endpoint in vertex_map and vertex_map[endpoint].get("kind") != "column":
                kind_errors.append(f"{endpoint}: expected kind column, found {vertex_map[endpoint].get('kind')}")
        actual_objects_read = {edge["to"] for edge in outgoing if edge.get("kind") == "reads" and vertex_map.get(edge.get("to"), {}).get("kind") == "table"}
        actual_columns_read = {edge["to"] for edge in outgoing if edge.get("kind") == "reads" and vertex_map.get(edge.get("to"), {}).get("kind") == "column"}
        actual_objects_write = {edge["to"] for edge in outgoing if edge.get("kind") == "writes" and vertex_map.get(edge.get("to"), {}).get("kind") == "table"}
        actual_columns_write = {edge["to"] for edge in outgoing if edge.get("kind") == "writes" and vertex_map.get(edge.get("to"), {}).get("kind") == "column"}
        actual_read_endpoints = {edge["to"] for edge in outgoing if edge.get("kind") == "reads" and edge.get("to") in vertex_map}
        actual_write_endpoints = {edge["to"] for edge in outgoing if edge.get("kind") == "writes" and edge.get("to") in vertex_map}
        owner_hash = record.get("body_hash")
        owner_origins = {
            (origin.get("from"), origin.get("to"))
            for origin in origins
            if origin.get("body_hash") == owner_hash and origin.get("kind") == "derives-from"
        }
        expected_temporal = {pair for pair in facts["lineage"] if pair[0] == pair[1]}
        expected_non_temporal = facts["lineage"] - expected_temporal
        owner_lineage = _score(expected_non_temporal, owner_origins)
        item = {
            "name": case["name"],
            "subject": subject,
            "expected_kind": spec["kind"],
            "actual_kind": subject_vertex.get("kind"),
            "expected_state": case["state"],
            "body_hash": owner_hash,
            "facts": {
                "object_reads": _score(facts["object_reads"], actual_objects_read),
                "column_reads": _score(facts["column_reads"], actual_columns_read),
                "object_writes": _score(facts["object_writes"], actual_objects_write),
                "column_writes": _score(facts["column_writes"], actual_columns_write),
                "read_endpoint_kinds": _score(facts["object_reads"] | facts["column_reads"], actual_read_endpoints),
                "write_endpoint_kinds": _score(facts["object_writes"] | facts["column_writes"], actual_write_endpoints),
                "lineage": {"scope": "owner-origin", **owner_lineage},
            },
        }
        analysis = _analysis_item(graph, subject)
        actual_state = analysis.get("state", "unavailable") if analysis else "unavailable"
        item["actual_state"] = actual_state
        diagnostics = {diagnostic.get("code") for diagnostic in (analysis or {}).get("diagnostics", []) if isinstance(diagnostic, dict)}
        item["diagnostics"] = sorted(diagnostics)
        provenance_errors = []
        if not analysis:
            provenance_errors.append("missing analysis record")
        elif analysis.get("body_hash") != owner_hash:
            provenance_errors.append("analysis body_hash does not match raw subject body")
        if spec["mode"] == "sql_files" and (analysis or {}).get("source") != spec["source"]:
            provenance_errors.append("analysis source does not match SQL-file subject")
        item["provenance_errors"] = provenance_errors
        item["temporal_self_lineage"] = {
            "expected": sorted(expected_temporal),
            "represented": sorted(expected_temporal & owner_origins),
            "required_diagnostic": bool(expected_temporal) and "SG_TEMPORAL_SELF_LINEAGE" in diagnostics,
        }
        mismatch = bool(kind_errors or provenance_errors)
        mismatch = mismatch or any(value["missing"] or value["unexpected"] for name, value in item["facts"].items() if name != "lineage")
        mismatch = mismatch or bool(owner_lineage["missing"] or owner_lineage["unexpected"])
        if expected_temporal and "SG_TEMPORAL_SELF_LINEAGE" not in diagnostics:
            mismatch = True
            item["temporal_self_lineage"]["error"] = "missing SG_TEMPORAL_SELF_LINEAGE diagnostic"
        if actual_state != case["state"]:
            mismatch = True
            item["state_error"] = f"expected {case['state']}, found {actual_state}"
        if kind_errors:
            item["kind_errors"] = kind_errors
            failures.extend(kind_errors)
        item["status"] = "failed" if mismatch else "passed"
        if mismatch:
            failures.append(subject)
            if actual_state == "complete":
                false_complete_cases.append(subject)
        cases.append(item)
    global_lineage = _score(all_expected_lineage, actual_global_lineage)
    origin_by_edge: dict[tuple[str, str], set[str]] = {}
    for origin in origins:
        if origin.get("kind") == "derives-from":
            origin_by_edge.setdefault((origin.get("from"), origin.get("to")), set()).add(origin.get("body_hash"))
    unattributed_lineage = sorted(edge for edge in actual_global_lineage if not origin_by_edge.get(edge))
    known_body_hashes = {record.get("body_hash") for record in records.values() if record.get("body_hash")}
    unknown_owner_origins = sorted(
        edge for edge, hashes in origin_by_edge.items()
        if edge in actual_global_lineage and not hashes & known_body_hashes
    )
    if unattributed_lineage:
        failures.append("lineage edge missing matching origin")
    if unknown_owner_origins:
        failures.append("lineage origin has unknown owner body hash")
    if global_lineage["missing"] or global_lineage["unexpected"]:
        failures.append("global DML lineage")
    return {
        "dialect": dialect,
        "phase": "analyzer-graph-scoring",
        "cases": cases,
        "ghosts": ghosts,
        "origin_ghosts": origin_ghosts,
        "failures": sorted(set(failures)),
        "false_complete_cases": sorted(false_complete_cases),
        "global_lineage": global_lineage,
        "lineage_target_scope": sorted(all_expected_outputs),
        "unattributed_lineage": unattributed_lineage,
        "unknown_owner_origins": unknown_owner_origins,
        "status": "failed" if failures else "passed",
        "analyzer_evaluated": True,
    }


def scan_document(engine: Path, document: Path, output: Path) -> None:
    """호출자가 선택한 CLI scan을 실행한다. validation-only에서는 호출하지 않는다."""

    result = subprocess.run([str(engine), "scan", "--document", str(document), "-o", str(output)], capture_output=True, text=True, timeout=120)
    if result.returncode:
        raise CorpusError(f"analyzer scan failed: {result.stderr[-2000:]}")


def main(argv: list[str] | None = None) -> int:
    """DB 검증을 실행하거나 명시적으로 기존 분석 graph를 채점한다."""

    parser = argparse.ArgumentParser(description="Validate authored DML facts before scoring a graph analyzer.")
    parser.add_argument("--cases", type=Path, default=CORPUS)
    parser.add_argument("--validate-only", action="store_true", help="execute SQLite cases without invoking an analyzer")
    parser.add_argument("--graph", type=Path, help="score a previously generated graph JSON")
    parser.add_argument("--document", type=Path, help="catalog document for an optional explicit CLI scan")
    parser.add_argument("--engine", type=Path, help="analyzer binary used only with --document")
    parser.add_argument("--postgres-bin", type=Path, default=Path("/opt/homebrew/opt/postgresql@16/bin"))
    parser.add_argument("--dialect", choices=("sqlite", "postgres", "sqlserver", "oracle"), default="sqlite")
    parser.add_argument("--case", action="append", dest="selected_cases")
    parser.add_argument("--strict", action="store_true")
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args(argv)
    try:
        corpus = load_corpus(args.cases)
        selected = set(args.selected_cases or []) or None
        if args.engine and args.graph:
            parser.error("--engine is only used when --document is scanned")
        if args.engine and not args.document:
            parser.error("--engine requires --document")
        if args.document:
            document_value = json.loads(args.document.read_text(encoding="utf-8"))
            catalog = validate_catalog_document(document_value, corpus, args.dialect)
            if catalog["failures"]:
                raise CorpusError("catalog preflight failed: " + "; ".join(catalog["failures"]))
            if args.graph:
                result = evaluate_graph(
                    json.loads(args.graph.read_text(encoding="utf-8")),
                    corpus,
                    args.dialect,
                    catalog["subject_records"],
                )
            else:
                if not args.engine:
                    parser.error("--document requires --engine unless --graph is supplied")
                generated = args.output.with_suffix(".graph.json")
                scan_document(args.engine, args.document, generated)
                result = evaluate_graph(
                    json.loads(generated.read_text(encoding="utf-8")),
                    corpus,
                    args.dialect,
                    catalog["subject_records"],
                )
            result["catalog"] = catalog
        elif args.graph:
            if args.dialect in {"postgres", "sqlserver", "oracle"}:
                parser.error("wrapper-dialect graph scoring requires --document for actual body hashes")
            result = evaluate_graph(json.loads(args.graph.read_text(encoding="utf-8")), corpus, args.dialect)
        else:
            if args.dialect == "sqlite":
                result = validate_sqlite(corpus, selected)
            elif args.dialect == "postgres":
                result = validate_postgres(corpus, selected, args.postgres_bin)
            else:
                cases = [case for case in eligible_cases(corpus, args.dialect) if not selected or case["name"] in selected]
                result = {
                    "dialect": args.dialect,
                    "phase": "native-database-validation-pending",
                    "analyzer_evaluated": False,
                    "status": "skipped",
                    "passed": 0,
                    "skipped": len(cases),
                    "failed": 0,
                    "cases": [{"name": case["name"], "status": "skipped", "reason": "native database runner is supplied by CI"} for case in cases],
                }
        result["corpus_sha256"] = _file_sha256(args.cases)
        result["case_count"] = len(corpus["cases"])
        result["case_names"] = [case["name"] for case in corpus["cases"]]
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(json.dumps(result, indent=2, sort_keys=True) + "\n", encoding="utf-8")
        failed = result.get("status") == "failed" or (args.strict and result.get("failures"))
        return 1 if failed else 0
    except (CorpusError, OSError, json.JSONDecodeError, sqlite3.Error) as error:
        report = {"status": "failed", "phase": "preflight", "error": str(error), "analyzer_evaluated": False}
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(json.dumps(report, indent=2, sort_keys=True) + "\n", encoding="utf-8")
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
