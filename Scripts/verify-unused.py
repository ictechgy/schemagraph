#!/usr/bin/env python3
"""실제 PostgreSQL 사용 통계로 unused 후보를 워크로드에서 도출한 기대값과 대조한다.

통계를 리셋한 뒤 아래 워크로드만 실행하므로 어떤 객체가 읽혔는지는 실행한
쿼리가 정한다. index-only scan은 테이블 튜플을 가져오지 않아 테이블 읽기
카운터가 0으로 남는데, 그런 테이블을 후보로 내지 않는지가 핵심 함정이다.
"""

import argparse
from contextlib import closing
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import sqlite3
import subprocess
import tempfile
from urllib.parse import urlsplit

# 임시 PostgreSQL cluster·명령 실행 도우미를 정확도 검증기와 공유한다.
SPEC = importlib.util.spec_from_file_location('accuracy', Path(__file__).with_name('verify-accuracy.py'))
ACCURACY = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(ACCURACY)

# ionly는 VACUUM으로 visibility map을 채워 index-only scan이 되게 한다.
SETUP = '''
CREATE TABLE hot (id integer PRIMARY KEY, v integer);
CREATE INDEX hot_v ON hot (v);
CREATE TABLE idle (id integer PRIMARY KEY, v integer, w integer, parent_id integer REFERENCES hot(id));
CREATE INDEX idle_v ON idle (v);
CREATE UNIQUE INDEX idle_w ON idle (w);
CREATE INDEX idle_parent ON idle (parent_id);
CREATE TABLE ionly (id integer PRIMARY KEY, v integer);
CREATE INDEX ionly_v ON ionly (v);
CREATE TABLE writeonly (id integer PRIMARY KEY);
CREATE VIEW idle_view AS SELECT v FROM idle;
INSERT INTO hot SELECT g, g FROM generate_series(1, 1000) g;
INSERT INTO ionly SELECT g, g FROM generate_series(1, 1000) g;
'''

# 리셋 이후 실행하는 유일한 사용 — hot는 seq scan, hot_v·ionly_v는 인덱스 scan,
# writeonly는 쓰기만 한다. idle과 그 인덱스는 건드리지 않는다.
WORKLOAD = '''
SET enable_seqscan = off;
SET enable_bitmapscan = off;
SELECT v FROM ionly WHERE v = 5;
SELECT id FROM hot WHERE v = 7;
RESET enable_seqscan;
SELECT count(*) FROM hot;
INSERT INTO writeonly VALUES (1), (2);
'''


def expected_candidates(fk_name):
    """워크로드에서 도출한 후보와 붙어야 할 사실이다(id → 기대 키·값)."""
    return {
        'public.idle': {'bodyDependents': 1},
        'public.writeonly': {'writes': 2},
        'public.idle.idle_v': {},
        'public.idle.idle_w': {'enforcesUniqueness': True},
        'public.idle.idle_parent': {'coversForeignKeys': [f'public.idle.{fk_name}']},
    }


def unused(engine, graph, env, expected_exit=0, strict=False):
    """unused JSON을 읽는다. strict면 종료 코드도 확인한다."""
    command = [str(engine), 'unused', '--graph', str(graph), '--max', '1000'] + (['--strict'] if strict else [])
    result = subprocess.run(command, capture_output=True, text=True, env=env, timeout=120)
    assert result.returncode == expected_exit, (result.returncode, result.stderr)
    return json.loads(result.stdout)


def check_candidates(label, report, wanted):
    """후보 집합과 각 후보의 사실이 기대와 같은지 본다."""
    actual = {candidate['id']: candidate for candidate in report['candidates']}
    assert set(actual) == set(wanted), {label: {'missing': sorted(set(wanted) - set(actual)),
                                                'unexpected': sorted(set(actual) - set(wanted))}}
    for identifier, facts in wanted.items():
        candidate = actual[identifier]
        assert candidate['usage']['reads'] == 0 and 'since' in candidate['usage'], candidate
        assert 'windowUnknown' not in candidate, candidate
        for key, value in facts.items():
            observed = candidate['usage']['writes'] if key == 'writes' else candidate.get(key)
            assert observed == value, {label: identifier, key: observed}


def workload_stats(psql, env):
    """리셋 이후 워크로드가 남긴 카운터를 카탈로그에서 직접 읽어 전제가 성립했는지 본다."""
    rows = ACCURACY.run(psql + ['-Atc', "SELECT relname, seq_tup_read + COALESCE(idx_tup_fetch, 0) FROM pg_stat_user_tables WHERE relname IN ('hot', 'ionly') ORDER BY 1"], env=env)
    tables = dict(line.split('|') for line in rows.splitlines())
    rows = ACCURACY.run(psql + ['-Atc', "SELECT indexrelname, idx_scan FROM pg_stat_user_indexes WHERE indexrelname = 'ionly_v'"], env=env)
    index = dict(line.split('|') for line in rows.splitlines())
    # ionly는 index-only scan만 받아 테이블 읽기가 0이고 인덱스 scan은 양수여야 한다.
    assert int(tables['hot']) > 0 and int(tables['ionly']) == 0 and int(index['ionly_v']) > 0, (tables, index)


def producer_graph(engine, url, work, env, go_probe, jdbc_jar, java, name):
    """Go 또는 JDBC 수집기로 같은 DB를 읽어 graph를 만든다."""
    document, graph = work / f'{name}.json', work / f'{name}.graph.json'
    if name == 'go':
        ACCURACY.run([go_probe, '--url-env', 'SG_UNUSED_URL', '--source-id', 'unused', '--schema', 'public', '-o', document],
                     env=dict(env, SG_UNUSED_URL=url))
    else:
        parts = urlsplit(url)
        ACCURACY.run([java, '-jar', jdbc_jar, '--url', f'jdbc:postgresql://{parts.hostname}:{parts.port}{parts.path}',
                      '--user', parts.username, '--source-id', 'unused', '--schema', 'public', '-o', document], env=env)
    ACCURACY.run([engine, 'scan', '--document', document, '-o', graph], env=env)
    return graph


def unscanned_primary_keys(psql, env):
    """JDBC는 PK 인덱스도 인덱스로 수집한다 — DB 통계에서 읽히지 않은 PK 인덱스를 직접 읽는다."""
    rows = ACCURACY.run(psql + ['-Atc', "SELECT i.relname, s.idx_scan FROM pg_index x JOIN pg_class i ON i.oid = x.indexrelid JOIN pg_stat_user_indexes s ON s.indexrelid = x.indexrelid JOIN pg_class t ON t.oid = x.indrelid WHERE x.indisprimary ORDER BY 1"], env=env)
    names = [name for name, scans in (line.split('|') for line in rows.splitlines()) if int(scans) == 0]
    return {f'public.{name.removesuffix("_pkey")}.{name}@index': {'enforcesUniqueness': True, 'backsConstraint': True}
            for name in names}


def check_no_counters(label, report):
    """사용 통계가 없는 경로(SQLite)는 후보 없이 미관측으로만 세야 한다."""
    assert report['candidates'] == [] and report['totalCandidates'] == 0, {label: report['candidates']}
    assert report['unobserved']['tables'] >= 1, {label: report['unobserved']}


def postgres_case(engine, bindir, work, go_probe, jdbc_jar, java):
    """임시 PG에서 워크로드를 실행하고 native·Go·JDBC의 unused를 대조한다."""
    with ACCURACY.postgres(work, bindir) as (url, psql, env):
        ACCURACY.run(psql + ['-qc', SETUP], env=env)
        ACCURACY.run(psql + ['-qc', 'VACUUM ANALYZE ionly'], env=env)
        # 통계 리셋은 슈퍼유저 함수라 관리자 역할로 실행한다(대상 DB는 같다).
        admin = list(psql)
        admin[admin.index('-U') + 1] = 'postgres'
        ACCURACY.run(admin + ['-qc', 'SELECT pg_stat_reset()'], env=env)
        # 별도 세션으로 실행해 세션 종료 시 통계가 공유 메모리로 flush되게 한다.
        ACCURACY.run(psql + ['-qc', WORKLOAD], env=env)
        workload_stats(psql, env)
        fk_name = ACCURACY.run(psql + ['-Atc', "SELECT conname FROM pg_constraint WHERE conrelid = 'idle'::regclass AND contype = 'f'"], env=env).strip()
        wanted = expected_candidates(fk_name)
        graph = work / 'native.graph.json'
        ACCURACY.run([engine, 'scan', url, '--schema', 'public', '-o', graph], env=env)
        results = {}
        report = unused(engine, graph, env, expected_exit=1, strict=True)
        check_candidates('native', report, wanted)
        results['native'] = len(report['candidates'])
        if go_probe:
            report = unused(engine, producer_graph(engine, url, work, env, go_probe, jdbc_jar, java, 'go'), env)
            check_candidates('go', report, wanted)
            results['go'] = len(report['candidates'])
        if jdbc_jar:
            # JDBC는 사용 통계와 함께 PK 인덱스도 수집하므로 읽히지 않은 PK 인덱스가 더 나온다.
            report = unused(engine, producer_graph(engine, url, work, env, go_probe, jdbc_jar, java, 'jdbc'), env)
            check_candidates('jdbc', report, wanted | unscanned_primary_keys(psql, env))
            results['jdbc'] = len(report['candidates'])
        return results


def sqlite_case(engine, work):
    """SQLite는 사용 카운터가 없으므로 후보가 없고 모든 테이블이 미관측이다."""
    database = work / 'unused.sqlite'
    with closing(sqlite3.connect(database)) as connection:
        connection.executescript('CREATE TABLE a (id INTEGER PRIMARY KEY); CREATE TABLE b (x TEXT); CREATE INDEX b_x ON b (x);')
    graph = work / 'sqlite.graph.json'
    ACCURACY.run([engine, 'scan', f'sqlite:{database}', '-o', graph])
    report = unused(engine, graph, None, strict=True)
    check_no_counters('sqlite', report)
    assert report['unobserved'] == {'tables': 2, 'indexes': 1}, report['unobserved']
    return report['unobserved']


def main():
    """PG·SQLite 사례를 실행하고 결과를 JSON으로 남긴다. 실패는 예외로 끝난다."""
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
    with tempfile.TemporaryDirectory(prefix='schemagraph-unused-') as temporary:
        work = Path(temporary)
        result = {'status': 'ok', 'engine_sha256': digest,
                  'postgres': postgres_case(engine, args.postgres_bin, work, args.go_probe, args.jdbc_jar, args.java),
                  'sqlite': sqlite_case(engine, work)}
    assert hashlib.sha256(engine.read_bytes()).hexdigest() == digest, 'engine binary changed during verification'
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(result, sort_keys=True, indent=2) + '\n')
    print(json.dumps(result, sort_keys=True))


if __name__ == '__main__':
    main()
