# schemagraph

schemagraph builds a dependency graph of a database schema — tables, columns,
views, routines, triggers, foreign keys — and answers **judgment queries** on it:

- `impact` — what breaks if this column or table is changed or dropped
- `cycles` — circular dependencies (delete ordering, batch deadlock analysis)
- `dead` — objects nothing reaches inside the database (candidates, never verdicts)
- `rules` — team architecture rules evaluated over the schema graph
- `query` — who uses this / what does it use (deterministic JSON, agent-first)

Most schema tools stop at diagrams and documentation. schemagraph goes further
in two ways: it parses view/routine/trigger **bodies** — not only declared
foreign keys — to build `reads`/`writes`/`calls` edges, and it emits
deterministic JSON designed for coding agents, reporting graph facts and
evidence rather than verdicts ("unreachable", never "safe to drop").

## Architecture

A Rust engine (graph model, `sqlparser-rs` body parsing, analysis, CLI) consumes
a single versioned **catalog document**. Documents are produced by built-in
native readers (PostgreSQL, MySQL, SQLite via `sqlx`) or — from phase P2 — by an
external JDBC probe (Kotlin) that reaches any database with a JDBC driver.

```
DB ──native/sqlx──┐
                  ├──> catalog document ──> graph ──> judgment queries
DB ──jdbc probe───┘        (versioned contract)
```

The engine never touches a database directly; the probe is a dumb extractor
that moves catalog rows and body text. Parse failures are measured and
reported in `limitations`, not hidden.

## JDBC probe (any database with a driver)

For databases without a native reader, `probe/` builds a fat jar that emits
the same versioned catalog document over JDBC:

```
gradle -p probe shadowJar
java -jar probe/build/libs/schemagraph-probe-all.jar \
    --url jdbc:h2:file:/tmp/mydb -o catalog.json
schemagraph scan --document catalog.json -o graph.json
```

Bundled drivers: PostgreSQL, H2, SQLite (permissive licenses only). Any other
database — Oracle, MSSQL, MySQL, DB2 — works via `--driver /path/to.jar`
(and `--driver-class` when ServiceLoader can't find the implementation).
`DatabaseMetaData` provides the portable baseline (schemas, tables, columns,
PK/FK, indexes); view/trigger/routine bodies are harvested best-effort from
`INFORMATION_SCHEMA` (`ALL_*` on Oracle), with failures reported in
`limitations`. On MySQL the probe's dependency edges match the native
reader's exactly.

## Commands

```
schemagraph scan <url> [-o graph.json]   # sqlite:PATH, postgres://…, mysql://…
schemagraph scan --document catalog.json # probe output → graph
schemagraph graph --format mermaid|json|dot [--level schema|object|column]
schemagraph query <object> [--depth N]
schemagraph impact <object>
schemagraph cycles [--level object|column] [--strict]
schemagraph dead [--strict]
schemagraph stats                          # collected usage evidence
schemagraph rules [--config schemagraph.toml] [--strict]
```

`--strict` exits 1 when findings are reported, for CI gates.

## Rules file

`schemagraph rules` reads a TOML file (default `schemagraph.toml`). Each rule
forbids dependency edges matching a `from` glob → `to` glob; `*` covers any
characters including dots, `?` covers exactly one.

```toml
[[rule]]
name = "reporting must not write to core"
from = "reporting.*"
to = "core.*"
kinds = ["writes"]           # optional; default = all dependency kinds

[[rule]]
name = "views must not call routines"
from = "*.order_totals"
to = "*"
kinds = ["calls"]
```

Output lists every violating edge with its rule name; `checked: 0` means no
rules were evaluated — not "pass".

## Usage statistics

PostgreSQL (`pg_stat_user_tables`/`_indexes`) and MySQL
(`sys.schema_table_statistics`/`schema_unused_indexes`) readers attach observed
usage to vertices. `stats` lists it; `dead` candidates carry it as evidence.

The contract: a `usage` record is valid only since its `since` timestamp —
missing usage means "not collected" (SQLite, unsupported views), `reads: 0`
with usage present means "observed zero". Statistics are evidence to weigh,
never proof an object is unused: application queries are not in the graph.

## Current state

- Readers: SQLite, PostgreSQL, MySQL (native `sqlx`), plus every JDBC
  database via the probe. `mysqlx://` is explicitly unsupported.
- Body parsing: views (`reads`, object + member level), triggers
  (`writes`/`reads`/`NEW.`/`OLD.`, `EXECUTE FUNCTION` → `calls`), SQL-language
  routines (`reads`/`writes`/`calls`, including in-body `CALL`/function
  invocations). `plpgsql` and other procedural languages are reported in
  `limitations`, not parsed.
- Deterministic JSON everywhere; unknown targets never become ghost vertices —
  they land in `limitations`.

## Roadmap

- **P0** — core graph model + native readers + `scan`/`graph`/`query`/`cycles` — done
- **P1** — body parsing + `impact` + `dead` — done
- **P2** — catalog document protocol + Kotlin JDBC probe + `rules` — done
- **P3** — `stats` usage evidence + `dead` evidence — done (mermaid polish, `skill` remain)
- **P4** — MSSQL/Oracle rich probes, `diff`, inferred edges

Details and trade-offs: [DESIGN.md](DESIGN.md).
