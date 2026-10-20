# Performance measurements

## Remote PostgreSQL collection (2026-10-20)

Both probes used to ask PostgreSQL for each object's columns, constraints,
indexes, and triggers separately. The JDBC probe called `getColumns`,
`getPrimaryKeys`, `getImportedKeys`, and its index catalog query per object.
Collection runs on one pinned connection, so these statements ran one after
another and their number grew four times as fast as the table count. Against a
remote server, collection time grew with the round-trip latency. Both probes now
read each kind once per schema and split the rows by exact relation name. The
JDBC probe does this only through pgjdbc, whose `getPrimaryKeys` and
`getImportedKeys` treat a null table as the whole schema. Other drivers keep
the per-object calls.

`Scripts/benchmark-probe-postgres.py` produced these measurements on one macOS
26.7.1 ARM64 host while other jobs ran (load average 5 to 17). It starts a
private PostgreSQL 16.13 cluster with autovacuum and parallel query turned off,
so that `pg_stat_database.xact_commit` grows by one for each autocommit
statement a probe sends, and loads one schema of tables like these:

```sql
CREATE SCHEMA "bench_000";
CREATE TABLE "bench_000"."table_0000" (id BIGINT NOT NULL PRIMARY KEY,
  parent_id BIGINT, column_02 INTEGER, column_03 INTEGER, column_04 INTEGER,
  column_05 INTEGER, column_06 INTEGER, column_07 INTEGER);
CREATE INDEX "table_0000_parent" ON "bench_000"."table_0000" (parent_id);
CREATE TABLE "bench_000"."table_0001" (id BIGINT NOT NULL PRIMARY KEY,
  parent_id BIGINT, column_02 INTEGER, column_03 INTEGER, column_04 INTEGER,
  column_05 INTEGER, column_06 INTEGER, column_07 INTEGER,
  CONSTRAINT "table_0001_fk" FOREIGN KEY (parent_id)
    REFERENCES "bench_000"."table_0000"(id));
CREATE INDEX "table_0001_parent" ON "bench_000"."table_0001" (parent_id);
-- Every later table references the one before it in the same way.
```

With `--latency-ms 1`, every collection connects through the relay in
`Scripts/latency-relay.py`. It delays each chunk by 1 ms in each direction,
which adds about 2 ms per round trip.

The previous Go probe and JAR were built from `835e054`. The previous JAR
SHA-256 starts `06f479f17b64` and the changed JAR starts `b8f62ecfdfbe`. Go is
1.27.1, and the JDBC probe ran on OpenJDK 17.0.20 with bundled pgjdbc 42.7.8.
Each cell is the median of three fresh processes after one warm-up per build,
with the builds alternating. Statements are the increase of `xact_commit` for
the benchmark database, read after the probe's session ended.

| Probe | Tables | Format | Before s | After s | Before statements | After statements |
| --- | ---: | --- | ---: | ---: | ---: | ---: |
| Go | 2,000 | JSON | 23.720 | 0.317 | 8,048 | 52 |
| Go | 2,000 | NDJSON | 26.035 | 0.303 | 8,048 | 52 |
| Go | 4,000 | JSON | 47.339 | 0.403 | 16,048 | 52 |
| Go | 4,000 | NDJSON | 51.498 | 0.373 | 16,048 | 52 |
| JDBC | 2,000 | JSON | 27.063 | 1.001 | 8,024 | 28 |
| JDBC | 4,000 | JSON | 55.469 | 1.349 | 16,024 | 28 |

Every pair wrote the same document bytes, and the relay recorded no connection
errors. Before the change each table added four statements. After it, the count
was the same at both sizes. Without the relay, 4,000 tables took 1.995 s before
and 0.322 s after for Go JSON, and 13.168 s before and 1.270 s after for JDBC
JSON. These direct times depend on host load more than the relay times do. An
earlier run on the same host at load average 3 to 7 measured them as 1.067 s
and 0.253 s for Go, and 3.923 s and 1.038 s for JDBC. That run used a scratch
relay for its relay rows and left parallel query on. Its JDBC count at 4,000
tables was 30, because PostgreSQL started parallel workers for one schema-wide
catalog query; this is why the script turns parallel query off.

JDBC `getColumns` takes LIKE patterns for its schema and table arguments. The
per-object calls therefore also collected the columns of tables or schemas whose
names matched the pattern: table `a_b` also read `a1b`, `pct%name` also read
`pctxname`, and schema `my_app` also read `myxapp`. A backslash is the LIKE
escape character, so table `back\slash` matched `backslash` instead of itself.
It read the columns of that table, or none when no such table existed, and the
catalog visibility check then reported an uncollected column. Splitting by exact
name keeps each table's own columns. Databases without such names produce the
same documents as before.

These tests check that the number of catalog queries per schema does not depend
on the table count:

```sh
(cd probe-go && go test -run TestPostgresQueriesPerSchemaDoNotGrowWithObjectCount .)
gradle -p probe test --tests schemagraph.probe.PostgresSchemaMetadataTest
```

The table comes from these commands. Build the previous Go probe and JAR from
the older commit with `git archive`, as in the Reproduce section at the end of
this file. The script builds `--new-probe` from `--probe-dir` (default
`probe-go`).

```sh
python3 Scripts/test-probe-postgres-benchmark.py
python3 Scripts/benchmark-probe-postgres.py \
  --baseline-probe /path/to/previous/schemagraph-probe-go \
  --new-probe /path/to/new/schemagraph-probe-go \
  --baseline-jar /path/to/previous/schemagraph-probe-all.jar \
  --new-jar probe/build/libs/schemagraph-probe-all.jar --java /path/to/java \
  --engine /path/to/schemagraph --pg-bin /opt/homebrew/opt/postgresql@16/bin \
  --schemas 1 --tables-per-schema 2000 --columns 8 --indexes --format json \
  --latency-ms 1 --warmup 1 --repeat 3 --output /path/to/report
```

For the NDJSON rows, use `--format ndjson` and leave out the JAR options. For
the larger rows, use `--tables-per-schema 4000`, and for the direct times,
`--latency-ms 0`. `results.json` keeps every sample with its statement count
and document SHA-256, a per-build summary, and the relay's connection errors.

## Remote MySQL and MariaDB collection (2026-10-20)

The Go probe used to ask MySQL and MariaDB for each object's primary-key
positions, columns, constraints, indexes, and triggers separately, and for each
routine's parameters. go-sql-driver/mysql sends a statement with arguments as a
prepare and an execute unless `interpolateParams` is set, so each table cost
about ten round trips on the probe's one pinned connection. The probe now reads
each kind once per schema and splits the rows by exact table name.

Constraints and their key columns come from two schema-wide queries that the
probe matches by exact schema, table, and constraint name. Joining the two
`information_schema` tables on the server made MariaDB 11.4 compare every
constraint row with every key-column row (a block nested-loop join without an
index): for 4,000 tables that one query took about 4 s, and its time grew with
the square of the table count. Read separately, each half took about 0.05 s.

Routine parameters are read once per schema as well, except for routine names
that the server's case- and accent-insensitive comparison could match with
another name. Those routines are still asked one at a time, so their signatures
stay byte-for-byte the same as before and the same as the native reader's. When
a schema has a routine name that is not printable ASCII or that ends with a
space, every routine in that schema is asked one at a time.

`Scripts/benchmark-probe-mysql.py` produced these measurements on the same
macOS 26.7.1 ARM64 host while other jobs ran (load average 2 to 7). It starts
the `mysql:8.4` and `mariadb:11.4` images pinned by digest in
`Fixtures/accuracy/mysql-environments.json` (MySQL 8.4.11 and MariaDB 11.4.13)
in Colima 0.10.2 with 2 CPUs and 4 GiB, publishes the port on 127.0.0.1 only,
and loads one schema of tables like these, with one procedure for every ten
tables:

```sql
CREATE TABLE t00000 (id BIGINT NOT NULL PRIMARY KEY, parent_id BIGINT,
  c2 INT, c3 INT, c4 INT, c5 INT, c6 INT, c7 INT,
  KEY t00000_parent (parent_id));
CREATE TABLE t00001 (id BIGINT NOT NULL PRIMARY KEY, parent_id BIGINT,
  c2 INT, c3 INT, c4 INT, c5 INT, c6 INT, c7 INT,
  KEY t00001_parent (parent_id),
  CONSTRAINT t00001_fk FOREIGN KEY (parent_id) REFERENCES t00000 (id));
-- Every later table references the one before it in the same way.
CREATE PROCEDURE p00000(IN a INT, IN b VARCHAR(10)) SELECT a, b;
```

Both servers run with their default `performance_schema` setting: on for
MySQL, so a MySQL collection also reads the two `sys` usage views, and off for
MariaDB. With `--latency-ms 1`, every collection connects through
`Scripts/latency-relay.py`, which adds about 2 ms per round trip. Statements are
the increase of the server's `Questions` status variable during one
collection, with the cost of reading it removed. `Questions` counts executes
and plain statements but not prepares, which are counted separately from
`Com_stmt_prepare`.

The previous Go probe was built from `da64c1d`, which still asks per object
and per routine, and the changed one from `92af86d`. Go is 1.27.1. Each cell is
the median of three fresh processes after one warm-up per build, with the
builds alternating.

| Server | Tables | Format | Before s | After s | Before statements | After statements |
| --- | ---: | --- | ---: | ---: | ---: | ---: |
| MySQL | 2,000 | JSON | 63.184 | 0.269 | 10,211 | 18 |
| MySQL | 2,000 | NDJSON | 65.013 | 1.757 | 10,211 | 18 |
| MySQL | 4,000 | JSON | 153.222 | 0.476 | 20,411 | 18 |
| MariaDB | 2,000 | JSON | 113.698 | 0.197 | 10,208 | 15 |
| MariaDB | 2,000 | NDJSON | 118.590 | 0.186 | 10,208 | 15 |
| MariaDB | 4,000 | JSON | 316.888 | 0.390 | 20,408 | 15 |

Every pair wrote the same document bytes, the relay recorded no connection
errors, and no other container ran in the Colima VM. Before the change, each
table added five statements and each routine one. After it, a collection sent
18 statements, 12 of them prepared, on MySQL and 15, 10 of them prepared, on
MariaDB at both sizes. Each remaining prepare belongs to one schema-wide query,
so `interpolateParams` would save at most that many round trips per schema. The
probe leaves it off, and a URL can still turn it on with
`?interpolateParams=true`. Without the relay, 4,000 tables took 15.314 s before
and 0.383 s after on MySQL, and 204.666 s and 0.309 s on MariaDB, whose
per-table catalog queries cost more on the server itself. An intermediate build
that still joined the two constraint tables on the server took 1.169 s for
2,000 and 3.994 s for 4,000 MariaDB tables through the relay.

The MySQL NDJSON row is slower for a reason outside this change. In some MySQL
containers built from the same image and fixture, the optimizer ran
`sys.schema_unused_indexes`, the index-usage query that the probe sends once
per schema both before and after the change, as a nested loop that looks up the
schema's tables again for every index-usage row, instead of a hash join. That
one query then took about 1.56 s for 2,000 tables instead of about 0.01 s. In a
later check, three of five fresh containers chose that plan. In another, two of
five were as slow, whether the probe connected through the forwarded port or
ran inside the container. It adds the same time to both builds of a row.

These tests check that the number of catalog queries per schema does not depend
on the table or routine count, that no catalog query joins `information_schema`
tables on the server, and that a constraint keeps only its own key columns:

```sh
(cd probe-go && go test -run 'TestMySQL(QueriesPerSchema|CatalogQueriesDoNotJoin|ConstraintsKeepOnlyTheirOwn)' .)
```

The table comes from these commands. Build the previous Go probe from the older
commit with `git archive`, as in the Reproduce section at the end of this file.
The script needs Docker and pulls the pinned images on first use.

```sh
python3 Scripts/test-probe-mysql-benchmark.py
python3 Scripts/benchmark-probe-mysql.py --server mysql \
  --baseline-probe /path/to/previous/schemagraph-probe-go \
  --new-probe /path/to/new/schemagraph-probe-go \
  --engine /path/to/schemagraph --tables 2000 --routines 200 --columns 8 \
  --format json --latency-ms 1 --warmup 1 --repeat 3 --output /path/to/report
```

For MariaDB, use `--server mariadb`. For the NDJSON rows, use
`--format ndjson`. For the larger rows, use `--tables 4000 --routines 400`, and
for the direct times, `--latency-ms 0`. `results.json` keeps every sample with
its statement and prepare counts and document SHA-256, a per-build summary, and
the relay's connection errors.

## PL/pgSQL routine bodies at scale (2026-10-18)

These release-mode measurements compare the 0.7.1 engine (`a2f0dffc7af4…`,
built from `a6ee7d5`) with the unreleased parser change (`ea6e2019586a…`) on
the same macOS ARM64 host. The host was shared with other work, and the two
engines ran one after the other. Each value is the median of three fresh
processes per mode.

`Scripts/benchmark-analysis.py --routines` generates PostgreSQL catalog
documents in the form the readers collect: `pg_get_functiondef` text of
PL/pgSQL functions over half as many ten-column tables. Each function reads one
table with `SELECT … INTO` and updates another. The verifier checks every
function's table reads and writes against the generated SQL in both directions
and requires every body to parse completely. Both engines produce the same
graph bytes for each size, all modes of one size produce the same bytes, and
every reused-cache run reports a hit for every function.

| Functions | Engine | No cache s | Empty cache s | Reused cache s | No-cache peak RSS MiB |
| ---: | --- | ---: | ---: | ---: | ---: |
| 1,000 | 0.7.1 | 3.018 | 3.259 | 3.040 | 36.1 |
| 1,000 | Unreleased | 0.082 | 0.297 | 0.127 | 32.8 |
| 2,000 | 0.7.1 | 12.158 | 12.402 | 11.603 | 60.9 |
| 2,000 | Unreleased | 0.153 | 0.640 | 0.215 | 55.3 |
| 4,000 | 0.7.1 | 49.769 | 52.096 | 48.590 | 342.2 |
| 4,000 | Unreleased | 0.293 | 1.497 | 0.391 | 97.7 |

From 2,000 to 4,000 functions, 0.7.1 took 4.1–4.2 times as long in every mode.
It built a cache key for every body even without a cache, and each
`CREATE FUNCTION` body hashed the whole document. The unreleased engine builds
keys only when a cache is present and computes document-wide parts once per
run. The same step takes 1.9 times as long without a cache and 1.8 times with a
reused cache. Empty-cache runs also capture and write one entry per function
and took 2.3 times as long.

```sh
python3 Scripts/benchmark-analysis.py --engine engine/target/release/schemagraph \
  --routines 1000 2000 4000 --repeat 3 --max-growth 3 \
  --output engine/target/analysis-routines
```

`--max-growth` fails the run when the no-cache or reused-cache median grows by
more than the given factor between consecutive sizes. `results.json` records
the executable and catalog hashes, verified facts, every sample, and the growth
ratios.

## JDBC and large SQL bodies (2026-09-22 follow-up)

These measurements use fixed binaries on the same macOS ARM64 host and three
fresh processes per format/mode. They are observations of these fixtures, not
general database throughput guarantees. Earlier results below retain their
original executable identities.

### JDBC: one schema with 10,000 tables

The SQLite DDL and independent catalog/graph checks are the same as the Go
single-schema benchmark below. The JDBC JAR SHA-256 starts `17a75f09bd02`; the
runtime is OpenJDK 17.0.20 with bundled sqlite-jdbc 3.51.1.0. Python SQLite
3.53.4 creates the database file. Published engine 0.4.3 (`5f621fee1340`) checks
the outputs outside the collection timer.

| Tables | Format | Median collection s | Median peak RSS MiB | Maximum peak RSS MiB |
| --- | --- | ---: | ---: | ---: |
| 2,000 | JSON | 1.890 | 232.406 | 247.844 |
| 2,000 | NDJSON | 1.763 | 235.312 | 236.641 |
| 10,000 | JSON | 23.652 | 458.109 | 461.719 |
| 10,000 | NDJSON | 22.811 | 435.453 | 436.781 |

Both 10,000-table formats preserve exactly 40,001 vertices and 40,000 edges,
including every column and primary key. NDJSON does not make JDBC collection
constant-memory, and its memory result is not lower at every input size. JVM
startup, driver work, and serialization are included in collection time.

```sh
python3 Scripts/benchmark-probe-sqlite.py --jdbc-jar /path/to/fixed-probe.jar \
  --java /path/to/java --engine /path/to/fixed-schemagraph \
  --objects 2000 10000 --repeat 3 --timeout 600 --output jdbc-scale.json
```

SQLite keys without per-table driver metadata (2026-10-20). The rows above were
collected while the JDBC probe asked sqlite-jdbc for `getPrimaryKeys` and
`getImportedKeys` on every table. The bundled driver scans all of
`sqlite_schema` inside each call and once more for every foreign key, so
collection grew with the square of the table count. The probe now reads
`sqlite_schema` once per collection and builds the same rows from per-table
PRAGMAs. The previous JAR (`e35a12c27679`, built from `aaba709`) and the changed
JAR (`06f479f17b64`) ran alternately, two passes each, on one macOS 26.7.1 ARM64
host while other jobs were running (load average 5 to 7). The runtime is
OpenJDK 17.0.20 with bundled sqlite-jdbc 3.51.1.0, Python SQLite 3.53.4 creates
the database file, and engine 0.7.1 (`d970b76b18ed`) checks the outputs outside
the timer. Each row is the median of six fresh processes. Compare rows within
this table only; the 2026-09-22 rows above used an older JAR and engine.

| JAR | Tables | Format | Median collection s | Median peak RSS MiB | Maximum peak RSS MiB |
| --- | ---: | --- | ---: | ---: | ---: |
| Before | 2,000 | JSON | 1.282 | 206.7 | 213.5 |
| Before | 2,000 | NDJSON | 1.312 | 205.8 | 209.9 |
| Before | 10,000 | JSON | 7.577 | 430.6 | 441.8 |
| Before | 10,000 | NDJSON | 7.171 | 430.1 | 440.1 |
| Before | 20,000 | JSON | 28.421 | 519.3 | 523.8 |
| Before | 20,000 | NDJSON | 27.655 | 516.0 | 527.1 |
| After | 2,000 | JSON | 0.849 | 159.3 | 164.2 |
| After | 2,000 | NDJSON | 0.790 | 165.3 | 167.5 |
| After | 10,000 | JSON | 1.317 | 302.0 | 317.8 |
| After | 10,000 | NDJSON | 1.367 | 335.4 | 336.6 |
| After | 20,000 | JSON | 2.260 | 364.3 | 374.6 |
| After | 20,000 | NDJSON | 2.190 | 369.8 | 376.2 |

From 10,000 to 20,000 tables the median grows 3.8 to 3.9 times before and 1.6
to 1.7 times after. Both JARs wrote the same document bytes at every size and
format, and every document produced the same graph bytes (20,000 tables:
80,001 vertices and 80,000 edges).

The DDL above has no foreign keys. A second schema keeps the same two columns
and adds two references per table, so the previous JAR also scanned the schema
once per foreign key:

```sql
CREATE TABLE t000000 (id INTEGER PRIMARY KEY, value TEXT NOT NULL);
CREATE TABLE t000001 (id INTEGER PRIMARY KEY, value TEXT NOT NULL, prev_id INTEGER,
  root_id INTEGER REFERENCES t000000,
  CONSTRAINT fk_000001_prev FOREIGN KEY (prev_id) REFERENCES t000000(id));
-- Every later table references the table before it and t000000 the same way.
```

Each cell is the median of three alternating fresh processes after one warm-up
per JAR. Both JARs wrote the same document bytes at every size.

| Tables | Format | Before median s | After median s |
| ---: | --- | ---: | ---: |
| 2,500 | JSON | 3.020 | 1.568 |
| 2,500 | NDJSON | 2.942 | 1.700 |
| 5,000 | JSON | 6.941 | 2.618 |
| 5,000 | NDJSON | 5.813 | 2.471 |
| 10,000 | JSON | 18.038 | 4.490 |
| 10,000 | NDJSON | 16.968 | 4.420 |

```sh
python3 Scripts/benchmark-probe-sqlite.py --jdbc-jar /path/to/fixed-probe.jar \
  --java /path/to/java --engine /path/to/fixed-schemagraph \
  --objects 2000 10000 20000 --repeat 3 --timeout 600 --output jdbc-scale.json
```

### Cache: 200 large view definitions

This uses the unreleased parser/cache implementation in release-mode binary
`43e43458c0f6…`. An actual SQLite file contains 200 views over an eight-column
table. Each definition has a CTE, a join, computed projections, and a filter
with 10,000 literal values. The collected document is 10,177,775 bytes.
The verifier independently checks 1,810 vertices and 5,209 edges, including
every value-lineage edge; identical graph hashes alone are not the oracle.

| Mode | Median wall s | Median peak RSS MiB |
| --- | ---: | ---: |
| Cache disabled | 1.590 | 241.797 |
| Empty cache | 1.794 | 241.859 |
| Reused cache | 0.943 | 241.797 |

All nine results have the same complete graph bytes. Every warm run reports
200 hits, zero misses, zero writes, and zero warnings. This workload shows
less analysis time with reuse, with no meaningful peak-RSS reduction. Resolution
footprint validation can still parse an AST; the cache reuses analysis effects,
not the full in-memory catalog or graph. Separate regression checks verify
unrelated catalog edits, changed referenced columns, newly resolvable names,
routine overload changes, and corrupt-cache fallback.

```sh
python3 Scripts/benchmark-analysis.py --engine /path/to/fixed-schemagraph \
  --views 200 --columns 8 --filter-values 10000 --repeat 3 --output large-body-cache
```

Raw evidence is retained in the maintainer's
`verification/competitive-followups-20260922/jdbc-scale.json` and
`large-body-cache/results.json`. The latter report SHA-256 starts
`f155e076a610`; binary/input hashes and individual samples are in the reports.

## Large schemas, dense graphs, and cancellation (2026-09-22)

These are bounded observations on one macOS ARM64 host, not an optimization
claim or a latency SLA. The immutable engine reports 0.4.2 and was built from
`8e53c4f`; its SHA-256 starts `c28928ce3ade`. Each timing has three samples.
The JSON reports retain every sample, median, nearest-rank p95, maximum, input
hash, and executable hash. With three samples, p95 is the maximum, not a
well-estimated population tail. Process wall time includes startup and uses a
nominal 1 ms wait4 polling interval. Resident MCP request time starts after the client flush;
it excludes graph loading. Peak RSS is measured per child process.

### Engine input: one large synthetic catalog

The single schema has 10,000 tables with eight columns each, named PKs/indexes,
and a chain of FKs. Independent vertex/edge sets and formulas verify exactly
120,000 vertices and 139,997 edges. Repeated JSON and NDJSON scans produce the
same graph bytes. No SQL body analysis is involved in this workload.

| Catalog format | Median wall ms | Median peak RSS MiB | Maximum peak RSS MiB |
| --- | ---: | ---: | ---: |
| JSON | 525.8 | 254.69 | 270.95 |
| NDJSON | 452.3 | 171.77 | 171.78 |

### Dense graph queries

Each graph has the listed dependent-node count plus one root, one million
directed edges, and an explicit density of E / (V × (V − 1)). All dependent
nodes directly reach the root. The checks independently assert visited counts,
examined edges, IDs, distance, and truncation; an untimed request also validates
the complete 2,000-ID result. Timed requests cap returned rows at one while
traversing the complete graph. CLI numbers include a fresh process and graph
load; resident MCP numbers do not. They are different operating modes, not an
algorithm speedup comparison. MCP RSS is the peak of the resident process
across startup and requests, not a per-request allocation measurement.

| Dependent nodes | Directed density | CLI median ms | Resident MCP median ms | CLI median peak MiB | Resident MCP peak MiB |
| --- | ---: | ---: | ---: | ---: | ---: |
| 1,000 | 0.999001 | 880.5 | 65.5 | 450.23 | 450.70 |
| 2,000 | 0.249875 | 873.4 | 73.8 | 398.52 | 398.80 |

### Real single-schema Go collection

`Scripts/benchmark-probe-sqlite.py` creates an actual SQLite 3.53.4 file with
2,000 or 10,000 tables, each having `id INTEGER PRIMARY KEY` and
`value TEXT NOT NULL`. Input construction and graph validation are outside the
collection timing. Every collected table, column, type, ordinal, PK and absent
usage observation is checked against this DDL; the engine must preserve the
expected vertices/contains edges and emit identical JSON/NDJSON graph bytes.
The Go binary SHA-256 starts `6daea55f8e0b`; this source build includes
`--url-env`, which was unreleased when measured and shipped in 0.4.3. The
benchmark uses the unchanged literal SQLite URL path. It does not measure
JDBC collection or large routine bodies.

| Tables | Catalog format | Median collection ms | Median peak RSS MiB | Maximum peak RSS MiB |
| --- | --- | ---: | ---: | ---: |
| 2,000 | JSON | 56.1 | 35.16 | 35.28 |
| 2,000 | NDJSON | 54.2 | 27.80 | 28.00 |
| 10,000 | JSON | 260.8 | 80.48 | 87.97 |
| 10,000 | NDJSON | 252.5 | 44.83 | 45.95 |

The 10,000-table collected graph has 40,001 vertices and 40,000 edges. These
measurements confirm the format difference for this input; both formats still
retain a schema-sized collection and do not establish constant memory.

### Cancellation boundaries

The CLI test opens both FIFO ends, then sends SIGINT before any graph byte or
EOF. Both clocks start before the signal syscall. ACK latency ends when the
exact stderr line is observed; process-exit latency includes writing the graph
after the signal, completing the cancelled report, and cleanup. The report
must show one visited root, zero examined edges, and explicit cancellation.

The MCP test loads the graph, completes an idle preflight request, submits one
request and waits for at least 0.01 seconds of process CPU while draining
responses. A completed response or unavailable CPU evidence fails the proof.
All final samples observed this CPU progress before cancellation. It proves
active process work after submission, not traversal alone: request handling,
result construction and serialization may contribute. The cancellation clock
starts before notification write. A following missing-name `impact` call must
return the exact `isError=true`, `found=false` result through the compute worker.
This is an observable upper bound through notification delivery, worker
availability, the small probe, serialization and stdio. Reader-thread `ping`
is not worker evidence. The cancelled request must stay response-suppressed.

| Observed boundary | Median ms | Maximum ms |
| --- | ---: | ---: |
| CLI signal → ACK | 0.089 | 0.095 |
| CLI signal → process exit | 834.508 | 834.645 |
| MCP cancellation → worker response | 0.183 | 0.238 |

The final scale report SHA-256 starts `98549f3ee678` and its
script SHA-256 starts `eadb870500f9`. The SQLite collector
report starts `2eaf053fc8f2`. Full hashes, limits and
clock boundaries are in the reports. Earlier cancellation samples without CPU
proof and using reader-thread ping are retained with the
`failed-unconfirmed-active-request-proof` suffix and are not accepted as active
cancellation measurements. The final runs use 1 ms process-reap polling; earlier
20 ms polling reports are separately retained under `before-reap-resolution`.

### Reproduce

Build or copy binaries to immutable paths before a quiet timing window. The
following commands use the measured sizes; increasing repetitions improves
distribution evidence but does not create a hardware-independent threshold.

```sh
python3 Scripts/test-benchmark-scale.py
python3 Scripts/benchmark-scale.py --engine /path/to/immutable/schemagraph \
  --large-objects 10000 --columns 8 --dense-vertices 1000 2000 --repeat 3 \
  --timeout-seconds 120 --max-input-mib 512 --max-rss-mib 2048 \
  --output /path/to/scale-report
python3 Scripts/benchmark-probe-sqlite.py \
  --probe /path/to/immutable/schemagraph-probe-go \
  --engine /path/to/immutable/schemagraph --objects 2000 10000 --repeat 3 \
  --output /path/to/probe-sqlite-report.json
```

Use `--smoke` for generator/invariant checks without an engine. Generated DBs,
inputs, graphs and stderr are removed after success or failure; requested JSON
reports remain. Inputs, graph size, repeats and child/request duration are
bounded. Observed child RSS is checked against a limit, rather than pretending
to reserve or hard-limit total host memory.

## Graph construction and optional body cache (v0.4)

The graph shares vertex ID strings and indexes existing edge slots instead of
linearly searching a high-degree adjacency list on every insertion. On the same
macOS ARM64 host, a generated graph with 30,001 vertices and 30,000 outgoing edges
from one vertex produced these medians over three optimized runs:

| Work | Before (`a3fd72d`) | v0.4 implementation |
| --- | ---: | ---: |
| Build the vertices and edges | 833.435 ms | 35.632 ms |
| 100 adjacency passes with target lookup | 201.421 ms | 199.237 ms |
| Process peak RSS | 32.09 MiB | 27.70 MiB |

This isolates a wide-node construction bottleneck. It is not an end-to-end scan
benchmark or a comparison with another tool. The iterative SCC implementation
also passes 30,000-vertex chain and cycle tests without depending on call-stack
depth.

Body caching is **opt-in**. The following release-build measurements use 500
views, 12 output columns each, and a document collected from an actual temporary
SQLite database. Every view parses completely. Each run rebuilds the catalog
graph, then writes graph JSON. All modes produce byte-identical graphs; warm
runs report 500 hits and zero misses. Medians of three runs:

| SQL workload | No cache | Cold cache | Warm cache |
| --- | ---: | ---: | ---: |
| Short CTE/join views | 122 ms | 261 ms | 158 ms |
| Same views with 1,000-value `IN` filters | 297 ms | 469 ms | 194 ms |

Short SQL can cost less to parse than to read and validate a cache entry. The
longer SQL benefits from reuse; cache misses still add write cost. Peak RSS was
57–60 MiB for the short case and 62–63 MiB for the longer case. The cache does
not remove the in-memory catalog or graph, and it is not enabled automatically.
Atomic replacement and checksums allow corrupted entries to be reparsed without
forcing a disk flush for each disposable cache file.

Reproduce the current measurements:

```sh
cargo build --manifest-path engine/Cargo.toml --release --locked
rustc --edition=2021 -O Scripts/benchmark-core.rs \
  --extern schemagraph_core=engine/target/release/libschemagraph_core.rlib \
  -o engine/target/benchmark-core
engine/target/benchmark-core 30000

python3 Scripts/benchmark-analysis.py --engine engine/target/release/schemagraph \
  --views 500 --columns 12 --repeat 3 --output engine/target/analysis-short
python3 Scripts/benchmark-analysis.py --engine engine/target/release/schemagraph \
  --views 500 --columns 12 --filter-values 1000 --repeat 3 \
  --output engine/target/analysis-long
```

The analysis benchmark records the executable and graph SHA-256, per-run cache
counts, wall time, and peak RSS in `results.json`. Core construction can be
compared against the `a3fd72d` core sources using the same standalone harness
and Rust optimization settings. Machine load and filesystem behavior affect
these results; profile the workload before choosing a cache.

## Catalog memory (v0.2 to v0.3)

The v0.3 scan path avoids keeping a full input string, a second NDJSON value
tree, and multiple copies of the graph output. The Go probe emits NDJSON as
each schema is collected and uses a 64 KiB output buffer. Oracle and SQL
Server catalog queries bind the active schema so streaming does not repeatedly
transfer the entire catalog.

Measurements below compare v0.2.0 with the v0.3 implementation on the same
macOS ARM64 machine. Each number is the median of three runs; peak RSS is
measured separately for each child process with `wait4`. MiB means 2^20 bytes.

| Workload | v0.2.0 peak RSS | v0.3 peak RSS | Change |
| --- | ---: | ---: | ---: |
| Rust scan, 10,000 objects, JSON input | 633.5 MiB | 268.8 MiB | −58% |
| Rust scan, 10,000 objects, NDJSON input | 723.5 MiB | 170.1 MiB | −76% |
| Go SQLite probe, 10,000 tables, NDJSON | 58.3 MiB | 45.0 MiB | −23% |
| Go PostgreSQL probe, 4,000 tables across 20 schemas, NDJSON | 53.4 MiB | 23.8 MiB | −55% |

The Rust catalog fixture has eight columns per table, primary keys, foreign
keys, and indexes. Its JSON and NDJSON scans produce byte-identical graph
files across both implementations and all repeated runs. Median scan times
were 0.498→0.453 seconds for JSON and 0.512→0.322 seconds for NDJSON.

The PostgreSQL fixture runs on a real disposable PostgreSQL 16.13 instance.
It contains 32,000 columns and 7,999 constraints, including foreign keys
between schemas. All six baseline/current graphs have the same 44,020
vertices and 51,997 edges. Comparisons preserve every vertex/edge field and
the presence of usage fields; only observation timestamps and counters are
normalized. Probe times were similar: 2.644→2.626 seconds. The benchmark
interleaves baseline and current runs and forces `CGO_ENABLED=0` for the Go
build. The recorded toolchain was Go 1.27.1 on ARM64.

These are local measurements, not universal latency or memory guarantees.
The Go JSON path still assembles the full catalog and output: its 10,000-table
SQLite result remained about 85 MiB. Choose NDJSON for incremental emission.

## Reproduce

Both benchmark scripts use generated data and output under the ignored
`engine/target/` directory. They do not need production database access.

Install both historical CLI versions separately to reproduce the v0.3 comparison:

```sh
cargo install schemagraph-cli --version 0.3.0 --locked \
  --root engine/target/benchmark/v0.3 \
  --target-dir engine/target/benchmark/v0.3-build
cargo install schemagraph-cli --version 0.2.0 --locked \
  --root engine/target/benchmark/baseline \
  --target-dir engine/target/benchmark/baseline-build

python3 Scripts/benchmark-catalog.py \
  --engine engine/target/benchmark/baseline/bin/schemagraph \
  --label v0.2.0 --objects 1000 10000 --repeat 3 \
  --output engine/target/benchmark/before
python3 Scripts/benchmark-catalog.py \
  --engine engine/target/benchmark/v0.3/bin/schemagraph \
  --label v0.3 --objects 1000 10000 --repeat 3 \
  --output engine/target/benchmark/after
```

Add `--probe /path/to/schemagraph-probe-go` to measure the real SQLite probe
as well. The scripts record input/output sizes, graph hashes, peak RSS, and
wall time in `results.json`.

Build the baseline Go probe from the v0.2.0 source without changing the
working branch:

```sh
benchmark_root="$PWD/engine/target/benchmark"
mkdir -p "$benchmark_root/source-0.2.0"
git archive v0.2.0 probe-go | tar -x -C "$benchmark_root/source-0.2.0"
(cd "$benchmark_root/source-0.2.0/probe-go" && \
  CGO_ENABLED=0 go build -o "$benchmark_root/probe-0.2.0" .)
mkdir -p "$benchmark_root/source-0.3.0"
git archive v0.3.0 probe-go | tar -x -C "$benchmark_root/source-0.3.0"
(cd "$benchmark_root/source-0.3.0/probe-go" && \
  CGO_ENABLED=0 go build -o "$benchmark_root/probe-0.3.0" .)

python3 Scripts/benchmark-probe-postgres.py \
  --baseline-probe "$benchmark_root/probe-0.2.0" \
  --new-probe "$benchmark_root/probe-0.3.0" \
  --engine engine/target/benchmark/v0.3/bin/schemagraph \
  --pg-bin /opt/homebrew/opt/postgresql@16/bin \
  --schemas 20 --tables-per-schema 200 --columns 8 --repeat 3 \
  --output "$benchmark_root/postgres"
```

Set `--pg-bin` to the PostgreSQL binary directory on the host, for example
`/usr/lib/postgresql/16/bin` on the CI runner. This script starts a private
PostgreSQL cluster, applies generated fixtures, checks graph equality, and
removes its own cluster afterward.

## Remaining memory boundary

The engine still keeps the normalized catalog and dependency graph in memory
to resolve references across objects and schemas. JSON input still builds a
JSON value tree before normalization. NDJSON only needs one wire record at a
time during decoding, and graph JSON is written directly from borrowed graph
data, but this is not a constant-memory graph engine.

Both probes' NDJSON collection is bounded by the largest collected schema
plus discovery/diagnostic state. A single very large schema or routine body
can still require substantial memory. Use catalog v2 when a missing final
limitations record must be treated as an incomplete transfer; legacy v1
streams without that trailer remain supported.
