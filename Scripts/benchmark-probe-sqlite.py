#!/usr/bin/env python3
"""실제 단일 SQLite 스키마에서 Go/JDBC 수집 메모리와 전송 사실 보존을 측정한다."""
import argparse
import importlib.util
import json
from pathlib import Path
import platform
import sqlite3
import subprocess
import sys
import tempfile

SPEC = importlib.util.spec_from_file_location('scale_measurements', Path(__file__).with_name('benchmark-scale.py'))
SCALE = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = SCALE
SPEC.loader.exec_module(SCALE)


def create_schema(path, objects):
    """실제 DB가 PK와 컬럼을 정의하게 하며 생성 비용은 수집 시간에서 제외한다."""
    connection = sqlite3.connect(path)
    try:
        connection.execute('BEGIN')
        for number in range(objects):
            connection.execute(f'CREATE TABLE table_{number:06d} (id INTEGER PRIMARY KEY, value TEXT NOT NULL)')
        connection.commit()
    finally:
        connection.close()


def load_catalog(path, format_name):
    """v2 trailer와 단일 schema 경계를 확인하고 형식별 사실만 정규화한다."""
    if format_name == 'json':
        catalog = json.loads(path.read_text())
        if catalog.get('version') != 2 or len(catalog['schemas']) != 1:
            raise RuntimeError('JSON catalog must contain exactly one v2 schema')
        schema = catalog['schemas'][0]
        return schema['name'], schema['objects'], schema.get('routines', []), catalog.get('limitations', [])
    records = [json.loads(line) for line in path.read_text().splitlines()]
    if records[0].get('type') != 'document' or records[0].get('version') != 2 or records[-1].get('type') != 'limitations':
        raise RuntimeError('NDJSON catalog is missing its v2 header or required trailer')
    schemas = [row['name'] for row in records if row['type'] == 'schema']
    if schemas != ['main'] or any(row['type'] not in ('document', 'schema', 'object', 'limitations') for row in records):
        raise RuntimeError('NDJSON catalog contains an unexpected schema or record kind')
    if any(row.get('schema') != 'main' for row in records if row['type'] == 'object'):
        raise RuntimeError('NDJSON object belongs to the wrong schema')
    limitations = sorted(set(records[0].get('limitations', []) + records[-1]['data']))
    return 'main', [row['data'] for row in records if row['type'] == 'object'], [], limitations


def validate_catalog(path, format_name, count):
    """동일하게 틀린 JSON/NDJSON 출력도 실제 DDL의 독립 사실로 거부한다."""
    schema, objects, routines, limitations = load_catalog(path, format_name)
    expected = {f'table_{number:06d}' for number in range(count)}
    if schema != 'main' or routines or len(objects) != count or {obj['name'] for obj in objects} != expected:
        raise RuntimeError('Collector omitted or invented tables/routines in the known SQLite schema')
    for obj in objects:
        columns = {column['name']: column for column in obj['columns']}
        primary = [constraint for constraint in obj['constraints'] if constraint['kind'] == 'pk']
        if obj['kind'] != 'table' or set(columns) != {'id', 'value'} or len(obj['columns']) != 2:
            raise RuntimeError('Collector changed the known table columns or kind')
        if columns['id'].get('pk_position') != 1 or len(primary) != 1 or primary[0]['columns'] != ['id']:
            raise RuntimeError('Collector lost the declared integer primary key')
        if columns['id']['data_type'].upper() != 'INTEGER' or columns['value']['data_type'].upper() != 'TEXT' or columns['value']['nullable'] is not False:
            raise RuntimeError('Collector changed declared SQLite types or the value NOT NULL property')
        if columns['id']['ordinal'] != 1 or columns['value']['ordinal'] != 2:
            raise RuntimeError('Collector changed the declared column order')
        if obj.get('indexes') or obj.get('triggers') or 'usage' in obj:
            raise RuntimeError('Collector invented indexes/triggers or SQLite usage observations')
    return {'schemas': 1, 'tables': count, 'columns': 2*count, 'primary_keys': count,
            'limitations': sorted(limitations)}


def validate_graph(path, count):
    """엔진도 선언된 schema/table/column/PK와 contains 간선만 갖는지 확인한다."""
    graph = json.loads(path.read_text())
    vertices = {vertex['id']: vertex for vertex in graph['vertices']}
    tables = {f'main.table_{number:06d}' for number in range(count)}
    columns = {table+'.'+column for table in tables for column in ('id', 'value')}
    if {key for key, vertex in vertices.items() if vertex['kind'] == 'table'} != tables:
        raise RuntimeError('Engine table identities differ from the SQLite DDL')
    if {key for key, vertex in vertices.items() if vertex['kind'] == 'column'} != columns:
        raise RuntimeError('Engine column identities differ from the SQLite DDL')
    constraints = {key for key, vertex in vertices.items() if vertex['kind'] == 'constraint'}
    schemas = {key for key, vertex in vertices.items() if vertex['kind'] == 'schema'}
    if len(vertices) != 4*count+1 or len(vertices) != len(graph['vertices']) or len(constraints) != count or schemas != {'main'}:
        raise RuntimeError('Engine schema/PK vertex counts differ from the SQLite DDL')
    edges = {(edge['from'], edge['to'], edge['kind']) for edge in graph['edges']}
    required = {('main', table, 'contains') for table in tables}
    required |= {(table, table+'.'+column, 'contains') for table in tables for column in ('id', 'value')}
    if not required <= edges or len(edges) != 4*count or len(edges) != len(graph['edges']):
        raise RuntimeError('Engine contains facts are missing, duplicated, or unexpected')
    owned = dict.fromkeys(tables, 0)
    for source, target, kind in edges:
        if source in tables and target in constraints and kind == 'contains' and target.startswith(source+'.'):
            owned[source] += 1
    if any(value != 1 for value in owned.values()):
        raise RuntimeError('Engine assigned a primary key to the wrong table')
    if any(source not in vertices or target not in vertices or kind != 'contains' for source, target, kind in edges):
        raise RuntimeError('Engine invented a dependency or phantom endpoint')
    return {'vertices': len(vertices), 'edges': len(edges), 'primary_keys': len(constraints)}


def collector_command(probe, jdbc_jar, java, database, format_name, output):
    """수집기마다 URL 문법만 다르게 하고 같은 v2 전송 계약을 측정한다."""
    if jdbc_jar:
        command = [str(java), '-jar', str(jdbc_jar), '--url', 'jdbc:sqlite:'+str(database)]
    else:
        command = [str(probe), '--url', 'sqlite:'+str(database)]
    return command + ['--document-version', '2', '--format', format_name, '-o', str(output)]


def main():
    """수집 프로세스만 계측하고 독립 검증과 입력 생성은 계측 밖에서 수행한다."""
    parser = argparse.ArgumentParser(description='Measure Go or JDBC collection on a real, single SQLite schema and validate catalog/graph facts.')
    producer = parser.add_mutually_exclusive_group(required=True)
    producer.add_argument('--probe', type=Path)
    producer.add_argument('--jdbc-jar', type=Path)
    parser.add_argument('--java', type=Path, help='Explicit Java executable for a JDBC run')
    parser.add_argument('--engine', type=Path, required=True)
    parser.add_argument('--objects', type=int, nargs='+', default=[2000, 10000])
    parser.add_argument('--repeat', type=int, default=3)
    parser.add_argument('--timeout', type=int, default=120)
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    if bool(args.jdbc_jar) != bool(args.java):
        parser.error('--jdbc-jar and --java must be used together')
    if any(count < 1 or count > 20000 for count in args.objects) or len(set(args.objects)) != len(args.objects) or not 3 <= args.repeat <= 10 or not 1 <= args.timeout <= 600:
        parser.error('objects must be unique and 1..20000; repeat 3..10; timeout 1..600 seconds')
    artifact = (args.probe or args.jdbc_jar).resolve()
    report = {'producer': 'jdbc' if args.jdbc_jar else 'go', 'probe_sha256': SCALE.sha256(artifact), 'engine_sha256': SCALE.sha256(args.engine),
              'sqlite_version': sqlite3.sqlite_version, 'platform': platform.platform(),
              'verifier_sha256': SCALE.sha256(Path(__file__)), 'timer_sha256': SCALE.sha256(Path(SCALE.__file__)), 'results': {}}
    if args.java:
        version = subprocess.run([str(args.java.resolve()), '-version'], capture_output=True,
                                 text=True, timeout=15, check=True)
        report['java_version'] = (version.stdout+version.stderr).strip()
    with tempfile.TemporaryDirectory(prefix='schemagraph-probe-sqlite-benchmark-') as directory:
        for count in sorted(args.objects):
            work = Path(directory) / str(count)
            work.mkdir()
            database = work / 'single.sqlite'
            create_schema(database, count)
            result = report['results'][str(count)] = {'database_sha256': SCALE.sha256(database), 'formats': {}}
            expected_graph, expected_catalog = None, None
            for format_name in ('json', 'ndjson'):
                output = work / f'catalog.{format_name}'
                samples, runs = [], []
                for repeat in range(args.repeat):
                    command = collector_command(args.probe.resolve() if args.probe else None,
                                                artifact if args.jdbc_jar else None,
                                                args.java.resolve() if args.java else None,
                                                database, format_name, output)
                    sample = SCALE.run_measured(command,
                                               timeout=args.timeout, stderr_path=work/'probe.stderr')
                    SCALE.check_rss(sample, 2048, 'SQLite collector')
                    facts = validate_catalog(output, format_name, count)
                    if expected_catalog is None:
                        expected_catalog = facts
                    elif facts != expected_catalog:
                        raise RuntimeError('Catalog facts changed across formats or repetitions')
                    samples.append(sample)
                    runs.append({'seconds': sample.seconds, 'peak_rss_bytes': sample.peak_rss_bytes,
                                 'document_sha256': SCALE.sha256(output), 'document_bytes': output.stat().st_size})
                graph = work / f'{format_name}.graph.json'
                validation = SCALE.run_measured([str(args.engine.resolve()), 'scan', '--document', str(output), '-o', str(graph)],
                                                timeout=args.timeout, stderr_path=work/'engine.stderr')
                SCALE.check_rss(validation, 2048, 'SQLite catalog engine validation')
                graph_facts = validate_graph(graph, count)
                graph_hash = SCALE.sha256(graph)
                if expected_graph is None:
                    expected_graph = graph_hash
                elif expected_graph != graph_hash:
                    raise RuntimeError('JSON and NDJSON collected catalogs produced different graph bytes')
                result['formats'][format_name] = {'runs': runs, 'summary': SCALE.summarize(samples),
                                                  'catalog_facts': facts, 'graph_facts': graph_facts, 'graph_sha256': graph_hash}
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(report, indent=2, sort_keys=True)+'\n')
    print(json.dumps({'status': 'ok', 'report': str(args.output), 'objects': args.objects}))


if __name__ == '__main__':
    main()
