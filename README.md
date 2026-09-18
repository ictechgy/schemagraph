# schemagraph

> **Status: design phase.** The authoritative design lives in [DESIGN.md](DESIGN.md)
> (Korean). No code yet.

schemagraph builds a dependency graph of a database schema — tables, columns,
views, routines, triggers, foreign keys — and answers **judgment queries** on it:

- `impact` — what breaks if this column or table is changed or dropped
- `cycles` — circular dependencies (delete ordering, batch deadlock analysis)
- `dead` — objects nothing reaches, with usage-statistics evidence attached
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

## Commands (planned)

```
schemagraph scan <url> [-o graph.json]
schemagraph graph --format mermaid|json|dot [--level schema|object|column]
schemagraph query <object> [--depth N]
schemagraph impact <object>
schemagraph cycles [--level object|column]
schemagraph dead
schemagraph rules
schemagraph stats <url>
schemagraph diff <old.json> <new.json>
schemagraph skill
```

## Roadmap

- **P0** — core graph model + native readers (PG/MySQL/SQLite) + `scan`/`graph`/`query`/`cycles`
- **P1** — view/routine/trigger bodies via sqlparser-rs + `impact`
- **P2** — catalog document protocol + Kotlin JDBC probe ("any JDBC database") + `rules` + config
- **P3** — `stats` + `dead` + mermaid export + `skill`
- **P4** — MSSQL/Oracle rich probes, `diff`, inferred edges, Go probe if native-only distribution is demanded

Details and trade-offs: [DESIGN.md](DESIGN.md).
