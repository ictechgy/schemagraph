#!/usr/bin/env python3
"""Focused tests for the authored DML facts and verifier boundary."""

from __future__ import annotations

import contextlib
import importlib.util
import io
import json
from pathlib import Path
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[1]
SPEC = importlib.util.spec_from_file_location("dml_accuracy", Path(__file__).with_name("verify-dml-accuracy.py"))
CHECK = importlib.util.module_from_spec(SPEC)
assert SPEC.loader is not None
SPEC.loader.exec_module(CHECK)


def graph_fixture():
    """Build a producer-shaped graph from facts without parsing any authored SQL."""

    corpus = CHECK.load_corpus()
    dialect = "sqlite"
    vertices: list[dict] = []
    vertex_ids: set[str] = set()
    for relation in corpus["relations"].values():
        if relation.get("temporary"):
            continue
        relation_id = relation["ids"][dialect]
        vertices.append({"id": relation_id, "name": relation["name"], "kind": "table"})
        vertex_ids.add(relation_id)
        for column in relation["columns"]:
            column_id = f"{relation_id}.{column}"
            vertices.append({"id": column_id, "name": column, "kind": "column"})
            vertex_ids.add(column_id)
    edges: list[dict] = []
    origins: list[dict] = []
    analysis: list[dict] = []
    for case in CHECK.eligible_cases(corpus, dialect):
        subject = CHECK._subject_id(corpus, dialect, case)
        spec = CHECK.subject_spec(corpus, dialect, case)
        vertices.append({"id": subject, "name": spec["name"], "kind": spec["kind"]})
        vertex_ids.add(subject)
        facts = CHECK.expected_facts(corpus, case, dialect)
        for target in sorted(facts["object_reads"]):
            edges.append({"from": subject, "kind": "reads", "to": target})
        for target in sorted(facts["column_reads"]):
            edges.append({"from": subject, "kind": "reads", "to": target})
        for target in sorted(facts["object_writes"]):
            edges.append({"from": subject, "kind": "writes", "to": target})
        for target in sorted(facts["column_writes"]):
            edges.append({"from": subject, "kind": "writes", "to": target})
        owner_hash = spec["expected_body_hash"] or CHECK.body_hash(CHECK.case_sql(case, dialect))
        for output, source in sorted(facts["lineage"]):
            if output != source:
                edges.append({"from": output, "kind": "derives-from", "to": source})
        for output, source in sorted(facts["lineage"]):
            if output != source:
                origins.append({"from": output, "to": source, "kind": "derives-from", "body_hash": owner_hash, "role": "value"})
        diagnostics = []
        if case["state"] == "partial":
            diagnostics.append({"code": "SG_TEMPORAL_SELF_LINEAGE"})
        analysis.append({"id": subject, "state": case["state"], "diagnostics": diagnostics,
                         "body_hash": owner_hash, "source": spec["source"]})
    return corpus, {"vertices": vertices, "edges": edges, "analysis": analysis, "origins": origins}


def catalog_fixture():
    """Build raw producer metadata with exact query source and body bytes."""

    corpus = CHECK.load_corpus()
    objects = []
    for relation in corpus["relations"].values():
        objects.append({
            "name": relation["name"],
            "kind": "table",
            "columns": [{"name": column} for column in relation["columns"]],
        })
    routines = []
    for case in CHECK.eligible_cases(corpus, "sqlite"):
        spec = CHECK.subject_spec(corpus, "sqlite", case)
        routines.append({
            "name": spec["name"],
            "kind": spec["kind"],
            "body": CHECK.case_sql(case, "sqlite"),
            "source": spec["source"],
        })
    return corpus, {"schemas": [{"name": "main", "objects": objects, "routines": routines}]}


class DmlAccuracyTests(unittest.TestCase):
    """The verifier must expose independent fact mismatches at every boundary."""

    def test_corpus_has_independent_facts_and_dialect_ids(self):
        corpus = CHECK.load_corpus()
        self.assertEqual(len(corpus["cases"]), 16)
        self.assertEqual(sum("merge" in case["tags"] for case in corpus["cases"]), 1)
        self.assertTrue(all(relation.get("temporary") or set(relation["ids"]) == {"sqlite", "postgres", "sqlserver", "oracle"}
                            for relation in corpus["relations"].values()))
        self.assertTrue(all(relation.get("logical_only") for relation in corpus["relations"].values() if relation.get("temporary")))
        self.assertEqual(CHECK.column_id(corpus, "oracle", "source.source_id"), "DMLACC.DML_SOURCE.SOURCE_ID")
        self.assertEqual(CHECK._subject_id(corpus, "oracle", corpus["cases"][0]), "DMLACC.ACC_INSERT_SELECT_PERMUTED")
        sqlite_subject = CHECK.subject_spec(corpus, "sqlite", corpus["cases"][0])
        self.assertEqual(sqlite_subject["kind"], "query")
        self.assertEqual(sqlite_subject["name"], CHECK.query_name(sqlite_subject["source"]))
        self.assertEqual(CHECK.subject_spec(corpus, "postgres", corpus["cases"][0])["kind"], "function")
        self.assertEqual(CHECK.subject_spec(corpus, "sqlserver", corpus["cases"][0])["kind"], "procedure")
        self.assertTrue(any(case.get("fact_state", "complete") == "complete" and case["state"] == "partial"
                            for case in corpus["cases"]))

    def test_sqlite_runtime_oracle_passes_without_analyzer(self):
        corpus = CHECK.load_corpus()
        report = CHECK.validate_sqlite(corpus)
        self.assertEqual(report["status"], "passed")
        self.assertEqual(report["passed"], 15)
        self.assertEqual(report["skipped"], 1)
        self.assertEqual(report["failed"], 0)
        self.assertFalse(report["analyzer_evaluated"])

    def test_selected_case_validation_is_bounded(self):
        corpus = CHECK.load_corpus()
        report = CHECK.validate_sqlite(corpus, {"insert_values_source_free"})
        self.assertEqual(report["passed"], 1)
        self.assertEqual(len(report["cases"]), 1)
        self.assertEqual(report["cases"][0]["rows"], 1)

    def test_merge_is_explicitly_skipped_on_sqlite(self):
        corpus = CHECK.load_corpus()
        report = CHECK.validate_sqlite(corpus, {"merge_upsert"})
        self.assertEqual(report["skipped"], 1)
        self.assertIn("no MERGE", report["cases"][0]["reason"])

    def test_supported_dialects_filter_is_shared_by_runtime_and_graph_score(self):
        corpus, graph = graph_fixture()
        excluded = corpus["cases"][0]
        excluded["supported_dialects"] = ["postgres"]
        runtime = CHECK.validate_sqlite(corpus, {excluded["name"]})
        self.assertEqual(runtime["skipped"], 1)
        result = CHECK.evaluate_graph(graph, corpus, "sqlite")
        self.assertNotIn(excluded["name"], {item["name"] for item in result["cases"]})

    def test_matching_graph_passes_with_temporal_self_lineage_diagnostic(self):
        corpus, graph = graph_fixture()
        result = CHECK.evaluate_graph(graph, corpus, "sqlite")
        self.assertEqual(result["status"], "passed")
        self.assertEqual(result["failures"], [])
        self.assertEqual(result["false_complete_cases"], [])
        self.assertTrue(all(item["status"] == "passed" for item in result["cases"]))

    def test_false_subject_kind_is_rejected(self):
        corpus, graph = graph_fixture()
        subject = CHECK._subject_id(corpus, "sqlite", corpus["cases"][0])
        next(vertex for vertex in graph["vertices"] if vertex["id"] == subject)["kind"] = "procedure"
        result = CHECK.evaluate_graph(graph, corpus, "sqlite")
        self.assertEqual(result["status"], "failed")
        self.assertTrue(any("expected kind query" in failure for failure in result["failures"]))

    def test_false_table_kind_is_rejected(self):
        corpus, graph = graph_fixture()
        next(vertex for vertex in graph["vertices"] if vertex["id"] == "main.dml_source")["kind"] = "view"
        result = CHECK.evaluate_graph(graph, corpus, "sqlite")
        self.assertEqual(result["status"], "failed")
        self.assertTrue(any("expected kind table" in failure for failure in result["failures"]))

    def test_missing_origin_cannot_be_filled_by_global_writer(self):
        corpus, graph = graph_fixture()
        subject = CHECK._subject_id(corpus, "sqlite", corpus["cases"][0])
        owner_hash = next(item for item in graph["analysis"] if item["id"] == subject)["body_hash"]
        graph["origins"] = [origin for origin in graph["origins"] if origin["body_hash"] != owner_hash]
        result = CHECK.evaluate_graph(graph, corpus, "sqlite")
        item = next(item for item in result["cases"] if item["name"] == corpus["cases"][0]["name"])
        self.assertEqual(item["status"], "failed")
        self.assertTrue(item["facts"]["lineage"]["missing"])

    def test_wrong_writer_origin_is_not_accepted(self):
        corpus, graph = graph_fixture()
        first = corpus["cases"][0]
        second = corpus["cases"][1]
        first_hash = next(item for item in graph["analysis"] if item["id"] == CHECK._subject_id(corpus, "sqlite", first))["body_hash"]
        second_hash = next(item for item in graph["analysis"] if item["id"] == CHECK._subject_id(corpus, "sqlite", second))["body_hash"]
        for origin in graph["origins"]:
            if origin["body_hash"] == first_hash:
                origin["body_hash"] = second_hash
        result = CHECK.evaluate_graph(graph, corpus, "sqlite")
        item = next(item for item in result["cases"] if item["name"] == first["name"])
        self.assertEqual(item["status"], "failed")

    def test_constant_destination_rejects_owner_extra_lineage(self):
        corpus, graph = graph_fixture()
        case = next(case for case in corpus["cases"] if case["name"] == "insert_values_source_free")
        subject = CHECK._subject_id(corpus, "sqlite", case)
        owner_hash = next(item for item in graph["analysis"] if item["id"] == subject)["body_hash"]
        graph["edges"].append({"from": "main.dml_target.target_value", "kind": "derives-from", "to": "main.dml_source.amount"})
        graph["origins"].append({"from": "main.dml_target.target_value", "to": "main.dml_source.amount", "kind": "derives-from", "body_hash": owner_hash, "role": "value"})
        result = CHECK.evaluate_graph(graph, corpus, "sqlite")
        item = next(item for item in result["cases"] if item["name"] == case["name"])
        self.assertEqual(item["status"], "failed")
        self.assertIn(("main.dml_target.target_value", "main.dml_source.amount"), item["facts"]["lineage"]["unexpected"])

    def test_global_write_target_scope_requires_an_origin_for_every_lineage_edge(self):
        corpus, graph = graph_fixture()
        graph["edges"].append({"from": "main.dml_target.last_writer", "kind": "derives-from", "to": "main.dml_source.amount"})
        result = CHECK.evaluate_graph(graph, corpus, "sqlite")
        self.assertEqual(result["status"], "failed")
        self.assertIn("lineage edge missing matching origin", result["failures"])
        self.assertIn(("main.dml_target.last_writer", "main.dml_source.amount"), result["unattributed_lineage"])

    def test_missing_subject_vertex_is_rejected(self):
        corpus, graph = graph_fixture()
        subject = CHECK._subject_id(corpus, "sqlite", corpus["cases"][0])
        graph["vertices"] = [vertex for vertex in graph["vertices"] if vertex["id"] != subject]
        result = CHECK.evaluate_graph(graph, corpus, "sqlite")
        self.assertEqual(result["status"], "failed")
        self.assertTrue(any(subject in failure for failure in result["failures"]))

    def test_missing_column_is_reported_as_false_complete(self):
        corpus, graph = graph_fixture()
        edge = next(edge for edge in graph["edges"]
                     if edge["kind"] == "reads" and edge["to"] == "main.dml_source.amount")
        graph["edges"].remove(edge)
        result = CHECK.evaluate_graph(graph, corpus, "sqlite")
        self.assertEqual(result["status"], "failed")
        self.assertTrue(any("column_reads" in str(item) and item["status"] == "failed"
                            for item in result["cases"]))
        self.assertIn(CHECK._subject_id(corpus, "sqlite", corpus["cases"][0]), result["false_complete_cases"])

    def test_extra_write_and_phantom_endpoint_are_rejected(self):
        corpus, graph = graph_fixture()
        subject = CHECK._subject_id(corpus, "sqlite", corpus["cases"][0])
        graph["edges"].append({"from": subject, "kind": "writes", "to": "main.dml_source"})
        graph["edges"].append({"from": subject, "kind": "reads", "to": "main.missing"})
        result = CHECK.evaluate_graph(graph, corpus, "sqlite")
        self.assertEqual(result["status"], "failed")
        self.assertIn("phantom graph endpoints", result["failures"])
        self.assertIn(CHECK._subject_id(corpus, "sqlite", corpus["cases"][0]), result["false_complete_cases"])

    def test_extra_lineage_is_reported_in_global_destination_scope(self):
        corpus, graph = graph_fixture()
        graph["edges"].append({
            "from": "main.dml_target.target_value",
            "kind": "derives-from",
            "to": "main.dml_source.active",
        })
        result = CHECK.evaluate_graph(graph, corpus, "sqlite")
        self.assertEqual(result["status"], "failed")
        self.assertIn("global DML lineage", result["failures"])
        self.assertIn(("main.dml_target.target_value", "main.dml_source.active"),
                      result["global_lineage"]["unexpected"])

    def test_null_graph_identity_is_rejected(self):
        corpus, graph = graph_fixture()
        graph["edges"].append({"from": None, "kind": "reads", "to": "main.dml_source"})
        result = CHECK.evaluate_graph(graph, corpus, "sqlite")
        self.assertEqual(result["status"], "failed")
        self.assertIn("null graph edge endpoint", result["failures"])

    def test_missing_analysis_and_temporal_diagnostic_are_failures(self):
        corpus, graph = graph_fixture()
        missing_subject = CHECK._subject_id(corpus, "sqlite", corpus["cases"][0])
        graph["analysis"] = [item for item in graph["analysis"]
                              if item["id"] != missing_subject]
        result = CHECK.evaluate_graph(graph, corpus, "sqlite")
        self.assertIn(missing_subject, result["failures"])
        corpus, graph = graph_fixture()
        partial = next(item for item in graph["analysis"] if item["state"] == "partial")
        partial["diagnostics"] = []
        result = CHECK.evaluate_graph(graph, corpus, "sqlite")
        self.assertEqual(result["status"], "failed")
        self.assertTrue(any(item["temporal_self_lineage"].get("error")
                            for item in result["cases"]))

    def test_catalog_preflight_rejects_missing_actual_ids(self):
        corpus, _graph = graph_fixture()
        document = {"schemas": [{"name": "main", "objects": []}]}
        result = CHECK.validate_catalog_document(document, corpus, "sqlite")
        self.assertTrue(result["failures"])
        self.assertTrue(any("missing catalog relation" in failure for failure in result["failures"]))

    def test_catalog_subject_preflight_checks_kind_source_and_body(self):
        corpus, document = catalog_fixture()
        result = CHECK.validate_catalog_document(document, corpus, "sqlite")
        self.assertEqual(result["failures"], [])
        first = document["schemas"][0]["routines"][0]
        first["kind"] = "procedure"
        result = CHECK.validate_catalog_document(document, corpus, "sqlite")
        self.assertTrue(any("expected query" in failure for failure in result["failures"]))

    def test_catalog_preflight_rejects_duplicate_owner_body_hashes(self):
        corpus, document = catalog_fixture()
        routines = document["schemas"][0]["routines"]
        routines[1]["body"] = routines[0]["body"]
        result = CHECK.validate_catalog_document(document, corpus, "sqlite")
        self.assertTrue(result["duplicate_body_hashes"])
        self.assertTrue(any("duplicate subject body hash" in failure for failure in result["failures"]))

    def test_cli_validation_output_is_replayable_json(self):
        with tempfile.TemporaryDirectory(prefix="sg-dml-test-") as directory:
            output = Path(directory) / "report.json"
            code = CHECK.main(["--validate-only", "--strict", "--output", str(output)])
            self.assertEqual(code, 0)
            report = json.loads(output.read_text(encoding="utf-8"))
            self.assertEqual(report["status"], "passed")
            self.assertFalse(report["analyzer_evaluated"])

    def test_wrapper_dialect_graph_requires_raw_document(self):
        with tempfile.TemporaryDirectory(prefix="sg-dml-graph-") as directory:
            graph = Path(directory) / "graph.json"
            output = Path(directory) / "report.json"
            graph.write_text(json.dumps({"vertices": [], "edges": [], "analysis": [], "origins": []}), encoding="utf-8")
            with contextlib.redirect_stderr(io.StringIO()), self.assertRaises(SystemExit):
                CHECK.main(["--graph", str(graph), "--dialect", "postgres", "--output", str(output)])

    def test_wrapper_dialect_scorer_rejects_derived_case_hashes(self):
        corpus, graph = graph_fixture()
        with self.assertRaisesRegex(CHECK.CorpusError, "raw catalog subject records"):
            CHECK.evaluate_graph(graph, corpus, "postgres")


if __name__ == "__main__":
    unittest.main()
