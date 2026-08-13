# microvms-agentd · Tech debt

This register was assembled from four passes over the tree. One of the four, the marker pass, returned no results at all.

**The marker pass found nothing.** A case-sensitive grep for `\bTODO\b`, `\bFIXME\b`, `\bHACK\b`, and `\bXXX\b` across every `.rs`, `.py`, `.ts`, `.toml`, `.yml`, `.tf`, and `.sh` file in `agentd/`, `microvms-core/`, `microvms-cli/`, `microvms-js/`, `microvms-py/`, `protocol/`, `model/`, `spec/`, `conformance/`, `scripts/`, and `.github/` returns zero lines. A wider sweep adding `REFACTOR`, `DEPRECATED`, `WORKAROUND`, `KLUDGE`, `TEMPORARY`, `for now`, and `placeholder` also returns zero. The grep was re-run after the second sweep, with the ten new test files included, and still returned zero.

**The convention that replaced markers is a written acceptance at the debt site.** A grep for `deliberate|accepted|declined|assessed|on purpose|Retired` over the same trees returns 410 lines across 71 Rust files. Most of them are module-doc paragraphs under their own H2 heading. Each names a decision, its cost, and what would have to change for the decision to flip.

**Two sweeps have now run against this register. The second sweep closed most of the rows.** The first sweep landed in `2155dbb` ("Retire the session's debt register: one API, one FromStr, six written acceptances") and `7ffc602` ("Complete the CLI surface"). It moved three previously-undocumented debts out of a session document and into the code they would have touched; `CHANGELOG.md:83-93` records that move. The second sweep is the one this file now describes. Eight of the sixteen rows closed. Seven of those closed in the working tree as this is written, and one (`configure-aws-credentials`) closed in `39a7072`. What closed: the bindings grew a real unit tier, the four unpinned scanner downloads became version-and-hash pins, both bindings dropped the `Stream` path and `futures-util` behind a new async-callback driver in core, the CLI's redundant `protocol` edge came out, the two bare `#[allow]`s grew reasons, the stale allowlist reason went away with the entry it described, and every file citing the retired oracle by line number gained a recovery anchor. This register therefore reflects the post-second-sweep state. What remains is one debt open by explicit owner decision and eight *accepted* debts with a citable reason. Where a reason exists in the code, this file quotes it rather than re-arguing it.

**What the other three passes covered.** The other passes examined manifest and workflow pins (`Cargo.toml` files, `mise.toml`, `.github/workflows/`), scanner-suppression files (`.trivyignore.yaml`, `osv-scanner.toml`), error-swallowing patterns (`let _ =`, `.ok();`, `unwrap_or_default()`), and test-colocation (`grep -l "mod tests"` per crate against per-file line counts). Those passes produced two genuinely unrecorded findings, the unpinned installer downloads in the security job and the bindings' absent unit-test tier, and both are now closed. The error-swallowing pass turned out to have overcounted, and the row on it now says so.

Ranking is `cost-to-remove × consequence-of-leaving`, so the top rows are the ones where the remedy is real work *and* leaving it has an ongoing cost. The accepted-with-reason rows rank low because their consequence is bounded and someone has already priced it, not because they are cheap to fix. One row rose in rank during the second sweep. Closing the unpinned-installer row replaced four `latest` fetches with four exact version-and-hash pairs, and nothing in the repo watches a pin, so the absent-Dependabot row now carries consequence it did not carry before.

Category vocabulary is closed: `marker`, `wrong abstraction`, `error handling`, `dead code adjacent`, `deprecated pattern`, `version pin`, `duplicated logic`, `missing tests`. No `marker` row appears, because there are no markers.

## Ranked register

The register now has nine rows, re-ranked after the second sweep. The eight rows that closed are listed in [what closed](#what-the-second-sweep-closed) below with the evidence for each. They are kept because deleting closed rows would lose the record of what each remedy actually cost.

| Rank | Debt item | Category | Cost to fix | Citation |
| --- | --- | --- | --- | --- |
| 1 | `spec:core` — the 51-requirement Z3 verification tier — runs on exactly one machine, by absolute path into a home directory, and is in neither `check` nor CI. It is the repo's highest-value gate and the least reachable one. **Stays open by explicit owner decision**, not by oversight: the plan is to publish symspec to npm, which retires the absolute path and the vendoring question together. Until that happens the hole is unchanged, and it is now the only row here that is not an acceptance. | missing tests | L | `mise.toml:137-155`, `mise.toml:203-205`, `.github/workflows/ci.yml:306-314` |
| 2 | `setup-uv` deliberately stops at the `v7` major rather than `v9.0.0`, and the reason names a missing prerequisite: from v8 astral publishes no rolling tags, and an exact pin with no Dependabot in the repo silently stops receiving patches. The comment says to delete itself once a bot lands. **Risen from rank 9**: the second sweep added four exact version-and-hash pins for the scanner binaries, so "nothing in this repo watches a pin" now describes five pins rather than one, and `ci.yml:165-168` says out loud that refreshing one means editing both the version and the hash by hand. | version pin | S | `.github/workflows/ci.yml:28-34`, `.github/workflows/ci.yml:165-168`, absence of `.github/dependabot.yml` |
| 3 | No `Sandbox::attach` for the three attached lifecycle commands, so `suspend`/`resume`/`terminate` go through `ControlPlane` directly rather than through the type `run` and `build` use. Assessed and refused: an attach constructor manufactures a second initial state neither the symspec state model nor `model/src/client.rs`'s `init_states` enumerates, so both proof suites would quietly stop covering it. | wrong abstraction | L | `microvms-cli/src/commands/lifecycle.rs:15-54`, `spec/core.symspec.json` (`stateModel`), `model/src/client.rs` |
| 4 | `microvm logs` cannot read CloudWatch and exits `ERR_PRECONDITION` with an `aws logs` invocation instead. Assessed and refused on three grounds, the third decisive: `conformance/infra/main.tf` grants only `CreateLogGroup`/`CreateLogStream`/`PutLogEvents` and no log-*read* action to any identity, so a reader shipped today would answer `AccessDeniedException` in an account configured exactly as documented — and that message is the precise confusion the build-role-prefix finding exists to prevent. | wrong abstraction | M | `microvms-cli/src/commands/local.rs:92-145`, `microvms-core/src/control/image.rs:278`, `conformance/infra/main.tf:158-160`, `conformance/infra/main.tf:210-212` |
| 5 | The bindings' new unit tier reaches five of the eight modules in each crate. `sandbox.rs` — 764 lines in Python, 622 in JS, the largest file in each after `cost.rs` — has no unit file of its own, because every method on it is an AWS call: what a unit run can assert is the constructor refusal and the absent-parameter guards, which `test_smoke.py` already does at `:302-310` and `:316-331`. So the gap is narrower than "no tests" and wider than zero, and the honest statement is that the AWS lifecycle wrapper is covered by `conformance/run_rs.py` and by nothing local. This restates `CHANGELOG.md`'s own "Not yet" entry, which said the same thing at 0.1.0. | missing tests | M | `microvms-py/src/sandbox.rs` (764 lines, no unit file), `microvms-js/src/sandbox.rs` (622, same), `microvms-py/tests/test_smoke.py:302-310`, `microvms-py/tests/test_smoke.py:316-331` |
| 6 | JSON dollar strings may differ in trailing zeros from the retired oracle's, because `rust_decimal` normalizes a product's scale differently from Python's `decimal`. Accepted: the figures are numerically equal, and rescaling would *round* a figure whose exactness is the reason it is a string at all. | deprecated pattern | S | `microvms-cli/src/render.rs:27-51`, `microvms-core/src/cost.rs:180-184` |
| 7 | The 34 `cli.py:<line>` citations across the Rust tree still point into a file only reachable at `c4d396e^`, and the line numbers are still historical. Mitigated rather than closed — see the closed list — and the residue is deliberate: a citation rewritten to a live file would be a claim about code that does not exist. | dead code adjacent | S | `microvms-cli/src/render.rs` (10 such citations, anchored at `:53`), `docs/CLI-COVERAGE-PLAN.md:97`, `mise.toml:106-112` |
| 8 | `Results.skipped` and the `skip()` primitive have no live caller in the conformance suite; they survive on purpose, exercised only by `--self-test`, because "a suite that removed its own ability to report a skip is a suite whose next gap is silent". | dead code adjacent | S | `conformance/run_rs.py:70-74`, `conformance/run_rs.py:492-506`, `conformance/run_rs.py:2106` |
| 9 | `docs/CLI-COVERAGE-PLAN.md` survives as a plan whose numbers are wrong twice over — banner-marked history, kept for the reasoning. Retained deliberately; the live figure is now derived by the suite rather than written down. | dead code adjacent | S | `docs/CLI-COVERAGE-PLAN.md:1-19` |

## What the second sweep closed

The eight closed rows follow, in their former rank order. Each entry names what the fix was and cites the evidence for it, so that a "closed" claim can be checked the same way an acceptance paragraph can.

**Former rank 2 — the bindings' absent unit tier. Closed.** `microvms-py/tests` grew five files plus a `conftest.py`, and 198 tests now pass in the crate (170 in the new files, 28 in the pre-existing `test_smoke.py`). The new files mirror `src/`: `test_cost.py`, `test_errors.py`, `test_exec.py`, `test_region.py`, `test_session.py`. `microvms-js/__test__` grew four files plus a `support/` directory, and 152 tests pass (122 new, 30 in `smoke.mjs`) as `cost.mjs`, `errors.mjs`, `exec.mjs`, `session.mjs`, with `support/sse.mjs` and `support/decimal.mjs` shared. `microvms-js/package.json:14-15` adds `test:smoke` and `test:unit` so the two tiers are runnable apart. Both suites are already in CI's `bindings` job (`.github/workflows/ci.yml:263-273`), which runs the whole directory rather than a named list, so the new files were picked up without a workflow edit.

Both suites state the boundary of what they cover. `microvms-py/tests/conftest.py:18-22` and `microvms-js/__test__/support/sse.mjs:16-19` each say the same thing: the loopback SSE server's frames are this suite's transcription of what `microvms-core/src/session/sse.rs` parses, so nothing here proves `agentd` emits them. If the daemon's framing changed, these tests would stay green while the conformance suite went red. The scope is therefore this: the binding's task, channel, and iterator are under test, and the daemon is not.

**Former rank 3 — four unpinned installer fetches. Closed.** All four are now an exact version, a `sha256sum -c -`, and a `mv` into place: betterleaks 1.7.3 (`ci.yml:134-139`), syft 1.50.0 and grype 0.116.1 (`ci.yml:169-176`), osv-scanner 2.5.0 (`ci.yml:195-200`). No `install.sh | sh` and no `releases/latest` remain in the file. `ci.yml:165-168` also records the maintenance cost. Refreshing a pin means updating the version and the hash from the release's `checksums.txt`, which is the cost rank 2 above tracks.

**Former rank 4 — the bindings' `Stream` path. Closed. The work also surfaced two other defects, described below.** Core grew `ExecHandle::for_each_event_async` (`microvms-core/src/session/exec.rs:380-388`), a callback driver taking `FnMut(ExecEvent) -> Fut` over the same `advance` state machine (`:437`). `for_each_event` is now written in terms of it (`:336`, delegating through `std::future::ready`), so one reconnect loop serves three consumers instead of being spelled three times. Both bindings migrated (`microvms-py/src/exec.rs:417-446`, `microvms-js/src/exec.rs:229-257`). `futures-util` came out of both manifests with its absence documented in place (`microvms-py/Cargo.toml:44-49`, `microvms-js/Cargo.toml:40-45`), and `Cargo.lock` no longer lists it under either crate. Four new tests cover the async driver (`microvms-core/src/session/exec.rs:1432-1687`). The fourth test covers the property only this overload has: a capacity-1 channel with a deliberately slow consumer loses none of five events, which is the exact configuration the synchronous driver could not serve.

Two pre-existing JS defects surfaced during the migration and are fixed, with the reason recorded at each site. First, `stream()` was calling `napi::tokio::spawn` from a synchronous `#[napi] fn` on the JS main thread. That is tokio's own `spawn`, which needs an ambient runtime, so the call produced `there is no reactor running` followed by `fatal runtime error: failed to initiate panic`. A panic across the FFI boundary crashes the Node process. The call is now `napi::bindgen_prelude::spawn`, onto napi's managed runtime, with the whole measurement in the comment (`microvms-js/src/exec.rs:219-229`). Second, a stream rejection was rebuilt as a bare `napi::Error` from the error's reason string. That dropped the cause chain and left `err.cause.message` as `undefined` on the one rejection a caller is most likely to branch on, even though `src/errors.rs` documents `cause.message` as the uniform rule. The rejection now goes through `js_async` like every other path (`:290`). `__test__/exec.mjs` is the regression test for both defects. The cause-chain assertion at `:495-530` checks the chain rather than only the message, and says why.

One finding changed no behavior but is recorded because the next reader would otherwise re-derive it. `AsyncFnMut` is the obvious signature for the driver, and it cannot cross a `tokio::spawn` without the unstable `async_fn_traits` feature, because proving the returned future `Send` requires naming `F::CallRefFuture<'a>` under a `for<'a>` bound. Both bindings spawn this drive, so they cannot avoid that constraint. The measured consequence is a plain `Fut` type parameter and a per-event `Sender::clone` instead of a borrow, which costs one atomic increment. This is written down at `microvms-core/src/session/exec.rs:361-377`.

**Former rank 5 — the `cli.py` citations. Mitigated, deliberately not closed.** Every one of the 14 files carrying a `cli.py:<line>` citation now also carries a one-line anchor naming the recovery command: `` (cli.py line numbers resolve at `git show 'c4d396e^:clients/python/src/microvms_agentd/cli.py'` — the retired oracle.) ``. The set of files matching `cli.py:[0-9]+` and the set carrying the anchor are the same 14 files, and the recovery command resolves to a 2,361-line file. The 34 line numbers themselves remain historical by design. They cite a file that no longer exists, and the chosen fix is to give the reader a way to reach that file rather than to rewrite the numbers to point somewhere else. Because the numbers remain historical, the item stayed on the register at rank 7.

**Former rank 8 — `configure-aws-credentials` on Node 20. Closed in `39a7072`, before this sweep.** `.github/workflows/live-conformance.yml:69` is `@v6`, and `:50-52` records the check: the rolling v6 tag landed on node24, verified against the tag's own `action.yml` on 2026-08-09. No workflow step in the repo is on a Node 20 action.

**Former rank 10 — the swallowed filesystem failures. Closed. The original row had also overcounted.** Two of the five "unreasoned" sites already carried a local reason. A per-site read would have caught this; the pattern grep did not. `agentd/src/identity.rs:341` reads *"Absence is not an error here; `remove_file` on a missing path is ENOENT and the next step creates it anyway"*, describing a `remove_file` whose failure mode is the expected case. `agentd/src/fs.rs:381-383` reads *"Replacing rather than failing on an existing name: an archive may legitimately overwrite, and a partial extraction that has to be retried should converge"*. The two sites that genuinely lacked a reason now have one. `Ledger::clear` (`microvms-cli/src/ledger.rs:151-155`) says a failure to delete a clean run's record costs one stale file in the state dir, and raising would replace the command's real outcome with a housekeeping error. `doctor`'s `TempFile::drop` (`microvms-cli/src/commands/doctor.rs:343-345`) says a panic inside `Drop` during unwind is an abort, so swallowing is the only correct choice. The remaining `let _ = std::fs::` sites are all test scaffolding `Drop` impls under the same argument.

**Former rank 12 — the stale allowlist reason. Closed by deletion.** The `protocol` entry whose reason claimed "core does not re-export it" is gone entirely, along with the dependency it described (below). The six-versus-seven doc comments that counted it were corrected in the same edit: `microvms-cli/tests/thinness.rs:64` now reads "a new entry" rather than "a seventh", and `:133` reads "Six normal dependencies". One thing did not change. The guard's only assertion over a reason is still `reason.len() > 25` (`microvms-cli/tests/thinness.rs:213-218`), so the mechanism that let this reason go stale is intact. This instance closed, and the class did not. The second pattern-smell below covers the class.

**Former rank 13 — the redundant `protocol` edge. Closed.** `microvms-cli` names wire types through `microvms_core::protocol::` throughout (`microvms-cli/src/commands/attached.rs`, `microvms-cli/src/commands/lifecycle.rs:685-690`), the direct dependency is out of the manifest, `ALLOWED` is 6 entries (`microvms-cli/tests/thinness.rs:66`), and `cargo metadata` confirms the six: `clap`, `microvms-core`, `ratatui`, `serde`, `serde_json`, `tokio`. `microvms-cli/Cargo.toml:47-50` records the retirement where the dependency used to be, naming it as the "fine future cleanup" its own former comment predicted. `cargo test -p microvms-cli --test thinness` passes 4/4.

**Former rank 15 — the two bare `#[allow]`s. Closed.** `agentd/src/exec.rs:1124-1129` and `microvms-core/tests/turmoil_client.rs:462-466` both carry `reason =` strings now, and each argues the specific case rather than restating the lint. An AST walk over every `#[allow(` in the tracked Rust tree finds 14 attributes, and all 14 carry a reason.

## Explicit markers

There are no explicit markers in the tree, so this section records that absence rather than a list.

The grep run, verbatim:

```
grep -rInE '\b(TODO|FIXME|HACK|XXX)\b' --include="*.rs" --include="*.py" --include="*.ts" \
  --include="*.js" --include="*.toml" --include="*.yml" --include="*.tf" --include="*.sh" \
  agentd microvms-core microvms-cli microvms-js microvms-py protocol model spec \
  conformance scripts .github Cargo.toml
```

The command produces zero lines of output across 89 Rust files, eight Python files, seven `.mjs` files, the workflow directory, and every manifest. It was re-run after the sweep, including the ten new test files, which is where a marker would most plausibly have appeared. Widening the pattern to `REFACTOR|DEPRECATED|WORKAROUND|KLUDGE|TEMPORARY|for now|placeholder` also returns zero.

In place of markers, the codebase has a set of H2-headed acceptance paragraphs. Each names the debt in its heading, so the headings serve as this codebase's marker list. They are quoted verbatim below, with the two `futures-util` headings replaced by what the sweep put in their place:

- `# The stream is driven by core's async callback driver` — `microvms-js/src/exec.rs:32` and `microvms-py/src/exec.rs:30`, the two headings that used to read "This file can drop `futures-util`" and "Why this file still names `futures-util`"
- `# Why the overload exists, and what it closed` — `microvms-core/src/session/exec.rs:348`
- ``# `FnMut(ExecEvent) -> Fut` rather than `AsyncFnMut` `` — `microvms-core/src/session/exec.rs:361`, the unstable-feature measurement
- ``# There is no `Sandbox::attach`, and adding one would cost the proofs`` — `microvms-cli/src/commands/lifecycle.rs:15`
- `# Adding a reader to core was assessed and refused, and not on grounds of size` — `microvms-cli/src/commands/local.rs:92`
- `# Why this fails rather than returning an empty list` — `microvms-cli/src/commands/local.rs:85`
- `# A dollar string's trailing zeros may differ from the Python oracle's, and that is accepted` — `microvms-cli/src/render.rs:27`
- `# Runtime-checked rather than typestate, deliberately` — `microvms-core/src/sandbox.rs:19`
- `/// Writes the record. Failures are swallowed.` — `microvms-cli/src/ledger.rs:90`
- `# Leaked is recorded before the delete is attempted` — `microvms-cli/src/ledger.rs:11`

Three suppression entries follow the same stated rule, "an ignore without a reason is a finding someone silenced":

- `Accepted findings, each with its reason. An ignore without a reason is a finding someone silenced; an ignore with one is a decision someone made.` — `.trivyignore.yaml:2-3`
- `reason = "optional rust_decimal feature, never enabled; not compiled into any artifact"` — `osv-scanner.toml:13`
- `The conformance bucket holds a zipped daemon binary and a Dockerfile for the minutes a live run takes, then empties.` — `.trivyignore.yaml:9-13`

## Pattern-level smells

### The strongest verification tier is the least reachable one

The repo runs three formal-verification surfaces: a `stateright` model, a symspec/Z3 requirements check over the daemon draft, and a symspec/Z3 check over `microvms-core`'s 51 requirements including a five-variable state model, seven effects, three constraints, and an unbounded-reachability tier. Only the first is in `check`. The third surface is the one that proves the constraints hold over unbounded runs. It is invoked as `node ~/workplace/symspec/packages/symspec/dist/cli.mjs`, a path inside one developer's home directory, because symspec v5 (`1.0.0-alpha.0`) is published nowhere. The v0.1.0 package that is on npm cannot read `core.symspec.json` at all. The CI job was deleted rather than pointed at that package, on the argument that a green job labelled "requirements" verifying six draft requirements while skipping the 51 that matter is worse than no job. Both exclusions are argued well, and both leave the same hole. The strongest claim this codebase makes about itself is checked manually, by one person, on one machine, and was last recorded green on 2026-08-08.

Shows up in:

- `mise.toml:137-155` — the absolute path, the reachability timeout, and the "verified 2026-08-08" note
- `mise.toml:125-135` — the v0.1.0 task, scoped to six requirements, also out of `check`
- `mise.toml:203-205` — `check`'s `depends` list, with neither `spec` nor `spec:core` in it
- `.github/workflows/ci.yml:306-314` — the deleted job and the condition for its return
- `spec/core.symspec.json` — 51 requirements, `docVersion: 3`, `stateModel` with five variables

Cost: L. The remedy lives outside this repo. It is publishing or vendoring symspec v5, and the owner's decision is to publish to npm rather than vendor a `dist/cli.mjs` into the tree. Either option would let CI and `check` reach the tool. Publishing also fixes it for anyone else who ever reads `core.symspec.json`. Until it lands, this smell is unchanged, and the register's rank 1 says so.

### Debt is accepted in prose, and the prose is the only enforcement

Eight of the nine rows above are accepted debts whose reason lives in a doc comment. The reasoning is consistently good. Each names what the fix would cost, what would have to change for the answer to flip, and often where the counter-argument lives. What most of them still lack is a test that fails if the acceptance stops being true.

The second sweep moved this smell in two directions at once, which is why the smell stays. In one direction, `futures-util` is now retired from three crates, and `microvms-cli/tests/thinness.rs`'s `RETIRED` array asserts the CLI's edge stays out, with the replacement API named in the failure message. The two bindings dropped the same dependency with no equivalent guard, because no test in the repo reads `microvms-py/Cargo.toml` or `microvms-js/Cargo.toml`. Their acceptance is a manifest comment, which is exactly the shape this smell is about. In the other direction, the stale-reason instance closed by deleting the entry, but the mechanism that let it go stale is untouched. `microvms-cli/tests/thinness.rs:213-218` still asserts only `reason.len() > 25`, and `:212` still says "a seventh cannot be added silently" over a six-entry table. This is a judgment call. The acceptances are not wrong; the flag exists because prose degrades silently, and the repo has already demonstrated it knows the fix.

Shows up in:

- `microvms-cli/tests/thinness.rs:41-59` and `:198-210` — the one acceptance that is enforced, plus the failure message that names the replacement
- `microvms-cli/tests/thinness.rs:212-218` — the limit of that enforcement: `reason.len() > 25` is the only assertion over a reason, and the comment above it still counts to seven
- `microvms-py/Cargo.toml:44-49` and `microvms-js/Cargo.toml:40-45` — the same retirement, argued as well and guarded by nothing; a binding that re-added `futures-util` would fail no test
- `microvms-cli/src/commands/lifecycle.rs:15-54` — the `Sandbox::attach` refusal, unguarded
- `microvms-cli/src/render.rs:27-51` — the trailing-zeros acceptance, unguarded, and it explicitly says byte-for-byte comparison "was never a supported operation"
- `microvms-core/src/cost.rs:1351-1358` — the counter-example: a duplicated table that *was* pulled into core, with an exhaustive-match round-trip test so a new variant cannot skip it

Cost: M per site. The work is mostly adding one assertion beside each acceptance rather than changing behavior. The cheapest single item is a thinness-shaped manifest guard for the two binding crates. That guard would cover the newest acceptance, and the one most likely to be undone by someone reaching for `StreamExt`.

### Two bindings that are line-for-line twins, now with a unit tier that is also twinned

`microvms-py/src` and `microvms-js/src` are 3,802 and 3,087 lines respectively and mirror each other file for file: `cost.rs` (1,154 / 1,063), `sandbox.rs` (764 / 622), `exec.rs` (650 / 476), `session.rs` (554 / 436), plus `errors`, `hooks`, `region`, `lib`. The files acknowledge the twinning in prose, and "Python twin" appears in four files of the JS crate. Duplication across two FFI surfaces is not automatically debt. napi and pyo3 genuinely differ, and both crates document exactly where and why.

The test ratio was the sharper half of this smell, and it is fixed. 198 Python and 152 JS tests now run against these crates, organized to mirror `src/` module for module. The twinning propagated into the tests themselves: `test_cost.py` and `cost.mjs`, `test_exec.py` and `exec.mjs`, and two SSE-server helpers with the same scripted-frame design and the same boundary paragraph. This was a deliberate choice rather than a mistake, since a shared harness across pytest and `node:test` would put a third language in the middle. One consequence follows from the choice: the property the twin suites exist to protect, one binding loosening what the other kept closed, is now checked twice in parallel rather than once centrally. The `CostPhase` fix in `2155dbb` and the `for_each_event_async` migration in this sweep both showed the same thing: when both bindings hand-roll the same logic, that logic belongs in core.

Shows up in:

- `microvms-py/tests` (5 files + `conftest.py`, 198 passing) and `microvms-js/__test__` (4 files + `support/`, 152 passing) — the tier that closed the gap, itself mirrored
- `microvms-py/tests/conftest.py:18-22` and `microvms-js/__test__/support/sse.mjs:16-19` — the same boundary paragraph, written twice
- `microvms-core/src/cost.rs:1351-1358` — the duplication that was found and pulled up, with the round-trip guard
- `microvms-core/src/session/exec.rs:348-360` — the second one, pulled up in this sweep, with the reason both bindings could not use the existing API
- `microvms-js/src/exec.rs:10` and `microvms-js/src/lib.rs:62-66` — the twin relationship stated in the code

Cost: M. The remaining breadth is `sandbox.rs`, rank 5 above, where a unit run can only assert the constructor refusal because every other method is an AWS call.

### ~~The security job pins its actions carefully and downloads its scanners carelessly~~ — resolved

This smell is resolved. The smell was the asymmetry between thirty lines of reasoning about action versions (`.github/workflows/ci.yml:11-37`) and four unverified executables fetched by `install.sh | sh` or `releases/latest` in the two jobs whose entire output is supply-chain assurance. All four are now version-pinned with a `sha256sum -c -` before use: betterleaks 1.7.3, syft 1.50.0, grype 0.116.1, and osv-scanner 2.5.0. `ci.yml:165-168` argues the choice in the same voice as the action paragraph it was inconsistent with: *"this is the job whose entire output is supply-chain assurance, so its own inputs are the last place an unpinned fetch belongs."* No `install.sh | sh` and no `releases/latest` remain in the file.

The fix left one thing unsolved, which the register now tracks at rank 2. There are five exact pins in a repo with nothing watching them. `ci.yml:31` named that gap for actions before this sweep, and `:167-168` names it again for the binaries. Refreshing a pin is a manual edit of both a version and a hash. The trade is deliberate, and it is the right one, because an unpinned fetch is a live supply-chain hole while a stale pin is a delayed patch. It is still a trade rather than a clean win.

Shows up in:

- `.github/workflows/ci.yml:134-139` — betterleaks, exact tag, hash-checked before extraction
- `.github/workflows/ci.yml:169-176` — syft and grype, exact tags, hash-checked
- `.github/workflows/ci.yml:195-200` — osv-scanner, exact tag, hash-checked
- `.github/workflows/ci.yml:165-168` — the reasoning, in the same register as the action paragraph
- `.trivyignore.yaml:2-3` — the repo's own rule about un-reasoned decisions, now satisfied here

### A retired oracle that live code still cites by line number — now recoverable

The Python client was the discovery instrument and is now git history. It was retired deliberately, after both suites passed against real AWS on the same commit, with the reasoning recorded in five places. The citation habit did not retire with it. 34 references of the form `cli.py:718` remain across the Rust tree, ten of them in `microvms-cli/src/render.rs` alone, plus 7 to `test_cli.py`, 9 to `sandbox.py`, 4 to `cost.py`, and 1 to `sizing.py`. Each points at a line in a file that only exists at `c4d396e^`. These citations carry arguments rather than decoration. `render.rs` uses them to justify why dollars cross as strings and seconds as numbers, and `Cargo.toml` cites three of them for the shape of a static guard that was defeated three times.

The second sweep closed the half of this smell that was a real defect. Every file carrying a `cli.py:<line>` citation now also carries the recovery command in its own doc, a `git show 'c4d396e^:…'` naming the exact blob path. A reader who wants to check a citation no longer has to find a hash written down once in a document marked as history. Sixteen files gained the anchor. What remains is now accepted rather than merely unrecorded. The line numbers are historical, the anchor is per-file rather than a single convention statement, and the eight files citing `sandbox.py` / `sizing.py` / `test_cli.py` (rather than `cli.py`) were left unanchored, because the sweep's scope was the `cli.py` citations the register named.

Shows up in:

- `microvms-cli/src/render.rs:17`, `:20`, `:60` — the `cli.py:718` / `:743` / `:2189` / `:688` citations that carry the string-vs-number argument, with the anchor at `:53`
- `microvms-cli/Cargo.toml:28` — `tests/test_cli.py:209`, `:261`, `:281` cited as the three defeated guard legs; this file is one of the eight without an anchor
- `microvms-core/src/control/artifact.rs`, `connector.rs`, `image.rs`, `hooks.rs`, `region.rs`, `sizing.rs` — the `sandbox.py` / `sizing.py` citations, also unanchored
- `docs/CLI-COVERAGE-PLAN.md:97` — where the recovery hash was written down before the anchors existed
- `microvms-cli/src/render.rs:480` and `microvms-core/src/cost.rs:2565` — figures whose provenance is "git history is where the code behind the figure lives"
- `mise.toml:106-112` — the retirement itself, with the argument for why the oracle's job was finished rather than duplicated

Cost: S for the residue. The remaining work is the same one-line anchor applied to eight more files, which is mechanical. The harder half is already done. Leaving the numbers historical is deliberate, because a citation rewritten to point at live code would be a claim about code that never carried the argument.

## See also

- [microvms-agentd · Contract map](contract-map.md)
- [microvms-agentd · Impact analysis](impact-analysis.md)
- [microvms-agentd · Processes](../behavior/processes.md)
- [microvms-agentd · System overview](../architecture/system-overview.md)
- [microvms-agentd · Debugging guide](debugging-guide.md)
