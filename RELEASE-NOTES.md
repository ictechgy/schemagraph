# Unreleased

- PostgreSQL body relations and routine names fold only unquoted identifiers. TRUNCATE and data-modifying CTEs retain write evidence; CTE aliases do not become physical relations.
- Column DEFAULT expressions retain uniquely resolved function dependencies as `depends-on` edges with body-parse evidence. Overloaded, missing and unparsable calls remain measured limitations. Literal defaults do not invent dependencies.
- Workspace crates declare `rust-version = "1.96"`, the toolchain CI builds, tests and lints. Older toolchains are not verified.
- CI checks Clippy on the established Rust 1.96.0 toolchain. Mechanical lint fixes and explicit boundary exceptions preserve public API and command behavior.
- Catalog documents whose dialect is `mariadb` (the JDBC probe's name for `jdbc:mariadb:` URLs) or `postgresql` are parsed with the MySQL and PostgreSQL SQL dialects, matching the name and case rules those documents already used. MySQL-only syntax such as `DIV`, or PostgreSQL's `#` operator, no longer makes such a view or routine unsupported.
- SQL Server bodies that name a relation with different letter case than the catalog (for example `dbo.orders` for `dbo.Orders`) still produce no edge, because documents do not carry collation. The unresolved-relation limitation and `SG_RELATION_UNRESOLVED` message now name the case-only variant, or count several, so the gap is explained instead of looking like a missing object.
- Engine and native-reader messages are English, as the CLI flags and documentation already were. This covers parser limitations and `SG_BODY_PARTIAL` diagnostic messages, native PostgreSQL/MySQL/SQLite collection limitations (now worded like the Go probe's), graph id-collision notes, graph.json load notes, inferred-edge evidence and CLI errors. Diagnostic codes, limitation counts and the conditions that produce them are unchanged; consumers that matched Korean message text must match the English text instead. JDBC probe messages are not changed here.
- PostgreSQL routine definitions that declare `SET search_path` (the form `pg_get_functiondef` returns for `ALTER FUNCTION ... SET search_path` or `CREATE FUNCTION ... SET search_path`) resolve unqualified relation names in that order instead of the routine's own schema. Unqualified routine calls gather overloads from every listed schema, and overloads in different schemas stay ambiguous. Uncollected or `$user` entries, and paths that list no collected schema, are reported only for the unqualified names they affect; fully schema-qualified bodies stay complete. Routines without the declaration and bodies stored without their definition header resolve as before; the catalog document format is unchanged.
- A document with an unrecognized dialect name is still parsed with the generic SQL dialect, and now records one limitation with the number of bodies and column defaults read that way. Dialect names the engine handles, including `oracle`, `db2`, `informix`, `h2` and `external`, add nothing.
- `rules --level object|schema` checks rules on the projected graph, so a view that reads several columns of a forbidden table is reported once per object pair and edge kind instead of once per column edge. The report then carries `level`, and a rule pattern that matches no vertex at that level is listed in `limitations`. Without `--level`, rules check every recorded edge and the output is unchanged.
- An overloaded call that cannot be narrowed to one overload still creates no `calls` edge, but the caller's analysis diagnostic now lists the overloads it may reach in an optional `candidates` array. `dead` keeps each candidate while a caller that is live for another reason may reach it, instead of reporting every overload as `noDependents`; a routine never keeps itself, or overloads that only call each other, alive this way. It adds the measured limitation `routines named by ambiguous overloaded calls: N`. `query`, `impact`, `cycles` and `rules` are unchanged. Existing diagnostic codes and messages are kept; graphs scanned before this change carry no candidates. `schemagraph_core::Diagnostic` gains the `candidates` field and `Diagnostic::new`; the body cache format moves to 5.

# schemagraph 0.8.1

- The packaged SQLite migration helper works when Python was built without the
  optional `enable_load_extension` API, including the stock Apple Python build.
  A fresh connection keeps extension loading off; SQL `load_extension` is always
  rejected by the authorizer. A present disable API still runs and any error
  propagates before SQL execution.
- ATTACH, DETACH, VACUUM INTO and extension-loading guards remain in place;
  ordinary PRAGMAs and schema DDL still work. Catalog, graph, facts and review
  bodies retain their contracts; producer version metadata advances to 0.8.1.
- Cargo workspace, JDBC probe and manual Maven publication defaults move to
  0.8.1. Publication and public installation are checked per channel; source
  versions do not imply registry availability.

# schemagraph 0.8.0

- `Scripts/capture-migration.py` replays SQLite migration SQL only in a new isolated
  database and captures comparable before/after catalogs, graphs, bridge facts and
  review with input/output hashes and preparation costs. See [migration capture](MIGRATION-CAPTURE.md).
- `facts` preserves additive bounded column type/nullability and constraint
  structural evidence for Isthmus contract comparison. Runtime/data compatibility
  and uncollected defaults/CHECK bodies remain explicitly unassessed.

## Breaking

- MCP tool results carry their JSON report once, as compact JSON with sorted
  keys in `content[0].text`. `structuredContent` is no longer sent, and the
  text is no longer pretty-printed. Clients and scripts that read
  `structuredContent` must parse the text content instead. Apart from the new
  MCP bounds listed under Changed, the report value is the same as before, and
  `resources/read` responses are unchanged.
- `query` and `impact` with `--format language-traversal` now exit 64 instead
  of 1 when a subject cannot be resolved. The document with the
  `root-not-found:` limitation is still written. This matches the other isthmus
  language-traversal producers and the partial-success rule of
  `isthmus capture`; scripts that accepted exit 1 must accept 64. Usage and
  engine errors still exit 2, and the default JSON `notFound` still exits 1.
- In the default `sslmode=prefer` / `ssl-mode=PREFERRED` mode, the native
  PostgreSQL and MySQL/MariaDB readers now start TLS whenever the server offers
  it, and a failed handshake ends the scan with exit code 2; the reader does
  not retry without TLS. Before, the readers never attempted TLS, so such a
  server was scanned in plaintext and the scan exited 0. A server that
  supports only TLS 1.1 or older, or only cipher suites that rustls does not
  implement (such as suites without forward secrecy), now needs an explicit
  `sslmode=disable` or `ssl-mode=DISABLED`.

## Security

- The native PostgreSQL and MySQL/MariaDB readers are now built with TLS
  (rustls with the ring provider, TLS 1.2 and 1.3) and trust the Mozilla root
  certificates built into the binary. Before, the default mode connected
  without encryption, and `require` and the verify modes failed with "SQLx was
  built without TLS support". Servers whose TLS handshake fails in the default
  mode are covered under Breaking.
- The default stays `sslmode=prefer` / `ssl-mode=PREFERRED`, as in libpq: the
  reader encrypts when the server offers TLS but does not verify the server
  certificate, so an active network attacker can still force plaintext or
  impersonate the server. To require a verified connection, add
  `sslmode=verify-full` or `ssl-mode=VERIFY_IDENTITY`; for a private CA, also
  add `sslrootcert` or `ssl-ca`, which is added to the built-in roots.
- `require`, `verify-ca`, and `verify-full` (`REQUIRED`, `VERIFY_CA`,
  `VERIFY_IDENTITY`) now work. When the server refuses TLS or the certificate
  check fails, the scan exits with code 2 instead of falling back to an
  unencrypted connection. With the current libraries (sqlx 0.8.6, rustls
  0.23), `verify-ca` and `VERIFY_CA` also require the certificate to name the
  host. `sslmode=allow` does not attempt TLS.
- A catalog read over an unencrypted connection to a host that is not a Unix
  socket or a loopback address (`127.0.0.0/8`, `::1`, `localhost`) records a
  `connection-not-encrypted:` limitation in the catalog document and the graph.
  `sslmode=disable`, `sslmode=allow`, and `ssl-mode=DISABLED` always record it
  for such a host. In the default mode the reader first connects to such a
  host as `sslmode=require` / `ssl-mode=REQUIRED` does and reconnects without
  TLS only when the server refuses TLS; the limitation then says that the
  server did not accept TLS. The reader decides from its own TLS negotiation,
  not from the server's report of the session, so a proxy that refuses TLS
  toward the reader cannot hide the limitation. The limitation describes the
  reader's own connection, not the onward connection from a pooler or proxy
  to the database. Loopback and Unix-socket scans, including the fixtures,
  produce the same output as before, and `catalog_complete` is unchanged.
- New dependencies, pinned by `engine/Cargo.lock`: rustls 0.23.43,
  rustls-webpki 0.103.14, rustls-pki-types 1.15.1, ring 0.17.14, untrusted
  0.9.0, and webpki-roots 0.26.11 and 1.0.10. The root certificate data in
  webpki-roots is licensed under CDLA-Permissive-2.0.
- `scan --cache-dir` entries are now authenticated with an HMAC-SHA256 tag.
  Before, an entry was checked only against an unkeyed SHA-256 checksum that
  anyone with the same release binary and catalog could recompute, so a forged
  entry in a checked-out tree could hide or invent body dependencies while the
  analysis still reported `complete`.
- The cache key is `SCHEMAGRAPH_CACHE_KEY` (at least 32 bytes) when it is set
  and not empty. Otherwise it is read from
  `${XDG_CONFIG_HOME:-$HOME/.config}/schemagraph/cache.key`, which is created
  with 32 random bytes and mode `0600` when missing. A key file that is a
  symbolic link, is owned by another user, has group or other permission bits,
  or holds fewer than 32 or more than 4096 bytes is rejected. A short key, an
  unset `HOME`, or a rejected key file disables the cache with a warning. The
  key is never printed.
- Entries with a missing or wrong tag, symbolic-link entries, and FIFOs or other
  special files are reported on stderr, reparsed, and overwritten. Entries are
  opened without following symbolic links and without blocking on FIFOs.
- The cache directory is a trusted input: keep it outside checked-out
  pull-request code, for example `${XDG_CACHE_HOME:-$HOME/.cache}/schemagraph`
  or `$RUNNER_TEMP/schemagraph-cache`. A missing directory is created with mode
  `0700`. A cache directory that is a symbolic link, is owned by another user,
  or is writable by group or other users disables the cache with a warning.
  Graph output and exit codes do not change when the cache is disabled.

## Changed

- A native PostgreSQL or MySQL/MariaDB scan of a host that is not a Unix
  socket or a loopback address now adds a `connection-not-encrypted:`
  limitation to the catalog document and the graph when the catalog was read
  over an unencrypted connection (details under Security). Loopback and
  Unix-socket scans, including the fixtures, keep their output.
- Cache format version 4 makes existing `--cache-dir` entries miss once; the
  next scan rewrites them. Caches restored from another machine or CI run are
  reused only when they were written with the same `SCHEMAGRAPH_CACHE_KEY`.
  Without that secret, each fresh runner creates its own key file and
  recomputes. Graph output is unchanged.
- An existing cache directory that is writable by group or other users, such as
  one created under umask 002 by an earlier version, now disables the cache with
  a warning until `chmod go-w` is applied to it.
- On non-Unix builds, which cannot check file ownership or open files without
  following symbolic links, `--cache-dir` is disabled with a warning.
- MCP `stats`, `cycles`, and `diagnostics` accept `max` (default 1024, at most
  10000) and keep the first `max` entries in CLI order. Only a cut list adds
  `truncated: true`, and `cycles` then also adds the full count as `total`;
  `stats` `totals` and `diagnostics` `summary` still count every entry. MCP
  not-found results list at most 64 `candidates` and add `candidatesTotal` and
  `candidatesTruncated: true` when cut. CLI output is unchanged.
- On every MCP tool, an integer argument outside its declared range (such as
  `max` above 10000, `depth` above 64, or a negative value) now returns an
  `isError: true` tool result whose text is `{"error": "…"}` with the allowed
  range, instead of JSON-RPC error -32602, so agents can correct the value and
  retry. Arguments of the wrong type, unknown arguments, and unknown tools are
  still JSON-RPC errors (-32602).
- JDBC probe: with pgjdbc, PostgreSQL columns are now split by exact schema
  and table name. `getColumns` treats both names as LIKE patterns, so a table
  also picked up the columns of other tables whose names matched its own:
  `a_b` also read `a1b`, `pct%name` also read `pctxname`, and a table in
  schema `my_app` also read the same-named table in `myxapp`. A backslash is
  the LIKE escape character, so `back\slash` read the columns of `backslash`,
  or none at all, in which case the catalog visibility check reported an
  uncollected column. Each table now keeps only its own columns. Snapshots
  collected again from such databases differ from older ones, and `review`
  against an older snapshot can report those columns as added or removed.
  Databases without such names produce the same documents as before.
- The Go probe (PostgreSQL) and the JDBC probe (PostgreSQL through pgjdbc)
  read columns, keys, and indexes, and the Go probe also triggers, once per
  schema instead of once per object; see `PERFORMANCE.md`. Successful
  collections produce the same output, apart from the JDBC column change
  above. A failed schema-wide catalog query now affects every object in the
  schema instead of one object. The Go probe leaves that part empty and
  records its usual limitation once. The JDBC probe records its usual
  per-object limitation for each object and, for indexes, still falls back to
  `getIndexInfo`.
- Go probe: MySQL and MariaDB constraints now list only their own key
  columns. The probe joined `table_constraints` with `key_column_usage`, and
  MariaDB compared the names in that join without regard to case or accents.
  A constraint with a shared name (`PRIMARY`, or `UNIQUE` keys with the same
  name) therefore also listed the key columns of tables whose names differed
  only in case or accents, such as `P` and `p` or `resume` and `résumé`, and of
  same-named tables in a schema whose name differed only in case. The order of
  those columns also varied between servers. Each constraint now keeps only its
  own table's key columns, as the native reader already did. MySQL documents
  are unchanged, and so are MariaDB documents without such names. Snapshots
  collected again from such databases differ from older ones, and `review`
  against an older snapshot can report `constraints-changed` for those tables.
- The Go probe (MySQL and MariaDB) reads primary-key positions, columns,
  constraints, indexes, triggers, and routine parameters once per schema
  instead of once per object or routine; see `PERFORMANCE.md`. Routine
  parameters are still read one routine at a time for names that the server's
  case- and accent-insensitive comparison could match with another name, and
  for every routine of a schema with a routine name that is not printable
  ASCII or that ends with a space. Constraints are still read one table at a
  time for a table in which two constraints share a name, such as a `UNIQUE`
  key and a foreign key. Successful collections produce the same output, apart
  from the constraint change above.
  A failed schema-wide catalog query now leaves that part empty for every
  object in the schema and records its usual limitation once. If the
  schema-wide routine parameter query fails, every routine in the schema
  records its usual limitation and has no signature.

# schemagraph 0.7.1

This patch protects local inputs and saved snapshots during file exports.
It also corrects SQLite primary-key nullability in the native, Go, and JDBC
collectors. Catalog and graph document formats are unchanged.

## SQLite primary-key nullability

- A primary key does not always make a SQLite column non-nullable. Ordinary
  rowid-table text/composite keys, `INT PRIMARY KEY`, and the column constraint
  `INTEGER PRIMARY KEY DESC` now retain nullable metadata unless explicitly
  constrained. Actual `INTEGER PRIMARY KEY` rowid aliases, `STRICT` and
  `WITHOUT ROWID` primary keys, and explicit `NOT NULL` remain non-nullable.
- Recollect SQLite snapshots to obtain the corrected metadata. Compared with
  older snapshots, `nullable` values can change; adding or removing `NOT NULL`
  is reported by `review` as `column-nullability-changed` (`review-required`).
  This changes collected metadata, not the database schema or stored data.
- The shared fixture checks 9 table shapes and 11 columns against actual SQLite
  storage, all three collectors, and bidirectional `NOT NULL` review results.

## Safer file output

- `scan`, `merge`, `facts`, and `openlineage` finish and sync a temporary file
  before atomically replacing the destination. A write or serialization failure
  preserves the previous snapshot. OpenLineage file output streams its events.
- Output preflight rejects overwriting catalog/graph inputs, the source SQLite
  database, and collected `--sql-dir` files. SQLite URL encoding and `file:` URIs,
  canonical path aliases, and Unix hard links are checked. Encoded `:memory:`
  SQLite URLs remain supported.
- Symlink, special-file, and read-only output destinations are rejected. Two
  output filenames in the same directory must differ by more than letter case.
  Existing file permissions are preserved; new files are private (`0600` on Unix).
- `scan --emit-document` stages both outputs before publishing either. The two
  renames are independent, not a multi-file transaction; stdout and shell
  redirection do not have the atomic-file guarantee.

## Container exports

For host-readable bind-mounted files, use `docker run --user "$(id -u):$(id -g)"`.
The image still defaults to non-root UID/GID `65532:65532`; private files owned by
that user must be read by the same user. The container smoke checks both paths.
See [installation instructions](INSTALLATION.md#docker-with-host-readable-output).

## Upgrade and validation

The [v0.7.1 GitHub binary release](https://github.com/ictechgy/schemagraph/releases/tag/v0.7.1)
was published and verified on 2026-10-13 UTC. The Linux x86_64/macOS arm64
archives, standalone JDBC JAR and `SHA256SUMS` passed the
[release workflow](https://github.com/ictechgy/schemagraph/actions/runs/38450161095)
and public-download checksum checks. Clean Linux CLI/Go and Java 21 JDBC
consumer checks passed. Use the verified binary pin in [INSTALLATION.md](INSTALLATION.md).

Other release coordinates remain targets: `schemagraph-cli` 0.7.1 (with
`--locked`), Action pin `ictechgy/schemagraph@v0.7.1`, and JDBC artifact
`io.github.ictechgy:schemagraph-probe:0.7.1`. This binary release does not verify
crates.io or Maven Central/GitHub Maven Packages publication, Action consumer
installation, or Pages deployment. All six Rust crate versions and the JDBC
manifest align at 0.7.1; third-party dependency versions are unchanged.

The original file-output regression run recorded 440 Rust tests, a regression
that fails with the old writer, real SQLite output-contract checks, and container
command tests. That count is historical evidence for the file-output change,
not the current full-suite total. The selected release source must pass source
and fixture checks; release artifacts and platform/container end-to-end checks
remain additional release CI gates. A version bump does not publish an artifact.

---

# schemagraph 0.7.0

This release makes `query` and `impact` output joinable by downstream tools:
versioned JSON with a `via` parent on every neighbor, a multi-subject isthmus
`language-traversal` format for cross-language `trace`, and a stable
`symbol.usr` on every `facts` relation. Existing JSON fields keep their
meaning; only new keys are added.

## Behavior changes

- `query`/`impact --format language-traversal` exits 1 when any subject cannot
  be resolved, **after** writing the document. Scripts that treat any nonzero
  exit as "no output" should read the document and check each root's `symbol`
  (or the `root-not-found:` limitation) instead.
- `query` and `impact` JSON gain top-level `format`/`version` and a `via` field
  on each neighbor. Consumers that reject unknown keys must accept them.
- The unbudgeted library functions `analysis::query` and `analysis::impact` no
  longer follow edges to unregistered vertices (see Upgrade notes).

## Traversal output for downstream joins

- `query` and `impact` JSON (CLI and MCP) now carry top-level `format`
  (`schemagraph-query`, `schemagraph-impact`) and `version: 1`. New keys,
  including ones inside nested objects, do not change the version; it increases
  only when an existing key changes meaning or type. Earlier reports have no
  `format` key. `notFound`
  responses are unchanged and carry no `format`.
- Each `query` and `impact` neighbor now carries `via`, the vertex before it on
  a shortest path from the subject. Direct neighbors have the subject as `via`;
  when several parents are equally close, the lexicographically smallest id is
  chosen, so output stays deterministic. `review` findings list impacted
  objects with the same field. Existing fields keep their meaning.

## language-traversal output

- `impact` and `query` accept `--format language-traversal`, which writes an
  isthmus `language-traversal` v1 document for cross-language `trace` joins.
  `impact` reports `dependents`; `query` requires `--direction dependencies` or
  `--direction dependents` and honors `--depth`. Both accept several subjects
  and traverse them in one multi-root pass under a single visited/edge budget.
- Each reached vertex carries `symbol` (`usr` and `qualifiedName`, both the
  vertex id that `facts` writes), `via`, `depth` from the nearest subject,
  `relationships` (edge kinds seen), and `roots`: every subject index that
  reaches it within the depth, capped at 64 per vertex with top-level
  `rootsTruncated`. Output is ordered by depth, then id.
- A subject reached from another subject is listed in `reached` too, with only
  the other subjects in its `roots` and `depth`/`via` measured from them, so
  passing every table of a database keeps each FK dependent. A subject reached
  only from itself is not listed.
- The header records `tool`, `generatedAt` (`--generated-at` pins it),
  `platform: "sql"`, `project` (`--project`, canonicalized like `facts`),
  optional `revision` (`--revision`), and `graphRevision`, the SHA-256 of the
  graph.json bytes read.
- An unresolved subject keeps its index without `symbol`, adds a
  `root-not-found:` limitation and truncation reason, and the command exits 1
  after writing the document. The missing `symbol` (or the limitation) is the
  per-root signal; `truncated` only says the document is not complete.
- `--max` caps the combined `reached` list across all subjects, keeping the
  nearest (depth, then id). The default JSON sorts by id before cutting, so with
  one subject the two formats agree only when nothing is cut.
- The new options do not affect the default JSON output of `query` and
  `impact`, which still takes one subject; they are rejected without
  `--format language-traversal`. MCP tools are unchanged.
- CI's bridge-facts check now runs every `facts` usr from real SQLite and
  PostgreSQL scans through multi-root `language-traversal` and compares each
  subject's reached set and depths with separate single-subject `impact` runs.

## isthmus facts

- `facts` now sets `symbol.usr` on every `relation-decl` to the same vertex id
  as `symbol.qualifiedName` (kept unchanged). isthmus treats `usr` as the
  producer's stable identifier, so a relation found through a persistence join
  can be passed to `schemagraph query` or `impact` without a platform-specific
  lookup. CI checks that every `usr` resolves as the `impact` subject in the
  graph from the same SQLite and PostgreSQL scans.

## Upgrade notes

- The CLI `query` and `impact` subject argument is now variadic (`<NAMES>...`).
  With the default `--format json`, passing more than one name is a usage error
  (exit 2), as before.

- `schemagraph_analysis::Neighbor` gains a public `via` field. Code that builds
  `Neighbor` with a struct literal must set it.
- The unbudgeted library functions `analysis::query` and `analysis::impact` no
  longer traverse edges whose endpoint is not a registered vertex, matching the
  budgeted traversal used by the CLI and MCP. Reading `graph.json` already
  drops such edges.

## Upgrade and validation

Install the CLI with `cargo install schemagraph-cli --version 0.7.0 --locked`,
or use the Linux x86_64/macOS arm64 release archives. The Action can be
referenced as `ictechgy/schemagraph@v0.7.0`.

All six Rust crates move to 0.7.0 because `Neighbor` gains a public field and
the CLI gains new options. The JDBC and Go probes have no code changes; the
probe version moves to 0.7.0 only to stay aligned with the engine
(`io.github.ictechgy:schemagraph-probe:0.7.0`). Catalog and graph document
formats are unchanged.

The `language-traversal` output is consumed by isthmus `trace`, which needs an
isthmus release that includes language-traversal input (isthmus-cli 0.9.0 does
not).

---

# schemagraph 0.6.0

This release adds agent-facing discovery tools, catalog lint rules, an
observation-based unused-object report, and OpenLineage export. It also fixes
PostgreSQL collection gaps found while validating them.

## Discovery and MCP

- `schemagraph search <pattern>` finds vertices by a case-insensitive name
  fragment or a `*`/`?` glob. The default `names` detail lists ids; `summary`
  adds kind, schema, name, and neighbor counts. `total` and `truncated` report
  what was cut.
- The MCP server adds `search`, `dead`, `cycles`, `lint`, `stats`, and `unused`
  tools, each returning the same JSON as the CLI, plus two read-only resources:
  a snapshot summary and the agent skill contract.
- `serve --config/--retain/--as-of` sets the `dead` retention policy at startup.
  `serve` does not read `schemagraph.toml` from its working directory, and tool
  arguments never name files. `review` is not exposed, since it needs two
  snapshots.

## Catalog lint

- `fk-type-mismatch` reports a foreign-key column whose declared type differs
  from the referenced column (PostgreSQL `serial` shorthands compare as their
  integer types; references that omit target columns use the referenced key).
- `table-without-primary-key` and `duplicate-index` are advisory: they are
  reported but never fail `lint --strict`, which now uses `blockingCount`.
  Primary-key absence is confirmed only for a complete catalog, and duplicate
  indexes stay unverified because index access methods are not collected.
- Graph metadata carries `schema_metadata.catalog_complete` when the collector
  declared a complete catalog.

## Unused tables and indexes

- `schemagraph unused` (CLI and MCP) reports tables and indexes with zero
  observed reads since `usage.since`, with facts that weaken a removal reading:
  uniqueness, constraint backing, covered foreign keys, SQL references, missing
  index counters, missing scan counts, and unknown windows. Objects without
  counters are counted as unobserved. It is an observation, not a deletion
  verdict.
- Table usage may carry `scans` (sequential plus index scans), so a table polled
  while empty is not reported. PostgreSQL partitioned parents carry no usage,
  and `track_counts = off` records no table or index usage with a limitation.

## OpenLineage export

- `schemagraph openlineage --namespace <uri>` writes one OpenLineage RunEvent
  (spec 2-0-2) per SQL body that produces a dataset, with schema and column
  lineage facets. Value lineage is `DIRECT` without a subtype; join and filter
  columns are dataset-level `INDIRECT` entries for views only. Jobs with partial
  analysis carry a `schemagraphAnalysis` facet. Fixing `--event-time` makes the
  output reproducible; `runId` is derived from the event content.

## Fixes

- PostgreSQL native and Go collectors now fill `pk_position` from the primary-key
  constraint; the engine also repairs documents that omitted it. Previously the
  FK primary-key-prefix lint could not confirm PostgreSQL coverage.

## Upgrade and validation

Install the CLI with `cargo install schemagraph-cli --version 0.6.0 --locked`,
or use the Linux x86_64/macOS arm64 release archives. Maven coordinates are
`io.github.ictechgy:schemagraph-probe:0.6.0`. The JDBC and Go probes collect
table scan counts and check `track_counts`. The Action can be referenced as
`ictechgy/schemagraph@v0.6.0`.

All six Rust crates move to 0.6.0 because of new public modules and fields.
Catalog v1/v2 and graph v2 gain optional fields (`usage.scans`,
`schema_metadata.catalog_complete`) that older readers ignore; existing fields
keep their meaning. `lint --strict` results do not change for existing rules.

New CI checks run the lint rules, `unused`, and OpenLineage export against real
PostgreSQL (native, Go, and JDBC collectors for the first two). OpenLineage
events are validated against pinned official schemas; ingestion by a specific
catalog has not been tested.

---

# schemagraph 0.5.1

This patch release fixes PostgreSQL materialized-view column collection and
adds `facts`, an export of catalog declarations for isthmus.

## PostgreSQL materialized views

- The native and Go PostgreSQL collectors now read materialized-view columns.
  In 0.5.0 these columns were empty, while the catalog visibility check still
  counted them. Owner-privileged scans of any database with a materialized view
  reported `catalog_complete: false` with a misleading permissions limitation,
  and reviews of those documents were `unverified`. The JDBC probe already
  collected these columns.
- Both collectors share one column query. Materialized-view columns follow the
  same privilege rule, `data_type` spelling, and nullability as
  `information_schema.columns` reports for tables. Materialized-view names
  remain visible through `pg_matviews` regardless of table privileges.
- Graphs of such databases now include materialized-view column vertices.
  Re-collect review baselines that were taken with 0.5.0.

## isthmus bridge-facts export

- `schemagraph facts --document <catalog>` writes an isthmus bridge-facts v1
  document (`platform: "sql"`, `target: "persistence"`). It declares tables,
  views, materialized views, and their columns as `relation-decl` facts. Each
  `symbol.qualifiedName` is the vertex id in the graph built from the same
  catalog. Incomplete catalogs carry a `catalog-coverage:` limitation so
  isthmus reports missing declarations as unverified.
- `generatedAt` fails with an explanation instead of recording a false time
  when the host clock is before 1970 or after 9999.
- Joining these documents needs an isthmus build with the persistence domain.
  isthmus 0.9.0 does not include it.

## Upgrade and validation

Install the CLI with `cargo install schemagraph-cli --version 0.5.1 --locked`,
or use the Linux x86_64/macOS arm64 release archives. Maven coordinates are
`io.github.ictechgy:schemagraph-probe:0.5.1`; the JDBC probe is unchanged apart
from its version. The Action can be referenced as `ictechgy/schemagraph@v0.5.1`.

All six Rust crates move to 0.5.1. `schemagraph-source` adds the public
`bridge_facts` module; existing public APIs, catalog v1/v2, and graph v2 are
unchanged.

`facts` output is checked in CI against declarations read directly from real
SQLite and PostgreSQL system catalogs, and against the graph from the same scan
in both directions. Consumption by isthmus was verified locally against its
development build; CI records that step as partial. Restricted-role PostgreSQL
collection with a materialized view is verified for the native, Go, and JDBC
collectors.

---

# schemagraph 0.5.0

This release adds policy-driven schema reviews, static DML column lineage,
external SQL imports, and dependency-aware analysis caching.

## Schema reviews in CI

- Review policies assign severity and a failure threshold. Fingerprints bind a
  finding to its structure and collection identity; reviewed baselines and
  dated waivers remain visible in reports.
- JSON, Markdown, and SARIF share the same review decision. SARIF uses logical
  database locations and does not invent SQL file positions.
- The consumer GitHub Action produces all three reports and a job summary, with
  bounded inputs, timeouts, immutable-input checks, and cancellation cleanup.
- Index predicates, UNIQUE index additions, and empty schema changes are now
  included in reviews. Incomplete collection and missing database identity
  cannot be suppressed into a successful comparison.

## Column dependencies and cache reuse

- INSERT SELECT, CTAS/SELECT INTO, UPDATE, and MERGE record column writes,
  expression/predicate reads, and value-lineage edges within the supported
  static subset. UNION SELECT INTO preserves both input value sources.
- Straight-line temporary relations are local symbols, never invented catalog
  vertices. Declared scalar variables and trigger transition rows are resolved
  separately from ordinary SQL columns.
- Self-updates retain read/write facts and report `SG_TEMPORAL_SELF_LINEAGE`
  with partial analysis instead of adding a false schema self-cycle.
- Unrelated catalog changes can reuse analysis with a verified resolution
  footprint. Uncertain paths use conservative whole-catalog invalidation;
  owner, write, endpoint, and provenance checks protect cached DML effects.

## External inputs and operations

- `schemagraph import` accepts dbt compiled model SQL and bounded query-log
  JSONL. Stable query identities, SQL/input hashes, collection context, and
  observation windows are preserved. Execution counts are not fabricated into
  database usage statistics.
- Imports validate file-descriptor, path, size, identity, and output ownership
  boundaries. They require a supported Unix runner and two new output files.
- PostgreSQL collection detects measured catalog omissions under restricted
  permissions. JDBC reads view/trigger definitions from native catalogs.
- A pinned, non-root Docker build, verified support matrix, and additional
  JDBC/large-body benchmarks are included. No hosted container registry is
  required to build the image from this tag.
- Maven Pages publication is explicit and builds the selected release tag;
  ordinary main merges do not publish an existing version again.

## Upgrade and validation

Install the CLI with `cargo install schemagraph-cli --version 0.5.0 --locked`,
or use the Linux x86_64/macOS arm64 release archives. Maven coordinates are
`io.github.ictechgy:schemagraph-probe:0.5.0`; the standalone probe requires
Java 17. The Action can be referenced as `ictechgy/schemagraph@v0.5.0` or by its
full source commit.

The six Rust crates move together to 0.5.0 because public parser/cache types
changed. Catalog v1/v2 and graph v2 remain supported. Local cache format changes
invalidate older entries automatically. New column coverage can expose partial
analysis in bodies previously assessed only at object level; inspect diagnostics
when adopting `review --require-complete`.

Independent DML expectations were fixed before analyzer evaluation. Validation
covers SQLite, PostgreSQL, SQL Server, and Oracle, with full uncached/cold/warm
graph equivalence; existing DB fixtures and public accuracy cases remain in CI.
The static subset, declared observation windows, and full in-memory catalog/graph
boundary still apply. These checks do not certify every SQL dialect or procedural
control path.

See [review/Action usage](REVIEW-ACTION.md), [external SQL imports](EXTERNAL-SQL.md),
[DML evidence](DML-ACCURACY.md), [support](SUPPORT-MATRIX.md), and
[performance measurements](PERFORMANCE.md).
