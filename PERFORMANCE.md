# Catalog memory measurements

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

Build the current CLI and install the baseline separately:

```sh
cargo build --manifest-path engine/Cargo.toml --release --locked
cargo install schemagraph-cli --version 0.2.0 --locked \
  --root engine/target/benchmark/baseline \
  --target-dir engine/target/benchmark/baseline-build

python3 Scripts/benchmark-catalog.py \
  --engine engine/target/benchmark/baseline/bin/schemagraph \
  --label v0.2.0 --objects 1000 10000 --repeat 3 \
  --output engine/target/benchmark/before
python3 Scripts/benchmark-catalog.py \
  --engine engine/target/release/schemagraph \
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

python3 Scripts/benchmark-probe-postgres.py \
  --baseline-probe "$benchmark_root/probe-0.2.0" \
  --new-probe "$benchmark_root/probe-current" \
  --engine engine/target/release/schemagraph \
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
