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
    concat = fixture_id(graph, "dynamic_concat")
    reassign = fixture_id(graph, "dynamic_reassign")
    branch = fixture_id(graph, "dynamic_branch")
    cleanup = fixture_id(graph, "dynamic_cleanup")
    customers = fixture_id(graph, "customers")
    orders = fixture_id(graph, "orders")
    want = {
        ("writes", touch, customers),
        ("reads", touch, orders),
        ("writes", concat, customers),
        ("writes", reassign, orders),
    }
    complete = [touch, concat, reassign]
    if dialect == "postgres":
        formatted = fixture_id(graph, "dynamic_format")
        rows = fixture_id(graph, "dynamic_rows")
        complete.append(rows)
        complete.append(formatted)
        want.update({
            ("writes", formatted, customers),
            ("reads", rows, customers),
            ("reads", rows, orders),
            ("calls", rows, fixture_id(graph, "dynamic_key")),
        })
    assert want <= edges, f"Missing dynamic SQL dependencies: {sorted(want - edges)}"
    assert not any(
        source == cleanup for _, source, _ in edges
    ), "Unresolved SQL produced dependencies"
    assert not any(
        kind == "writes" and source == reassign and target == customers
        for kind, source, target in edges
    ), "Reassignment retained a stale dynamic SQL target"
    assert not any(source == branch for _, source, _ in edges), (
        "Branch-dependent SQL produced a guessed dependency"
    )
    notes = graph.get("limitations", [])
    assert any(cleanup in note for note in notes), "Unresolved SQL was not reported"
    assert any(branch in note for note in notes), "Branch-dependent SQL was not reported"
    assert not any(owner in note for owner in complete for note in notes), notes

    if dialect == "oracle":
        package_member_ids = {
            vertex["id"]
            for vertex in graph["vertices"]
            if ".order_ops" in vertex["id"].casefold()
            and vertex["kind"] in {"function", "procedure", "package"}
        }
        standalone = fixture_id(graph, "standalone")
        assert not any(
            source in package_member_ids and target == standalone
            for _, source, target in edges
        ), "Package comments/q-quote created a phantom standalone dependency"

        touch_member = fixture_id(graph, "touch")
        count_all = fixture_id(graph, "count_all")
        refresh = fixture_id(graph, "refresh")
        tickets = fixture_id(graph, "tickets")
        member_want = {
            ("writes", touch_member, customers),
            ("reads", count_all, orders),
            ("calls", refresh, count_all),
            ("writes", refresh, tickets),
        }
        assert member_want <= edges, (
            f"Package member attribution changed: {sorted(member_want - edges)}"
        )


if __name__ == "__main__":
    with open(sys.argv[1], encoding="utf-8") as source:
        check(json.load(source), sys.argv[2])
