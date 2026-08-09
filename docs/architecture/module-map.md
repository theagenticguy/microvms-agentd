# microvms-agentd · Module map

The workspace declares seven members (`Cargo.toml:2-10`). Modules below are ordered as in the
`system-overview.md` flowchart, followed by a `Supporting code` section for the verification
tooling that is not a crate.

## microvms-core

The client library: the control plane, the session client, the cost engine, and every trap
closure in one crate, whose own doc comment splits it into a foundation (`error`, `region`,
`sizing`, `hooks`, `constants`) and a product surface (`cost`, `control`, `session`, `sandbox`)
(`microvms-core/src/lib.rs:59-63`). It exists to spend each measured platform finding once so no
caller has to measure it again, and it ranks every closure on a strength ladder where S1 means the
mistake cannot be written down at all (`microvms-core/src/lib.rs:25-30`). It re-exports `protocol`
so consumers name wire types through this crate rather than depending on the contract directly,
which is what the CLI's thinness guard counts on (`microvms-core/src/lib.rs:75-77`). It is the
largest crate in the repo and the only member the CLI and both bindings depend on
(`microvms-cli/Cargo.toml:46`, `microvms-py/Cargo.toml:26`, `microvms-js/Cargo.toml:21`).

- `microvms-core/src/cost.rs` (4125 LOC)
- `microvms-core/src/sandbox.rs` (1991 LOC)
- `microvms-core/src/session/exec.rs` (1372 LOC)
- `microvms-core/src/control/microvm.rs` (1235 LOC)
- `microvms-core/src/control/image.rs` (1026 LOC)
- `microvms-core/src/control/transport.rs` (1018 LOC)
- `microvms-core/src/session/mod.rs` (996 LOC)
- `microvms-core/src/error.rs` (578 LOC)

## microvms-cli

The `microvm` binary: a second door onto `microvms-core`'s room, which parses, renders, and exits
with a code while every AWS call and every trap guard stays in the library
(`microvms-cli/src/main.rs:2-13`). That thinness is checked three ways rather than intended — the
dependency set is an exact allowlist, no source file here names a transport or a control-plane
operation, and every AWS-touching command must fail when the library seam is made to refuse
(`microvms-cli/src/main.rs:8-13`). A coding agent is a first-class consumer, so `microvm manifest`
generates the whole command tree with its option domains from the clap parser rather than from a
hand-maintained list (`microvms-cli/src/manifest.rs:6-10`), and every command emits exactly one
JSON envelope on stdout with progress on stderr (`microvms-cli/src/envelope.rs:4-11`). There is
deliberately no lib target, so nothing can import from here — which is why the modules are declared
in `main.rs` (`microvms-cli/src/main.rs:24-28`).

- `microvms-cli/src/guards.rs` (2249 LOC)
- `microvms-cli/src/cli.rs` (1265 LOC)
- `microvms-cli/src/commands/attached.rs` (1138 LOC)
- `microvms-cli/src/commands/lifecycle.rs` (1090 LOC)
- `microvms-cli/src/render.rs` (899 LOC)
- `microvms-cli/src/exit.rs` (613 LOC)
- `microvms-cli/src/envelope.rs` (596 LOC)
- `microvms-cli/src/seam.rs` (556 LOC)

## agentd

The daemon that supplies the exec and file-transfer APIs the platform does not have, running as the
container `CMD` inside the VM (`agentd/src/lib.rs:2-7`). Its ten modules are seamed by defect class
rather than by HTTP surface: `state` owns the one-shot bootstrap, `auth` decides authorization
before a body byte is read, `exec` owns idempotent start and ack-gated release, `fs` owns streaming
tar (`agentd/src/lib.rs:31-40`). The trust boundary is the crate's central fact — the platform's own
`/run` hook arrives from `127.0.0.1`, indistinguishable from an in-VM process, so source-address
filtering is wrong rather than merely unverified and the one-shot bootstrap is the only available
defense (`agentd/src/lib.rs:11-16`). The router is assembled by walking the same endpoint list
`/v1/schema` publishes, so a documented route with no handler fails at startup
(`agentd/src/routes.rs:31`, `agentd/src/routes.rs:36`).

- `agentd/src/exec.rs` (3035 LOC)
- `agentd/src/fs.rs` (1631 LOC)
- `agentd/src/schema.rs` (873 LOC)
- `agentd/src/identity.rs` (737 LOC)
- `agentd/src/routes.rs` (594 LOC)
- `agentd/src/disk.rs` (435 LOC)
- `agentd/src/state.rs` (317 LOC)
- `agentd/src/config.rs` (196 LOC)

## microvms-py

PyO3 bindings over `microvms-core`: a total, thin mapping where every public core constructor gets
one binding constructor and no arithmetic or coercion surface the core does not have
(`microvms-py/src/lib.rs:5-9`). No validation lives here — no range check, no state check, no region
check — because a guard added in a binding is the copy every Python caller hits and the copy nothing
else tests (`microvms-py/src/lib.rs:15-19`). The crate is organized around the four closures a
binding could give away for free, each stopped by an absence rather than a check: no `__float__` on
a dollar amount, no `__new__` on a duration, no region string on any method, and two hook timeouts
as separate `#[pyclass]`es so transposing them is a `TypeError` before any Rust runs
(`microvms-py/src/lib.rs:21-38`). Methods are synchronous over the async core, blocking on one
shared multi-thread tokio runtime with the GIL released first (`microvms-py/src/lib.rs:42-44`).

- `microvms-py/src/cost.rs` (1152 LOC)
- `microvms-py/src/sandbox.rs` (764 LOC)
- `microvms-py/src/exec.rs` (640 LOC)
- `microvms-py/src/session.rs` (554 LOC)
- `microvms-py/src/errors.rs` (226 LOC)
- `microvms-py/src/region.rs` (150 LOC)
- `microvms-py/src/hooks.rs` (124 LOC)
- `microvms-py/src/runtime.rs` (92 LOC)

## microvms-js

napi-rs bindings over the same core, with the same thin-mapping and no-validation rules as the
Python side (`microvms-js/src/lib.rs:5-10`, `microvms-js/src/lib.rs:12-18`). The load-bearing
decision is `#[napi]` classes rather than `#[napi(object)]` for anything carrying a closure:
`#[napi(object)]` converts by structure, so `{ seconds: 3600 }` would satisfy a `RunHookTimeout`
and `{ amount: 1.5 }` an `EstimatedUsd`, which is exactly the coercion those types exist to prevent
(`microvms-js/src/lib.rs:20-27`). JS coerces more eagerly than Python, so the money type carries no
`valueOf`, no `toJSON`, and no `Symbol.toPrimitive` — the figure comes out only through `.amount` as
a string (`microvms-js/src/lib.rs:40-44`). Async maps straight through with no `block_on` bridge,
unlike the Python side (`microvms-js/src/lib.rs:58-60`); `index.d.ts` and `index.js` are generated
and gitignored (`.gitignore:21-22`), so they are absent from the list below.

- `microvms-js/src/cost.rs` (1061 LOC)
- `microvms-js/src/sandbox.rs` (622 LOC)
- `microvms-js/src/exec.rs` (462 LOC)
- `microvms-js/src/session.rs` (436 LOC)
- `microvms-js/src/errors.rs` (158 LOC)
- `microvms-js/src/region.rs` (139 LOC)
- `microvms-js/src/lib.rs` (97 LOC)
- `microvms-js/src/hooks.rs` (96 LOC)

## protocol

The wire contract as Rust types, shared by the daemon and every Rust client of it, so a field
renamed here breaks compilation on whichever side has not caught up — the earliest and cheapest
place a protocol change can fail (`protocol/src/lib.rs:2-13`). It carries pure data only: serde plus
schemars, no tokio, no axum, no base64, with the admission rule being whether a type is what travels
or machinery for making it travel (`protocol/src/lib.rs:15-22`). Every type derives both halves of
serde even where one side needs only one, because the missing half is exactly what a client would
have to hand-write, and `docs/schema.json` is generated from these attributes under both contracts
and byte-compared in CI (`protocol/src/lib.rs:23-27`). `PROTOCOL_VERSION` tracks the `/v1/` path
namespace rather than the crate version, and there is deliberately no negotiation request
(`protocol/src/lib.rs:58`, `protocol/src/lib.rs:56-57`).

- `protocol/src/exec.rs` (346 LOC)
- `protocol/src/health.rs` (82 LOC)
- `protocol/src/lib.rs` (66 LOC)
- `protocol/src/hook.rs` (64 LOC)
- `protocol/src/fs.rs` (42 LOC)

## model

An executable specification rather than daemon code: a state machine whose reachable states
stateright enumerates exhaustively, plus the safety properties the real daemon must uphold
(`model/src/lib.rs:3-7`). It exists to settle with a proof over every interleaving what was
previously argued in prose — above all whether an in-VM process can hijack the unauthenticated
`/run` bootstrap hook (`model/src/lib.rs:7-10`). Its sharpest move is pricing an unenforced
invariant instead of asserting it is fine: `Config::attacker_before_bootstrap` toggles the
assumption that no in-VM workload runs before bootstrap, and the model reports both that the
attacker never gains authority when it holds and the concrete path by which it does when it breaks
(`model/src/lib.rs:27-34`, `model/src/lib.rs:170`). `client.rs` is the deliberate sibling covering
what `Sandbox` may do from *outside* the VM (`model/src/client.rs:3-9`).

- `model/src/client.rs` (912 LOC)
- `model/src/lib.rs` (655 LOC)
- `model/Cargo.toml` (9 LOC)

## Supporting code

Verification tooling and requirements documents; none is a workspace crate.

- `conformance/run_rs.py` (2294 LOC) — the only live suite, driving the built CLI against real AWS through 75 named checks (`conformance/run_rs.py:8-9`).
- `conformance/infra/main.tf` (242 LOC)
- `spec/core.symspec.json` (1049 LOC) — 51 requirements in the v5 `docVersion: 3` format, checked with a state model the 0.1.0 `symspec` on PATH cannot read (`mise.toml:128-130`).
- `spec/microvms-core-kickoff.md` (175 LOC)
- `spec/agentd.symspec.json` (117 LOC) — six bootstrap and control-token requirements, gated by `symspec check --strict` (`mise.toml:135`).
- `scripts/check-model-drift` (807 LOC) — checks the constraints the client hardcodes against the shipped service model (`mise.toml:185`).
- `scripts/check-live-rates` (624 LOC) — fails if the pinned rate table no longer matches the AWS Pricing API (`mise.toml:275`).
- `scripts/verify-clean` (167 LOC) — asks the account what the project left behind rather than trusting teardown (`mise.toml:282`).
- `scripts/check-license-headers` (81 LOC) — proves every tracked `.rs` and `.py` file carries its SPDX line (`lefthook.yml:31`).

## See also

- [microvms-agentd · System overview](system-overview.md)
- [microvms-agentd · Contract map](../insights/contract-map.md)
- [microvms-agentd · Impact analysis](../insights/impact-analysis.md)
- [microvms-agentd · Tech debt](../insights/tech-debt.md)
- [microvms-agentd · CLI](../reference/cli.md)
