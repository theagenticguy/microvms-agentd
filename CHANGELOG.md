# Changelog

Format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).
Versions are [semantic](https://semver.org/spec/v2.0.0.html); the wire contract in
`docs/PROTOCOL.md` is the public surface that versioning applies to.

## Unreleased

### Removed

- **`clients/python`** — the Python client, its 83 tests, and the two conformance
  scripts that imported it (`conformance/run.py`, the 56-check oracle;
  `conformance/probe_oom.py` and `conformance/probe_suspend_resume.py`). It was
  the discovery instrument: it found and closed fifteen client-side API traps and
  measured the platform's pricing and lifecycle semantics. All of that is pinned
  elsewhere now — in `docs/PLATFORM.md`, in `spec/core.symspec.json`, and in
  `microvms-core`'s own guards — and the Rust port has driven the live suite green
  against real AWS on the same commit the oracle last passed on. Git history keeps
  every line.

  What moved rather than went away: the rate-drift check is now
  `scripts/check-live-rates` (a PEP 723 uv script with its own pinned table, held
  equal to `microvms-core`'s `pinned_rates()`), and `scripts/check-model-drift`
  pins the region list and sizing table against its own literals, since those two
  values were verified by the Python-vs-Rust cross-comparison and by nothing else.

  What is genuinely lost: 34 of the oracle's 56 checks have no live coverage, because
  the `microvm` CLI has no `cp`, `ack`, `exec --stream`, `stdin`, or `health`
  subcommand. `conformance/run_rs.py` reports each one as SKIP by name.

## [0.1.0] — 2026-08-06

First release. Source only: there are no published binaries, and the daemon is
built from this tree.

### Added

- **`agentd/`** — the daemon, a static binary intended to run as the container
  `CMD`. One-shot token bootstrap from the platform's `/run` hook, authorization
  decided before any request body byte is read and compared in constant time on
  bytes, idempotent exec with caller-minted ids and ack-then-collect output
  capture, streaming tar upload and download with CPython `data`-filter parity,
  SSE output streaming with a byte-offset cursor, and opt-in stdin as a separate
  request with explicit EOF.
- **Operational guards** — panic recovery so a panicking handler cannot take the
  only channel into the VM with it (`panic = "unwind"` is deliberate and
  documented in `Cargo.toml`), a disk-pressure guard that refuses a write before
  it starts rather than surfacing ENOSPC mid-stream, and identity repair for VMs
  restored from a shared image (`/etc/machine-id`, hostname, `boot_id`,
  `random-seed`).
- **`model/`** — stateright model over every reachable bootstrap and exec state:
  seven safety properties plus six coverage properties, and a second
  configuration that deliberately breaks the deployment invariant and asserts the
  attack path is found.
- **`spec/`** — symspec requirements document for bootstrap and authorization,
  reporting `verified: true` under `--strict` via Z3.
- **`docs/PROTOCOL.md`** — the wire contract, with every rule traced to the
  defect that bought it.
- **`docs/PLATFORM.md`** — measured AWS behavior, each entry carrying its date,
  region, and API version.
- **`docs/schema.json`** — generated protocol schema, with a CI staleness check
  (`cargo run -p agentd --bin schema -- --check`).
- **`clients/python`** — `Session` and `ExecHandle` speak the wire protocol with
  no AWS dependency; `Sandbox` wraps the AWS lifecycle. Handles proxy-token
  minting across the 60-minute JWE ceiling, stream reconnect at the last good
  offset, and a typed error taxonomy separating a retryable 503 from a fatal 401.
- **`conformance/`** — the live suite against real Lambda MicroVMs plus a
  standalone suspend/resume probe, and the Terraform stack they need.
- **CI** — fmt, clippy `-D warnings`, `cargo test --all`, the schema staleness
  check, an `aarch64-unknown-linux-musl` cross-compile, and symspec strict.

### Verified

- **56 conformance checks passed, none failed, teardown clean** — 2026-08-05,
  us-east-1, API version `2025-09-09`. A 1.41 MB static `aarch64-musl` binary
  baked into a MicroVM image as the container `CMD` and driven through every
  protocol rule via the platform's own endpoint, including SSE surviving the
  endpoint proxy, stdin round-tripping through a child, and a suspend/resume cycle.
- **155 Rust tests across six targets and 83 Python client tests**, green as of
  2026-08-06. Every guard was verified to fail against the code without its fix.
- Two rounds of live runs found five defects no local tier could have caught, all
  of them wrong assumptions about the service rather than bugs in the daemon's
  logic: lifecycle hooks live under a fixed `/aws/lambda-microvms/runtime/v1/`
  prefix, `ready` and `validate` are called at image-build time, `runHookPayload`
  arrives wrapped in an envelope rather than as the request body, network
  connectors are ARNs, and `CreateMicrovmAuthToken` returns a header map. Each is
  in `docs/PLATFORM.md` with its date, and the transport tier was corrected so it
  fails against the old behavior.
- Suspend/resume is a freeze and restore, not a stop and start: the in-memory
  agent token, the filesystem, exec records, and running background processes all
  survive, and the endpoint URL is unchanged. This inverted what the project had
  assumed, and the daemon's resume-hook docstring had claimed the opposite by
  reasoning from where state lives rather than measuring it.

### Not yet

- **One region.** Every live measurement is us-east-1. Nothing has been re-run
  elsewhere, so no `docs/PLATFORM.md` entry should be assumed regional-invariant.
- **`Sandbox` is verified only against fakes plus the conformance run.** The
  protocol layer (`Session`, `ExecHandle`) has unit coverage; the AWS lifecycle
  wrapper has one live path and no test suite of its own.
- **No published binaries.** No release artifacts, no crates.io publish, no PyPI
  publish. CI uploads an `aarch64-musl` build artifact per run; that is not a
  release.
- **CI has never executed against a remote.** The repository had no git remote
  until this release, so every gate has only been run locally. The cross-compile
  and symspec jobs in particular are unproven in GitHub's environment.
- **No fork or process-tree snapshot**, and none planned — see
  `docs/STRATEGY.md` for why it is unavailable above the hypervisor.
- **No orchestrator, no PTY, no AgentCore parity.** Deliberately out of scope.
