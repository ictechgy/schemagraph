#!/usr/bin/env python3
"""실제 제한 권한 PostgreSQL 역할에서 카탈로그 범위와 비교 실패를 확인한다."""

import argparse
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import subprocess
import tempfile
from urllib.parse import urlsplit

SPEC = importlib.util.spec_from_file_location('accuracy', Path(__file__).with_name('verify-accuracy.py'))
ACCURACY = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(ACCURACY)


def check(engine, bindir, go_probe=None, jdbc_jar=None, java=None):
    """데이터 권한이 없는 역할도 공개 메타데이터만 수집하는지 독립 DDL과 대조한다."""
    artifacts = {'engine': engine}
    if go_probe or jdbc_jar:
        artifacts['producer'] = go_probe or jdbc_jar
    digests = {key: hashlib.sha256(path.read_bytes()).hexdigest() for key, path in artifacts.items()}
    with tempfile.TemporaryDirectory(prefix='schemagraph-scope-') as temporary:
        directory = Path(temporary)
        with ACCURACY.postgres(directory, bindir) as (url, owner, environment):
            admin = list(owner)
            admin[admin.index('-U')+1] = 'postgres'
            admin[admin.index('-d')+1] = 'postgres'
            ACCURACY.run(admin+['-qc', 'CREATE ROLE collector LOGIN NOSUPERUSER NOCREATEDB NOCREATEROLE NOINHERIT'], env=environment)
            ACCURACY.run(owner+['-qc', '''
                CREATE SCHEMA locked;
                CREATE TABLE locked.parent (id integer PRIMARY KEY, value text NOT NULL);
                CREATE TABLE locked.child (id integer PRIMARY KEY, parent_id integer REFERENCES locked.parent(id));
                CREATE VIEW locked.parent_values AS SELECT id, value FROM locked.parent;
                CREATE FUNCTION locked.count_parents() RETURNS bigint LANGUAGE SQL
                    AS 'SELECT count(*) FROM locked.parent';
                REVOKE ALL ON SCHEMA locked FROM PUBLIC;
                REVOKE ALL ON ALL TABLES IN SCHEMA locked FROM PUBLIC;
                REVOKE ALL ON ALL FUNCTIONS IN SCHEMA locked FROM PUBLIC;
            '''], env=environment)
            limited = list(owner)
            limited[limited.index('-U')+1] = 'collector'
            for sql in ('SELECT * FROM locked.parent', 'CREATE TABLE public.forbidden (id integer)'):
                result = subprocess.run([str(x) for x in limited+['-qc', sql]], env=environment,
                                        capture_output=True, text=True, timeout=30)
                assert result.returncode != 0 and 'permission denied' in result.stderr, sql

            def scan(label, connection, schema='locked'):
                path = directory/(label+'.json')
                if go_probe:
                    probe_env = dict(environment, SG_SCOPE_URL=connection)
                    ACCURACY.run([go_probe, '--url-env', 'SG_SCOPE_URL', '--source-id', 'scope-check',
                                  '--schema', schema, '-o', path], env=probe_env)
                elif jdbc_jar:
                    parts = urlsplit(connection)
                    jdbc = f'jdbc:postgresql://{parts.hostname}:{parts.port}{parts.path}'
                    ACCURACY.run([java, '-jar', jdbc_jar, '--url', jdbc, '--user', parts.username,
                                  '--source-id', 'scope-check', '--schema', schema, '-o', path], env=environment)
                else:
                    ACCURACY.run([engine, 'scan', connection, '--source-id', 'scope-check', '--schema', schema,
                                  '--emit-document', path, '-o', directory/(label+'.graph.json')], env=environment)
                return path, json.loads(path.read_text())

            before, collected = scan('owner', url)
            denied_path, denied = scan('denied', url.replace('corpus@', 'collector@'))
            no_grants_complete = denied['context']['catalog_complete']
            if jdbc_jar:
                # pgjdbc의 pg_catalog 경로는 테이블 데이터 권한 없이도 메타데이터를 준다.
                assert no_grants_complete is True
                assert sum(len(s['objects']) for s in denied['schemas']) == 3
                assert sum(len(o['columns']) for s in denied['schemas'] for o in s['objects']) == 6
            else:
                assert no_grants_complete is False
                assert any('3 uncollected relations and 6 uncollected columns' in note for note in denied['limitations'])
            ACCURACY.run(owner+['-qc', '''
                GRANT USAGE ON SCHEMA locked TO collector;
                GRANT REFERENCES ON ALL TABLES IN SCHEMA locked TO collector;
            '''], env=environment)
            after, restricted = scan('restricted', url.replace('corpus@', 'collector@'))
            expected = {'parent': 2, 'child': 2, 'parent_values': 2}
            for value in (collected, restricted):
                assert value['context']['catalog_complete'] is True
                assert len(value['schemas']) == 1
                schema = value['schemas'][0]
                assert schema['name'] == 'locked'
                actual = {item['name']: len(item['columns']) for item in schema['objects']}
                assert actual == expected, actual
                objects = {item['name']: item for item in schema['objects']}
                assert objects['parent_values']['body']
                assert any(c['kind'] == 'fk' and c['referenced']['table'] == 'parent'
                           for c in objects['child']['constraints'])
                assert len(schema['routines']) == 1 and schema['routines'][0]['body']

            def review(old, new, expected_code):
                result = subprocess.run([str(engine), 'review', str(old), str(new), '--strict', '--require-complete'],
                                        env=environment, capture_output=True, text=True, timeout=30)
                assert result.returncode == expected_code, result.stderr
                return json.loads(result.stdout)

            matched = review(before, after, 0)
            assert matched['comparison'] == 'matched' and matched['totalChanges'] == 0
            denied_code = 0 if no_grants_complete else 2
            assert review(before, denied_path, denied_code)['comparison'] == ('matched' if no_grants_complete else 'unverified')
            for sql in ('SELECT * FROM locked.parent', 'CREATE TABLE public.forbidden (id integer)'):
                denied_sql = subprocess.run([str(x) for x in limited+['-qc', sql]], env=environment,
                                            capture_output=True, text=True, timeout=30)
                assert denied_sql.returncode != 0 and 'permission denied' in denied_sql.stderr
            missing, missing_doc = scan('missing', url.replace('corpus@', 'collector@'), 'absent')
            assert missing_doc['schemas'] == []
            assert missing_doc['context']['catalog_complete'] is False
            assert any('absent' in note for note in missing_doc['limitations'])
            assert review(missing, missing, 2)['comparison'] == 'unverified'
            ACCURACY.run(admin+['-qc', 'CREATE DATABASE second TEMPLATE corpus'], env=environment)
            other, other_doc = scan('other-database', url.rsplit('/', 1)[0]+'/second')
            assert other_doc['context']['database'] == 'second'
            mismatch = review(before, other, 2)
            assert any('database identities differ' in note for note in mismatch['comparisonNotes'])
            version = ACCURACY.run(owner+['-Atqc', 'SELECT version()'], env=environment).strip()
            assert digests == {key: hashlib.sha256(path.read_bytes()).hexdigest() for key, path in artifacts.items()}, 'test binaries changed during execution'
            return {'status': 'ok', 'server_version': version, 'reader': collected['reader'],
                    'role': 'NOSUPERUSER NOCREATEDB NOCREATEROLE NOINHERIT; schema USAGE and table REFERENCES only',
                    'data_select_denied': True, 'ddl_denied': True,
                    'catalog': {'tables': 2, 'views': 1, 'routines': 1, 'columns': 6, 'foreign_keys': 1},
                    'no_grants_catalog_complete': no_grants_complete,
                    'same_scope_review_exit': 0, 'no_grants_review_exit': denied_code,
                    'missing_schema_review_exit': 2, 'different_database_review_exit': 2,
                    'artifact_sha256': digests,
                    'engine_sha256': hashlib.sha256(engine.read_bytes()).hexdigest(),
                    'verifier_sha256': hashlib.sha256(Path(__file__).read_bytes()).hexdigest()}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--engine', type=Path, required=True)
    parser.add_argument('--postgres-bin', type=Path, default=Path(os.environ.get('PGBIN', '/opt/homebrew/opt/postgresql@16/bin')))
    parser.add_argument('--output', type=Path, required=True)
    producer = parser.add_mutually_exclusive_group()
    producer.add_argument('--go-probe', type=Path)
    producer.add_argument('--jdbc-jar', type=Path)
    parser.add_argument('--java', type=Path)
    args = parser.parse_args()
    if bool(args.jdbc_jar) != bool(args.java):
        parser.error('--jdbc-jar and --java must be used together')
    result = check(args.engine.resolve(), args.postgres_bin,
                   args.go_probe.resolve() if args.go_probe else None,
                   args.jdbc_jar.resolve() if args.jdbc_jar else None,
                   args.java.resolve() if args.java else None)
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(result, sort_keys=True, indent=2)+'\n')
    print(json.dumps(result, sort_keys=True))


if __name__ == '__main__':
    main()
