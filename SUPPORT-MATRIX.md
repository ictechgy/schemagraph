# Verified collection scope

Support is specific to a database version, reader, role, and object shape.
An available driver does not establish complete catalog visibility or complete
SQL analysis. `context.catalog_complete`, body analysis state, and query
truncation are separate facts.

## Current evidence

| Database / version | Reader paths | Verified scope | Role evidence |
| --- | --- | --- | --- |
| PostgreSQL 16.13 | Native, Go, JDBC (pgjdbc 42.7.8) | Tables, columns, PK/FK, view and SQL-function definitions; same-scope and wrong-scope review | Restricted role tested below |
| SQLite | Native, Go, JDBC (sqlite-jdbc 3.51.1.0) | File catalog, tables, keys, views, triggers; no database usage counters | File access; database roles do not apply |
| MySQL 8.4 / MariaDB 11.4 pinned images | Native, Go, JDBC | Public SQL accuracy corpus and fixture parity; statistics enabled/disabled cases | Fixture accounts; a minimal production role has not been certified |
| SQL Server 2022, 16.0.4295.3 | Go, JDBC | 18 independently authored view/routine/trigger cases, original and stored SQL; 8 producer/transport combinations | Fixture owner/admin; metadata visibility under restricted grants is not certified |
| Oracle Free 23.26.3 | Go, JDBC | 18 independently authored view/routine/trigger cases, original and stored SQL; 8 producer/transport combinations | Fixture schema owner; cross-owner visibility is not certified |
| H2 2.4.240 | JDBC | Bundled database fixture, catalog and transport parity | Fixture owner |
| Db2 LUW / Informix | External JDBC drivers | Dedicated real-container fixture jobs | Fixture accounts; version/driver setup is pinned by the job configuration |
| Other JDBC databases | Compatible external driver | Metadata baseline only | Not certified |

The released v0.4.3 evidence is the [full fixture job](https://github.com/ictechgy/schemagraph/actions/runs/35686249564),
[SQL Server/Oracle accuracy job](https://github.com/ictechgy/schemagraph/actions/runs/35686249608),
and [IBM fixture job](https://github.com/ictechgy/schemagraph/actions/runs/35686249593).
These runs apply to their tested commit, not automatically to later source edits.
Images and independent expectations are recorded under `Fixtures/accuracy`;
CI configuration records the other fixture environments. The new DML corpus and
its separate baseline are described in [DML-ACCURACY.md](DML-ACCURACY.md).

## Restricted PostgreSQL collection

Version 0.5.0 adds a real PostgreSQL 16.13 test with a login role declared
`NOSUPERUSER NOCREATEDB NOCREATEROLE NOINHERIT`. The known schema has two tables,
one view, one SQL function, six columns, and one foreign key.

| Profile | Native / Go | JDBC |
| --- | --- | --- |
| No object grants | Detects 3 uncollected relations and 6 uncollected columns; `catalog_complete: false`; strict review exits 2 | pgjdbc exposes the known catalog without table data privileges; definitions are read from `pg_catalog` |
| Schema `USAGE` plus `REFERENCES` on its tables/views | Collects all fixture facts; owner-vs-collector review has zero structural changes | Collects the same fixture facts |
| Requested schema absent | Incomplete collection and strict review exit 2 | Incomplete collection and strict review exit 2 |
| Different physical database, same logical source ID | Strict review exits 2 | Strict review exits 2 |

The test separately proves that the collector cannot `SELECT` table data or
create a table. `REFERENCES` is an actual database privilege; this verified
profile is not a claim that those grants are universally minimal. PostgreSQL
catalog visibility, ownership, custom catalog permissions, and managed-service
policies can differ. Use a role matching the tested collection scope and retain
the actual limitations in the collected document.

Native and Go collection compare returned relation/column names with an
independent `pg_catalog` inventory. They report measured omissions; they do not
invent a hidden-object count for other databases. JDBC uses the same check and
reads PostgreSQL view/trigger definitions directly from their native catalogs.
Optional statistics can remain unavailable even when catalog collection is
complete.

```sh
python3 Scripts/verify-collection-scope.py --engine engine/target/debug/schemagraph \
  --postgres-bin /path/to/postgresql/bin --output scope-native.json
python3 Scripts/verify-collection-scope.py --engine engine/target/debug/schemagraph \
  --go-probe probe-go/schemagraph-probe-go --output scope-go.json
python3 Scripts/verify-collection-scope.py --engine engine/target/debug/schemagraph \
  --jdbc-jar probe/build/libs/schemagraph-probe-all.jar --java /path/to/java \
  --output scope-jdbc.json
```

## Runtime and distribution

Released archives target Linux x86_64 and macOS arm64; the JVM probe requires
Java 17. Windows, Linux arm64 release archives, and additional warehouse
dialects have no new release support claim here.

Version 0.5.0 includes a [consumer Action](REVIEW-ACTION.md) and a Docker
build. The Dockerfile pins the official Rust and Debian image manifests and
runs the CLI as UID/GID 65532. It contains the Rust engine; JDBC and Go collection
remain separate producer choices. No container image is published by this
change.

```sh
docker build --tag schemagraph:local .
docker run --rm --network none --read-only \
  --mount type=bind,src="$PWD",dst=/work,readonly \
  schemagraph:local review before.json after.json --strict --require-complete
python3 Scripts/verify-container.py --image schemagraph:local --output container.json
```

The smoke test exercises file collection, query, policy failure, and SARIF with
network disabled, a read-only root filesystem, no Linux capabilities, and a
separate writable output mount. Local Linux arm64 execution has passed; the CI
job repeats the build and smoke test on Linux x86_64 for each tested commit.
