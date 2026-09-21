#!/usr/bin/env python3
"""bounded scale-validation 생성기와 계약 불변식을 실행한다."""

from __future__ import annotations

import importlib.util
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest import mock


ROOT = Path(__file__).resolve().parent.parent
MODULE_PATH = ROOT / "Scripts" / "benchmark-scale.py"
SPEC = importlib.util.spec_from_file_location("benchmark_scale", MODULE_PATH)
if SPEC is None or SPEC.loader is None:
    raise RuntimeError(f"could not load {MODULE_PATH}")
MODULE = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = MODULE
SPEC.loader.exec_module(MODULE)


class ScaleValidationTests(unittest.TestCase):
    """측정 전 입력 생성·출력 shape·요약 계약을 작은 fixture로 검사한다."""

    def test_catalog_formats_are_deterministic_and_have_one_large_schema(self) -> None:
        with tempfile.TemporaryDirectory(prefix="schemagraph-scale-test-") as directory:
            work = Path(directory)
            (work / "first").mkdir()
            (work / "second").mkdir()
            first = MODULE.write_large_schema(work / "first", 7, 3)
            second = MODULE.write_large_schema(work / "second", 7, 3)
            self.assertEqual(
                [MODULE.sha256(first[name]) for name in ("json", "ndjson")],
                [MODULE.sha256(second[name]) for name in ("json", "ndjson")],
            )
            document = json.loads(first["json"].read_text(encoding="utf-8"))
            self.assertEqual(document["version"], 2)
            self.assertEqual(len(document["schemas"]), 1)
            self.assertEqual(document["schemas"][0]["name"], "large")
            self.assertEqual(len(document["schemas"][0]["objects"]), 7)
            records = [json.loads(line) for line in first["ndjson"].read_text(encoding="utf-8").splitlines()]
            self.assertEqual([record["type"] for record in records[:2]], ["document", "schema"])
            self.assertEqual(sum(record["type"] == "object" for record in records), 7)
            self.assertEqual(records[-1]["type"], "limitations")

    def test_dense_graph_facts_are_bounded_and_endpoint_complete(self) -> None:
        with tempfile.TemporaryDirectory(prefix="schemagraph-scale-test-") as directory:
            path = MODULE.write_dense_graph(Path(directory), 11, 3, max_edges=MODULE.MAX_DENSE_EDGES)
            facts = MODULE.graph_facts(path)
            self.assertEqual(facts["vertices"], 12)
            self.assertEqual(facts["edges"], 44)
            self.assertEqual(facts["vertex_kind_counts"], {"table": 1, "view": 11})
            self.assertEqual(facts["edge_kind_counts"], {"reads": 44})

    def test_large_topology_rejects_tampered_graph(self) -> None:
        vertices, edges, expected = MODULE.expected_large_topology(2, 3)
        with tempfile.TemporaryDirectory(prefix="schemagraph-scale-test-") as directory:
            path = Path(directory) / "graph.json"
            value = {
                "version": 2,
                "vertices": [
                    {
                        "id": identifier,
                        "kind": kind,
                        "level": "schema" if kind == "schema" else "object" if kind == "table" else "member",
                        "name": identifier.rsplit(".", 1)[-1],
                        "schema": "large",
                    }
                    for identifier, kind in vertices
                ],
                "edges": [{"from": source, "kind": kind, "to": target} for source, kind, target in edges],
            }
            path.write_text(json.dumps(value), encoding="utf-8")
            facts = MODULE.graph_facts(path, expected_vertices=vertices, expected_edges=edges)
            self.assertEqual(facts["vertices"], expected["vertices"])
            value["edges"].pop()
            path.write_text(json.dumps(value), encoding="utf-8")
            with self.assertRaises(MODULE.BenchmarkError):
                MODULE.graph_facts(path, expected_vertices=vertices, expected_edges=edges)

    def test_dense_workloads_use_two_scales_and_forbid_duplicate_targets(self) -> None:
        workloads = MODULE.dense_workloads([1000, 2000], None, MODULE.MAX_DENSE_EDGES)
        self.assertEqual([(item["vertices"], item["degree"]) for item in workloads], [(1000, 999), (2000, 499)])
        self.assertEqual([item["edges"] for item in workloads], [1_000_000, 1_000_000])
        with self.assertRaises(MODULE.BenchmarkError):
            MODULE.dense_workloads([10, 20], 20, MODULE.MAX_DENSE_EDGES)

    def test_summary_reports_median_tail_and_max(self) -> None:
        measurements = [
            MODULE.Measurement(seconds=0.1, peak_rss_bytes=10),
            MODULE.Measurement(seconds=0.2, peak_rss_bytes=20),
            MODULE.Measurement(seconds=0.3, peak_rss_bytes=30),
            MODULE.Measurement(seconds=0.4, peak_rss_bytes=40),
            MODULE.Measurement(seconds=0.5, peak_rss_bytes=50),
        ]
        summary = MODULE.summarize(measurements)
        self.assertEqual(summary["count"], 5)
        self.assertEqual(summary["seconds"], {"median": 0.3, "p95": 0.5, "max": 0.5})
        self.assertEqual(summary["peak_rss_bytes"]["median"], 30)
        self.assertEqual(summary["peak_rss_bytes"]["max"], 50)
        self.assertEqual(MODULE.summarize_values([0.1, 0.2, 0.3]), {"count": 3, "median": 0.2, "p95": 0.3, "max": 0.3})

    def test_cancelled_impact_requires_explicit_incomplete_fact(self) -> None:
        with self.assertRaises(MODULE.BenchmarkError):
            MODULE.assert_impact(
                {
                    "impacted": [],
                    "visited": 1,
                    "complete": True,
                    "truncationReasons": ["cancelled"],
                },
                allow_cancelled=True,
            )

    def test_queued_request_without_cpu_progress_fails_active_proof(self) -> None:
        class QueuedSession:
            timeout = 1.0
            pending: dict[object, list[dict[str, object]]] = {}
            process = mock.Mock(pid=123)

            def poll_messages(self, timeout: float) -> list[dict[str, object]]:
                return []

        with (
            mock.patch.object(MODULE, "ACTIVE_PROOF_TIMEOUT", 0.001, create=True),
            mock.patch.object(MODULE, "process_cpu_seconds", return_value=4.2),
            self.assertRaisesRegex(MODULE.BenchmarkError, "CPU progress"),
        ):
            MODULE.observe_active_request(QueuedSession(), 10, 4.2)

    def test_reader_only_ping_cannot_prove_worker_unblocked(self) -> None:
        with self.assertRaisesRegex(MODULE.BenchmarkError, "ping bypasses the analysis worker"):
            MODULE.assert_worker_bound_followup_request(
                {"jsonrpc": "2.0", "id": 12, "method": "ping"}
            )

    def test_impact_facts_reject_wrong_topology(self) -> None:
        expected = MODULE.expected_dense_impact(10, 4)
        value = {
            "complete": True,
            "truncated": True,
            "visited": 11,
            "examinedEdges": 49,
            "truncationReasons": ["result-limit"],
            "subject": {"id": "main.root"},
            "impacted": [{"id": "main.node_000000", "distance": 1, "edges": ["reads"]}],
        }
        with self.assertRaises(MODULE.BenchmarkError):
            MODULE.assert_impact(value, expected=expected)

    def test_edge_limited_dense_expectation_is_explicit(self) -> None:
        expected = MODULE.expected_dense_impact(2000, 999)
        value = {
            "complete": False,
            "truncated": True,
            "visited": 2001,
            "examinedEdges": 1_000_000,
            "truncationReasons": ["edge-limit", "result-limit"],
            "subject": {"id": "main.root"},
            "impacted": [{"id": "main.node_000000", "distance": 1, "edges": ["reads"]}],
        }
        self.assertEqual(MODULE.assert_impact(value, expected=expected), expected)

    def test_full_impact_facts_validate_every_impacted_id(self) -> None:
        expected = {
            "complete": True,
            "truncated": False,
            "visited": 4,
            "examined_edges": 12,
            "truncation_reasons": [],
            "reported_impacted": 3,
            "impacted_ids": [
                "main.node_000000",
                "main.node_000001",
                "main.node_000002",
            ],
            "distances": [1, 1, 1],
            "edge_kinds": [["reads"], ["reads"], ["reads"]],
            "subject": "main.root",
        }
        value = {
            "complete": True,
            "truncated": False,
            "visited": 4,
            "examinedEdges": 12,
            "truncationReasons": [],
            "subject": {"id": "main.root"},
            "impacted": [
                {"id": identifier, "distance": 1, "edges": ["reads"]}
                for identifier in expected["impacted_ids"]
            ],
        }
        self.assertEqual(MODULE.assert_impact(value, expected=expected), expected)

    def test_smoke_command_has_no_engine_or_repo_artifact_requirement(self) -> None:
        result = subprocess.run(
            [sys.executable, str(MODULE_PATH), "--smoke"],
            cwd=ROOT,
            capture_output=True,
            text=True,
            check=False,
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        output = json.loads(result.stdout)
        self.assertEqual(output["status"], "ok")
        self.assertEqual(output["dense_graph_facts"]["dangling_edges"], 0)


if __name__ == "__main__":
    raise SystemExit(unittest.main())
