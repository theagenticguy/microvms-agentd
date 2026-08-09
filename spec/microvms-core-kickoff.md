# microvms-core: port the client stack to Rust

Copy this whole document into a fresh Claude Code session started in
`~/bonk-fs/projects/microvms-agentd`. Everything below is the prompt.

---

Build `microvms-core`: a Rust workspace that replaces the Python client stack for
AWS Lambda MicroVMs, in this monorepo, as new members of the existing Cargo
workspace (`Cargo.toml` at the root already declares `members = ["agentd","model"]`
with resolver 3, edition 2024). Please /erpaval this end to end with the same
verification stack the daemon used: symspec + Z3 for requirements, stateright for
model checking, proptest for properties, turmoil where transport faults matter,
and live AWS conformance at the end.

## Why this port exists

The Python client (`clients/python/`, deleted after this port went live-green; see
git history) was the discovery instrument: it found and
closed fifteen client-side API traps, measured the platform's pricing and
lifecycle semantics, and encoded cost-reporting honesty rules. All of that is now
settled and pinned — in `docs/PLATFORM.md` (every claim dated and measured), in
`scripts/check-model-drift` (33 constraints bound to the botocore service model),
and in `spec/core.symspec.json` (the formal spec for what you are building, with
a state model and reachability-proved lifecycle properties). Your job is a
translation of settled semantics into a stronger type system, not a
re-discovery. When the Python code and the docs disagree, the docs win; when the
docs and the service model disagree, measure.

## The architecture (decided; do not reopen)

Five crates, one dependency direction:

1. **`microvms-core`** — the product. Control-plane client over `aws-sdk-lambda`
   raw HTTP or a hand-rolled SigV4 client (the `lambda-microvms` service is not
   in aws-sdk-rust; the botocore model at
   `clients/python/.venv/.../botocore/data/lambda-microvms/2025-09-09/service-2.json.gz`
   is the wire contract — 24 rest-json operations, endpoint prefix `lambda`,
   signing name `lambda`). In-VM client for the agentd protocol. All fifteen
   trap closures. The cost engine and sizing model. This crate shares protocol
   types with the existing `agentd` crate (extract them into a `protocol` crate
   both depend on, so daemon/client drift becomes a compile error — that is the
   single strongest reason this port exists).
2. **`microvms-py`** — PyO3 bindings. Thin, idiomatic, and incapable of
   reopening a trap: if a binding can express the mistake, the closure belongs
   one level down in core.
3. **`microvms-js`** — napi-rs bindings, same contract.
4. **`microvms-cli`** — the `microvm` binary, ratatui for interactive surfaces,
   plain stdout when piped (agents and scripts read it; check `is_terminal`).
   Depends on core. Core never depends on it. Nothing a binding needs may live
   here — that decoupling is a spec requirement, not a style preference.
5. **`agentd` / `model`** — already exist; the daemon and its stateright model.
   Do not restructure them beyond extracting the shared protocol types.

## What "port the traps" means

`.erpaval/specs/001-control-plane-client/spec.md` defines three strengths:
S1 inexpressible (no parameter to misuse), S2 rejected locally (error before any
AWS call), S3 documented. Rust upgrades several S2s to S1s — a closed
`Region` enum instead of a runtime check, a `Duration`-with-provenance type whose
unlabeled construction is a compile error, `EstimatedUsd` without `Into<f64>`.
Every trap keeps or raises its strength; the spec's per-requirement keys (TRAP-*)
name each one. The guard-proof rule from the Python era carries over unchanged:
for every guard, break the invariant deliberately, watch the specific test fail,
restore it. A test that passes either way is not a test.

Traps with measured semantics to preserve (details in PLATFORM.md):
- `clientToken` is a PERMANENT idempotency key; scope tokens to one attempt,
  never truncate the nonce (the Python fix truncates only the label; 200 draws
  must stay 200 distinct).
- `runHookPayload` ceiling is 4096 bytes inclusive, measured 2026-08-07;
  botocore-class SDKs do not enforce `max` locally, so core must.
- Five regions only (us-east-1, us-east-2, us-west-2, eu-west-1,
  ap-northeast-1); an unsupported region answers AccessDeniedException with a
  null message, indistinguishable from an IAM denial. Refuse at construction
  with an escape hatch.
- Hook timeouts: run-time hooks cap at 60s, image-build hooks at 3600s. Two
  types, not one with a comment.
- `MicrovmImageBuildSummary` has `buildState`, not `state` — and the fake that
  hid this bug in Python is the cautionary tale: a test may not assert against
  a fake that shares the client's own assumptions. Generate fakes from the
  service model where possible.
- Baseline/peak sizing (guest reports peak, billing follows baseline), the
  suspended-pool economics (break-even hold ≈ 1357s at 2 GB), ARM-only rates
  17.9% below the x86 column the Pricing API also returns.

## Correctness stack, tier by tier

- **symspec v5**: the spec is already authored and verified at
  `spec/core.symspec.json` — 51 requirements (ARCH-5, TRAP-13, COST-10,
  STATE-12, BIND-5, CLI-6), a state model (`vm_state` enum over the six service
  states; `token_installed`, `image_exists`, `was_terminated` booleans;
  `bootstrap_count` finite int 0..3), an initial predicate, and 7 effects + 3
  constraints classified for the reachability tier. The Z3 Spacer tier proves
  three lifecycle invariants under the frame hypotheses (~0.9s): bootstrap
  fires at most once, suspend from a non-RUNNING state is unreachable, and a
  terminated VM never reaches RUNNING. All three guard-proofs were run before
  you: a planted contradiction was reported (FND_CONTRADICTION, exit 1), a
  planted TERMINATED→RUNNING effect produced FND_REACHABILITY_VIOLATED with a
  step-by-step trace naming the requirement keys, and a planted double
  bootstrap violated STATE-3 with its own trace. Each plant was deleted; the
  final check is exit 0 with 0 error findings.

  Drive implementation from the requirement keys. The v5 CLI is
  `node ~/workplace/symspec/packages/symspec/dist/cli.mjs` (1.0.0-alpha.0) —
  NOT the 0.1.0 `symspec` on PATH, which lacks the state model and
  reachability tier entirely. Wire
  `check spec/core.symspec.json --reachability-timeout-ms 5000` into the mise
  spec task. If you change the spec, re-run the planted-defect proofs (add a
  contradiction, watch it report, delete it). Two tool quirks the authoring
  session hit: GTWR lint errors silently exclude a requirement from the formal
  tier (so fix wording before trusting solver silence), and the units rule
  does not know "bytes" (waive with a reason rather than rewording the 4096).
- **stateright**: extend the model checking to the client's view of the
  lifecycle: prove resume-after-terminate is rejected without a wire call, prove
  the suspend/resume path preserves the session invariants PLATFORM.md measured.
- **proptest**: token generation (length ≤128, nonce collision-free), payload
  boundary at exactly 4096/4097 bytes, cost arithmetic (Decimal-equivalent via
  `rust_decimal`; the seconds-per-month constant is 730h).
- **turmoil**: the in-VM client's retry/reconnect against the daemon, reusing
  the daemon's existing turmoil listener newtype.
- **conformance**: the Python suite in `conformance/` stays as the oracle — it
  is proven against real AWS. Add a `conformance/run_rs.py` variant (or a flag)
  that drives the Rust CLI through the same 56 checks. Live runs are authorized
  via the EC2 instance profile; keep the Terraform stack applied; run
  `scripts/verify-clean` after every live session and treat its prefix list as
  a correctness condition if you add new resource names.
- **drift**: `scripts/check-model-drift` must learn to read the Rust constants
  too (or a small `--emit-json` on the CLI that exposes them for checking).
  `mise run check` is the definition of done; `mise run live` is the billable
  tier and needs no hook.

## Cost honesty rules (port exactly)

From `cost.py`, these are type-system obligations now: durations carry
provenance (measured vs projected) with no default; dollar figures render as
estimates and cannot coerce to bare floats; an unbilled phase is `Unpriced { reason }`,
never zero; totals containing any unpriced line render as lower bounds ("at
least..."); the rate table pins region + retrieval date, warns when stale, and
`mise run live:rates` compares it against the AWS Pricing API (ARM rows only —
the Architecture enum has exactly one member).

## Bindings contract

`microvms-py` replaces `clients/python` eventually, but do not delete the Python
package in this session: it is the conformance oracle and the API reference.
(Resolved: it was deleted in a later session, once both suites ran green against
real AWS on the same commit.)
The binding crates ship the same surface shape (Sandbox, Session, ExecHandle,
cost report types) and inherit every closure from core. Build them with maturin
/ napi-cli but do NOT publish anything — this repo is source-only, no push to
any remote, no crates.io, no PyPI, no npm.

## CLI

`microvm` in Rust: same subcommands as the Python CLI (`run`, `build`, `exec`,
`suspend`, `resume`, `terminate`, `ls`, `logs`, `cost`, `doctor`, `manifest`),
same stable exit-code catalog (the Python `cli.py` documents 14), `--json`
envelopes for agents, ratatui only when stdout is a TTY (live run progress, cost
tables, `ls`). The manifest command is the machine-readable command surface —
port its shape from the Python one.

## Ground rules

- Source-only: no push, no publish, no new remotes.
- Live AWS: authorized via the instance profile, us-east-1, account
  392583147479. Terraform stack stays applied. Nothing billable in `mise run
  check`.
- The teardown ordering trap: delete log groups LAST (the service recreates a
  group deleted before its image — this leaked twice).
- Every doc claim you add to PLATFORM.md carries date, region, API version.
- Commit checkpoints with the story in the message, hooks green, no --no-verify.

Start by reading `spec/core.symspec.json`, `docs/PLATFORM.md`, and
`.erpaval/specs/001-control-plane-client/spec.md`, then plan the waves.
