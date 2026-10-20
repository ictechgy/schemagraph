#!/usr/bin/env python3
"""실제 SQLite에서 수집한 같은 문서로 SQL 캐시의 시간·RSS·출력 일치를 측정한다.

`--routines`를 주면 pg_get_functiondef 꼴 PL/pgSQL 함수 문서를 크기별로 만들어
routine 수에 대한 분석 시간 증가율을 함께 잰다.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import sqlite3
import statistics
import subprocess
import tempfile
import time


def validate_graph(path, views, columns):
    """cache의 동일한 오답도 통과하지 않도록 생성 DDL의 간선을 양방향 대조한다."""
    graph = json.loads(path.read_text())
    vertices = {'main': 'schema', 'main.t': 'table'}
    vertices.update({f'main.t.c{column}': 'column' for column in range(columns)})
    edges = {('main', 'main.t', 'contains')}
    edges.update(('main.t', f'main.t.c{column}', 'contains') for column in range(columns))
    for index in range(views):
        view = f'main.v{index:05}'
        vertices[view] = 'view'
        edges.update({('main', view, 'contains'), (view, 'main.t', 'reads')})
        for column in range(columns):
            output, source = f'{view}.value{column}', f'main.t.c{column}'
            vertices[output] = 'column'
            edges.update({(view, output, 'contains'), (view, source, 'reads'), (output, source, 'derives-from')})
    actual_vertices = {vertex['id']: vertex['kind'] for vertex in graph['vertices']}
    actual_edges = {(edge['from'], edge['to'], edge['kind']) for edge in graph['edges']}
    if actual_vertices != vertices or len(graph['vertices']) != len(vertices):
        raise RuntimeError('graph vertices differ from independent benchmark DDL facts')
    if actual_edges != edges or len(graph['edges']) != len(edges):
        raise RuntimeError(f'graph edges differ from independent SQL facts: {len(edges-actual_edges)} missing, {len(actual_edges-edges)} unexpected')
    analyses = graph.get('analysis', [])
    if len(analyses) != views or any(item['state'] != 'complete' for item in analyses):
        raise RuntimeError('benchmark SQL must parse completely')
    return {'vertices': len(vertices), 'edges': len(edges), 'views': views}


def routine_tables(functions):
    """함수 두 개가 테이블 하나를 나눠 쓰게 해 테이블 수도 함께 늘린다."""
    return max(functions // 2, 2)


def routine_document(functions):
    """PostgreSQL reader가 싣는 pg_get_functiondef 원문 모양의 PL/pgSQL 함수 문서."""
    tables = routine_tables(functions)
    objects = []
    for index in range(tables):
        columns = [{'name': 'id', 'data_type': 'integer', 'nullable': False, 'ordinal': 1, 'pk_position': 1}]
        columns += [{'name': f'c{column}', 'data_type': 'text', 'nullable': True, 'ordinal': column + 1, 'pk_position': 0} for column in range(1, 10)]
        objects.append({'name': f't{index:05}', 'kind': 'table', 'columns': columns, 'constraints': [], 'indexes': [], 'triggers': []})
    routines = []
    for index in range(functions):
        source, target = f't{index % tables:05}', f't{(index + 1) % tables:05}'
        body = (f'CREATE OR REPLACE FUNCTION public.f{index:05}(p_id integer)\n RETURNS integer\n LANGUAGE plpgsql\n'
                f'AS $function$\nDECLARE\n  v_total integer;\nBEGIN\n'
                f'  SELECT count(*) INTO v_total FROM public.{source} WHERE c1 = p_id::text;\n'
                f'  UPDATE public.{target} SET c2 = v_total::text WHERE id = p_id;\n'
                f'  RETURN v_total;\nEND;\n$function$\n')
        routines.append({'name': f'f{index:05}', 'kind': 'function', 'language': 'plpgsql', 'signature': 'p_id integer', 'body': body})
    return {'version': 1, 'dialect': 'postgres', 'reader': 'benchmark-analysis', 'limitations': [],
            'schemas': [{'name': 'public', 'objects': objects, 'routines': routines}]}


def validate_routine_graph(path, functions):
    """함수마다 생성 SQL의 테이블 읽기·쓰기만 생겼는지 양방향 대조한다."""
    graph = json.loads(path.read_text())
    tables = routine_tables(functions)
    expected = set()
    for index in range(functions):
        function = f'public.f{index:05}(p_id integer)'
        expected.add((function, f'public.t{index % tables:05}', 'reads'))
        expected.add((function, f'public.t{(index + 1) % tables:05}', 'writes'))
    table_ids = {vertex['id'] for vertex in graph['vertices'] if vertex['kind'] == 'table'}
    actual = {(edge['from'], edge['to'], edge['kind']) for edge in graph['edges'] if edge['to'] in table_ids and edge['kind'] in ('reads', 'writes')}
    if actual != expected:
        raise RuntimeError(f'routine table edges differ from generated SQL facts: {len(expected-actual)} missing, {len(actual-expected)} unexpected')
    analyses = graph.get('analysis', [])
    if len(analyses) != functions or any(item['state'] != 'complete' for item in analyses):
        raise RuntimeError('benchmark routines must parse completely')
    return {'vertices': len(graph['vertices']), 'edges': len(graph['edges']), 'routines': functions}


def cache_environment(work):
    """캐시 키 파일을 임시 작업 디렉터리에 만들어 개발자 홈의 ~/.config와 환경 키를 쓰지 않는다."""
    environment = {name: value for name, value in os.environ.items() if name != 'SCHEMAGRAPH_CACHE_KEY'}
    environment['XDG_CONFIG_HOME'] = str(work/'config')
    return environment


def measure_modes(engine, catalog, expected, repeat, work, bodies):
    """캐시 없음·빈 캐시·재사용 캐시를 반복마다 새 cache로 재고 출력 바이트를 대조한다."""
    measurements = []
    environment = cache_environment(work)
    # 매 반복은 새 cache에서 시작해 cold와 warm을 실제로 구별한다.
    for index in range(repeat):
        cache = work/f'cache-{index}'
        for mode in ('uncached','cold','warm'):
            output = work/'result.json'
            command = [str(engine),'scan','--document',str(catalog),'-o',str(output)]
            if mode != 'uncached':
                command += ['--cache-dir',str(cache)]
            darwin = __import__('sys').platform == 'darwin'
            timed = ['/usr/bin/time','-l',*command] if darwin else ['/usr/bin/time','-v',*command]
            started = time.perf_counter()
            result = subprocess.run(timed, capture_output=True, text=True, check=True,timeout=180,env=environment)
            seconds = time.perf_counter() - started
            assert hashlib.sha256(output.read_bytes()).hexdigest() == expected, f'{mode}: graph differs'
            pattern = r'(\d+)\s+maximum resident set size' if darwin else r'Maximum resident set size \(kbytes\):\s*(\d+)'
            match = re.search(pattern,result.stderr)
            rss = int(match[1])/(1024*1024 if darwin else 1024) if match else None
            cache_stats = re.search(r'schemagraph cache: hits=(\d+) misses=(\d+) writes=(\d+) warnings=(\d+)',result.stderr)
            stats = dict(zip(('hits','misses','writes','warnings'), map(int,cache_stats.groups()))) if cache_stats else None
            if mode=='warm':
                assert stats and stats['hits']==bodies and stats['misses']==0,stats
            measurements.append({'mode':mode,'repeat':index,'seconds':seconds,'rss_mib':rss,'cache':stats})
    return measurements


def summarize(measurements):
    """모드별 중앙값 시간과 RSS."""
    return {mode:{'seconds':statistics.median(row['seconds'] for row in measurements if row['mode']==mode),'rss_mib':statistics.median(row['rss_mib'] for row in measurements if row['mode']==mode)} for mode in ('uncached','cold','warm')}


def benchmark_routines(args, engine):
    """routine 수를 늘려 가며 분석 시간 증가율을 재고, 상한을 주면 넘을 때 실패한다.

    cold 실행은 몸체마다 캐시 항목을 쓰는 비용을 함께 재므로 증가율을 보고만 하고,
    상한은 캐시 없음과 재사용 캐시 실행에만 건다.
    """
    sizes = []
    with tempfile.TemporaryDirectory(prefix='schemagraph-routine-benchmark-') as directory:
        work = Path(directory)
        for functions in args.routines:
            catalog = work/f'routines-{functions}.json'
            catalog.write_text(json.dumps(routine_document(functions)))
            initial = work/f'routines-{functions}.graph.json'
            subprocess.run([str(engine),'scan','--document',str(catalog),'-o',str(initial)],check=True,capture_output=True,timeout=180)
            expected = hashlib.sha256(initial.read_bytes()).hexdigest()
            facts = validate_routine_graph(initial, functions)
            cache_root = work/f'caches-{functions}'
            cache_root.mkdir()
            measurements = measure_modes(engine, catalog, expected, args.repeat, cache_root, functions)
            sizes.append({'routines':functions,'graph_sha256':expected,'catalog_bytes':catalog.stat().st_size,'catalog_sha256':hashlib.sha256(catalog.read_bytes()).hexdigest(),'facts':facts,'measurements':measurements,'summary':summarize(measurements)})
    growth = [{'from':smaller['routines'],'to':larger['routines'],**{mode:larger['summary'][mode]['seconds']/smaller['summary'][mode]['seconds'] for mode in ('uncached','cold','warm')}} for smaller, larger in zip(sizes, sizes[1:])]
    report = {'routines':args.routines,'repeat':args.repeat,'max_growth':args.max_growth,'engine_sha256':hashlib.sha256(engine.read_bytes()).hexdigest(),'sizes':sizes,'growth':growth}
    (args.output/'results.json').write_text(json.dumps(report,indent=2,sort_keys=True)+'\n')
    print(json.dumps({'summary':{row['routines']:row['summary'] for row in sizes},'growth':growth},indent=2,sort_keys=True))
    exceeded = [row for row in growth if args.max_growth and max(row[mode] for mode in ('uncached','warm')) > args.max_growth]
    if exceeded:
        raise RuntimeError(f'uncached or warm routine analysis grew faster than {args.max_growth}x between sizes: {exceeded}')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--engine', type=Path, required=True)
    parser.add_argument('--views', type=int, default=500)
    parser.add_argument('--columns', type=int, default=12)
    parser.add_argument('--repeat', type=int, default=3)
    parser.add_argument('--filter-values', type=int, default=0)
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--routines', type=int, nargs='+', help='measure generated PL/pgSQL routine documents of these increasing sizes instead of SQLite views')
    parser.add_argument('--max-growth', type=float, help='fail when the uncached or warm median grows by more than this factor between consecutive --routines sizes; cold runs also write cache entries and are only reported')
    args = parser.parse_args()
    engine = args.engine.resolve()
    if not 1 <= args.views <= 10000 or not 2 <= args.columns <= 256 or not 3 <= args.repeat <= 10 or not 0 <= args.filter_values <= 10000:
        parser.error('views must be 1..10000, columns 2..256, repeat 3..10 and filter-values 0..10000')
    if args.routines and (any(not 2 <= size <= 20000 for size in args.routines) or sorted(set(args.routines)) != args.routines):
        parser.error('routines must be strictly increasing sizes within 2..20000')
    if args.max_growth is not None and (not args.routines or args.max_growth <= 1):
        parser.error('max-growth needs --routines and must be greater than 1')
    args.output.mkdir(parents=True, exist_ok=True)
    if args.routines:
        benchmark_routines(args, engine)
        return
    with tempfile.TemporaryDirectory(prefix='schemagraph-analysis-benchmark-') as directory:
        work = Path(directory)
        database = work/'fixture.db'
        with sqlite3.connect(database) as connection:
            connection.execute('CREATE TABLE t (' + ','.join(f'c{i} INTEGER' for i in range(args.columns)) + ')')
            for i in range(args.views):
                projection = ','.join(f'b.c{j} + z.c{j} AS value{j}' for j in range(args.columns))
                predicate = f'c0 > {i}'
                if args.filter_values:
                    predicate += ' AND c0 IN (' + ','.join(str(value) for value in range(args.filter_values)) + ')'
                connection.execute(f'CREATE VIEW v{i:05} AS WITH a AS (SELECT * FROM t), b AS (SELECT * FROM a WHERE {predicate}) SELECT {projection} FROM b JOIN t z ON b.c0 = z.c0')
        catalog = work/'catalog.json'
        subprocess.run([str(engine),'scan',f'sqlite:{database}','--emit-document',str(catalog),'-o',str(work/'initial.graph.json')],check=True,capture_output=True,timeout=180)
        expected = hashlib.sha256((work/'initial.graph.json').read_bytes()).hexdigest()
        facts = validate_graph(work/'initial.graph.json', args.views, args.columns)
        measurements = measure_modes(engine, catalog, expected, args.repeat, work, args.views)
        summary = summarize(measurements)
        report = {'views':args.views,'columns':args.columns,'filter_values':args.filter_values,'repeat':args.repeat,'engine_sha256':hashlib.sha256(engine.read_bytes()).hexdigest(),'graph_sha256':expected,'catalog_bytes':catalog.stat().st_size,'catalog_sha256':hashlib.sha256(catalog.read_bytes()).hexdigest(),'facts':facts,'measurements':measurements,'summary':summary}
        (args.output/'results.json').write_text(json.dumps(report,indent=2,sort_keys=True)+'\n')
        print(json.dumps(summary,indent=2,sort_keys=True))


if __name__ == '__main__':
    main()
