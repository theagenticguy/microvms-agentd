# microvms-agentd · Tech debt

This register was assembled from four passes over the tree, and one of them came back empty in a way worth stating first.

**The marker pass found nothing.** A case-sensitive grep for `\bTODO\b`, `\bFIXME\b`, `\bHACK\b`, and `\bXXX\b` across every `.rs`, `.py`, `.ts`, `.toml`, `.yml`, `.tf`, and `.sh` file in `agentd/`, `microvms-core/`, `microvms-cli/`, `microvms-js/`, `microvms-py/`, `protocol/`, `model/`, `spec/`, `conformance/`, `scripts/`, and `.github/` returns zero lines. A wider sweep adding `REFACTOR`, `DEPRECATED`, `WORKAROUND`, `KLUDGE`, `TEMPORARY`, `for now`, and `placeholder` also returns zero. The only marker hits repo-wide are inside `docs/.packets/*.md`, which is this documentation run's own scaffolding rather than the codebase.

**The convention that replaced markers is a written acceptance at the debt site.** A grep for `deliberate|accepted|declined|assessed|on purpose|Retired` over the same trees returns roughly sixty hits, most of them module-doc paragraphs under their own H2 heading, naming a decision, its cost, and what would have to change for the decision to flip. A debt sweep landed in the two most recent commits (`2155dbb` "Retire the session's debt register: one API, one FromStr, six written acceptances" and `7ffc602` "Complete the CLI surface") and moved three previously-undocumented debts out of a session document and into the code they would have touched — `CHANGELOG.md:83-93` records that move explicitly. So this register reflects post-sweep state: the rows are mostly *accepted* debts with a citable reason, not open defects, and where a reason exists this file quotes it rather than re-arguing it.

**What the other three passes covered.** Manifest and workflow pins (`Cargo.toml` files, `mise.toml`, `.github/workflows/`), scanner-suppression files (`.trivyignore.yaml`, `osv-scanner.toml`), error-swallowing patterns (`let _ =`, `.ok();`, `unwrap_or_default()`), and test-colocation (`grep -l "mod tests"` per crate against per-file line counts). Two of these produced genuinely unrecorded findings — the unpinned installer downloads in the security job, and the bindings' absent unit-test tier — and those are the rows without an acceptance quote.

Ranking is `cost-to-remove × consequence-of-leaving`, so the top rows are the ones where the remedy is real work *and* leaving it has an ongoing cost. The accepted-with-reason rows sink not because they are cheap but because their consequence is bounded and someone has already priced it.

Category vocabulary is closed: `marker`, `wrong abstraction`, `error handling`, `dead code adjacent`, `deprecated pattern`, `version pin`, `duplicated logic`, `missing tests`. No `marker` row appears, because there are no markers.

## Ranked register

| Rank | Debt item | Category | Cost to fix | Citation |
| --- | --- | --- | --- | --- |
| 1 | `spec:core` — the 51-requirement Z3 verification tier — runs on exactly one machine, by absolute path into a home directory, and is in neither `check` nor CI. It is the repo's highest-value gate and the least reachable one. | missing tests | L | `mise.toml:137-155`, `mise.toml:203-205`, `.github/workflows/ci.yml:296-304` |
| 2 | Bindings have no unit-test tier: 3,790 lines of `microvms-py/src` and 3,071 of `microvms-js/src` carry zero `mod tests`, backed only by two smoke files that are scoped to guard proofs rather than coverage by their own docstring. | missing tests | L | `microvms-py/src` (no `mod tests` in any file), `microvms-js/src` (same), `microvms-py/tests/test_smoke.py:4-9`, `microvms-js/__test__/smoke.mjs` |
| 3 | Four unpinned installer fetches inside the job whose purpose is supply-chain assurance — two `curl … install.sh \| sh`, one `releases/latest` tarball piped to `tar xz`, one `releases/latest` binary written to `/usr/local/bin`. No checksum, no hash pin, no acceptance written. | version pin | M | `.github/workflows/ci.yml:136-138`, `.github/workflows/ci.yml:166-167`, `.github/workflows/ci.yml:188` |
| 4 | Both bindings keep the `Stream` path and the `futures-util` dependency the CLI retired. Accepted with the reason at both sites: the callback is synchronous, so it can only `blocking_send`, and capacity 1 means the channel is full whenever the host consumer is slightly behind — which would block the runtime worker driving the stream. Closing it needs an async-callback overload in core. | duplicated logic | M | `microvms-js/src/exec.rs:32-61`, `microvms-py/src/exec.rs:31-46`, `microvms-js/Cargo.toml:40`, `microvms-py/Cargo.toml:45` |
| 5 | 40 `cli.py:<line>` citations across the Rust tree point into a file that is only reachable at `c4d396e^`. The retirement is deliberate and documented; the line numbers are now uncheckable without a `git show`. | dead code adjacent | M | `microvms-cli/src/render.rs` (10 such citations), `docs/CLI-COVERAGE-PLAN.md:97`, `README.md:19`, `mise.toml:106-112` |
| 6 | No `Sandbox::attach` for the three attached lifecycle commands, so `suspend`/`resume`/`terminate` go through `ControlPlane` directly rather than through the type `run` and `build` use. Assessed and refused: an attach constructor manufactures a second initial state neither the symspec state model nor `model/src/client.rs`'s `init_states` enumerates, so both proof suites would quietly stop covering it. | wrong abstraction | L | `microvms-cli/src/commands/lifecycle.rs:15-54`, `spec/core.symspec.json` (`stateModel`), `model/src/client.rs` |
| 7 | `microvm logs` cannot read CloudWatch and exits `ERR_PRECONDITION` with an `aws logs` invocation instead. Assessed and refused on three grounds, the third decisive: `conformance/infra/main.tf` grants no log-*read* action to any identity, so a reader shipped today would answer `AccessDeniedException` in an account configured exactly as documented — and that message is the precise confusion the build-role-prefix finding exists to prevent. | wrong abstraction | M | `microvms-cli/src/commands/local.rs:92-145`, `microvms-core/src/control/image.rs:366-379`, `conformance/infra/main.tf:196` |
| 8 | `aws-actions/configure-aws-credentials@v4` is the one workflow step still on Node 20, printing a deprecation warning on every live-conformance run in a file whose stated goal is zero warnings. Accepted as unfixable downstream: "v5 is still Node 20 upstream". | version pin | S | `.github/workflows/live-conformance.yml:50-53`, `.github/workflows/live-conformance.yml:70`, `.github/workflows/ci.yml:11-37` |
| 9 | `setup-uv` deliberately stops at the `v7` major rather than `v9.0.0`, and the reason names a missing prerequisite: from v8 astral publishes no rolling tags, and an exact pin with no Dependabot in the repo silently stops receiving patches. The comment says to delete itself once a bot lands. | version pin | S | `.github/workflows/ci.yml:28-34`, absence of `.github/dependabot.yml` |
| 10 | Ledger and teardown writes swallow every filesystem failure. Accepted at the site: a ledger write that raised would replace the caller's real failure with a filesystem one, and the identifiers are still in the failure envelope. The same shape recurs at four other cleanup sites without a local reason. | error handling | S | `microvms-cli/src/ledger.rs:88-103`, `microvms-cli/src/ledger.rs:143-151`, `agentd/src/identity.rs:342`, `agentd/src/fs.rs:384`, `microvms-cli/src/commands/doctor.rs:343` |
| 11 | JSON dollar strings may differ in trailing zeros from the retired oracle's, because `rust_decimal` normalizes a product's scale differently from Python's `decimal`. Accepted: the figures are numerically equal, and rescaling would *round* a figure whose exactness is the reason it is a string at all. | deprecated pattern | S | `microvms-cli/src/render.rs:27-51`, `microvms-core/src/cost.rs:182` |
| 12 | The thinness allowlist's stated reason for the `protocol` dependency is now false: it reads "core does not re-export it", while `microvms-core/src/lib.rs:77` is `pub use protocol;` and the CLI manifest says so in the same sweep. The guard only asserts `reason.len() > 25`, so a reason can go stale without failing. This is the enforcement mechanism itself carrying a wrong statement. | deprecated pattern | S | `microvms-cli/tests/thinness.rs:69-72`, `microvms-core/src/lib.rs:77`, `microvms-cli/Cargo.toml:47-51`, `microvms-cli/tests/thinness.rs:214-219` |
| 13 | `microvms-cli`'s direct `protocol` edge is no longer forced — core re-exports it — and the manifest calls dropping it "a fine future cleanup". It stays because resolution is identical either way and the thinness allowlist names it. | dead code adjacent | S | `microvms-cli/Cargo.toml:47-51`, `microvms-cli/src/commands/lifecycle.rs:675-678` |
| 14 | `Results.skipped` and the `skip()` primitive have no live caller in the conformance suite; they survive on purpose, exercised only by `--self-test`, because "a suite that removed its own ability to report a skip is a suite whose next gap is silent". | dead code adjacent | S | `conformance/run_rs.py:70-74`, `conformance/run_rs.py:492-506`, `conformance/run_rs.py:2106` |
| 15 | Two `#[allow(clippy::…)]` attributes carry no `reason =` string, in a codebase where the other twelve all do. | error handling | S | `agentd/src/exec.rs:1124`, `microvms-core/tests/turmoil_client.rs:462` |
| 16 | `docs/CLI-COVERAGE-PLAN.md` survives as a plan whose numbers are wrong twice over — banner-marked history, kept for the reasoning. Retained deliberately; the live figure is now derived by the suite rather than written down. | dead code adjacent | S | `docs/CLI-COVERAGE-PLAN.md:1-19` |

## Explicit markers

**There are none.** This section is empty of the usual content, and the emptiness is the finding.

The grep run, verbatim:

```
grep -rInE '\b(TODO|FIXME|HACK|XXX)\b' --include="*.rs" --include="*.py" --include="*.ts" \
  --include="*.js" --include="*.toml" --include="*.yml" --include="*.tf" --include="*.sh" \
  agentd microvms-core microvms-cli microvms-js microvms-py protocol model spec \
  conformance scripts .github Cargo.toml
```

Zero lines of output across 87 Rust files, two Python files, the workflow directory, and every manifest. Widening the pattern to `REFACTOR|DEPRECATED|WORKAROUND|KLUDGE|TEMPORARY|for now|placeholder` also returns zero.

What stands in their place is a set of H2-headed acceptance paragraphs. Each names the debt in its heading, so the headings themselves are the marker list this codebase has. Quoted verbatim:

- ``# This file can drop `futures-util`, and the API to do it with already exists`` — `microvms-js/src/exec.rs:32`
- ``# Why this file still names `futures-util` `` — `microvms-py/src/exec.rs:31`
- ``# There is no `Sandbox::attach`, and adding one would cost the proofs`` — `microvms-cli/src/commands/lifecycle.rs:15`
- `# Adding a reader to core was assessed and refused, and not on grounds of size` — `microvms-cli/src/commands/local.rs:92`
- `# Why this fails rather than returning an empty list` — `microvms-cli/src/commands/local.rs:85`
- `# A dollar string's trailing zeros may differ from the Python oracle's, and that is accepted` — `microvms-cli/src/render.rs:27`
- `# Runtime-checked rather than typestate, deliberately` — `microvms-core/src/sandbox.rs:19`
- `/// Writes the record. Failures are swallowed.` — `microvms-cli/src/ledger.rs:88`
- `# Leaked is recorded before the delete is attempted` — `microvms-cli/src/ledger.rs:11`

And three suppression entries, each carrying a reason under the same stated rule — "an ignore without a reason is a finding someone silenced":

- `Accepted findings, each with its reason. An ignore without a reason is a finding someone silenced; an ignore with one is a decision someone made.` — `.trivyignore.yaml:2-3`
- `reason = "optional rust_decimal feature, never enabled; not compiled into any artifact"` — `osv-scanner.toml:13`
- `The conformance bucket holds a zipped daemon binary and a Dockerfile for the minutes a live run takes, then empties.` — `.trivyignore.yaml:9-13`

## Pattern-level smells

### The strongest verification tier is the least reachable one

The repo runs three formal-verification surfaces: a `stateright` model, a symspec/Z3 requirements check over the daemon draft, and a symspec/Z3 check over `microvms-core`'s 51 requirements including a five-variable state model, seven effects, three constraints, and an unbounded-reachability tier. Only the first is in `check`. The third — the one that proves the constraints hold over unbounded runs — is invoked as `node ~/workplace/symspec/packages/symspec/dist/cli.mjs`, a path inside one developer's home directory, because symspec v5 (`1.0.0-alpha.0`) is published nowhere. The v0.1.0 package that *is* on npm cannot read `core.symspec.json` at all, and the CI job was deleted rather than pointed at it, on the argument that a green job labelled "requirements" verifying six draft requirements while skipping the 51 that matter is worse than no job. Both exclusions are argued well, and both leave the same hole: the strongest claim this codebase makes about itself is checked manually, by one person, on one machine, last recorded green on 2026-08-08.

Shows up in:

- `mise.toml:137-155` — the absolute path, the reachability timeout, and the "verified 2026-08-08" note
- `mise.toml:125-135` — the v0.1.0 task, scoped to six requirements, also out of `check`
- `mise.toml:203-205` — `check`'s `depends` list, with neither `spec` nor `spec:core` in it
- `.github/workflows/ci.yml:296-304` — the deleted job and the condition for its return
- `spec/core.symspec.json` — 51 requirements, `docVersion: 3`, `stateModel` present

Cost: L. The remedy is not in this repo — it is publishing or vendoring symspec v5. Vendoring a `dist/cli.mjs` into the tree is the contained version and is what would let CI and `check` both reach it.

### Debt is accepted in prose, and the prose is the only enforcement

Nine of the fifteen rows above are accepted debts whose reason lives in a doc comment. The reasoning is consistently good — each names what the fix would cost, what would have to change for the answer to flip, and often where the counter-argument lives. What none of them have, with one exception, is a test that fails if the acceptance stops being true. The `futures-util` acceptance is the exception and shows what the pattern looks like when it is closed: `microvms-cli/tests/thinness.rs` carries a `RETIRED` array whose one entry names the dependency *and* its replacement, asserted on every run, so a contributor who reaches for `StreamExt` gets the alternative in the failure message. Nothing analogous guards the other eight. If someone adds a `Sandbox::attach`, the symspec model does not notice; if `rust_decimal` changes its scale normalization, no test reads the trailing zeros. *Judgment-call* — this is flagged not because the acceptances are wrong but because prose degrades silently and this repo has already demonstrated it knows the fix.

Shows up in:

- `microvms-cli/tests/thinness.rs:39-57` and `:203-211` — the one acceptance that is enforced, plus the failure message that names the replacement
- `microvms-cli/tests/thinness.rs:69-72` and `:214-219` — and the limit of that enforcement: the `protocol` allowance's reason string still claims "core does not re-export it" after `microvms-core/src/lib.rs:77` made that false, because the only assertion over a reason is `reason.len() > 25`
- `microvms-cli/src/commands/lifecycle.rs:15-54` — the `Sandbox::attach` refusal, unguarded
- `microvms-cli/src/render.rs:27-51` — the trailing-zeros acceptance, unguarded, and it explicitly says byte-for-byte comparison "was never a supported operation"
- `microvms-core/src/cost.rs:1351-1358` — the counter-example: a duplicated table that *was* pulled into core, with an exhaustive-match round-trip test so a new variant cannot skip it

Cost: M per site, and the pattern is mostly a matter of adding one assertion beside each acceptance rather than changing behavior. The stale `protocol` reason is S and is the one row here that is wrong rather than merely unguarded.

### Two bindings that are line-for-line twins, tested by two smoke files

`microvms-py/src` and `microvms-js/src` are 3,790 and 3,071 lines respectively and mirror each other file for file: `cost.rs` (1,152 / 1,061), `sandbox.rs` (764 / 622), `exec.rs` (640 / 462), `session.rs` (554 / 436), plus `errors`, `hooks`, `region`, `lib`. The files acknowledge the twinning in prose — "its Python twin" appears four times in the JS crate. Duplication across two FFI surfaces is not automatically debt; napi and pyo3 genuinely differ, and both crates document exactly where and why. What makes it a smell is the test ratio: neither crate has a single `mod tests`, and the entire tier is `test_smoke.py` (517 lines) plus `smoke.mjs` (455), both of which state in their own headers that they are guard proofs for specific BIND requirements rather than coverage. So the largest duplicated surface in the repo has the thinnest verification, and the divergence the twinning invites — one binding loosening something the other kept closed — is precisely what those smoke files check narrowly and nothing checks broadly. The `CostPhase` fix in `2155dbb` is the proof this happens: both bindings had independently grown the same seven-element phase table, and the core comment notes a parallel table "would have gone stale the first time a phase was added".

Shows up in:

- `microvms-py/src/cost.rs` (1,152 lines) and `microvms-js/src/cost.rs` (1,061 lines) — no `mod tests` in either
- `microvms-core/src/cost.rs:1351-1358` — the duplication that was found and pulled up, with the round-trip guard
- `microvms-py/tests/test_smoke.py:4-9` — "Every test here is a **guard proof** for BIND-2 or BIND-5, not a coverage exercise"
- `microvms-js/src/exec.rs:10` and `microvms-js/src/lib.rs:64` — the twin relationship stated in the code
- `microvms-py/src/exec.rs:45-46` — "`microvms-js/src/exec.rs` carries the same note for the same reason", the twinning applied to an acceptance

Cost: M. The smoke files are the right shape and the gap is breadth, so this is adding tests rather than restructuring.

### The security job pins its actions carefully and downloads its scanners carelessly

`.github/workflows/ci.yml:11-37` is thirty lines of reasoning about action versions: which major is on Node 24, why `checkout` stops at v5 rather than v7, why `setup-uv` takes a rolling major over an exact v9 pin given no Dependabot in the repo. That care does not reach the tools the security and SBOM jobs actually run. betterleaks is fetched by resolving `releases/latest` through the GitHub API and piping a tarball into `tar xz`; syft and grype are `curl … install.sh | sh`; osv-scanner is a `releases/latest` binary written straight to `/usr/local/bin`. Four unverified executables, no checksum on any, in the two jobs whose entire output is a supply-chain assurance. The mitigation is real — these run on the default `contents: read` token and produce no artifact the build consumes — but the asymmetry is stark against the adjacent paragraph, and this is the one debt cluster in the register with no acceptance written anywhere.

Shows up in:

- `.github/workflows/ci.yml:136-138` — betterleaks via `latest` tag resolution, piped to `tar`
- `.github/workflows/ci.yml:166-167` — syft and grype via `install.sh | sh`
- `.github/workflows/ci.yml:188` — osv-scanner from `releases/latest/download`
- `.github/workflows/ci.yml:28-34` — the paragraph that reasons about exactly this tradeoff for actions and reaches the opposite conclusion
- `.trivyignore.yaml:2-3` — the repo's own rule about un-reasoned suppressions, applied here by analogy

Cost: S. Pinning a version plus a `sha256sum -c` per tool is a handful of lines; the harder question is who watches the pins, which is the same Dependabot gap `ci.yml:31` already names.

### A retired oracle that live code still cites by line number

The Python client was the discovery instrument and is now git history — retired deliberately, after both suites passed against real AWS on the same commit, with the reasoning recorded in five places. What did not retire is the citation habit. 40 references of the form `cli.py:718` remain across the Rust tree — 36 in shipped source, 4 in tests, ten of them in `microvms-cli/src/render.rs` alone — each pointing at a line in a file that only exists at `c4d396e^`. These are load-bearing citations, not decoration: `render.rs` uses them to justify why dollars cross as strings and seconds as numbers, and `thinness.rs` cites three of them for the shape of a static guard that was defeated three times. A reader who wants to check one needs the commit hash, which appears once, in a doc marked as history. The same pattern applies to figures: several tests quote numbers the retired suite printed, with "git history is where the code behind the figure lives" as the provenance.

Shows up in:

- `microvms-cli/src/render.rs:17`, `:25`, `:59` — the `cli.py:718` / `:743` / `:2189` / `:688` citations that carry the string-vs-number argument
- `microvms-cli/Cargo.toml:28` — `tests/test_cli.py:209`, `:261`, `:281` cited as the three defeated guard legs
- `docs/CLI-COVERAGE-PLAN.md:97` — the only place the recovery hash `c4d396e^` is written down
- `microvms-cli/src/render.rs:480` and `microvms-core/src/cost.rs:2565` — figures whose provenance is "git history is where the code behind the figure lives"
- `mise.toml:106-112` — the retirement itself, with the argument for why the oracle's job was finished rather than duplicated

Cost: S. Either append the recovery hash to the citation convention once, in a place a reader hits first, or accept it in the same style as everything else here — the one thing that does not fit this codebase's standard is that this particular staleness is nowhere written down.

## See also

- [microvms-agentd · Contract map](contract-map.md)
- [microvms-agentd · Impact analysis](impact-analysis.md)
- [microvms-agentd · Processes](../behavior/processes.md)
- [microvms-agentd · System overview](../architecture/system-overview.md)
- [microvms-agentd · Debugging guide](debugging-guide.md)
