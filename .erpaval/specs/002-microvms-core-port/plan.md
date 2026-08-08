# Plan: microvms-core — port the client stack to Rust

Derived from `spec/core.symspec.json` (51 requirements: ARCH-5, TRAP-13, COST-10,
STATE-12, BIND-5, CLI-6), `.erpaval/specs/001-control-plane-client/spec.md` (the
S1/S2/S3 ladder and falsification discipline), and `.erpaval/microvms-core-kickoff.md`
(the architecture, decided). Session: `.erpaval/sessions/session-fa0814/`.

## Layout (decided by the kickoff; not reopened)

```
protocol/        NEW  wire types extracted from agentd (serde+schemars only)
microvms-core/   NEW  control-plane client, in-VM client, traps, cost, sizing
microvms-py/     NEW  PyO3 bindings (build-only, no publish)
microvms-js/     NEW  napi-rs bindings (build-only, no publish)
microvms-cli/    NEW  the `microvm` binary
agentd/          EXISTS  depends on protocol after extraction; schema.json byte-identical
model/           EXISTS  gains client-side lifecycle model (or sibling module)
```

Workspace `members` grows to seven. One dependency direction:
cli → core → protocol; bindings → core; agentd → protocol; model stays standalone
(mirrors become compile-checked where protocol exports Phase).

## Decisions the spec left open — one answer each

**Transport: hand-rolled SigV4, not smithy codegen.** aws-config 1.10.1 +
aws-credential-types 1.3.0 + aws-sigv4 1.5.1 + reqwest 0.13.4 + http 1.5.0.
Codegen needs Gradle/JDK/Kotlin and takes Smithy models, not botocore JSON;
maintainers point hand-rollers at exactly this stack (research-sigv4.yaml).
Retry via backon 1.6.0 with `.when()` on 429/5xx/Throttling; the proxy-token
mint sits inside the retry path (TRAP-9). The botocore service-2.json (gzipped,
pinned 2025-09-09) is the wire contract; serde request/response types are
hand-written against it and drift-checked by TRAP-12's gate.

**Protocol extraction keeps `docs/schema.json` byte-identical.** Response types
gain pub fields + Deserialize; requests gain Serialize; `ErrorBody.error` and
`Health.version` become `Cow<'static, str>`. The schema_artifact test
(definition_collisions == []) polices exactly this — it runs before anything else
lands on top. schemars 1.2.2, preserve_order stays OFF.

**Errors: one `Error` type in core with two granularities.** `kind()` maps onto
the exit-code catalog (14 `Exit` rows counting OK; 13 `ERR_*` codes) mirroring
cli.py:149. A second accessor, `wire_kind()`, preserves the fine-grained HTTP
taxonomy the Python exception CLASSES carry (Conflict/NotFound/Unauthorized/
ProtocolError/StdinClosed/TooLarge/RequestTimeout/NotBootstrapped) — the Python
CLI collapses all non-retryable HttpErrors into ERR_PROTOCOL, so exit codes
alone CANNOT drive conformance `raises()` assertions (critic objection 1). The
CLI's failure envelope carries `data.kind` = wire_kind so run_rs.py asserts at
the same granularity the oracle does. thiserror in core (the daemon's
no-thiserror rule is a daemon convention; the client is a library whose errors
are API). CLI maps kind → exit code in one table that also feeds `manifest`.

**Cost types are the S1 upgrades.**
- `DurationP` enum { Measured(Duration), Projected(Duration) } — no unlabeled
  constructor exists (COST-1/COST-10).
- `EstimatedUsd(Decimal)` — no `Into<f64>`, `From<f64>` only at the rate-table
  boundary, Display renders "≈ $X (estimate)" (COST-2/COST-6, rust_decimal 1.42.1,
  serde-with-str).
- `Amount` enum { Estimated(EstimatedUsd), Unpriced { reason } } (COST-3); summing
  any Unpriced yields `Total::AtLeast { floor, unpriced: Vec<...> }` (COST-4).
- Rate table pins region + retrieval date; staleness window 90 days (matches
  Python cost.py); ARM-only catalog, missing ARM line is an error not an x86
  substitute (COST-7/COST-9). Seconds-per-month constant is 730h. One-week
  snapshot retention floor (COST-8). Baseline-not-peak (COST-5, TRAP-13 table).

**Region is a closed enum** { UsEast1, UsEast2, UsWest2, EuWest1, ApNortheast1 }
with `Region::unsupported(&str) -> UnsupportedRegion` as the escape hatch — an
explicit constructor that names the null-message AccessDeniedException trap in its
docs and error, usable only via a separately-named client constructor (TRAP-6, S1
with documented S3 hatch).

**Hook timeouts are two types** — `RunHookTimeout` (≤60s) and `BuildHookTimeout`
(≤3600s), both validated-constructor newtypes (S2 at construction, S1 at use
sites because the two cannot be swapped).

**clientToken: no caller-supplied override on create paths** (TRAP-1, S1). Token =
label (truncated to fit) + full 8-hex nonce; 128-char ceiling checked against the
worst legal scope in the drift gate; 200 draws must stay 200 distinct (proptest).

**Identity repair is `repair_guest_identity: bool`** → emits `["ALL"]` (TRAP-3, S1).
**Connectors are enumerated intents** → region-interpolated ARNs (TRAP-4, S1).
**Payload ceiling 4096 inclusive, checked locally** before any wire call (TRAP-5).
**Memory takes a `SizeClass` enum** (five rows, baseline+peak from the table —
TRAP-10/TRAP-13, S1).

**Lifecycle is a typestate-informed state machine matching the symspec state
model** (vm_state 6-enum, token_installed, image_exists, was_terminated,
bootstrap_count 0..3; STATE-1..12). Client-side stateright model extends model/
with resume-after-terminate rejected without a wire call and suspend/resume
session-invariant preservation. STATE-12: resume checks the elapsed suspended
window locally and rejects naming the window.

**In-VM client reuses the daemon's turmoil discipline.** SimListener pattern from
agentd/tests/turmoil_transport.rs:121; byte-offset cursor reconnect per the prior
lesson (solutions/architecture-patterns/byte-offset-cursor...); two-clocks rule
respected (never pace a child with sleep under turmoil).

**CLI: clap 4.6 derive + std ExitCode; ratatui 0.30 only when
`stdout().is_terminal()` and not `--json`** (CLI-1..6). Same 11 subcommands, same
14-code catalog, manifest generated from the same tables the parser uses (AC-5-3).
Ctrl-C teardown emits undeleted identifiers (CLI-6/AC-5-6).

**Bindings: PyO3 0.29.2 (abi3-py39) + maturin 1.14.1; napi 3.12.0 + @napi-rs/cli
3.8.2.** Sync-over-async via one static tokio Runtime + `py.allow_threads(block_on)`.
Wrapper classes hold core newtypes and expose only smart constructors — no
coercions, so BIND-2/BIND-5 hold by construction. Build-only; nothing published.

**Verification wiring.**
- `mise` gains `spec:core` task running the v5 CLI
  (`node ~/workplace/symspec/packages/symspec/dist/cli.mjs check
  spec/core.symspec.json --reachability-timeout-ms 5000`); stays out of `check`'s
  depends (fresh-clone argument stands — global node path).
- `check-model-drift` gains a Rust source: `microvm constants --emit-json`
  (offline), compared against the same model plus a python-vs-rust cross-check
  (TRAP-12).
- `conformance/run_rs.py` is a HYBRID driver (critic objection 1). The client
  under test is the Rust CLI via `--json` envelopes, asserting failure kinds on
  the envelope's `data.kind` field (conflict vs not_found vs protocol — exit
  codes are too coarse). Six checks that deliberately bypass the client library
  (the raw run-hook POST at run.py:157 feeding the 409-hijack/200-replay
  checks, and four raw status-code sends at run.py:199,207,263,305) STAY on the
  Python httpx transport inside run_rs.py — they test the daemon, not the
  client under test, and adding a raw-request affordance to the CLI would
  violate CLI-2/CLI-5. run_rs.py states this split in its module docstring.
  New `live:conformance-rs` task joins the `live` aggregate. Teardown ordering
  preserved: log groups LAST. `scripts/verify-clean` prefix list is a
  correctness condition if new prefixes appear (none planned — `microvm-`
  covered).
- Guard-proof rule per REQUIREMENT, not just per trap (critic objection 3):
  break the invariant deliberately, watch the specific test fail, restore.
  Recorded per task packet.
- ARCH/BIND structural guards get named homes (critic objection 3):
  - ARCH-2 belongs to T-1 (the protocol crate IS the common dependency).
  - ARCH-3/ARCH-4/ARCH-5/BIND-1 get a dependency-direction guard test in T-7:
    a test parses `cargo metadata` and asserts the exact edge set (cli→core,
    bindings→core only, core↛cli, nothing binding-shaped exported from cli).
  - CLI-2's dual guard (from AC-5-4) lives in T-7: (1) static — no aws/http
    crate in microvms-cli's dependency closure and no control-plane operation
    strings in its source; (2) behavioral — every networked command run against
    a fail-closed core seam must fail through the classified path. Neither
    guard alone suffices (re-export defeats 1; call-core-then-also-reqwest
    defeats 2).

## What this plan does not do

- Does not delete `clients/python` — it is the conformance oracle.
- Does not restructure agentd beyond the protocol extraction.
- Does not publish anything anywhere; no new remotes.
- Does not reopen the five-crate architecture.

## Waves

**W1 — protocol extraction (T-1, solo).** The riskiest structural change runs
alone against the strongest existing gate (schema byte-compare + full test suite).

**W2 — T-2 first, then three parallel lanes (critic objection 2).**
- T-2 foundation runs SOLO: error taxonomy (kind + wire_kind), Region,
  SizeClass, hook-timeout newtypes, constants module (TRAP-6/10/13, TRAP-12
  constants). T-2 also lands the COMPLETE microvms-core skeleton — lib.rs with
  every module stub (`cost`, `control`, `session`, `sandbox`) and a Cargo.toml
  carrying every planned dependency (rust_decimal, aws-config/sigv4/
  credential-types, reqwest, backon, tokio, base64, serde; dev: proptest,
  turmoil, hyper) — so the parallel lanes only ADD files, never edit lib.rs or
  Cargo.toml. The root workspace `members` list is finalized in W1 (T-1 adds
  all five new crates at once, with stub crates for the four not yet built).
- Then in parallel: T-3 cost engine (COST-1..10); T-4 control-plane client
  (TRAP-1/2/3/4/5/8/11); T-5 in-VM client over protocol (TRAP-7/9, turmoil).

**W3 — lifecycle + surfaces (T-6..T-9).**
- T-6 state machine + stateright client model (STATE-1..12; depends T-4/T-5).
- T-7 CLI (depends T-3/T-4/T-5/T-6).
- T-8 PyO3 + napi bindings (depends on core surface freeze after T-6).
- T-9 verification wiring: drift extension, run_rs.py, mise tasks (depends T-7).

**W4 — guard-proof sweep + validate (T-10), then live conformance.**
`mise run check` green is the definition of done for the local tier;
`mise run live` + `live:conformance-rs` + `verify-clean` close the loop.
Commit checkpoints per wave, hooks green, no --no-verify.

## Cut line (critic objection 4)

The minimum shippable increment is W1 + W2 with `mise run check` green and
guard proofs recorded: a protocol crate the daemon consumes, a microvms-core
with the foundation, cost engine, and both clients, all committed. Each wave
ends in a commit, so a session ending early leaves a coherent workspace, never
a half-wired one. Deferrable in order: live conformance (needs a human-timed
billable window anyway), bindings, run_rs.py, CLI, state machine. If W3 starts
but cannot finish, the unfinished crate stays a compiling stub with its packet
noting exactly what remains.
