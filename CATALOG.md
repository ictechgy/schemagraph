# Catalog document protocol

The catalog document is the probe-to-engine wire format. It carries a
deterministic catalog snapshot and the limitations observed while collecting
it. The engine normalizes both wire versions into the same internal v1
`CatalogDocument`; graph format and graph version remain unchanged.

## Versions and compatibility

| Wire version | Producer field | Required feature declaration | Status |
| --- | --- | --- | --- |
| `1` | `reader: "..."` | None | Compatibility default and legacy format |
| `2` | `producer: {"name": "..."}` | Required, sorted array | Current negotiated format |

Only versions 1 and 2 are supported. Unknown versions, negative or fractional
versions, and integers too large to represent are rejected. A v2 document must
have a nonempty `producer.name`. A v2 document that
contains the removed `reader` field is rejected; it must use `producer.name`.
The producer name identifies the extraction path, such as `native-sqlx`,
`probe-jdbc`, or `probe-go`. It does not identify credentials or a database
connection.

The known required features are:

- `package-members-v1` — routine records use `member_of` for package members.
- `usage-v1` — objects, indexes, or routines carry `usage` evidence.

A v2 producer must declare every feature used by its records. An unknown
required feature is rejected; the engine never silently downgrades or ignores
semantics it cannot understand. A streaming producer may declare a supported
superset in its header because it must emit the header before seeing every
record. Producers emit sorted, duplicate-free declarations for deterministic output.
Readers treat the declaration as a set, so input order and duplicate names do not
change its meaning.

Unknown optional fields are accepted and reported in `limitations`, so an
unknown field is never silently presented as an absent field. Unknown required
features remain fatal. Repeated schema, object, routine, or member records are
accepted for v1 compatibility, but their duplicate identities are counted in
limitations because graph construction may merge them. Producers should emit
unique records.

Changing or removing a field is a non-additive protocol change. Such a change
requires a new major wire version, or a feature that the reader explicitly
understands. A v1 field must never be silently reinterpreted.

## JSON

Version 1 is the default for the Rust CLI and both probes:

```json
{
  "version": 1,
  "dialect": "sqlite",
  "reader": "probe-jdbc",
  "schemas": [],
  "limitations": []
}
```

Version 2 replaces `reader` and adds the sorted feature declaration:

```json
{
  "version": 2,
  "dialect": "postgres",
  "producer": {"name": "native-sqlx"},
  "required_features": ["usage-v1"],
  "schemas": [],
  "limitations": []
}
```

The remaining record fields are unchanged between versions. A schema has
`name`, `objects`, and `routines`. Object, column, constraint, referenced,
index, trigger, routine, and usage fields are the v1 internal document fields;
optional values such as `body`, `default`, `since`, `total_ms`, `self_ms`, and
`member_of` are omitted when absent.

To emit a document from a live scan, use the CLI option that writes the raw
catalog alongside the graph:

```sh
schemagraph scan postgres://db.example/app \
  --emit-document catalog-v2.json --document-version 2
```

`--document-version` is valid only with `--emit-document` and accepts `1` or
`2`. The capability query reports the same contract:

```sh
schemagraph document-capabilities
```

It returns the default version, supported versions, supported features, and
formats (`json` and `ndjson`).

The JVM probe and Go probe accept `--document-version 1|2`; both default to
1. Their ordinary JSON output is a complete document before serialization.

## NDJSON

NDJSON uses one JSON object per non-empty line. The first non-empty line must
be a `document` header. A `schema` line starts a schema, followed by its
`object` and `routine` lines. When an object or routine record includes a
`schema` field, it must match the immediately preceding schema line. Records
without a preceding schema are rejected.

Version 2 must end with exactly one `limitations` trailer, and no record may
follow it. The header's `limitations` is normally empty because a streaming
producer does not know its final limitations yet; the reader combines header
and trailer limitations. Version 1 legacy NDJSON may omit the trailer.

```text
{"type":"document","version":2,"dialect":"postgres","producer":{"name":"probe-jdbc"},"required_features":["usage-v1"],"limitations":[]}
{"type":"schema","name":"public"}
{"type":"object","schema":"public","data":{"name":"orders","kind":"table","columns":[],"constraints":[],"indexes":[],"triggers":[]}}
{"type":"limitations","data":[]}
```

Unknown record types, a second header, a missing v2 trailer, a record after
the trailer, malformed records, and schema mismatches are errors. Unknown
optional fields on recognized records are reported in `limitations`.

## Normalization and graph compatibility

JSON v1, JSON v2, NDJSON v1, and NDJSON v2 that describe the same catalog
normalize to the same internal v1 document. Consequently, building a graph
from v1 or v2 has no semantic delta, and a document diff reports no change
caused only by the wire-version metadata. The graph itself remains version 1.

Use the engine's document reader for compatibility checks rather than
rewriting fields in a producer. In particular, do not remove an unknown
required feature or downgrade a v2 document to v1 when its required semantics
are not understood.
