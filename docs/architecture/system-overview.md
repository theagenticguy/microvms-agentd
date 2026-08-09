# microvms-agentd · System overview

AWS Lambda MicroVMs hands you an isolated Firecracker VM and no way to run anything inside
it: the service has no exec API and no file-transfer API (`docs/PLATFORM.md:12`). Every
harness that wraps the service therefore has to write its own in-VM daemon to supply both.
This repository is that daemon, plus the client stack that drives it and the verification
harness that keeps both honest. The daemon is a static binary intended to run as the
container `CMD` inside the VM (`agentd/src/main.rs:4`); the client is a library that closes
the measured platform traps once so no caller pays for them again
(`microvms-core/src/lib.rs:5-14`). The audience is whoever builds a sandbox product on
MicroVMs — an agent harness, a CI runner, a code-execution service.

The workspace declares seven members (`Cargo.toml:2-10`) plus a live conformance suite.
`protocol` is the wire contract as Rust types: pure data, serde plus schemars, deliberately
no tokio, no axum, no base64 — the rule is whether a type is what travels or machinery for
making it travel (`protocol/src/lib.rs:16-21`, 600 LOC). Both the daemon and the client
compile against it, so a renamed field breaks the other side's build rather than a
consumer's runtime (`agentd/Cargo.toml:10-14`).

`agentd` is the daemon: ten modules whose seams follow defect classes rather than the HTTP
surface — `state` owns the one-shot bootstrap, `auth` decides authorization before a body
byte is read, `exec` owns idempotent exec, `fs` owns streaming tar, `serve` is generic over
the listener so faults can be simulated (`agentd/src/lib.rs:31-40`, 11988 LOC). Its router
is assembled by walking the same endpoint list `/v1/schema` publishes, so a documented route
with no handler panics at startup (`agentd/src/routes.rs:29-34`). Eighteen routes: six
platform lifecycle hooks under a fixed prefix, six `/v1/exec/*`, four `/v1/fs/*`, health, and
schema (`agentd/src/routes.rs:112-137`).

`microvms-core` is the client library and the largest crate (20608 LOC). Its own doc comment
splits it: `error`, `region`, `sizing`, `hooks`, `constants` are the foundation; `cost`,
`control`, `session`, `sandbox` are the product surface (`microvms-core/src/lib.rs:59-63`).
`control` is hand-signed SigV4 rest-json with each trap closed before the request leaves the
process (`microvms-core/src/control/mod.rs:13-29`), `session` is the in-VM client with proxy
auth and a resumable byte cursor (`microvms-core/src/session/mod.rs:1-7`), `sandbox` is the
lifecycle state machine whose private fields mirror the verified model
(`microvms-core/src/sandbox.rs:9-17`), and `cost` is decimal money where unpriced is a
distinct variant rather than zero (`microvms-core/src/cost.rs:22-27`).

`microvms-cli` ships the `microvm` binary — 16 subcommands, one JSON envelope per invocation
on stdout with progress on stderr (`microvms-cli/src/cli.rs:97-215`,
`microvms-cli/src/envelope.rs:4-11`). It has no lib target, so nothing can depend on it, and
its direct dependency set is an allowlist of exactly six names asserted by a test that reads
`cargo metadata` (`microvms-cli/Cargo.toml:9-42`). AWS is reachable only through
`microvms-core`. `microvms-py` and `microvms-js` wrap that same core through PyO3 and
napi-rs, never through the CLI (`microvms-py/Cargo.toml:22-26`).

Verification lives in two more places. `model` is a stateright model of the bootstrap and
exec lifecycle with one dependency and no edge to any member — it models the protocol rather
than importing it (`model/Cargo.toml:8-9`). `conformance/run_rs.py` drives the built CLI
against real AWS through 75 named checks (`conformance/run_rs.py:8-10`). Start reading at
`agentd/src/lib.rs` for the trust boundary, then `microvms-core/src/lib.rs` for the trap
ladder.

## Stack

| Layer | Technology | Source |
| --- | --- | --- |
| Language | Rust, edition 2024, resolver 3, stable toolchain | `Cargo.toml:23`, `Cargo.toml:11`, `rust-toolchain.toml:14` |
| Shipping target | `aarch64-unknown-linux-musl` static binary, size-tuned release profile | `rust-toolchain.toml:16`, `Cargo.toml:30-31` |
| Daemon HTTP | `axum = "0.8.9"` with `tower-http` `limit` + `catch-panic` | `agentd/Cargo.toml:15`, `agentd/Cargo.toml:24` |
| Async runtime | `tokio = "1.53"`, current-thread in the daemon | `agentd/Cargo.toml:25`, `agentd/src/main.rs:24-27` |
| AWS control plane | `reqwest` on rustls plus hand-rolled `aws-sigv4` signing | `microvms-core/Cargo.toml:75`, `microvms-core/Cargo.toml:69` |
| Wire schema | `schemars = "1.2.2"`, `preserve_order` off so the artifact is reproducible | `protocol/Cargo.toml:15` |
| Money | `rust_decimal` with `serde-with-str` | `microvms-core/Cargo.toml:45` |
| CLI surface | `clap` derive plus `ratatui` for the interactive view | `microvms-cli/Cargo.toml:61`, `microvms-cli/Cargo.toml:65` |
| Bindings | `pyo3` `abi3-py39` via maturin; `napi = "3"` with `async` | `microvms-py/Cargo.toml:38`, `microvms-js/Cargo.toml:30` |
| Verification | `stateright`, `turmoil`, `proptest`; live suite in PEP 723 Python | `model/Cargo.toml:9`, `agentd/Cargo.toml:75`, `conformance/run_rs.py:1-5` |
| Build and gates | `mise` tasks; `check` is the definition of done | `mise.toml:203-205` |

## Module map

```mermaid
flowchart LR
  protocol[protocol wire types]
  agentd[agentd daemon]
  core[microvms-core]
  cli[microvms-cli]
  py[microvms-py]
  js[microvms-js]
  model[model stateright]
  conf[conformance]

  agentd --> protocol
  core --> protocol
  cli --> core
  cli --> protocol
  py --> core
  py --> protocol
  js --> core
  js --> protocol
  conf -->|drives| cli
  model -.checks.-> agentd
```

## See also

- [microvms-agentd · Impact analysis](../insights/impact-analysis.md)
- [microvms-agentd · Module map](module-map.md)
- [microvms-agentd · Contract map](../insights/contract-map.md)
- [microvms-agentd · Tech debt](../insights/tech-debt.md)
- [microvms-agentd · Public API](../reference/public-api.md)
