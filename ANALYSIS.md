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

`query` and `impact` JSON reports carry top-level `format` (`schemagraph-query` or
`schemagraph-impact`) and `version` (currently `1`), so a saved file identifies
the command that produced it. New keys may be added without a version change;
the version increases only when an existing key changes meaning or shape. A
`notFound` response (`found: false`; CLI exit code 1) is a separate shape and carries
no `format`.

Each `query` and `impact` neighbor carries `distance`, every dependency edge
kind that reaches it (`edges`), and `via`: the vertex before it on a shortest
path from the subject. Direct neighbors have the subject as `via`. When several
parents are equally close, `via` is the lexicographically smallest id, so the
same graph always gives the same report. Following `via` back to the subject
reconstructs one shortest path without calling `path` for each neighbor. The
`via` vertex can be missing from a list cut by `--max`, and an incomplete
traversal only chooses among the edges it examined. `review` findings list
their impacted objects in the same form.

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

## Find objects with no observed reads

```sh
schemagraph unused --graph graph.json
```

`unused` looks at collected usage counters instead of reachability. It reports
tables and indexes whose read counter is zero since `usage.since` (the
statistics reset or server start). Table `reads` count tuples, so a table is
also required to have no recorded scans (`usage.scans`): a queue polled while
empty reads zero tuples but accumulates scans. A table is not reported when any
of its indexes was read, because index-only scans do not count as table reads.
Objects without a usage record are counted under `unobserved`, never as
candidates.

Each candidate carries the facts that weaken a removal reading when they apply:
`enforcesUniqueness`, `backsConstraint` (a PK/UNIQUE-style constraint of the same
name), `coversForeignKeys` (only for unfiltered, complete indexes),
`metadataUnavailable` (index facts could not be checked), `bodyDependents` (SQL
bodies outside the table that reference it), `indexWithoutUsage` (index-only
reads cannot be ruled out), `scanCountUnavailable` (the collector did not record
scans), and `windowUnknown`. Writes stay visible in `usage`. The
counters only cover their window and never include use by other databases,
replicas, or periods before a reset. `--strict` exits 1 when any candidate
exists.

Coverage depends on the collector. PostgreSQL provides table and index counters
(`pg_stat_user_*`) plus table scans, skips partitioned parents (their scans are
counted on the leaf partitions), and records no table/index usage when
`track_counts` is off, reporting a limitation instead. The native, Go, and JDBC
PostgreSQL collectors are verified against a real workload, including polled
empty tables, index-only scans, partitions, and disabled counters. MySQL/MariaDB provide table counters and record only the
indexes that `sys.schema_unused_indexes` lists, so indexes that were used stay
unobserved and their tables carry `indexWithoutUsage`. SQLite has no counters,
so everything is unobserved.

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
`unverified`. It also reports three catalog facts:

- `fk-type-mismatch` (blocking): a foreign-key column's declared type differs
  from the referenced column's. Case and spacing are ignored and PostgreSQL
  `serial` shorthands compare as their integer types; other synonyms and implicit
  conversions are not interpreted. A reference that omits its target columns is
  compared with the referenced primary key. Pairs whose types were not collected
  are `unverified` without lowering report completeness.
- `table-without-primary-key` (advisory): no primary-key column was declared. It
  is `confirmed` only when the collector declared a complete catalog, because a
  zero key position cannot otherwise distinguish "no key" from "not read".
  Foreign and virtual tables are collected as tables and cannot declare a key.
- `duplicate-index` (advisory): two unfiltered indexes share key columns, order,
  and uniqueness. It is always `unverified` because the access method, operator
  class, and sort order are not collected (a B-tree and a hash index on the same
  column are different indexes). An index backing a constraint is named as the
  reference, not as the candidate.

A finding describes observed facts; it is not an instruction to add or drop an
index. Strict lint exits 1 for confirmed blocking findings (`blockingCount`), 2
for incomplete coverage, and 0 otherwise. Findings marked `advisory: true` are
reported and counted in `confirmedCount` but never fail strict mode, so adding
these rules does not change an existing strict gate.

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

## Export lineage to OpenLineage

```sh
schemagraph openlineage --graph graph.json --namespace postgres://db.example:5432 \
  --database app --event-time 2026-09-25T00:00:00Z -o lineage.ndjson
```

`openlineage` writes one OpenLineage `RunEvent` (spec 2-0-2, `eventType`
`COMPLETE`) per SQL body that produces a dataset: a view or materialized view
produces itself, and a routine, trigger, or query produces the tables it writes.
Inputs are the datasets it reads. Each output carries a `schema` facet and, when
lineage exists, a `columnLineage` facet:

- Value lineage (`derives-from`) becomes `DIRECT` input fields without a subtype,
  because the graph does not record whether a value is copied or transformed.
- For views and materialized views (a single SELECT), columns used in join and
  filter conditions become dataset-level `INDIRECT` entries with subtype `JOIN`
  or `FILTER`. Routines, triggers, and queries get no `INDIRECT` entries, since
  the graph does not record which statement (a load, a delete, ...) a condition
  belongs to; the command names such jobs on stderr. Sorting, grouping, and
  window use are not recorded separately in the graph and are not reported.
- A job whose SQL analysis is partial or unsupported carries a
  `schemagraphAnalysis` job facet (`state`, `diagnostics`), so consumers can tell
  missing lineage from absent lineage. stderr also counts graph limitations.

Jobs and datasets share the `[database.]` prefix: a view's job name equals its
output dataset name (`database.schema.object`); a routine's job name is its
graph id with the prefix. The `runId` is a version 8 UUID derived from the event
content, so fixing `--event-time` (validated as RFC 3339) makes the output
byte-for-byte reproducible and re-ingestion idempotent. This is a static
snapshot, not an observed run. Events are validated against the pinned official
schemas in `schemas/openlineage`, including their formats; ingestion by a
specific catalog (Marquez, DataHub, OpenMetadata) has not been tested.

## Export declarations to isthmus

```sh
schemagraph scan postgres://… --emit-document catalog.json -o graph.json
schemagraph facts --document catalog.json --project /path/to/repo -o schema.facts.json
schemagraph impact "$(jq -r '.facts[0].symbol.usr' schema.facts.json)" --graph graph.json
```

`facts` writes an isthmus bridge-facts v1 document (`platform: "sql"`,
`target: "persistence"`) with one `relation-decl` per table, view, materialized
view, and column. Each fact's `symbol.qualifiedName` and `symbol.usr` hold the
same value: the vertex id in the graph built from the same catalog. A consumer
that joins code-side relation uses to these declarations can pass `symbol.usr`
directly to `query` or `impact` (check that `subject.id` equals it) to continue
into in-database dependents. Graphs and facts from different catalogs do not
share ids reliably; build both from one `--emit-document` output.

## Share or serve a snapshot

```sh
schemagraph graph --format html > report.html
schemagraph serve --graph graph.json
```

The HTML report is a single offline file with search, a selected object, bounded
neighbors, and analysis evidence. It embeds the catalog-derived names and graph
metadata; share it with the same audience as the graph snapshot.

`serve` provides JSON-RPC MCP tools over stdin/stdout: `query`, `impact`, `explain`,
`path`, `diagnostics`, `search`, `dead`, `cycles`, `lint`, `stats`, and `unused`. It loads
one graph at startup and exposes no database connection or arbitrary file-reading
tools. Configure the MCP client to launch the command above. The tool results
preserve the CLI analysis contract and explicit result/traversal limits; each
tool returns the same JSON as the CLI command with the same options.

Start with `search` when the exact id is unknown. Its default `names` detail
lists ids only; request `summary` for the candidates you need. The `dead` tool
uses only the retention policy given explicitly when the server starts
(`serve --config policy.toml --retain <glob> --as-of YYYY-MM-DD`). Unlike the
`dead` command, `serve` does not read `schemagraph.toml` from its working
directory, because the MCP client chooses that directory; tool arguments never
name a file. Search `kind` labels are case-insensitive.
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
