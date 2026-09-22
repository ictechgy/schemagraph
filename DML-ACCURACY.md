# Authored DML accuracy corpus

`Fixtures/accuracy/dml-cases.json` is a small, authored corpus for the part of
the graph contract that is not covered by view-only lineage fixtures. The SQL,
column facts, and row results were written independently. The analyzer is not
used to produce any expected value.

The corpus has 16 cases:

- `INSERT ... SELECT` with reordered destinations, joins, expressions,
  predicates, constants, `UNION ALL`, and `COALESCE`.
- source-free `INSERT ... VALUES`.
- filtered and joined `CREATE TABLE AS SELECT` (SQL Server uses `SELECT INTO`).
- `UPDATE ... FROM`, correlated updates, and multi-column updates.
- a native `MERGE` upsert for PostgreSQL, SQL Server, and Oracle.
- two sequential temporary-table chains.
- a procedure body that writes the same destination twice.

The subject is dialect-specific and is declared by `subject_contract`:

- SQLite uses the engine's `sql_files` contract: `kind: query`,
  `source: dml/<case>.sql`, and the exact byte-hex `query_<hex-relative-path>`
  name.
- PostgreSQL uses a `function` wrapper for ordinary DML. SQL files are used for
  CTAS and temporary-table sequences because those are standalone statement
  sources. Native validation creates each function, invokes it, and checks the
  stored `pg_proc.prosrc` bytes before checking result rows.
- SQL Server and Oracle use `procedure` wrappers for ordinary DML. Their CTAS
  and temporary-table sequences are standalone SQL-file sources.

The `procedure_wrappers` templates describe the static wrappers. The catalog
preflight checks the actual raw subject name, kind, source, and body hash before
the graph is scored. CTAS on SQL Server is authored as `SELECT INTO`; native
validation executes it first, collects the persistent target, then attaches the
original SQL as its query source.

`supported_dialects` is applied consistently: runtime validation reports an
explicit skip, catalog preflight omits the case, and graph scoring does not
invent a missing subject for an inapplicable database.

Facts use logical relation names such as `source.source_id`. The top-level
`relations` map resolves each relation and column to an exact catalog ID for
SQLite, PostgreSQL, SQL Server, and Oracle. This keeps the authored expectation
independent of case folding while preventing a verifier from guessing a
column or creating a ghost endpoint.

Each case separates four scopes:

- `reads.objects` and `writes.objects` are relation-level dependencies. The
  target relation of an update is a write and is not duplicated as an object
  read, matching the graph contract.
- `reads.columns` and `writes.columns` record the finer DML contract. An
  update may therefore read the old target value while its object-level fact
  remains a write.
- `lineage` records destination-column to source-column value flow. Constants
  have no source. A self-source such as `target_value = target_value + bump`
  is retained as a semantic fact even when a graph must avoid a physical
  self-edge.
- `intermediate_writes` records temporary staging columns. Temporary objects
  are logical symbols only; the corpus never invents `temp.*`, `pg_temp.*`,
  `#temp`, or another guessed graph ID. They may be absent from a persistent
  catalog and are not required as graph vertices.

The four updates that carry a temporal self-source have `fact_state: complete`
and `state: partial`, with the required `SG_TEMPORAL_SELF_LINEAGE` limitation.
The verifier expects the non-self source edges and the diagnostic, while
reporting the self-source separately. This prevents a schema cycle from hiding
the fact that the updated value also depended on its previous value.

## Independent validation phases

The validation-only commands execute only the selected database and never start
`schemagraph`, SQLGlot, or another analyzer. The SQLite command is:

```sh
python3 Scripts/verify-dml-accuracy.py \
  --validate-only --strict --output /tmp/schemagraph-dml-validation.json
```

It creates a new in-memory database for each case, applies the authored setup
rows, executes the case SQL, and compares the exact ordered result rows with
the `runtime.sqlite` oracle. Fifteen cases execute locally. The `MERGE` case is
reported as an explicit skip because SQLite has no `MERGE` statement; it is
not counted as a pass or silently rewritten to another statement.

The verifier can be used after the case file is frozen with a producer catalog
and graph. A catalog preflight checks that the expected relations, columns, and
subject IDs have exact IDs and kinds. It also records the raw body hash for
each owner. An optional CLI scan is deliberately a separate path:

```sh
python3 Scripts/verify-dml-accuracy.py \
  --document producer-catalog.json --engine ./schemagraph \
  --dialect sqlserver --strict --output dml-score.json
```

The graph scorer rejects null IDs, duplicate vertices, missing subjects,
wrong subject kinds, missing or unexpected object and column edges, false
`complete` states, and ghost endpoints. It verifies `analysis.body_hash` and
SQL-file `analysis.source` against the raw subject. `origins` are checked in
the exported `{body_hash, from, to, kind, role}` format, so writer B cannot
cover a missing lineage origin for writer A. This also catches an unexpected
source on a source-free constant destination. DML destination columns can be
shared by several procedure bodies, so the global destination union is retained
as a secondary report; pass/fail lineage attribution uses the owning body hash.
Every expected write column is in the global lineage target scope, including
constant destinations with no expected source. Every `derives-from` edge in
that scope must have a matching exported origin, and the catalog preflight
rejects duplicate eligible subject body hashes because they cannot identify an
owner safely.

Wrapper dialect graph scoring requires `--document` so the scorer receives the
actual raw subject body hashes. It does not fabricate wrapper provenance from
the case SQL text. SQLite query-source graphs can use the direct graph path;
PostgreSQL, SQL Server, and Oracle graphs must carry their raw catalog document.

The verifier preserves the distinction between phases in its report through
`phase`, `analyzer_evaluated`, and the frozen case-file SHA-256. The self-tests exercise both phases without
running an analyzer:

```sh
python3 Scripts/test-dml-accuracy.py -v
```

The local database validation commands are:

```sh
python3 Scripts/verify-dml-accuracy.py \
  --validate-only --strict --output /tmp/schemagraph-dml-sqlite.json

python3 Scripts/verify-dml-accuracy.py \
  --dialect postgres --strict --output /tmp/schemagraph-dml-postgres.json
```

The first command passes 15 cases and explicitly skips SQLite's unsupported
`MERGE`. PostgreSQL 16.13 passes all 16 cases, including native `MERGE`, in an
owned temporary cluster: 12 wrapper functions were created, invoked, and
checked through `pg_proc.prosrc`; four standalone SQL-file cases were checked
as SQL-only. PostgreSQL, SQL Server, and Oracle producer parity
still belongs to their native CI jobs. No analyzer score should be published
for those producer runs until SQL validity, runtime rows, catalog IDs, raw body
hashes, and exported origins are captured in a first immutable baseline.

The producer baseline command uses the published v0.4.3 engine and refuses an
existing output directory. It preserves raw Go/JDBC documents, the document
after SQL-file subjects are attached, graphs, runtime rows, catalog preflight,
and score failures in per-dialect directories. The expectation bytes were frozen
in commit `776ebe8`, SHA-256
`fd129759650617b06c81ab826fb11627f5315dc72ebc1470dbdaabb10bd41d4b`,
before any analyzer evaluated them:

```sh
python3 Scripts/verify-dml-producers.py \
  --database all \
  --engine /path/to/published-v0.4.3/schemagraph \
  --go-probe /path/to/immutable/schemagraph-probe-go \
  --probe-jar /path/to/immutable/schemagraph-probe-v0.4.3-all.jar \
  --oracle-jar /path/to/ojdbc8.jar \
  --output "/path/to/verification/dml-first-baseline"
```

Operational failures are command failures. Accuracy mismatches are retained in
the baseline report; add `--strict` when they should also determine the exit
status. A full invocation requires every requested native runner and never
turns an unavailable SQL Server or Oracle runner into a successful skip.

## Current-source replay

`Scripts/verify-dml-replay.py` reads a completed producer baseline and writes a
new result directory. It leaves the frozen input and first failures unchanged.
For Oracle, the fixture's owner is mapped from the logical `DMLACC` namespace
to `SGACC`; the mapping changes qualification only and is recorded separately.

```sh
python3 Scripts/verify-dml-replay.py --engine /path/to/current/schemagraph \
  --baseline /path/to/dml-first-baseline --output /path/to/new-dml-score
```

Local current-source verification has passed SQLite's 15 applicable cases,
PostgreSQL's 16 cases, and Oracle Go/JDBC's 16 cases each. Uncached, cold-cache,
and warm-cache graph bytes match in every run, and every warm body is restored
without warnings. SQL Server's native 16-case run is checked by the dedicated
x86_64 CI job. Its result must be read from the tested commit's job, not inferred
from another dialect's passing score.

These are targeted static DML fixtures. They do not establish whole-product
accuracy, every procedural control path, or superiority over another tool.
