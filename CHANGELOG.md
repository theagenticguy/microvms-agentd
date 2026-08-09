# Changelog

Format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).
Versions are [semantic](https://semver.org/spec/v2.0.0.html); the wire contract in
`docs/PROTOCOL.md` is the public surface that versioning applies to.

## Unreleased

### Added

- **Five `microvm` subcommands, and the live coverage they unlock.** `microvm health`,
  `microvm ack`, `microvm stdin`, `microvm cp` (with `--tar` and `--mode`), and five new
  shapes on `exec` — `--exec-id` for an idempotent retry, `--poll` for a read-only status
  read, `--detach` to start without waiting or acking, `--stream` for output as it arrives,
  `--stdin` for feeding a child. All five go through `microvms-core`'s existing session
  surface and through the one `attach_session` seam; no daemon, protocol, or core change was
  needed, which is what `docs/CLI-COVERAGE-PLAN.md` predicted and is the reason it was one
  task rather than four.

  `--detach` was added after the first live round rather than designed in, and it is worth
  its own note: every other `exec` shape ends in start-wait-**ack**, and that ack releases
  the output irreversibly — a second one is a 409 and a later poll reports `acked` with
  nothing. A caller who wants to own an exec's lifecycle (start now, poll later, ack when
  ready, possibly from another process, since the record lives in the VM) needs a start that
  stops after starting.

  `cp --tar` is asymmetric and the asymmetry is the design: the `vm:` side is a **directory**
  that the daemon packs or extracts through `/v1/fs/tar`, and the local side is a `.tar`
  **file**, because neither this binary nor `microvms-core` carries a tar library — which
  keeps the daemon's confined extractor the only extractor in the system. Nothing outside the
  daemon packs or unpacks, including the guest, whose base image may have no `tar` at all.

  `exec --stream` is the one documented exception to "exactly one envelope on stdout": it
  emits NDJSON — one event object per line, the envelope last — under a **different**
  discriminant, `microvm.exec.stream`. `microvm manifest` publishes the alternate shape as
  `exec`'s `alternateResponse` and states the exception in its `conventions`, so a consumer
  discovers it rather than encountering it. Stream chunks are the command's *output*, not
  progress, which is why they cannot go on stderr; buffering them to keep stdout one
  document would remove the only reason to stream.

  This added a direct dependency on `futures-util` for the `Stream` trait
  `ExecHandle::stream_with` returns, with a paragraph in `tests/thinness.rs` justifying it
  and a note that a callback driver in `microvms-core` was the preferred fix. That fix has
  since landed — see `ExecHandle::for_each_event` below — and the dependency is gone again,
  so the CLI is back to six direct dependencies.

- **`ExecHandle::for_each_event`** in `microvms-core`: a callback driver over the same
  reconnecting stream state machine `stream_with` runs, taking a
  `FnMut(ExecEvent) -> ControlFlow<()>` and returning a `StreamEnd` that names *why* the
  stream ended — `Exited`, `Stopped` (the callback broke), or `Cut` (a body with no terminal
  event) — plus core's own cursor to resume at. Both types are `std`, so a consumer no longer
  has to name the crate that defines `Stream` in order to advance one.

  `microvm exec --stream` moved onto it and `microvm`'s direct dependency on `futures-util`
  came out with it; `microvms-cli/tests/thinness.rs` now asserts that edge stays out and
  names the replacement API in its failure message. The bindings can make the same move and
  have not: their stream tasks `.await` a bounded `send`, which a synchronous callback can
  only do as a `blocking_send` — blocking the runtime worker the driver runs on. Both
  `microvms-py/src/exec.rs` and `microvms-js/src/exec.rs` record that as the reason.

- **`impl FromStr for CostPhase`** and **`CostPhase::ALL`** in `microvms-core`. Both
  bindings judged a bare phase string against their own hand-written seven-element table —
  two parallel lists over one closed enum. They now parse through core, and a phase added to
  the enum appears in the refusal message without an edit. A round-trip test covers every
  variant by exhaustive match, so adding one without adding it to `ALL` fails to compile.

- **`RateTable::minimum_retention_days`**, so the floored-storage note reads its day count
  off the rate row instead of dividing `as_secs()` by 86,400 beside the message.

### Changed

- **CI runs on Node 24 actions throughout.** `checkout@v5`, `setup-uv@v7`,
  `upload-artifact@v6`, `setup-node@v5`, `setup-terraform@v4` — the first major of each on
  the runtime that replaced the Node 20 the runner deprecated, so every green run is now
  warning-free instead of printing a deprecation notice per step. `ci.yml`'s header records
  why `checkout` stops at v5 and why `setup-uv` stays on a rolling major. `enable-cache:
  false` on every `setup-uv` step, because there is no lockfile in this repo to key a cache
  on — every Python entry point is a `uvx` invocation or a PEP 723 script — and a cache that
  can never be invalidated would pin the boto3 whose bundled service model the drift gate
  exists to read fresh. `configure-aws-credentials@v4` is the one step still on Node 20;
  upstream has no Node 24 major yet.

- **Three accepted debts now carry their reasons in the code**, rather than in a session
  document a reader cannot open: why there is no `Sandbox::attach` for the three attached
  lifecycle commands (`microvms-cli/src/commands/lifecycle.rs` — adding one would
  manufacture a second initial state, and both the symspec and stateright models declare
  exactly one, so their proofs would stop covering it); why `microvm logs` refuses to read
  CloudWatch rather than growing a reader (`commands/local.rs` — a second signing name and
  host in a transport whose single-service-ness is four readable constants, for a read no
  role in `conformance/infra` is granted); and why the JSON envelope's dollar strings may
  differ in trailing zeros from the retired Python oracle's (`render.rs` — numerically equal,
  `rust_decimal` normalizes scale differently, and rescaling would round a figure whose
  exactness is why it is a string).

- **`conformance/run_rs.py` expresses every named check.** The `UNSUPPORTED` table and its
  `unsupported()` primitive are gone: all 34 entries became real live check bodies under
  the names `conformance/run.py` gave them, so this suite's report diffs line for line
  against the last recorded oracle run in git history — `SKIP` there, `PASS` here. The four
  hostile archives are hand-built with `tarfile` (GNU tar sanitizes several of them) and
  handed to `microvm cp --tar`; the expected failure is the **daemon's** refusal surfacing
  as `data.kind: ProtocolError`, because the CLI deliberately does not pre-validate an
  archive and a byte-scan guard proves it.

  75 checks rather than the plan's 72: two of the old 38 were weak readings off the launch
  envelope and are now asserted directly against `microvm health`, and three checks are
  new. `Results.skipped` and a `skip()` primitive remain with no live caller, exercised by
  `--self-test`, because a suite that removed its own ability to report a gap is a suite
  whose next gap is silent.

  The first live round found seven failures across two clusters, both in this driver rather
  than in the five subcommands: a tar chain that shelled out to a `tar` binary the base image
  does not have (deleted — it tested the image's tooling) and pointed `--tar` at a file where
  the route wants a directory, and a start/poll/ack sequence that could not be expressed
  because `exec` acked its own output. The second is what `--detach` exists for. Fixed and
  re-verified offline; the live tier is rerun by the orchestrator.

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

  What was genuinely lost, and has since been recovered: 34 of the oracle's 56 checks
  had no live coverage, because the `microvm` CLI had no `cp`, `ack`, `exec --stream`,
  `stdin`, or `health` subcommand, and `conformance/run_rs.py` reported each one as SKIP
  by name. Those five subcommands landed in the same Unreleased cycle (see Added above)
  and every SKIP became a real check — so the loss lasted one release and is recorded
  here rather than edited out, because "the CLI grew the subcommand" is the outcome the
  SKIP list existed to make actionable.

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
- **CI had never executed against a remote** when this was written. It has since:
  every job in `.github/workflows/ci.yml` runs on push and is green, cross-compile
  included. The symspec job is the one that is still unproven, and for a different
  reason — it is not in the workflow at all, because the version this repo needs is
  not installable from a registry (the comment at the end of `ci.yml` says so).
- **No fork or process-tree snapshot**, and none planned — see
  `docs/STRATEGY.md` for why it is unavailable above the hypervisor.
- **No orchestrator, no PTY, no AgentCore parity.** Deliberately out of scope.
