# Inspecting and reviewing dependencies

The examples use a `graph.json` produced by `schemagraph scan`. Analysis consumes
that snapshot without opening a database connection. Collection context and SQL
analysis coverage are separate facts: a complete catalog can contain unsupported
routine bodies.

## Trace a dependency

```sh
schemagraph query main.orders --depth 2
schemagraph impact main.customers --max 100 --max-visited 10000 --max-examined-edges 100000
schemagraph explain main.order_totals
schemagraph path main.order_totals main.customers --depth 16 --max-paths 8
schemagraph diagnostics
```

`explain` lists incident edges with catalog/body evidence, source locations where
available, and the SHA-256 of the original SQL body. `path` reports shortest
dependency paths and preserves parallel edge kinds. `derives-from` records value
lineage between columns; `reads` also includes columns used only in filters or
joins. `depends-on` preserves a database catalog dependency without claiming it
is a read, write, or call.

Check `complete`, `truncated`, `truncationReasons`, and `limitations` before
treating a missing result as evidence. `--max` limits displayed results after
traversal. The separate vertex and edge budgets can stop traversal itself.
`path` uses `--max-edges`; `query`, `impact`, and `review` use
`--max-examined-edges`. Library callers can pass cancellation flags to path and
budgeted traversal. CLI/MCP calls are synchronous and do not promise cooperative
cancellation of an in-flight request.

Graph version 2 includes optional object analysis, source origins, and schema
metadata. Version 1 graphs remain readable; missing analysis or lint metadata
is unavailable coverage. The JSON Schemas in [schemas/](schemas/) describe the
graph and catalog wire formats.

## Declare application entry points

```toml
retain = ["main.order_totals", "main.public_api*"]

[[suppress]]
pattern = "main.old_report"
reason = "Scheduled export still calls this view"
until = "2026-12-31"
```

```sh
schemagraph dead --config schemagraph.toml --as-of 2026-09-21 --strict
schemagraph dead --retain 'main.public_api*'
```

Retained objects protect their transitive dependency closure. Suppressions
annotate actual candidates and require a reason. Expiry evaluation requires an
explicit `--as-of` date so the same input is reproducible. Expired or unmatched
entries are reported. `--strict` uses the full unsuppressed candidate count,
including candidates hidden by the output limit.

These are reachability facts, not permission to delete database objects. A
missing usage record is different from observed zero activity; neither is an
application entry-point inventory.

## Review a schema change

Capture both documents with a stable logical label and the same producer and
schema filter. Do not use a connection URL as the label.

```sh
schemagraph scan "sqlite:before.db" --source-id app --emit-document before.json -o before.graph.json
schemagraph scan "sqlite:after.db" --source-id app --emit-document after.json -o after.graph.json
schemagraph review before.json after.json --strict --require-complete
schemagraph review before.json after.json --format markdown
```

The review reports catalog changes and traces affected dependents in the before
graph when the changed object existed there. It distinguishes ordinary additions
from changes requiring review, including removals, type/nullability changes,
required columns without a default, and changed definitions. This is a review
signal, not a prediction that every reported dependent will fail at runtime.

`--strict` exits 1 for changes requiring review and 2 when snapshot identity or
collection scope cannot be compared. `--require-complete` also exits 2 for
partial analysis or truncated results. Graph-only `diff` compares graph
structure; use catalog documents for column definitions and other DDL facts.

```sh
schemagraph lint --graph after.graph.json --strict
```

Lint checks ordered foreign-key prefixes supplied by either an unfiltered index
or the table primary key, and surfaces structured unresolved/ambiguous SQL
references. A partial or expression index, incomplete inventory, or missing PK
column positions cannot establish complete coverage and is reported as
`unverified`. A finding describes observed facts; it is not an instruction to
add or drop an index. Strict lint exits 1 for confirmed findings, 2 for
incomplete coverage, and 0 for a complete report without confirmed findings.

## Add SQL files and reuse parsing work

```sh
schemagraph scan --document catalog.json --sql-dir ./queries --query-schema main \
  --cache-dir .schemagraph-cache --emit-document with-queries.json -o graph.json
```

SQL files become `query` vertices named from their relative paths. The collector
preserves UTF-8 SQL and relative source paths; the Rust parser resolves dependencies.
Files are read recursively in sorted order. Symlinks, `.git`, `target`, `build`,
and `node_modules` are excluded. A failed read or invalid file leaves the input
document unchanged. Exporting query records requires catalog version 2 and the
`external-queries-v1` required feature; the example selects v2 automatically.

The optional cache stores body analysis with provenance, not raw SQL, current
catalog facts, or usage counters. Its namespace includes the running executable,
dialect, and structural catalog; a changed body invalidates that body's entry,
while changed catalog structure invalidates the namespace. Corrupt or oversized
entries are reported on stderr and reparsed. Cache counts also go to stderr so
graph JSON stays machine-readable. Cache files are disposable build artifacts.

## Collect and combine DB catalog references

```sh
schemagraph scan "$DATABASE_URL" --source-id app --catalog-dependencies \
  --emit-document app.json -o app.graph.json
schemagraph merge app.json reporting.json -o combined.graph.json
```

The optional dependency catalog collector supports PostgreSQL natively, and
PostgreSQL, SQL Server, and Oracle through the probes. The probes accept the same
`--catalog-dependencies` and `--source-id` options. Missing permissions or
unsupported catalogs produce measured limitations.

Merge requires a distinct nonempty source ID in every document. Each database is
parsed independently, then IDs become `source-id::local-id`. Cross-database edges
are added only for explicit catalog facts whose target database and object resolve
uniquely among the supplied documents. Missing or ambiguous targets remain
limitations. This does not resolve arbitrary dynamic SQL or create remote nodes
for databases that were not collected.

## Share or serve a snapshot

```sh
schemagraph graph --format html > report.html
schemagraph serve --graph graph.json
```

The HTML report is a single offline file with search, a selected object, bounded
neighbors, and analysis evidence. It embeds the catalog-derived names and graph
metadata; share it with the same audience as the graph snapshot.

`serve` provides JSON-RPC MCP tools over stdin/stdout: `query`, `impact`, `explain`,
`path`, and `diagnostics`. It loads one graph at startup and exposes no database
connection or arbitrary file-reading tools. Configure the MCP client to launch
the command above. The tool results preserve the CLI analysis contract and
explicit result/traversal limits.
