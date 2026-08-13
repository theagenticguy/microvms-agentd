# microvms-agentd

A verified client stack and in-VM daemon for AWS Lambda MicroVMs, written in
Rust. The workspace contains an exec-and-file-transfer daemon (`agentd`), a
client library (`microvms-core`) whose types make the platform's measured
failure modes impossible to express, a CLI (`microvm`), Python and Node
bindings, and a live conformance suite that verifies all of it against the real
service.

The service gives you an isolated Firecracker VM but no way to run anything in
it. There is no exec API and no file-transfer API. Every harness that wraps
Lambda MicroVMs therefore writes an in-VM daemon to supply both. Those
harnesses then discover, through billable failures, that the platform's error
responses often do not indicate the real cause. An unsupported region answers
`AccessDeniedException` with a null message. A reused `clientToken` wedges an
image in `CREATING` for fifteen hours with no error at all. A
`minimumMemoryInMiB` of 512 produces a guest that reports 2 GB. This project
ran those measurements once and recorded each finding in
[docs/PLATFORM.md](docs/PLATFORM.md) with its date and region. The client's
type system encodes those findings, so callers cannot repeat the mistakes.

**Status: every conformance check runs live and green.** The most recent
`mise run live` built an image, launched a real MicroVM in us-east-1, and drove
75 checks through the `microvm` CLI. The checks cover the full lifecycle, tar
round trips through the daemon's confined packer, rejection of hostile
archives, SSE ordering with mid-stream reconnect at a byte cursor, the stdin
lifecycle, start/poll/ack decomposition, and suspend/resume preserving the
token, filesystem, and a running process. All 75 checks passed, none failed,
and none were skipped. Teardown left the account clean, and the pinned rate
table matched the AWS Pricing API on all five rates.

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

Dependencies flow in one direction: `cli → core → protocol`, bindings → core,
and `agentd → protocol`. The CLI has no lib target and an allowlisted
dependency set. Tests assert both properties, so the CLI cannot reach AWS
except through the library.

## How strongly a mistake is closed

The spec ranks every guard ([docs/insights/business-logic.md](docs/insights/business-logic.md)
catalogs all 41):

- **S1 — inexpressible.** `Region` is a closed enum over the five regions that
  carry MicroVMs. `SizeClass` is the five documented baselines. Run-hook and
  build-hook timeouts are two types with no conversion (their ceilings differ
  60x). There is no conversion from a dollar figure to a bare float. No create
  path accepts a caller-supplied idempotency token.
- **S2 — rejected locally**, before any billable call, with an error naming the
  `docs/PLATFORM.md` finding that measured the behavior.
- **S3 — correct by default, overridable**, with the override's cost stated.

Every guard carries a falsification: a specific plausible regression that turns
a specific test red. Before that rule was enforced, four guards in this repo's
history passed against deliberately broken code.

## Cost honesty

The cost engine produces estimates and has no billing path. Durations carry
provenance (measured vs projected) as an enum variant, so an unlabeled duration
does not construct. An unbilled phase is represented as `Unpriced { reason }`
rather than as zero. A total containing any unpriced line renders as a lower
bound naming its phases. Rates are pinned with their retrieval date and are
ARM-only; the Architecture enum has one member because the x86 column the
Pricing API also returns overstates compute by 17.9%. `mise run live:rates`
compares the pinned table against the live Pricing API. A twin copy of that
check in `scripts/check-live-rates` serves as an independent oracle.

## Quick start

```bash
mise run install         # git hooks
mise run check           # the definition of done: lint, security, all test
                         # tiers, schema freshness, model drift, cross-compile
cargo build --release -p microvms-cli
target/release/microvm manifest        # the machine-readable command surface
target/release/microvm doctor          # is this machine ready to launch?
```

The live tier is deliberately separate because it creates real MicroVMs and
costs money:

```bash
mise run live            # conformance + rates + leak check, ~5 minutes
```

`microvm --json` emits exactly one envelope object per invocation on stdout.
Agents and scripts parse that envelope, and `data.kind` carries the
fine-grained error taxonomy. `exec --stream` is the documented exception; it
emits NDJSON events and then the envelope last. See
[docs/reference/cli.md](docs/reference/cli.md).

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
machine-verified `path:line` citations throughout. Start with the
[system overview](docs/architecture/system-overview.md) or the
[debugging guide](docs/insights/debugging-guide.md).

## License

Apache-2.0. The project is source-only; nothing here is published to
crates.io, PyPI, or npm.
