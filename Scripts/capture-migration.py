#!/usr/bin/env python3
"""Replay SQLite schema migrations in a new local DB and capture comparable catalogs.

Only a newly created output directory is accepted. This does not accept a live DB
URL and never applies SQL to an existing DB. Other database dialects still need
their existing isolated fixture/probe workflow.
"""
import argparse
from contextlib import closing
import hashlib
import json
from pathlib import Path
import sqlite3
import subprocess
import sys
import time

MAX_SQL_BYTES = 16 * 1024 * 1024


def read_sql(path):
    """Read bounded immutable SQL bytes once so receipt identity matches execution."""
    with Path(path).open('rb') as stream:
        data = stream.read(MAX_SQL_BYTES + 1)
    if len(data) > MAX_SQL_BYTES:
        raise ValueError('SQL input exceeds 16 MiB; narrow the fixture')
    return data.decode('utf-8-sig'), hashlib.sha256(data).hexdigest()


def apply_sql(database, sql):
    """SQLite itself executes the migration; authorizer prevents external DB access."""
    deadline = time.monotonic() + 30
    with closing(sqlite3.connect(database)) as connection:
        connection.enable_load_extension(False)
        connection.set_authorizer(lambda action, *_: sqlite3.SQLITE_DENY
                                  if action in (sqlite3.SQLITE_ATTACH, sqlite3.SQLITE_DETACH)
                                  else sqlite3.SQLITE_OK)
        connection.set_progress_handler(lambda: int(time.monotonic() > deadline), 10000)
        connection.execute('PRAGMA foreign_keys = ON')
        connection.executescript(sql)
        if connection.execute('PRAGMA foreign_key_check').fetchone() is not None:
            raise ValueError('Migration leaves foreign-key violations in the isolated fixture')
        connection.commit()


def run_engine(engine, arguments, allowed=(0,)):
    """Use argv without a shell and preserve non-success as a capture failure."""
    result = subprocess.run([str(engine), *map(str, arguments)], capture_output=True,
                            text=True, encoding='utf-8', timeout=120, check=False)
    if result.returncode not in allowed:
        raise RuntimeError(f'schemagraph {arguments[0]} failed with exit {result.returncode}: {result.stderr[:2000]}')
    return result


def capture(engine, base_schema, migrations, output, project):
    """Return replay and analysis costs plus exact input/output identities."""
    engine = Path(engine).resolve(strict=True)
    output = Path(output).resolve()
    project = Path(project).resolve(strict=True)
    sources = [read_sql(path) for path in [base_schema, *migrations]]
    output.mkdir(parents=False, exist_ok=False)
    database = output / 'isolated.sqlite'
    start = time.monotonic()
    apply_sql(database, sources[0][0])
    prepared = time.monotonic()
    for side in ('before', 'after'):
        if side == 'after':
            for sql, _ in sources[1:]:
                apply_sql(database, sql)
        # Reuse one isolated DB path so source_id stays comparable across captures.
        run_engine(engine, ['scan', 'sqlite:' + database.as_posix(), '--emit-document', output / f'{side}.catalog.json',
                            '-o', output / f'{side}.graph.json'])
        run_engine(engine, ['facts', '--document', output / f'{side}.catalog.json', '--project', project,
                            '-o', output / f'{side}.facts.json'])
    reviewed = time.monotonic()
    review = run_engine(engine, ['review', output / 'before.catalog.json', output / 'after.catalog.json', '--strict', '--require-complete'], (0, 1, 2))
    # Exit 2 may carry a valid incomplete review. Preserve its evidence and exit
    # status; command/input failures without a review must still fail capture.
    review_document = json.loads(review.stdout)
    if not isinstance(review_document, dict) or review_document.get('kind') != 'review' or not isinstance(review_document.get('complete'), bool):
        raise ValueError('Analyzer did not return a structured review')
    (output / 'review.json').write_text(review.stdout, encoding='utf-8')
    receipt = {'format': 'schemagraph-migration-capture', 'version': 1, 'dialect': 'sqlite',
               'inputs': [{'role': 'base-schema' if i == 0 else 'migration', 'sha256': digest} for i, (_, digest) in enumerate(sources)],
               'outputs': {path.name: hashlib.sha256(path.read_bytes()).hexdigest() for path in sorted(output.glob('*.json'))},
               'reviewExitCode': review.returncode, 'reviewComplete': review_document['complete'], 'complete': False,
               'seconds': {'basePreparation': prepared - start, 'captureAndMigration': reviewed - prepared,
                           'review': time.monotonic() - reviewed, 'total': time.monotonic() - start},
               'limitations': ['Isolated SQLite replay only; other DB dialects, production data and deployment locks are not assessed.',
                               'Static dependents are review candidates; empty output does not establish runtime compatibility.']}
    (output / 'capture.json').write_text(json.dumps(receipt, sort_keys=True, indent=2) + '\n', encoding='utf-8')
    return receipt


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--engine', required=True)
    parser.add_argument('--base-schema', required=True)
    parser.add_argument('--migration', required=True, action='append')
    parser.add_argument('--output', required=True, help='New directory under an existing parent')
    parser.add_argument('--project', required=True)
    args = parser.parse_args()
    receipt = capture(args.engine, args.base_schema, args.migration, args.output, args.project)
    print(json.dumps(receipt, sort_keys=True))
    return receipt['reviewExitCode']


if __name__ == '__main__':
    sys.exit(main())
