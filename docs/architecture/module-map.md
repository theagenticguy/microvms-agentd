# microvms-agentd · Module map

The workspace declares seven members (`Cargo.toml:2-9`), and the sections below run in the
dependency order of the `system-overview.md` flowchart, bottom-up: the wire contract first,
then the two crates that compile against it, then the three surfaces over the client, then the
checked model (`docs/architecture/system-overview.md:79-97`). Each crate's file list is its
`src/` tree ranked by size and by how many other files reference it; a crate's `tests/` tier is
excluded so the source files a reader is looking for are not crowded out, since
`agentd/tests/turmoil_transport.rs` alone is 2,163 lines. Files belonging to no crate are
collected under `Supporting code` at the end.

## protocol

`protocol` is the wire contract expressed as Rust types, split into `exec`, `fs`, `health`, and
`hook` (`protocol/src/lib.rs:29-32`). The daemon and every Rust client of it compile against the
same definitions, so a renamed field breaks compilation on whichever side has not caught up
rather than surfacing as a consumer's runtime bug (`protocol/src/lib.rs:10-12`). Membership is
decided by one rule — pure data that travels on the wire is admitted and machinery for making it
travel is not, which is why the SSE event payloads live here and the stream that emits them does
not (`protocol/src/lib.rs:16-21`). Every type derives both halves of serde even where one side
needs only one, because the missing half is what a client would otherwise hand-write, and
`docs/schema.json` is generated from those same attributes under both contracts and byte-compared
in CI (`protocol/src/lib.rs:23-27`).

- `protocol/src/exec.rs` (415 LOC)
- `protocol/src/lib.rs` (66 LOC)
- `protocol/src/hook.rs` (263 LOC)
- `protocol/src/health.rs` (152 LOC)
- `protocol/src/fs.rs` (92 LOC)

## agentd

`agentd` is the in-VM daemon supplying the exec and file-transfer APIs AWS Lambda MicroVMs does
not have (`agentd/src/lib.rs:4-7`). Its ten modules divide by defect class rather than by HTTP
surface: `state` owns the one-shot bootstrap, `auth` decides authorization before a body byte is
read, `exec` owns idempotent start with ack-gated release, and `fs` owns streaming tar
(`agentd/src/lib.rs:31-40`). The trust boundary is the crate's organizing fact — the platform's
own `/run` hook arrives from `127.0.0.1`, indistinguishable at the socket level from a request
sent by a process inside the VM, so source-address filtering would reject a legitimate bootstrap
and the one-shot property is the only defense left (`agentd/src/lib.rs:11-16`). `routes.rs`
assembles the router by walking `surface_docs`, the same eighteen-endpoint list `/v1/schema`
publishes, so a documented route with no handler panics at startup, and each endpoint's declared
auth mode decides which of the two routers it joins (`agentd/src/routes.rs:31-35`,
`agentd/src/routes.rs:48-58`, `agentd/src/routes.rs:371`, `docs/schema.json:497-1154`).

- `agentd/src/exec.rs` (3296 LOC)
- `agentd/src/fs.rs` (2628 LOC)
- `agentd/src/schema.rs` (890 LOC)
- `agentd/src/routes.rs` (807 LOC)
- `agentd/src/identity.rs` (737 LOC)
- `agentd/src/state.rs` (427 LOC)
- `agentd/src/disk.rs` (435 LOC)
- `agentd/src/config.rs` (196 LOC)

## microvms-core

`microvms-core` is the client library and the workspace's largest crate, holding the control
plane, the in-VM session client, the cost engine, and every trap closure
(`microvms-core/src/lib.rs:2-3`). Its own doc comment splits it in two: `error`, `region`,
`sizing`, `hooks`, and `constants` are the foundation, while `cost`, `control`, `session`, and
`sandbox` are the product surface (`microvms-core/src/lib.rs:59-63`). Each of the seventeen
measured platform findings is spent once here so no caller has to measure it again, and every
closure is ranked on a strength ladder where S1 means the mistake cannot be written down at all
(`microvms-core/src/lib.rs:7-14`, `microvms-core/src/lib.rs:23-40`). `cost.rs` is the largest
file in the repository and carries the rule that makes the rest of it legible — unknown is not
zero, so `Amount::Unpriced` is a distinct variant a consumer has to match on rather than a $0.00
line (`microvms-core/src/cost.rs:22-27`) — and the crate re-exports `protocol` so consumers name
wire types through here instead of depending on the contract crate
(`microvms-core/src/lib.rs:75-77`).

- `microvms-core/src/cost.rs` (4127 LOC)
- `microvms-core/src/control/image.rs` (3462 LOC)
- `microvms-core/src/session/exec.rs` (1711 LOC)
- `microvms-core/src/control/microvm.rs` (2009 LOC)
- `microvms-core/src/sandbox.rs` (2371 LOC)
- `microvms-core/src/control/ops.rs` (2272 LOC)
- `microvms-core/src/control/mod.rs` (1525 LOC)
- `microvms-core/src/session/mod.rs` (1200 LOC)

## microvms-cli

`microvms-cli` builds the `microvm` binary: seventeen subcommands in lifecycle order over
`microvms-core`, and nothing the library does not do (`microvms-cli/src/cli.rs:83`,
`microvms-cli/src/main.rs:2`). Thinness is checked three ways rather than intended — the direct
dependency set is an exact six-crate allowlist, no source file here names a transport or a
control-plane operation, and every AWS-touching command must fail when the library seam is made
to refuse (`microvms-cli/src/main.rs:10-13`, `microvms-cli/tests/thinness.rs:66`). A coding agent
is a first-class consumer, so `microvm manifest` emits the whole command tree with its option
domains, exit codes, and envelope schema generated from the parser, and every command writes
exactly one envelope object to stdout with progress on stderr
(`microvms-cli/src/main.rs:17-21`). There is no lib target, which is why the modules are declared
in `main.rs`, and `guards.rs` — the crate's largest file — holds the three guards that have to
inject a refusing seam from inside the crate and so compiles only under `cfg(test)`
(`microvms-cli/src/main.rs:23-28`, `microvms-cli/src/guards.rs:12-20`).

- `microvms-cli/src/guards.rs` (3125 LOC)
- `microvms-cli/src/cli.rs` (1723 LOC)
- `microvms-cli/src/exit.rs` (615 LOC)
- `microvms-cli/src/commands/attached.rs` (1173 LOC)
- `microvms-cli/src/commands/lifecycle.rs` (1303 LOC)
- `microvms-cli/src/render.rs` (901 LOC)
- `microvms-cli/src/seam.rs` (620 LOC)
- `microvms-cli/src/envelope.rs` (593 LOC)

## microvms-py

`microvms-py` is the PyO3 binding over `microvms-core`: a total, thin mapping where every public
core constructor gets one binding constructor and no arithmetic or coercion surface the core does
not have (`microvms-py/src/lib.rs:6-11`). No validation lives here — no range check, no state
check, no region check, no size check — because a guard added in a binding is the copy every
Python caller hits and the copy nothing else tests (`microvms-py/src/lib.rs:12-18`). Four
closures a binding could give away for free are each stopped by an absent surface rather than an
added check: no `__float__` on a dollar amount, no `__new__` on a duration, no region string on
any method, and the two hook timeouts as separate `#[pyclass]`es so transposing them is a
`TypeError` before any Rust runs (`microvms-py/src/lib.rs:20-40`). Methods are synchronous over
the async core, blocking on one shared multi-thread tokio runtime with the GIL released first
(`microvms-py/src/lib.rs:42-46`), and module membership is declared inside the `#[pymodule] mod`
so the committed `microvms.pyi` is a function of this file and `mise run stubs:check` fails when
the two disagree (`microvms-py/src/lib.rs:98-101`).

- `microvms-py/src/cost.rs` (1123 LOC)
- `microvms-py/src/sandbox.rs` (780 LOC)
- `microvms-py/src/exec.rs` (631 LOC)
- `microvms-py/src/session.rs` (605 LOC)
- `microvms-py/src/errors.rs` (226 LOC)
- `microvms-py/src/lib.rs` (138 LOC)
- `microvms-py/src/hooks.rs` (117 LOC)
- `microvms-py/src/runtime.rs` (92 LOC)

## microvms-js

`microvms-js` is the napi-rs binding over the same core under the same thin-mapping and
no-validation rules as the Python side, plus one module the Python side has no twin for —
`process`, the same exec seen as two byte streams for a consumer shaped like the AI SDK's
`SandboxProcess` (`microvms-js/src/lib.rs:6-17`, `microvms-js/src/lib.rs:72-74`). Its single most
important decision is `#[napi]` classes rather than `#[napi(object)]` for anything carrying a
closure: `#[napi(object)]` converts by structure, so `{ seconds: 3600 }` would satisfy a
`RunHookTimeout` and `{ amount: 1.5 }` an `EstimatedUsd`, which is precisely the coercion those
types exist to prevent (`microvms-js/src/lib.rs:19-35`). JS coerces more eagerly than Python, so
the money type carries no `valueOf`, no `toJSON`, and no `Symbol.toPrimitive` — the figure comes
out only through `.amount`, a string (`microvms-js/src/lib.rs:39-42`). Async maps straight through
with no `block_on` bridge, at the cost of the one divergence from the Python twin — napi's async
rejection path is typed over its own closed `Status` enum, so a caller branches on
`err.cause.message` rather than `err.code` (`microvms-js/src/lib.rs:58-67`) — and the generated
`index.js`, `index.d.ts`, and `.node` addon are untracked, so they are absent from the list below
(`.gitignore:27-29`).

- `microvms-js/src/cost.rs` (1038 LOC)
- `microvms-js/src/session.rs` (609 LOC)
- `microvms-js/src/exec.rs` (456 LOC)
- `microvms-js/src/sandbox.rs` (623 LOC)
- `microvms-js/src/process.rs` (541 LOC)
- `microvms-js/src/region.rs` (139 LOC)
- `microvms-js/src/lib.rs` (100 LOC)
- `microvms-js/src/errors.rs` (158 LOC)

## model

`model` builds the `agentd-model` crate, an executable specification rather than daemon code: a
state machine whose reachable states stateright enumerates exhaustively, plus the safety
properties the real daemon must uphold (`model/Cargo.toml:2`, `model/src/lib.rs:3-9`). It has one
dependency and no edge to any workspace member, because it models the protocol instead of
importing it (`model/Cargo.toml:9-10`). The question it settles is whether an in-VM process can
hijack the unauthenticated `/run` bootstrap hook, and it prices the unenforced invariant instead
of asserting it: `Config::attacker_before_bootstrap` toggles the assumption that no in-VM workload
runs before bootstrap, so the model reports both that the attacker never obtains authority while
the assumption holds and the concrete path by which it does once the assumption breaks
(`model/src/lib.rs:20-34`). `client.rs` is the deliberate sibling covering what `microvms-core`'s
`Sandbox` may do from outside the VM, where `State::wire` counts the calls the client issued so a
property can say no resume ever fires once `was_terminated` holds (`model/src/client.rs:2-9`,
`model/src/client.rs:23-30`).

- `model/src/client.rs` (982 LOC)
- `model/src/lib.rs` (658 LOC)
- `model/Cargo.toml` (10 LOC)

## Supporting code

Verification tooling, generated surfaces, and requirements data. None of it is a workspace crate
(`Cargo.toml:2-9`), and none of it is a module under the enumeration rule that skips
tooling-only paths.

- `conformance/run_rs.py` (2355 LOC)
- `microvms-py/microvms.pyi` (1476 LOC)
- `scripts/check-model-drift.py` (1085 LOC)
- `spec/core.symspec.json` (1049 LOC)
- `scripts/check-live-rates.py` (625 LOC)
- `scripts/check-live-wiring.py` (450 LOC)
- `scripts/generate-py-stubs.py` (345 LOC)
- `scripts/check-lint-coverage.py` (292 LOC)
- `conformance/infra/main.tf` (243 LOC)
- `spec/microvms-core-kickoff.md` (175 LOC)
- `scripts/verify-clean.py` (168 LOC)
- `examples/coding-agents-on-bedrock/run.sh` (122 LOC)
- `spec/agentd.symspec.json` (117 LOC)
- `scripts/check-license-headers.py` (112 LOC)
- `examples/coding-agents-on-bedrock/Dockerfile` (30 LOC)

Related: [System overview](system-overview.md) ·
[Data flow](data-flow.md) ·
[Contract map](../insights/contract-map.md) ·
[Impact analysis](../insights/impact-analysis.md) ·
[Tech debt](../insights/tech-debt.md) ·
[CLI reference](../reference/cli.md)

## See also

- [system overview](system-overview.md) — 8 shared source citations
- [business logic](../insights/business-logic.md) — 8 shared source citations
- [contract map](../insights/contract-map.md) — 8 shared source citations
- [impact analysis](../insights/impact-analysis.md) — 6 shared source citations
- [dependency graph](../diagrams/structural/dependency-graph.md) — 5 shared source citations
