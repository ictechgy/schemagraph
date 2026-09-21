#!/usr/bin/env python3
"""로컬 H2로 정확도용 JDBC 실행기의 SQL 경계·JSON·비밀번호 보호를 검증한다."""
import argparse
import importlib.util
import json
import os
from pathlib import Path
import secrets
import tempfile
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[1]
SPEC = importlib.util.spec_from_file_location('extended_accuracy', ROOT/'Scripts/verify-sqlserver-oracle-accuracy.py')
CHECK = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(CHECK)


def main():
    """실제 JDBC를 사용하되 외부 DB나 네트워크를 요구하지 않는다."""
    parser = argparse.ArgumentParser(description='Verify JDBC SQL execution, JSON rows, fail-fast batches, and credential redaction on local H2.')
    parser.add_argument('--jdbc-jar', type=Path, required=True)
    parser.add_argument('--java')
    args = parser.parse_args()
    option_marker = 'fixture-option-' + secrets.token_hex(16)
    with patch.dict(os.environ, {key: '-Dfixture.token='+option_marker for key in
                               ('JAVA_TOOL_OPTIONS', 'JDK_JAVA_OPTIONS', '_JAVA_OPTIONS')}):
        java = CHECK.java_binary(args.java)
        versions = CHECK.run([java, '-version']).stderr + CHECK.run([java.with_name('javac'), '-version']).stdout
        if option_marker in versions or 'Picked up' in versions:
            raise RuntimeError('Java option values leaked into version provenance')
    with tempfile.TemporaryDirectory(prefix='schemagraph-accuracy-jdbc-test-') as directory:
        work = Path(directory)
        CHECK.run([java.with_name('javac'), '-d', work, ROOT/'Scripts/AccuracySql.java'])
        classpath = os.pathsep.join([str(work), str(args.jdbc_jar.resolve())])
        secret = secrets.token_hex(20)
        sql = CHECK.Sql(java, classpath, 'jdbc:h2:file:'+str(work/'sample'), 'sa', secret, work)
        sql.execute(['CREATE TABLE sample (id INT, label VARCHAR(100))', "INSERT INTO sample VALUES (1, 'hello')"])
        if sql.rows('SELECT id, label FROM sample') != [{'id': '1', 'label': 'hello'}]:
            raise RuntimeError('JDBC batches or JSON row values differ')
        values = sql.rows("SELECT CHAR(10) || CHAR(9) || CHAR(34) || CHAR(92) || CHAR(0) || '한글' AS escaped, CAST(NULL AS VARCHAR) AS absent")
        if values != [{'escaped': '\n\t"\\\x00한글', 'absent': None}]:
            raise RuntimeError('JDBC JSON string/null escaping is incorrect')
        failed = sql.execute(["SELECT '"+secret+"' FROM missing_table", 'CREATE TABLE should_not_execute (id INT)'], check=False)
        if failed.returncode != 1 or secret in failed.stderr or '<redacted>' not in failed.stderr:
            raise RuntimeError('Invalid SQL failure or credential redaction contract failed')
        if sql.rows("SELECT COUNT(*) AS observed FROM INFORMATION_SCHEMA.TABLES WHERE TABLE_NAME='SHOULD_NOT_EXECUTE'") != [{'observed': '0'}]:
            raise RuntimeError('JDBC executor continued after a failed SQL batch')
    print(json.dumps({'status': 'ok', 'checks': ['real JDBC batch/query', 'JSON control/Unicode/null values',
                                              'invalid SQL blocks later batches', 'credential redaction',
                                              'Java option values excluded from provenance']}))


if __name__ == '__main__':
    main()
