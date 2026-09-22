#!/usr/bin/env python3
"""DML producer baseline 하네스의 범위·보안·skip 경계를 점검한다."""

from __future__ import annotations

import importlib.util
import json
from pathlib import Path
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[1]
SPEC = importlib.util.spec_from_file_location("dml_producers", Path(__file__).with_name("verify-dml-producers.py"))
CHECK = importlib.util.module_from_spec(SPEC)
assert SPEC.loader is not None
SPEC.loader.exec_module(CHECK)


class DmlProducerTests(unittest.TestCase):
    """published producer 입력을 analyzer와 분리한 경계만 검증한다."""

    def test_frozen_corpus_and_sql_file_subjects(self):
        corpus = CHECK.load_frozen_corpus()
        with tempfile.TemporaryDirectory(prefix="sg-dml-producer-test-") as directory:
            root = Path(directory)
            sqlite = CHECK.sql_file_directory(corpus, "sqlite", root / "sqlite")
            postgres = CHECK.sql_file_directory(corpus, "postgres", root / "postgres")
            self.assertEqual(len(list(sqlite.rglob("*.sql"))), 15)
            self.assertEqual(len(list(postgres.rglob("*.sql"))), 4)
            self.assertTrue((sqlite / "dml/insert_select_permuted.sql").is_file())
            self.assertTrue((postgres / "dml/ctas_filtered.sql").is_file())

    def test_subject_contract_separates_wrappers_and_sql_only(self):
        corpus = CHECK.load_frozen_corpus()
        self.assertEqual(CHECK.DML.subject_spec(corpus, "postgres", corpus["cases"][0])["kind"], "function")
        self.assertEqual(CHECK.DML.subject_spec(corpus, "sqlserver", corpus["cases"][0])["kind"], "procedure")
        self.assertEqual(CHECK.DML.subject_spec(corpus, "oracle", corpus["cases"][0])["kind"], "procedure")
        for dialect in ("postgres", "sqlserver", "oracle"):
            case = next(case for case in corpus["cases"] if case["name"] == "ctas_filtered")
            spec = CHECK.DML.subject_spec(corpus, dialect, case)
            self.assertEqual((spec["kind"], spec["mode"]), ("query", "sql_files"))

    def test_merge_uses_native_postgres_oracle_rows(self):
        corpus = CHECK.load_frozen_corpus()
        case = next(case for case in corpus["cases"] if case["name"] == "merge_upsert")
        self.assertEqual(CHECK.runtime_oracle(case, "sqlserver")["rows"], [[20, 222, "merge-existing", "merge_upsert"], [40, 444, "merge-new", "merge_upsert"]])
        self.assertEqual(CHECK.runtime_oracle(case, "oracle")["rows"], [[20, 222, "merge-existing", "merge_upsert"], [40, 444, "merge-new", "merge_upsert"]])

    def test_wrapper_sql_and_calls_are_native(self):
        corpus = CHECK.load_frozen_corpus()
        oracle_corpus = CHECK.adapt_database_schema(corpus, "oracle", "SGACC")
        case = next(case for case in corpus["cases"] if case["name"] == "insert_select_permuted")
        self.assertIn("CREATE OR REPLACE FUNCTION public.acc_insert_select_permuted", CHECK.wrapper_sql(corpus, "postgres", case))
        self.assertIn("CREATE OR ALTER PROCEDURE dbo.acc_insert_select_permuted", CHECK.wrapper_sql(corpus, "sqlserver", case))
        self.assertIn("CREATE OR REPLACE PROCEDURE SGACC.ACC_INSERT_SELECT_PERMUTED", CHECK.wrapper_sql(oracle_corpus, "oracle", case))
        self.assertEqual(CHECK.wrapper_call("postgres", case, corpus), "SELECT public.acc_insert_select_permuted();")
        self.assertEqual(CHECK.wrapper_call("sqlserver", case, corpus), "EXEC dbo.acc_insert_select_permuted")
        self.assertEqual(CHECK.wrapper_call("oracle", case, oracle_corpus), "BEGIN ACC_INSERT_SELECT_PERMUTED; END;")

    def test_full_native_invocation_does_not_skip_missing_external_runners(self):
        args = type("Args", (), {
            "engine": Path("/tmp/missing-published-engine"),
            "go_probe": None,
            "probe_jar": None,
            "oracle_jar": None,
            "postgres_bin": Path("/tmp/missing-postgres"),
        })()
        with self.assertRaisesRegex(RuntimeError, "published engine"):
            CHECK.validate_resources(args, {"sqlite", "postgres", "sqlserver", "oracle"})

    def test_baseline_directory_is_immutable(self):
        with tempfile.TemporaryDirectory(prefix="sg-dml-producer-output-") as directory:
            path = Path(directory) / "baseline"
            path.mkdir()
            with self.assertRaisesRegex(RuntimeError, "refusing overwrite"):
                CHECK.ensure_new_directory(path)

    def test_runtime_reports_are_json_safe(self):
        corpus = CHECK.load_frozen_corpus()
        case = corpus["cases"][0]
        report = CHECK.compare_runtime(case, "sqlite", [[102, 24, "mid", "insert_permuted"], [104, 60, "high", "insert_permuted"]])
        self.assertEqual(report["status"], "passed")
        json.dumps(report)
        pg_report = CHECK.compare_runtime(case, "postgres", [["102", "24", "mid", "insert_permuted"], ["104", "60", "high", "insert_permuted"]])
        self.assertEqual(pg_report["status"], "passed")


if __name__ == "__main__":
    unittest.main()
