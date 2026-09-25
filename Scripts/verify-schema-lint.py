#!/usr/bin/env python3
"""실제 SQLite·PostgreSQL 카탈로그에서 lint 규칙을 DDL로 도출한 기대값과 대조한다.

기대 finding은 엔진 출력이 아니라 아래 DDL에서 손으로 도출했다. 같은 DB를
Go·JDBC 수집기로 읽은 경우에도 같은 판정이 나오는지 선택적으로 확인한다.
"""

import argparse
from contextlib import closing
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import sqlite3
import tempfile
from urllib.parse import urlsplit

# 임시 PostgreSQL cluster·명령 실행 도우미를 정확도 검증기와 공유한다.
SPEC = importlib.util.spec_from_file_location('accuracy', Path(__file__).with_name('verify-accuracy.py'))
ACCURACY = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(ACCURACY)

# 새 규칙과, 같은 DDL에서 함께 생기는 기존 FK 인덱스 규칙만 비교한다.
RULES = {'fk-type-mismatch', 'table-without-primary-key', 'duplicate-index', 'fk-index-prefix'}

# child.parent_id는 참조 대상과 선언 타입이 다르고 같은 키의 인덱스가 둘이다.
# same_type은 타입이 같지만 FK 인덱스가 없고, nopk는 PK가 없다.
SQLITE_DDL = '''
CREATE TABLE parent (id INTEGER PRIMARY KEY);
CREATE TABLE child (id INTEGER PRIMARY KEY, parent_id BIGINT REFERENCES parent(id));
CREATE TABLE same_type (id INTEGER PRIMARY KEY, parent_id INTEGER REFERENCES parent(id));
CREATE TABLE nopk (x TEXT);
CREATE VIEW nopk_view AS SELECT x FROM nopk;
CREATE INDEX child_a ON child (parent_id);
CREATE INDEX child_b ON child (parent_id);
CREATE UNIQUE INDEX child_unique ON child (parent_id, id);
CREATE INDEX child_partial ON child (parent_id) WHERE parent_id > 0;
'''

# PG는 child_hash가 같은 키의 hash 인덱스다 — 방식이 달라도 수집 사실로는 구분되지
# 않으므로 duplicate-index가 확정이 아니라 미확인이어야 하는 실제 사례다.
POSTGRES_DDL = '''
CREATE TABLE parent (id integer PRIMARY KEY);
CREATE TABLE child (id integer PRIMARY KEY, parent_id bigint REFERENCES parent(id));
CREATE TABLE same_type (id integer PRIMARY KEY, parent_id integer REFERENCES parent(id));
CREATE TABLE nopk (x text);
CREATE VIEW nopk_view AS SELECT x FROM nopk;
CREATE INDEX child_a ON child (parent_id);
CREATE INDEX child_hash ON child USING hash (parent_id);
CREATE INDEX child_partial ON child (parent_id) WHERE parent_id > 0;
'''


def expected(schema, fk_names, duplicate):
    """DDL에서 도출한 (rule, subject, status) 집합이다. FK 제약 이름은 방언마다 달라 인자로 받는다."""
    child_fk, same_fk = fk_names
    return {
        ('fk-type-mismatch', f'{schema}.child.{child_fk}', 'confirmed'),
        ('fk-index-prefix', f'{schema}.same_type.{same_fk}', 'confirmed'),
        ('table-without-primary-key', f'{schema}.nopk', 'confirmed'),
        ('duplicate-index', f'{schema}.child.{duplicate}', 'unverified'),
    }


def lint(engine, graph, env=None):
    """lint JSON에서 비교 대상 규칙의 finding만 고른다."""
    report = json.loads(ACCURACY.run([engine, 'lint', '--graph', graph, '--max', '1000'], env=env))
    return report, {(f['rule'], f['subject'], f['status']) for f in report['findings'] if f['rule'] in RULES}


def check(label, actual, wanted):
    """차이를 누락·예상 밖으로 나눠 보고한다."""
    if actual != wanted:
        raise AssertionError({label: {'missing': sorted(wanted - actual), 'unexpected': sorted(actual - wanted)}})


def sqlite_fk_names(database):
    """SQLite는 FK에 이름이 없어 reader가 id 순서로 이름을 붙인다 — 그 규칙을 따른다."""
    return ('child_fk_0', 'same_type_fk_0')


def sqlite_case(engine, work):
    """SQLite 파일을 scan해 새 규칙의 확정·미확인 판정을 확인한다."""
    database = work / 'lint.sqlite'
    with closing(sqlite3.connect(database)) as connection:
        connection.executescript(SQLITE_DDL)
    graph = work / 'sqlite.graph.json'
    ACCURACY.run([engine, 'scan', f'sqlite:{database}', '-o', graph])
    report, actual = lint(engine, graph)
    check('sqlite', actual, expected('main', sqlite_fk_names(database), 'child_b'))
    message = next(f['message'] for f in report['findings'] if f['rule'] == 'fk-type-mismatch')
    assert 'BIGINT' in message and 'INTEGER' in message, message
    return {'findings': len(actual), 'complete': report['complete']}


def postgres_fk_names(psql, env):
    """PG가 실제로 붙인 FK 제약 이름을 카탈로그에서 읽는다."""
    output = ACCURACY.run(psql + ['-Atc', "SELECT conrelid::regclass::text, conname FROM pg_constraint WHERE contype = 'f' ORDER BY 1"], env=env)
    names = dict(line.split('|') for line in output.splitlines())
    return (names['child'], names['same_type'])


def producer_graphs(engine, url, work, env, go_probe, jdbc_jar, java):
    """같은 DB를 Go·JDBC 수집기로 읽어 graph를 만든다(주어진 수집기만)."""
    graphs = {}
    if go_probe:
        document = work / 'go.json'
        ACCURACY.run([go_probe, '--url-env', 'SG_LINT_URL', '--source-id', 'lint', '--schema', 'public', '-o', document],
                     env=dict(env, SG_LINT_URL=url))
        graphs['go'] = document
    if jdbc_jar:
        parts, document = urlsplit(url), work / 'jdbc.json'
        ACCURACY.run([java, '-jar', jdbc_jar, '--url', f'jdbc:postgresql://{parts.hostname}:{parts.port}{parts.path}',
                      '--user', parts.username, '--source-id', 'lint', '--schema', 'public', '-o', document], env=env)
        graphs['jdbc'] = document
    for name, document in list(graphs.items()):
        graph = work / f'{name}.graph.json'
        ACCURACY.run([engine, 'scan', '--document', document, '-o', graph], env=env)
        graphs[name] = graph
    return graphs


def restricted_case(engine, url, psql, work, env):
    """일부 관계만 보이는 역할의 수집은 불완전하므로 PK 부재가 미확인이어야 한다."""
    admin = list(psql)
    admin[admin.index('-U') + 1], admin[admin.index('-d') + 1] = 'postgres', 'postgres'
    ACCURACY.run(admin + ['-qc', 'CREATE ROLE collector LOGIN NOSUPERUSER NOCREATEDB NOCREATEROLE NOINHERIT'], env=env)
    ACCURACY.run(psql + ['-qc', 'GRANT REFERENCES ON public.nopk TO collector'], env=env)
    graph = work / 'restricted.graph.json'
    ACCURACY.run([engine, 'scan', url.replace('corpus@', 'collector@'), '--schema', 'public', '-o', graph], env=env)
    report, actual = lint(engine, graph, env)
    assert ('table-without-primary-key', 'public.nopk', 'unverified') in actual, actual
    assert report['complete'] is False, report


def postgres_case(engine, bindir, work, go_probe, jdbc_jar, java):
    """임시 PG에서 native와 선택한 수집기의 판정이 DDL 기대값과 같은지 본다."""
    with ACCURACY.postgres(work, bindir) as (url, psql, env):
        ACCURACY.run(psql + ['-qc', POSTGRES_DDL], env=env)
        wanted = expected('public', postgres_fk_names(psql, env), 'child_hash')
        graphs = {'native': work / 'native.graph.json'}
        ACCURACY.run([engine, 'scan', url, '--schema', 'public', '-o', graphs['native']], env=env)
        graphs.update(producer_graphs(engine, url, work, env, go_probe, jdbc_jar, java))
        results = {}
        for name, graph in graphs.items():
            report, actual = lint(engine, graph, env)
            check(f'postgres-{name}', actual, wanted)
            duplicate = next(f for f in report['findings'] if f['rule'] == 'duplicate-index')
            assert 'access method' in duplicate['message'], duplicate
            results[name] = len(actual)
        restricted_case(engine, url, psql, work, env)
        return results


def main():
    """SQLite·PG 사례를 실행하고 결과를 JSON으로 남긴다. 실패는 예외로 끝난다."""
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--engine', type=Path, required=True)
    parser.add_argument('--postgres-bin', type=Path, default=Path(os.environ.get('PGBIN', '/opt/homebrew/opt/postgresql@16/bin')))
    parser.add_argument('--go-probe', type=Path)
    parser.add_argument('--jdbc-jar', type=Path)
    parser.add_argument('--java', type=Path, default=Path('java'))
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    engine = args.engine.resolve()
    digest = hashlib.sha256(engine.read_bytes()).hexdigest()
    with tempfile.TemporaryDirectory(prefix='schemagraph-lint-') as temporary:
        work = Path(temporary)
        result = {'status': 'ok', 'engine_sha256': digest, 'sqlite': sqlite_case(engine, work),
                  'postgres': postgres_case(engine, args.postgres_bin, work, args.go_probe, args.jdbc_jar, args.java)}
    assert hashlib.sha256(engine.read_bytes()).hexdigest() == digest, 'engine binary changed during verification'
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(result, sort_keys=True, indent=2) + '\n')
    print(json.dumps(result, sort_keys=True))


if __name__ == '__main__':
    main()
