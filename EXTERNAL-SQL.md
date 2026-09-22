# External SQL imports

External SQL import is available from version 0.5.0. It adds offline query
records to an existing catalog document as RoutineDoc records with
kind: "query" and leaves SQL interpretation to the Rust parser.

The CLI wiring is intentionally small. It reads a catalog document, imports
one adapter input, then writes two deterministic files:

~~~text
schemagraph import <catalog> --format dbt --input manifest.json \
  --project-root . --output imported.json --report import-report.json

schemagraph import <catalog> --format query-log --input queries.jsonl \
  --output imported.json --report import-report.json
~~~

The catalog must carry an explicit collection context with a nonempty
source_id, database, and collected schema. An imported database or schema
outside that context fails the import and leaves the input document untouched.

## dbt manifest

The adapter reads model nodes from the manifest. unique_id, database, schema,
and resource_type are validated. compiled_code is preferred; if it is absent,
compiled_path is read below the supplied project root.

The stable query routine name encodes the dbt unique_id in the reserved
`query_dbt_` namespace. `source` is set only when the bytes of a real relative
`.sql` file below the project root exactly match `compiled_code`; the manifest's
`original_file_path` is retained only in the report and is never read as SQL.
Traversal, absolute paths, symlinks, duplicate IDs, duplicate source paths,
missing compiled SQL, and SQL size over the configured budget fail closed.
Model target columns are not synthesized and no lineage is inferred by this
adapter.

An augmented catalog is an input snapshot, not an import workspace. If it
already contains the same adapter's reserved `query_dbt_` or `query_log_`
routine namespace, a new import by that adapter is rejected. The other adapter
may still be combined. Start another import from the original catalog when
repeating the same adapter; this preserves the existing SQL-directory and
other external query records.

## Query log JSONL

The first line is a strict header:

~~~json
{"version":1,"type":"query-log","source_id":"warehouse-prod","database":"warehouse","window_start":"2026-01-01T00:00:00Z","window_end":"2026-01-01T01:00:00Z","sampling":"1/10"}
~~~

Each following line is a strict query record:

~~~json
{"query_id":"q-1","schema":"analytics","sql":"SELECT 1","observed_at":"2026-01-01T00:20:00Z","executions":3}
~~~

The header source and database must equal the catalog context. Every row schema
must be collected and every observation must fall inside the declared UTC
window. The query routine name uses a collision-free, canonical hex encoding
of source_id and query_id in the reserved `query_log_` namespace; routine IDs
escape reserved characters such as `:`. The raw IDs, observation window,
sample count, and sampling declaration remain in ImportReport; executions is not copied into
UsageDoc, because that type cannot represent the observation window.

The adapters enforce catalog/input byte, identifier, row, per-query SQL, total
SQL, and relative path budgets. Reports include SHA-256 values for the
catalog, input, aggregate SQL, and each imported SQL entry. Import reports are
sorted by stable identity, and failed validation never writes a partial
document or report.

The CLI only publishes two new output files. Existing files, directories,
symlinks, and paths aliasing either input are rejected. Both temporary files
are written and synced before no-clobber publication; if the second publish
fails, the first file created by the command is removed. This is rollback
behavior for ordinary failures, not a cross-file atomicity guarantee if the
process is forcibly terminated between publishes. On non-Unix builds, offline
input and dbt compiled_path reads are rejected because the required no-symlink
file-descriptor walk is unavailable; use a supported Unix runner for import.

## Verified integration

`Scripts/verify-external-imports.py` runs dbt Core 1.12.5 with dbt-postgres
1.11.0 against an owned PostgreSQL 16 cluster. It compiles a Jinja `source()`
model into a real manifest v12, executes the compiled SQL, imports the manifest,
and checks the exact table/column read edges and canonical query identity.
Database-qualified SQL resolves locally only when its database matches the
catalog's declared context. The same check imports one executed query with its
observation window and verifies that no database usage counters are invented.

```sh
python3 Scripts/verify-external-imports.py --engine engine/target/debug/schemagraph \
  --dbt /path/to/dbt --postgres-bin /path/to/postgresql/bin --output imports-check.json
```

dbt's [manifest contract](https://docs.getdbt.com/reference/artifacts/manifest-json)
and [compile command](https://docs.getdbt.com/reference/commands/compile) describe
the input artifacts. This test establishes compatibility with that tested
version and PostgreSQL example, not every dbt adapter or warehouse dialect.
