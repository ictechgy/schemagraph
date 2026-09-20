#!/usr/bin/env python3
"""Validate checked-in JSON schemas against real CLI catalog and graph output."""

from __future__ import annotations

import argparse
import copy
import json
import sqlite3
import subprocess
import sys
import tempfile
from pathlib import Path

try:
    from jsonschema import Draft202012Validator
except ImportError as error:  # pragma: no cover - exercised by the CI setup check
    raise SystemExit("verify-contracts.py requires pinned jsonschema; install jsonschema==4.23.0") from error


ROOT = Path(__file__).resolve().parents[1]


def fail(message: str) -> None:
    raise RuntimeError(message)


def run(engine: Path, *args: object) -> str:
    result = subprocess.run(
        [str(engine), *(str(arg) for arg in args)],
        capture_output=True,
        text=True,
        check=False,
    )
    if result.returncode != 0:
        fail(f"{args[0]} exited {result.returncode}: {(result.stderr or result.stdout).strip()}")
    return result.stdout


def load_json(path: Path) -> dict:
    try:
        value = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as error:
        fail(f"invalid JSON {path}: {error}")
    if not isinstance(value, dict):
        fail(f"expected JSON object in {path}")
    return value


def validate(schema_path: Path, value: object, label: str) -> None:
    schema = load_json(schema_path)
    errors = sorted(Draft202012Validator(schema).iter_errors(value), key=lambda error: list(error.path))
    if errors:
        details = "; ".join(f"{label} {list(error.path)}: {error.message}" for error in errors[:5])
        fail(details)


def require_invalid(schema_path: Path, value: object, label: str) -> None:
    schema = load_json(schema_path)
    if Draft202012Validator(schema).is_valid(value):
        fail(f"schema unexpectedly accepted invalid {label}")


def fixture(path: Path) -> None:
    with sqlite3.connect(path) as connection:
        connection.executescript(
            """
            PRAGMA foreign_keys = ON;
            CREATE TABLE customers (id INTEGER PRIMARY KEY, name TEXT NOT NULL);
            CREATE TABLE orders (
                id INTEGER PRIMARY KEY,
                customer_id INTEGER NOT NULL REFERENCES customers(id),
                total REAL NOT NULL
            );
            CREATE INDEX idx_orders_customer ON orders(customer_id);
            CREATE VIEW order_totals AS
                SELECT o.id AS order_id, c.name AS customer_name, o.total
                FROM orders o JOIN customers c ON c.id = o.customer_id;
            """
        )


def main() -> int:
    parser = argparse.ArgumentParser(description="Validate schemagraph JSON contracts.")
    parser.add_argument("--engine", required=True, type=Path)
    arguments = parser.parse_args()
    engine = arguments.engine.resolve()
    if not engine.is_file() or not engine.stat().st_mode & 0o111:
        print(f"error: engine is not executable: {engine}", file=sys.stderr)
        return 2

    try:
        with tempfile.TemporaryDirectory(prefix="schemagraph-contracts-") as directory:
            work = Path(directory)
            database = work / "fixture.db"
            fixture(database)
            v1_document = work / "catalog-v1.json"
            v1_graph = work / "graph-v2.json"
            run(engine, "scan", f"sqlite:{database}", "--emit-document", v1_document, "-o", v1_graph)
            validate(ROOT / "schemas/catalog-v1.schema.json", load_json(v1_document), "catalog v1")
            validate(ROOT / "schemas/graph-v2.schema.json", load_json(v1_graph), "graph v2")

            v2_document = work / "catalog-v2.json"
            v2_graph = work / "graph-v2-from-v2.json"
            run(
                engine,
                "scan",
                f"sqlite:{database}",
                "--emit-document",
                v2_document,
                "--document-version",
                2,
                "-o",
                v2_graph,
            )
            validate(ROOT / "schemas/catalog-v2.schema.json", load_json(v2_document), "catalog v2")
            graph_v2_value = load_json(v2_graph)
            validate(ROOT / "schemas/graph-v2.schema.json", graph_v2_value, "graph v2 from v2")
            if "schema_metadata" not in graph_v2_value:
                fail("graph v2 omitted schema_metadata from a catalog with metadata")
            valid_metadata = copy.deepcopy(graph_v2_value)
            valid_metadata["schema_metadata"] = {
                "columns": {"main.orders.id": {"data_type": "INTEGER", "nullable": False, "ordinal": 1}},
                "indexes": {
                    "main.orders.idx_orders_customer": {
                        "table": "main.orders",
                        "columns": ["main.orders.customer_id"],
                        "unique": False,
                        "has_predicate": False,
                        "complete": True,
                    }
                },
                "foreign_keys": {},
            }
            validate(ROOT / "schemas/graph-v2.schema.json", valid_metadata, "valid graph schema metadata")

            valid_catalog = load_json(v2_document)
            valid_catalog["required_features"].append("external-queries-v1")
            valid_catalog["schemas"][0]["routines"].append(
                {
                    "name": "query_616.sql",
                    "kind": "query",
                    "language": "sql",
                    "body": "SELECT id FROM orders",
                    "source": "queries/orders.sql",
                }
            )
            indexed_object = next(
                obj for obj in valid_catalog["schemas"][0]["objects"] if obj["indexes"]
            )
            indexed_object["indexes"][0].update(
                {"definition_complete": True, "predicate": None, "has_predicate": False}
            )
            validate(ROOT / "schemas/catalog-v2.schema.json", valid_catalog, "catalog v2 metadata/query")

            invalid_catalog = copy.deepcopy(valid_catalog)
            invalid_catalog["schemas"][0]["objects"][0]["columns"][0]["data_type"] = 17
            require_invalid(ROOT / "schemas/catalog-v2.schema.json", invalid_catalog, "catalog data_type")
            invalid_graph = copy.deepcopy(load_json(v2_graph))
            invalid_graph["origins"] = [{"from": "main.order_totals", "to": "main.orders.id", "kind": "reads", "role": "relation"}]
            require_invalid(ROOT / "schemas/graph-v2.schema.json", invalid_graph, "graph origin")
            invalid_metadata = copy.deepcopy(valid_metadata)
            invalid_metadata["schema_metadata"]["indexes"]["main.orders.idx_orders_customer"]["columns"] = "main.orders.customer_id"
            require_invalid(ROOT / "schemas/graph-v2.schema.json", invalid_metadata, "graph metadata index columns")
            invalid_metadata = copy.deepcopy(valid_metadata)
            invalid_metadata["schema_metadata"]["columns"]["main.orders.id"]["nullable"] = "false"
            require_invalid(ROOT / "schemas/graph-v2.schema.json", invalid_metadata, "graph metadata column nullable")

            records = []
            document = load_json(v2_document)
            header = {key: value for key, value in document.items() if key not in {"schemas", "limitations", "dependencies"}}
            records.append({"type": "document", **header, "limitations": []})
            for schema in document["schemas"]:
                records.append({"type": "schema", "name": schema["name"]})
                records.extend(
                    {"type": kind, "schema": schema["name"], "data": item}
                    for collection, kind in (("objects", "object"), ("routines", "routine"))
                    for item in schema.get(collection, [])
                )
            records.extend({"type": "dependency", "data": item} for item in document.get("dependencies", []))
            records.append({"type": "limitations", "data": document.get("limitations", [])})
            for index, record in enumerate(records, 1):
                validate(ROOT / "schemas/catalog-ndjson.schema.json", record, f"NDJSON line {index}")
    except (RuntimeError, sqlite3.Error) as error:
        print(f"contract validation failed: {error}", file=sys.stderr)
        return 1
    print("JSON contract validation: ok")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
