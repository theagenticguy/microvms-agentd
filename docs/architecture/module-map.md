# microvms-agentd · Module map

The workspace declares seven members (`Cargo.toml:2-10`). Modules below are ordered as in the
`system-overview.md` flowchart, followed by a `Supporting code` section for the verification
tooling that is not a crate.

## microvms-core

This crate is the client library. It holds the control plane, the session client, the cost
engine, and every trap closure. Its own doc comment splits it into a foundation (`error`,
`region`, `sizing`, `hooks`, `constants`) and a product surface (`cost`, `control`, `session`,
`sandbox`) (`microvms-core/src/lib.rs:59-63`). The crate encodes each measured platform finding
once, so no caller has to measure it again. It also ranks every closure on a strength ladder,
where S1 means the mistake cannot be written down at all (`microvms-core/src/lib.rs:25-30`). It
re-exports `protocol`, so consumers name wire types through this crate instead of depending on
the contract crate directly. The CLI's thinness guard depends on that re-export
(`microvms-core/src/lib.rs:75-77`). It is the largest crate in the repo and the only member the
CLI and both bindings depend on
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

This crate builds the `microvm` binary, a thin front end over `microvms-core`. It parses
arguments, renders output, and exits with a code. Every AWS call and every trap guard stays in
the library (`microvms-cli/src/main.rs:2-13`). Three checks enforce that thinness. The dependency
set is an exact allowlist, no source file here names a transport or a control-plane operation,
and every AWS-touching command must fail when the library seam is made to refuse
(`microvms-cli/src/main.rs:8-13`). A coding agent is a first-class consumer, so `microvm
manifest` generates the whole command tree with its option domains from the clap parser instead
of from a hand-maintained list (`microvms-cli/src/manifest.rs:6-10`). Every command emits exactly
one JSON envelope on stdout, with progress on stderr (`microvms-cli/src/envelope.rs:4-11`). The
crate has no lib target, so nothing can import from here. Because there is no lib target, the
modules are declared in `main.rs` (`microvms-cli/src/main.rs:24-28`).

- `microvms-cli/src/guards.rs` (2249 LOC)
- `microvms-cli/src/cli.rs` (1265 LOC)
- `microvms-cli/src/commands/attached.rs` (1138 LOC)
- `microvms-cli/src/commands/lifecycle.rs` (1090 LOC)
- `microvms-cli/src/render.rs` (899 LOC)
- `microvms-cli/src/exit.rs` (613 LOC)
- `microvms-cli/src/envelope.rs` (596 LOC)
- `microvms-cli/src/seam.rs` (556 LOC)

## agentd

This crate is the daemon that supplies the exec and file-transfer APIs the platform does not
have. It runs as the container `CMD` inside the VM (`agentd/src/lib.rs:2-7`). Its ten modules are
divided by defect class rather than by HTTP surface. `state` owns the one-shot bootstrap, `auth`
decides authorization before a body byte is read, `exec` owns idempotent start and ack-gated
release, and `fs` owns streaming tar (`agentd/src/lib.rs:31-40`). The trust boundary shapes the
whole crate. The platform's own `/run` hook arrives from `127.0.0.1`, so it looks identical to a
request from an in-VM process. Because of that, source-address filtering cannot distinguish the
two, and the one-shot bootstrap is the only available defense (`agentd/src/lib.rs:11-16`). The
router is assembled by walking the same endpoint list `/v1/schema` publishes, so a documented
route with no handler fails at startup (`agentd/src/routes.rs:31`, `agentd/src/routes.rs:36`).

- `agentd/src/exec.rs` (3035 LOC)
- `agentd/src/fs.rs` (1631 LOC)
- `agentd/src/schema.rs` (873 LOC)
- `agentd/src/identity.rs` (737 LOC)
- `agentd/src/routes.rs` (594 LOC)
- `agentd/src/disk.rs` (435 LOC)
- `agentd/src/state.rs` (317 LOC)
- `agentd/src/config.rs` (196 LOC)

## microvms-py

This crate holds the PyO3 bindings over `microvms-core`. The mapping is total and thin: every
public core constructor gets one binding constructor, and the bindings add no arithmetic or
coercion surface the core does not have (`microvms-py/src/lib.rs:5-9`). No validation lives here.
There is no range check, no state check, and no region check, because a guard added in a binding
would be the copy every Python caller hits and the copy nothing else tests
(`microvms-py/src/lib.rs:15-19`). The crate is organized around four closures a binding could
give away for free. Each is stopped by omitting a surface rather than adding a check: there is no
`__float__` on a dollar amount, no `__new__` on a duration, and no region string on any method,
and the two hook timeouts are separate `#[pyclass]`es so transposing them is a `TypeError` before
any Rust runs (`microvms-py/src/lib.rs:21-38`). Methods are synchronous over the async core. They
block on one shared multi-thread tokio runtime and release the GIL first
(`microvms-py/src/lib.rs:42-44`).

- `microvms-py/src/cost.rs` (1152 LOC)
- `microvms-py/src/sandbox.rs` (764 LOC)
- `microvms-py/src/exec.rs` (640 LOC)
- `microvms-py/src/session.rs` (554 LOC)
- `microvms-py/src/errors.rs` (226 LOC)
- `microvms-py/src/region.rs` (150 LOC)
- `microvms-py/src/hooks.rs` (124 LOC)
- `microvms-py/src/runtime.rs` (92 LOC)

## microvms-js

This crate holds the napi-rs bindings over the same core. It follows the same thin-mapping and
no-validation rules as the Python side (`microvms-js/src/lib.rs:5-10`,
`microvms-js/src/lib.rs:12-18`). The key decision is to use `#[napi]` classes rather than
`#[napi(object)]` for anything carrying a closure. `#[napi(object)]` converts by structure, so
`{ seconds: 3600 }` would satisfy a `RunHookTimeout` and `{ amount: 1.5 }` an `EstimatedUsd`.
That structural conversion is exactly the coercion those types exist to prevent
(`microvms-js/src/lib.rs:20-27`). JS coerces more eagerly than Python, so the money type carries
no `valueOf`, no `toJSON`, and no `Symbol.toPrimitive`. The figure comes out only through
`.amount` as a string (`microvms-js/src/lib.rs:40-44`). Async maps straight through with no
`block_on` bridge, unlike the Python side (`microvms-js/src/lib.rs:58-60`). `index.d.ts` and
`index.js` are generated and gitignored (`.gitignore:21-22`), so they are absent from the list
below.

- `microvms-js/src/cost.rs` (1061 LOC)
- `microvms-js/src/sandbox.rs` (622 LOC)
- `microvms-js/src/exec.rs` (462 LOC)
- `microvms-js/src/session.rs` (436 LOC)
- `microvms-js/src/errors.rs` (158 LOC)
- `microvms-js/src/region.rs` (139 LOC)
- `microvms-js/src/lib.rs` (97 LOC)
- `microvms-js/src/hooks.rs` (96 LOC)

## protocol

This crate expresses the wire contract as Rust types. The daemon and every Rust client of it
share the crate, so a field renamed here breaks compilation on whichever side has not caught up.
Compilation is the earliest and cheapest place a protocol change can fail
(`protocol/src/lib.rs:2-13`). It carries pure data only, with serde plus schemars and no tokio,
axum, or base64. A type is admitted if it is what travels on the wire, and excluded if it is
machinery for making it travel (`protocol/src/lib.rs:15-22`). Every type derives both halves of
serde even where one side needs only one, because the missing half is exactly what a client would
otherwise have to hand-write. `docs/schema.json` is generated from these attributes under both
contracts and byte-compared in CI (`protocol/src/lib.rs:23-27`). `PROTOCOL_VERSION` tracks the
`/v1/` path namespace rather than the crate version, and there is deliberately no negotiation
request (`protocol/src/lib.rs:58`, `protocol/src/lib.rs:56-57`).

- `protocol/src/exec.rs` (346 LOC)
- `protocol/src/health.rs` (82 LOC)
- `protocol/src/lib.rs` (66 LOC)
- `protocol/src/hook.rs` (64 LOC)
- `protocol/src/fs.rs` (42 LOC)

## model

This crate is an executable specification, not daemon code. It contains a state machine whose
reachable states stateright enumerates exhaustively, plus the safety properties the real daemon
must uphold (`model/src/lib.rs:3-7`). It replaces prose arguments with a check over every
interleaving. The main question it settles is whether an in-VM process can hijack the
unauthenticated `/run` bootstrap hook (`model/src/lib.rs:7-10`). The model also makes an
unenforced invariant testable in both directions. `Config::attacker_before_bootstrap` toggles the
assumption that no in-VM workload runs before bootstrap. When the assumption holds, the model
reports that the attacker never gains authority. When it breaks, the model reports the concrete
path by which the attacker gains authority (`model/src/lib.rs:27-34`, `model/src/lib.rs:170`).
`client.rs` is the deliberate sibling covering what `Sandbox` may do from *outside* the VM
(`model/src/client.rs:3-9`).

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
- `scripts/verify-clean` (167 LOC) — queries the account for resources the project left behind, instead of assuming teardown succeeded (`mise.toml:282`).
- `scripts/check-license-headers` (81 LOC) — checks that every tracked `.rs` and `.py` file carries its SPDX line (`lefthook.yml:31`).

## See also

- [microvms-agentd · System overview](system-overview.md)
- [microvms-agentd · Contract map](../insights/contract-map.md)
- [microvms-agentd · Impact analysis](../insights/impact-analysis.md)
- [microvms-agentd · Tech debt](../insights/tech-debt.md)
- [microvms-agentd · CLI](../reference/cli.md)
