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
    if args.views < 1 or args.columns < 2 or args.repeat < 1:
        parser.error('views/repeat must be positive and columns must be at least two')
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
        subprocess.run([str(engine),'scan',f'sqlite:{database}','--emit-document',str(catalog),'-o',str(work/'initial.graph.json')],check=True,capture_output=True)
        expected = hashlib.sha256((work/'initial.graph.json').read_bytes()).hexdigest()
        analyzed = json.loads((work/'initial.graph.json').read_text())
        assert len(analyzed.get('analysis', [])) == args.views
        assert all(item['state']=='complete' for item in analyzed['analysis']), 'benchmark SQL must parse completely'
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
                result = subprocess.run(timed, capture_output=True, text=True, check=True)
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
        report = {'views':args.views,'columns':args.columns,'filter_values':args.filter_values,'repeat':args.repeat,'engine_sha256':hashlib.sha256(engine.read_bytes()).hexdigest(),'graph_sha256':expected,'measurements':measurements,'summary':summary}
        (args.output/'results.json').write_text(json.dumps(report,indent=2,sort_keys=True)+'\n')
        print(json.dumps(summary,indent=2,sort_keys=True))


if __name__ == '__main__':
    main()
