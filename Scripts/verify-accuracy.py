#!/usr/bin/env python3
"""고정 공개 스키마의 SQL 분석을 실DB 카탈로그와 수동 기대값으로 평가한다."""
from __future__ import annotations

import argparse
from collections import Counter
from contextlib import contextmanager
import hashlib
import json
import os
from pathlib import Path
import re
import socket
import sqlite3
import subprocess
import tempfile

ROOT = Path(__file__).resolve().parents[1]
CORPUS = ROOT / 'Fixtures/accuracy'


def run(command, *, env=None):
    result = subprocess.run([str(value) for value in command], capture_output=True, text=True, env=env, timeout=120)
    if result.returncode:
        raise RuntimeError(f'{Path(command[0]).name} failed: {result.stderr[-4000:]}')
    return result.stdout


def read_json(path):
    return json.loads(path.read_text())


def verify_sources():
    manifest = read_json(CORPUS/'sources.json')
    for source in manifest['datasets']:
        for path_key,hash_key in [('file','sha256'),('license_file','license_sha256')]:
            actual = hashlib.sha256((CORPUS/source[path_key]).read_bytes()).hexdigest()
            if actual != source[hash_key]:
                raise RuntimeError(f"pinned corpus checksum mismatch: {source[path_key]}")
    return manifest


def cases(name):
    path = CORPUS/f'{name}-cases.json'
    result = read_json(path)['cases']
    names = [case['name'] for case in result]
    if not names or len(names)!=len(set(names)) or any(not re.fullmatch(r'[a-z][a-z0-9_]*',name) for name in names):
        raise RuntimeError(f'{path.name} needs nonempty, unique SQL-safe case names')
    return result


@contextmanager
def postgres(work, bindir):
    """사용자 서비스 설정을 사용하지 않는 루프백 전용 임시 cluster다."""
    env = {key: value for key, value in os.environ.items() if not key.startswith('PG')}
    passwords = work/'empty-passwords'
    passwords.write_text('')
    passwords.chmod(0o600)
    env['PGPASSFILE'] = str(passwords)
    for tool in ('initdb', 'pg_ctl', 'psql'):
        if not (bindir/tool).is_file():
            raise RuntimeError(f'PostgreSQL tools are required: {bindir/tool}')
    data = work/'postgres-data'
    run([bindir/'initdb', '-D', data, '-U', 'postgres', '-A', 'trust', '--no-locale', '--encoding=UTF8'], env=env)
    with socket.socket() as candidate:
        candidate.bind(('127.0.0.1', 0))
        port = candidate.getsockname()[1]
    log = work/'postgres.log'
    # 서버 프로세스가 Python PIPE를 붙잡지 않도록 pg_ctl 출력도 파일로 보낸다.
    admin = [bindir/'psql', '-X', '-h', '127.0.0.1', '-p', str(port), '-U', 'postgres', '-d', 'postgres', '-v', 'ON_ERROR_STOP=1']
    try:
        with (work/'pg-control.log').open('w') as output:
            started = subprocess.run([str(bindir/'pg_ctl'), '-D', str(data), '-l', str(log), '-o', f'-p {port} -k /tmp -h 127.0.0.1', '-w', 'start'], stdout=output, stderr=output, env=env, timeout=60)
        if started.returncode:
            raise RuntimeError('temporary PostgreSQL failed to start: '+log.read_text()[-2000:])
        run(admin+['-qc','CREATE ROLE corpus LOGIN NOSUPERUSER NOCREATEDB NOCREATEROLE'],env=env)
        run(admin+['-qc','CREATE DATABASE corpus OWNER corpus'],env=env)
        psql = [bindir/'psql', '-X', '-h', '127.0.0.1', '-p', str(port), '-U', 'corpus', '-d', 'corpus', '-v', 'ON_ERROR_STOP=1']
        yield f'postgres://corpus@127.0.0.1:{port}/corpus', psql, env
    finally:
        if (data/'postmaster.pid').exists():
            run([bindir/'pg_ctl', '-D', data, '-m', 'fast', '-w', 'stop'], env=env)


def scan(engine, url, directory, label, baseline, authored, env=None):
    document = directory/f'{label}.catalog.json'
    initial = directory/f'{label}.initial.graph.json'
    collector = baseline or engine
    run([collector, 'scan', url, '--emit-document', document, '-o', initial],env=env)
    # DB는 USING 등을 명시적 컬럼 표현식으로 다시 출력하기도 한다. 검토 사례는
    # DB가 확인한 컬럼 모양과 원래 SQL을 조합해 두 분석기에 동일한 입력을 준다.
    catalog = read_json(document)
    bodies = {'acc_'+case['name']:case['sql'] for case in authored}
    for schema in catalog['schemas']:
        for object in schema['objects']:
            if object['name'] in bodies:
                object['body'] = bodies[object['name']]
    document.write_text(json.dumps(catalog))
    current = directory/f'{label}.current.graph.json'
    run([engine, 'scan', '--document', document, '-o', current],env=env)
    result = {'current': read_json(current)}
    if baseline:
        run([baseline,'scan','--document',document,'-o',initial],env=env)
        result['baseline'] = read_json(initial)
    return result, catalog


def score(expected, actual):
    """완전한 기대 집합을 검토한 범위에서만 정밀도·재현율을 계산한다."""
    true = expected & actual
    missing, unexpected = sorted(expected-actual), sorted(actual-expected)
    return {'expected': len(expected), 'reported': len(actual), 'true_positive': len(true),
            'precision': len(true)/len(actual) if actual else None,
            'recall': len(true)/len(expected) if expected else None,
            'missing': missing, 'unexpected': unexpected}


def totals(scores):
    expected = sum(item['expected'] for item in scores)
    reported = sum(item['reported'] for item in scores)
    true = sum(item['true_positive'] for item in scores)
    return {'expected':expected,'reported':reported,'true_positive':true,
            'false_positive':reported-true,'false_negative':expected-true,
            'precision':true/reported if reported else None,'recall':true/expected if expected else None}


def inspect_case(graph, subject, case):
    reads = {(edge['to'],) for edge in graph['edges'] if edge['from']==subject and edge['kind']=='reads'}
    lineage = {(edge['from'].removeprefix(subject+'.'), edge['to']) for edge in graph['edges'] if edge['from'].startswith(subject+'.') and edge['kind']=='derives-from'}
    expected = {(output, source) for output, sources in case['lineage'].items() for source in sources}
    analysis = next((item for item in graph.get('analysis', []) if item['id']==subject), None)
    state = analysis['state'] if analysis else 'unavailable'
    return {'subject': subject, 'tags': case['tags'], 'reads': score({(item,) for item in case['reads']}, reads),
            'lineage': score(expected, lineage), 'state': state, 'expected_state': case['state'],
            'diagnostics': analysis.get('diagnostics', []) if analysis else []}


def catalog_reference(psql, env):
    """뷰의 직접 관계/컬럼 참조는 DB가 저장한 OID 관계에서 얻는다."""
    query = """
      SELECT coalesce(json_agg(row_to_json(facts)), '[]'::json) FROM (
        SELECT DISTINCT sn.nspname||'.'||s.relname AS source,
          tn.nspname||'.'||t.relname AS relation, a.attname AS column
        FROM pg_depend d
        JOIN pg_rewrite r ON d.classid='pg_rewrite'::regclass AND d.objid=r.oid
        JOIN pg_class s ON s.oid=r.ev_class
        JOIN pg_namespace sn ON sn.oid=s.relnamespace
        JOIN pg_class t ON d.refclassid='pg_class'::regclass AND d.refobjid=t.oid
        JOIN pg_namespace tn ON tn.oid=t.relnamespace
        LEFT JOIN pg_attribute a ON a.attrelid=t.oid AND a.attnum=d.refobjsubid AND d.refobjsubid>0
        WHERE s.relkind IN ('v','m') AND t.relkind IN ('r','p','v','m','f')
          AND sn.nspname='public' AND tn.nspname='public' AND d.deptype='n'
          AND d.refobjid<>r.ev_class
        ORDER BY source, relation, a.attname
      ) facts
    """
    rows = json.loads(run(psql+['-At', '-c', query], env=env))
    result = {}
    for row in rows:
        values = result.setdefault(row['source'], set())
        values.add((row['relation'],))
        if row['column'] is not None:
            values.add((row['relation']+'.'+row['column'],))
    return result


def corpus_report(graph, authored, schema, catalog=None):
    vertices = {vertex['id'] for vertex in graph['vertices']}
    ghosts = [(edge['from'], edge['to']) for edge in graph['edges'] if edge['from'] not in vertices or edge['to'] not in vertices]
    checked = [inspect_case(graph, f'{schema}.acc_'+case['name'], case) for case in authored]
    reference = []
    if catalog is not None:
        for subject, expected in sorted(catalog.items()):
            actual = {(edge['to'],) for edge in graph['edges'] if edge['from']==subject and edge['kind']=='reads'}
            reference.append({'subject':subject, **score(expected,actual)})
    view_ids = {v['id'] for v in graph['vertices'] if v['kind'] in ('view','materialized-view')}
    view_analysis = [{'subject':a['id'],'state':a['state'],'codes':sorted({d['code'] for d in a['diagnostics']})} for a in graph.get('analysis',[]) if a['id'] in view_ids]
    false_complete = [c['subject'] for c in checked if c['state']=='complete' and any(c[k]['missing'] or c[k]['unexpected'] for k in ('reads','lineage'))]
    return {'vertices':len(vertices), 'edges':len(graph['edges']), 'ghosts':ghosts,
            'analysis_states':dict(sorted(Counter(item['state'] for item in graph.get('analysis', [])).items())),
            'authored_cases':checked, 'postgres_catalog_reads':reference,'view_analysis':view_analysis,
            'summary':{'manual_reads':totals([c['reads'] for c in checked]),
                       'manual_lineage':totals([c['lineage'] for c in checked]),
                       'catalog_reads':totals(reference),'false_complete_cases':false_complete}}


def failing(report):
    problems = []
    if report['ghosts']:
        problems.append('ghost endpoints')
    for case in report['authored_cases']:
        for kind in ('reads','lineage'):
            if case[kind]['missing'] or case[kind]['unexpected']:
                problems.append(case['subject']+': '+kind)
        if case['state'] != case['expected_state']:
            problems.append(case['subject']+': analysis state')
    for case in report['postgres_catalog_reads']:
        if case['missing'] or case['unexpected']:
            problems.append(case['subject']+': catalog reads')
    return problems


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--engine',type=Path,required=True)
    parser.add_argument('--baseline',type=Path)
    parser.add_argument('--pg-bin',type=Path,default=Path(os.environ.get('PGBIN','/opt/homebrew/opt/postgresql@16/bin')))
    parser.add_argument('--output',type=Path,required=True)
    parser.add_argument('--strict',action='store_true')
    parser.add_argument('--sqlglot',action='store_true',help='also measure the pinned optional SQLGlot development adapter')
    args = parser.parse_args()
    manifest = verify_sources()
    result = {'report_version':1,'sources':manifest['datasets'], 'results':{},'engines':{},
              'method':'DB-validated original SQL replay for reviewed cases; native definitions for upstream views; fixed catalog shared by both engine versions and SQLGlot'}
    for label,engine in [('current',args.engine),('baseline',args.baseline)]:
        if engine:
            result['engines'][label]={'version':run([engine,'--version']).strip(),'sha256':hashlib.sha256(engine.read_bytes()).hexdigest()}
    result['cases_sha256']={name:hashlib.sha256((CORPUS/f'{name}-cases.json').read_bytes()).hexdigest() for name in ('chinook','pagila')}
    with tempfile.TemporaryDirectory(prefix='schemagraph-accuracy-') as directory:
        work = Path(directory)
        sqlite_cases = cases('chinook')
        database = work/'chinook.db'
        connection = sqlite3.connect(database)
        try:
            connection.executescript((CORPUS/'chinook/schema.sql').read_text())
            for case in sqlite_cases:
                connection.execute('CREATE VIEW "acc_'+case['name']+'" AS '+case['sql'])
                connection.execute('SELECT * FROM "acc_'+case['name']+'" LIMIT 0')
            connection.commit()
        finally:
            connection.close()
        graphs, document = scan(args.engine, 'sqlite:'+str(database), work, 'chinook', args.baseline,sqlite_cases)
        result['results']['chinook'] = {name:corpus_report(graph,sqlite_cases,'main') for name,graph in graphs.items()}
        if args.sqlglot:
            result['results']['chinook']['sqlglot']=compare_sqlglot(document,sqlite_cases,'sqlite','main')
        result['sqlite_version'] = sqlite3.sqlite_version
        with postgres(work,args.pg_bin) as (url,psql,env):
            applied = work/'pagila.sql'
            applied.write_text((CORPUS/'pagila/schema.sql').read_text().replace(' OWNER TO postgres;', ' OWNER TO corpus;'))
            run(psql+['-qf',applied],env=env)
            pg_cases = cases('pagila')
            for case in pg_cases:
                run(psql+['-qc','CREATE VIEW public.acc_'+case['name']+' AS '+case['sql']],env=env)
            reference = catalog_reference(psql,env)
            result['postgres_version'] = run(psql+['-Atc','SHOW server_version'],env=env).strip()
            graphs, document = scan(args.engine,url,work,'pagila',args.baseline,pg_cases,env)
            result['results']['pagila'] = {name:corpus_report(graph,pg_cases,'public',reference) for name,graph in graphs.items()}
            if args.sqlglot:
                result['results']['pagila']['sqlglot']=compare_sqlglot(document,pg_cases,'postgres','public')
    failures = {name:failing(corpus['current']) for name,corpus in result['results'].items()}
    result['failures'] = failures
    args.output.parent.mkdir(parents=True,exist_ok=True)
    args.output.write_text(json.dumps(result,indent=2,sort_keys=True)+'\n')
    for name,corpus in result['results'].items():
        print(f"{name}: {len(corpus['current']['authored_cases'])} reviewed cases, {len(corpus['current']['postgres_catalog_reads'])} DB view definitions, {len(failures[name])} failing checks")
    return 1 if args.strict and any(failures.values()) else 0


def compare_sqlglot(document, authored, dialect, schema):
    # 상대 도구는 정답 생성기가 아니다. 같은 독립 기대값에 별도로 채점한다.
    from accuracy_sqlglot import evaluate
    result = evaluate(document,authored,dialect,schema)
    definitions = {case['name']:case for case in authored}
    for case in result['cases']:
        expected = {(output,source) for output,sources in definitions[case['name']]['lineage'].items() for source in sources}
        case['score'] = score(expected,{tuple(edge) for edge in case['lineage']})
    result['summary'] = totals([case['score'] for case in result['cases']])
    return result


if __name__=='__main__':
    raise SystemExit(main())
