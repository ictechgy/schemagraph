# Performance measurements

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
