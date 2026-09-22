# Installing schemagraph

The GitHub release page contains prebuilt CLI and probe artifacts. The release
workflow publishes them only from a `vMAJOR.MINOR.PATCH` tag whose version
matches both `engine/Cargo.toml` and `probe/build.gradle.kts`. A manual run of
the same workflow builds and tests the artifacts without publishing anything.

## Choose an artifact

| Platform | CLI and Go probe archive |
| --- | --- |
| Linux x86_64 | `schemagraph-VERSION-x86_64-unknown-linux-gnu.tar.gz` |
| macOS arm64 (Apple Silicon) | `schemagraph-VERSION-aarch64-apple-darwin.tar.gz` |

The archive contains `schemagraph`, `schemagraph-probe-go`, this installation
guide, and the MIT and Apache 2.0 license files. The standalone JVM probe is
`schemagraph-probe-VERSION-all.jar`; it bundles the supported JDBC drivers and
requires Java 17. The Go probe needs no JVM and is built with CGO disabled.

Download the archive for the machine that will run the CLI, then verify its
checksum before extracting it:

```sh
curl --fail --location --remote-name \
  "https://github.com/ictechgy/schemagraph/releases/download/vVERSION/schemagraph-VERSION-x86_64-unknown-linux-gnu.tar.gz"
curl --fail --location --remote-name \
  https://github.com/ictechgy/schemagraph/releases/download/vVERSION/SHA256SUMS
shasum -a 256 --ignore-missing --check SHA256SUMS
tar -xzf schemagraph-VERSION-x86_64-unknown-linux-gnu.tar.gz
mkdir -p "$HOME/.local/bin"
install -m 0755 schemagraph-VERSION/schemagraph "$HOME/.local/bin/schemagraph"
install -m 0755 schemagraph-VERSION/schemagraph-probe-go "$HOME/.local/bin/schemagraph-probe-go"
```

Use `sha256sum --ignore-missing --check SHA256SUMS` on Linux if `shasum` is unavailable.
The check covers the downloaded assets; missing assets for other platforms are skipped. Put
`$HOME/.local/bin` on `PATH`, then confirm the installation:

```sh
schemagraph --version
schemagraph --help
schemagraph-probe-go --help
```

The release assets are portable command-line tools, so they do not connect to a
database during installation. A scan opens the database read-only where the
selected reader supports it; use a catalog document from a JDBC or Go probe
with `schemagraph scan --document` when the engine should remain separate from
the database connection.

## JDBC probe

The standalone JAR is useful for JDBC databases and can be run without a Gradle
checkout:

```sh
java -jar schemagraph-probe-VERSION-all.jar \
  --url jdbc:sqlite:/path/to/database.db \
  -o catalog.json
schemagraph scan --document catalog.json -o graph.json
```

PostgreSQL, H2, SQLite, and SQL Server drivers are bundled. For a database
whose driver is not bundled, pass a compatible driver with `--driver` and add
`--driver-class` when service discovery does not find it. Do not put passwords
in shell history; the probe accepts `SG_DB_PASSWORD` for the password.

The Go probe supports `--url-env NAME` from version 0.4.3 for reading a
complete URL from an exported environment variable without placing it in process
arguments. It is mutually exclusive with `--url`, and missing/empty variables
fail.

## Reviewing a catalog change in CI

The catalog document is a reviewable snapshot. A customer CI job can collect a
before and after document with a stable source label, then make the review a
required check:

```sh
set -eu

schemagraph scan "$DATABASE_URL" \
  --source-id app \
  --emit-document before.json \
  -o before.graph.json

# Recreate or point at the candidate schema before collecting the second snapshot.
schemagraph scan "$DATABASE_URL" \
  --source-id app \
  --emit-document after.json \
  -o after.graph.json

schemagraph review before.json after.json --strict
```

The command reports additions, removals, column and dependency changes, and
the dependents found in the earlier graph. `--strict` exits with status 1 when
reviewable changes are present, so a CI system can require an explicit review.
Use `--require-complete` when incomplete analysis should also fail the check.
This is a catalog and dependency review step; it does not execute migrations or
pretend to be a turnkey migration engine. Preserve `before.json` and
`after.json` as CI artifacts when reviewers need the evidence behind a result.

For a probe-backed scan, replace the live URL with the generated document:

```sh
schemagraph-probe-go --url "$DATABASE_URL" --source-id app -o after.json
schemagraph scan --document after.json -o after.graph.json
```
