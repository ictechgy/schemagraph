#!/usr/bin/env python3
"""실제 PostgreSQL 그래프의 OpenLineage 출력을 공식 스키마·그래프·DDL 기대값과 대조한다.

1. 고정한 공식 OpenLineage 스키마(schemas/openlineage)로 모든 이벤트를 오프라인 검증한다.
2. 같은 입력의 두 실행이 바이트까지 같은지(runId 포함) 본다.
3. graph.json의 derives-from 간선과 이벤트의 DIRECT 항목이 1:1로 대응하는지,
   INDIRECT 항목이 join/predicate 역할의 reads 간선과 대응하는지 양방향으로 본다.
4. 아래 DDL에서 손으로 도출한 사실(조인·필터 컬럼, DML 소유 작업)을 확인한다.
"""

import argparse
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import subprocess
import tempfile

try:
    from jsonschema import Draft202012Validator
    from referencing import Registry, Resource
except ImportError as error:
    raise SystemExit("verify-openlineage.py requires pinned jsonschema; install jsonschema==4.23.0") from error

# 임시 PostgreSQL cluster·명령 실행 도우미를 정확도 검증기와 공유한다.
SPEC = importlib.util.spec_from_file_location('accuracy', Path(__file__).with_name('verify-accuracy.py'))
ACCURACY = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(ACCURACY)

ROOT = Path(__file__).resolve().parent.parent
NAMESPACE = 'postgres://verify.invalid:5432'
EVENT_TIME = '2026-09-25T00:00:00Z'

# order_names는 customers와 조인하고 total로 거르며 name을 그대로 옮긴다.
# load_archive는 한 테이블에만 쓰고(단일 출력), fan_out은 두 테이블에 쓴다(다중 출력).
DDL = '''
CREATE TABLE customers (id integer PRIMARY KEY, name text NOT NULL);
CREATE TABLE orders (id integer PRIMARY KEY, customer_id integer REFERENCES customers(id), total integer);
CREATE TABLE archive (customer_name text, total integer);
CREATE TABLE audit (total integer);
CREATE VIEW order_names AS
  SELECT c.name AS customer_name, o.total
  FROM orders o JOIN customers c ON c.id = o.customer_id
  WHERE o.total > 0;
CREATE FUNCTION load_archive() RETURNS void LANGUAGE sql AS $$
  INSERT INTO archive (customer_name, total)
  SELECT c.name, o.total FROM orders o JOIN customers c ON c.id = o.customer_id WHERE o.total > 10
$$;
CREATE FUNCTION fan_out() RETURNS void LANGUAGE sql AS $$
  INSERT INTO archive (customer_name) SELECT name FROM customers WHERE id > 1;
  INSERT INTO audit (total) SELECT total FROM orders WHERE total > 5
$$;
'''


def validator():
    """고정 스키마 세 개를 절대 $id로 묶은 RunEvent 검증기다 — 네트워크를 쓰지 않는다."""
    registry = Registry()
    schemas = {}
    for path in sorted((ROOT / 'schemas/openlineage').glob('*.json')):
        schema = json.loads(path.read_text())
        schemas[path.name] = schema
        registry = registry.with_resource(schema['$id'], Resource.from_contents(schema))
    return Draft202012Validator(schemas['OpenLineage.json'], registry=registry), registry


def validate_events(events):
    """이벤트와 그 facet을 각 스키마로 검증한다(facet은 $schemaURL이 가리키는 정의로)."""
    event_validator, registry = validator()
    for event in events:
        errors = sorted(event_validator.iter_errors(event), key=lambda e: list(e.path))
        assert not errors, (event['job']['name'], [e.message for e in errors[:3]])
        for output in event['outputs']:
            for name, facet in output.get('facets', {}).items():
                # $ref 래퍼로 검증해야 facet 문서 안의 상대 참조가 원래 문서 기준으로 풀린다.
                facet_validator = Draft202012Validator({'$ref': facet['_schemaURL']}, registry=registry)
                errors = list(facet_validator.iter_errors(facet))
                assert not errors, (event['job']['name'], name, [e.message for e in errors[:3]])


def run_export(engine, graph, output, env):
    """openlineage를 실행해 이벤트 목록과 stderr를 돌려준다."""
    result = subprocess.run([str(engine), 'openlineage', '--graph', str(graph), '--namespace', NAMESPACE,
                             '--database', 'corpus', '--event-time', EVENT_TIME, '-o', str(output)],
                            capture_output=True, text=True, env=env, timeout=120)
    assert result.returncode == 0, result.stderr
    return [json.loads(line) for line in output.read_text().splitlines()], result.stderr


def dataset_field(entry):
    """(데이터셋 이름, 컬럼) — 데이터셋 이름에서 `corpus.` 접두를 떼 그래프 id와 맞춘다."""
    return entry['name'].removeprefix('corpus.') + '.' + entry['field']


def event_pairs(events):
    """이벤트의 DIRECT (작업, 출력 컬럼 id, 입력 컬럼 id)와 INDIRECT (작업, 입력 컬럼 id, subtype)."""
    direct, indirect = [], set()
    for event in events:
        for output in event['outputs']:
            lineage = output.get('facets', {}).get('columnLineage', {})
            target = output['name'].removeprefix('corpus.')
            for field, value in lineage.get('fields', {}).items():
                for source in value['inputFields']:
                    assert source['transformations'] == [{'type': 'DIRECT'}], source
                    direct.append((event['job']['name'], f'{target}.{field}', dataset_field(source)))
            for source in lineage.get('dataset', []):
                for transformation in source['transformations']:
                    assert transformation['type'] == 'INDIRECT', transformation
                    indirect.add((event['job']['name'], dataset_field(source), transformation['subtype']))
    return direct, indirect


def graph_facts(graph):
    """graph.json에서 derives-from 쌍과 join/predicate 역할의 reads를 직접 읽는다."""
    derives = {(edge['from'], edge['to']) for edge in graph['edges'] if edge['kind'] == 'derives-from'}
    roles = {}
    for origin in graph.get('origins') or []:
        if origin['kind'] == 'reads':
            roles.setdefault((origin['from'], origin['to']), set()).add(origin['role'].rsplit(':', 1)[-1])
    subtype = {'join': 'JOIN', 'predicate': 'FILTER'}
    indirect = {(job, column, subtype[role]) for (job, column), found in roles.items()
                for role in found if role in subtype}
    return derives, indirect


def check_bijection(graph, events, stderr):
    """그래프 계보와 이벤트 계보가 양방향으로 같은지 본다. 다중 출력 작업의 간접 사용만 빠질 수 있다."""
    direct, indirect = event_pairs(events)
    derives, graph_indirect = graph_facts(graph)
    # 같은 컬럼 쌍을 두 작업이 만들 수 있다(두 함수가 같은 목적 컬럼에 쓰는 경우) —
    # 작업 안에서는 한 번, 작업을 떼면 그래프의 derives-from 집합과 같아야 한다.
    assert len(direct) == len(set(direct)), 'a derives-from edge appears twice within one job'
    pairs = {(output, source) for _, output, source in direct}
    assert pairs == derives, {'graph_only': sorted(derives - pairs), 'events_only': sorted(pairs - derives)}
    # 알림 끝의 `: ` 뒤가 작업 목록이다(문장 앞부분에도 `: `가 있다).
    omitted = {job for jobs in (line.rsplit(': ', 1)[-1] for line in stderr.splitlines() if line.startswith('note:'))
               for job in jobs.split(', ')}
    assert indirect <= graph_indirect, sorted(indirect - graph_indirect)
    missing = {fact for fact in graph_indirect - indirect if fact[0] not in omitted}
    assert not missing, sorted(missing)
    return len(direct), len(indirect), sorted(omitted)


def check_ddl_facts(events):
    """DDL에서 손으로 도출한 사실 — 뷰의 조인·필터 컬럼과 DML 계보의 소유 작업."""
    _, indirect = event_pairs(events)
    by_job = {event['job']['name']: event for event in events}
    assert {('public.order_names', 'public.customers.id', 'JOIN'), ('public.order_names', 'public.orders.customer_id', 'JOIN'),
            ('public.order_names', 'public.orders.total', 'FILTER')} <= indirect, sorted(indirect)
    archive = by_job['public.load_archive']
    assert [o['name'] for o in archive['outputs']] == ['corpus.public.archive'], archive['outputs']
    fields = archive['outputs'][0]['facets']['columnLineage']['fields']
    assert [s['field'] for s in fields['customer_name']['inputFields']] == ['name'], fields
    assert ('public.load_archive', 'public.orders.total', 'FILTER') in indirect, sorted(indirect)
    fan_out = by_job['public.fan_out']
    assert sorted(o['name'] for o in fan_out['outputs']) == ['corpus.public.archive', 'corpus.public.audit']


def main():
    """임시 PG에서 스키마·결정성·양방향 대응·DDL 사실을 검사하고 결과를 JSON으로 남긴다."""
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--engine', type=Path, required=True)
    parser.add_argument('--postgres-bin', type=Path, default=Path(os.environ.get('PGBIN', '/opt/homebrew/opt/postgresql@16/bin')))
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    engine = args.engine.resolve()
    digest = hashlib.sha256(engine.read_bytes()).hexdigest()
    with tempfile.TemporaryDirectory(prefix='schemagraph-openlineage-') as temporary:
        work = Path(temporary)
        with ACCURACY.postgres(work, args.postgres_bin) as (url, psql, env):
            ACCURACY.run(psql + ['-qc', DDL], env=env)
            graph_path = work / 'graph.json'
            ACCURACY.run([engine, 'scan', url, '--schema', 'public', '-o', graph_path], env=env)
            events, stderr = run_export(engine, graph_path, work / 'first.ndjson', env)
            again, _ = run_export(engine, graph_path, work / 'second.ndjson', env)
            assert (work / 'first.ndjson').read_bytes() == (work / 'second.ndjson').read_bytes(), 'output is not deterministic'
            validate_events(events)
            direct, indirect, omitted = check_bijection(json.loads(graph_path.read_text()), events, stderr)
            check_ddl_facts(events)
            assert omitted == ['public.fan_out'], omitted
    assert hashlib.sha256(engine.read_bytes()).hexdigest() == digest, 'engine binary changed during verification'
    result = {'status': 'ok', 'engine_sha256': digest, 'events': len(events), 'direct': direct,
              'indirect': indirect, 'indirect_omitted': omitted,
              'consumer_ingestion': 'not tested; events are validated against the pinned official schemas'}
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(result, sort_keys=True, indent=2) + '\n')
    print(json.dumps(result, sort_keys=True))


if __name__ == '__main__':
    main()
