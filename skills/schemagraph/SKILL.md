---
name: schemagraph
description: Read schemagraph's database dependency analysis correctly — the graph is the artifact, everything else is a query over it, and no output is a deletion verdict.
---

# schemagraph — consuming the analysis

schemagraph reads a live database catalog (or a probe-produced catalog
document), parses routine/trigger/view bodies, and emits **graph.json** —
the artifact. Every command after `scan` is a query over that artifact.

```bash
schemagraph scan postgres://… -o graph.json     # build the artifact
schemagraph scan --document catalog.json -o graph.json   # from a probe doc
schemagraph search <pattern> [--detail summary]  # find ids by name fragment or glob first
schemagraph query <object> [--depth N]          # who uses / what it uses
schemagraph impact <object>                     # what breaks if it changes
schemagraph dead                                # no-internal-consumer candidates
schemagraph cycles [--level object|column]      # dependency cycles
schemagraph rules [--strict]                    # declared rules, CI gate
schemagraph stats                               # collected usage evidence
schemagraph document-capabilities               # catalog versions and required features
schemagraph graph --format mermaid|json|dot|html # render
schemagraph diagnostics                         # body coverage and located diagnostics
schemagraph explain <object>                    # incident evidence and source origins
schemagraph path <from> <to>                     # shortest dependency paths
schemagraph review before.json after.json --strict --require-complete
schemagraph lint --strict                       # schema facts and unresolved references
schemagraph merge app.json reporting.json       # distinct source-id namespaces
schemagraph serve --graph graph.json            # read-only MCP over one snapshot
```

## The output contract — read this before interpreting anything

- **Nothing here is a deletion verdict.** `dead` reports objects with no
  reachable consumer under the declared retention policy. Application queries
  appear only when collected using `scan --sql-dir`; other uses need explicit
  retention roots. An object with no dependents may still be heavily used.
  Report candidates with their evidence; let the human decide.
- **`usage` is evidence, not proof.** `usage.reads`/`usage.writes` come from
  the database's own statistics (`pg_stat_*`, `sys.schema_*`). They cover
  only what the engine observed since `usage.since`. A missing `usage` key
  means *not collected* — it is **not** zero. Zero values mean zero was
  actually observed.
- **Check `limitations` in every response.** They are counted per-scan, not
  boilerplate: unparsed bodies, unsupported routine languages, unsupported
  stats sources, member-id collisions. An absent object may be an object the
  tool could not see, not an object that does not exist.
- **`contains` is structure, not dependency.** It means "this member belongs
  to that object". Only `references`, `reads`, `writes`, `calls`, `fires`,
  `uses-sequence`, `uses-type`, `derives-from`, and `depends-on` are dependencies.
- **Lineage and catalog dependencies are distinct.** `derives-from` identifies
  output-column value sources. `reads` includes filter and join dependencies.
  `depends-on` preserves a raw DB catalog dependency without inventing its
  read/write/call meaning.
- **Coverage is structured.** Object analysis states are `complete`, `partial`,
  or `unsupported` within a stated scope. Missing analysis is unavailable, not
  success. Original body hashes and available locations accompany diagnostics
  and origins; raw SQL is not embedded in graph analysis records.
- **Traversal and output limits are separate.** Check `complete`, `truncated`,
  and `truncationReasons`. A `result-limit` can hide results after complete
  traversal; vertex/edge budgets can leave traversal incomplete. Never infer
  absence from an incomplete traversal.
- **Edges carry `evidence`.** `catalog` evidence is schema fact; `body-parse`
  evidence came from parsing routine/trigger/view SQL and is conservative —
  the parser reports what it could not parse rather than guessing.
- **Vertex ids may carry a `@kind` suffix** (`orders.sku@index`). The
  database can give a column, an index, and a constraint the same name; when
  they collide the later vertex is renamed and the collision is listed in
  `limitations`. Match by `id` exactly — do not strip the suffix.
- **`notFound` still carries `limitations`.** "Not in the graph" and
  "the tool could not see it" are different answers.

## What the queries answer

- `search <pattern>` — ids matching a case-insensitive name fragment or a
  `*`/`?` glob over the whole id. Start with the default `names` detail and ask
  for `summary` only for candidates; `total` and `truncated` say what was cut.
- `query <x>` — direct neighbors both directions + reachability context.
- `impact <x>` — transitive dependents that may be affected if `x` changed.
- Version 0.4.2 and later support Ctrl+C for `query`, `impact`, `path`, and
  `review` (exit 130). A traversal stopped by cancellation reports `cancelled`
  in `truncationReasons`; partial results never establish absence. MCP clients
  cancel by request ID; cancelled calls may have no response. See ANALYSIS.md
  for preparation/output boundaries and completion races.
- `dead` — objects nothing inside the database consumes. Consumer-kind
  objects (views, routines, packages) with no caller are the usual
  candidates; each carries its usage evidence when collected.
- `cycles` — dependency cycles, useful for delete ordering and deadlock analysis.
- `unused` — tables and indexes with zero observed reads since `usage.since`.
  Objects without counters are `unobserved`, not unused. Quote the attached
  facts (`enforcesUniqueness`, `coversForeignKeys`, `bodyDependents`, ...);
  a candidate is an observation, not a deletion verdict.
- `openlineage --namespace <uri>` — column lineage as OpenLineage RunEvents
  (NDJSON). `DIRECT` has no subtype; `INDIRECT` covers only JOIN and FILTER, and
  only for views. Partial analysis appears as a `schemagraphAnalysis` job facet.
- `stats` — every collected usage entry with `since`; `totals` shows how
  much of the graph went unobserved.
- `dead --retain <glob>` protects a root and its dependency closure. TOML
  suppressions require a reason, and expiry requires an explicit `--as-of`
  date. Strict mode uses the full unsuppressed count even when output is cut.
- `review` requires catalog documents for DDL facts; removed objects are traced
  in the before graph. Strict mode returns 1 for changes requiring review and
  2 for incomparable snapshots. `--require-complete` also fails on incomplete
  analysis or truncation. It does not execute migrations or prove runtime breakage.
- `lint` distinguishes confirmed facts from unverified metadata. Partial indexes
  may be useful even when no unfiltered FK prefix index was observed. It also
  reports FK declared-type mismatches, plus advisory tables without a primary
  key (confirmed only for a complete catalog) and always-unverified
  duplicate-index candidates. `--strict` fails on `blockingCount`; findings with
  `advisory: true` never fail it.
- `merge` requires unique logical source IDs. Database catalog references cross
  namespaces only when the collected target database/object is unambiguous.

## Workflow

1. `scan` the database (or run a JDBC/Go probe → `scan --document`).
2. Read `limitations` in graph.json **first** — know what was not visible.
3. Use `query`/`impact`/`dead`/`stats` for the question at hand.
4. When reporting candidates, quote the evidence (`usage`, `evidence`,
   `limitations`) rather than paraphrasing it into a verdict.

## Catalog compatibility

The engine accepts catalog v1 and v2, reads graph v1/v2, and emits graph v2. Both probes
default to catalog v1. Query `document-capabilities` before selecting
`--document-version 2`. Unknown required features and incomplete v2 NDJSON
are errors; do not silently downgrade them. See [CATALOG.md](https://github.com/ictechgy/schemagraph/blob/main/CATALOG.md)
in the source repository for the producer and required-feature contract.

Catalog v2 query records require `external-queries-v1`. Explicit DB dependencies
require `catalog-dependencies-v1` in v2. A collection context distinguishes
logical source identity, database, schema filter, and catalog completeness.
Do not treat different filters, missing identity, or incomplete collection as a
verified DROP. Query tools read a graph only; `serve` provides no arbitrary file
or database access tools. See the [analysis guide](https://github.com/ictechgy/schemagraph/blob/main/ANALYSIS.md)
for cache, limits, HTML, and MCP usage.
