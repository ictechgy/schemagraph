#!/usr/bin/env python3
"""실제 CLI로 Action의 정책·기준선·불완전 결과 계약을 확인한다."""

import argparse
import copy
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile

ROOT = Path(__file__).resolve().parents[1]


def fixtures(directory):
    """검토 대상은 DB 접속 없이 재현할 수 있는 고정 카탈로그다."""
    document = {
        'version': 1, 'dialect': 'sqlite', 'reader': 'action-smoke',
        'limitations': [], 'dependencies': [],
        'context': {'source_id': 'action-smoke', 'database': 'review',
                    'schema_filter': ['main'], 'catalog_complete': True},
        'schemas': [{'name': 'main', 'routines': [], 'objects': [{
            'name': 'customers', 'kind': 'table', 'constraints': [],
            'indexes': [], 'triggers': [], 'columns': [
                {'name': 'id', 'data_type': 'INTEGER', 'nullable': False,
                 'ordinal': 1, 'pk_position': 1},
                {'name': 'email', 'data_type': 'TEXT', 'nullable': True,
                 'ordinal': 2, 'pk_position': 0}]}]}],
    }
    directory.mkdir(parents=True, exist_ok=True)
    (directory/'before.json').write_text(json.dumps(document), encoding='utf-8')
    after = copy.deepcopy(document)
    after['schemas'][0]['objects'][0]['columns'].pop()
    (directory/'after.json').write_text(json.dumps(after), encoding='utf-8')
    (directory/'policy.toml').write_text(
        'version = 1\nfail_threshold = "high"\n[severity]\ncolumn-removed = "high"\n',
        encoding='utf-8')


def verify(engine, schema):
    """동일한 실제 입력에서 세 렌더러의 gate와 SARIF 계약을 확인한다."""
    with tempfile.TemporaryDirectory(prefix='.review-smoke-', dir=ROOT) as temporary:
        directory = Path(temporary).resolve()
        fixtures(directory)
        environment = dict(os.environ, GITHUB_WORKSPACE=str(ROOT), RUNNER_TEMP=str(directory),
                           SG_REVIEW_ENGINE=str(engine), SG_REVIEW_BEFORE=str(directory/'before.json'),
                           SG_REVIEW_AFTER=str(directory/'after.json'),
                           SG_REVIEW_POLICY=str(directory/'policy.toml'), SG_REVIEW_BASELINE='',
                           SG_REVIEW_COMPLETE='true', SG_REVIEW_AS_OF='', SG_REVIEW_TIMEOUT='30')

        def run(name, expected):
            report_dir = directory/name
            environment.update(SG_REVIEW_OUTPUT=str(report_dir),
                               GITHUB_OUTPUT=str(directory/(name+'.outputs')),
                               GITHUB_STEP_SUMMARY=str(directory/(name+'.md')))
            result = subprocess.run([sys.executable, str(ROOT/'Scripts/run-review-action.py')],
                                    env=environment, capture_output=True, text=True, timeout=120)
            assert result.returncode == expected, (name, result.returncode, result.stderr)
            action = json.loads((report_dir/'action-result.json').read_text())
            assert action['exit_code'] == expected
            value = json.loads((report_dir/'review.json').read_text())
            sarif = json.loads((report_dir/'review.sarif').read_text())
            assert sarif['runs'][0]['properties']['totalChanges'] == value['totalChanges']
            for finding in sarif['runs'][0]['results']:
                assert 'physicalLocation' not in finding['locations'][0]
            assert (directory/(name+'.md')).read_text() == (report_dir/'review.md').read_text()
            if schema:
                from jsonschema import Draft4Validator
                Draft4Validator(json.loads(schema.read_text())).validate(sarif)
            return value, sarif

        changed, _ = run('findings', 1)
        assert changed['policy']['failed'] is True
        baseline = directory/'baseline.json'
        subprocess.run([str(engine), 'review', str(directory/'before.json'),
                        str(directory/'after.json'), '--write-baseline', str(baseline)],
                       stdout=subprocess.DEVNULL, check=True)
        environment['SG_REVIEW_BASELINE'] = str(baseline)
        existing, sarif = run('baseline', 0)
        assert existing['policy']['findings'][0]['baselineState'] == 'existing'
        assert sarif['runs'][0]['results'][0]['baselineState'] == 'unchanged'
        incomplete = json.loads((directory/'after.json').read_text())
        incomplete['context']['catalog_complete'] = False
        incomplete['limitations'] = ['one catalog scope was not readable']
        (directory/'after.json').write_text(json.dumps(incomplete))
        partial, _ = run('incomplete', 2)
        assert partial['comparison'] == 'unverified'
        assert partial['comparisonNotes']
        print('Actual CLI + Action: findings=1, baseline=0, incomplete=2; reports verified')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--engine', type=Path)
    parser.add_argument('--schema', type=Path)
    parser.add_argument('--prepare', type=Path)
    args = parser.parse_args()
    if args.prepare:
        fixtures(args.prepare)
    else:
        if not args.engine:
            parser.error('--engine is required unless --prepare is used')
        verify(args.engine.resolve(), args.schema)


if __name__ == '__main__':
    main()
