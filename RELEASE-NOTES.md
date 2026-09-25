# schemagraph 0.5.1

This patch release fixes PostgreSQL materialized-view column collection and
adds `facts`, an export of catalog declarations for isthmus.

## PostgreSQL materialized views

- The native and Go PostgreSQL collectors now read materialized-view columns.
  In 0.5.0 these columns were empty, while the catalog visibility check still
  counted them. Owner-privileged scans of any database with a materialized view
  reported `catalog_complete: false` with a misleading permissions limitation,
  and reviews of those documents were `unverified`. The JDBC probe already
  collected these columns.
- Both collectors share one column query. Materialized-view columns follow the
  same privilege rule, `data_type` spelling, and nullability as
  `information_schema.columns` reports for tables. Materialized-view names
  remain visible through `pg_matviews` regardless of table privileges.
- Graphs of such databases now include materialized-view column vertices.
  Re-collect review baselines that were taken with 0.5.0.

## isthmus bridge-facts export

- `schemagraph facts --document <catalog>` writes an isthmus bridge-facts v1
  document (`platform: "sql"`, `target: "persistence"`). It declares tables,
  views, materialized views, and their columns as `relation-decl` facts. Each
  `symbol.qualifiedName` is the vertex id in the graph built from the same
  catalog. Incomplete catalogs carry a `catalog-coverage:` limitation so
  isthmus reports missing declarations as unverified.
- `generatedAt` fails with an explanation instead of recording a false time
  when the host clock is before 1970 or after 9999.
- Joining these documents needs an isthmus build with the persistence domain.
  isthmus 0.9.0 does not include it.

## Upgrade and validation

Install the CLI with `cargo install schemagraph-cli --version 0.5.1 --locked`,
or use the Linux x86_64/macOS arm64 release archives. Maven coordinates are
`io.github.ictechgy:schemagraph-probe:0.5.1`; the JDBC probe is unchanged apart
from its version. The Action can be referenced as `ictechgy/schemagraph@v0.5.1`.

All six Rust crates move to 0.5.1. `schemagraph-source` adds the public
`bridge_facts` module; existing public APIs, catalog v1/v2, and graph v2 are
unchanged.

`facts` output is checked in CI against declarations read directly from real
SQLite and PostgreSQL system catalogs, and against the graph from the same scan
in both directions. Consumption by isthmus was verified locally against its
development build; CI records that step as partial. Restricted-role PostgreSQL
collection with a materialized view is verified for the native, Go, and JDBC
collectors.

---

# schemagraph 0.5.0

This release adds policy-driven schema reviews, static DML column lineage,
external SQL imports, and dependency-aware analysis caching.

## Schema reviews in CI

- Review policies assign severity and a failure threshold. Fingerprints bind a
  finding to its structure and collection identity; reviewed baselines and
  dated waivers remain visible in reports.
- JSON, Markdown, and SARIF share the same review decision. SARIF uses logical
  database locations and does not invent SQL file positions.
- The consumer GitHub Action produces all three reports and a job summary, with
  bounded inputs, timeouts, immutable-input checks, and cancellation cleanup.
- Index predicates, UNIQUE index additions, and empty schema changes are now
  included in reviews. Incomplete collection and missing database identity
  cannot be suppressed into a successful comparison.

## Column dependencies and cache reuse

- INSERT SELECT, CTAS/SELECT INTO, UPDATE, and MERGE record column writes,
  expression/predicate reads, and value-lineage edges within the supported
  static subset. UNION SELECT INTO preserves both input value sources.
- Straight-line temporary relations are local symbols, never invented catalog
  vertices. Declared scalar variables and trigger transition rows are resolved
  separately from ordinary SQL columns.
- Self-updates retain read/write facts and report `SG_TEMPORAL_SELF_LINEAGE`
  with partial analysis instead of adding a false schema self-cycle.
- Unrelated catalog changes can reuse analysis with a verified resolution
  footprint. Uncertain paths use conservative whole-catalog invalidation;
  owner, write, endpoint, and provenance checks protect cached DML effects.

## External inputs and operations

- `schemagraph import` accepts dbt compiled model SQL and bounded query-log
  JSONL. Stable query identities, SQL/input hashes, collection context, and
  observation windows are preserved. Execution counts are not fabricated into
  database usage statistics.
- Imports validate file-descriptor, path, size, identity, and output ownership
  boundaries. They require a supported Unix runner and two new output files.
- PostgreSQL collection detects measured catalog omissions under restricted
  permissions. JDBC reads view/trigger definitions from native catalogs.
- A pinned, non-root Docker build, verified support matrix, and additional
  JDBC/large-body benchmarks are included. No hosted container registry is
  required to build the image from this tag.
- Maven Pages publication is explicit and builds the selected release tag;
  ordinary main merges do not publish an existing version again.

## Upgrade and validation

Install the CLI with `cargo install schemagraph-cli --version 0.5.0 --locked`,
or use the Linux x86_64/macOS arm64 release archives. Maven coordinates are
`io.github.ictechgy:schemagraph-probe:0.5.0`; the standalone probe requires
Java 17. The Action can be referenced as `ictechgy/schemagraph@v0.5.0` or by its
full source commit.

The six Rust crates move together to 0.5.0 because public parser/cache types
changed. Catalog v1/v2 and graph v2 remain supported. Local cache format changes
invalidate older entries automatically. New column coverage can expose partial
analysis in bodies previously assessed only at object level; inspect diagnostics
when adopting `review --require-complete`.

Independent DML expectations were fixed before analyzer evaluation. Validation
covers SQLite, PostgreSQL, SQL Server, and Oracle, with full uncached/cold/warm
graph equivalence; existing DB fixtures and public accuracy cases remain in CI.
The static subset, declared observation windows, and full in-memory catalog/graph
boundary still apply. These checks do not certify every SQL dialect or procedural
control path.

See [review/Action usage](REVIEW-ACTION.md), [external SQL imports](EXTERNAL-SQL.md),
[DML evidence](DML-ACCURACY.md), [support](SUPPORT-MATRIX.md), and
[performance measurements](PERFORMANCE.md).
