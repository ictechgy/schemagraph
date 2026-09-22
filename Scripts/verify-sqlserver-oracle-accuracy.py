#!/usr/bin/env python3
"""공개 SQL Server·Oracle 표본을 실제 DB와 독립 기대값으로 검증한다."""
from __future__ import annotations

import argparse
import copy
from contextlib import contextmanager
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import re
import secrets
import shutil
import subprocess
import sys
import tempfile
import time
from urllib.parse import quote
import uuid

ROOT = Path(__file__).resolve().parents[1]
CORPUS = ROOT / 'Fixtures/accuracy'
LABEL = 'io.github.ictechgy.schemagraph.extended-accuracy'
SPEC = importlib.util.spec_from_file_location('accuracy_reference', ROOT / 'Scripts/verify-accuracy.py')
ACCURACY = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(ACCURACY)
RELATIONS = {'table', 'view', 'materialized-view'}


def digest(path):
    """원본과 실행 도구의 식별자를 보고서에 남겨 이후 변경과 구분한다."""
    return hashlib.sha256(Path(path).read_bytes()).hexdigest()


def child_environment():
    """사용자의 Java 옵션이 검증 동작이나 버전 보고서에 섞이지 않게 한다."""
    return {key: value for key, value in os.environ.items()
            if key not in ('JAVA_TOOL_OPTIONS', 'JDK_JAVA_OPTIONS', '_JAVA_OPTIONS')}


def run(command, *, env=None, timeout=120, redactions=(), check=True):
    """실패에도 접속 인자를 출력하지 않고 모든 자식 실행에 상한을 둔다."""
    command = [str(value) for value in command]
    if env is None:
        env = child_environment()
    try:
        result = subprocess.run(command, capture_output=True, text=True, env=env, timeout=timeout)
    except subprocess.TimeoutExpired:
        raise RuntimeError(f'{Path(command[0]).name} timed out after {timeout}s') from None
    if check and result.returncode:
        detail = result.stderr[-2000:].strip()
        for value in redactions:
            if value:
                detail = detail.replace(value, '<redacted>')
        raise RuntimeError(f'{Path(command[0]).name} failed ({result.returncode}): {detail}')
    return result


def java_binary(value):
    """macOS java 스텁을 유효한 JDK로 오인하지 않는다."""
    candidates = [value] if value else [shutil.which('java'), '/opt/homebrew/opt/openjdk/bin/java',
                                      '/usr/local/opt/openjdk/bin/java']
    for candidate in candidates:
        if candidate and Path(candidate).is_file() and not run([candidate, '-version'], check=False).returncode:
            return Path(candidate).resolve()
    raise RuntimeError('A working JDK is required; install Java 17+ or pass --java')


class Sql:
    """DB 고유 JSON 함수 없이 같은 JDBC 도구로 검증·카탈로그 조회를 수행한다."""

    def __init__(self, java, classpath, url, user, password, work):
        self.java, self.classpath, self.url = java, classpath, url
        self.user, self.password, self.work = user, password, work

    def execute(self, statements, *, query=False, timeout=120, check=True):
        """원문 경계를 명시하고 비밀번호를 자식 환경으로만 전달한다."""
        path = self.work / 'query.sql'
        path.write_text('\n-- SG-ACCURACY-BATCH\n'.join(statements), encoding='utf-8')
        env = child_environment()
        env['SG_ACCURACY_JDBC_PASSWORD'] = self.password
        return run([self.java, '-cp', self.classpath, 'AccuracySql', self.url, self.user,
                    'query' if query else 'exec', path], env=env, timeout=timeout,
                   redactions=(self.password, self.url), check=check)

    def rows(self, statement):
        """카탈로그와 실제 루틴 결과를 별도의 정답 근거로 가져온다."""
        return json.loads(self.execute([statement], query=True).stdout)


def inspect_owned(name, owner):
    """환경 변수에 든 DB 비밀번호를 읽지 않고 소유권·실행 상태만 조회한다."""
    template = ('{"id":{{json .Id}},"running":{{json .State.Running}},'
                '"labels":{{json .Config.Labels}},"ports":{{json .NetworkSettings.Ports}},'
                '"image":{{json .Image}},"oom_killed":{{json .State.OOMKilled}},'
                '"exit_code":{{json .State.ExitCode}}}')
    result = run(['docker', 'inspect', '--format', template, name], check=False, timeout=10)
    if result.returncode:
        if 'No such' in result.stderr:
            return None
        raise RuntimeError('Cannot inspect the temporary accuracy database; check Docker availability')
    record = json.loads(result.stdout)
    if (record.get('labels') or {}).get(LABEL) != owner:
        raise RuntimeError('Temporary database ownership differs; refusing to modify it')
    return record


def cleanup_owned(name, owner):
    """정상 종료 실패 뒤에도 검증한 ID만 강제 정리하고 복구 여부를 기록한다."""
    record = inspect_owned(name, owner)
    if record is None:
        return {'removed': True, 'already_absent': True}
    identifier = record['id']
    stop_error = None
    if record['running']:
        try:
            run(['docker', 'stop', '--time', '15', identifier], timeout=30)
        except (RuntimeError, OSError) as error:
            stop_error = str(error)
    try:
        # 이름이 재사용돼도 새 컨테이너를 건드리지 않도록 확인한 ID를 조회한다.
        if inspect_owned(identifier, owner) is not None:
            run(['docker', 'rm', '--force', '--volumes', identifier], timeout=30)
    except (RuntimeError, OSError) as error:
        details = f'{stop_error}; forced removal failed: {error}' if stop_error else str(error)
        raise RuntimeError('Temporary database cleanup failed: ' + details) from None
    result = {'removed': True, 'forced_after_stop_failure': stop_error is not None}
    if stop_error:
        result['recovered_stop_error'] = stop_error
    return result


@contextmanager
def database(kind, settings, java, classpath, work):
    """기존 DB나 host mount 없이 루프백에 연결한 소유 컨테이너만 사용한다."""
    owner = uuid.uuid4().hex
    name = f'sg-accuracy-{kind}-{owner[:12]}'
    entry = settings['engines'][kind]
    if not re.fullmatch(r'.+@sha256:[0-9a-f]{64}', entry['image']):
        raise RuntimeError('Accuracy database images must be pinned by SHA-256 digest')
    if kind == 'sqlserver':
        architecture = run(['docker', 'info', '--format', '{{.OSType}}/{{.Architecture}}']).stdout.strip()
        if architecture not in ('linux/x86_64', 'linux/amd64'):
            raise RuntimeError('SQL Server validation requires Linux x86_64 Docker; use the GitHub accuracy workflow')
    # 고정 Oracle 이미지/JDBC 19.3 조합에서 40자 인증 실패를 재현했다.
    # 96비트 난수는 유지하면서 실제 접속으로 검증한 28자 ASCII 범위를 쓴다.
    password = 'Sg9!' + secrets.token_hex(12)
    port = 1433 if kind == 'sqlserver' else 1521
    env = dict(os.environ)
    # 삭제 책임은 finally의 소유 ID 정리에 모아 Docker --rm과 경쟁하지 않는다.
    command = ['docker', 'run', '--detach', '--name', name, '--label', LABEL+'='+owner,
               '--memory', entry.get('memory', '3g'), '--cpus', '2', '--publish', f'127.0.0.1::{port}']
    if kind == 'sqlserver':
        env['MSSQL_SA_PASSWORD'] = password
        command += ['--env', 'ACCEPT_EULA=Y', '--env', 'MSSQL_PID=Developer', '--env', 'MSSQL_SA_PASSWORD']
    else:
        env.update(ORACLE_PASSWORD=password, APP_USER_PASSWORD=password)
        command += ['--env', 'ORACLE_PASSWORD', '--env', 'APP_USER=SGACC', '--env', 'APP_USER_PASSWORD']
    command.append(entry['image'])
    failure = None
    cleanup = {}
    try:
        run(command, env=env, timeout=600, redactions=(password,))
        record = inspect_owned(name, owner)
        if record is None:
            raise RuntimeError('Temporary accuracy database disappeared immediately after startup')
        bindings = record['ports'].get(str(port)+'/tcp', [])
        if len(bindings) != 1 or bindings[0]['HostIp'] != '127.0.0.1':
            raise RuntimeError('Accuracy database must be published only on IPv4 loopback')
        host_port = int(bindings[0]['HostPort'])
        if kind == 'sqlserver':
            url = f'jdbc:sqlserver://127.0.0.1:{host_port};databaseName=master;encrypt=false;loginTimeout=5'
            user = 'sa'
        else:
            url = f'jdbc:oracle:thin:@//127.0.0.1:{host_port}/FREEPDB1'
            user = 'SGACC'
        sql = Sql(java, classpath, url, user, password, work)
        until = time.monotonic() + 240
        last_startup_error = ''
        while True:
            record = inspect_owned(name, owner)
            if record is None or not record['running']:
                raise RuntimeError(f'Temporary {kind} stopped during startup; check Docker memory and disk availability')
            if time.monotonic() >= until:
                raise RuntimeError(f'Temporary {kind} did not accept SQL within 240 seconds: {last_startup_error}')
            try:
                ready = sql.execute(['SELECT 1 AS ready' + (' FROM dual' if kind == 'oracle' else '')],
                                    query=True, timeout=min(10, max(1, until-time.monotonic())), check=False)
                if not ready.returncode:
                    break
                detail = ready.stderr.strip()[-1200:]
                for value in (password, quote(password, safe=''), url):
                    detail = detail.replace(value, '<redacted>')
                if detail != last_startup_error:
                    print(f'{kind}: JDBC startup pending: {detail}', file=sys.stderr, flush=True)
                last_startup_error = detail
            except RuntimeError:
                # JDBC 연결 자체의 지연도 전체 기동 기한 안에서만 재시도한다.
                if time.monotonic() >= until:
                    raise
            time.sleep(1)
        if kind == 'sqlserver':
            sql.execute(['CREATE DATABASE sgaccuracy'])
            sql.url = url.replace('databaseName=master', 'databaseName=sgaccuracy')
            version = sql.rows("SELECT CAST(SERVERPROPERTY('ProductVersion') AS NVARCHAR(100)) AS version, CAST(SERVERPROPERTY('Edition') AS NVARCHAR(100)) AS edition, CAST(SERVERPROPERTY('EngineEdition') AS NVARCHAR(100)) AS engine_edition")
            if any('Edge' in str(row) for row in version):
                raise RuntimeError('Azure SQL Edge cannot stand in for the SQL Server accuracy environment')
        else:
            version = sql.rows('SELECT banner AS version FROM v$version ORDER BY banner')
        yield sql, {'image_reference': entry['image'], 'image_id': record['image'],
                    'version': version, 'host_port': host_port, 'cleanup': cleanup}
    except (RuntimeError, OSError, ValueError) as error:
        failure = error
        raise
    finally:
        try:
            cleanup.update(cleanup_owned(name, owner))
        except (RuntimeError, OSError) as error:
            prefix = f'{failure}; ' if failure is not None else ''
            raise RuntimeError(prefix + str(error)) from None


def folded(name, definition):
    """Oracle의 미인용 이름 접힘만 적용하고 SQL Server의 수집 이름은 보존한다."""
    return name.upper() if definition['dialect'] == 'oracle' else name


def find_subject(graph, case, definition):
    """독립 SQL 이름이 실제로 수집된 단일 정점인지 확인하고 reader의 ID를 사용한다."""
    name = folded('acc_' + case['name'], definition)
    hits = [v['id'] for v in graph['vertices'] if v.get('schema') == definition['schema']
            and v.get('name') == name and v.get('kind') == case['kind']]
    if len(hits) != 1:
        raise RuntimeError(f'{definition["schema"]}.{name}: expected one collected {case["kind"]}, found {len(hits)}')
    return hits[0]


def call_name(target, vertices):
    """타입 시그니처 표기는 비교에서 분리하되 모호한 overload는 정답으로 합치지 않는다."""
    vertex = vertices.get(target)
    if not vertex or vertex['kind'] not in ('function', 'procedure', 'package'):
        return 'invalid-call-target:' + target
    logical = vertex.get('schema', '') + '.' + vertex['name']
    count = sum(v.get('schema') == vertex.get('schema') and v.get('name') == vertex['name']
                and v['kind'] in ('function', 'procedure', 'package') for v in vertices.values())
    return logical if count == 1 else 'ambiguous-call-target:' + logical


def inspect_case(graph, case, definition, vertices):
    """뷰의 컬럼 사실과 절차형 객체 사실을 별도 범위에서 채점한다."""
    subject = find_subject(graph, case, definition)
    edges = [edge for edge in graph['edges'] if edge['from'] == subject]
    reads = {e['to'] for e in edges if e['kind'] == 'reads'
             and (case['read_scope'] == 'column' or vertices.get(e['to'], {}).get('kind') in RELATIONS)}
    lineage = {(e['from'][len(subject)+1:], e['to']) for e in graph['edges']
               if e['kind'] == 'derives-from' and e['from'].startswith(subject + '.')}
    expected_lineage = {(output, source) for output, sources in case['lineage'].items() for source in sources}
    analysis = next((item for item in graph.get('analysis', []) if item['id'] == subject), {})
    column_writes = {e['to'] for e in edges if e['kind'] == 'writes'
                     and vertices.get(e['to'], {}).get('kind') == 'column'}
    writes = {e['to'] for e in edges if e['kind'] == 'writes'
              and (case['read_scope'] != 'object' or e['to'] not in column_writes)}
    result = {'subject': subject, 'kind': case['kind'], 'read_scope': case['read_scope'],
              'state': analysis.get('state', 'unavailable'), 'expected_state': case['state'],
              'diagnostics': analysis.get('diagnostics', []),
              'reads': ACCURACY.score(set(case['reads']), reads),
              'writes': ACCURACY.score(set(case['writes']), writes),
              'calls': ACCURACY.score(set(case['calls']), {call_name(e['to'], vertices) for e in edges if e['kind'] == 'calls'})}
    if case['kind'] == 'view':
        result['lineage'] = ACCURACY.score(expected_lineage, lineage)
    else:
        # 기존 코퍼스의 절차형 기대값은 객체 단위다. 새 컬럼 사실은 새 DML
        # 코퍼스에서 평가하며 이 점수의 오탐/정답으로 섞지 않는다.
        result['write_scope'] = 'object'
        result['column_writes_unscored'] = sorted(column_writes)
    if case['kind'] == 'trigger':
        result['fires'] = ACCURACY.score({definition['schema'] + '.' + case['parent']},
                                        {e['to'] for e in edges if e['kind'] == 'fires'})
    return result


def evaluate_graph(graph, definition):
    """수용된 SQL의 독립 기대값과 그래프 사실을 대조한다."""
    vertices = {v['id']: v for v in graph['vertices']}
    ghosts = [edge for edge in graph['edges'] if edge['from'] not in vertices or edge['to'] not in vertices]
    failures = ['phantom graph endpoints'] if ghosts else []
    if len(vertices) != len(graph['vertices']):
        failures.append('duplicate graph vertex identities')
    checked, false_complete = [], []
    categories = ('reads', 'lineage', 'writes', 'calls', 'fires')
    for case in definition['cases']:
        try:
            result = inspect_case(graph, case, definition, vertices)
        except RuntimeError as error:
            failures.append(str(error))
            continue
        checked.append(result)
        mismatch = False
        for category in categories:
            if category in result and (result[category]['missing'] or result[category]['unexpected']):
                mismatch = True
                failures.append(result['subject'] + ': ' + category)
        if mismatch and result['state'] == 'complete':
            false_complete.append(result['subject'])
        if result['state'] != result['expected_state']:
            failures.append(result['subject'] + ': analysis state')
    summary = {kind: {category: ACCURACY.totals([c[category] for c in checked
                                               if c['kind'] == kind and category in c])
                      for category in categories}
               for kind in sorted({c['kind'] for c in checked})}
    return {'vertices': len(vertices), 'edges': len(graph['edges']), 'ghosts': ghosts,
            'cases': checked, 'summary': summary, 'false_complete_cases': sorted(false_complete),
            'failures': sorted(failures)}


def load_definition(path, kind):
    """실행할 SQL 객체 이름과 점수 범위를 분석 전에 검증한다."""
    definition = json.loads(path.read_text())
    if definition['dialect'] != kind or definition['schema'] != ('dbo' if kind == 'sqlserver' else 'SGACC'):
        raise RuntimeError('Case file dialect/schema does not match the owned database')
    names = [case['name'] for case in definition['cases']]
    if not names or len(names) != len(set(names)) or any(not re.fullmatch(r'[a-z][a-z0-9_]{0,23}', name) for name in names):
        raise RuntimeError('Accuracy cases need nonempty, unique SQL-safe names')
    for case in definition['cases']:
        if case['kind'] not in ('view', 'function', 'procedure', 'trigger'):
            raise RuntimeError('Unsupported accuracy case kind')
        if case['read_scope'] != ('column' if case['kind'] == 'view' else 'object'):
            raise RuntimeError('Views score column reads; procedural cases score direct object reads')
        if not case['sql'].strip() or case['state'] not in ('complete', 'partial', 'unsupported'):
            raise RuntimeError('Every accuracy case needs SQL and an explicit expected analysis state')
    required = {'rename_genre': 'procedure', 'copy_genres': 'procedure', 'genre_count': 'function',
                'call_count': 'procedure', 'genre_insert': 'trigger', 'genre_update': 'trigger'}
    supplied = {case['name']: case['kind'] for case in definition['cases']}
    if any(supplied.get(name) != kind for name, kind in required.items()):
        raise RuntimeError('Case variants must preserve the six fixed Genre runtime modules and their kinds')
    return definition


def audit_tables(kind):
    """실행 결과를 공개 데이터와 구분되는 합성 감사 테이블에 기록한다."""
    integer, text = ('INT', 'NVARCHAR(120)') if kind == 'sqlserver' else ('NUMBER', 'VARCHAR2(120)')
    return [f'CREATE TABLE ACC_GENRE_AUDIT (GenreId {integer}, GenreName {text})',
            f'CREATE TABLE ACC_GENRE_CHANGES (GenreId {integer}, OldName {text}, NewName {text})',
            f'CREATE TABLE ACC_GENRE_COUNT_LOG (ObservedCount {integer})']


def validate_sql(sql, definition):
    """뷰 바인딩과 저장 객체 컴파일을 실제 DB에서 검사한 뒤에만 분석을 허용한다."""
    kind, schema = definition['dialect'], definition['schema']
    schema_path = CORPUS / f'chinook-{kind}/schema.sql'
    print(f'{kind}: applying pinned schema', flush=True)
    sql.execute([schema_path.read_text(encoding='utf-8')])
    sql.execute(audit_tables(kind))
    # 함수가 procedure의 정적 호출 대상이므로 실제 생성 순서를 먼저 고정한다.
    order = {'function': 0, 'procedure': 1, 'trigger': 2, 'view': 3}
    for case in sorted(definition['cases'], key=lambda case: order[case['kind']]):
        print(f'{kind}: validating {case["kind"]} acc_{case["name"]}', flush=True)
        statement = case['sql']
        if case['kind'] == 'view':
            statement = 'CREATE VIEW ' + schema + '.' + folded('acc_'+case['name'], definition) + ' AS ' + statement
        try:
            sql.execute([statement])
            if case['kind'] == 'view':
                sql.rows('SELECT * FROM ' + schema + '.' + folded('acc_'+case['name'], definition) + ' WHERE 1=0')
        except RuntimeError as error:
            raise RuntimeError(f'{kind}/{case["name"]}: SQL validation failed: {error}') from None
    if kind == 'oracle':
        errors = sql.rows("SELECT name, type, line, position, text FROM user_errors WHERE SUBSTR(name,1,4)='ACC_' ORDER BY name, sequence")
        if errors:
            raise RuntimeError('Oracle stored objects compiled with errors: ' + json.dumps(errors, sort_keys=True))


def verify_runtime(sql, definition):
    """새 절차·함수·트리거의 실제 행 효과를 독립 기대값과 비교한다."""
    sql.execute(["INSERT INTO Genre (GenreId, Name) VALUES (9001, 'Before')",
                 "INSERT INTO Genre (GenreId, Name) VALUES (9002, 'Second')"])
    if definition['dialect'] == 'sqlserver':
        sql.execute(["EXEC dbo.acc_rename_genre @genre_id=9001, @genre_name=N'After'",
                     'EXEC dbo.acc_copy_genres', 'EXEC dbo.acc_call_count'])
        count = sql.rows('SELECT dbo.acc_genre_count() AS observed')
    else:
        sql.execute(["BEGIN acc_rename_genre(9001, 'After'); acc_copy_genres; acc_call_count; END;"])
        count = sql.rows('SELECT acc_genre_count() AS observed FROM dual')
    genres = sql.rows('SELECT GenreId AS genre_id, Name AS genre_name FROM Genre ORDER BY GenreId')
    audit = sql.rows('SELECT GenreId AS genre_id, GenreName AS genre_name FROM ACC_GENRE_AUDIT ORDER BY GenreId, GenreName')
    changes = sql.rows('SELECT GenreId AS genre_id, OldName AS old_name, NewName AS new_name FROM ACC_GENRE_CHANGES ORDER BY GenreId')
    observed = sql.rows('SELECT ObservedCount AS observed FROM ACC_GENRE_COUNT_LOG')
    expected_genres = [{'genre_id': '9001', 'genre_name': 'After'}, {'genre_id': '9002', 'genre_name': 'Second'}]
    expected_audit = [expected_genres[0], {'genre_id': '9001', 'genre_name': 'Before'}, expected_genres[1], expected_genres[1]]
    expected_changes = [{'genre_id': '9001', 'old_name': 'Before', 'new_name': 'After'}]
    if genres != expected_genres or audit != expected_audit or changes != expected_changes or count != [{'observed': '2'}] or observed != count:
        raise RuntimeError('Routine/trigger execution did not produce the independently expected rows')
    return {'genre_rows': genres, 'insert_and_copy_audit': audit, 'update_audit': changes,
            'function_result': count, 'procedure_call_result': observed}


def catalog_reference(sql, definition):
    """입력 catalog에 넣지 않은 DB 의존성을 뷰의 독립 참조로 수집한다."""
    reference = {}
    for case in definition['cases']:
        if case['kind'] != 'view':
            continue
        name = folded('acc_'+case['name'], definition)
        subject = definition['schema'] + '.' + name
        if definition['dialect'] == 'sqlserver':
            rows = sql.rows("SELECT COALESCE(r.referenced_schema_name, OBJECT_SCHEMA_NAME(r.referenced_id)) AS target_schema, r.referenced_entity_name AS target_name, r.referenced_minor_name AS column_name FROM sys.dm_sql_referenced_entities('" + subject + "', 'OBJECT') r JOIN sys.objects o ON o.object_id=r.referenced_id WHERE o.type IN ('U','V')")
        else:
            rows = sql.rows("SELECT referenced_owner AS target_schema, referenced_name AS target_name FROM user_dependencies WHERE name='" + name + "' AND type='VIEW' AND referenced_owner=USER AND referenced_type IN ('TABLE','VIEW') ORDER BY referenced_owner, referenced_name")
        targets = set()
        for row in rows:
            target = row['target_schema'] + '.' + row['target_name']
            targets.add(target)
            if row.get('column_name'):
                targets.add(target + '.' + row['column_name'])
        reference[subject] = sorted(targets)
    return {'available': True, 'scope': 'column' if definition['dialect'] == 'sqlserver' else 'object',
            'source': 'sys.dm_sql_referenced_entities' if definition['dialect'] == 'sqlserver' else 'USER_DEPENDENCIES',
            'reads': reference}


def score_catalog(graph, reference):
    """컬럼을 제공하지 않는 Oracle 참조를 빈 컬럼 집합으로 채점하지 않는다."""
    kinds = {vertex['id']: vertex['kind'] for vertex in graph['vertices']}
    items = []
    for subject, expected in sorted(reference['reads'].items()):
        actual = {edge['to'] for edge in graph['edges'] if edge['from'] == subject and edge['kind'] == 'reads'
                  and (reference['scope'] == 'column' or kinds.get(edge['to']) in RELATIONS)}
        items.append({'subject': subject, **ACCURACY.score(set(expected), actual)})
    return {'scope': reference['scope'], 'source': reference['source'], 'cases': items,
            'summary': ACCURACY.totals(items),
            'failures': [item['subject']+': catalog reads' for item in items if item['missing'] or item['unexpected']]}


def original_document(native, definition):
    """DB가 확인한 객체·컬럼을 보존하며 검토 대상의 SQL만 작성 원문으로 돌린다."""
    document = copy.deepcopy(native)
    definitions = {folded('acc_'+case['name'], definition): case for case in definition['cases']}
    replaced = set()
    for schema in document['schemas']:
        if schema['name'] != definition['schema']:
            continue
        for obj in schema['objects']:
            case = definitions.get(obj['name'])
            if case and case['kind'] == 'view':
                obj['body'] = case['sql']
                replaced.add(obj['name'])
            for trigger in obj.get('triggers', []):
                case = definitions.get(trigger['name'])
                if case and case['kind'] == 'trigger':
                    trigger['body'] = case['sql']
                    replaced.add(trigger['name'])
        for routine in schema.get('routines', []):
            case = definitions.get(routine['name'])
            if case and case['kind'] in ('function', 'procedure'):
                routine['body'] = case['sql']
                replaced.add(routine['name'])
    if replaced != set(definitions):
        raise RuntimeError('Collector omitted accepted accuracy objects: ' + ', '.join(sorted(set(definitions)-replaced)))
    return document


def producer_commands(args, java, sql, server, definition):
    """같은 폐기용 DB를 두 프로브에 제공하고 보고서에는 접속 인자를 남기지 않는다."""
    schema, kind = definition['schema'], definition['dialect']
    if kind == 'sqlserver':
        go_url = f'sqlserver://sa:{quote(sql.password, safe="")}@127.0.0.1:{server["host_port"]}?database=sgaccuracy&encrypt=disable'
    else:
        go_url = f'oracle://SGACC:{quote(sql.password, safe="")}@127.0.0.1:{server["host_port"]}/FREEPDB1'
    jdbc = [java, '-jar', args.jdbc_jar, '--url', sql.url, '--user', sql.user, '--schema', schema]
    if kind == 'oracle':
        jdbc += ['--driver', args.oracle_jar]
    env = child_environment()
    env.update(SG_ACCURACY_GO_URL=go_url, SG_DB_PASSWORD=sql.password)
    commands = {'go': [args.go_probe, '--url-env', 'SG_ACCURACY_GO_URL', '--schema', schema], 'jdbc': jdbc}
    return commands, env, (sql.password, quote(sql.password, safe=''), go_url, sql.url)


def preserve_artifact(source, destination, secrets_to_exclude):
    """공개 합성 fixture만 보관하고 접속 비밀번호가 섞인 산출물은 거부한다."""
    if destination is None:
        return
    if not source.is_file() or source.is_symlink():
        raise RuntimeError('Only regular generated fixture files may be preserved')
    if destination.resolve().is_relative_to(source.parent.resolve()):
        raise RuntimeError('Artifact destination must be outside temporary fixture work')
    content = source.read_text(encoding='utf-8')
    if any(value and value in content for value in secrets_to_exclude):
        raise RuntimeError('Fixture output contains connection credentials; refusing to preserve it')
    destination.mkdir(parents=True, exist_ok=True)
    temporary = None
    try:
        with tempfile.NamedTemporaryFile(mode='w', encoding='utf-8', dir=destination, delete=False) as output:
            temporary = Path(output.name)
            output.write(content)
        # 완성된 파일만 공개하고 기존 첫 평가 artifact는 덮어쓰지 않는다.
        os.link(temporary, destination / source.name)
    finally:
        if temporary is not None:
            temporary.unlink()


def compare_producers(native, replay):
    """같은 원문을 넣었을 때 provenance·진단·metadata까지 같은지 검증한다."""
    if any(native['go'][field] != native['jdbc'][field] for field in ('vertices', 'edges')):
        raise RuntimeError('Go and JDBC native corpus graph vertices/edges differ')
    if replay['go'] != replay['jdbc']:
        raise RuntimeError('Go and JDBC original-SQL replay graphs differ (including analysis/provenance/metadata)')
    native_differences = sorted(key for key in set(native['go']) | set(native['jdbc'])
                                if native['go'].get(key) != native['jdbc'].get(key))
    return {'original_sql_full_graph_equal': True, 'native_vertices_edges_equal': True,
            'native_other_field_differences': native_differences}


def analyze(args, java, sql, server, definition, reference, work, reports):
    """첫 평가·원문/DB 정규화 SQL·생산자/전송 차이를 각각 보존한다."""
    commands, producer_env, redactions = producer_commands(args, java, sql, server, definition)
    artifacts = args.artifacts / definition['dialect'] if args.artifacts else None
    secret_values = (sql.password, quote(sql.password, safe=''))
    graphs, replay_graphs = {}, {}
    for producer, command in commands.items():
        entry = reports[producer] = {'transport_checks': [], 'analysis': {}}
        native_path = work / f'{producer}.native.json'
        run(command + ['--document-version', '1', '--format', 'json', '-o', native_path], env=producer_env, redactions=redactions)
        preserve_artifact(native_path, artifacts, secret_values)
        native = json.loads(native_path.read_text())
        for mode in ('native', 'original'):
            document = native if mode == 'native' else original_document(native, definition)
            path = work / f'{producer}.{mode}.document.json'
            path.write_text(json.dumps(document, sort_keys=True), encoding='utf-8')
            preserve_artifact(path, artifacts, secret_values)
            output = work / f'{producer}.{mode}.graph.json'
            run([args.engine, 'scan', '--document', path, '-o', output])
            preserve_artifact(output, artifacts, secret_values)
            graph = json.loads(output.read_text())
            scored = evaluate_graph(graph, definition)
            catalog = score_catalog(graph, reference)
            scored['catalog_reference'] = catalog
            scored['failures'].extend(catalog['failures'])
            scored['failures'].sort()
            scored['document_sha256'] = digest(path)
            scored['graph_sha256'] = digest(output)
            entry['analysis'][mode] = scored
            if mode == 'native':
                graphs[producer] = graph
            else:
                replay_graphs[producer] = graph
        for version in (1, 2):
            for format_name in ('json', 'ndjson'):
                path = work / f'{producer}.v{version}.{format_name}.document'
                output = work / f'{producer}.v{version}.{format_name}.graph.json'
                run(command + ['--document-version', str(version), '--format', format_name, '-o', path], env=producer_env, redactions=redactions)
                preserve_artifact(path, artifacts, secret_values)
                run([args.engine, 'scan', '--document', path, '-o', output])
                preserve_artifact(output, artifacts, secret_values)
                graph = json.loads(output.read_text())
                if graph != graphs[producer]:
                    raise RuntimeError(f'{producer} v{version}/{format_name} graph differs from native v1/json')
                entry['transport_checks'].append({'version': version, 'format': format_name, 'graph_sha256': digest(output)})
    return compare_producers(graphs, replay_graphs)


def main():
    """SQL 검증 실패도 보고서에 남기며 불완전한 평가는 성공으로 종료하지 않는다."""
    parser = argparse.ArgumentParser(description='Validate SQL Server/Oracle public schemas and stored SQL against independent references.')
    parser.add_argument('--engine', type=Path)
    parser.add_argument('--go-probe', type=Path)
    parser.add_argument('--jdbc-jar', type=Path, required=True)
    parser.add_argument('--oracle-jar', type=Path)
    parser.add_argument('--java')
    parser.add_argument('--database', choices=('sqlserver', 'oracle', 'all'), default='all')
    parser.add_argument('--cases', type=Path, help='case variant for one database; must preserve the six fixed Genre runtime modules and effects')
    parser.add_argument('--environments', type=Path, default=CORPUS/'sqlserver-oracle-environments.json')
    parser.add_argument('--validate-only', action='store_true')
    parser.add_argument('--strict', action='store_true')
    parser.add_argument('--artifacts', type=Path, help='preserve checked public-corpus catalogs/graphs for failure reproduction')
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    if not args.validate_only and (args.engine is None or args.go_probe is None):
        parser.error('--engine and --go-probe are required unless --validate-only is selected')
    if args.cases and args.database == 'all':
        parser.error('--cases requires one --database')
    if args.database in ('oracle', 'all') and args.oracle_jar is None:
        parser.error('--oracle-jar is required for Oracle')
    if args.artifacts:
        args.artifacts = args.artifacts.resolve()
        if args.artifacts.exists():
            parser.error('--artifacts must point to a new directory; existing evaluation evidence is not overwritten')
    for attribute in ('jdbc_jar', 'oracle_jar', 'engine', 'go_probe'):
        value = getattr(args, attribute)
        if value is not None:
            value = value.resolve()
            if not value.is_file():
                parser.error(f'--{attribute.replace("_", "-")} must point to an existing file')
            setattr(args, attribute, value)
    manifest = ACCURACY.verify_sources()
    settings = json.loads(args.environments.read_text())
    java = java_binary(args.java)
    report = {'report_version': 1, 'validate_only': args.validate_only,
              'case_design_commit': '8e53c4fa2bd2d13b55ed37cf41c959629177a45f',
              'verifier_sha256': digest(Path(__file__)), 'jdbc_utility_sha256': digest(ROOT/'Scripts/AccuracySql.java'),
              'environments_sha256': digest(args.environments), 'results': {},
              'sources': [s for s in manifest['datasets'] if s['name'] in ('chinook-sqlserver', 'chinook-oracle')],
              'producers': {'jdbc_sha256': digest(args.jdbc_jar)}}
    report['java_version'] = run([java, '-version']).stderr.strip()
    report['javac_version'] = run([java.with_name('javac'), '-version']).stdout.strip()
    if args.oracle_jar:
        report['producers']['oracle_driver_sha256'] = digest(args.oracle_jar)
    if args.go_probe:
        report['producers']['go_sha256'] = digest(args.go_probe)
    if args.engine:
        report['engine'] = {'version': run([args.engine, '--version']).stdout.strip(), 'sha256': digest(args.engine)}
    try:
        with tempfile.TemporaryDirectory(prefix='schemagraph-extended-accuracy-') as directory:
            work = Path(directory)
            classes = work / 'classes'
            classes.mkdir()
            run([java.with_name('javac'), '-d', classes, ROOT/'Scripts/AccuracySql.java'])
            jars = [str(classes), str(args.jdbc_jar)] + ([str(args.oracle_jar)] if args.oracle_jar else [])
            classpath = os.pathsep.join(jars)
            for kind in ('sqlserver', 'oracle'):
                if args.database not in ('all', kind):
                    continue
                path = args.cases or CORPUS/f'chinook-{kind}-cases.json'
                definition = load_definition(path, kind)
                entry = report['results'][kind] = {'cases_sha256': digest(path), 'case_count': len(definition['cases']),
                                                   'cases_source': 'override-with-fixed-runtime-cohort' if args.cases else 'tracked-corpus',
                                                   'sql_validated': False, 'analysis': {}}
                try:
                    with database(kind, settings, java, classpath, work) as (sql, server):
                        entry['server'] = server
                        validate_sql(sql, definition)
                        entry['runtime'] = verify_runtime(sql, definition)
                        entry['sql_validated'] = True
                        reference = entry['catalog_reference'] = catalog_reference(sql, definition)
                        if not args.validate_only:
                            entry['producer_comparison'] = analyze(args, java, sql, server, definition, reference, work, entry['analysis'])
                except (RuntimeError, OSError, ValueError) as error:
                    entry['error'] = str(error)
                failures = sum(len(mode['failures']) for producer in entry['analysis'].values()
                               for mode in producer['analysis'].values())
                print(f'{kind}: {entry["case_count"]} cases, SQL validation {"passed" if entry["sql_validated"] else "failed"}, {failures} accuracy checks failed, operational error {"yes" if "error" in entry else "no"}')
    except (RuntimeError, OSError, ValueError) as error:
        report['error'] = str(error)
    finally:
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(json.dumps(report, indent=2, sort_keys=True)+'\n', encoding='utf-8')
    operational_failure = 'error' in report or any('error' in entry or not entry['sql_validated'] for entry in report['results'].values())
    accuracy_failure = any(mode['failures'] for entry in report['results'].values()
                           for producer in entry['analysis'].values() for mode in producer['analysis'].values())
    return 1 if operational_failure or (args.strict and accuracy_failure) else 0


if __name__ == '__main__':
    raise SystemExit(main())
