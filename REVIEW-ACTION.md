# Review policy and GitHub Action

Policy, baseline, expiring waiver, SARIF, and the consumer Action are available
from version 0.5.0. The 0.4.3 CLI does not include these options.

`review` compares two collected catalog documents and traces dependents in the
previous snapshot. It does not execute migrations or infer that an unreachable
object can be deleted. Keep both documents from the same reader, logical source,
database, and collection scope.

## Policy and reviewed findings

```toml
# review.toml
version = 1
fail_threshold = "high"

[severity]
column-removed = "high"
column-type-changed = "high"
index-added = "low"
```

```sh
schemagraph review before.json after.json \
  --policy review.toml --strict --require-complete
schemagraph review before.json after.json --format sarif > review.sarif
```

Severity values are `info`, `low`, `medium`, `high`, and `critical`. An explicit
policy replaces the legacy `--strict` change gate. Without `--strict`, policy
evaluation is report-only. Without a policy, the existing strict behavior is
unchanged. Output limits restrict displayed findings; hidden findings still
count toward the decision.

After reviewing the actual changes, save their fingerprints:

```sh
schemagraph review before.json after.json --write-baseline reviewed.json
schemagraph review before.json after.json \
  --policy review.toml --baseline reviewed.json --strict --require-complete
```

A baseline applies only to the same change, structure, and collection identity.
It is not an inventory snapshot. Usage-counter changes do not create new
fingerprints. Baseline generation rejects incomplete, incomparable, or truncated
reviews. Existing findings remain visible and carry `baselineState: existing`.

For a temporary exception, copy the finding's full SHA-256 fingerprint into the
policy and provide an explicit evaluation date:

```toml
[[waivers]]
fingerprint = "<64-character fingerprint from the review report>"
reason = "Replacement consumer deployment tracked in issue 123"
expires = "2026-10-31"
```

```sh
schemagraph review before.json after.json --policy review.toml \
  --as-of 2026-10-01 --strict --require-complete
```

Expired waivers cause a policy validation error (exit 2); remove or explicitly
renew them before evaluating the review. Unknown policy fields, invalid
dates, duplicate waiver fingerprints, and missing waiver reasons are errors.
Neither a baseline nor a waiver makes incomplete evidence complete.

## Consumer workflow

Use the v0.5.0 Action or pin a reviewed full commit containing `action.yml`;
v0.4.3 does not contain it.
For an immutable source pin, replace `v0.5.0` below with its verified full commit SHA.

```yaml
permissions:
  contents: read

jobs:
  schema-review:
    runs-on: ubuntu-24.04
    steps:
      - uses: actions/checkout@3d3c42e5aac5ba805825da76410c181273ba90b1
        with:
          persist-credentials: false
      # Produce before.json and after.json using your existing collection job.
      - id: schema
        uses: ictechgy/schemagraph@v0.5.0
        with:
          before: snapshots/before.json
          after: snapshots/after.json
          policy: review.toml
          baseline: reviewed.json
      - if: always() && steps.schema.outputs.json != ''
        uses: actions/upload-artifact@043fb46d1a93c77aae656e7c1c64a875d1fc6a0a
        with:
          name: schema-review
          path: |
            ${{ steps.schema.outputs.json }}
            ${{ steps.schema.outputs.markdown }}
            ${{ steps.schema.outputs.sarif }}
```

The default builds the pinned action source with Rust 1.96.0. A compatible local
binary can be supplied using `engine-path`. The runner needs Python 3.9+, Rust,
and native build prerequisites; the workflow above targets GitHub-hosted Ubuntu.
Inputs must be files inside the checked-out workspace. Optional `output-directory`
must be a new directory inside that workspace or `RUNNER_TEMP`. Each input is
limited to 128 MiB. `timeout-seconds` defaults to 180 per report rendering; source
builds have a separate 900-second timeout. A changed input during rendering is an
error. Reports above 64 MiB are rejected after rendering; this is not a process
memory or disk quota.

The action writes the Markdown report to the step summary and exposes `json`,
`markdown`, `sarif`, and `exit-code`. Decisions are 0 (no failure), 1 (findings),
2 (invalid/incomplete evaluation), and 130 (cancellation). It retains all three
reports for valid reviews even when the policy fails. It does not connect to a
database, comment on PRs, or upload data by itself.

SARIF 2.1.0 contains logical database locations and stable fingerprints. It has
no invented SQL file or line positions. GitHub PR line annotations require a
matching physical source location, so use the step summary and artifacts for
database-only findings. See GitHub's [SARIF support documentation](https://docs.github.com/en/code-security/reference/code-scanning/sarif-files/sarif-support).

## Reproduce the checks

```sh
python3 Scripts/test-review-action.py
python3 Scripts/verify-review-action.py --engine engine/target/debug/schemagraph
```

The second check invokes the actual CLI through the Action runner for policy
failure, accepted baseline, and incomplete collection. `--schema PATH` additionally
validates the emitted SARIF with the official OASIS 2.1.0 schema and Python's
`jsonschema` package.
