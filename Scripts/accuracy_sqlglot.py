#!/usr/bin/env python3
"""공개 정확도 코퍼스를 위한 선택적 SQLGlot 비교기.

개발용 비교기이므로 런타임 의존성을 갖지 않는다. ``evaluate``를 호출할 때만
SQLGlot을 읽고, 값 계보에 타입 추론이 필요하지 않으므로 모든 타입은
``UNKNOWN``으로 전달한다. 공급자별 타입을 추측하면 비교 근거가 약해진다.
"""

from __future__ import annotations

from collections.abc import Mapping
from typing import Any


_RELATION_KINDS = frozenset(
    {
        "table",
        "view",
        "materialized-view",
        "materialized_view",
        "foreign-table",
        "foreign_table",
        "partitioned-table",
        "partitioned_table",
    }
)


def evaluate(
    document: dict,
    cases: list[dict],
    dialect: str,
    default_schema: str,
) -> dict:
    """고정한 SQLGlot 계보기로 작성된 SQL 케이스를 평가한다.

    반환 계보에는 ``document``의 객체·컬럼으로 확인된 출처만 담는다. SQLGlot
    리프를 카탈로그 id로 연결할 수 없으면 추측하지 않고 ``errors``에 남긴다.
    """

    sqlglot, exp, lineage, MappingSchema = _sqlglot_imports()
    catalog = _catalog(document, default_schema, dialect)
    schema = _sqlglot_schema(catalog, default_schema, dialect, MappingSchema)
    results = []

    for case in cases:
        results.append(
            _evaluate_case(
                case,
                catalog,
                schema,
                default_schema,
                dialect,
                exp,
                lineage,
            )
        )

    return {"version": sqlglot.__version__, "cases": results}


def _sqlglot_imports():
    try:
        import sqlglot
        from sqlglot import exp
        from sqlglot.lineage import lineage
        from sqlglot.schema import MappingSchema
    except ImportError as error:  # 호출자가 준비한 가상환경에 따라 달라진다.
        raise RuntimeError(
            "SQLGlot 30.18.0 is required for the optional accuracy comparator; "
            "install it in the disposable comparator environment"
        ) from error
    if sqlglot.__version__ != "30.18.0":
        raise RuntimeError(
            f"accuracy comparator requires SQLGlot 30.18.0, found {sqlglot.__version__}"
        )
    return sqlglot, exp, lineage, MappingSchema


def _catalog(document: Mapping[str, Any], default_schema: str, dialect: str) -> dict:
    """카탈로그 문서에서 이름을 보존한 조회 구조를 만든다."""

    tables: list[dict[str, Any]] = []
    for schema_doc in document.get("schemas", []):
        schema_name = schema_doc.get("name")
        if not isinstance(schema_name, str) or not _same_schema(
            schema_name, default_schema, dialect
        ):
            continue
        for object_doc in schema_doc.get("objects", []):
            kind = str(object_doc.get("kind", "")).lower()
            if kind not in _RELATION_KINDS:
                continue
            table_name = object_doc.get("name")
            if not isinstance(table_name, str):
                continue
            columns = {}
            for column_doc in object_doc.get("columns", []):
                column_name = column_doc.get("name")
                if isinstance(column_name, str) and column_name:
                    # 이름만 근거로 사용하고, 공급자별 타입을 추측하지 않도록
                    # 타입 세부 정보는 SQLGlot에 전달하지 않는다.
                    columns.setdefault(column_name, "UNKNOWN")
            if columns:
                tables.append(
                    {
                        "schema": schema_name,
                        "name": table_name,
                        "kind": kind,
                        "columns": columns,
                    }
                )
    return {"tables": tables}


def _sqlglot_schema(catalog: dict, default_schema: str, dialect: str, schema_type):
    mapping = {default_schema: {}}
    for table in catalog["tables"]:
        mapping[default_schema][table["name"]] = dict(table["columns"])
    # PostgreSQL은 인용 식별자의 정확한 이름을 보존해야 한다. SQLite는
    # 대소문자를 구분하지 않으므로 SQLGlot 정규화를 사용한다.
    normalize = not _is_postgres(dialect)
    return schema_type(mapping, dialect=dialect, normalize=normalize)


def _evaluate_case(case, catalog, schema, default_schema, dialect, exp, lineage):
    name = case.get("name", "")
    errors: list[str] = []
    pairs: set[tuple[str, str]] = set()
    sql = case.get("sql")
    if not isinstance(sql, str) or not sql.strip():
        errors.append("case has no SQL text")
        return {"name": name, "lineage": [], "errors": errors}

    output_columns = _case_output_columns(catalog, name, default_schema, dialect)
    if output_columns is None:
        errors.append(f"missing catalog output columns for view acc_{name}")

    try:
        outputs = lineage(None, sql, schema=schema, dialect=dialect)
    except Exception as error:  # SQLGlot이 여러 예외 형식으로 파싱 실패를 알린다.
        errors.append(f"lineage failed: {error}")
        return {"name": name, "lineage": [], "errors": errors}

    if output_columns is not None and len(outputs) != len(output_columns):
        errors.append(
            f"catalog output column count mismatch for view acc_{name}: "
            f"SQLGlot={len(outputs)}, catalog={len(output_columns)}"
        )

    for output_name, root in outputs.items():
        output = _resolve_output_name(output_name, output_columns, dialect, errors)
        leaves = list(root.walk())
        source_nodes = [node for node in leaves if isinstance(node.expression, exp.Table)]
        unknown_nodes = [
            node
            for node in leaves
            if not node.downstream and not isinstance(node.expression, exp.Table)
        ]
        for node in unknown_nodes:
            # 상수와 COUNT(*)에는 의도적으로 값 출처가 없다. 다만
            # placeholder는 SQLGlot이 참조 컬럼을 해석하지 못했다는 뜻이므로
            # 보고서에 남긴다.
            if isinstance(node.expression, exp.Placeholder):
                _append_error(errors, f"unmappable lineage leaf: {node.name}")

        for node in source_nodes:
            raw_column = _leaf_column(node.name)
            if raw_column is None:
                _append_error(errors, f"unmappable wildcard lineage leaf: {node.name}")
                continue
            table = _resolve_table(node.expression, catalog, default_schema, dialect)
            if table is None:
                _append_error(
                    errors,
                    f"unmappable table lineage leaf: {node.expression.sql(dialect=dialect)}",
                )
                continue
            column = _resolve_column(raw_column, table["columns"], dialect)
            if column is None:
                _append_error(
                    errors,
                    f"unmappable column lineage leaf: {table['schema']}.{table['name']}.{raw_column}",
                )
                continue
            source = f"{table['schema']}.{table['name']}.{column}"
            if output is not None:
                pairs.add((output, source))

    return {
        "name": name,
        "lineage": [[output, source] for output, source in sorted(pairs)],
        "errors": errors,
    }


def _case_output_columns(catalog, case_name, default_schema, dialect):
    requested = f"acc_{case_name}"
    candidates = [
        table
        for table in catalog["tables"]
        if table["kind"] in {"view", "materialized-view", "materialized_view"}
        and _name_matches(table["schema"], default_schema, False, dialect)
        and _name_matches(table["name"], requested, False, dialect)
    ]
    if len(candidates) != 1:
        return None
    return list(candidates[0]["columns"])


def _resolve_output_name(raw_name, output_columns, dialect, errors):
    if output_columns is None:
        return None
    requested = _unquote(raw_name.rsplit(".", 1)[-1])
    if _is_postgres(dialect):
        exact = [name for name in output_columns if name == requested]
        matches = exact or [name for name in output_columns if name == requested.lower()]
    else:
        matches = [
            name for name in output_columns if name.casefold() == requested.casefold()
        ]
    if len(matches) == 1:
        return matches[0]
    if not matches:
        _append_error(errors, f"unmappable output column: {raw_name}")
    else:
        _append_error(errors, f"ambiguous output column: {raw_name}")
    return None


def _resolve_table(table_expr, catalog: Mapping[str, Any], default_schema: str, dialect: str):
    table_name = table_expr.name
    table_identifier = table_expr.this
    table_quoted = bool(getattr(table_identifier, "args", {}).get("quoted", False))
    db = table_expr.args.get("db")
    schema_name = db.name if db is not None else default_schema
    schema_quoted = bool(getattr(db, "args", {}).get("quoted", False)) if db is not None else False
    candidates = [
        table
        for table in catalog["tables"]
        if _name_matches(table["schema"], schema_name, schema_quoted, dialect)
        and _name_matches(table["name"], table_name, table_quoted, dialect)
    ]
    return candidates[0] if len(candidates) == 1 else None


def _resolve_column(raw_column: str, columns: Mapping[str, str], dialect: str):
    quoted = _is_quoted(raw_column)
    name = _unquote(raw_column)
    matches = [
        column
        for column in columns
        if _name_matches(column, name, quoted, dialect)
    ]
    return matches[0] if len(matches) == 1 else None


def _leaf_column(node_name: str) -> str | None:
    raw = node_name.rsplit(".", 1)[-1]
    if raw == "*":
        return None
    return raw


def _append_error(errors: list[str], message: str) -> None:
    if message not in errors:
        errors.append(message)


def _same_schema(left: str, right: str, dialect: str) -> bool:
    return _name_matches(left, right, False, dialect)


def _name_matches(actual: str, requested: str, requested_quoted: bool, dialect: str) -> bool:
    if _is_postgres(dialect):
        if requested_quoted:
            return actual == requested
        return actual == requested.lower()
    return actual.casefold() == requested.casefold()


def _is_quoted(value: str) -> bool:
    return len(value) >= 2 and value[0] == value[-1] and value[0] in {'"', "`", "["}


def _unquote(value: str) -> str:
    if len(value) >= 2 and value[0] == value[-1] and value[0] in {'"', "`"}:
        return value[1:-1].replace(value[0] * 2, value[0])
    if len(value) >= 2 and value[0] == "[" and value[-1] == "]":
        return value[1:-1].replace("]]", "]")
    return value


def _is_postgres(dialect: str) -> bool:
    return dialect.lower().replace("-", "") in {"postgres", "postgresql"}


__all__ = ["evaluate"]
