# microvms-agentd · Documentation

Prose is generated; structure is mechanical. Cross-references are deterministic.

Two kinds of document live here. The five hand-written originals carry measured
platform findings and design rationale — they are authoritative and predate the
generated tree. Everything else was produced by a per-file documentation pass
over the codebase; every factual claim in those files carries a `path:line`
citation that was machine-verified against the source at generation time.

## Hand-written and authoritative

- [PLATFORM.md](PLATFORM.md) — every measured claim about AWS Lambda MicroVMs,
  dated and scoped. The trap findings here are why the client exists.
- [PROTOCOL.md](PROTOCOL.md) — the daemon's wire protocol.
- [TRUST.md](TRUST.md) — the in-VM trust boundary.
- [STRATEGY.md](STRATEGY.md) — how the verification stack fits together.
- [CLI-COVERAGE-PLAN.md](CLI-COVERAGE-PLAN.md) — the plan that took live
  conformance from 38 to all checks; kept for the reasoning (implemented).
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

- [Processes](behavior/processes.md) — the eight load-bearing flows, from image
  build to interrupt teardown.
- [State machines](behavior/state-machines.md) — the VM lifecycle, the exec
  phase, and the SSE cursor, with the Z3-proved invariants.

## Analysis

- [Risk hotspots](analysis/risk-hotspots.md) — where the live rounds found bugs
  and where coverage is thinnest.
- [Ownership](analysis/ownership.md) — knowledge concentration by artifact (the
  bus-factor question does not apply to a repo built by agent sessions; the
  irreplaceable artifacts are the measured ones).

## Diagrams

- [Components](diagrams/architecture/components.md)
- [Dependency graph](diagrams/structural/dependency-graph.md)
- [Sequences](diagrams/behavioral/sequences.md)

## Insights

- [Impact analysis](insights/impact-analysis.md) — the eight surfaces where a
  change fans out, including the three couplings that do not fail compilation.
- [Debugging guide](insights/debugging-guide.md) — failure-mode index, log
  surfaces, and the first-checks ladder (`microvm doctor`'s order is the spine).
- [Contract map](insights/contract-map.md) — nine inter-artifact contracts with
  producer, consumer, shape, and drift risk.
- [Business logic](insights/business-logic.md) — the 41 domain rules: trap
  closures, cost honesty, and lifecycle invariants, each with its strength.
- [Tech debt](insights/tech-debt.md) — the ranked register; this repo records
  acceptance reasons at the debt site, and the register cites them.
