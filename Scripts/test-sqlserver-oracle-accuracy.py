#!/usr/bin/env python3
"""정확도 평가가 누락·오탐·거짓 complete와 유령 간선을 거부하는지 확인한다."""
import importlib.util
import json
from pathlib import Path
from types import SimpleNamespace
import subprocess
import tempfile
import unittest
from unittest.mock import patch

SPEC = importlib.util.spec_from_file_location('extended_accuracy', Path(__file__).with_name('verify-sqlserver-oracle-accuracy.py'))
CHECK = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(CHECK)


def fixture():
    """한 출력과 조건 전용 컬럼으로 읽기와 값 계보를 구별한다."""
    case = dict(name='sample', kind='view', read_scope='column',
                reads=['dbo.Genre', 'dbo.Genre.Name', 'dbo.Genre.GenreId'],
                lineage={'label': ['dbo.Genre.Name']}, writes=[], calls=[], state='complete')
    vertices = [dict(id='dbo.Genre', name='Genre', schema='dbo', kind='table'),
                dict(id='dbo.Genre.Name', name='Name', schema='dbo', kind='column'),
                dict(id='dbo.Genre.GenreId', name='GenreId', schema='dbo', kind='column'),
                dict(id='dbo.acc_sample', name='acc_sample', schema='dbo', kind='view'),
                dict(id='dbo.acc_sample.label', name='label', schema='dbo', kind='column')]
    edges = [dict(from_='dbo.acc_sample', kind='reads', to=target) for target in case['reads']]
    edges.append(dict(from_='dbo.acc_sample.label', kind='derives-from', to='dbo.Genre.Name'))
    for edge in edges:
        edge['from'] = edge.pop('from_')
    graph = dict(vertices=vertices, edges=edges,
                 analysis=[dict(id='dbo.acc_sample', state='complete', diagnostics=[])])
    return graph, dict(schema='dbo', dialect='sqlserver', cases=[case])


class AccuracyTests(unittest.TestCase):
    """집계 숫자가 아니라 각 독립 사실의 수용·거부 결과를 고정한다."""

    def test_matching_facts_pass(self):
        """조건 컬럼은 읽기만 보고해도 정확한 결과로 인정한다."""
        graph, cases = fixture()
        self.assertEqual(CHECK.evaluate_graph(graph, cases)['failures'], [])

    def test_missing_read_rejects_false_complete(self):
        """complete 상태라도 조건 의존성을 누락하면 실패해야 한다."""
        graph, cases = fixture()
        graph['edges'] = [e for e in graph['edges'] if e['to'] != 'dbo.Genre.GenreId']
        result = CHECK.evaluate_graph(graph, cases)
        self.assertTrue(result['failures'])
        self.assertEqual(result['false_complete_cases'], ['dbo.acc_sample'])

    def test_extra_value_source_is_not_a_read_only_success(self):
        """필터 컬럼을 출력 계보에 끼워 넣는 오탐도 검출한다."""
        graph, cases = fixture()
        graph['edges'].append({'from': 'dbo.acc_sample.label', 'kind': 'derives-from', 'to': 'dbo.Genre.GenreId'})
        self.assertTrue(CHECK.evaluate_graph(graph, cases)['failures'])

    def test_changed_expected_fact_fails(self):
        """검토 기대값의 변조가 실제 보고와 불일치하면 통과시키지 않는다."""
        graph, cases = fixture()
        cases['cases'][0]['lineage']['label'] = ['dbo.Genre.GenreId']
        self.assertTrue(CHECK.evaluate_graph(graph, cases)['failures'])

    def test_missing_analysis_is_failure(self):
        """대상 정점이 있어도 분석 상태가 없으면 검증 완료로 세지 않는다."""
        graph, cases = fixture()
        graph['analysis'] = []
        self.assertTrue(CHECK.evaluate_graph(graph, cases)['failures'])

    def test_phantom_endpoint_is_failure(self):
        """점수 범위 밖 간선에도 유령 정점을 허용하지 않는다."""
        graph, cases = fixture()
        graph['edges'].append({'from': 'dbo.missing', 'kind': 'reads', 'to': 'dbo.Genre'})
        self.assertTrue(CHECK.evaluate_graph(graph, cases)['failures'])

    def test_missing_subject_is_failure(self):
        """수집기가 유효한 SQL 객체를 빠뜨린 경우 빈 점수로 숨기지 않는다."""
        graph, cases = fixture()
        graph['vertices'] = [v for v in graph['vertices'] if v['id'] != 'dbo.acc_sample']
        self.assertTrue(CHECK.evaluate_graph(graph, cases)['failures'])

    def test_procedure_calls_use_collected_signature_without_guessing_overloads(self):
        """호출은 실제 routine ID로 연결하되 같은 이름의 복수 후보를 합치지 않는다."""
        graph, cases = fixture()
        cases['cases'] = [dict(name='sample', kind='procedure', read_scope='object',
                               reads=[], lineage={}, writes=[], calls=['dbo.acc_count'], state='complete')]
        graph['vertices'][3]['kind'] = 'procedure'
        graph['vertices'].append(dict(id='dbo.acc_count(integer)', name='acc_count', schema='dbo', kind='function'))
        graph['edges'] = [{'from': 'dbo.acc_sample', 'kind': 'calls', 'to': 'dbo.acc_count(integer)'}]
        self.assertEqual(CHECK.evaluate_graph(graph, cases)['failures'], [])
        graph['vertices'].append(dict(id='dbo.acc_count(text)', name='acc_count', schema='dbo', kind='function'))
        self.assertTrue(CHECK.evaluate_graph(graph, cases)['failures'])

    def test_trigger_firing_owner_is_required(self):
        """몸체 쓰기가 맞아도 잘못된 테이블에 귀속된 trigger를 통과시키지 않는다."""
        graph, cases = fixture()
        cases['cases'] = [dict(name='sample', kind='trigger', parent='Genre', read_scope='object',
                               reads=[], lineage={}, writes=[], calls=[], state='complete')]
        graph['vertices'][3]['kind'] = 'trigger'
        graph['edges'] = [{'from': 'dbo.acc_sample', 'kind': 'fires', 'to': 'dbo.Genre'}]
        self.assertEqual(CHECK.evaluate_graph(graph, cases)['failures'], [])
        graph['edges'] = []
        self.assertTrue(CHECK.evaluate_graph(graph, cases)['failures'])

    def test_oracle_catalog_relation_scope_does_not_claim_column_reference_coverage(self):
        """관계만 제공하는 참조에서도 객체를 비교하되 컬럼 간선을 오탐으로 세지 않는다."""
        graph, _cases = fixture()
        reference = dict(scope='object', source='USER_DEPENDENCIES', reads={'dbo.acc_sample': ['dbo.Genre']})
        result = CHECK.score_catalog(graph, reference)
        self.assertEqual(result['failures'], [])
        self.assertEqual(result['summary']['expected'], 1)

    def test_credential_bearing_artifact_is_refused_before_copy(self):
        """실패 재현용 산출물이 비밀번호를 외부 artifact로 옮기지 않게 한다."""
        with tempfile.TemporaryDirectory(prefix='sg-artifact-test-') as directory:
            work = Path(directory) / 'work'
            work.mkdir()
            source = work / 'catalog.json'
            destination = Path(directory) / 'artifacts'
            marker = 'synthetic-credential-marker'
            source.write_text('{"unexpected":"'+marker+'"}')
            with self.assertRaisesRegex(RuntimeError, 'credentials'):
                CHECK.preserve_artifact(source, destination, [marker])
            self.assertFalse(destination.exists())
            source.write_text('{"schemas":[]}')
            CHECK.preserve_artifact(source, destination, [marker])
            self.assertEqual((destination / source.name).read_text(), source.read_text())
            with self.assertRaises(FileExistsError):
                CHECK.preserve_artifact(source, destination, [marker])
            self.assertEqual(list(destination.iterdir()), [destination / source.name])

    def test_original_replay_parity_includes_analysis_and_provenance(self):
        """같은 정점·간선만으로 서로 다른 진단이나 원문 해시를 같다고 하지 않는다."""
        graph, _cases = fixture()
        other = {**graph, 'origins': [{'bodyHash': 'different'}]}
        with self.assertRaisesRegex(RuntimeError, 'replay graphs differ'):
            CHECK.compare_producers({'go': graph, 'jdbc': graph}, {'go': graph, 'jdbc': other})

    def test_producer_passwords_are_not_process_arguments(self):
        """임시 관리자 비밀번호가 Go URL이나 JDBC argv에 노출되지 않게 한다."""
        args = SimpleNamespace(go_probe=Path('go-probe'), jdbc_jar=Path('probe.jar'), oracle_jar=Path('oracle.jar'))
        sql = SimpleNamespace(url='jdbc:sqlserver://127.0.0.1:1234', user='sa', password='synthetic-password')
        commands, env, _redactions = CHECK.producer_commands(args, Path('java'), sql, {'host_port': 1234}, {'schema': 'dbo', 'dialect': 'sqlserver'})
        self.assertNotIn(sql.password, str(commands))
        self.assertEqual(env['SG_DB_PASSWORD'], sql.password)
        self.assertIn(sql.password, env['SG_ACCURACY_GO_URL'])

    def test_stop_failure_still_removes_only_the_verified_container_id(self):
        """정상 종료가 실패해도 다른 이름으로 대상을 바꾸지 않고 소유 ID를 정리한다."""
        removed = []
        def command(arguments, **_kwargs):
            if arguments[1] == 'stop':
                raise RuntimeError('stop timed out')
            self.assertEqual(arguments, ['docker', 'rm', '--force', '--volumes', 'owned-id'])
            removed.append(arguments[-1])
        with patch.object(CHECK, 'inspect_owned', return_value={'id': 'owned-id', 'running': True}), \
                patch.object(CHECK, 'run', side_effect=command):
            result = CHECK.cleanup_owned('fixture-name', 'owner')
        self.assertEqual(removed, ['owned-id'])
        self.assertTrue(result['removed'])
        self.assertTrue(result['forced_after_stop_failure'])

    def test_cleanup_reports_graceful_and_forced_failures(self):
        """정리마저 실패하면 앞선 종료 실패도 진단에서 잃지 않는다."""
        def command(arguments, **_kwargs):
            raise RuntimeError('stop failed' if arguments[1] == 'stop' else 'remove failed')
        with patch.object(CHECK, 'inspect_owned', return_value={'id': 'owned-id', 'running': True}), \
                patch.object(CHECK, 'run', side_effect=command):
            with self.assertRaisesRegex(RuntimeError, 'stop failed.*remove failed'):
                CHECK.cleanup_owned('fixture-name', 'owner')

    def test_validation_error_survives_a_cleanup_error(self):
        """DB 실패와 정리 실패를 함께 보고해 실제 최초 원인을 보존한다."""
        sql = SimpleNamespace(execute=lambda *_a, **_k: SimpleNamespace(returncode=0),
                              rows=lambda *_a: [{'version': 'fixture'}])
        record = {'id': 'owned-id', 'running': True, 'ports': {'1521/tcp': [{'HostIp': '127.0.0.1', 'HostPort': '1234'}]}, 'image': 'fixture'}
        settings = {'engines': {'oracle': {'image': 'fixture@sha256:'+'0'*64}}}
        with patch.object(CHECK, 'run', return_value=SimpleNamespace(returncode=0)), \
                patch.object(CHECK, 'Sql', return_value=sql), \
                patch.object(CHECK, 'inspect_owned', return_value=record), \
                patch.object(CHECK, 'cleanup_owned', side_effect=RuntimeError('cleanup failed')):
            with self.assertRaisesRegex(RuntimeError, 'validation failed; cleanup failed'):
                with CHECK.database('oracle', settings, Path('java'), 'classpath', Path('/unused')):
                    raise RuntimeError('validation failed')

    def test_foreign_container_ownership_is_rejected(self):
        """같은 이름의 다른 컨테이너에는 정리 권한을 부여하지 않는다."""
        result = subprocess.CompletedProcess([], 0, stdout='{"labels":{"'+CHECK.LABEL+'":"someone-else"}}', stderr='')
        with patch.object(CHECK, 'run', return_value=result):
            with self.assertRaisesRegex(RuntimeError, 'ownership differs'):
                CHECK.inspect_owned('fixture', 'owner')

    def test_failed_transport_input_is_preserved_before_engine_decode(self):
        """NDJSON 경로만 실패해도 그 입력을 CI artifact에서 재현할 수 있어야 한다."""
        graph, definition = fixture()
        definition['cases'][0]['sql'] = 'SELECT Name AS label FROM Genre WHERE GenreId > 0'
        native = {'schemas': [{'name': 'dbo', 'objects': [{'name': 'acc_sample', 'kind': 'view', 'body': 'SELECT Name FROM Genre'}], 'routines': []}]}
        with tempfile.TemporaryDirectory(prefix='sg-transport-artifact-test-') as directory:
            work = Path(directory) / 'work'
            work.mkdir()
            artifacts = Path(directory) / 'artifacts'
            args = SimpleNamespace(engine=Path('engine'), artifacts=artifacts)
            sql = SimpleNamespace(password='unit-credential-not-in-output')
            def command(arguments, **_kwargs):
                output = Path(arguments[arguments.index('-o')+1])
                if str(arguments[0]) == 'producer':
                    output.write_text('invalid-transport\n' if '.ndjson.' in output.name else json.dumps(native))
                elif '.ndjson.' in str(arguments[arguments.index('--document')+1]):
                    raise RuntimeError('transport decode failed')
                else:
                    output.write_text(json.dumps(graph))
                return SimpleNamespace(returncode=0)
            with patch.object(CHECK, 'producer_commands', return_value=({'go': ['producer']}, {}, ())), \
                    patch.object(CHECK, 'run', side_effect=command):
                with self.assertRaisesRegex(RuntimeError, 'transport decode failed'):
                    CHECK.analyze(args, Path('java'), sql, {}, definition,
                                  {'scope': 'object', 'source': 'test', 'reads': {}}, work, {})
            preserved = artifacts / 'sqlserver' / 'go.v1.ndjson.document'
            self.assertEqual(preserved.read_text(), 'invalid-transport\n')


if __name__ == '__main__':
    unittest.main()
