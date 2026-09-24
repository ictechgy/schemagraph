#!/usr/bin/env python3
"""실제 SQLite·PostgreSQL 카탈로그의 facts(isthmus bridge-facts) 출력을 독립 기대값과 대조한다.

기대값은 엔진 출력이 아니라 각 DB의 시스템 카탈로그를 직접 읽어 만든다.
같은 scan의 그래프 정점 id와 양방향으로 대조해 유령 id가 없음을 확인하고,
`--isthmus`를 주면 실제 isthmus check가 문서를 소비한 결과까지 검사한다.
"""

import argparse
from contextlib import closing
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import re
import sqlite3
import subprocess
import tempfile

# 임시 PostgreSQL cluster·명령 실행 도우미를 정확도 검증기와 공유한다 —
# 사용자 PG 설정을 읽지 않는 같은 격리 규칙을 두 번 구현하지 않기 위해서다.
SPEC = importlib.util.spec_from_file_location('accuracy', Path(__file__).with_name('verify-accuracy.py'))
ACCURACY = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(ACCURACY)

# 계약상 선언 대상인 관계 종류다 — sequence·type·index 등은 선언이 아니다.
RELATION_KINDS = {'table', 'view', 'materialized-view'}
# 계약의 generatedAt 형식(초 단위 RFC 3339 UTC)이다. fullmatch로만 쓴다 —
# `$`는 끝의 개행을 허용해 오염된 값을 통과시킨다.
TIMESTAMP = re.compile(r'\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}Z')
# isthmus가 수신 측 공백으로 인정해 미선언 진단을 -unverified로 내리는 접두사다.
COVERAGE_PREFIX = 'catalog-coverage:'

# 이름 충돌·escape·대소문자·비관계 객체를 실제 DB에서 만든다.
# clash의 인덱스 x와 트리거 id는 컬럼 이름과 같아 정점 id 충돌을 유발한다.
SQLITE_DDL = '''
CREATE TABLE users (id INTEGER PRIMARY KEY, email TEXT NOT NULL, "Display Name" TEXT);
CREATE TABLE "a.b" (x INTEGER, "pct%col" TEXT);
CREATE TABLE "pct%rel" (y INTEGER);
CREATE TABLE "Mixed" ("Id" INTEGER);
CREATE TABLE clash (id INTEGER, x INTEGER);
CREATE INDEX x ON clash (id);
CREATE TRIGGER id AFTER INSERT ON clash BEGIN SELECT 1; END;
CREATE VIEW active_users AS SELECT id, email FROM users;
'''

# 파티션·materialized view·다른 스키마의 같은 이름·비관계 객체(sequence·type·
# 함수)와 컬럼 이름과 같은 제약·트리거를 함께 만든다.
POSTGRES_DDL = '''
CREATE SCHEMA audit;
CREATE TABLE public.users (id integer PRIMARY KEY, email text NOT NULL);
CREATE TABLE audit.users (id integer, actor text);
CREATE TABLE public."Account" ("Id" integer, "a.b" text);
CREATE TABLE public."dot.table" (v integer);
CREATE VIEW public.active_users AS SELECT id, email FROM public.users;
CREATE MATERIALIZED VIEW public.user_counts AS SELECT count(*) AS total FROM public.users;
CREATE SEQUENCE public.ticket_seq;
CREATE TYPE public.mood AS ENUM ('ok');
CREATE TABLE public.events (id integer, at date) PARTITION BY RANGE (at);
CREATE TABLE public.events_2026 PARTITION OF public.events FOR VALUES FROM ('2026-01-01') TO ('2027-01-01');
CREATE TABLE public.clash (id integer, x integer, CONSTRAINT id CHECK (id > 0));
CREATE FUNCTION public.touch() RETURNS trigger LANGUAGE plpgsql AS $$BEGIN RETURN NEW; END$$;
CREATE TRIGGER x BEFORE INSERT ON public.clash FOR EACH ROW EXECUTE FUNCTION public.touch();
CREATE DOMAIN public.positive AS integer CHECK (VALUE > 0);
CREATE DOMAIN public.required AS integer NOT NULL;
CREATE TABLE public.typed (a integer, b varchar(10), c integer[], d public.mood, e numeric(5,2), f public.positive, g timestamptz, h public.required);
CREATE MATERIALIZED VIEW public.typed_copy AS SELECT * FROM public.typed;
'''

# 독립 카탈로그 질의 — relkind r/p/f는 테이블, v는 뷰, m은 materialized view다.
POSTGRES_RELATIONS = '''
SELECT n.nspname, c.relname, a.attname
FROM pg_class c
JOIN pg_namespace n ON n.oid = c.relnamespace
LEFT JOIN pg_attribute a ON a.attrelid = c.oid AND a.attnum > 0 AND NOT a.attisdropped
WHERE c.relkind IN ('r', 'p', 'f', 'v', 'm')
  AND n.nspname NOT IN ('pg_catalog', 'information_schema') AND n.nspname NOT LIKE 'pg\\_%'
ORDER BY 1, 2, 3
'''


def escape_channel(segment):
    """계약의 channel 세그먼트 escape — `%`를 먼저 바꿔야 escape 결과와 원문이 섞이지 않는다."""
    return segment.replace('%', '%25').replace('.', '%2E')


def escape_component(name):
    """정점 id 컴포넌트 escape를 엔진과 독립적으로 재구현한다(`.%@:()`)."""
    return ''.join(f'%{ord(char):02X}' if char in '.%@:()' else char for char in name)


def expected_facts(relations):
    """(schema, relation) → 컬럼 목록에서 계약이 요구하는 선언 사실 집합을 만든다."""
    facts = set()
    for (schema, relation), columns in relations.items():
        channel = escape_channel(schema)+'.'+escape_channel(relation)
        base = escape_component(schema)+'.'+escape_component(relation)
        facts.add((channel, None, base))
        facts.update((channel, column, base+'.'+escape_component(column)) for column in columns)
    return facts


def actual_facts(document):
    """문서의 사실을 비교 가능한 (channel, method, qualifiedName) 집합으로 바꾼다."""
    return {(fact['channel'], fact.get('method'), fact['symbol']['qualifiedName']) for fact in document['facts']}


def run_facts(engine, catalog, project, output, env):
    """facts를 파일(또는 `-`면 stdout)로 실행하고 JSON 문서를 돌려준다."""
    result = ACCURACY.run([engine, 'facts', '--document', catalog, '--project', project, '-o', output], env=env)
    return json.loads(result if output == '-' else Path(output).read_text())


def check_contract(document, project, version):
    """GRAPH-EXCHANGE의 bridge-facts v1 머리 필드와 relation-decl 사실 규칙을 검사한다."""
    assert (document['format'], document['version'], document['platform']) == ('bridge-facts', 1, 'sql'), document
    assert document['tool'] == {'name': 'schemagraph', 'version': version}, document['tool']
    assert TIMESTAMP.fullmatch(document['generatedAt']), document['generatedAt']
    assert document['project'] == os.path.realpath(project), document['project']
    assert document['target'] == ('persistence' if document['facts'] else None), document['target']
    for fact in document['facts']:
        assert fact['kind'] == 'relation-decl' and fact['dynamic'] is False and 'location' not in fact, fact
    keys = [(fact['channel'], fact.get('method') or '') for fact in document['facts']]
    assert keys == sorted(keys) and len(keys) == len(set(keys)), 'facts must be sorted and unique'
    assert document['limitations'] == sorted(set(document['limitations'])), document['limitations']


def check_graph_ids(document, graph):
    """모든 qualifiedName이 같은 scan 그래프의 관계·컬럼 정점이고, 그 반대도 성립하는지 본다."""
    kinds = {vertex['id']: vertex['kind'] for vertex in graph['vertices']}
    for _, method, qualified in actual_facts(document):
        kind = kinds.get(qualified)
        valid = kind == 'column' if method else kind in RELATION_KINDS
        assert valid, f'ghost or wrong-kind id {qualified}: {kind}'
    declared = {qualified for _, _, qualified in actual_facts(document)}
    graph_ids = {vid for vid, kind in kinds.items() if kind in RELATION_KINDS | {'column'}}
    assert declared == graph_ids, {'graph_only': sorted(graph_ids-declared), 'facts_only': sorted(declared-graph_ids)}


def compare(label, document, relations):
    """독립 기대값과 정확히 같은지 확인하고 차이를 원인별로 보고한다."""
    expected, actual = expected_facts(relations), actual_facts(document)
    assert actual == expected, {label: {'missing': sorted(map(str, expected-actual)),
                                        'unexpected': sorted(map(str, actual-expected))}}
    return {'relations': len(relations), 'facts': len(actual)}


def check_outputs(engine, catalog, work, env, version):
    """파일·stdout·symlink project·반복 실행이 generatedAt 외에 같은 문서인지 본다."""
    project = work/'project'
    project.mkdir(exist_ok=True)
    alias = work/'project-link'
    if not alias.exists():
        alias.symlink_to(project, target_is_directory=True)
    first = run_facts(engine, catalog, project, work/'facts.json', env)
    runs = [run_facts(engine, catalog, alias, '-', env), run_facts(engine, catalog, project, work/'again.json', env)]
    check_contract(first, project, version)
    stable = {key: value for key, value in first.items() if key != 'generatedAt'}
    for other in runs:
        assert {key: value for key, value in other.items() if key != 'generatedAt'} == stable
    return first


def sqlite_relations(database):
    """SQLite 자체 카탈로그에서 관계와 컬럼을 읽는다 — 내부 sqlite_ 객체는 제외한다."""
    with closing(sqlite3.connect(database)) as connection:
        schema = connection.execute('PRAGMA database_list').fetchone()[1]
        names = connection.execute("SELECT name FROM sqlite_schema WHERE type IN ('table', 'view') AND name NOT LIKE 'sqlite\\_%' ESCAPE '\\'").fetchall()
        return {(schema, name): [row[1] for row in connection.execute(f'PRAGMA table_info("{name.replace(chr(34), chr(34)*2)}")')]
                for (name,) in names}


def sqlite_case(engine, work, version):
    """실제 SQLite 파일에서 scan→facts를 독립 기대값·그래프와 대조한다."""
    database = work/'facts.sqlite'
    with closing(sqlite3.connect(database)) as connection:
        connection.executescript(SQLITE_DDL)
    catalog, graph = work/'sqlite.catalog.json', work/'sqlite.graph.json'
    ACCURACY.run([engine, 'scan', f'sqlite:{database}', '--emit-document', catalog, '-o', graph])
    document = check_outputs(engine, catalog, work, None, version)
    check_graph_ids(document, json.loads(graph.read_text()))
    assert not any(note.startswith(COVERAGE_PREFIX) for note in document['limitations']), document['limitations']
    return dict(compare('sqlite', document, sqlite_relations(database)), sqlite=sqlite3.sqlite_version)


def postgres_relations(psql, env):
    """pg_catalog에서 관계·컬럼을 직접 읽는다. 컬럼 없는 관계도 관계 사실은 가진다."""
    relations = {}
    output = ACCURACY.run(psql+['-AtF', '\x1f', '-c', POSTGRES_RELATIONS], env=env)
    for line in output.splitlines():
        schema, relation, column = line.split('\x1f')
        relations.setdefault((schema, relation), [])
        if column:
            relations[(schema, relation)].append(column)
    return relations


def role_psql(psql, role, database):
    """같은 임시 cluster에 다른 역할·DB로 접속하는 psql 명령을 만든다."""
    command = list(psql)
    command[command.index('-U')+1] = role
    command[command.index('-d')+1] = database
    return command


def restricted_case(engine, url, psql, work, env, version, complete):
    """권한 없는 관계가 있는 역할의 facts가 보인 것만 선언하고 coverage 공백을 싣는지 본다."""
    admin = role_psql(psql, 'postgres', 'postgres')
    ACCURACY.run(admin+['-qc', 'CREATE ROLE collector LOGIN NOSUPERUSER NOCREATEDB NOCREATEROLE NOINHERIT'], env=env)
    ACCURACY.run(psql+['-qc', 'GRANT REFERENCES ON public.users TO collector'], env=env)
    catalog = work/'restricted.catalog.json'
    # 스키마를 한정하지 않는다 — USAGE가 없는 audit 스키마의 관계가 새지 않는지도 본다.
    ACCURACY.run([engine, 'scan', url.replace('corpus@', 'collector@'),
                  '--emit-document', catalog, '-o', work/'restricted.graph.json'], env=env)
    document = run_facts(engine, catalog, work/'project', work/'restricted.facts.json', env)
    check_contract(document, work/'project', version)
    assert json.loads(catalog.read_text())['context']['catalog_complete'] is False
    assert any(note.startswith(COVERAGE_PREFIX) for note in document['limitations']), document['limitations']
    # pg_matviews는 권한으로 걸러지지 않아 MV 관계는 보인다 — 실제로 존재하는
    # 관계라 선언은 참이다. 권한으로 숨은 테이블과 MV 컬럼은 선언하지 않아야 한다.
    facts, known = actual_facts(document), actual_facts(complete)
    relations = {channel for channel, method, _ in facts if method is None}
    assert facts <= known and 'public.users' in relations, sorted(map(str, facts-known))
    assert not relations & {'public.clash', 'public.typed', 'audit.users'}, relations
    assert {(channel, method) for channel, method, _ in facts if method} == {('public.users', 'id'), ('public.users', 'email')}, facts
    return document


def missing_schema_case(engine, url, work, env, version):
    """없는 스키마를 읽은 문서는 사실 없이 target null과 coverage 공백을 싣는다."""
    catalog = work/'missing.catalog.json'
    ACCURACY.run([engine, 'scan', url, '--schema', 'absent', '--emit-document', catalog,
                  '-o', work/'missing.graph.json'], env=env)
    document = run_facts(engine, catalog, work/'project', work/'missing.facts.json', env)
    check_contract(document, work/'project', version)
    assert document['facts'] == [] and document['target'] is None, document
    assert any(note.startswith(COVERAGE_PREFIX) for note in document['limitations']), document['limitations']


def check_matview_columns(catalog):
    """MV 컬럼 표기가 같은 컬럼을 가진 원본 테이블의 information_schema 표기와 같은지 본다.

    MV는 information_schema.columns에 없어 수집기가 pg_attribute에서 따로 읽는다.
    원본 테이블 쪽은 information_schema가 직접 준 값이라 독립 기준이 된다.
    """
    objects = {obj['name']: obj for schema in catalog['schemas'] if schema['name'] == 'public' for obj in schema['objects']}
    shape = lambda name: [(c['name'], c['data_type'], c['nullable'], c['ordinal']) for c in objects[name]['columns']]
    assert objects['typed_copy']['kind'] == 'materialized-view', objects['typed_copy']['kind']
    assert shape('typed_copy') == shape('typed') and len(shape('typed')) == 8, (shape('typed_copy'), shape('typed'))
    # NOT NULL 도메인은 MV에서도 nullable이 아니다 — 도메인 분기가 실제로 쓰였는지 확인한다.
    assert ('h', 'integer', False, 8) in shape('typed_copy'), shape('typed_copy')
    assert catalog['context']['catalog_complete'] is True, catalog['limitations']


def postgres_case(engine, bindir, work, version):
    """임시 PostgreSQL cluster에서 완전·제한·빈 카탈로그의 facts를 모두 검사한다."""
    with ACCURACY.postgres(work, bindir) as (url, psql, env):
        ACCURACY.run(psql+['-qc', POSTGRES_DDL], env=env)
        catalog, graph = work/'postgres.catalog.json', work/'postgres.graph.json'
        ACCURACY.run([engine, 'scan', url, '--emit-document', catalog, '-o', graph], env=env)
        check_matview_columns(json.loads(catalog.read_text()))
        document = check_outputs(engine, catalog, work, env, version)
        check_graph_ids(document, json.loads(graph.read_text()))
        assert not any(note.startswith(COVERAGE_PREFIX) for note in document['limitations']), document['limitations']
        summary = compare('postgres', document, postgres_relations(psql, env))
        restricted = restricted_case(engine, url, psql, work, env, version, document)
        missing_schema_case(engine, url, work, env, version)
        server = ACCURACY.run(psql+['-Atqc', 'SHOW server_version'], env=env).strip()
        return dict(summary, server_version=server), document, restricted


# 호출 측(Go 생산자 모양) 사용 사실과 isthmus가 내야 할 판정이다. 기대 판정은
# GRAPH-EXCHANGE persistence 조인 규칙에서 손으로 도출했다.
CALLER_USES = [
    ('public.users', None),          # 한정 사용 → match
    ('public.users', 'email'),       # 선언된 컬럼 → match
    ('public.users', 'nickname'),    # 선언 없는 컬럼 → column-use-without-decl
    ('users', None),                 # public.users·audit.users 둘 다 후보 → ambiguous
    ('account', None),               # "Account" 선언과 소문자 접기로 match
    ('public.dot%2Etable', None),    # 이름 안의 점은 escape된 채널로 match
    ('public.user_counts', 'total'), # MV 컬럼 → match (관계 사용은 함축하지 않는다)
    ('missing_table', None),         # 선언 없음 → relation-use-without-decl
]


def caller_document(project):
    """isthmus 계약의 relation-use 문서다 — 사용 측은 location이 필수다."""
    facts = [{'kind': 'relation-use', 'channel': channel, 'dynamic': False,
              'location': {'path': 'db/access.go', 'line': line, 'column': 1},
              **({'method': method} if method else {})}
             for line, (channel, method) in enumerate(CALLER_USES, start=1)]
    return {'format': 'bridge-facts', 'version': 1, 'tool': {'name': 'verify-bridge-facts', 'version': '1'},
            'generatedAt': '2026-09-24T00:00:00Z', 'platform': 'go', 'target': 'persistence',
            'project': os.path.realpath(project), 'facts': facts, 'limitations': []}


def isthmus_check(node, isthmus, work, label, facts_document):
    """실제 isthmus check에 호출 측 문서와 facts 문서를 넣고 이슈 목록을 돌려준다."""
    caller, facts = work/f'{label}.caller.json', work/f'{label}.facts.json'
    caller.write_text(json.dumps(caller_document(work/'project')))
    facts.write_text(json.dumps(facts_document))
    result = subprocess.run([str(node), str(isthmus), 'check', '--format', 'json', str(caller), str(facts)],
                            capture_output=True, text=True, timeout=120)
    if result.returncode not in (0, 1):
        raise RuntimeError(f'isthmus check failed ({result.returncode}): {result.stderr[-2000:]}')
    report = json.loads(result.stdout)
    return {(issue['code'], issue['channel'], issue.get('method')): issue for issue in report['issues']}


def isthmus_case(node, isthmus, work, complete, restricted):
    """완전한 카탈로그는 error로, coverage 공백이 있는 카탈로그는 -unverified 경고로 내려가야 한다."""
    issues = isthmus_check(node, isthmus, work, 'complete', complete)
    errors = {key for key, issue in issues.items() if issue['severity'] == 'error'}
    assert errors == {('relation-use-without-decl', 'missing_table', None),
                      ('column-use-without-decl', 'public.users', 'nickname')}, sorted(map(str, errors))
    ambiguous = issues[('ambiguous-relation-use', 'users', None)]
    assert sorted(ambiguous['candidates']) == ['audit.users', 'public.users'], ambiguous
    # 계약상 컬럼 사용은 관계 사용을 함축하지 않으므로 user_counts는 미사용 선언이다.
    unused = {channel for code, channel, _ in issues if code == 'relation-decl-without-use'}
    assert 'public.user_counts' in unused, unused
    assert not unused & {'public.users', 'public.Account', 'public.dot%2Etable'}, unused
    degraded = isthmus_check(node, isthmus, work, 'restricted', restricted)
    assert not any(issue['severity'] == 'error' for issue in degraded.values()), sorted(map(str, degraded))
    assert ('relation-use-without-decl-unverified', 'missing_table', None) in degraded, sorted(map(str, degraded))
    return {'complete_issues': len(issues), 'restricted_issues': len(degraded), 'errors': len(errors)}


def engine_version(engine):
    """facts의 tool.version과 대조할 엔진 자신의 버전 문자열을 읽는다."""
    return ACCURACY.run([engine, '--version']).split()[-1]


def main():
    """DB 사례를 실행하고 결과를 JSON으로 남긴다.

    isthmus의 persistence 조인은 아직 npm 배포본에 없어 --isthmus가 없으면 소비
    검사를 건너뛴다. 그때 status는 ok가 아니라 partial이다 — 건너뛴 검사를
    통과로 읽히게 두지 않는다. 실패는 예외로 끝나 종료 코드가 0이 아니다.
    """
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--engine', type=Path, required=True)
    parser.add_argument('--postgres-bin', type=Path, default=Path(os.environ.get('PGBIN', '/opt/homebrew/opt/postgresql@16/bin')))
    parser.add_argument('--isthmus', type=Path, help='isthmus dist/cli/main.js with persistence support (optional)')
    parser.add_argument('--node', type=Path, default=Path('node'))
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    engine = args.engine.resolve()
    digest = hashlib.sha256(engine.read_bytes()).hexdigest()
    version = engine_version(engine)
    with tempfile.TemporaryDirectory(prefix='schemagraph-facts-') as temporary:
        work = Path(temporary)
        result = {'status': 'ok', 'engine_sha256': digest, 'engine_version': version,
                  'verifier_sha256': hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),
                  'sqlite': sqlite_case(engine, work, version)}
        result['postgres'], complete, restricted = postgres_case(engine, args.postgres_bin, work, version)
        if args.isthmus:
            result['isthmus'] = isthmus_case(args.node, args.isthmus.resolve(), work, complete, restricted)
        else:
            result['status'] = 'partial'
            result['isthmus'] = 'skipped: --isthmus not given (persistence join is unreleased in isthmus 0.9.0)'
    assert hashlib.sha256(engine.read_bytes()).hexdigest() == digest, 'engine binary changed during verification'
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(result, sort_keys=True, indent=2)+'\n')
    print(json.dumps(result, sort_keys=True))


if __name__ == '__main__':
    main()
