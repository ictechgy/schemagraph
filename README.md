# schemagraph

<img src="icon.png" alt="schemagraph's owl mascot" width="112" height="112" align="right">

English is the canonical version of this README. [한국어 참고 번역](README.ko.md)

schemagraph builds a dependency graph of a database schema and helps you assess
the effects of a schema change. It combines declared foreign keys with
`reads`, `writes`, and `calls` dependencies extracted from view, routine, and
trigger bodies.

**The graph is the artifact; analysis runs as queries over it.** Scan once,
then inspect dependencies, trace impact, find cycles, check architecture
rules, or compare snapshots without reconnecting to the database. Reports
use deterministic JSON for scripts and coding agents; diagrams are available
as Mermaid, Graphviz DOT, and a standalone offline HTML explorer.

Column lineage, structured diagnostics, evidence paths, retention policies, and
catalog change reviews are described in [the analysis guide](ANALYSIS.md).
See [installation and CI examples](INSTALLATION.md) for release artifacts.
For features introduced in v0.5.0, see [review policies and the Action](REVIEW-ACTION.md),
[external SQL imports](EXTERNAL-SQL.md), and the [verified support matrix](SUPPORT-MATRIX.md).
The [accuracy corpus](ACCURACY.md) checks public PostgreSQL, SQLite, MySQL, and
MariaDB schemas against independently reviewed SQL cases and available database
dependency records.

## Quick start

Install the CLI with Rust and Cargo:

```sh
cargo install schemagraph-cli --version 0.5.0 --locked
```

### Build from source

Clone the repository, then install the CLI with Rust and Cargo:

```sh
git clone https://github.com/ictechgy/schemagraph.git
cd schemagraph
cargo install --path engine/cli --locked
```

### Try a sample database

With `schemagraph` on your `PATH` and the `sqlite3` CLI installed, try the
included fixture. It creates a sample database in a new temporary directory:

```sh
sg_demo_dir="$(mktemp -d)"
sqlite3 "$sg_demo_dir/shop.db" < Fixtures/sqlite/basic.sql

schemagraph scan "sqlite:$sg_demo_dir/shop.db" -o "$sg_demo_dir/shop.graph.json"
schemagraph query main.orders --graph "$sg_demo_dir/shop.graph.json"
schemagraph impact main.customers --graph "$sg_demo_dir/shop.graph.json"
schemagraph graph --graph "$sg_demo_dir/shop.graph.json" --format mermaid
```

The fixture includes a foreign-key chain, a view, a trigger, and dependency
cycles. `query main.orders` shows its dependency on `main.customers`;
`impact main.customers` also reaches the view and trigger that use it.

## Database support

| Database | Native reader | JDBC probe | Go probe |
| --- | --- | --- | --- |
| SQLite | Yes | Bundled driver | Yes |
| PostgreSQL | Yes | Bundled driver | Yes |
| MySQL / MariaDB | Yes | External MySQL driver | Yes |
| SQL Server | — | Bundled driver | Yes |
| Oracle | — | External Oracle driver | Yes |
| H2 | — | Bundled driver | — |
| Db2 LUW | — | External IBM JCC driver | — |
| Informix | — | External Informix driver | — |
| Other JDBC databases | — | Metadata baseline, with a compatible driver | — |

Native readers accept `sqlite:PATH`, `postgres://…` (or `postgresql://…`),
and `mysql://…`. MariaDB uses `mysql://`; MySQL X Protocol (`mysqlx://`) is
unsupported. JDBC URLs go to the probe, whose output the CLI reads with
`scan --document`.

The JDBC baseline collects schemas, tables, columns, keys, indexes, and
routine metadata exposed by the driver. Body and usage collection depend on
the database dialect, permissions, and available catalogs. Collection gaps
are reported in `limitations`.

## Commands

`scan` writes `graph.json` by default. Commands that query or render a graph
read that file by default; use `--graph <path>` to select another snapshot.

| Command | Purpose |
| --- | --- |
| `scan <url>` | Collect a catalog and build the dependency graph. |
| `scan --document <path>` | Build a graph from a JSON or NDJSON catalog document. |
| `graph --format mermaid\|json\|dot\|html` | Render the graph; select `--level schema\|object\|column`. |
| `query <name> --depth N` | List dependencies and dependents, with supporting edges. |
| `impact <name>` | Trace dependents that may be affected by a change. |
| `cycles` | Report dependency cycles and self-loops. |
| `dead` | Report unreachable view/routine candidates, with declared retention roots and suppressions. |
| `diagnostics` | Report SQL analysis state and located unresolved or unsupported constructs. |
| `explain <name>` | Show incident dependencies and their catalog/body provenance. |
| `path <from> <to>` | Find shortest dependency paths within explicit traversal budgets. |
| `review <before> <after>` | Combine catalog changes with dependent impact and comparison coverage. |
| `lint` | Inspect FK index-prefix facts and unresolved/ambiguous references. |
| `merge <documents>…` | Combine independently parsed DB catalogs under distinct source IDs. |
| `serve` | Expose read-only MCP tools over one preloaded graph. |
| `stats` | List collected usage evidence and collection coverage. |
| `rules --config <path>` | Check dependency edges against TOML rules. |
| `diff <old> <new>` | Compare two graph snapshots or two JSON/NDJSON catalog documents. |
| `skill` | Print the output contract and usage guide for coding agents. |
| `document-capabilities` | Report supported catalog versions, features, and formats. |

`cycles`, `dead`, `rules`, and `diff` accept `--strict`: the command still
prints its report but exits with code **1** when it finds cycles,
candidates, violations, or differences, respectively. `query` and `impact`
also exit **1** when a name cannot be resolved. Usage and engine errors exit
**2**. Use `schemagraph <command> --help` for all options.
Version 0.4.2 adds cooperative Ctrl+C and MCP request cancellation;
see [the cancellation contract](ANALYSIS.md#trace-a-dependency).

`scan --inferred` adds optional naming-based guesses for undeclared `*_id`
references. Declared foreign keys take precedence, and ambiguous matches
are reported in `limitations`. Inferred edges remain separate from dependency
evidence and do not affect dependency queries or rule checks.

## Probes

### JDBC: Kotlin / JVM

Build with Gradle and JDK 17. This example scans an existing SQLite database:

```sh
gradle -p probe shadowJar
java -jar probe/build/libs/schemagraph-probe-all.jar \
    --url jdbc:sqlite:/path/to/database.db -o catalog.json
schemagraph scan --document catalog.json -o graph.json
```

PostgreSQL, H2, SQLite, and SQL Server drivers are bundled. Supply other
drivers with `--driver /path/to/driver.jar`; add `--driver-class` if automatic
driver discovery fails. MySQL and Oracle drivers are supplied externally.

Use `--user` for the database user and `SG_DB_PASSWORD` for the password.
`--schema app,reporting` restricts collection to the named schemas; by
default, the probe collects non-system schemas.

Db2 LUW and Informix driver setup and live fixture checks are described in
[IBM.md](IBM.md). Maven Central installation, the additional GitHub Pages
repository, and publication instructions are documented in [MAVEN.md](MAVEN.md).

### Go probe

The Go probe supports SQLite, PostgreSQL, MySQL/MariaDB, Oracle, and SQL Server
without a JVM or JDBC jars. Its drivers also support `CGO_ENABLED=0` builds.
Use a Go toolchain compatible with [probe-go/go.mod](probe-go/go.mod):

```sh
(cd probe-go && CGO_ENABLED=0 go build -o schemagraph-probe-go .)
./probe-go/schemagraph-probe-go --url "$SG_DATABASE_URL" -o catalog.json
schemagraph scan --document catalog.json -o graph.json
```

Set `SG_DATABASE_URL` to `sqlite:/path/to/database.db`, `postgres://…`,
`mysql://…`, `oracle://…`, or `sqlserver://…`. A database in a MySQL URL
limits collection to that schema unless `--schema` overrides it. SQLite
connections use read-only mode. The probe also accepts `--schema`,
`--format json|ndjson`, and `--document-version 1|2`.

Version 0.4.3 and later also accept `--url-env SG_DATABASE_URL` to read the
complete URL from an exported environment variable, keeping a credential-bearing
URL out of process arguments. Choose either `--url` or `--url-env`; an unset or
empty selected variable is an error.

### Document formats

Both probes emit JSON by default and accept `--format ndjson`.
`scan --document` detects either format automatically. NDJSON uses a
`document` header, `schema` / `object` / `routine` records, and a final
`limitations` record.

Both probes default to catalog version 1 for compatibility. Select
`--document-version 2` to use the v2 producer metadata and required-feature
contract. The engine reads both versions into the same graph model. It rejects
unknown required features and incomplete v2 NDJSON streams. Use
`schemagraph document-capabilities` to choose a compatible producer format;
[CATALOG.md](CATALOG.md) specifies the wire contract and migration rules.

Both probes emit NDJSON one schema at a time. The Rust CLI decodes NDJSON
record by record and writes graph JSON without cloning the entire output.
The normalized catalog and graph still reside in memory; Go JSON output also
retains the full catalog. See [PERFORMANCE.md](PERFORMANCE.md) for measured
memory reductions, reproducible benchmarks, and the remaining limits.

## Interpreting results

Reports describe the dependencies captured in the graph. Application
queries are represented only when explicitly collected with `--sql-dir`; other
external uses can be declared with `--retain`. `dead` candidates are **not deletion
recommendations**. Tables are never `dead` candidates; views, materialized
views, functions, procedures, and packages can be.

- **Evidence and gaps:** reports carry `limitations` for observed collection
  and parsing gaps. Unsupported routine languages, unresolved references,
  and ambiguous names are reported without inventing target vertices.
- **Partial results:** `query`, `impact`, and `dead` report `truncated` when
  results are cut short by a result limit. Check it alongside `limitations`
  before treating a result as complete. Budgeted traversals separately report
  `complete`, visited/edge counts, and `truncationReasons`.
- **Stable output:** the same input document produces the same graph.
  Live scans can differ as catalog contents and usage statistics change.
- **Body coverage:** views and triggers yield table and column dependencies.
  SQL routines, PL/pgSQL, PL/SQL, and T-SQL have body parsing or statement
  extraction support, including Oracle package members. Supported dynamic
  SQL forms include complete PostgreSQL dollar-quoted commands, Oracle
  q-quoted commands, and T-SQL `EXEC(N'…')` / `EXECUTE(N'…')`. Cursor, loop,
  and return-query forms also recover literal commands and binding calls.
  Constant concatenations, an unambiguous PostgreSQL `format()` subset, and
  known text variables in straight-line code are evaluated conservatively.
  Branches, loops, uncertain writes, unsupported conversions, and unknown
  values invalidate that knowledge. A literal prefix is never treated as
  the full command. Views support scoped column binding, CTEs, derived tables,
  wildcard expansion, correlated subqueries, set operations, and value lineage.
  Recursive CTEs and unsupported constructs remain explicitly partial.

### Usage evidence

PostgreSQL supplies table, index, and routine statistics. MySQL and MariaDB
supply table statistics and unused-index observations where their statistics
catalogs are available. `stats` lists the observations; `dead` includes
available usage evidence with each candidate.

Missing `usage` means no observation was collected. A present record with
`reads: 0` means zero was observed during the collection window. Read it with
its `since` timestamp; usage alone cannot establish that an object is unused.
Disabled statistics, such as `track_functions=none` or
`performance_schema=OFF`, are reported in `limitations`.

For PostgreSQL routines, `reads` contains the call count, with optional
`total_ms` and `self_ms` timings. Snapshot diffs exclude usage changes so
changing counters do not appear as schema changes.

## Architecture rules

`rules` reads `schemagraph.toml` by default. Each `[[rule]]` forbids dependency
edges matching a `from` glob and a `to` glob. `*` matches any characters,
including dots; `?` matches one character.

```toml
[[rule]]
name = "reporting must not write to core"
from = "reporting.*"
to = "core.*"
kinds = ["writes"]

[[rule]]
name = "views must not call routines"
from = "*.order_totals"
to = "*"
kinds = ["calls"]
```

Omit `kinds` to check all dependency kinds. Run
`schemagraph rules --graph graph.json --config schemagraph.toml --strict`
to use the result as a CI gate. The report names each rule and violating
edge; `checked: 0` means no rules were evaluated.

## How it works

```text
Database ── native reader (Rust / sqlx) ──┐
Database ── JDBC probe (Kotlin) ──────────┼── catalog document ── graph ── queries
Database ── Go probe ────────────────────┘   versioned contract
```

Database access belongs to the native source adapters and probes. Probes
collect catalog metadata and SQL text; graph construction, body parsing,
and dependency analysis stay in Rust. The graph core has no external
dependencies and can be tested from file fixtures without a database.

## Development

The [CI workflow](.github/workflows/ci.yml) runs on pushes, pull requests,
and manual dispatch. It checks Rust formatting, builds and tests the engine,
builds all six crate packages, builds both probes, and runs the full database
fixture suite. Skipped checks fail CI. Bundled skill and license files must
also match their repository originals.

From the repository root:

```sh
cargo fmt --manifest-path engine/Cargo.toml --all -- --check
cargo build --manifest-path engine/Cargo.toml --locked
cargo test --manifest-path engine/Cargo.toml --locked
Scripts/verify-fixtures.sh
```

The fixture script uses SQLite, local PostgreSQL tools, Docker, and the
probe toolchains for database integration checks. External MySQL and Oracle
JDBC jars can be supplied through `SG_MYSQL_JAR` and `SG_ORACLE_JAR`.
Unavailable checks print skip warnings; an exit code of zero alone does not
mean every database was tested. Build the CLI before starting the script
and keep that binary unchanged until the run finishes.

The follow-up to the [competitive review](COMPETITIVE-ANALYSIS.md) adds scoped
column lineage, evidence diagnostics, change review, declared entry points,
catalog dependency facts, file SQL, MCP/HTML consumers, traversal budgets, and
an optional body-analysis cache. Runtime-dependent SQL remains an explicit
limitation. [HANDOFF.md](HANDOFF.md) records release and validation status.

See [DESIGN.md](DESIGN.md) for the design and output contract,
[HANDOFF.md](HANDOFF.md) for implementation status and verification notes,
and [AGENTS.md](AGENTS.md) for contributor guidance. These maintainer
documents are written in Korean.

## License

Dual-licensed under [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE), at your
option. Third-party drivers retain their own licenses.
