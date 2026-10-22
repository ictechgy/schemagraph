# Reproducible local migration capture

For SQLite, `Scripts/capture-migration.py` prepares a new isolated database from a
baseline schema and applies the actual SQL migration files in order. It captures
before/after catalog documents, graphs, bridge facts, and the existing independent
snapshot review. It never accepts a live DB URL or an existing output directory.

```sh
python3 Scripts/capture-migration.py --engine /path/to/schemagraph \
  --base-schema schema.sql --migration changes/001.sql --migration changes/002.sql \
  --project /path/to/application --output /path/to/new-capture
```

The output parent must exist. All SQL files are read once and hashed before
execution (16 MiB per file); an SQLite authorizer blocks ATTACH/DETACH, including
VACUUM INTO, and extensions remain disabled. A 30-second SQL budget and
120-second analyzer-command timeout bound preparation. Failures preserve partial
artifacts for diagnosis and never emit a successful capture receipt. The helper
creates one local DB and reuses its path to keep catalog source identity stable.
Review uses `--strict --require-complete`; code 1 remains recorded as a review or
coverage finding. Preparation, capture/migration and review costs are separate.

`capture.json` identifies exact SQL and output bytes. It is not a claim about a
production DB, data-dependent migration safety, locks, or other SQL dialects.
PostgreSQL/MySQL/JDBC changes still need the existing isolated fixture/probe
workflow; do not apply them to an operational database to produce these inputs.

## Application contract evidence

Bridge `facts` now adds optional `dataContract` v1 to relation and column
declarations. It carries explicit catalog column types/nullability and named
constraint structural hashes. Defaults, CHECK expressions and application/runtime
compatibility remain unassessed. The field/string budget is 512/1024 and omission
is explicit. This does not change declaration identity or dependency graph edges.
Old consumers may ignore this additive field; upgraded Isthmus `diff --contracts`
compares it and joins fixed application facts separately against both snapshots:

```sh
isthmus diff --contracts --before capture/before.facts.json \
  --after capture/after.facts.json --clients app.facts.json
```

Review callers and their USRs are candidates for investigation, not predicted
test failures. Use the graph from the corresponding capture for follow-up impact.
`Scripts/test-capture-migration.py --engine engine/target/debug/schemagraph` tests
real SQLite DDL and the compiled engine. The Windows-only helper tests cannot
replace the Linux CI compiled-engine/database checks.
