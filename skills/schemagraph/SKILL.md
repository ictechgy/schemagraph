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
schemagraph query <object> [--depth N]          # who uses / what it uses
schemagraph impact <object>                     # what breaks if it changes
schemagraph dead                                # no-internal-consumer candidates
schemagraph cycles [--level object|column]      # FK cycles
schemagraph rules [--strict]                    # declared rules, CI gate
schemagraph stats                               # collected usage evidence
schemagraph graph --format mermaid|json|dot     # render
```

## The output contract — read this before interpreting anything

- **Nothing here is a deletion verdict.** `dead` reports objects with no
  *database-internal* consumer. Application queries are outside the graph —
  an object with no dependents may be the hottest table in the product.
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
  `uses-sequence`, `uses-type` are dependencies.
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

- `query <x>` — direct neighbors both directions + reachability context.
- `impact <x>` — transitive dependents: what would break if `x` changed.
- `dead` — objects nothing inside the database consumes. Consumer-kind
  objects (views, routines, triggers) with no caller are the usual
  candidates; each carries its usage evidence when collected.
- `cycles` — FK cycles, useful for delete ordering and deadlock analysis.
- `stats` — every collected usage entry with `since`; `totals` shows how
  much of the graph went unobserved.

## Workflow

1. `scan` the database (or run the JDBC probe → `scan --document`).
2. Read `limitations` in graph.json **first** — know what was not visible.
3. Use `query`/`impact`/`dead`/`stats` for the question at hand.
4. When reporting candidates, quote the evidence (`usage`, `evidence`,
   `limitations`) rather than paraphrasing it into a verdict.
