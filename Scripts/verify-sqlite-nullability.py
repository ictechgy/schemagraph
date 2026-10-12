#!/usr/bin/env python3
"""SQLite 저장 의미론·세 수집기·nullable 변경 검토를 같은 임시 DB로 대조한다."""
import argparse
import json
from pathlib import Path
import sqlite3
import subprocess
import tempfile

ROOT = Path(__file__).resolve().parents[1]


def run(command, expected=0):
    result = subprocess.run([str(part) for part in command], capture_output=True, text=True)
    assert result.returncode == expected, (command, result.returncode, result.stderr)
    return result.stdout


def collect(args, directory, database, producer, label):
    document = directory / f"{producer}-{label}.json"
    if producer == "native":
        run([args.engine, "scan", f"sqlite:{database}", "--emit-document", document,
             "-o", directory / f"{producer}-{label}.graph.json"])
    elif producer == "go":
        run([args.go_probe, "--url", f"sqlite:{database}", "-o", document])
    else:
        run([args.java, "-jar", args.jdbc_jar, "--url", f"jdbc:sqlite:{database}", "-o", document])
    return document, json.loads(document.read_text())


def verify(args):
    producers = ["native"] + (["go"] if args.go_probe else []) + (["jdbc"] if args.jdbc_jar else [])
    with tempfile.TemporaryDirectory(prefix="schemagraph-nullability-") as temporary:
        directory = Path(temporary)
        database = directory / "catalog.db"
        with sqlite3.connect(database) as connection:
            connection.executescript((ROOT / "Fixtures/sqlite-nullability.sql").read_text())
            tables = [row[0] for row in connection.execute("SELECT name FROM sqlite_master WHERE type='table'")]
            assert len(tables) == 9
            # 기대값을 수집기에서 가져오지 않고 실제 INSERT와 저장된 값으로 검증한다.
            for table in tables:
                columns = list(connection.execute(f'PRAGMA table_xinfo("{table}")'))
                values = ", ".join("NULL" for _ in columns)
                try:
                    connection.execute(f'INSERT INTO "{table}" VALUES ({values})')
                except sqlite3.IntegrityError:
                    assert table.startswith("required_")
                else:
                    row = connection.execute(f'SELECT * FROM "{table}"').fetchone()
                    assert all(value is None for value in row) == table.startswith("nullable_"), (table, row)
        expected = None
        for producer in producers:
            _, document = collect(args, directory, database, producer, "fixture")
            actual = {f'{obj["name"]}.{col["name"]}': col["nullable"]
                      for schema in document["schemas"] for obj in schema["objects"] for col in obj["columns"]}
            assert len(actual) == 11, actual
            for column, nullable in actual.items():
                assert nullable == column.startswith("nullable_"), (producer, column, nullable)
            if expected is None:
                expected = actual
            assert actual == expected, producer

        before = directory / "before.db"
        after = directory / "after.db"
        for path, suffix in [(before, ""), (after, " NOT NULL")]:
            with sqlite3.connect(path) as connection:
                connection.execute(f"CREATE TABLE accounts(id TEXT PRIMARY KEY{suffix})")
        for producer in producers:
            old_path, old = collect(args, directory, before, producer, "before")
            new_path, new = collect(args, directory, after, producer, "after")
            # 파일별 DB 식별자 차이는 제거해 동일한 DB의 migration 스냅샷으로 비교한다.
            for path, doc in [(old_path, old), (new_path, new)]:
                doc["context"] = {"source_id": "nullability-fixture", "database": "main", "catalog_complete": True}
                path.write_text(json.dumps(doc))
            for left, right, old_value, new_value in [(old_path, new_path, "true", "false"), (new_path, old_path, "false", "true")]:
                review = json.loads(run([args.engine, "review", left, right, "--strict", "--format", "json"], expected=1))
                findings = [change for change in review["changes"] if change["change"] == "column-nullability-changed"]
                assert len(findings) == 1, (producer, review)
                assert (findings[0]["before"], findings[0]["after"]) == (old_value, new_value)
                assert findings[0]["classification"] == "review-required"
        print(f"SQLite nullability: 9 table shapes, 11 columns, {len(producers)} collectors, bidirectional review verified")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--engine", type=Path, required=True)
    parser.add_argument("--go-probe", type=Path)
    parser.add_argument("--jdbc-jar", type=Path)
    parser.add_argument("--java", default="java")
    verify(parser.parse_args())
