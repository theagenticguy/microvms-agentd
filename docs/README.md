# microvms-agentd · Documentation

The prose in most of these documents was generated. The structure was produced
mechanically, so the cross-references are deterministic.

Two kinds of document live here. The hand-written originals carry measured
platform findings and design rationale. They are authoritative and predate the
generated tree. Everything else was produced by a per-file documentation pass
over the codebase; every factual claim in those files carries a `path:line`
citation that was machine-verified against the source at generation time.

Those citations are the generated tree's value and its expiry date. They anchor to
line numbers, so they rot whenever the cited source moves — and a citation that
lands on the wrong line still reads as authoritative. Regenerate the tree after any
substantial refactor rather than editing a stale file in place, and treat a
hand-written document as winning wherever the two disagree.

## Hand-written and authoritative

- [PLATFORM.md](PLATFORM.md) — every measured claim about AWS Lambda MicroVMs,
  dated and scoped. The trap findings recorded here motivated building the client.
- [PROTOCOL.md](PROTOCOL.md) — the daemon's wire protocol.
- [EMBEDDING.md](EMBEDDING.md) — embedding agentd in your own image with
  `microvm dockerfile` and driving it from your own harness: the wire contract
  a client implements, the proxy-token reality, and the `AGENTD_*` knobs.
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
  `cp --tar`, step by step.

## Reference

- [Public API](reference/public-api.md) — microvms-core and protocol exports,
  binding surfaces, and the daemon's 18 HTTP routes.
- [CLI](reference/cli.md) — all 17 `microvm` commands, the JSON envelope, the
  NDJSON stream exception, and the 14-row exit-code catalog.
- [RPC tools](reference/rpc-tools.md) — the daemon's 18 endpoints one by one,
  each with its handler signature, Bearer-or-open auth, and status codes.

## Behavior

- [Processes](behavior/processes.md) — the eight core flows, from image
  build to interrupt teardown.
- [State machines](behavior/state-machines.md) — the VM lifecycle, the exec
  phase, and the SSE cursor, with the Z3-proved invariants.

## Analysis

- [Risk hotspots](analysis/risk-hotspots.md) — where the live rounds found bugs
  and where coverage is thinnest.
- [Ownership](analysis/ownership.md) — knowledge concentration by artifact.
  One human author plus a bot means bus factor is 1 by construction, so the file
  measures churn and symbol density instead of people, and grades how well the
  specs, lessons, and gates externalize what one head holds.
- [Dead code](analysis/dead-code.md) — what is safe to delete, with the
  falsification test for each candidate. Short by design: clippy already covers
  crate-private dead code, so this file only carries what cross-crate and
  macro-boundary reasoning can find.

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
- [Contract map](insights/contract-map.md) — twelve inter-artifact contracts with
  producer, consumer, shape, and drift risk, plus the gate-coverage asymmetry
  between the two bindings.
- [Business logic](insights/business-logic.md) — the domain rules across all six
  requirement families, each labelled validation, invariant, calculation, or
  policy, and each cited to the test or Z3 proof that enforces it.
- [Tech debt](insights/tech-debt.md) — the ranked register. This repo records
  acceptance reasons at the debt site, and the register cites them.
