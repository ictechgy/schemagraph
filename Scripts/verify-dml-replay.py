#!/usr/bin/env python3
"""보존된 실DB DML 입력을 새 엔진으로 재평가하며 cold/warm 전체 그래프를 비교한다."""

import argparse
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import re
import subprocess
import tempfile

SPEC = importlib.util.spec_from_file_location('dml_producers', Path(__file__).with_name('verify-dml-producers.py'))
PRODUCERS = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(PRODUCERS)


def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def cache_environment(config_home):
    """캐시 키 파일을 결과 디렉터리 밖 임시 디렉터리에 만든다.

    결과 디렉터리는 CI 아티팩트로 올라가므로 키를 그 안에 두지 않는다. 개발자 홈의
    ~/.config와 환경 키도 쓰지 않는다. cold와 warm이 같은 키 파일을 써야 warm이 hit다.
    """
    environment = {name: value for name, value in os.environ.items() if name != 'SCHEMAGRAPH_CACHE_KEY'}
    environment['XDG_CONFIG_HOME'] = str(config_home)
    return environment


def replay(engine, baseline, output, environment):
    """기대값·첫 실패를 보존하고 수정 후 점수는 새 디렉터리에만 기록한다."""
    corpus = PRODUCERS.load_frozen_corpus()
    original = json.loads((baseline/'result.json').read_text())
    manifest = json.loads((baseline/'manifest.json').read_text())
    if manifest['corpus_sha256'] != PRODUCERS.FROZEN_CORPUS_SHA256 or original.get('operational_errors'):
        raise ValueError('baseline must have the frozen corpus and successful database/producer execution')
    output.mkdir(parents=True, exist_ok=False)
    engine_hash = digest(engine)
    reports, failures = {}, []
    for dialect, result in sorted(original['results'].items()):
        scoped = PRODUCERS.adapt_database_schema(corpus, dialect, result.get('schema_mapping', {}).get('actual', corpus['schemas'][dialect]))
        inputs = result.get('producers', {'native': result})
        for producer, facts in sorted(inputs.items()):
            key = dialect+'/'+producer
            document = Path(facts.get('attached_document') or facts['document']).resolve()
            if not document.is_relative_to(baseline.resolve()):
                raise ValueError('baseline document is outside the owned baseline directory')
            document_hash = digest(document)
            catalog = PRODUCERS.DML.validate_catalog_document(json.loads(document.read_text()), scoped, dialect)
            if catalog['failures']:
                raise ValueError('raw catalog preflight failed: '+str(catalog['failures']))
            work = output/dialect/producer
            work.mkdir(parents=True)
            graphs, cache_stats = {}, {}
            for mode in ('uncached', 'cold', 'warm'):
                graph = work/(mode+'.graph.json')
                command = [str(engine), 'scan', '--document', str(document), '-o', str(graph)]
                if mode != 'uncached':
                    command += ['--cache-dir', str(work/'cache')]
                scan = subprocess.run(command, capture_output=True, text=True, timeout=120, env=environment)
                (work/(mode+'.stderr')).write_text(scan.stderr)
                if scan.returncode:
                    raise RuntimeError(f'{key}/{mode} scan failed: '+scan.stderr[-1500:])
                graphs[mode] = digest(graph)
                stats = re.search(r'schemagraph cache: hits=(\d+) misses=(\d+) writes=(\d+) warnings=(\d+)', scan.stderr)
                if stats:
                    cache_stats[mode] = dict(zip(('hits', 'misses', 'writes', 'warnings'), map(int, stats.groups())))
            if len(set(graphs.values())) != 1:
                failures.append(key+': uncached/cold/warm full graph bytes differ')
            warm = cache_stats.get('warm', {})
            if not warm.get('hits') or any(warm.get(name) != 0 for name in ('misses', 'writes', 'warnings')):
                failures.append(key+': warm cache did not restore every analyzed body cleanly')
            if digest(document) != document_hash:
                raise RuntimeError('immutable baseline document changed during replay')
            graph_value = json.loads((work/'uncached.graph.json').read_text())
            score = PRODUCERS.DML.evaluate_graph(graph_value, scoped, dialect, catalog['subject_records'])
            failures.extend(key+': '+item for item in score['failures'])
            reports[key] = {'document_sha256': document_hash, 'graph_sha256': graphs,
                            'cache': cache_stats, 'score': score}
    if digest(engine) != engine_hash:
        raise RuntimeError('engine changed during replay; use an immutable binary')
    report = {'status': 'failed' if failures else 'ok', 'failures': failures, 'results': reports,
              'corpus_sha256': PRODUCERS.FROZEN_CORPUS_SHA256, 'engine_sha256': engine_hash,
              'baseline_report_sha256': digest(baseline/'result.json')}
    (output/'result.json').write_text(json.dumps(report, sort_keys=True, indent=2)+'\n')
    print(json.dumps({'status': report['status'], 'failures': len(failures), 'output': str(output)}))
    return 1 if failures else 0


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--engine', type=Path, required=True)
    parser.add_argument('--baseline', type=Path, required=True)
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    with tempfile.TemporaryDirectory(prefix='schemagraph-dml-replay-config-') as config_home:
        return replay(args.engine.resolve(), args.baseline.resolve(), args.output.resolve(),
                      cache_environment(Path(config_home)))


if __name__ == '__main__':
    raise SystemExit(main())
