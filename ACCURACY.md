# SQL accuracy checks

This corpus checks dependency facts on public application-style schemas. It does
not measure accuracy on production traffic, all supported dialects, or arbitrary
SQL. The expected output is independent of schemagraph and of the optional
SQLGlot comparison.

## Inputs and reference facts

- [Pagila](https://github.com/devrimgunduz/pagila/tree/fef9675714cfba1756df4719b5e36075a7ddf90e),
  `pagila-v3.1.0`: the complete schema dump, including eight original views and
  materialized views. This version works with the PostgreSQL 16 fixture tools.
- [Chinook](https://github.com/lerocha/chinook-database/tree/4a944a942426e1f3263fe539155fb7ef92b04b4a),
  `v1.4.5`: SQLite DDL and indexes. Row inserts are omitted, the UTF-8 BOM is
  removed, and line endings are normalized.

Pinned commits, upstream URLs, transformations, and SHA-256 digests are in
[sources.json](Fixtures/accuracy/sources.json). Upstream license notices are
preserved beside each schema. Tests verify the checked-in schema digests and
perform no downloads.

There are two reference methods:

1. PostgreSQL's `pg_depend` records independently check direct relation and
   column reads for every view in the corpus. Routine calls, types, and
   sequences are outside this particular comparison. Catalog dependency rows
   do not describe which output column receives a value.
2. Reviewed reporting queries have explicit read sets and output-column source
   sets in [Chinook cases](Fixtures/accuracy/chinook-cases.json) and
   [Pagila cases](Fixtures/accuracy/pagila-cases.json). Join/filter/group/order
   references remain reads even when they are not output value sources.

Every reviewed query is accepted by its actual database as a view. For these
queries, the database supplies the output-column metadata and the original SQL
is replayed into a catalog document. This matters because PostgreSQL can rewrite
`JOIN USING` outputs as explicitly qualified columns, hiding a parser defect in
the original syntax. Both engine versions and SQLGlot receive the same catalog
and original SQL. The eight upstream Pagila view definitions retain the native
collector's SQL.

Equivalent forms provide additional checks: named versus inline windows and
`USING` versus explicit `ON`/`COALESCE` projections must preserve value sources.
`LEFT` and `RIGHT` merged keys use the preserved side; `FULL` merged keys use
both. All joined keys remain read dependencies.

## Interpretation

Reports list missing and unexpected edges per query. Precision and recall are
micro-averages within each explicitly scored category; an empty denominator is
`null`. Manual reads, manual value lineage, and PostgreSQL catalog reads are
reported separately because some queries appear in both checks.

Analysis states and diagnostics are also reported. A query reported `complete`
despite a missing or unexpected reviewed edge is listed under
`false_complete_cases`. Matching read edges does not establish complete routine
analysis or prove that an object is unused. SQLGlot errors and unmappable outputs
remain visible; its result is not used as the expected answer or as evidence of
schemagraph's complete dependency coverage.

The runner uses disposable SQLite files and a private loopback PostgreSQL
cluster. It applies Pagila as a non-superuser database owner, replacing only the
dump's `OWNER TO postgres` assignments with the fixture role. User PostgreSQL
service/password settings are excluded. Temporary databases and graphs are
removed even after a failed check; only the requested report remains.

## Measured results

Measured on 2026-09-21 with PostgreSQL 16.13 and SQLite 3.53.4: 15 reviewed
Chinook queries, 12 reviewed Pagila queries, and eight upstream Pagila views
(35 distinct view definitions). The fixed engine is the source build at
[`ad0e720`](https://github.com/ictechgy/schemagraph/commit/ad0e720ce73e118e1f975149be7af025eb127bd6),
compared with the published `schemagraph-cli 0.4.0`. The source build still
reports version `0.4.0`; executable hashes in the report distinguish the builds.
These fixes are not included in the published 0.4.0 binaries.

The 27 reviewed queries specify 66 expected output-column/source-column pairs:

| Analyzer | Correct pairs | Unexpected | Missing | Precision | Recall |
| --- | ---: | ---: | ---: | ---: | ---: |
| schemagraph 0.4.0 | 62 | 4 | 4 | 93.94% | 93.94% |
| schemagraph with fixes | 66 | 0 | 0 | 100% | 100% |
| SQLGlot 30.18.0 adapter | 61 | 5 | 5 | 92.42% | 92.42% |

Both schemagraph builds matched all 144 reviewed read dependencies and all 202
PostgreSQL catalog read dependencies across 20 views. Those categories overlap
and are not added together. Cases with incorrect reviewed edges but a `complete`
analysis state fell from six to zero.

The fixes address two observed defects:

- `LEFT`/`RIGHT JOIN USING` attributed a merged output key to both joined
  columns. It now uses the preserved side while retaining both read dependencies.
  `NATURAL` joins follow the same rule; `FULL` keeps both value sources.
- Named and inherited `WINDOW` clauses lost partition/order sources in output
  lineage. Resolution now stays within the query scope. Invalid references,
  duplicate definitions, cycles, and depth/expansion limits produce diagnostics
  and omit untrusted output lineage. Unused windows do not add output sources.

This is a regression corpus used to find and fix defects, not a held-out ranking
of products. SQLGlot receives table/column names from the same catalog, with
types set to `UNKNOWN`; the adapter uses its public lineage API and maps physical
leaves back to catalog columns. Its differences include merged outer-join keys,
named windows, and a correlated `LATERAL` query with unresolved leaves. API
coverage, the adapter, and differences in what counts as a value source can all
affect the comparison. Read-dependency coverage is not scored for SQLGlot here.

Three original Pagila views (`actor_info`, `film_list`, and
`nicer_but_slower_film_list`) still report `partial` with `SG_CALL_UNRESOLVED`
because the collector does not expose their custom aggregate `public.group_concat`
as a callable routine. Their direct read dependencies match the database. This
remaining collection gap, other dialects, and production query distributions are
outside the 100% result above.

## Reproduce

```sh
cargo build --manifest-path engine/Cargo.toml --locked
python3 Scripts/verify-accuracy.py \
  --engine engine/target/debug/schemagraph \
  --pg-bin /opt/homebrew/opt/postgresql@16/bin \
  --strict --output engine/target/accuracy/current.json
```

On Linux, use the installed PostgreSQL binary directory, for example
`/usr/lib/postgresql/16/bin`. Missing tools fail the check instead of silently
skipping a database. `--baseline /path/to/schemagraph-0.4.0` compares the same
frozen catalog with the published baseline; the report includes executable
versions and SHA-256 digests.

To compare the supported value-lineage cases using SQLGlot's documented
[lineage API](https://sqlglot.com/sqlglot/lineage.html):

```sh
python3 -m venv engine/target/accuracy/tools
engine/target/accuracy/tools/bin/pip install 'sqlglot==30.18.0'
engine/target/accuracy/tools/bin/python Scripts/verify-accuracy.py \
  --engine engine/target/debug/schemagraph --sqlglot \
  --strict --output engine/target/accuracy/comparison.json
```

SQLGlot is a development comparison dependency. It is not part of the Rust
engine or either probe. The corpus has no row data, so it does not verify
data-dependent dynamic SQL or runtime query performance.
