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

Value lineage includes both branches of `UNION` and `INTERSECT`. For `EXCEPT`,
the right branch determines which rows are excluded and contributes read
dependencies, while values retain their left-branch sources. Window partition
and order expressions contribute to the corresponding window output's lineage.

Check `complete`, `truncated`, `truncationReasons`, and `limitations` before
treating a missing result as evidence. `--max` limits displayed results after
traversal. The separate vertex and edge budgets can stop traversal itself.
`path` uses `--max-edges`; `query`, `impact`, and `review` use
`--max-examined-edges`. Library callers can pass cancellation flags to path and
budgeted traversal, including the cancellation-aware review entry point.

Cancellation support described here is available since v0.4.2.
For `query`, `impact`, `path`, and `review`, the first **Ctrl+C** requests a
cooperative stop and exits with status **130**. When traversal observes the stop,
its partial report includes `cancelled` in `truncationReasons` and `truncated: true`;
query, impact, and review also report `complete: false`. Cancellation during
review preparation may exit before a report is available. A second Ctrl+C exits
immediately. Loading, individual sorts, and output I/O are not preempted by the
first signal, so there is no fixed cancellation latency guarantee.

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
schema filter and catalog-access role. Do not use a connection URL as the label.
Collection completeness is a producer report, not an independent attestation of
database privileges: catalogs that silently filter inaccessible objects can hide
permission changes. Keep catalog visibility unchanged between snapshots.

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
Incomplete index definitions or foreign-key mappings also make review coverage
partial. Catalog dependency records with unresolved endpoints prevent a verified
comparison, including references outside the collected schema scope.

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
catalog facts, or usage counters. From v0.5.0, entries record the
relations, column shapes, and routine candidates needed for name resolution.
An unrelated catalog edit can reuse those entries; changed or newly resolvable
dependencies invalidate them. Executable, dialect, and collection context are
also part of compatibility. Bodies without a trustworthy resolution footprint
use a conservative whole-catalog namespace. Corrupt or oversized
entries are reported on stderr and reparsed. Cache counts also go to stderr so
graph JSON stays machine-readable. Cache files are disposable build artifacts.
The full catalog and graph are still rebuilt and held in memory. This caches
SQL analysis; it is not a persistent incremental graph engine. Version
v0.4.3 uses whole-catalog structural invalidation for all entries.

Version 0.5.0 DML analysis records destination-column `writes`, source and
predicate `reads`, and value-only `derives-from` edges for the supported static
INSERT SELECT, CTAS/SELECT INTO, UPDATE, MERGE, and straight-line temp-table
subset. Temporary symbols never create catalog vertices. A self-update such as
`x = x + 1` records its read/write facts and `SG_TEMPORAL_SELF_LINEAGE`, with
partial state instead of a fabricated physical self-edge. Implicit INSERT
destination shape and unknown procedural effects remain conservative. See
[the independent DML corpus](DML-ACCURACY.md) and [external imports](EXTERNAL-SQL.md).

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
`path`, `diagnostics`, `search`, `dead`, `cycles`, `lint`, and `stats`. It loads
one graph at startup and exposes no database connection or arbitrary file-reading
tools. Configure the MCP client to launch the command above. The tool results
preserve the CLI analysis contract and explicit result/traversal limits; each
tool returns the same JSON as the CLI command with the same options.

Start with `search` when the exact id is unknown. Its default `names` detail
lists ids only; request `summary` for the candidates you need. The `dead` tool
uses the retention policy given when the server starts
(`serve --config policy.toml --retain <glob> --as-of YYYY-MM-DD`, or
`schemagraph.toml` in the working directory); tool arguments never name a file.
`review` is not exposed because it compares two snapshots and the server holds one.

The server also lists two read-only resources: `schemagraph://graph/summary`
(vertex and edge counts by kind, schemas, and limitations) and
`schemagraph://skill` (the output contract printed by `schemagraph skill`).
Other URIs are rejected with the MCP resource-not-found error.

Since v0.4.2, stdin remains responsive while a single worker
executes tool calls. Use a fresh request ID for each call in the session. To
cancel an active or queued call, send the same request ID
(including its string/number type) in an MCP notification:

```json
{"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":"query-42"}}
```

The cancelled call produces no response, in accordance with the
[MCP cancellation protocol](https://modelcontextprotocol.io/specification/2025-11-25/basic/utilities/cancellation).
Other calls and `ping` remain usable. Malformed, unknown, and completed request
IDs are ignored; initialization cannot be cancelled. A completion that wins the
race may still return its response. At most 16 tool calls can be active or queued;
requests beyond that bounded capacity
receive a server-busy error, and duplicate active IDs are rejected.

`query`, `impact`, and `path` observe cancellation within traversal. Other tool
reports check at request boundaries; sorting, serialization, and blocked output
I/O are not preempted. Responses can arrive out of request order, so match them
by ID. Closing stdin drains accepted requests before shutdown; send cancellation
notifications first if those requests should be abandoned.
