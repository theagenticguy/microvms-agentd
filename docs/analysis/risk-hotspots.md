# microvms-agentd · Risk hotspots

Risk here combines four signals, because this repo has no conventional static analyser whose findings are severity-labeled per file. The score is `2 × live_round_bugs + 0.25 × distinct_trap_markers + 1 × rising_churn + 1 × live_coverage_gap`. A **live-round bug** is a defect found only by spending real money against real AWS. Five commits carry them, and each one marks a place where a fake diverged from the platform. A **trap marker** is a `TRAP-N` reference in the source, counted once per distinct trap ID per file; each is a guard holding a documented platform behavior in place, so a regression there reappears live rather than in CI. **Rising churn** is a commit count above median + 1σ. A **live-coverage gap** means the file's behavior is not reachable by the one live suite, `conformance/run_rs.py`. The two severity tiers map cleanly onto the packet's original schema: a live-round bug is `error`-class evidence, a trap marker is a `warn`-class standing hazard.

This analysis has three limitations. First, the repo is five days old, with 31 commits between 2026-08-05 and 2026-08-09, so the 30-day window covers the entire history and there is no trend to decay. Every surviving file has at least one commit in the window, which makes `↓ falling` unreachable and leaves only `↑ rising` and `→ flat` in the Trend column. Second, trap-marker counts were initially taken as raw mention counts, which ranked `microvms-core/src/control/mod.rs` above `microvms-core/src/session/mod.rs`. That ranking was misleading because that file is the trap catalog; its 10 markers sit in a module doc-comment as a table of contents, not as 10 resident hazards. To correct this, the weight was cut to 0.25 and counting switched to distinct trap IDs. Third, every file in this repo has the same top owner, `bgagent`, so the ownership column separates nothing. The column is retained because the minority shares mark which files the four publication-hardening commits touched.

| File | Trend | Open findings | Top owner | Citation |
| --- | --- | --- | --- | --- |
| The in-VM session transport | ↑ rising | 2 error, 2 warn | bgagent 80% | `microvms-core/src/session/mod.rs` (996 LOC) |
| Launch, the launch wait, and the proxy token | → flat | 1 error, 7 warn | bgagent 100% | `microvms-core/src/control/microvm.rs` (1235 LOC) |
| The SSE frame parser | ↑ rising | 1 error, 0 warn | bgagent 100% | `microvms-core/src/session/sse.rs` (695 LOC) |
| The control-plane client and trap catalog | → flat | 0 error, 10 warn | bgagent 100% | `microvms-core/src/control/mod.rs` (714 LOC) |
| Cost and pricing arithmetic | ↑ rising | 1 error, 1 warn | bgagent 83% | `microvms-core/src/cost.rs` (4125 LOC) |
| The wire request and response types | → flat | 0 error, 10 warn | bgagent 100% | `microvms-core/src/control/ops.rs` (701 LOC) |
| The CLI lifecycle commands | ↑ rising | 1 error, 2 warn | bgagent 60% | `microvms-cli/src/commands/lifecycle.rs` (1090 LOC) |
| Guest identity repair | → flat | 1 error, 0 warn | bgagent 100% | `agentd/src/identity.rs` (737 LOC) |
| Envelope and human rendering | ↑ rising | 1 error, 0 warn | bgagent 80% | `microvms-cli/src/render.rs` (899 LOC) |
| Image create, build wait, and capabilities | → flat | 1 error, 3 warn | bgagent 67% | `microvms-core/src/control/image.rs` (1026 LOC) |
| The fake control plane every offline tier trusts | → flat | 0 error, 4 warn | bgagent 100% | `microvms-core/src/control/fake.rs` (443 LOC) |
| The Python binding's cost surface | ↑ rising | 0 error, 1 warn | bgagent 75% | `microvms-py/src/cost.rs` (1152 LOC) |
| The HTTP transport seam | → flat | 1 error, 0 warn | bgagent 100% | `microvms-core/src/session/http.rs` (531 LOC) |
| The Node binding's cost surface | → flat | 0 error, 1 warn | bgagent 67% | `microvms-js/src/cost.rs` (1061 LOC) |

## Per-file drill-down

### microvms-core/src/session/mod.rs

**What's there.** The in-VM client: a `Transport` holding an HTTP backend, the agent token, and the optional proxy auth, plus the `Session` and `SessionBuilder` that sit on top of it (`microvms-core/src/session/mod.rs:62-71`). It is the single funnel every request to a running MicroVM passes through, which is why the proxy-token mint sits inside it rather than beside it (`microvms-core/src/session/mod.rs:88-90`).

**Recent activity.** 5 commits, above the rising threshold of 4 (median 2, σ 1.70). Two of those five are live-round fixes, which is the highest concentration in the repo.

**Owners.** bgagent 80% (4 of 5), Laith Al-Saadoon 20%.

**Findings.** 2 error-class, 2 warn-class. The first error is the header-injection bug from `522bcd2`: `Transport::request` replaced the header vector with the auth headers instead of prepending, stripping the content-type the exec path set, so the daemon answered 400 "body is not a valid start request" while all 310 tests stayed green, because the fakes parse bodies without reading content-type. The fix and its reasoning are inline at `microvms-core/src/session/mod.rs:106-113`, and the guard is `a_callers_content_type_survives_the_auth_header_injection` at `microvms-core/src/session/mod.rs:699-711` with its break-proof recipe written into the doc comment at `microvms-core/src/session/mod.rs:696-697`. The second error is the null `agentToken` from `1a567e4`: core minted the token inside `Sandbox::run` and nothing exposed it, so conformance authenticated as the literal string `'None'` and three checks failed in one shape. `Session::agent_token()` now hands the bearer back (`microvms-core/src/session/mod.rs:199-201`) with the reasoning at `microvms-core/src/session/mod.rs:717`, and `Debug` still redacts (`microvms-core/src/session/mod.rs:983`). The two warn-class traps are TRAP-7 and TRAP-9, both proxy-token properties (`microvms-core/src/session/mod.rs:5-6`). There is no live-coverage gap, because `drive_exec` and `drive_lifecycle` both drive this path (`conformance/run_rs.py:839`, `conformance/run_rs.py:938`).

### microvms-core/src/control/microvm.rs

**What's there.** Launch, the launch wait, the four lifecycle calls, and the proxy token, with TRAP-5 and TRAP-8 closed at the type level (`microvms-core/src/control/microvm.rs:1-2`). `RunHookPayload` cannot hold an over-ceiling value, so the 4096-byte check happens where the payload is constructed rather than where the request is sent, closing every path into `RunMicrovm` with one guard (`microvms-core/src/control/microvm.rs:5-8`).

**Recent activity.** 3 commits, flat. One is a live-round fix (`2cc840b`, +85/-2).

**Owners.** bgagent 100% (3 of 3).

**Findings.** 1 error-class, 7 warn-class, which is the highest distinct-trap count of any file. The error is credential leakage. Both `RunHookPayload`'s `Debug` (`microvms-core/src/control/microvm.rs:78`) and `ProxyToken`'s (`microvms-core/src/control/microvm.rs:222`) were derived and printed live secrets, while three sibling types already guarded the same invariant by hand. Each now redacts, and the guard was verified by restoring the derive and confirming the test fails. Among the warns, TRAP-5 shows the service's own documentation reproducing the bug. The model's doc string for `runHookPayload` reads "Maximum: 16,384 bytes" while its shape `RunMicrovmRequestRunHookPayloadString` declares `max: 4096`, so a reader who checks the docs rather than the shape uses a limit 4x too large (`microvms-core/src/control/microvm.rs:10-17`, corroborated at `docs/PLATFORM.md:56-76`). The live-coverage gap covers the payload ceiling and the terminal-state fast-fail. 25 inline tests cover them, but no live check pushes a 4097-byte payload or kills a VM before `RUNNING`, since both would cost a launch to observe.

### microvms-core/src/session/sse.rs

**What's there.** Incremental Server-Sent Events parsing and the typed events the daemon emits, hand-rolled rather than taken from a crate because "an eventsource crate would bring a reconnect policy keyed on `Last-Event-ID`, which is the wrong cursor: it resumes at an event, and this protocol resumes at a byte" (`microvms-core/src/session/sse.rs:17-21`). Bytes buffer until a blank line proves a frame complete, so a `data:` line split across two reads is not lost (`microvms-core/src/session/sse.rs:4-9`).

**Recent activity.** 4 commits, at the rising threshold. `2cc840b` rewrote it (+266/-21), the largest single-file change in that commit after `cost.rs`.

**Owners.** bgagent 100% (4 of 4).

**Findings.** 1 error-class, 0 warn-class. The error is a denial-of-service case the offline tiers could not see. The parser rescanned its whole buffer from byte zero on every chunk with no ceiling, so 6.5 MB of unterminated stream cost 31 seconds of CPU inside the async task. The fix has two parts. A resuming cursor backs up `MAX_TERMINATOR_LEN - 1` so a terminator that straddles two reads is still found (`microvms-core/src/session/sse.rs:34`). A 4 MiB pending ceiling errors as `Protocol`, which is fatal to the stream loop rather than a reconnect into the same wedge (`microvms-core/src/session/sse.rs:68`, `microvms-core/src/session/sse.rs:115-125`). The constant's doc comment explains the threat model. The peer is a proxy, and a proxy answering a stream request with an HTML error page produces exactly that shape, with no frame boundary ever coming (`microvms-core/src/session/sse.rs:39-46`). The regression test names the measured shape — 800 unterminated 8 KiB reads (`microvms-core/src/session/sse.rs:514`). The live-coverage gap is the hostile-input path. `drive_streaming` exercises well-formed streaming (`conformance/run_rs.py:1282`), but no live check feeds the parser a non-SSE body, so the ceiling is proven only by the 13 inline tests.

### microvms-core/src/control/mod.rs

**What's there.** The control-plane client — SigV4-signed rest-json — and the repo's trap catalog, enumerating TRAP-1, 2, 3, 4, 5, 8, and 11 against the specific method or type that closes each, with an S1/S2 strength label per entry (`microvms-core/src/control/mod.rs:15-29`). `ControlPlane::new` resolves credentials and either yields a usable client or says why not; there is no builder and no partially constructed state (`microvms-core/src/control/mod.rs:7-11`).

**Recent activity.** 2 commits, flat. No live-round fix touched it.

**Owners.** bgagent 100% (2 of 2).

**Findings.** 0 error-class, 10 warn-class. This ties for the highest distinct-trap count, and it is the count that forced the scoring-weight correction. These 10 are references rather than resident hazards. The file's main risk is that it holds the written rationale for why the other guards exist. Two entries carry most of that weight. TRAP-11 is closed by leaving a method unimplemented: `CreateMicrovmShellAuthToken` is in the service model and deliberately not implemented, so the test for it counts the calls a full lifecycle made rather than asserting a refusal (`microvms-core/src/control/mod.rs:26-37`). The file also explains why every downstream guard is necessary rather than redundant. botocore's `VALIDATED_METADATA_ATTRS` is `{required, min, document, union}`, so `max`, `pattern`, and `enum` violations go to the wire; the assumption that the SDK validates the model was never true (`microvms-core/src/control/mod.rs:39-44`). The live-coverage gap is the whole class of traps closed by absence. A trap closed by a method not existing cannot be probed live at all, so the 11 inline tests and the `spec:core` requirement tier are the only oracle.

### microvms-core/src/cost.rs

**What's there.** What a run cost and what a plan will cost, with every figure labeled by provenance. Two rules hold throughout. Seconds are measured and dollars are estimated, so there is no type for an actual dollar amount, only `EstimatedUsd` (`microvms-core/src/cost.rs:15-21`). An unknown cost is not represented as zero; `Amount::Unpriced` carries a reason as a distinct variant a consumer must handle (`microvms-core/src/cost.rs:23-28`).

**Recent activity.** 6 commits, the highest churn of any source file in the repo and well above the rising threshold. `2cc840b` changed it +266/-27.

**Owners.** bgagent 83% (5 of 6), Laith Al-Saadoon 17%.

**Findings.** 1 error-class, 1 warn-class, plus the repo's worst live-coverage gap. The error is the oracle-parity divergence from `2cc840b`. It survived because a wrong guard test required the extra row: the guard asserted the divergence rather than catching it. As a result, lower-bound totals named reasons instead of phases, `--estimate` mislabeled its report, and dense output carried a total row the Python oracle never emitted. The coverage gap has three parts. First, 65 inline tests cover the arithmetic but no live check exercises a bill. The only live-adjacent gate is `scripts/check-live-rates`, which validates the rates against the Pricing API but not the computation over them (`mise.toml:256-275`). Second, the same arithmetic is triplicated into `microvms-py/src/cost.rs` (1152 LOC) and `microvms-js/src/cost.rs` (1061 LOC). Both report zero tests under `cargo test --workspace` because their guards only run under maturin and napi (`.github/workflows/ci.yml`, `bindings` job). `conformance/run_rs.py` drives the CLI, so nothing exercises either binding against real AWS. Third, one rate in the table is derived rather than read. `RateTable::storage_gb_month` is the API's per-GB-hour figure times `HOURS_PER_MONTH`, and the earlier hand-copied value was 1.37% low (`microvms-core/src/cost.rs:58-60`, `docs/PLATFORM.md:259-264`). The warn-class trap is TRAP-13, the five-row sizing table that must be read as data rather than computed as four times the baseline. Planting the peak in place of the baseline produces an error exactly 4x in magnitude (`microvms-core/src/sizing.rs:13`, `microvms-core/src/sizing.rs:290`).

## See also

- [microvms-agentd · System overview](../architecture/system-overview.md)
- [microvms-agentd · Contract map](../insights/contract-map.md)
- [microvms-agentd · Business logic](../insights/business-logic.md)
- [microvms-agentd · Impact analysis](../insights/impact-analysis.md)
- [microvms-agentd · Debugging guide](../insights/debugging-guide.md)
