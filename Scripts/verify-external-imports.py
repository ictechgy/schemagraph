#!/usr/bin/env python3
"""실제 dbt compile 산출물과 실행한 쿼리를 import해 그래프·관측 계약을 검사한다."""

import argparse
from datetime import datetime, timezone
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import subprocess
import tempfile
from urllib.parse import urlsplit

SPEC = importlib.util.spec_from_file_location('accuracy', Path(__file__).with_name('verify-accuracy.py'))
ACCURACY = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(ACCURACY)


def timestamp():
    return datetime.now(timezone.utc).strftime('%Y-%m-%dT%H:%M:%SZ')


def run(command, *, cwd, env, code=0):
    result = subprocess.run([str(arg) for arg in command], cwd=cwd, env=env,
                            capture_output=True, text=True, timeout=120)
    if result.returncode != code:
        raise RuntimeError(f'{Path(command[0]).name}: expected {code}, got {result.returncode}: {result.stderr[-2000:]} {result.stdout[-2000:]}')
    return result


def verify(engine, dbt, bindir):
    """사용자 dbt profiles를 읽지 않는 폐기용 PostgreSQL project로 검증한다."""
    with tempfile.TemporaryDirectory(prefix='schemagraph-import-') as temporary:
        work = Path(temporary)
        with ACCURACY.postgres(work, bindir) as (url, psql, environment):
            ACCURACY.run(psql+['-qc', 'CREATE TABLE public.orders (id integer PRIMARY KEY, amount integer NOT NULL); INSERT INTO public.orders VALUES (1, 5), (2, -1)'], env=environment)
            project, profiles = work/'project', work/'profiles'
            (project/'models').mkdir(parents=True)
            profiles.mkdir()
            (project/'dbt_project.yml').write_text('name: sg_import\nversion: "1.0"\nconfig-version: 2\nprofile: sg_import\nmodel-paths: [models]\n')
            (project/'models/order_amounts.sql').write_text('select id, amount from {{ source("raw", "orders") }} where amount > 0\n')
            (project/'models/sources.yml').write_text('version: 2\nsources:\n  - name: raw\n    schema: public\n    tables:\n      - name: orders\n')
            connection = urlsplit(url)
            (profiles/'profiles.yml').write_text(
                f'sg_import:\n  target: test\n  outputs:\n    test:\n      type: postgres\n      host: 127.0.0.1\n      port: {connection.port}\n      user: corpus\n      password: ""\n      dbname: corpus\n      schema: public\n      threads: 1\n')
            environment = dict(environment, DBT_SEND_ANONYMOUS_USAGE_STATS='false', DBT_USE_COLORS='false')
            run([dbt, 'compile', '--project-dir', project, '--profiles-dir', profiles], cwd=work, env=environment)
            manifest_path = project/'target/manifest.json'
            manifest = json.loads(manifest_path.read_text())
            compiled = manifest['nodes']['model.sg_import.order_amounts']['compiled_code']
            assert '"corpus"."public"."orders"' in compiled
            rows = ACCURACY.run(psql+['-Atqc', compiled], env=environment).strip()
            assert rows == '1|5', rows
            run([engine, 'scan', url, '--source-id', 'import-check', '--schema', 'public',
                 '--emit-document', 'catalog.json', '-o', 'base.graph.json'], cwd=work, env=environment)

            def import_file(kind, input_path, label, extra=()):
                run([engine, 'import', 'catalog.json', '--format', kind, '--input', input_path,
                     '--output', label+'.json', '--report', label+'.report.json', *extra], cwd=work, env=environment)
                report = json.loads((work/(label+'.report.json')).read_text())
                assert report['imported'] == 1
                entry = report['entries'][0]
                assert len(entry['sql_sha256']) == 64
                run([engine, 'scan', '--document', label+'.json', '-o', label+'.graph.json'], cwd=work, env=environment)
                graph = json.loads((work/(label+'.graph.json')).read_text())
                vertices = {v['id']: v for v in graph['vertices']}
                owner = entry['routine_id']
                assert vertices[owner]['kind'] == 'query'
                assert 'usage' not in vertices[owner]
                reads = {e['to'] for e in graph['edges'] if e['from'] == owner and e['kind'] == 'reads'}
                assert reads == {'public.orders', 'public.orders.id', 'public.orders.amount'}, reads
                analysis = next(a for a in graph['analysis'] if a['id'] == owner)
                assert analysis['state'] == 'complete', analysis
                assert all(e['from'] in vertices and e['to'] in vertices for e in graph['edges'])
                return report, graph

            dbt_report, _ = import_file('dbt', manifest_path, 'dbt', ['--project-root', project])
            start = timestamp()
            query = 'SELECT id, amount FROM public.orders WHERE amount > 0'
            assert ACCURACY.run(psql+['-Atqc', query], env=environment).strip() == '1|5'
            observed = timestamp()
            log = work/'queries.jsonl'
            header = dict(version=1, type='query-log', source_id='import-check', database='corpus',
                          window_start=start, window_end=timestamp(), sampling='all observed fixture executions')
            row = dict(query_id='fixture-select', schema='public', sql=query, observed_at=observed, executions=1)
            log.write_text(json.dumps(header)+'\n'+json.dumps(row)+'\n')
            log_report, _ = import_file('query-log', log, 'log')
            assert log_report['window_start'] == start and log_report['window_end'] == header['window_end']
            assert log_report['entries'][0]['executions'] == 1
            run([engine, 'import', 'log.json', '--format', 'query-log', '--input', log,
                 '--output', 'repeat.json', '--report', 'repeat.report.json'], cwd=work, env=environment, code=2)
            assert not (work/'repeat.json').exists() and not (work/'repeat.report.json').exists()
            return {'status': 'ok', 'dbt_version': manifest['metadata']['dbt_version'],
                    'manifest_schema': manifest['metadata']['dbt_schema_version'],
                    'manifest_sha256': hashlib.sha256(manifest_path.read_bytes()).hexdigest(),
                    'engine_sha256': hashlib.sha256(engine.read_bytes()).hexdigest(),
                    'dbt_import': dbt_report, 'query_log_import': log_report,
                    'compiled_sql_executed_rows': 1, 'duplicate_import_rejected': True}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--engine', type=Path, required=True)
    parser.add_argument('--dbt', type=Path, required=True)
    parser.add_argument('--postgres-bin', type=Path, default=Path(os.environ.get('PGBIN', '/opt/homebrew/opt/postgresql@16/bin')))
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    result = verify(args.engine.resolve(), args.dbt.resolve(), args.postgres_bin)
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(result, sort_keys=True, indent=2)+'\n')
    print(json.dumps({'status': result['status'], 'dbt_version': result['dbt_version'], 'report': str(args.output)}))


if __name__ == '__main__':
    main()
