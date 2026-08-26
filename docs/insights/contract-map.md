# microvms-agentd · Contract map

When module A passes something to module B, what is B really expecting?

## What counts as a contract here

Three tiers, all of which cross a boundary a single `cargo build` cannot fully police:

1. **A Rust type or constant declared in one workspace crate and named by at least one
   other.** The dependency edges are `cli -> core -> protocol`, `bindings -> core`,
   `agentd -> protocol`, asserted as *equalities* over the metadata by
   `microvms-cli/tests/dependency_direction.rs:68-125` — a violation is a test failure, not a
   convention.
2. **A shape that crosses a language boundary**, where no compiler checks either side: the
   HTTP/SSE wire format, the `--json` envelope the Python conformance suite parses, the
   generated Python stub, the generated Node declarations.
3. **A contract stated as a machine-checked assertion rather than as a type**: the generated
   `docs/schema.json`, the 51 EARS requirements in `spec/core.symspec.json`, and the
   cross-language agreement tests such as
   `every_rate_byte_matches_the_python_literal` (`microvms-core/src/cost.rs:2179-2196`).

Ranking is by distinct consumer *file* count, measured with
`rg -l` over qualified paths and `rg -o … | uniq -c` over occurrences. `agentd` counts as one
consumer of every `protocol` type, because `agentd/src/exec.rs:87-90` re-exports the whole
`protocol::exec` surface in one block. Every client-side consumer reaches the same types
through `microvms_core::protocol::…`, because `microvms-core/src/lib.rs:77` re-exports the
crate — which is what lets `microvms-cli` name wire types while its allowlisted dependency
set (`microvms-cli/tests/thinness.rs:66`) contains six entries and none of them is
`protocol`.

### Gate coverage, which is not symmetric

Three surfaces in this repo are generated from Rust and consumed by something that cannot
read Rust. Two have a regenerate-then-diff gate; one does not.

| Surface | Producer | Gate |
| --- | --- | --- |
| `docs/schema.json` | `agentd/src/bin/schema.rs` | `mise.toml:168-173`, `cargo run -p agentd --bin schema -- --check` |
| `microvms-py/microvms.pyi` + `microvms-py/py.typed` | `scripts/generate-py-stubs.py` | `mise.toml:179-195`, `mise run stubs:check` |
| `microvms-js/index.d.ts` | `napi build --platform` (`microvms-js/package.json:12`) | **none** |

`microvms-js/index.d.ts` is gitignored at `.gitignore:29` alongside `index.js` and `*.node`,
with the rationale at `.gitignore:23-26` (a platform-specific binary must not be committed).
The declarations file is not a binary and shares the exclusion anyway, so nothing in
`mise tasks` regenerates and diffs it. `mise.toml:181-189` spells out why the Python side is
gated — `microvms-py/py.typed` promises a type checker the package is typed, so a stale stub "degrades to
confidently wrong" rather than to unchecked. The Node package makes the same promise via
`"types": "index.d.ts"` (`microvms-js/package.json:7`) with no equivalent check. There is no
`--check` mode in either toolchain: `git diff --exit-code` after regeneration is the whole
state of the art.

---

## microvms_core::ErrorKind — the coarse failure taxonomy

**Producer:** `microvms-core/src/error.rs:126-159` (enum), `:166-180` (`ALL`), `:187-203` (`code`)

**Consumer(s):**

- `microvms-cli/src/exit.rs:140-156` — `Exit::for_kind`, a total match that turns a
  fourteenth kind into a compile error rather than a fall-through to `ERR_UNEXPECTED`.
- `microvms-cli/src/exit.rs:486-503` — asserts the CLI's `EXIT_TABLE` and `ErrorKind::ALL`
  are the same thirteen classes with byte-identical code strings.
- `microvms-py/src/errors.rs:129-145` — `exception_for`, one Python exception type per kind.
- `microvms-js/src/errors.rs:143-149` — `error_codes()`, enumerated from `ErrorKind::ALL`
  rather than transcribed.
- Raise sites across core: `microvms-core/src/sandbox.rs:68`,
  `microvms-core/src/hooks.rs:156`, `microvms-core/src/sizing.rs:266`,
  `microvms-core/src/cost.rs:2124`, plus `control/{artifact,image,microvm,mod,transport}.rs`
  and `session/{exec,http,mod,proxy,sse}.rs`.
- The daemon's own uses: `agentd/src/disk.rs:69`, `agentd/src/exec.rs:776`,
  `agentd/src/fs.rs:1107`, `agentd/src/identity.rs:411`.

Count: 36 non-declaring files, from
`rg -l '\bErrorKind\b' --type rust --glob '!microvms-core/src/error.rs'`.

**Shape:**

```rust
pub enum ErrorKind {
    /// No handler claimed this — a bug in this crate, not the platform.
    Unexpected,
    /// Refused locally, before any AWS call. Every trap closure lands here.
    InvalidArg,
    /// Transient. Run the identical request again.
    Retryable,
    /// An identity is wrong or absent; waiting will not fix it.
    Credentials,
    /// The daemon rejected the request on its merits.
    Protocol,
    /// The image build was never scheduled — the `clientToken` replay signature.
    BuildWedged,
    /// The MicroVM reached a terminal state before RUNNING; read `stateReason`.
    LaunchDied,
    /// The launch-time suspended window passed, so there is nothing to resume.
    WindowClosed,
    /// A control-plane failure with no more specific class.
    Platform,
    /// A client-side deadline elapsed. The VM and the exec are untouched.
    Timeout,
    /// Interrupted after launch; teardown ran and any leak is named in the payload.
    Interrupted,
    /// A prerequisite is missing.
    Precondition,
    /// The sandbox worked and the command in it exited non-zero.
    ///
    /// Its own class because it is the one failure that means nothing is wrong with
    /// the platform, the credentials, or this client — a CI caller needs to tell
    /// "your tests failed" from "we never got a VM", and one shared class cannot say
    /// both.
    ExecFailed,
}
```

**Assumptions consumers make:**

- **The mapping to exit integers is injective, and consumers rely on that.**
  `microvms-cli/src/exit.rs:512-525` asserts no two kinds collapse onto one exit row, and
  names the plausible edit it exists to catch (routing `Precondition` to `InvalidArg`).
- **The `ERR_*` string is the branch key, not the integer.** `microvms-core/src/error.rs:182-186`
  states it: a shell reads `$?`, an agent parsing `--json` reads `code` and should never keep
  an integer table.
- **`ALL` is in exit-code order, and two independent hand-written tables depend on that
  order.** `microvms-core/src/error.rs:434-452` and `microvms-cli/src/exit.rs:406-433` both
  spell the thirteen codes out as literals, deliberately: "a generated list would agree with
  a typo" (`microvms-core/src/error.rs:430-431`).
- **Retryability is derived, never stored.** `microvms-core/src/error.rs:116-118` reads the
  kind; `microvms-core/src/error.rs:399-417` keeps a second, test-only table so the two can
  be compared rather than trusted.
- **The Python exception hierarchy is one-to-one with the kinds and rooted at one base**, so
  `except MicrovmError` catches everything (`microvms-py/src/errors.rs:4-9`).
- **Node callers cannot read `.code`.** `microvms-js/src/errors.rs:16-38` records the measured
  collapse: `code="ERR_INVALID_ARG"` on a sync export, `code="GenericFailure"` through a
  Promise rejection. The contract is `err.cause.message`, which is exactly the `ERR_*` string
  on every path (`microvms-js/src/errors.rs:70-80`). Restates
  `.erpaval/solutions/api-patterns/napi-async-collapses-error-codes.md`.

**Drift risk:** adding a fourteenth kind is forced into three exhaustive matches
(`microvms-core/src/error.rs:188`, `microvms-cli/src/exit.rs:141`,
`microvms-py/src/errors.rs:130`) but **not** into `ALL`, so a variant added without an `ALL`
entry compiles and silently vanishes from `error_codes()`
(`microvms-js/src/errors.rs:143-149`) and from every consumer that enumerates the catalog.
The cross-check at `microvms-cli/src/exit.rs:486-492` catches it only when an `EXIT_TABLE` row
is added in the same change. Mitigation: assert `ErrorKind::ALL.len()` against a literal
alongside the thirteen spelled codes at `microvms-core/src/error.rs:434-452`.

## microvms_core::Region — the closed region set, S1 closure

**Producer:** `microvms-core/src/region.rs:44-63` (enum), `:73` (`MICROVM_REGIONS`), `:107` (`unlisted`), `:137-146` (`FromStr`)

**Consumer(s):**

- `microvms-cli/src/cli.rs:33` — the `--region` value, parsed into the enum at the CLI edge.
- `microvms-cli/src/commands/doctor.rs:17`, `microvms-cli/src/guards.rs:29`, `microvms-cli/src/seam.rs:29`
- `microvms-py/src/region.rs:11`, `microvms-py/src/sandbox.rs:309`
- `microvms-js/src/region.rs:8`, `microvms-js/src/sandbox.rs:59`, `microvms-js/src/lib.rs:46`
- `microvms-core/src/lib.rs:81` (re-export), `microvms-core/src/cost.rs:850`,
  `microvms-core/src/sandbox.rs:465`, plus `control/{artifact,connector,image,microvm,mod,transport}.rs`
- `microvms-core/tests/live_pagination.rs:59`, `microvms-core/tests/live_versions.rs:37`

Count: 20 non-declaring files, from
`rg -l '\bRegion\b' --type rust --glob '!microvms-core/src/region.rs'`.

**Shape:**

```rust
pub enum Region {
    UsEast1,
    UsEast2,
    UsWest2,
    EuWest1,
    ApNortheast1,
    /// A region this client has not seen carry MicroVMs.
    ///
    /// **You are opting into the null-message trap.** If this region does not run
    /// MicroVMs, the first control-plane call answers `AccessDeniedException` with a
    /// null message and you will spend the next hour reading an IAM policy that is
    /// correct. Constructible only through [`Region::unlisted`], which says so at
    /// the call site.
    ///
    /// It exists because AWS adds regions faster than this list is re-read, and a
    /// client that refuses a region AWS has just launched in is its own kind of
    /// wrong. The override costs exactly the diagnostic above.
    Unlisted(String),
}
```

**Assumptions consumers make:**

- **`Unlisted` is a visible variant, not a hidden flag**, so a `match` over regions cannot
  forget the case exists and a reader of a call site can see that someone opted into the trap
  (`microvms-core/src/region.rs:40-43`).
- **`Region` is not `Copy`.** It carries a `String` in `Unlisted`, so it derives
  `Clone, Debug, Eq, Hash, PartialEq` only (`microvms-core/src/region.rs:44`). Consumers that
  hold a region across an `async` boundary clone it; `RateTable::region()` returns `&Region`
  for the same reason (`microvms-core/src/cost.rs:868-870`).
- **The region label is priced, not cosmetic.** `microvms-core/src/cost.rs:864-867` measures
  the consequence: a Tokyo caller reading the us-east-1 table understates snapshot write by
  22.6%, and staleness checking would never surface it.
- **The five-region list is measurement-backed and cannot be model-checked.**
  `scripts/check-model-drift.py:52-58` states that no service model names the regions and
  that the two botocore calls that look like substitutes disagree with each other, so
  `PINNED_REGIONS` in that script is a deliberate second reader rather than a self-comparison.

**Drift risk:** AWS launching a sixth MicroVM region leaves every caller on the `Unlisted`
path, which works but discards the null-message diagnostic that is the whole reason the enum
exists. Mitigation: the region list is the one constant with no model to check it against, so
re-read it whenever `docs/PLATFORM.md` gains a dated region finding and update both
`microvms-core/src/region.rs:73` and `scripts/check-model-drift.py`'s `PINNED_REGIONS` in the
same commit.

## microvms_core::session::Session — the in-VM control API handle

**Producer:** `microvms-core/src/session/mod.rs:183-188` (struct), `:190-224` (constructors), `:466-474` (`SessionBuilder`)

**Consumer(s):**

- `microvms-py/src/session.rs:8`, `microvms-py/src/exec.rs:482`, `microvms-py/src/runtime.rs:12`, `microvms-py/src/sandbox.rs:632`
- `microvms-js/src/session.rs:7`, `microvms-js/src/exec.rs:319`, `microvms-js/src/process.rs:190`, `microvms-js/src/sandbox.rs:28`
- `microvms-cli/src/seam.rs:18`, `microvms-cli/src/guards.rs:28`, `microvms-cli/src/commands/attached.rs:39`, `microvms-cli/tests/thinness.rs:242`
- `microvms-core/src/sandbox.rs:535` (`session()`), `:648` and `:837` — `run` and `resume`
  hand back `&mut Session`; plus `microvms-core/src/control/ops.rs` and
  `microvms-core/src/session/{http,proxy}.rs`
- `microvms-core/tests/turmoil_client.rs:66`

Count: 17 non-declaring files, from
`rg -l '\bSession\b' --type rust --glob '!microvms-core/src/session/mod.rs'`.

**Shape:**

```rust
/// The control API of one running MicroVM.
pub struct Session {
    transport: Arc<Transport>,
    endpoint: String,
    port: u16,
}
```

**Assumptions consumers make:**

- **Constructing a session does not probe the VM.** `microvms-core/src/session/mod.rs:203-206`
  makes this explicit: "do I have a session" and "is the VM up" are different questions with
  different answers during a launch, so a probing constructor would conflate them.
- **`agent_token()` is readable but never printed.** `microvms-core/src/session/mod.rs:191-200`
  makes it public because a reattaching caller needs it; the hand-written `Debug` at
  `:454-463` drops it. Restates
  `.erpaval/solutions/best-practices/credential-structs-never-derive-debug.md`.
- **`Session::direct` is a supported shape, not a test escape hatch**
  (`microvms-core/src/session/mod.rs:217-222`) — the conformance path and every local-binary
  test go through it, so proxy-auth headers being absent is a valid state rather than a bug.
- **Token minting happens inside the request path**, which is what makes a long run survive
  the 60-minute proxy-token lifetime; `SessionBuilder::with_minter` /
  `with_proxy_auth` decide the schedule and the latter wins
  (`microvms-core/src/session/mod.rs:476-490`).
- **The HTTP backend is a replaceable seam** (`microvms-core/src/session/mod.rs:492-497`),
  which is what lets `microvms-core/tests/turmoil_client.rs` drive the real client under
  simulated network faults. Restates
  `.erpaval/solutions/api-patterns/axum-listener-trait-enables-turmoil.md`.

**Drift risk:** the port a session was built with is also the port its proxy token is scoped
to (`microvms-core/src/session/mod.rs:499-504`), so a consumer that changes the agent port
without rebuilding the session gets a token scoped to the old port and a 401-shaped failure
that reads as a credential problem. Mitigation: keep `with_port` the only way to set it, so
the scope and the header are assigned from one value.

## microvms_core::WireKind — the fine taxonomy, where 400 and 404 stay different

**Producer:** `microvms-core/src/error.rs:218-268` (enum), `:272-286` (`ALL`), `:292-308` (`as_str`), `:315-331` (`status`), `:343-356` (`from_status`), `:366-397` (`error_kind`)

**Consumer(s):**

- `microvms-cli/src/envelope.rs:321-339` — writes `data.kind` from it.
- `microvms-cli/src/exit.rs:336-365` — keys the remedy suggestion on it where two conditions
  share an exit code.
- `microvms-cli/src/exit.rs:534-560` — pins which five collapse onto `ERR_PROTOCOL`.
- `microvms-cli/src/guards.rs:2470`
- `microvms-py/src/errors.rs:161-164` — sets `.wire_kind` on the raised exception.
- `microvms-js/src/errors.rs:70-80`, `:152-158` — the cause's cause, and `wire_kinds()`.
- `microvms-core/src/lib.rs:79` (re-export), plus `control/{microvm,transport}.rs` and
  `session/{exec,files,http,mod,proxy,sse}.rs`
- `microvms-core/tests/turmoil_client.rs:63`

Count: 15 non-declaring files, from
`rg -l '\bWireKind\b' --type rust --glob '!microvms-core/src/error.rs'`.

**Shape:**

The thirteen variants are listed at `microvms-core/src/error.rs:219-268`. The load-bearing
member is the status table, because it is where a consumer's 400-versus-404 distinction is
either preserved or lost:

```rust
    pub fn from_status(status: u16) -> Option<WireKind> {
        match status {
            400 => Some(WireKind::ProtocolError),
            401 => Some(WireKind::Unauthorized),
            404 => Some(WireKind::NotFound),
            408 => Some(WireKind::RequestTimeout),
            409 => Some(WireKind::Conflict),
            410 => Some(WireKind::StdinClosed),
            413 => Some(WireKind::TooLarge),
            503 => Some(WireKind::NotBootstrapped),
            s if s >= 500 => Some(WireKind::ServerError),
            _ => None,
        }
    }
```

**Assumptions consumers make:**

- **There is deliberately no generic 4xx fallback.** `microvms-core/src/error.rs:336-342`
  names the defect a fallback would reintroduce — a 4xx mapped to `NotFound` made a protocol
  typo look like a missing file, and it hid for a full review round. Asserted at
  `microvms-core/src/error.rs:519-529`: 402, 403, 405, 418, 429, 451 must all resolve to
  `None`.
- **5xx *does* fall back, and 503 is the one exception.** `microvms-core/src/error.rs:535-540`
  — "come back in a moment" is not "the daemon broke".
- **`status()` and `from_status()` are inverses wherever both are defined**, asserted at
  `microvms-core/src/error.rs:546-556`, and four variants deliberately have no status
  (`Transport`, `AuthTokenMint`, `ExecTimeout`, `OutputGap` — `:326-329`).
- **The `as_str` strings are Python exception class names, not a re-spelling.**
  `microvms-core/src/error.rs:288-291` — the conformance oracle compares against them, and
  `conformance/run_rs.py:187-196` reads them out of `data.kind`.
- **`None` is information.** `conformance/run_rs.py:189-193` states that an absent
  `data.kind` means the client refused before any call. The CLI preserves that by inserting
  the key only when a wire kind exists (`microvms-cli/src/envelope.rs:326-328`), and
  `microvms-core/src/error.rs:562-567` asserts a local reject carries none.
- **Exactly five variants are retryable**, named as literals so a sixth added by mistake
  fails a test rather than a retry loop (`microvms-core/src/error.rs:485-501`).

**Drift risk:** a new variant added to `WireKind` reaches `error_kind()` as a compile error
(the match is closed) but reaches `from_status` silently — a status the daemon starts using
with no row in that table maps to `None` and surfaces as something other than the daemon's
own decision. Mitigation: `status()`/`from_status()` inverse test at
`microvms-core/src/error.rs:546-556` catches it only if the new variant declares a status, so
declare one whenever the daemon does.

## microvms_core::sandbox::Sandbox — the product surface and its state machine

**Producer:** `microvms-core/src/sandbox.rs:422-442` (struct), `:97-131` (`Lifecycle`), `:551-935` (the transitions)

**Consumer(s):**

- `microvms-cli/src/commands/lifecycle.rs:6`, `microvms-cli/src/guards.rs:27`, `microvms-cli/src/ledger.rs:94`, `microvms-cli/src/seam.rs:9`, `microvms-cli/tests/thinness.rs:240`
- `microvms-py/src/sandbox.rs:6`, `microvms-py/src/session.rs:27`, `microvms-py/src/runtime.rs:12`
- `microvms-js/src/sandbox.rs:6`, `microvms-js/src/session.rs:7`, `microvms-js/src/region.rs:11`

Count: 14 non-declaring files, from
`rg -l '\bSandbox\b' --type rust --glob '!microvms-core/src/sandbox.rs'`.

**Shape:**

```rust
/// The symspec's `vm_state`, verbatim.
///
/// Six states and no others, which is the S1 half of this module: a lifecycle held as a
/// `String` would let `"RUNNING "` and `"Running"` both exist, and every guard below would
/// have to decide which it meant.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Lifecycle {
    /// The initial state, and the state a launch is accepted into (STATE-1).
    Pending,
    /// The run hook answered with a success status and the token is installed (STATE-2).
    Running,
    /// A suspend was accepted and the platform has not yet reported it complete (STATE-4).
    Suspending,
    /// The platform reported suspension complete (STATE-6).
    Suspended,
    /// A terminate was accepted (STATE-9).
    Terminating,
    /// The platform reported termination complete (STATE-10).
    Terminated,
}
```

```rust
pub struct Sandbox {
    control: Arc<ControlPlane>,
    image: Option<Image>,
    microvm: Option<Microvm>,
    session: Option<Session>,

    // ── the symspec's five variables ─────────────────────────────────────────
    lifecycle: Lifecycle,
    token_installed: bool,
    image_exists: bool,
    was_terminated: bool,
    bootstrap_count: u32,
```

**Assumptions consumers make:**

- **Every field is private; the contract is the accessor set.** `lifecycle()`,
  `token_installed()`, `image_exists()`, `was_terminated()`, `bootstrap_count()` at
  `microvms-core/src/sandbox.rs:500-520` are named after the symspec's five state variables,
  so a consumer asserting against the formal model reads them rather than reconstructing
  state.
- **`Suspended` is still billing.** `Lifecycle::is_live` at
  `microvms-core/src/sandbox.rs:126-131` includes `Pending | Running | Suspending | Suspended`,
  which is what a `Drop` warning is for. That is a *different* question from
  `constants::TERMINAL_STATES` (`microvms-core/src/constants.rs:448`), which lists
  `SUSPENDED`/`SUSPENDING` as states a launch wait must stop on.
- **The suspended window is local knowledge, not readable from the platform.**
  `microvms-core/src/sandbox.rs:435-436` — `GetMicrovm` does not return it, so a consumer
  that reconstructs a `Sandbox` from a `GetMicrovm` response cannot answer "is the resume
  window still open".
- **`terminate` returns a report and never raises.** `microvms-core/src/sandbox.rs:332-334`,
  `:935` — it runs where a `finally` would, so a consumer must inspect
  `TeardownReport::leaked()` (`:364-366`) rather than trusting the absence of an error.
- **`undeleted` carries identifiers, not a boolean**, because "a leak nobody can name is a
  leak nobody can clean up" (`microvms-core/src/sandbox.rs:336-348`), and the build log group
  lands there unconditionally: this crate cannot delete it.
- **`image_deleted: Option<bool>`** distinguishes "deletion was not asked for" from
  "deletion failed" (`microvms-core/src/sandbox.rs:355`). Restates
  `.erpaval/solutions/architecture-patterns/an-absent-value-is-not-a-neutral-one.md`.
- **`Debug` omits the agent token** (`microvms-core/src/sandbox.rs:444-448`), so a consumer
  logging a sandbox does not leak the credential.

**Drift risk:** the six `Lifecycle` variants and the six `MICROVM_STATES` wire strings are two
readers of one AWS fact, and `microvms-core/src/constants.rs:341-347` states plainly that a
wire string cannot be exhaustively matched. A state AWS respells fails the model gate
(`scripts/check-model-drift.py`) and a subset test, but does not fail to compile.
Mitigation: keep `scripts/check-model-drift.py` in `mise run check` — it needs no network and
no credentials because the model is a file inside botocore.

## protocol::exec::StreamKind — one offset space, two channels

**Producer:** `protocol/src/exec.rs:76-99`

**Consumer(s):**

- `agentd/src/exec.rs:87-90` (re-exported into the daemon's own namespace)
- `microvms-core/src/session/sse.rs:243` — the `ExecEvent::Output` payload field.
- `microvms-cli/src/commands/attached.rs:354-355` — the two-arm map to `"stdout"` / `"stderr"`.
- `microvms-py/src/session.rs:600-603` — published as `streamKinds` from `StreamKind::ALL`.
- `microvms-js/src/session.rs:582-586` — the same list, built as a JSON array.
- `microvms-js/src/process.rs:172-174` — `is_stderr`.
- `microvms-core/tests/turmoil_client.rs:850`

**Shape:**

```rust
/// Which pipe a streamed chunk came from. Both share one offset space, so a
/// client holds one cursor rather than two that can disagree about ordering.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StreamKind {
    Stdout,
    Stderr,
}

impl StreamKind {
    /// Both kinds, in the order the daemon documents them.
    ///
    /// Same reason as [`Phase::ALL`]: the bindings publish this closed set, and a
    /// list they spell themselves is a list the enum can outgrow.
    pub const ALL: [StreamKind; 2] = [StreamKind::Stdout, StreamKind::Stderr];

    /// The wire spelling — the exact string serde writes under `rename_all` above.
    pub const fn as_str(self) -> &'static str {
        match self {
            StreamKind::Stdout => "stdout",
            StreamKind::Stderr => "stderr",
        }
    }
}
```

**Assumptions consumers make:**

- **One shared byte-offset space across both pipes.** `protocol/src/exec.rs:76-78` states it,
  and every reconnect depends on it: a client resumes with a single `?offset=N`
  (`protocol/src/exec.rs:171-178`). Restates
  `.erpaval/solutions/architecture-patterns/byte-offset-cursor-is-what-makes-reconnect-work.md`.
- **A gap is attributed to the stream a *later* frame named**, not to one the gap frame
  carried — `microvms-js/src/process.rs:159-169` names this as the field a reader would guess
  wrong. So a consumer demultiplexing into two channels cannot attribute a gap without
  look-ahead.
- **`is_stderr` is a boolean predicate, not an exhaustive match.**
  `microvms-js/src/process.rs:172-174` is `matches!(stream, StreamKind::Stderr)`. A third
  variant would be classified as stdout with no compile error. Every other consumer matches
  exhaustively.
- **`as_str` and serde's `rename_all` must agree**, asserted for every variant at
  `protocol/src/exec.rs:318-339`, with `ALL` held complete by wildcard-free matches rather
  than by a length check (`protocol/src/exec.rs:313-316`).

**Drift risk:** a third channel added to `StreamKind` compiles against
`microvms-js/src/process.rs:172` and silently routes to stdout. Mitigation: rewrite
`is_stderr` as a total match returning the channel, so the Node binding fails to build.

## protocol::exec::Phase — the exec lifecycle, and its two redundant name tables

**Producer:** `protocol/src/exec.rs:22-54`

**Consumer(s):**

- `agentd/src/exec.rs:87-90` (re-export)
- `microvms-core/src/session/exec.rs:69` (`ExecResult::phase`), `:86-91` (`done()`), `:264-269`, `:689`
- `microvms-cli/src/commands/attached.rs:442-448` (`phase_name`)
- `microvms-py/src/session.rs:596-599` — published as `phases` from `Phase::ALL`
- `microvms-js/src/session.rs:577-581` — the same list
- `microvms-core/tests/turmoil_client.rs:954`

**Shape:**

```rust
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    /// Child spawned, still running (or its pipes still held by a grandchild).
    Running,
    /// Child exited and output is buffered and readable.
    Exited,
    /// Caller acked; output has been released and the entry awaits collection.
    Acked,
}

impl Phase {
    /// Every phase, in lifecycle order.
    ///
    /// Public because a client that publishes the closed set — both bindings do, in
    /// their `session_constants` — needs the list from the type rather than a spelled-out
    /// copy that goes stale the first time a phase is added. The round-trip test below
    /// holds `ALL` complete by exhaustive match.
    pub const ALL: [Phase; 3] = [Phase::Running, Phase::Exited, Phase::Acked];

    /// The wire spelling — the exact string serde writes under `rename_all` above.
    ///
    /// Here rather than in each client because two bindings each grew their own
    /// three-arm match over this enum; a variant renamed on the wire must change
    /// exactly one table, and the test below is what keeps this one equal to serde's.
    pub const fn as_str(self) -> &'static str {
        match self {
            Phase::Running => "running",
            Phase::Exited => "exited",
            Phase::Acked => "acked",
        }
    }
}
```

**Assumptions consumers make:**

- **`Exited` and `Acked` both mean finished.** `microvms-core/src/session/exec.rs:86-91`
  defines `done()` as `Exited | Acked`, so a consumer must not treat `Acked` as an error
  state.
- **`Acked` means the output is already gone.** `microvms-core/src/session/exec.rs:681-693`
  returns the *ack* response rather than a post-ack poll, because a poll after the ack reports
  `acked` with no output — named a "silent empty-output bug" at `:685`.
- **The daemon keeps a separate exit marker precisely because an ack takes the `Outcome`.**
  `agentd/src/exec.rs:92-99` — after an ack, `result` is `None` again and is no longer usable
  as "has this exec finished?", so a stream attaching then would wait on a channel that never
  carries another message.
- **`as_str` is meant to be the only phase-name table, and two consumers spell their own
  anyway.** The bindings comply (`microvms-py/src/session.rs:596-599`,
  `microvms-js/src/session.rs:577-581`), but `microvms-core/src/session/exec.rs:264-269` and
  `microvms-cli/src/commands/attached.rs:442-448` each hand-write all three strings. Those
  matches are exhaustive, so a new *variant* is a compile error — a renamed *wire spelling* is
  not, because `as_str` and serde would move together under the test at
  `protocol/src/exec.rs:318` while the two copies keep emitting the old string.
- **`ALL` is in lifecycle order**, and the CLI's `phase_name` deliberately avoids `Debug`
  because `Debug` emits `Running` where the wire carries `running`
  (`microvms-cli/src/commands/attached.rs:437-441`).

**Drift risk:** renaming a phase on the wire (a `#[serde(rename)]` on a variant) updates
`as_str` under compiler pressure from `protocol/src/exec.rs:318` but leaves the two
hand-spelled tables emitting the old string, so the CLI envelope and a core timeout message
would disagree with the daemon's own JSON. Mitigation: route both call sites through
`Phase::as_str` and delete the local tables.

## protocol::health::Health — the liveness answer, with two defaulted fields

**Producer:** `protocol/src/health.rs:10-89` (`Health`), `:92-102` (`DiskHealth`)

**Consumer(s):**

- `agentd/src/routes.rs:19` — `pub use protocol::health::{DiskHealth, Health};`
- `microvms-core/src/session/mod.rs:329` — `pub async fn health(&self) -> Result<protocol::health::Health, Error>`
- `microvms-py/src/session.rs:59-85` — `PyHealth::wrap`
- `microvms-js/src/session.rs:84-97` — `Health::wrap`
- `microvms-core/tests/turmoil_client.rs:876`

**Shape:**

Quoted in full, because the field doc comments *are* the contract here — every one of them
names a monitor behaviour a consumer would otherwise get wrong.

```rust
/// `GET /v1/health` response.
#[derive(Debug, Deserialize, JsonSchema, Serialize)]
pub struct Health {
    // The daemon's own version, distinct from `PROTOCOL_VERSION`. `Cow` so the
    // daemon reports its `CARGO_PKG_VERSION` borrowed while a client deserializes
    // into an owned string.
    //
    // Not a doc comment, deliberately: schemars publishes doc comments as
    // `description` and `docs/schema.json` is byte-compared, so adding one here is a
    // schema change. The field was undocumented before the extraction and stays so.
    pub version: Cow<'static, str>,
    pub bootstrapped: bool,
    /// Free space on the daemon's working filesystem, and the reserve it is judged
    /// against.
    ///
    /// Reported so disk pressure is something an orchestrator *watches* rather than
    /// something it discovers from a failed write. anthropics/claude-code#59856
    /// filled two 10 GB disks to 100% with never-collected session directories and
    /// the first symptom was `useradd: No space left on device` — by which point
    /// every writer in the sandbox was already broken. A number on a health endpoint
    /// is what makes that curve visible while there is still time to act.
    ///
    /// `None` when free space could not be measured, which is deliberately distinct
    /// from zero: unmeasurable is not full, and a monitor that conflated them would
    /// page on a missing `statvfs`.
    pub disk: Option<DiskHealth>,
    /// Whether any startup identity repair step failed. True means the VM is serving
    /// with a value from the shared image still in place — a duplicate machine-id or
    /// boot_id — which is a security-relevant condition an operator may want to
    /// drain the VM over, but is never a reason for the daemon to refuse to serve.
    pub identity_degraded: bool,
    /// False when identity repair was switched off by config. Distinguished from a
    /// repair that ran and found nothing so a monitor can tell "opted out" from
    /// "nothing to do".
    pub identity_repaired: bool,
```

The two defaulted fields, and the reason the asymmetry is deliberate
(`protocol/src/health.rs:44-88`):

```rust
    /// `#[serde(default)]`, unlike every field above it, and the asymmetry is not an
    /// oversight. The daemon is baked into an image while the client is installed
    /// separately, so a current client routinely talks to a daemon from whenever that
    /// image was built — and a required field would make `health()` fail outright
    /// against a daemon that predates it, turning a missing signal into an
    /// unreachable VM. False is also the right absence: a daemon that cannot say
    /// whether it is busy has not asserted that it is.
    #[serde(default)]
    pub busy: bool,
    /// How many execs are registered, in any phase.
    ///
    /// Alongside `busy` because the two answer different questions and a monitor
    /// wants both: `busy: false, execs: 0` is a fresh or drained VM, while
    /// `busy: false, execs: 7` is a VM holding seven unacked results that somebody
    /// still has to collect. Terminating the second loses output nobody read.
    ///
    /// Defaulted for the same reason as `busy`: a client routinely talks to a daemon
    /// baked into an older image, and zero is the honest reading of a daemon that
    /// does not report a count.
    #[serde(default)]
    pub execs: usize,
}
```

```rust
pub struct DiskHealth {
    /// Bytes available to an unprivileged writer, from `statvfs` `f_bavail`.
    pub available_bytes: u64,
    /// Bytes that must stay free before a write is refused. Zero means the guard is
    /// disabled.
    pub reserve_bytes: u64,
    /// Whether a write would be refused right now. Precomputed rather than left to
    /// the client, so every consumer applies the same comparison the write path does.
    pub under_pressure: bool,
}
```

**Assumptions consumers make:**

- **`busy: false` is not an assertion of idleness.** `busy` and `execs` are the only
  `#[serde(default)]` fields (`protocol/src/health.rs:75`, `:87`), and the reason at
  `:68-74` is that the daemon is baked into an image while the client installs separately —
  so `false`/`0` is also what a pre-feature daemon returns. A required field would turn a
  missing signal into an unreachable VM.
- **Polling from outside the VM is the keepalive, and the daemon must not self-keepalive.**
  `protocol/src/health.rs:46-61` — the platform measures idleness by inbound traffic through
  a proxy that terminates outside the guest, so in-guest traffic cannot reset the idle timer.
- **`disk: None` is not `disk: 0`.** Asserted at `protocol/src/health.rs:111-128`:
  unmeasurable is not full, and a monitor that conflated them would page on a missing
  `statvfs`. Both bindings preserve the distinction by flattening into three `Option`s
  (`microvms-py/src/session.rs:77-79`, `microvms-js/src/session.rs:89-91`).
- **`busy` and `execs` answer different questions.** `protocol/src/health.rs:78-86` —
  `busy: false, execs: 7` is a VM holding seven unacked results, and terminating it loses
  output nobody read. Asserted at `:134-151`.
- **`version` is deliberately undocumented.** `protocol/src/health.rs:12-18` is a `//`
  comment, not a doc comment, because schemars publishes doc comments as `description` and
  `docs/schema.json` is byte-compared — adding one is a schema change.
- **The Node binding narrows `execs: usize` to `i64`** (`microvms-js/src/session.rs:95`),
  which is what `#[napi]` can express; a count above `i64::MAX` is not reachable.

**Drift risk:** a new `Health` field without `#[serde(default)]` makes `health()` fail
outright against a daemon baked into an older image, which reads to a caller as an
unreachable VM rather than a version skew. Mitigation: default every field added after the
first release, and take `busy`'s doc comment (`protocol/src/health.rs:68-74`) as the rule.

## protocol::exec::StartRequest — the exec start body

**Producer:** `protocol/src/exec.rs:103-139`

**Consumer(s):**

- `agentd/src/exec.rs:87-90` (re-export; the daemon's extractor target)
- `microvms-core/src/session/mod.rs:380` — `pub async fn run(&self, req: protocol::exec::StartRequest)`
- `microvms-cli/src/commands/lifecycle.rs:859` — `pub fn start_request(spec: StartSpec<'_>) -> microvms_core::protocol::exec::StartRequest`
- `microvms-py/src/session.rs:338`, `:400` — two construction sites
- `microvms-js/src/session.rs:142` — `fn into_request(self, command: Either<String, Vec<String>>) -> protocol::exec::StartRequest`

**Shape:**

```rust
pub struct StartRequest {
    /// Caller-minted idempotency key. Harbor retries, and a retry must not
    /// produce a second child.
    pub exec_id: String,
    /// argv when `shell` is false, or the script when it is true.
    pub command: Vec<String>,
    #[serde(default)]
    pub shell: bool,
    /// Omitted means inherit the daemon's working directory. See the module docs.
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub env: HashMap<String, String>,
    /// Numeric uid to demote to. Optional; omitted means run as the daemon's own
    /// user.
    #[serde(default)]
    pub user: Option<u32>,
    #[serde(default)]
    pub group: Option<u32>,
    /// Wall-clock budget. Validated before the child spawns — the predecessor
    /// raised on a bad value inside the waiter thread, by which point the child
    /// was already running and became an orphan.
    #[serde(default)]
    pub timeout_sec: Option<f64>,
```

**Assumptions consumers make:**

- **`exec_id` is the idempotency key, and a retry must not spawn twice**
  (`protocol/src/exec.rs:105-107`). A consumer that mints a fresh id on retry loses that.
- **`stdin: false` is the default and matters.** `protocol/src/exec.rs:128-138` — a child
  holding an open stdin pipe nobody writes to blocks forever the first time it reads, and
  `/bin/sh`, `git`, and any tool that probes for input behave differently against a pipe than
  against `/dev/null`. Writing without it is a 409
  (`microvms-js/src/session.rs:111-112`).
- **`command` is never split on whitespace.** `microvms-py/src/session.rs:566-572` states the
  rule: splitting turns a path containing a space into two arguments nobody meant;
  `shell=True` is how a caller asks for a script.
- **`timeout_sec` is validated before the spawn**, not inside the waiter — the predecessor
  raised late and orphaned a running child (`protocol/src/exec.rs:123-126`).
- **Every defaulted field can be omitted**, asserted by round-tripping a body carrying only
  `exec_id` and `command` (`protocol/src/exec.rs:359-367`). Both bindings expose all of them
  as optional (`microvms-js/src/session.rs:101-113`).
- **An absent `env` and an empty `env` are the same thing** — a plain `HashMap` rather than
  an `Option`, matching the same decision on the run hook
  (`protocol/src/hook.rs:48-53`).

**Drift risk:** adding a required field to `StartRequest` makes the daemon reject every body a
pinned client sends. Mitigation: `#[serde(default)]` on every field but `exec_id` and
`command`, and the omit-everything test at `protocol/src/exec.rs:359-367` as the guard.

## protocol::exec::PollResponse and the flattened Outcome

**Producer:** `protocol/src/exec.rs:229-236` (`PollResponse`), `:56-74` (`Outcome`)

**Consumer(s):**

- `agentd/src/exec.rs:87-90` (re-export)
- `microvms-core/src/session/exec.rs:74-82` — `impl From<protocol::exec::PollResponse> for ExecResult`
- `microvms-cli/src/guards.rs:1941` — the expected envelope shape is written out rather than
  serialized from `PollResponse`, "which is the whole point".
- `microvms-cli/src/commands/attached.rs:1089`, `:1093` — constructs `Outcome` values for its render tests.
- `microvms-core/tests/turmoil_client.rs:952`

**Shape:**

```rust
#[derive(Debug, Deserialize, JsonSchema, Serialize)]
pub struct PollResponse {
    pub exec_id: String,
    pub phase: Phase,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(flatten)]
    pub result: Option<Outcome>,
}
```

```rust
/// Captured output and exit status of a finished exec.
#[derive(Clone, Debug, Default, Deserialize, JsonSchema, Serialize)]
pub struct Outcome {
    /// Exit code, or `None` when the child died to a signal.
    pub exit_code: Option<i32>,
    /// Signal number that killed the child, when one did.
    pub signal: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    /// Set when either stream hit `max_output_bytes` and was cut. An explicit
    /// flag rather than a sentinel string in the output: a marker inside the
    /// bytes is indistinguishable from output that happens to contain it.
    pub truncated: bool,
    /// Set when the post-exit linger deadline expired with the pipes still open,
    /// meaning some grandchild is alive and may write more that nobody will see.
    /// Reported rather than hidden, because a harness that sees empty output from
    /// a command it knows produced some needs to be able to tell why.
    pub writers_may_be_alive: bool,
}
```

**Assumptions consumers make:**

- **An absent outcome is normal, not an error.** `protocol/src/exec.rs:369-374` names the
  failure a stricter reading would cause: a client that read a running exec's missing outcome
  as an error would fail on every poll before the first one that mattered. A running poll
  serializes to exactly `{"exec_id":"e1","phase":"running"}` (`:382`).
- **`truncated` is a flag, never a sentinel string in the bytes**
  (`protocol/src/exec.rs:65-67`) — a marker inside the output is indistinguishable from
  output that happens to contain it.
- **`writers_may_be_alive` is reported rather than hidden**
  (`protocol/src/exec.rs:69-73`), so a harness seeing empty output from a command it knows
  produced some can tell why.
- **`exit_code: None` means a signal killed the child**, so a consumer must read `signal`
  before concluding failure (`protocol/src/exec.rs:60-62`; `microvms-core/src/session/exec.rs:93-96`).
- **`ExecResult` is a thin wrapper and renames one field.**
  `microvms-core/src/session/exec.rs:61-82` maps `result` to `outcome`, deliberately not
  re-modelling the shape so the two cannot disagree.

**Drift risk:** `#[serde(flatten)]` means schemars inlines `Outcome`'s fields into
`PollResponse` and emits **no `$defs` entry for `Outcome`** — the generated
`PollResponse` definition in `docs/schema.json` even carries *Outcome's* doc comment as its
`description`. A generated client reading `docs/schema.json` therefore has no `Outcome` type
to name, and a new field on `Outcome` appears as a new optional field on `PollResponse` with
no signal that it belongs to the finished-exec half. Mitigation: `agentd/tests/schema_artifact.rs`
already asserts `definition_collisions == []`; extend it to assert the expected `$defs` key
set so a flatten added or removed shows up as a named failure.

## The /v1/exec/{id}/stream SSE contract — three payloads and three event names

**Producer:** `protocol/src/exec.rs:180-206` (`OutputEvent`, `GapEvent`, `ExitEvent`), `:256-258` (`EVENT_*`), `:171-178` (`StreamQuery`)

**Consumer(s):**

- `agentd/src/exec.rs:73-78` — the daemon imports `EVENT_EXIT`, `EVENT_GAP`, `EVENT_OUTPUT`
  and re-exports the payload types at `:87-90`.
- `microvms-core/src/session/sse.rs:272` — matches on `protocol::exec::EVENT_OUTPUT` /
  `EVENT_GAP`, and deserializes each payload into `ExecEvent` (`microvms-core/src/session/sse.rs:240-256`).
- `microvms-cli/src/commands/attached.rs:253`, `:1027` — renders `ExitEvent` into the NDJSON stream.
- `microvms-core/tests/turmoil_client.rs:855`, `:870` — drives all three event names under
  simulated faults.

**Shape:**

```rust
/// One `output` SSE event.
#[derive(Debug, Deserialize, JsonSchema, Serialize)]
pub struct OutputEvent {
    pub offset: u64,
    pub stream: StreamKind,
    pub output: String,
}

/// One `gap` SSE event: the byte range a lagging or late subscriber lost.
#[derive(Debug, Deserialize, JsonSchema, Serialize)]
pub struct GapEvent {
    pub from: u64,
    pub to: u64,
}

/// The terminal `exit` SSE event. Emitted before the stream ends, so a client
/// that sees the body close without one knows the connection failed rather than
/// the command finishing.
#[derive(Debug, Deserialize, JsonSchema, Serialize)]
pub struct ExitEvent {
    pub exit_code: Option<i32>,
    pub signal: Option<i32>,
    pub truncated: bool,
    pub writers_may_be_alive: bool,
    /// Total bytes published, so a client can assert it saw all of them.
    pub offset: u64,
}
```

```rust
pub const EVENT_OUTPUT: &str = "output";
pub const EVENT_GAP: &str = "gap";
pub const EVENT_EXIT: &str = "exit";
```

**Assumptions consumers make:**

- **The absence of an `exit` event is the signal, not silence.**
  `protocol/src/exec.rs:195-197` and `microvms-core/src/session/sse.rs:253-255` — a raw byte
  stream cannot distinguish a finished command from a dropped connection, so the terminal
  typed event is what makes the difference observable. Restates
  `.erpaval/solutions/architecture-patterns/byte-offset-cursor-is-what-makes-reconnect-work.md`.
- **`GapEvent.from` is inclusive and `to` is exclusive**, so `to` is where a cursor resumes
  (`microvms-core/src/session/sse.rs:249-252`). Nothing in the wire type says this — it is a
  client-side convention documented only at the consumer.
- **An unknown event name is dropped, not raised; a bad base64 payload is raised.**
  `microvms-core/src/session/sse.rs:260-269` splits the two deliberately: one bad frame must
  not end a live stream, but silently dropping output a caller asked for is the failure the
  whole protocol is shaped to prevent.
- **`ExitEvent.offset` is a total, so a client can assert it saw every byte**
  (`protocol/src/exec.rs:204-205`).
- **The event names are constants because a typo on either side is a stream that carries
  events nobody dispatches** (`protocol/src/exec.rs:251-255`).
- **`ExecEvent` is not `Clone`**, because `ExitEvent` is not, and adding the derive would be
  an edit to a crate the consumer does not own
  (`microvms-core/src/session/sse.rs:236-238`).
- **`StreamQuery.offset` absent means 0**, i.e. everything still in the replay window
  (`protocol/src/exec.rs:174-177`) — not "everything the command ever wrote". The window is
  `stream_replay_bytes: 1048576` in `docs/schema.json`.

**Drift risk:** a fourth event name added on the daemon side is dropped silently by
`microvms-core/src/session/sse.rs` (`Ok(None)`), which is the correct degradation for an old
client but means a *new* client failing to dispatch a name it should handle looks identical.
Mitigation: the `EVENT_*` constants are the single source; a consumer adding dispatch should
match on the constant, and `microvms-core/tests/turmoil_client.rs` should gain a case per
name.

## The microvm --json envelope — the contract the conformance oracle reads

**Producer:** `microvms-cli/src/envelope.rs:311-318` (`ok`), `:321-339` (`error`), `:66` (`API_VERSION`)

**Consumer(s):**

- `conformance/run_rs.py:164-219` — the `Envelope` dataclass, which reads every failure field
  directly so a missing key is a `KeyError` rather than a `None` that flows into a passing
  assertion (`:168-172`).
- `conformance/run_rs.py:222-249` — `KindError`, carrying kind, code, and exit code so a check
  can assert at whichever granularity it means.
- `conformance/run_rs.py:284-302` — cross-checks the process exit code against the envelope's
  own `exitCode`, because they are two independent renderings of one decision.
- `conformance/run_rs.py:1685-1710` — the offline self-test's frozen envelope fixtures.
- `microvms-cli/tests/exit_codes.rs:96`, `microvms-cli/tests/manifest.rs:205`.

**Shape:**

```rust
pub fn ok(kind: &str, data: Map<String, Value>) -> Value {
    json!({
        "status": "ok",
        "apiVersion": API_VERSION,
        "type": kind,
        "data": Value::Object(data),
    })
}
```

```rust
pub fn error(failure: &CliError) -> Value {
    let mut data = failure.data.clone();
    // The fine-grained daemon status, for the consumer the exit code is too coarse for.
    // Inserted rather than replacing whatever `data` already holds, so a teardown's leaked
    // identifiers and the kind coexist on one failure.
    if let Some(wire) = failure.wire_kind {
        data.insert("kind".to_string(), json!(wire.as_str()));
    }
    json!({
        "status": "error",
        "apiVersion": API_VERSION,
        "error": failure.message,
        "code": failure.code(),
        "exitCode": failure.exit.as_u8(),
        "finding": failure.finding(),
        "suggestions": failure.suggestions,
        "data": Value::Object(data),
    })
}
```

**Assumptions consumers make:**

- **Every failure key is unconditional.** `microvms-cli/src/envelope.rs:20-25` — `finding` is
  present and empty when no measured finding applies, `suggestions` is an empty array and
  `data` an empty object rather than absent, because "the consumer that forgets reads
  `undefined` as 'no finding' for a failure that had one". `conformance/run_rs.py:168-172`
  takes the CLI at its word and reads them directly.
- **Exactly one JSON object reaches stdout, except on the streaming path.**
  `microvms-cli/src/envelope.rs:4-11` — progress goes to stderr always, and
  `microvms-cli/tests/thinness.rs:503` asserts no module but `envelope` and two `main`
  exceptions writes to stdout. `conformance/run_rs.py:252-258` types a second document as an
  `EnvelopeError`, deliberately distinct from a protocol result, because it means the binary
  is wrong.
- **The streaming exception is a different discriminant, not a relaxed rule.**
  `microvms-cli/src/envelope.rs:33-49` — a streamed exec emits NDJSON and its final envelope's
  `type` is `microvm.exec.stream`, never `microvm.exec`, and that envelope is written
  **compact** because "the last line is the envelope" is only true if the envelope is one
  line.
- **`--quiet` cannot buy silence about a leak.** `microvms-cli/src/envelope.rs:13-18` — only
  `progress` is suppressed; a stale rate table and a leaked resource still reach `warn`.
  `conformance/run_rs.py:268-271` relies on this to pass `--quiet` on every invocation.
- **`data.kind` is the only place the daemon's fine status survives.**
  `microvms-cli/src/envelope.rs:27-31` names `conformance/run_rs.py` as the consumer that
  needs it, because `ERR_PROTOCOL` covers five `WireKind`s.
- **`apiVersion` bumps on a meaning change, not on a new command.**
  `microvms-cli/src/envelope.rs:62-65` — adding a command changes `microvm manifest`, not this.

**Drift risk:** the envelope is hand-built with `json!` and has no generated schema, so a
renamed key breaks the Python oracle at runtime rather than at build time — and the offline
half of that suite (`conformance/run_rs.py:1685-1710`) carries frozen fixtures that would
need the same edit. Mitigation: `./conformance/run_rs.py --self-test` is free and offline and
is already in `mise run check`'s neighbourhood; keep the fixtures and the `json!` literals
edited in one commit.

## Other contracts

- **`microvms_core::SizeClass`** — `microvms-core/src/sizing.rs:112-119`, five closed baselines
  with `ALL` at `:132`, `DEFAULT = Mib2048` at `:129`, and the one S2 boundary at
  `from_baseline_mib` (`:146`). 13 consumer files across cli, py, js, and core's cost engine.
- **`microvms-cli::Exit` and `EXIT_TABLE`** — `microvms-cli/src/exit.rs:78-102`, `:173-258`.
  Fourteen append-only rows; `#[repr(u8)]` with explicit discriminants so an inserted variant
  cannot silently renumber the contract (`:74-77`). 9 consumer files.
- **`microvms-cli::CliError`** — `microvms-cli/src/exit.rs:266-278`. Carries `wire_kind`,
  `suggestions`, and a `data` map so a partial result stays machine-readable on the failure
  path. 9 consumer files.
- **`microvms_core::session::ExecEvent` / `ExecResult`** — `microvms-core/src/session/sse.rs:240-256`,
  `microvms-core/src/session/exec.rs:67-72`. The client-side view of the wire types, 8 and 7
  consumer files.
- **`microvms_core::sandbox::TeardownReport`** — `microvms-core/src/sandbox.rs:335-361`.
  Returned where a `finally` would run; `image_deleted: Option<bool>` separates "not asked
  for" from "failed". 5 consumer files.
- **`docs/schema.json`** — generated by `agentd/src/bin/schema.rs`, gated by
  `mise.toml:168-173`. 18 routes, 17 `$defs`, `protocol_version: "1"`,
  `definition_collisions: []`, plus a `limits` object publishing every operative cap.
  `agentd/tests/schema_artifact.rs:149-328` probes the real router against it.
- **`microvms-py/microvms.pyi` + `microvms-py/py.typed`** — generated by
  `scripts/generate-py-stubs.py`, gated by `mise.toml:179-195`. 1476 lines, with the
  do-not-edit header at `microvms-py/microvms.pyi:1-8`. The script pins `maturin@1.14.1`
  because maturin 1.15.0 moved `generate-stubs` output into the module's package dir, so
  bumping that pin breaks `mise run stubs:check`.
- **`microvms-js/index.d.ts`** — generated by `napi build --platform`
  (`microvms-js/package.json:12`), gitignored at `.gitignore:29`, **no drift gate**. 1075
  lines, declared as the package's `"types"` at `microvms-js/package.json:8`.
- **`pinned_rates()` and its Python twin** — `microvms-core/src/cost.rs:1011-1026` against
  `scripts/check-live-rates.py:119-145`. A deliberate second copy: `:112-118` states that
  importing the values would compare a table against itself. `verify_twin` (`:148-211`) reads
  the Rust literals as text, so a reflow of `pinned_rates()` is a named exit 1.
  `every_rate_byte_matches_the_python_literal` (`microvms-core/src/cost.rs:2179-2196`) checks
  scale as well as value.
- **`microvms-core/src/constants.rs` against the botocore service model** — 40+ constants
  (`MODEL_API_VERSION = "2025-09-09"` at `:57`, `MAX_RUN_HOOK_PAYLOAD_BYTES = 4096` at `:83`)
  read back out of the shipped `lambda-microvms` model by `scripts/check-model-drift.py`.
  `DOCUMENTED_RUN_HOOK_PAYLOAD_BYTES = 16_384` (`:97`) is retained as the contradicted prose
  figure the check caught.
- **`session_constants`, which diverges between the two bindings** —
  `microvms-py/src/session.rs:578-604` publishes 7 keys;
  `microvms-js/src/session.rs:574-606` publishes 10, adding `wsSubprotocol`,
  `wsAuthSubprotocolPrefix`, `wsPortSubprotocolPrefix`. Nothing asserts the two dictionaries
  agree.
- **`protocol::hook::RunHookEnvelope` / `RunHook`** — `protocol/src/hook.rs:26-30`, `:45-54`.
  The platform wraps the caller's string, so the payload is one `serde_json` parse deeper than
  the body (measured 2026-08-05, `:20-25`). `RunHook::parse` (`:119-150`) is hand-walked so no
  refusal quotes a value, because the payload carries the agent token
  (`:56-63`); unknown keys are ignored on purpose, since a 400 here terminates the VM
  (`:108-114`).
- **`protocol::fs::FsQuery` / `FileReadQuery`** — `protocol/src/fs.rs:17-24`, `:38-57`. `path`
  missing is 400 and never 404, because clients map 404 onto `FileNotFoundError` (`:13-16`);
  `mode` is a string so `0644` and `644` both parse (`:21-23`); line ranges are 1-based
  inclusive on both ends with `end_line` past EOF reading through, verbatim from the AI SDK
  harness contract (`:34-37`).
- **`protocol::exec::ErrorBody` and the ten `ERROR_*` slugs** — `protocol/src/exec.rs:245-249`,
  `:266-286`. A client branches on `error` plus the status code and never on `detail`
  (`:240-244`). The fs routes answer `text/plain` instead (`protocol/src/fs.rs:5-6`), and
  `ErrorBody` gets **no `$defs` entry** in `docs/schema.json`.
- **`protocol::PROTOCOL_VERSION` and `VERSION_HEADER`** — `protocol/src/lib.rs:58`, `:66`.
  `protocol/src/lib.rs:40-54` states what a client must do on each mismatch case, including that a
  differing `daemon_version` under the same `protocol_version` must not be treated as an
  error.
- **`spec/core.symspec.json` and `spec/agentd.symspec.json`** — 51 EARS requirements held as
  an id-keyed object (each with `key`, `patternType`, `priority`, `sentence`, `systemName`,
  `verificationMethod`) plus a `stateModel` whose five variables are mirrored field-for-field
  by `microvms-core/src/sandbox.rs:428-433`.
- **The workspace dependency edges** — `microvms-cli/tests/dependency_direction.rs:68-125`
  asserts them as equalities, not as `assert!(no edge)`, because a stub crate with no
  dependencies passes a negative assertion (`:11-12`).
  `microvms-cli/tests/thinness.rs:66` holds the CLI's six-entry allowlist, each with a
  paragraph, and `:145-213` asserts the direct dependency set is exactly that.

## See also

- [impact analysis](impact-analysis.md) — 40 shared source citations
- [business logic](business-logic.md) — 22 shared source citations
- [public api](../reference/public-api.md) — 22 shared source citations
- [debugging guide](debugging-guide.md) — 16 shared source citations
- [tech debt](tech-debt.md) — 13 shared source citations
