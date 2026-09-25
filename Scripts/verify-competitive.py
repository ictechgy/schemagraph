#!/usr/bin/env python3
"""새 분석·변경 검토 계약을 실제 SQLite 입력으로 재검사한다."""

from __future__ import annotations

import argparse
from collections import Counter
import copy
import json
import shutil
import sqlite3
import subprocess
import sys
import tempfile
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]


SCHEMA_SQL = """
PRAGMA foreign_keys = ON;
CREATE TABLE customers (
    id INTEGER PRIMARY KEY,
    name TEXT NOT NULL
);
CREATE TABLE orders (
    id INTEGER PRIMARY KEY,
    customer_id INTEGER NOT NULL REFERENCES customers(id),
    amount INTEGER NOT NULL
);
CREATE VIEW unqualified_view AS
    SELECT id FROM orders;
CREATE VIEW wildcard_cte_view AS
    WITH q AS (SELECT * FROM orders)
    SELECT q.* FROM q;
CREATE VIEW derived_view AS
    SELECT d.id, d.amount FROM (SELECT id, amount FROM orders) d;
CREATE VIEW correlated_view AS
    SELECT o.id,
           (SELECT c.name FROM customers c WHERE c.id = o.customer_id) AS customer_name
    FROM orders o;
CREATE VIEW alias_shadow_view AS
    SELECT o.id AS order_id
    FROM orders o
    WHERE EXISTS (SELECT 1 FROM customers o WHERE o.name IS NOT NULL);
CREATE VIEW union_view AS
    SELECT id FROM orders UNION ALL SELECT id FROM customers;
CREATE VIEW using_view AS
    SELECT id, o.id AS order_id, c.id AS customer_id
    FROM orders o JOIN customers c USING (id);
CREATE VIEW constant_view AS
    SELECT 1 AS one;
"""


def fail(message: str) -> None:
    raise RuntimeError(message)


def run(engine: Path, *arguments: object, expected: int = 0) -> str:
    """자식 프로세스의 표준 오류를 검증 실패에 포함하되 입력 비밀은 받지 않는다."""
    command = [str(engine), *(str(argument) for argument in arguments)]
    try:
        result = subprocess.run(command, capture_output=True, text=True, check=False)
    except OSError as error:
        fail(f"could not start engine: {error}")
    if result.returncode != expected:
        detail = (result.stderr or result.stdout).strip()
        fail(f"{command[1]} exited {result.returncode}, expected {expected}: {detail}")
    return result.stdout


def run_capture(engine: Path, *arguments: object, expected: int = 0, input_text: str = "", cwd: Path | None = None) -> tuple[str, str]:
    """실행 결과의 stdout/stderr를 함께 돌려준다 — cache 통계와 경고를 검사할 때 쓴다."""
    command = [str(engine), *(str(argument) for argument in arguments)]
    result = subprocess.run(command, input=input_text, capture_output=True, text=True, check=False, cwd=cwd)
    if result.returncode != expected:
        detail = (result.stderr or result.stdout).strip()
        fail(f"{command[1]} exited {result.returncode}, expected {expected}: {detail}")
    return result.stdout, result.stderr


def write_db(path: Path, sql: str) -> None:
    """검증 전용 SQLite 파일을 만들고 fixture DDL을 실제 SQLite에 실행한다."""
    connection = sqlite3.connect(path)
    try:
        connection.executescript(sql)
        connection.commit()
    finally:
        connection.close()


def scan(
    engine: Path,
    database: Path,
    output: Path,
    document: Path,
    *,
    source_id: str = "app",
    schemas: tuple[str, ...] = (),
    version: int = 1,
) -> None:
    """논리 source와 선택 스키마를 표시한 document 및 graph를 함께 만든다."""
    command: list[object] = [
        "scan",
        f"sqlite:{database}",
        "--source-id",
        source_id,
        "--emit-document",
        document,
        "--document-version",
        version,
        "-o",
        output,
    ]
    if schemas:
        command.extend(("--schema", ",".join(schemas)))
    run(engine, *command)


def read_json(path: Path) -> dict:
    """JSON 산출물의 최상위 객체를 확인한다."""
    try:
        value = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as error:
        fail(f"invalid JSON in {path}: {error}")
    if not isinstance(value, dict):
        fail(f"expected an object in {path}")
    return value


def graph_edges(graph: dict) -> set[tuple[str, str, str]]:
    """간선 비교를 위해 deterministic graph의 dependency 사실만 평탄화한다."""
    return {
        (edge["from"], edge["to"], edge["kind"])
        for edge in graph.get("edges", [])
        if isinstance(edge, dict)
    }


def require_edges(graph: dict, expected: set[tuple[str, str, str]]) -> None:
    """필요한 간선이 모두 있고, 실제 정점 사이를 가리키는지 확인한다."""
    edges = graph_edges(graph)
    missing = sorted(expected - edges)
    if missing:
        fail(f"required graph edges are missing: {missing}")
    vertices = {vertex["id"] for vertex in graph.get("vertices", [])}
    ghosts = sorted((source, target) for source, target, _ in edges if source not in vertices or target not in vertices)
    if ghosts:
        fail(f"graph contains edges to missing vertices: {ghosts[:5]}")


def require_no_edges(graph: dict, forbidden: set[tuple[str, str, str]]) -> None:
    """파서가 추측하면 안 되는 계보 간선을 명시적으로 거부한다."""
    present = sorted(graph_edges(graph) & forbidden)
    if present:
        fail(f"forbidden graph edges were reported: {present}")


def analysis_for(graph: dict, object_id: str) -> dict:
    """그래프의 객체별 분석 레코드를 찾는다."""
    matches = [item for item in graph.get("analysis", []) if item.get("id") == object_id]
    if len(matches) != 1:
        fail(f"expected one analysis record for {object_id}, found {len(matches)}")
    return matches[0]


def assert_complete(graph: dict, object_ids: tuple[str, ...]) -> None:
    """정확도 fixture가 부분 분석으로 조용히 통과하지 않았는지 확인한다."""
    for object_id in object_ids:
        record = analysis_for(graph, object_id)
        if record.get("state") != "complete":
            fail(f"{object_id} is not complete: {record}")
        if record.get("diagnostics"):
            fail(f"{object_id} unexpectedly has diagnostics: {record['diagnostics']}")


def assert_diagnostic(graph: dict, object_id: str, code: str) -> None:
    """예상한 진단 코드와 원문 위치가 함께 전송되는지 확인한다."""
    record = analysis_for(graph, object_id)
    diagnostics = record.get("diagnostics", [])
    matches = [item for item in diagnostics if item.get("code") == code]
    if not matches:
        fail(f"{object_id} did not report {code}: {diagnostics}")
    location = matches[0].get("location")
    if not location or not isinstance(location.get("line"), int) or location["line"] < 1:
        fail(f"{object_id} diagnostic {code} has no source location: {matches[0]}")


def ndjson(document: dict) -> str:
    """JSON document를 독립적으로 행 단위로 조립해 transport reader를 검증한다."""
    header = {key: value for key, value in document.items() if key not in {"schemas", "limitations"}}
    records = [{"type": "document", **header, "limitations": []}]
    for schema in document.get("schemas", []):
        records.append({"type": "schema", "name": schema["name"]})
        records.extend(
            {"type": kind, "schema": schema["name"], "data": value}
            for collection, kind in (("objects", "object"), ("routines", "routine"))
            for value in schema.get(collection, [])
        )
    records.extend({"type": "dependency", "data": value} for value in document.get("dependencies", []))
    records.append({"type": "limitations", "data": document.get("limitations", [])})
    return "".join(json.dumps(record, ensure_ascii=False, sort_keys=True) + "\n" for record in records)


def check_document_versions(engine: Path, database: Path, work: Path) -> None:
    """v1/v2 JSON과 v2 NDJSON가 같은 graph를 만들고 거부 경로를 지키는지 확인한다."""
    v1_graph = work / "document-v1.graph.json"
    v1_doc = work / "document-v1.json"
    v2_graph = work / "document-v2.graph.json"
    v2_doc = work / "document-v2.json"
    scan(engine, database, v1_graph, v1_doc, version=1)
    scan(engine, database, v2_graph, v2_doc, version=2)
    baseline = read_json(v1_graph)
    if read_json(v2_graph) != baseline:
        fail("catalog document v1 and v2 changed graph semantics")

    v2_value = read_json(v2_doc)
    transport = work / "document-v2.ndjson"
    transport.write_text(ndjson(v2_value), encoding="utf-8")
    ndjson_graph = work / "document-v2-ndjson.graph.json"
    run(engine, "scan", "--document", transport, "-o", ndjson_graph)
    if read_json(ndjson_graph) != baseline:
        fail("catalog document v2 NDJSON changed graph semantics")

    invalid = copy.deepcopy(v2_value)
    invalid["required_features"] = ["unsupported-meaning-v9"]
    invalid_path = work / "unsupported-feature.json"
    invalid_path.write_text(json.dumps(invalid), encoding="utf-8")
    run(engine, "scan", "--document", invalid_path, "-o", "-", expected=2)

    incomplete = work / "incomplete-v2.ndjson"
    incomplete.write_text("\n".join(ndjson(v2_value).splitlines()[:-1]) + "\n", encoding="utf-8")
    run(engine, "scan", "--document", incomplete, "-o", "-", expected=2)


def check_graph_features(engine: Path, database: Path, work: Path) -> Path:
    """SQLite view fixture의 계보·금지 간선·분석 진단을 검사한다."""
    graph_path = work / "features.graph.json"
    document_path = work / "features.json"
    scan(engine, database, graph_path, document_path)
    graph = read_json(graph_path)
    require_edges(
        graph,
        {
            ("main.orders", "main.customers", "references"),
            ("main.unqualified_view.id", "main.orders.id", "derives-from"),
            ("main.wildcard_cte_view.id", "main.orders.id", "derives-from"),
            ("main.wildcard_cte_view.customer_id", "main.orders.customer_id", "derives-from"),
            ("main.wildcard_cte_view.amount", "main.orders.amount", "derives-from"),
            ("main.derived_view.id", "main.orders.id", "derives-from"),
            ("main.derived_view.amount", "main.orders.amount", "derives-from"),
            ("main.correlated_view.id", "main.orders.id", "derives-from"),
            ("main.correlated_view.customer_name", "main.customers.name", "derives-from"),
            ("main.alias_shadow_view.order_id", "main.orders.id", "derives-from"),
            ("main.union_view.id", "main.orders.id", "derives-from"),
            ("main.union_view.id", "main.customers.id", "derives-from"),
            ("main.using_view.id", "main.orders.id", "derives-from"),
            ("main.using_view.id", "main.customers.id", "derives-from"),
            ("main.using_view.order_id", "main.orders.id", "derives-from"),
            ("main.using_view.customer_id", "main.customers.id", "derives-from"),
        },
    )
    require_no_edges(
        graph,
        {
            ("main.correlated_view.id", "main.customers.id", "derives-from"),
            ("main.alias_shadow_view.order_id", "main.customers.id", "derives-from"),
            ("main.derived_view.amount", "main.customers.amount", "derives-from"),
            ("main.wildcard_cte_view.id", "main.customers.id", "derives-from"),
            ("main.constant_view.one", "main.orders.id", "derives-from"),
        },
    )
    assert_complete(
        graph,
        (
            "main.unqualified_view",
            "main.wildcard_cte_view",
            "main.derived_view",
            "main.correlated_view",
            "main.alias_shadow_view",
            "main.union_view",
            "main.using_view",
            "main.constant_view",
        ),
    )

    diagnostics = json.loads(run(engine, "diagnostics", "--graph", graph_path))
    if diagnostics.get("state") != "available" or diagnostics.get("summary", {}).get("total", 0) < 8:
        fail(f"graph-wide diagnostics are incomplete: {diagnostics}")

    explain = json.loads(run(engine, "explain", "main.correlated_view", "--graph", graph_path))
    if explain.get("subject", {}).get("id") != "main.correlated_view" or not explain.get("edges"):
        fail(f"explain did not return incident edges: {explain}")
    origins = [origin for edge in explain["edges"] for origin in edge.get("origins", [])]
    if not any(origin.get("body_hash", "").startswith("sha256:") and origin.get("location", {}).get("line", 0) >= 1 for origin in origins):
        fail(f"explain omitted body hash or source location: {explain}")

    path = json.loads(
        run(
            engine,
            "path",
            "main.correlated_view.customer_name",
            "main.customers.name",
            "--graph",
            graph_path,
        )
    )
    if not any(path_value[-1] == "main.customers.name" for path_value in path.get("paths", [])):
        fail(f"path did not reach main.customers.name: {path}")
    if not path.get("edges") or not all(edge.get("evidence") for edge in path["edges"]):
        fail(f"path omitted edge evidence: {path}")

    # SQLite는 DDL 작성 시 모호한 열을 거부하므로, 실제 수집 document의 몸체만
    # 모호한 SQL로 바꿔 parser의 음성 경로를 별도 transport 입력으로 확인한다.
    synthetic = read_json(document_path)
    for schema in synthetic["schemas"]:
        for obj in schema["objects"]:
            if obj["name"] == "unqualified_view":
                obj["body"] = "SELECT id FROM orders o JOIN customers c ON c.id = o.customer_id"
    synthetic_path = work / "ambiguous-document.json"
    synthetic_path.write_text(json.dumps(synthetic), encoding="utf-8")
    synthetic_graph_path = work / "ambiguous.graph.json"
    run(engine, "scan", "--document", synthetic_path, "-o", synthetic_graph_path)
    synthetic_graph = read_json(synthetic_graph_path)
    assert_diagnostic(synthetic_graph, "main.unqualified_view", "SG_COLUMN_AMBIGUOUS")
    require_no_edges(
        synthetic_graph,
        {
            ("main.unqualified_view.id", "main.orders.id", "derives-from"),
            ("main.unqualified_view.id", "main.customers.id", "derives-from"),
        },
    )
    return graph_path


def check_dead_policy(engine: Path, work: Path, feature_graph: Path) -> None:
    """보존 루트·기간 예외·절단 표시와 strict 종료 코드를 확인한다."""
    truncated = json.loads(run(engine, "dead", "--graph", feature_graph, "--max", 1, "--strict", expected=1))
    if not truncated.get("truncated") or truncated.get("totalCandidates", 0) <= 1:
        fail(f"dead strict truncation was not reported: {truncated}")

    dead_db = work / "dead.db"
    write_db(dead_db, "CREATE VIEW dead_view AS SELECT 1 AS one; CREATE VIEW dead_view_two AS SELECT 1 AS one;")
    dead_graph = work / "dead.graph.json"
    dead_doc = work / "dead.json"
    scan(engine, dead_db, dead_graph, dead_doc)

    retain = json.loads(
        run(
            engine,
            "dead",
            "--graph",
            dead_graph,
            "--retain",
            "main.dead_view,main.dead_view_two",
            "--strict",
        )
    )
    if any(candidate["id"] == "main.dead_view" for candidate in retain.get("candidates", [])):
        fail(f"retained view remained a dead candidate: {retain}")
    if not any(item["id"] == "main.dead_view" and item["reason"] == "explicit-root" for item in retain.get("retained", [])):
        fail(f"retain root was not reported: {retain}")

    policy = work / "dead-policy.toml"
    policy.write_text(
        '[[suppress]]\npattern = "main.dead_view"\nreason = "fixture is an external entry point"\nuntil = "2099-12-31"\n',
        encoding="utf-8",
    )
    suppressed = json.loads(
        run(engine, "dead", "--graph", dead_graph, "--config", policy, "--as-of", "2026-09-20", "--strict", expected=1)
    )
    candidate = next(item for item in suppressed["candidates"] if item["id"] == "main.dead_view")
    if not candidate.get("suppressed") or suppressed.get("unsuppressedCount") != 1:
        fail(f"active suppression was not applied: {suppressed}")
    expired = json.loads(
        run(engine, "dead", "--graph", dead_graph, "--config", policy, "--as-of", "2100-01-01", "--strict", expected=1)
    )
    if expired.get("unsuppressedCount", 0) < 2 or not any("expired" in note for note in expired.get("limitations", [])):
        fail(f"expired suppression was not exposed: {expired}")


def review_document(engine: Path, before: Path, after: Path, name: str, expected: int) -> dict:
    """review JSON을 반환하면서 의도한 strict 종료 코드를 확인한다."""
    output = run(engine, "review", before, after, "--strict", expected=expected)
    try:
        value = json.loads(output)
    except json.JSONDecodeError as error:
        fail(f"review output for {name} was not JSON: {error}")
    if not isinstance(value, dict):
        fail(f"review output for {name} is not an object")
    return value


def check_review(engine: Path, work: Path) -> None:
    """삭제 영향·추가 변경·범위 불일치가 서로 다른 review 결과를 내는지 확인한다."""
    before_db = work / "review-before.db"
    after_db = work / "review-after.db"
    write_db(
        before_db,
        "CREATE TABLE accounts (id INTEGER PRIMARY KEY, obsolete TEXT); "
        "CREATE VIEW legacy_report AS SELECT obsolete FROM accounts;",
    )
    write_db(after_db, "CREATE TABLE accounts (id INTEGER PRIMARY KEY);")
    before_graph = work / "review-before.graph.json"
    before_doc = work / "review-before.json"
    after_graph = work / "review-after.graph.json"
    after_doc = work / "review-after.json"
    scan(engine, before_db, before_graph, before_doc)
    scan(engine, after_db, after_graph, after_doc)
    removed = review_document(engine, before_doc, after_doc, "removed dependency", 1)
    removed_finding = next(item for item in removed["changes"] if item["id"] == "main.accounts.obsolete")
    if removed_finding["classification"] != "review-required" or removed_finding["impactSnapshot"] != "before":
        fail(f"removed column review was not based on before graph: {removed}")
    if not any(item["id"].startswith("main.legacy_report") for item in removed_finding["impacted"]):
        fail(f"removed column lost its previous dependent: {removed_finding}")

    additive_before = work / "additive-before.db"
    additive_after = work / "additive-after.db"
    write_db(additive_before, "CREATE TABLE accounts (id INTEGER PRIMARY KEY);")
    write_db(additive_after, "CREATE TABLE accounts (id INTEGER PRIMARY KEY, note TEXT);")
    additive_before_doc = work / "additive-before.json"
    additive_before_graph = work / "additive-before.graph.json"
    additive_after_doc = work / "additive-after.json"
    additive_after_graph = work / "additive-after.graph.json"
    scan(engine, additive_before, additive_before_graph, additive_before_doc)
    scan(engine, additive_after, additive_after_graph, additive_after_doc)
    additive = review_document(engine, additive_before_doc, additive_after_doc, "additive column", 1)
    note_change = next((item for item in additive["changes"] if item["id"] == "main.accounts.note"), None)
    if not note_change or note_change.get("classification") != "additive":
        fail(f"nullable column addition was not classified as additive: {additive}")

    changed_source = work / "changed-source.json"
    scan(engine, after_db, after_graph, changed_source, source_id="different-app")
    source_result = review_document(engine, after_doc, changed_source, "source identity", 2)
    if source_result.get("comparison") != "unverified":
        fail(f"source identity change was not unverified: {source_result}")

    filtered = work / "filtered.json"
    scan(engine, after_db, after_graph, filtered, schemas=("missing_schema",))
    filter_result = review_document(engine, after_doc, filtered, "schema filter", 2)
    if not any("schema collection filters differ" in note for note in filter_result.get("comparisonNotes", [])):
        fail(f"schema filter change was not reported: {filter_result}")


def check_graph_v1(engine: Path, work: Path) -> None:
    """기존 v1 graph fixture를 새 CLI가 계속 읽는지 확인한다."""
    legacy = read_json(ROOT / "Fixtures" / "sqlite" / "basic.graph.golden.json")
    legacy["version"] = 1
    for field in ("analysis", "origins", "schema_metadata"):
        legacy.pop(field, None)
    legacy["edges"] = [edge for edge in legacy["edges"] if edge["kind"] not in {"derives-from", "depends-on"}]
    fixture = work / "legacy-v1.graph.json"
    fixture.write_text(json.dumps(legacy), encoding="utf-8")
    result = json.loads(run(engine, "query", "main.orders", "--graph", fixture))
    if result.get("subject", {}).get("id") != "main.orders":
        fail(f"v1 graph fixture was not queryable: {result}")
    diagnostics = json.loads(run(engine, "diagnostics", "--graph", fixture))
    if diagnostics.get("state") != "unavailable":
        fail(f"v1 graph unexpectedly claimed analysis coverage: {diagnostics}")


def check_sql_files(engine: Path, database: Path, work: Path) -> None:
    """실제 SQLite 카탈로그에 외부 SQL query routine을 붙이는 계약을 확인한다."""
    sql_dir = work / "sql-files"
    (sql_dir / "nested").mkdir(parents=True)
    (sql_dir / "orders.sql").write_text("SELECT id FROM orders;\n", encoding="utf-8")
    (sql_dir / "nested" / "orders.sql").write_text("SELECT customer_id FROM orders;\n", encoding="utf-8")
    graph_path = work / "sql-files.graph.json"
    document_path = work / "sql-files.json"
    run(
        engine,
        "scan",
        f"sqlite:{database}",
        "--sql-dir",
        sql_dir,
        "--query-schema",
        "main",
        "--emit-document",
        document_path,
        "-o",
        graph_path,
    )
    document = read_json(document_path)
    routines = [routine for schema in document["schemas"] for routine in schema.get("routines", [])]
    sources = sorted(routine.get("source") for routine in routines if routine.get("kind") == "query")
    if sources != ["nested/orders.sql", "orders.sql"]:
        fail(f"SQL file sources were not deterministic or distinct: {sources}")
    if "external-queries-v1" not in document.get("required_features", []):
        fail(f"external query feature was not declared: {document}")
    graph = read_json(graph_path)
    query_vertices = [vertex for vertex in graph["vertices"] if vertex.get("kind") == "query"]
    if len(query_vertices) != 2:
        fail(f"external SQL queries did not become query vertices: {query_vertices}")


def check_cache(engine: Path, database: Path, work: Path) -> None:
    """cold/warm, per-body, schema namespace, and corruption cache behavior를 확인한다."""
    document = work / "cache-document.json"
    baseline = work / "cache-baseline.graph.json"
    cache_dir = work / "cache"
    scan(engine, database, baseline, document)
    _, cold_stderr = run_capture(
        engine,
        "scan",
        "--document",
        document,
        "--cache-dir",
        cache_dir,
        "-o",
        work / "cache-cold.graph.json",
    )
    _, warm_stderr = run_capture(
        engine,
        "scan",
        "--document",
        document,
        "--cache-dir",
        cache_dir,
        "-o",
        work / "cache-warm.graph.json",
    )
    if not read_json(baseline) == read_json(work / "cache-cold.graph.json") == read_json(work / "cache-warm.graph.json"):
        fail("uncached, cold and warm cache graph JSON differed")
    if "hits=" not in warm_stderr or "writes=" not in cold_stderr:
        fail(f"cache statistics were not reported: cold={cold_stderr!r}, warm={warm_stderr!r}")

    changed = copy.deepcopy(read_json(document))
    views = [obj for schema in changed["schemas"] for obj in schema["objects"] if obj["name"] in {"correlated_view", "derived_view"}]
    if len(views) != 2:
        fail("cache fixture did not contain two changeable views")
    views[0]["body"] = "SELECT id FROM orders"
    changed_path = work / "cache-one-body-changed.json"
    changed_path.write_text(json.dumps(changed), encoding="utf-8")
    _, changed_stderr = run_capture(
        engine,
        "scan",
        "--document",
        changed_path,
        "--cache-dir",
        cache_dir,
        "-o",
        work / "cache-one-body.graph.json",
    )
    if "hits=0" in changed_stderr:
        fail(f"changing one body invalidated every body: {changed_stderr}")

    schema_changed = copy.deepcopy(read_json(document))
    schema_changed["schemas"][0]["objects"].append(
        {
            "name": "cache_schema_change",
            "kind": "table",
            "columns": [],
            "constraints": [],
            "indexes": [],
            "triggers": [],
        }
    )
    schema_changed_path = work / "cache-schema-changed.json"
    schema_changed_path.write_text(json.dumps(schema_changed), encoding="utf-8")
    _, schema_stderr = run_capture(
        engine,
        "scan",
        "--document",
        schema_changed_path,
        "--cache-dir",
        cache_dir,
        "-o",
        work / "cache-schema.graph.json",
    )
    if "hits=0" in schema_stderr:
        fail(f"unrelated relation invalidated every body: {schema_stderr}")
    run_capture(engine, "scan", "--document", schema_changed_path, "-o", work / "cache-schema-uncached.graph.json")
    if read_json(work / "cache-schema.graph.json") != read_json(work / "cache-schema-uncached.graph.json"):
        fail("unrelated catalog edit produced different cached and uncached graphs")

    referenced = copy.deepcopy(read_json(document))
    orders = next(obj for schema in referenced['schemas'] for obj in schema['objects'] if obj['name'] == 'orders')
    orders['columns'].append(dict(name='new_amount', data_type='INTEGER', nullable=True,
                                 ordinal=max(column['ordinal'] for column in orders['columns'])+1, pk_position=0))
    referenced_path = work / 'cache-reference-changed.json'
    referenced_path.write_text(json.dumps(referenced), encoding='utf-8')
    _, referenced_stderr = run_capture(engine, 'scan', '--document', referenced_path, '--cache-dir', cache_dir,
                                      '-o', work/'cache-reference.graph.json')
    run_capture(engine, 'scan', '--document', referenced_path, '-o', work/'cache-reference-uncached.graph.json')
    if 'misses=0' in referenced_stderr:
        fail('a changed referenced relation unexpectedly reused every body')
    if read_json(work/'cache-reference.graph.json') != read_json(work/'cache-reference-uncached.graph.json'):
        fail('referenced catalog edit produced different cached and uncached graphs')

    entries = list(cache_dir.glob("*.json"))
    if not entries:
        fail("cache did not persist any entries")
    for entry in entries:
        entry.write_text("{corrupt", encoding="utf-8")
    _, corruption_stderr = run_capture(
        engine,
        "scan",
        "--document",
        document,
        "--cache-dir",
        cache_dir,
        "-o",
        work / "cache-corrupt.graph.json",
    )
    if "warning" not in corruption_stderr.lower():
        fail(f"corrupt cache entry did not produce a stderr warning: {corruption_stderr}")
    if read_json(work/'cache-corrupt.graph.json') != read_json(baseline):
        fail('corrupt cache fallback changed graph facts')


def check_lint(engine: Path, database: Path, work: Path) -> None:
    """FK ordered-prefix lint와 partial-index 불확실성 표시를 확인한다."""
    lint_db = work / "lint.db"
    write_db(
        lint_db,
        "CREATE TABLE customers (id INTEGER PRIMARY KEY);"
        "CREATE TABLE missing_index (id INTEGER PRIMARY KEY, customer_id INTEGER REFERENCES customers(id));"
        "CREATE TABLE partial_index (id INTEGER PRIMARY KEY, customer_id INTEGER REFERENCES customers(id));"
        "CREATE INDEX partial_customer ON partial_index(customer_id) WHERE customer_id IS NOT NULL;",
    )
    graph_path = work / "lint.graph.json"
    document_path = work / "lint.json"
    scan(engine, lint_db, graph_path, document_path)
    report = json.loads(run(engine, "lint", "--graph", graph_path))
    findings = report.get("findings", [])
    missing = next((item for item in findings if item.get("table") == "main.missing_index"), None)
    partial = next((item for item in findings if item.get("table") == "main.partial_index"), None)
    if not missing or missing.get("status") != "confirmed":
        fail(f"missing FK prefix was not confirmed: {report}")
    if not partial or partial.get("status") != "confirmed" or "partial indexes" not in partial.get("message", ""):
        fail(f"partial FK index did not preserve its caveat: {report}")


def mcp_session(engine: Path, graph_path: Path, calls: list[dict], *serve_args: object, cwd: Path | None = None) -> list[dict]:
    """한 MCP 세션에서 요청들을 보내고 initialize 이후의 응답을 id 순으로 돌려준다."""
    messages = [
        {"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "verify", "version": "1"}}},
        {"jsonrpc": "2.0", "method": "notifications/initialized"},
    ]
    messages += [dict(call, jsonrpc="2.0", id=index) for index, call in enumerate(calls, start=2)]
    requests = "\n".join(json.dumps(message) for message in messages) + "\n"
    output, _ = run_capture(engine, "serve", "--graph", graph_path, *serve_args, input_text=requests, cwd=cwd)
    responses = [json.loads(line) for line in output.splitlines() if line.strip()]
    if len(responses) != len(calls) + 1:
        fail(f"MCP handshake/notification response count was wrong: {responses}")
    return sorted(responses[1:], key=lambda response: response["id"])


def tool(tool_name: str, /, **arguments: object) -> dict:
    """tools/call 요청 본문을 만든다. 도구 인자 `name`과 겹치지 않게 위치 전용이다."""
    return {"method": "tools/call", "params": {"name": tool_name, "arguments": arguments}}


def check_mcp_parity(engine: Path, graph_path: Path) -> None:
    """MCP 도구 결과가 같은 그래프에 대한 CLI JSON과 일치하는지 확인한다."""
    cli = {
        "query": ("query", "main.orders"),
        "impact": ("impact", "main.customers"),
        "search-names": ("search", "VIEW"),
        "search-summary": ("search", "main.*", "--kind", "view", "--detail", "summary", "--max", "3"),
        "dead": ("dead",),
        "cycles": ("cycles",),
        "lint": ("lint",),
        "stats": ("stats",),
        "unused": ("unused",),
    }
    expected = {key: json.loads(run(engine, *arguments, "--graph", graph_path)) for key, arguments in cli.items()}
    responses = mcp_session(engine, graph_path, [
        tool("query", name="main.orders"),
        tool("impact", name="main.customers"),
        tool("search", pattern="VIEW"),
        tool("search", pattern="main.*", kind="view", detail="summary", max=3),
        tool("dead"),
        tool("cycles"),
        tool("lint"),
        tool("stats"),
        tool("unused"),
        {"method": "tools/list"},
    ])
    for key, response in zip(cli, responses):
        if response.get("result", {}).get("structuredContent") != expected[key]:
            fail(f"MCP {key} report differed from CLI: {response}")
    if not expected["search-names"]["matches"] or not expected["search-summary"]["truncated"]:
        fail(f"search fixture did not exercise matches and truncation: {expected['search-names']} {expected['search-summary']}")
    if not expected["dead"].get("candidates"):
        fail(f"dead fixture has no candidates to compare: {expected['dead']}")
    names = [item["name"] for item in responses[-1]["result"]["tools"]]
    if names[-6:] != ["search", "dead", "cycles", "lint", "stats", "unused"]:
        fail(f"MCP tools/list lacks the discovery tools: {names}")
    check_mcp_retention(engine, graph_path, expected["dead"])
    check_mcp_resources(engine, graph_path)


def check_mcp_retention(engine: Path, graph_path: Path, unretained: dict) -> None:
    """serve 시작 시 준 보존 루트가 MCP dead에 CLI dead --retain과 같게 적용되는지 본다."""
    root = unretained["candidates"][0]["id"]
    retained_cli = json.loads(run(engine, "dead", "--graph", graph_path, "--retain", root))
    response = mcp_session(engine, graph_path, [tool("dead")], "--retain", root)[0]
    if response.get("result", {}).get("structuredContent") != retained_cli:
        fail(f"MCP dead with serve --retain differed from CLI: {response}")
    if retained_cli == unretained:
        fail(f"retention root did not change the dead report: {retained_cli}")
    # 실행 위치의 schemagraph.toml은 serve가 읽지 않는다 — dead 전용 설정 오류가
    # 서버 시작을 막거나, 클라이언트가 고른 cwd에 따라 정책이 바뀌면 안 된다.
    workdir = graph_path.parent / "serve-cwd"
    workdir.mkdir(exist_ok=True)
    (workdir / "schemagraph.toml").write_text(
        f'retain = ["{root}"]\n[[suppress]]\npattern = "*"\nreason = "x"\nuntil = "2099-01-01"\n', encoding="utf-8")
    run(engine, "dead", "--graph", graph_path, "--config", workdir / "schemagraph.toml", expected=2)
    response = mcp_session(engine, graph_path, [tool("dead")], cwd=workdir)[0]
    if response.get("result", {}).get("structuredContent") != unretained:
        fail(f"serve read schemagraph.toml from its working directory: {response}")


def check_mcp_resources(engine: Path, graph_path: Path) -> None:
    """요약 리소스를 graph.json을 직접 센 독립 기대값과, skill 리소스를 CLI 출력과 대조한다."""
    graph = read_json(graph_path)
    vertices = Counter(vertex["kind"] for vertex in graph["vertices"])
    edges = Counter(edge["kind"] for edge in graph["edges"])
    schemas = sorted({vertex["schema"] for vertex in graph["vertices"]})
    responses = mcp_session(engine, graph_path, [
        {"method": "resources/list"},
        {"method": "resources/read", "params": {"uri": "schemagraph://graph/summary"}},
        {"method": "resources/read", "params": {"uri": "schemagraph://skill"}},
        {"method": "resources/read", "params": {"uri": "file:///etc/hosts"}},
    ])
    uris = [item["uri"] for item in responses[0]["result"]["resources"]]
    if uris != ["schemagraph://graph/summary", "schemagraph://skill"]:
        fail(f"MCP resources/list was unexpected: {uris}")
    summary = json.loads(responses[1]["result"]["contents"][0]["text"])
    if (summary["vertices"], summary["edges"], summary["schemas"]) != (dict(vertices), dict(edges), schemas):
        fail(f"MCP summary differed from graph.json counts: {summary}")
    if summary["limitations"] != graph.get("limitations", []):
        fail(f"MCP summary limitations differed from graph.json: {summary['limitations']}")
    if responses[2]["result"]["contents"][0]["text"] != run(engine, "skill"):
        fail("MCP skill resource differed from `schemagraph skill`")
    if responses[3].get("error", {}).get("code") != -32002:
        fail(f"MCP resources/read accepted an arbitrary URI: {responses[3]}")


def check_merge_dependencies(engine: Path, database: Path, work: Path) -> None:
    """유일한 외부 database 대상만 연결하고 ambiguous 대상은 limitation으로 남기는지 확인한다."""
    documents: list[Path] = []
    for source_id, database_name in (("app", "app"), ("warehouse", "warehouse")):
        graph = work / f"merge-{source_id}.graph.json"
        document_path = work / f"merge-{source_id}.json"
        scan(engine, database, graph, document_path, source_id=source_id)
        document = read_json(document_path)
        document["context"]["database"] = database_name
        document["dependencies"] = [
            {
                "source": {"schema": "main", "name": "orders", "kind": "table"},
                "target": {"schema": "main", "name": "customers", "kind": "table", "database": "warehouse"},
                "catalog": "fixture",
                "dependency_type": "by-name",
            }
        ] if source_id == "app" else []
        document_path.write_text(json.dumps(document), encoding="utf-8")
        documents.append(document_path)
    unique = work / "merge-unique.graph.json"
    run(engine, "merge", *documents, "-o", unique)
    unique_graph = read_json(unique)
    if not any(edge.get("kind") == "depends-on" for edge in unique_graph.get("edges", [])):
        fail(f"unique external dependency was not merged: {unique_graph}")

    replica = work / "merge-replica.json"
    replica_doc = read_json(documents[1])
    replica_doc["context"]["source_id"] = "replica"
    replica.write_text(json.dumps(replica_doc), encoding="utf-8")
    ambiguous = work / "merge-ambiguous.graph.json"
    run(engine, "merge", documents[0], documents[1], replica, "-o", ambiguous)
    ambiguous_graph = read_json(ambiguous)
    if any(edge.get("kind") == "depends-on" for edge in ambiguous_graph.get("edges", [])):
        fail(f"ambiguous external dependency was inferred: {ambiguous_graph}")
    if not any("ambiguous" in note.lower() for note in ambiguous_graph.get("limitations", [])):
        fail(f"ambiguous external dependency had no limitation: {ambiguous_graph}")


def check_offline_html(engine: Path, graph_path: Path, work: Path) -> None:
    """HTML 출력의 payload escaping과 inline JavaScript 구문을 확인한다."""
    payload = read_json(graph_path)
    payload["vertices"][0]["name"] = "</script><script>alert('breakout')</script>"
    malicious = work / "malicious.graph.json"
    malicious.write_text(json.dumps(payload), encoding="utf-8")
    html, _ = run_capture(engine, "graph", "--graph", malicious, "--format", "html")
    if "</script><script>alert('breakout')</script>" in html:
        fail("HTML graph payload escaped script tags incorrectly")
    if "\\u003c/script\\u003e" not in html:
        fail("HTML graph payload did not contain escaped script terminator")
    node = shutil.which("node")
    if node is None:
        fail("node is required to syntax-check offline HTML JavaScript")
    script = html.rsplit("<script>\n", 1)[-1].split("</script>", 1)[0]
    javascript = work / "offline-html.js"
    javascript.write_text(script, encoding="utf-8")
    result = subprocess.run([node, "--check", javascript], capture_output=True, text=True, check=False)
    if result.returncode != 0:
        fail(f"offline HTML JavaScript syntax failed: {result.stderr.strip()}")


def main() -> int:
    parser = argparse.ArgumentParser(description="Run competitive SQLite regression checks.")
    parser.add_argument("--engine", required=True, type=Path, help="path to an executable schemagraph binary")
    arguments = parser.parse_args()
    engine = arguments.engine.resolve()
    if not engine.is_file() or not engine.stat().st_mode & 0o111:
        print(f"error: engine is not an executable file: {engine}", file=sys.stderr)
        return 2

    try:
        with tempfile.TemporaryDirectory(prefix="schemagraph-competitive-") as directory:
            work = Path(directory)
            database = work / "features.db"
            write_db(database, SCHEMA_SQL)
            feature_graph = check_graph_features(engine, database, work)
            check_document_versions(engine, database, work)
            check_sql_files(engine, database, work)
            check_cache(engine, database, work)
            check_lint(engine, database, work)
            check_mcp_parity(engine, feature_graph)
            check_merge_dependencies(engine, database, work)
            check_offline_html(engine, feature_graph, work)
            check_dead_policy(engine, work, feature_graph)
            check_review(engine, work)
            check_graph_v1(engine, work)
    except (AssertionError, RuntimeError, StopIteration, KeyError, sqlite3.Error) as error:
        print(f"competitive regression failed: {error}", file=sys.stderr)
        return 1
    print("competitive SQLite regression: ok")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
