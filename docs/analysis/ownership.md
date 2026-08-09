# microvms-agentd · Ownership

Per-author ownership analysis does not apply to this repo, so this file answers a different question.

The whole history is 31 commits over four days, 2026-08-05 through 2026-08-09, under two identities:
`bgagent` (27) and `Laith Al-Saadoon` (4). `bgagent` is the commit identity of the agent sessions that
wrote the code, not a person who holds knowledge in their head. A commit-share table would report
roughly 87% / 13% on every folder and tell a reader nothing about bus factor. There is also no
`CODEOWNERS` file to validate against. So the usual output — folders ranked by top-contributor share —
is omitted deliberately rather than fabricated.

What does carry risk here is **knowledge concentration by artifact**: a handful of files encode
behavior that was *measured against the live AWS service*, not derived from code. Delete or silently
edit one of them and no amount of reading the source recovers the value. Re-deriving it costs live AWS
runs, and in some cases a full build-and-run cycle per fact. That is the real single point of failure,
and it is a file property, not a person property.

## Knowledge concentration

Ranked by what it would cost to recover the file's contents if it were lost.

| Artifact | What it encodes | Recoverable from code? | Recovery cost |
| --- | --- | --- | --- |
| `docs/PLATFORM.md` | 22 sections of measured service behavior, 17 carrying an explicit measurement date | No | Live AWS runs, one build-and-run cycle per trap |
| `microvms-core/src/cost.rs` (rate table) | us-east-1 rates read 2026-08-07, plus the regional-spread reasoning | No | AWS Pricing API fetch + reconciliation |
| `scripts/check-model-drift` (pinned constants) | `PINNED_REGIONS`, `PINNED_SIZE_CLASSES` — the only second reader left | No | Re-measurement; no service model states either value |
| `spec/core.symspec.json` | 51 approved EARS requirements, 13 of them `TRAP-*` | Partly | Re-derivable only if PLATFORM.md survives |
| `docs/TRUST.md` | The trust boundary contract, implementable without this project | Partly | Re-argued from PLATFORM.md, not from source |
| `conformance/` | The live-run harness that produced the measurements | Yes | It is code; but it is how everything above was obtained |

Detail on the three that matter most:

- **`docs/PLATFORM.md`** states its own measurement context up front — `us-east-1`, API version
  `2025-09-09`, `al2023-minimal` aarch64, measured 2026-08-01 through 2026-08-04. Two entries show why
  the file cannot be regenerated: `runHookPayload` arrives wrapped in an envelope rather than as the
  request body, and finding that "cost a full build-and-run cycle" because the platform terminates the
  VM on the resulting 400 before you can look inside it. Separately, the `runHookPayload` ceiling is
  4096 bytes, not the 16 KB two other docs had asserted — an error that ran in the dangerous direction,
  telling a reader they could fit four times the secret material they actually can.

- **`scripts/check-model-drift`** holds `PINNED_REGIONS` and `PINNED_SIZE_CLASSES` as deliberate
  hand-maintained copies. The script's own header explains why: those two values used to be verified by
  comparing the Python client against the Rust client, the Python client was deleted after the Rust port
  went green, and no AWS service model states either value. The literals are that lost second reader,
  restored. Two of the five size-class rows are measured rather than documented, and the rows are *read*
  rather than computed as 4x the baseline because a formula there would be the exact bug the table
  exists to prevent.

- **`microvms-core/src/cost.rs`** carries a drift scar inline. `storage_gb_month` reads
  `dec!(0.0811111030)`; the comment records that it "Was 0.08 — a plausible-looking" figure, 1.37% low,
  from quoting storage per GB-hour while the table holds per GB-month. Region is not a cosmetic label on
  this table: eu-west-1 runs 5.3% over us-east-1 on compute and 19% on snapshot storage, so a Tokyo
  caller reading the us-east-1 rates understates their snapshot write bill by 22.6%. Only us-east-1 is
  pinned, and `scripts/check-live-rates` is the oracle that catches it going stale.

## Read first

A new maintainer should read in this order. The first three are non-negotiable; skipping PLATFORM.md
means re-learning its traps at the cost of live AWS runs.

1. `README.md` — why the project exists: the first daemon was 787 lines of Python inside Harbor
   PR #2469 and took 28 review findings across six rounds, nearly all in the daemon or in the service's
   lifecycle semantics.
2. `docs/PLATFORM.md` — every measured trap, dated and scoped to a region and API version. Read all
   22 sections before touching launch, cost, or hook code.
3. `docs/PROTOCOL.md` — wire protocol v1, and which paths the platform fixes versus which this
   project owns.
4. `docs/TRUST.md` — the boundary contract; explains what the daemon must refuse and why the workload
   is untrusted by design.
5. `spec/core.symspec.json` — the 51 requirements, especially the 13 `TRAP-*` entries, which are
   PLATFORM.md's findings in enforceable form with a `verificationMethod` each.
6. `microvms-core/src/` — the largest crate at ~20.6k lines and the only remaining client of the
   constants; `constants.rs` and `cost.rs` are where the measured values land in code.
7. `docs/STRATEGY.md` — scope and audience, plus the labeling discipline every claim follows
   (measured / documented / vendor-claimed / inferred).

## Single points of failure

No per-author bus-factor SPOFs can be computed — see the intro. The equivalent risks are artifact-shaped:

- `docs/PLATFORM.md` — the only record of the project's live-service measurements. Treat edits as
  measurement changes requiring a new dated run, never as prose cleanup.
- `scripts/check-model-drift` — `PINNED_REGIONS` and `PINNED_SIZE_CLASSES` lost their cross-client
  reader when the Python client was deleted; port any change to both sides in the same commit, as the
  file instructs.
- `microvms-core/src/cost.rs` — a hand-pinned us-east-1 rate table that has already drifted once;
  keep `scripts/check-live-rates` in CI so staleness surfaces as a failure rather than a wrong estimate.
- `spec/core.symspec.json` — 51 requirements in one file with no second copy; its `TRAP-*` entries are
  only as good as the PLATFORM.md sections they encode, so change the two together.
