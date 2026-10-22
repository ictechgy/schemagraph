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

SPEC = importlib.util.spec_from_file_location('capture', Path(__file__).with_name('capture-migration.py'))
CAPTURE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(CAPTURE)
ENGINE = None


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

    def test_real_engine_catalog_before_after_and_contract_export(self):
        if ENGINE is None:
            self.skipTest('pass --engine to exercise the compiled analyzer')
        with tempfile.TemporaryDirectory() as folder:
            root = Path(folder)
            before, migration = root / 'base.sql', root / 'migration.sql'
            before.write_text('CREATE TABLE users(id INTEGER PRIMARY KEY, email TEXT NOT NULL); CREATE VIEW emails AS SELECT email FROM users;', encoding='utf-8')
            migration.write_text('DROP VIEW emails; ALTER TABLE users RENAME COLUMN email TO address; CREATE VIEW emails AS SELECT address FROM users;', encoding='utf-8')
            receipt = CAPTURE.capture(ENGINE, before, [migration], root / 'capture', root)
            self.assertEqual(receipt['reviewExitCode'], 1)
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
