#!/usr/bin/env python3
"""Real SQLite replay regression and optional built-engine integration."""
import argparse
from contextlib import closing
import importlib.util
import json
from pathlib import Path
import sqlite3
import tempfile
import unittest
from unittest.mock import patch

SPEC = importlib.util.spec_from_file_location('capture', Path(__file__).with_name('capture-migration.py'))
CAPTURE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(CAPTURE)
ENGINE = None


class WithoutExtensionAPI(sqlite3.Connection):
    """선택적으로 빌드되는 확장 API가 없는 실제 SQLite 연결을 대조한다."""

    def __getattribute__(self, name):
        if name == 'enable_load_extension':
            raise AttributeError(name)
        return super().__getattribute__(name)


class CaptureTests(unittest.TestCase):
    def test_real_type_nullability_constraint_and_rename_replay(self):
        with tempfile.TemporaryDirectory() as folder:
            database = Path(folder) / 'fixture.sqlite'
            CAPTURE.apply_sql(database, 'CREATE TABLE users(id INTEGER PRIMARY KEY, email TEXT NOT NULL UNIQUE);')
            CAPTURE.apply_sql(database, 'ALTER TABLE users RENAME TO people; ALTER TABLE people ADD COLUMN nickname TEXT;')
            with closing(sqlite3.connect(database)) as connection:
                columns = {row[1]: row for row in connection.execute('PRAGMA table_info(people)')}
                self.assertEqual(columns['email'][2:4], ('TEXT', 1))
                self.assertEqual(columns['nickname'][2:4], ('TEXT', 0))
                self.assertTrue(connection.execute('PRAGMA index_list(people)').fetchall())

    def test_cannot_attach_or_vacuum_into_another_file(self):
        with tempfile.TemporaryDirectory() as folder:
            database, outside = Path(folder) / 'fixture.sqlite', Path(folder) / 'outside.sqlite'
            for sql in [f"ATTACH DATABASE '{outside.as_posix()}' AS extra;", f"VACUUM INTO '{outside.as_posix()}';"]:
                with self.assertRaises(sqlite3.DatabaseError):
                    CAPTURE.apply_sql(database, sql)
                self.assertFalse(outside.exists())

    def test_missing_extension_api_preserves_real_ddl_and_local_pragma(self):
        original_connect = sqlite3.connect
        with tempfile.TemporaryDirectory() as folder:
            database = Path(folder) / 'fixture.sqlite'
            def connect(*args, **kwargs):
                return original_connect(*args, factory=WithoutExtensionAPI, **kwargs)
            with patch.object(CAPTURE.sqlite3, 'connect', side_effect=connect):
                CAPTURE.apply_sql(database, 'PRAGMA user_version = 7; CREATE TABLE people(id INTEGER PRIMARY KEY, email TEXT NOT NULL);')
                CAPTURE.apply_sql(database, 'ALTER TABLE people RENAME COLUMN email TO address;')
            with closing(original_connect(database)) as connection:
                self.assertEqual(connection.execute('PRAGMA user_version').fetchone(), (7,))
                self.assertEqual([row[1] for row in connection.execute('PRAGMA table_info(people)')], ['id', 'address'])

    def test_sql_extension_and_external_database_guards_with_optional_api(self):
        original_connect = sqlite3.connect
        modes = [WithoutExtensionAPI]
        if hasattr(sqlite3.Connection, 'enable_load_extension'):
            modes.append(sqlite3.Connection)
        with tempfile.TemporaryDirectory() as folder:
            database, outside = Path(folder) / 'fixture.sqlite', Path(folder) / 'outside.sqlite'
            calls = []
            for factory in modes:
                def connect(*args, **kwargs):
                    connection = original_connect(*args, factory=factory, **kwargs)
                    # 실제 라이브러리 로딩 대신 무해한 함수로 준비 단계의 거부를 확인한다.
                    connection.create_function('load_extension', 1, lambda value: calls.append(value))
                    return connection
                with patch.object(CAPTURE.sqlite3, 'connect', side_effect=connect):
                    for sql in ["SELECT load_extension('not-loaded');", "SELECT LOAD_EXTENSION('not-loaded');",
                                "SELECT \"load_extension\"('not-loaded');", f"ATTACH DATABASE '{outside.as_posix()}' AS extra;",
                                'DETACH DATABASE main;', f"VACUUM INTO '{outside.as_posix()}';"]:
                        with self.subTest(api=factory.__name__, sql=sql), self.assertRaises(sqlite3.DatabaseError):
                            CAPTURE.apply_sql(database, sql)
                self.assertEqual(calls, [])
                self.assertFalse(outside.exists())

    def test_present_extension_api_disable_failure_stops_before_sql(self):
        calls = []
        class DisableFails(sqlite3.Connection):
            def enable_load_extension(self, enabled):
                calls.append(enabled)
                raise sqlite3.OperationalError('extension disable failed')
        original_connect = sqlite3.connect
        with tempfile.TemporaryDirectory() as folder:
            database = Path(folder) / 'fixture.sqlite'
            def connect(*args, **kwargs):
                return original_connect(*args, factory=DisableFails, **kwargs)
            with patch.object(CAPTURE.sqlite3, 'connect', side_effect=connect):
                with self.assertRaisesRegex(sqlite3.OperationalError, 'extension disable failed'):
                    CAPTURE.apply_sql(database, 'CREATE TABLE must_not_execute(id INTEGER);')
            self.assertEqual(calls, [False])
            with closing(original_connect(database)) as connection:
                self.assertEqual(connection.execute('SELECT name FROM sqlite_schema').fetchall(), [])

    @unittest.skipUnless(hasattr(sqlite3.Connection, 'enable_load_extension'), 'SQLite build omits optional extension API')
    def test_present_extension_api_is_disabled_before_replay(self):
        calls = []
        class DisableRecords(sqlite3.Connection):
            def enable_load_extension(self, enabled):
                calls.append(enabled)
                return super().enable_load_extension(enabled)
        original_connect = sqlite3.connect
        with tempfile.TemporaryDirectory() as folder:
            database = Path(folder) / 'fixture.sqlite'
            def connect(*args, **kwargs):
                return original_connect(*args, factory=DisableRecords, **kwargs)
            with patch.object(CAPTURE.sqlite3, 'connect', side_effect=connect):
                CAPTURE.apply_sql(database, 'CREATE TABLE normal_replay(id INTEGER);')
            self.assertEqual(calls, [False])
            with closing(original_connect(database)) as connection:
                self.assertEqual(connection.execute('SELECT name FROM sqlite_schema WHERE type = ?;', ('table',)).fetchall(), [('normal_replay',)])

    def test_real_engine_catalog_before_after_and_contract_export(self):
        if ENGINE is None:
            self.skipTest('pass --engine to exercise the compiled analyzer')
        with tempfile.TemporaryDirectory() as folder:
            root = Path(folder)
            before, migration = root / 'base.sql', root / 'migration.sql'
            before.write_text('CREATE TABLE users(id INTEGER PRIMARY KEY, email TEXT NOT NULL); CREATE VIEW emails AS SELECT email FROM users;', encoding='utf-8')
            migration.write_text('DROP VIEW emails; ALTER TABLE users RENAME COLUMN email TO address; CREATE VIEW emails AS SELECT address FROM users;', encoding='utf-8')
            receipt = CAPTURE.capture(ENGINE, before, [migration], root / 'capture', root)
            self.assertEqual(receipt['reviewExitCode'], 2)
            review = json.loads((root / 'capture/review.json').read_text())
            self.assertEqual(review['kind'], 'review')
            self.assertGreater(review['totalChanges'], 0)
            self.assertTrue(review['analysisPartial'] or not review['complete'] or review['comparisonNotes'])
            self.assertEqual(receipt['reviewComplete'], review['complete'])
            self.assertFalse(receipt['complete'])
            self.assertEqual(len(receipt['outputs']), 7)
            self.assertGreater(receipt['seconds']['total'], 0)
            old = json.loads((root / 'capture/before.facts.json').read_text())
            new = json.loads((root / 'capture/after.facts.json').read_text())
            old_columns = [f['method'] for f in old['facts'] if f.get('method')]
            new_columns = [f['method'] for f in new['facts'] if f.get('method')]
            self.assertIn('email', old_columns)
            self.assertNotIn('email', new_columns)
            self.assertIn('address', new_columns)
            self.assertTrue(all('dataContract' in f for f in new['facts']))
            with self.assertRaises(FileExistsError):
                CAPTURE.capture(ENGINE, before, [migration], root / 'capture', root)


if __name__ == '__main__':
    parser = argparse.ArgumentParser()
    parser.add_argument('--engine')
    args, rest = parser.parse_known_args()
    ENGINE = args.engine
    unittest.main(argv=[__file__, *rest])
