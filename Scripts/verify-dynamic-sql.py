#!/usr/bin/env python3
"""실제 DB가 수락한 동적 SQL의 수확 결과를 양방향으로 검증한다."""

import json
import sys


def fixture_id(graph, name):
    """방언별 대소문자·시그니처 차이를 넘되 중복 이름을 추측하지 않는다."""
    candidates = [
        vertex["id"]
        for vertex in graph["vertices"]
        if vertex["kind"] in {"table", "function", "procedure"}
        and vertex["id"].rsplit(".", 1)[-1].split("(", 1)[0].casefold() == name
    ]
    assert len(candidates) == 1, f"Expected one fixture object {name}: {candidates}"
    return candidates[0]


def check(graph, dialect):
    """확정된 SQL의 간선과 미확정 SQL의 비간선을 함께 확인한다."""
    edges = {(edge["kind"], edge["from"], edge["to"]) for edge in graph["edges"]}
    touch = fixture_id(graph, "dynamic_touch")
    cleanup = fixture_id(graph, "dynamic_cleanup")
    want = {
        ("writes", touch, fixture_id(graph, "customers")),
        ("reads", touch, fixture_id(graph, "orders")),
    }
    complete = [touch]
    if dialect == "postgres":
        rows = fixture_id(graph, "dynamic_rows")
        complete.append(rows)
        want.update({
            ("reads", rows, fixture_id(graph, "customers")),
            ("reads", rows, fixture_id(graph, "orders")),
            ("calls", rows, fixture_id(graph, "dynamic_key")),
        })
    assert want <= edges, f"Missing dynamic SQL dependencies: {sorted(want - edges)}"
    assert not any(source == cleanup for _, source, _ in edges), "Unresolved SQL produced dependencies"
    notes = graph.get("limitations", [])
    assert any(cleanup in note for note in notes), "Unresolved SQL was not reported"
    assert not any(owner in note for owner in complete for note in notes), notes


if __name__ == "__main__":
    with open(sys.argv[1], encoding="utf-8") as source:
        check(json.load(source), sys.argv[2])
