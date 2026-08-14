# microvms-agentd · Documentation

The prose in most of these documents was generated. The structure was produced
mechanically, so the cross-references are deterministic.

Two kinds of document live here. The five hand-written originals carry measured
platform findings and design rationale. They are authoritative and predate the
generated tree. Everything else was produced by a per-file documentation pass
over the codebase; every factual claim in those files carries a `path:line`
citation that was machine-verified against the source at generation time.

## Hand-written and authoritative

- [PLATFORM.md](PLATFORM.md) — every measured claim about AWS Lambda MicroVMs,
  dated and scoped. The trap findings recorded here motivated building the client.
- [PROTOCOL.md](PROTOCOL.md) — the daemon's wire protocol.
- [TRUST.md](TRUST.md) — the in-VM trust boundary.
- [STRATEGY.md](STRATEGY.md) — how the verification stack fits together.
- [HARNESS-CAPABILITIES.md](HARNESS-CAPABILITIES.md) — what agent harnesses
  (Harbor, Omnigent, the Vercel Sandbox / eve shape) require of a sandbox
  platform, mapped onto this one, with the gaps ranked.
- [CLI-COVERAGE-PLAN.md](CLI-COVERAGE-PLAN.md) — the plan that took live
  conformance from 38 checks to all of them. The plan is implemented; the file
  is kept for its reasoning.
- [schema.json](schema.json) — the generated, byte-compared wire schema.

## Architecture

- [System overview](architecture/system-overview.md) — what this is and how the
  seven crates fit.
- [Module map](architecture/module-map.md) — each crate's contents and largest files.
- [Data flow](architecture/data-flow.md) — `microvm run`, `exec --stream`, and
  the cost path, step by step.

## Reference

- [Public API](reference/public-api.md) — microvms-core and protocol exports,
  binding surfaces, and the daemon's 18 HTTP routes.
- [CLI](reference/cli.md) — all 16 `microvm` commands, the JSON envelope, the
  NDJSON stream exception, and the 14-row exit-code catalog.

## Behavior

- [Processes](behavior/processes.md) — the eight core flows, from image
  build to interrupt teardown.
- [State machines](behavior/state-machines.md) — the VM lifecycle, the exec
  phase, and the SSE cursor, with the Z3-proved invariants.

## Analysis

- [Risk hotspots](analysis/risk-hotspots.md) — where the live rounds found bugs
  and where coverage is thinnest.
- [Ownership](analysis/ownership.md) — knowledge concentration by artifact.
  Because agent sessions built this repo, the bus-factor question does not
  apply; the artifacts that cannot be reproduced are the measured ones.

## Diagrams

- [Components](diagrams/architecture/components.md)
- [Dependency graph](diagrams/structural/dependency-graph.md)
- [Sequences](diagrams/behavioral/sequences.md)

## Insights

- [Impact analysis](insights/impact-analysis.md) — the eight surfaces where a
  change fans out, including the three couplings that do not fail compilation.
- [Debugging guide](insights/debugging-guide.md) — failure-mode index, log
  surfaces, and the first-checks ladder, which follows the check order of
  `microvm doctor`.
- [Contract map](insights/contract-map.md) — nine inter-artifact contracts with
  producer, consumer, shape, and drift risk.
- [Business logic](insights/business-logic.md) — the 41 domain rules: trap
  closures, cost honesty, and lifecycle invariants, each with its strength.
- [Tech debt](insights/tech-debt.md) — the ranked register. This repo records
  acceptance reasons at the debt site, and the register cites them.
