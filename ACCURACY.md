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
  `v1.4.5`: SQLite and MySQL DDL and indexes. Row inserts are omitted, the UTF-8
  BOM is removed, and line endings are normalized. The MySQL copy also omits
  database creation, deletion, and selection statements; the runner owns a fresh
  `chinook` database.

Pinned commits, upstream URLs, transformations, and SHA-256 digests are in
[sources.json](Fixtures/accuracy/sources.json). Upstream license notices are
preserved beside each schema. Tests verify the checked-in schema digests; no
schema downloads are needed. The MySQL/MariaDB runner pulls pinned official
Docker images if they are not already cached.

There are three reference methods:

1. PostgreSQL's `pg_depend` records independently check direct relation and
   column reads for every view in the corpus. Routine calls, types, and
   sequences are outside this particular comparison. Catalog dependency rows
   do not describe which output column receives a value.
2. Reviewed reporting queries have explicit read sets and output-column source
   sets in [Chinook cases](Fixtures/accuracy/chinook-cases.json) and
   [Pagila cases](Fixtures/accuracy/pagila-cases.json), plus the separate
   [MySQL/MariaDB cases](Fixtures/accuracy/chinook-mysql-cases.json).
   Join/filter/group/order references remain reads even when they are not output
   value sources.
3. MySQL's [`INFORMATION_SCHEMA.VIEW_TABLE_USAGE`](https://dev.mysql.com/doc/refman/8.4/en/information-schema-view-table-usage-table.html)
   independently checks relation reads, queried as the temporary database
   administrator to avoid privilege filtering. It supplies neither column reads
   nor output lineage. The runner checks whether the table exists and records its
   absence on MariaDB 11.4.13; it does not count that as an empty reference set.

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

## Original regression results

Measured on 2026-09-21 with PostgreSQL 16.13 and SQLite 3.53.4: 15 reviewed
Chinook queries, 12 reviewed Pagila queries, and eight upstream Pagila views
(35 distinct view definitions). The fixed engine is the source build at
[`ad0e720`](https://github.com/ictechgy/schemagraph/commit/ad0e720ce73e118e1f975149be7af025eb127bd6),
compared with the published `schemagraph-cli 0.4.0`. That source build reported
version `0.4.0`; executable hashes distinguish it from the published binary.
Version 0.4.1 includes these fixes.

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

At this stage, three original Pagila views (`actor_info`, `film_list`, and
`nicer_but_slower_film_list`) reported `partial` with `SG_CALL_UNRESOLVED`.
Their direct read dependencies matched the database, but the collector omitted
the custom aggregate `public.group_concat`. The final view also contained a
quoted lowercase `"substring"` call that required a parser correction.

## 0.4.1 validation

Sixteen additional queries (eight per database) were written and database-checked
without inspecting analyzer output, then frozen at
[`469910a`](https://github.com/ictechgy/schemagraph/commit/469910abd790fc6d55f7c76d1d1b94e458713932)
before their first engine run. Their first result was 62/64 correct value-lineage
pairs, with two missing pairs and no unexpected pairs; all 129 read dependencies
matched. Both mismatches involved `INTERSECT`, whose right-side value sources
were omitted. The expected facts were unchanged when the parser was fixed.
Both branches now contribute value sources, while `EXCEPT` retains only the
left branch's value sources and reads both branches. This is the project's
explicit lineage policy for the [set-operation semantics](https://www.postgresql.org/docs/16/queries-union.html).

After the fix, all 16 cases match (64 lineage pairs). They now form a regression
set too; the final 100% result is not an untouched holdout estimate.
The original 27-query regression set and the new validation cohort stay separate
in reports. Together with eight upstream views, they cover 51 view definitions:

| Analyzer, on the current collector's catalog | Correct pairs | Unexpected | Missing | Precision | Recall |
| --- | ---: | ---: | ---: | ---: | ---: |
| schemagraph 0.4.0 parser | 124 | 4 | 6 | 96.88% | 95.38% |
| schemagraph 0.4.1 | 130 | 0 | 0 | 100% | 100% |
| SQLGlot 30.18.0 adapter | 125 | 5 | 5 | 96.15% | 96.15% |

The current engine also matches 273 reviewed reads and 269 PostgreSQL catalog
reads across 28 views. These overlapping categories are scored separately.
The baseline parser comparison uses the **current collector's inventory**,
including aggregate identities; it does not score the old collector's omissions.

Native, Go, and JDBC collectors now preserve PostgreSQL aggregate/window
identities and server signatures. The three original views above resolve their
calls and report `complete`. The aggregate itself has no collected SQL body and
continues to report `unsupported` body analysis; its SQL definition is never
invented. With `--catalog-dependencies`, raw `pg_depend` references preserve
aggregate dependencies on user-defined transition, final, and combine functions.
The integration fixture executes an aggregate, checks required and forbidden
overload references, and exercises both probes' v1/v2 JSON/NDJSON transport.
See [the catalog contract](CATALOG.md) for JDBC identity compatibility details and
[PostgreSQL's aggregate catalog](https://www.postgresql.org/docs/16/catalog-pg-aggregate.html)
for the underlying support-function metadata.

## MySQL and MariaDB validation

Sixteen additional queries were written against the pinned Chinook MySQL schema,
accepted by both databases, and frozen at
[`8df2996`](https://github.com/ictechgy/schemagraph/commit/8df2996)
before any analyzer output was inspected. Database validation caught one SQL
portability issue: MariaDB rejected a frame on `ROW_NUMBER`. The final query uses
an unframed named window for ranking and a framed named window for `AVG`; its
expected dependencies did not change. This was a fixture correction before
evaluation, not an engine defect.

On 2026-09-21, the published **schemagraph-cli 0.4.2** passed all 16 cases on both
MySQL 8.4.11 and MariaDB 11.4.13. Each database is scored twice: once with the
original SQL and once with its native catalog definitions. All four runs matched
53 value-lineage pairs and 91 reviewed reads, with no unexpected edges, missing
edges, incorrect analysis states, or phantom endpoints. MySQL's independent
catalog reference also matched all 26 relation reads in each mode. These are
repeated checks of the same 16 definitions, not 64 distinct queries; overlapping
read categories are not added together. No engine changes were needed.

The cases cover mixed-case quoted columns and aliases, aggregate aliases,
`IF`/`IFNULL`/`CASE`, correlated queries, chained CTEs, derived wildcards,
ordered `GROUP_CONCAT`, set operations, outer `USING`/`NATURAL` joins, and named
and inline windows. They do not cover stored routines, triggers, dynamic SQL,
other SQL modes, `lower_case_table_names` settings, or production distributions.
They now form a regression set, not a general product-accuracy estimate.

The optional SQLGlot 30.18.0 adapter produced these value-lineage results on the
same catalog inventory and SQL:

| Input | Correct pairs | Unexpected | Missing |
| --- | ---: | ---: | ---: |
| Original SQL, either server | 47/53 | 3 | 6 |
| MySQL native definitions | 49/53 | 1 | 4 |
| MariaDB native definitions | 53/53 | 1 | 0 |

The adapter uses SQLGlot's MySQL dialect for both servers and passes `UNKNOWN`
types. Differences include unresolved mixed-case quoted columns, named-window
sources, merged outer-join keys, and the project's left-only value-source policy
for `EXCEPT`. The different native-definition scores show why server-normalized
SQL is scored separately. The reports retain adapter errors. These numbers compare this API
adapter and the project's lineage policy; they are not a product ranking.

[`mysql-environments.json`](Fixtures/accuracy/mysql-environments.json) pins
official image digests and the explicit SQL mode
`ONLY_FULL_GROUP_BY,STRICT_TRANS_TABLES,NO_ENGINE_SUBSTITUTION`. Reports record
the server version, SQL mode, `lower_case_table_names` (0 in these runs), database
character set, image identity, case digest, and executable identity. The runner
starts the two databases sequentially, each with a 1 GiB memory cap and a random
loopback port. It uses no existing database or mounted host directory and checks
its ownership label before removing its containers and anonymous volumes.
Only the requested report persists after successful or failed checks.

## Reproduce

```sh
cargo build --manifest-path engine/Cargo.toml --locked
python3 Scripts/verify-accuracy.py \
  --engine engine/target/debug/schemagraph \
  --pg-bin /opt/homebrew/opt/postgresql@16/bin \
  --suite all --strict --output engine/target/accuracy/current.json
```

On Linux, use the installed PostgreSQL binary directory, for example
`/usr/lib/postgresql/16/bin`. Missing tools fail the check instead of silently
skipping a database. `--baseline /path/to/schemagraph-0.4.0` compares the same
frozen catalog with the published baseline; the report includes executable
versions and SHA-256 digests.
Use `--suite regression` (the default) or `--suite validation` to score the cohorts
separately. `--suite all` is used in CI.

With Docker running, evaluate MySQL and MariaDB separately from the PostgreSQL/
SQLite runner:

```sh
python3 Scripts/verify-mysql-accuracy.py \
  --engine engine/target/debug/schemagraph --strict \
  --output engine/target/accuracy/mysql-current.json
```

Use `--database mysql` or `--database mariadb` to select one server. `--validate-only`
requires no analyzer and rejects SQL that the selected database does not accept.
`--baseline /path/to/schemagraph-0.4.2` scores a second parser against the same
current collector catalog. `--strict` fails on database validation, reviewed
read/lineage or analysis-state mismatches, phantom endpoints, and available
catalog-reference mismatches. Missing Docker or a failed database startup fails
the command. CI runs both servers and preserves `mysql-accuracy.json` alongside
the existing PostgreSQL/SQLite report.

To compare the supported value-lineage cases using SQLGlot's documented
[lineage API](https://sqlglot.com/sqlglot/lineage.html):

```sh
python3 -m venv engine/target/accuracy/tools
engine/target/accuracy/tools/bin/pip install 'sqlglot==30.18.0'
engine/target/accuracy/tools/bin/python Scripts/verify-accuracy.py \
  --engine engine/target/debug/schemagraph --sqlglot \
  --suite all --strict --output engine/target/accuracy/comparison.json
```

SQLGlot is a development comparison dependency. It is not part of the Rust
engine or either probe. The corpus has no row data, so it does not verify
data-dependent dynamic SQL or runtime query performance.
The same comparison environment can run `Scripts/verify-mysql-accuracy.py`
with `--sqlglot`; comparator differences do not change the independent expected
facts or fail the schemagraph regression gate.
