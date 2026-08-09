# microvms-agentd

A verified client stack and in-VM daemon for AWS Lambda MicroVMs, in Rust: an
exec-and-file-transfer daemon (`agentd`), a client library whose types make the
platform's measured traps unwritable (`microvms-core`), a CLI (`microvm`),
Python and Node bindings, and a live conformance suite that proves all of it
against the real service.

The service gives you an isolated Firecracker VM and no way to run anything in
it: there is no exec API and no file-transfer API. Every harness that wraps
Lambda MicroVMs therefore writes an in-VM daemon to supply both — and then
rediscovers, one billable failure at a time, that the platform's answers point
away from their causes. An unsupported region answers `AccessDeniedException`
with a null message. A reused `clientToken` wedges an image in `CREATING` for
fifteen hours with no error at all. A `minimumMemoryInMiB` of 512 produces a
guest that reports 2 GB. This project spent those measurements once, recorded
each in [docs/PLATFORM.md](docs/PLATFORM.md) with its date and region, and then
built a client where the type system carries the findings.

**Status: every conformance check runs live and green.** The most recent
`mise run live` built an image, launched a real MicroVM in us-east-1, and drove
75 checks through the `microvm` CLI — full lifecycle, tar round trips through
the daemon's confined packer, hostile archives refused, SSE ordering with
mid-stream reconnect at a byte cursor, stdin lifecycle, start/poll/ack
decomposition, suspend/resume preserving the token, filesystem, and a running
process — 75 passed, 0 failed, 0 skipped, teardown left the account clean, and
the pinned rate table matched the AWS Pricing API on all five rates.

## The shape of the workspace

```text
protocol/        the daemon↔client wire types; drift is a compile error
agentd/          the in-VM daemon: exec, file transfer, one-shot bootstrap
model/           stateright models of the daemon and the client lifecycle
microvms-core/   the client: control plane, session, cost engine, sandbox
microvms-cli/    the microvm binary — 16 commands, JSON envelopes, a manifest
microvms-py/     PyO3 binding (thin; every trap closure inherited from core)
microvms-js/     napi-rs binding (same contract)
conformance/     run_rs.py — the live suite; every check expressible via the CLI
spec/            51 formal requirements; three lifecycle invariants proved in Z3
```

One dependency direction: `cli → core → protocol`, bindings → core, `agentd →
protocol`. The CLI has no lib target and an allowlisted dependency set, both
asserted by tests — it cannot reach AWS except through the library.

## How strongly a mistake is closed

The spec ranks every guard ([docs/insights/business-logic.md](docs/insights/business-logic.md)
catalogs all 41):

- **S1 — inexpressible.** `Region` is a closed enum over the five regions that
  carry MicroVMs. `SizeClass` is the five documented baselines. Run-hook and
  build-hook timeouts are two types with no conversion (their ceilings differ
  60x). A dollar figure has no road to a bare float. No create path accepts a
  caller-supplied idempotency token.
- **S2 — rejected locally**, before any billable call, with an error naming the
  `docs/PLATFORM.md` finding that measured the behavior.
- **S3 — correct by default, overridable**, with the override's cost stated.

Every guard carries a falsification: a specific plausible regression that turns
a specific test red. Four guards in this repo's history passed against
deliberately broken code until that rule was enforced.

## Cost honesty

The cost engine renders estimates, never bills. Durations carry provenance
(measured vs projected) as an enum variant, so an unlabeled duration does not
construct. An unbilled phase is `Unpriced { reason }`, never zero. A total
containing any unpriced line renders as a lower bound naming its phases. Rates
are pinned with their retrieval date, ARM-only (the Architecture enum has one
member; the x86 column the Pricing API also returns overstates compute by
17.9%), and `mise run live:rates` compares the pinned table against the live
Pricing API — with a twin copy in `scripts/check-live-rates` as an independent
oracle.

## Quick start

```bash
mise run install         # git hooks
mise run check           # the definition of done: lint, security, all test
                         # tiers, schema freshness, model drift, cross-compile
cargo build --release -p microvms-cli
target/release/microvm manifest        # the machine-readable command surface
target/release/microvm doctor          # is this machine ready to launch?
```

The live tier is deliberately separate — it creates real MicroVMs and costs
money:

```bash
mise run live            # conformance + rates + leak check, ~5 minutes
```

`microvm --json` emits exactly one envelope object per invocation on stdout
(agents and scripts parse it; `data.kind` carries the fine-grained error
taxonomy). `exec --stream` is the documented exception: NDJSON events, envelope
last. See [docs/reference/cli.md](docs/reference/cli.md).

## Verification stack

| Tier | What it proves | Where |
| --- | --- | --- |
| symspec + Z3 | 51 requirements consistent; bootstrap-once, no suspend outside RUNNING, terminated never RUNNING — proved over unbounded runs | `spec/core.symspec.json`, `mise run spec:core` |
| stateright | The daemon model and the client lifecycle model, including a real interleaving bug the checker found | `model/` |
| proptest | Token collision-freedom, payload boundaries, decimal arithmetic | in-crate |
| turmoil | Reconnect-at-cursor and proxy-token expiry under simulated network faults | `microvms-core/tests/` |
| drift gate | 33 hardcoded service constraints against the pinned botocore model | `scripts/check-model-drift`, in `mise run check` |
| live conformance | All 75 checks against real AWS through the CLI | `conformance/run_rs.py`, `mise run live` |

Security gates run in `mise run check` and CI: semgrep, betterleaks over full
git history, SPDX license headers, plus SBOM generation (CycloneDX + SPDX) with
grype/trivy/osv-scanner in their own CI job.

## Documentation

[docs/README.md](docs/README.md) is the index. The hand-written, authoritative
documents are [PLATFORM.md](docs/PLATFORM.md) (the measured findings),
[PROTOCOL.md](docs/PROTOCOL.md), and [TRUST.md](docs/TRUST.md). The generated
tree (architecture, reference, behavior, analysis, diagrams, insights) carries
machine-verified `path:line` citations throughout — start with the
[system overview](docs/architecture/system-overview.md) or the
[debugging guide](docs/insights/debugging-guide.md).

## License

Apache-2.0. Source-only: nothing here is published to crates.io, PyPI, or npm.
