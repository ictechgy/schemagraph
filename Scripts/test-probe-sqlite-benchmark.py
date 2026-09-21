#!/usr/bin/env python3
"""단일 스키마 수집 벤치마크가 틀린 카탈로그·그래프를 정답으로 삼지 않는지 확인한다."""
import importlib.util
import json
from pathlib import Path
import tempfile
import unittest

SPEC = importlib.util.spec_from_file_location('probe_sqlite_benchmark', Path(__file__).with_name('benchmark-probe-sqlite.py'))
CHECK = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(CHECK)


def catalog():
    """실제 생성 DDL의 두 컬럼과 PK만 선언한다."""
    table = dict(name='table_000000', kind='table',
                 columns=[dict(name='id', data_type='INTEGER', ordinal=1, pk_position=1),
                          dict(name='value', data_type='TEXT', ordinal=2, nullable=False)],
                 constraints=[dict(name='pk', kind='pk', columns=['id'])], indexes=[], triggers=[])
    return dict(version=2, schemas=[dict(name='main', objects=[table], routines=[])], limitations=[])


class ProbeBenchmarkTests(unittest.TestCase):
    """양쪽 전송이 같은 오답을 내는 경우도 검증기가 거부해야 한다."""

    def test_catalog_pk_loss_is_rejected(self):
        with tempfile.TemporaryDirectory(prefix='sg-probe-benchmark-test-') as directory:
            path = Path(directory) / 'catalog.json'
            value = catalog()
            path.write_text(json.dumps(value))
            self.assertEqual(CHECK.validate_catalog(path, 'json', 1)['primary_keys'], 1)
            value['schemas'][0]['objects'][0]['columns'][0]['pk_position'] = 0
            path.write_text(json.dumps(value))
            with self.assertRaisesRegex(RuntimeError, 'primary key'):
                CHECK.validate_catalog(path, 'json', 1)

    def test_missing_table_is_rejected(self):
        with tempfile.TemporaryDirectory(prefix='sg-probe-benchmark-test-') as directory:
            path = Path(directory) / 'catalog.json'
            value = catalog()
            value['schemas'][0]['objects'] = []
            path.write_text(json.dumps(value))
            with self.assertRaisesRegex(RuntimeError, 'omitted or invented'):
                CHECK.validate_catalog(path, 'json', 1)

    def test_incomplete_ndjson_is_rejected(self):
        with tempfile.TemporaryDirectory(prefix='sg-probe-benchmark-test-') as directory:
            path = Path(directory) / 'catalog.ndjson'
            path.write_text('{"type":"document","version":2}\n{"type":"schema","name":"main"}\n')
            with self.assertRaisesRegex(RuntimeError, 'trailer'):
                CHECK.load_catalog(path, 'ndjson')

    def test_empty_graph_cannot_be_a_stable_reference(self):
        with tempfile.TemporaryDirectory(prefix='sg-probe-benchmark-test-') as directory:
            path = Path(directory) / 'graph.json'
            path.write_text('{"version":2,"vertices":[],"edges":[]}')
            with self.assertRaisesRegex(RuntimeError, 'table identities'):
                CHECK.validate_graph(path, 1)


if __name__ == '__main__':
    unittest.main()
