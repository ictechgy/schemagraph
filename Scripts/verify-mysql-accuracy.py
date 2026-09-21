#!/usr/bin/env python3
"""고정 Chinook SQL을 폐기용 MySQL/MariaDB에서 검증하고 독립 기대값과 비교한다."""
from __future__ import annotations

import argparse
import copy
from contextlib import contextmanager
import hashlib
import importlib.util
import json
from pathlib import Path
import re
import subprocess
import tempfile
import time
import uuid

ROOT = Path(__file__).resolve().parents[1]
CORPUS = ROOT / 'Fixtures/accuracy'
SCHEMA = 'chinook'
LABEL = 'io.github.ictechgy.schemagraph.accuracy'
SPEC = importlib.util.spec_from_file_location('accuracy_reference', ROOT / 'Scripts/verify-accuracy.py')
ACCURACY = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(ACCURACY)


def run(command, *, input=None, timeout=120, check=True):
    result = subprocess.run([str(v) for v in command], input=input, capture_output=True,
                            text=True, timeout=timeout)
    if check and result.returncode:
        raise RuntimeError(f'{Path(command[0]).name} failed: {result.stderr[-2000:]}')
    return result


def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def sql(container, client, text, *, user='root', database=True):
    command = ['docker', 'exec', '-i', container, client, '--no-defaults',
               '--batch', '--raw', '--skip-column-names', '--default-character-set=utf8mb4', '-u'+user]
    if database:
        command += ['--database', SCHEMA]
    return run(command, input=text).stdout.strip()


def inspect_owned(name, owner):
    result = run(['docker', 'container', 'inspect', name], check=False)
    if result.returncode:
        if 'No such' in result.stderr:
            return None
        raise RuntimeError(f'cannot inspect temporary container {name}: {result.stderr[-500:]}')
    record = json.loads(result.stdout)[0]
    if record.get('Config', {}).get('Labels', {}).get(LABEL) != owner:
        raise RuntimeError(f'container ownership differs for {name}; refusing to change it')
    return record


@contextmanager
def database(kind, settings):
    """기존 서비스·volume을 사용하지 않고 소유한 컨테이너만 종료한다."""
    owner = uuid.uuid4().hex
    name = f'sg-accuracy-{kind}-{owner[:12]}'
    entry = settings['engines'][kind]
    try:
        run(['docker', 'run', '--detach', '--rm', '--name', name,
             '--label', LABEL+'='+owner, '--memory', '1g', '--cpus', '1',
             '--publish', '127.0.0.1::3306',
             '--env', 'MYSQL_ALLOW_EMPTY_PASSWORD=yes', '--env', 'MYSQL_DATABASE='+SCHEMA,
             entry['image'], '--performance-schema=OFF', '--skip-log-bin',
             '--innodb-buffer-pool-size=67108864', '--character-set-server=utf8mb4',
             '--collation-server=utf8mb4_bin', '--sql-mode='+settings['sql_mode']], timeout=300)
        end = time.monotonic()+180
        while True:
            probe = run(['docker', 'exec', name, entry['client'], '--no-defaults',
                         '--protocol=tcp', '--host=127.0.0.1', '--port=3306',
                         '--database', SCHEMA, '-uroot', '--batch', '--execute', 'SELECT 1'],
                        check=False, timeout=10)
            if not probe.returncode:
                break
            record = inspect_owned(name, owner)
            if record is None or not record['State']['Running']:
                raise RuntimeError(f'temporary {kind} stopped during startup; check Docker memory availability')
            if time.monotonic() >= end:
                raise RuntimeError(f'temporary {kind} did not become ready within 180 seconds')
            time.sleep(1)
        record = inspect_owned(name, owner)
        binding = record['NetworkSettings']['Ports']['3306/tcp']
        if len(binding)!=1 or binding[0]['HostIp']!='127.0.0.1':
            raise RuntimeError('temporary database must be published only on IPv4 loopback')
        port = int(binding[0]['HostPort'])
        sql(name, entry['client'], "CREATE USER 'corpus'@'%' IDENTIFIED BY ''; GRANT SELECT, SHOW VIEW, CREATE VIEW ON chinook.* TO 'corpus'@'%';")
        version = json.loads(sql(name, entry['client'], "SELECT JSON_OBJECT('version',@@version,'sql_mode',@@sql_mode,'lower_case_table_names',@@lower_case_table_names,'character_set_database',@@character_set_database)"))
        version.update({'image_reference':entry['image'], 'image_id':record['Image']})
        yield name, entry['client'], f'mysql://corpus@127.0.0.1:{port}/{SCHEMA}', version
    finally:
        record = inspect_owned(name, owner)
        if record is not None:
            if record['State']['Running']:
                run(['docker', 'stop', '--time', '10', record['Id']])
            else:
                # 시작 도중 실패해 --rm이 작동하지 않은 소유 자원도 정리한다.
                run(['docker', 'rm', '--volumes', record['Id']])


def load_cases(path, kind):
    all_cases = json.loads(path.read_text())['cases']
    names = [case['name'] for case in all_cases]
    if not names or len(names)!=len(set(names)) or any(not re.fullmatch(r'[a-z][a-z0-9_]*',n) for n in names):
        raise RuntimeError('accuracy cases need nonempty, unique SQL-safe names')
    for case in all_cases:
        engines = case.get('engines', ['mysql','mariadb'])
        if not engines or not set(engines) <= {'mysql','mariadb'}:
            raise RuntimeError(f"invalid engine selection for {case['name']}")
    selected = [dict(case, cohort='mysql-validation') for case in all_cases
                if kind in case.get('engines', ['mysql','mariadb'])]
    if not selected:
        raise RuntimeError(f'no accuracy cases selected for {kind}')
    return selected


def validate_views(container, client, cases):
    failures = []
    for case in cases:
        try:
            sql(container, client, 'CREATE VIEW `acc_'+case['name']+'` AS '+case['sql']+'; SELECT * FROM `acc_'+case['name']+'` LIMIT 0;', user='corpus')
        except RuntimeError as error:
            failures.append({'name':case['name'],'error':str(error)})
    return failures


def table_reference(container, client, cases):
    """VIEW_TABLE_USAGE가 실제 존재할 때에만 DB의 관계 수준 참조를 대조한다."""
    available = sql(container, client, "SELECT COUNT(*) FROM information_schema.TABLES WHERE TABLE_SCHEMA='information_schema' AND TABLE_NAME='VIEW_TABLE_USAGE'") == '1'
    if not available:
        return {'available':False,'reason':'Server does not expose INFORMATION_SCHEMA.VIEW_TABLE_USAGE'}
    query = "SELECT JSON_OBJECT('view',VIEW_NAME,'table',TABLE_NAME,'schema',TABLE_SCHEMA) FROM information_schema.VIEW_TABLE_USAGE WHERE VIEW_SCHEMA='chinook' ORDER BY VIEW_NAME,TABLE_SCHEMA,TABLE_NAME"
    rows = [json.loads(line) for line in sql(container, client, query).splitlines() if line]
    reference = {SCHEMA+'.acc_'+case['name']:[] for case in cases}
    for row in rows:
        subject = SCHEMA+'.'+row['view']
        if subject in reference:
            reference[subject].append(row['schema']+'.'+row['table'])
    return {'available':True,'relations':reference}


def report_graph(graph, cases, reference):
    result = ACCURACY.corpus_report(graph, cases, SCHEMA)
    failures = ACCURACY.failing(result)
    result.pop('postgres_catalog_reads')
    result['summary'].pop('catalog_reads')
    if reference['available']:
        object_ids = {v['id'] for v in graph['vertices'] if v['kind'] in ('table','view','materialized-view')}
        scored = []
        for subject, expected in sorted(reference['relations'].items()):
            actual = {e['to'] for e in graph['edges'] if e['from']==subject and e['kind']=='reads' and e['to'] in object_ids}
            item = {'subject':subject,**ACCURACY.score(set(expected),actual)}
            scored.append(item)
            if item['missing'] or item['unexpected']:
                failures.append(subject+': DB table references')
        result['catalog_relations'] = scored
        result['summary']['catalog_relations'] = ACCURACY.totals(scored)
    result['failures'] = failures
    return result


def analyze(engine, baseline, url, work, cases, reference, sqlglot):
    catalog_path = work/'native.catalog.json'
    collected_graph = work/'collected.graph.json'
    run([engine,'scan',url,'--emit-document',catalog_path,'-o',collected_graph])
    native = json.loads(catalog_path.read_text())
    result = {}
    for mode in ('native','original'):
        document = copy.deepcopy(native)
        definitions = {'acc_'+case['name']:case['sql'] for case in cases}
        selected = []
        for schema in document['schemas']:
            if schema['name'] != SCHEMA:
                continue
            for obj in schema['objects']:
                if obj['name'] in definitions:
                    if mode == 'original':
                        obj['body'] = definitions[obj['name']]
                    selected.append(dict(next(c for c in cases if 'acc_'+c['name']==obj['name']), sql=obj['body']))
        if len(selected)!=len(cases):
            raise RuntimeError('collector omitted an accepted accuracy view')
        path = work/f'{mode}.catalog.json'
        path.write_text(json.dumps(document,sort_keys=True))
        result[mode] = {}
        for name,executable in [('current',engine),('baseline',baseline)]:
            if executable is None:
                continue
            output = work/f'{mode}-{name}.graph.json'
            run([executable,'scan','--document',path,'-o',output])
            result[mode][name] = report_graph(json.loads(output.read_text()),cases,reference)
        if sqlglot:
            result[mode]['sqlglot'] = ACCURACY.compare_sqlglot(document,selected,'mysql',SCHEMA)
    return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--engine',type=Path)
    parser.add_argument('--baseline',type=Path)
    parser.add_argument('--database',choices=('mysql','mariadb','all'),default='all')
    parser.add_argument('--cases',type=Path,default=CORPUS/'chinook-mysql-cases.json')
    parser.add_argument('--validate-only',action='store_true')
    parser.add_argument('--sqlglot',action='store_true')
    parser.add_argument('--strict',action='store_true')
    parser.add_argument('--output',type=Path,required=True)
    args = parser.parse_args()
    if not args.validate_only and args.engine is None:
        parser.error('--engine is required unless --validate-only is selected')
    manifest = ACCURACY.verify_sources()
    settings = json.loads((CORPUS/'mysql-environments.json').read_text())
    if settings['schema'] != SCHEMA:
        raise RuntimeError(f'accuracy environment must use the {SCHEMA} schema')
    report = {'report_version':1,'sources':[s for s in manifest['datasets'] if s['name']=='chinook-mysql'],
              'cases_sha256':digest(args.cases),'environments_sha256':digest(CORPUS/'mysql-environments.json'),
              'verifier_sha256':digest(Path(__file__)),
              'method':'Reviewed original and server-normalized SQL are scored separately on one current collector catalog; expected read/lineage facts are independent; VIEW_TABLE_USAGE scores relations only when available',
              'engines':{},'results':{},'validate_only':args.validate_only}
    if not args.validate_only:
        for label,executable in [('current',args.engine),('baseline',args.baseline)]:
            if executable:
                report['engines'][label] = {'version':run([executable,'--version']).stdout.strip(),'sha256':digest(executable)}
    for kind in ('mysql','mariadb'):
        if args.database not in ('all',kind):
            continue
        cases = load_cases(args.cases,kind)
        with tempfile.TemporaryDirectory(prefix='schemagraph-mysql-accuracy-') as directory:
            with database(kind,settings) as (container,client,url,version):
                sql(container,client,(CORPUS/'chinook-mysql/schema.sql').read_text())
                failures = validate_views(container,client,cases)
                reference = table_reference(container,client,cases) if not failures else None
                result = {'server':version,'case_count':len(cases),'validation_failures':failures,'table_reference':reference}
                if not failures and not args.validate_only:
                    result['analysis'] = analyze(args.engine,args.baseline,url,Path(directory),cases,reference,args.sqlglot)
                report['results'][kind] = result
        count = len(failures)
        if 'analysis' in result:
            count += sum(len(mode['current']['failures']) for mode in result['analysis'].values())
        print(f'{kind}: {len(cases)-len(failures)}/{len(cases)} database-accepted cases, {count} failing checks')
    failed = any(r['validation_failures'] or any(mode['current']['failures'] for mode in r.get('analysis',{}).values()) for r in report['results'].values())
    args.output.parent.mkdir(parents=True,exist_ok=True)
    args.output.write_text(json.dumps(report,indent=2,sort_keys=True)+'\n')
    return 1 if failed and (args.strict or args.validate_only) else 0


if __name__ == '__main__':
    raise SystemExit(main())
