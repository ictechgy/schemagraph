# Db2 LUW and Informix

The JDBC probe has specialized catalog readers for Db2 LUW and Informix.
They collect table/view metadata, keys, indexes, sequences, trigger text, and
routine definitions and signatures. SQL and SPL interpretation stays in the
Rust engine. Non-SQL routine bodies remain explicit analysis limitations.

The drivers are supplied separately with `--driver`; they are not bundled in
the probe's Maven artifacts.

| Database fixture | JDBC artifact |
| --- | --- |
| Db2 LUW 12.1.5.0 | `com.ibm.db2:jcc:12.1.5.0` |
| Informix 14.10.FC9W1DE | `com.ibm.informix:jdbc:15.0.1.4` |

This Db2 reader targets LUW catalogs. Db2 for i and z/OS are not covered by
the LUW validation suite. Informix engine-internal routines are excluded;
SQL-callable routines owned by `informix` remain visible, including public
server extensions. Usage statistics are not collected for these two readers.

## Collect a catalog

Set a JDBC URL such as `jdbc:db2://host:50000/database` or
`jdbc:informix-sqli://host:9088/database:INFORMIXSERVER=server;`. Supply the
password through `SG_DB_PASSWORD`, using the existing secure environment
mechanism on the host.

```sh
java -jar probe/build/libs/schemagraph-probe-all.jar \
  --url "$SG_JDBC_URL" --user "$SG_DB_USER" \
  --driver /path/to/vendor-jdbc.jar \
  --schema application_schema --format ndjson --document-version 2 \
  -o catalog.ndjson
schemagraph scan --document catalog.ndjson -o graph.json
```

Use the schema's catalog spelling, such as `SGFIX` for an unquoted Db2 schema.
The probe reads catalogs and requests a read-only connection. The fixture
runners below create and modify test objects and must use disposable targets.

## Reproduce integration checks

The dedicated [IBM CI workflow](.github/workflows/ibm-fixtures.yml) downloads
the public JDBC artifacts, verifies their SHA-256 hashes, and runs official
developer images pinned by digest. It tests actual routine/trigger execution,
required and forbidden graph edges, overloaded routines, long source
fragments, and all four v1/v2 JSON/NDJSON combinations.

```sh
python3 Scripts/verify-ibm-containers.py \
  engine/target/debug/schemagraph \
  probe/build/libs/schemagraph-probe-all.jar \
  --database all --memory 5g --drivers-dir /path/to/verified-jdbc-jars
```

The driver directory must contain `db2-jcc.jar` and `informix-jdbc.jar` at the
versions in the table. The runner validates their hashes, starts one database
at a time on a localhost port, and removes only its own containers and data
volumes afterward. The official images require their developer license
acceptance and privileged Docker operation. The script performs both for
these disposable test servers; no host data directory is mounted.

`Scripts/verify-ibm-fixtures.py --database db2|informix|all --require-all`
also accepts explicitly supplied disposable JDBC targets. `--require-all`
turns missing selected targets or drivers into failures. See `--help` for
the URL, user, driver, and retained-output options.

Catalog references: [Db2 routine definitions](https://www.ibm.com/docs/en/db2/12.1.x?topic=views-syscatroutines),
[Informix routine metadata](https://www.ibm.com/docs/en/informix-servers/14.10.0?topic=tables-sysprocedures),
[Informix source fragments](https://www.ibm.com/docs/en/informix-servers/15.0.x?topic=tables-sysprocbody).
