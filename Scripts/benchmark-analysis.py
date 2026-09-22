#!/usr/bin/env python3
"""실제 SQLite에서 수집한 같은 문서로 SQL 캐시의 시간·RSS·출력 일치를 측정한다."""
import argparse
import hashlib
import json
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


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--engine', type=Path, required=True)
    parser.add_argument('--views', type=int, default=500)
    parser.add_argument('--columns', type=int, default=12)
    parser.add_argument('--repeat', type=int, default=3)
    parser.add_argument('--filter-values', type=int, default=0)
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    engine = args.engine.resolve()
    if not 1 <= args.views <= 10000 or not 2 <= args.columns <= 256 or not 3 <= args.repeat <= 10 or not 0 <= args.filter_values <= 10000:
        parser.error('views must be 1..10000, columns 2..256, repeat 3..10 and filter-values 0..10000')
    args.output.mkdir(parents=True, exist_ok=True)
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
        measurements = []
        # 매 반복은 새 cache에서 시작해 cold와 warm을 실제로 구별한다.
        for repeat in range(args.repeat):
            cache = work/f'cache-{repeat}'
            for mode in ('uncached','cold','warm'):
                output = work/'result.json'
                command = [str(engine),'scan','--document',str(catalog),'-o',str(output)]
                if mode != 'uncached':
                    command += ['--cache-dir',str(cache)]
                darwin = __import__('sys').platform == 'darwin'
                timed = ['/usr/bin/time','-l',*command] if darwin else ['/usr/bin/time','-v',*command]
                started = time.perf_counter()
                result = subprocess.run(timed, capture_output=True, text=True, check=True,timeout=180)
                seconds = time.perf_counter() - started
                assert hashlib.sha256(output.read_bytes()).hexdigest() == expected, f'{mode}: graph differs'
                pattern = r'(\d+)\s+maximum resident set size' if darwin else r'Maximum resident set size \(kbytes\):\s*(\d+)'
                match = re.search(pattern,result.stderr)
                rss = int(match[1])/(1024*1024 if darwin else 1024) if match else None
                cache_stats = re.search(r'schemagraph cache: hits=(\d+) misses=(\d+) writes=(\d+) warnings=(\d+)',result.stderr)
                stats = dict(zip(('hits','misses','writes','warnings'), map(int,cache_stats.groups()))) if cache_stats else None
                if mode=='warm':
                    assert stats and stats['hits']==args.views and stats['misses']==0,stats
                measurements.append({'mode':mode,'repeat':repeat,'seconds':seconds,'rss_mib':rss,'cache':stats})
        summary = {mode:{'seconds':statistics.median(row['seconds'] for row in measurements if row['mode']==mode),'rss_mib':statistics.median(row['rss_mib'] for row in measurements if row['mode']==mode)} for mode in ('uncached','cold','warm')}
        report = {'views':args.views,'columns':args.columns,'filter_values':args.filter_values,'repeat':args.repeat,'engine_sha256':hashlib.sha256(engine.read_bytes()).hexdigest(),'graph_sha256':expected,'catalog_bytes':catalog.stat().st_size,'catalog_sha256':hashlib.sha256(catalog.read_bytes()).hexdigest(),'facts':facts,'measurements':measurements,'summary':summary}
        (args.output/'results.json').write_text(json.dumps(report,indent=2,sort_keys=True)+'\n')
        print(json.dumps(summary,indent=2,sort_keys=True))


if __name__ == '__main__':
    main()
