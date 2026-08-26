# microvms-agentd · Tech debt

**The marker pass returns zero rows, and that is a fact about the convention rather than about
the debt.** A case-sensitive grep for `\bTODO\b`, `\bFIXME\b`, `\bHACK\b`, and `\bXXX\b` across
every tracked `.rs` and `.py` file produces no output. Widened to every tracked file type, the
only hits are two documents describing the absence. Case-insensitive over `.rs` and `.py`, the
single hit is the byte literal `b"xxxxxxxx"` in a stream test (`agentd/src/exec.rs:3234`). The
repo states the convention that replaced markers: comments record constraints and defects
defended against, never narration. So the substitute for a `TODO` here is a doc-comment heading
at the debt site that names the decision, its cost, and what would flip it — 345 such headings,
carried by 77 of the 90 tracked `.rs` files. Reading "no markers" as "no debt" would invert the
finding.

**Methodology.** Marker-grepping being empty, this register was assembled from six passes, each
executed against source this session: (1) the marker grep above, kept as a negative result;
(2) declined and open scope in `docs/STRATEGY.md`; (3) the twelve compounded lessons in
`.erpaval/solutions/`, read for residual risk rather than for history; (4) version pins across
`Cargo.toml`, `Cargo.lock`, `deny.toml`, `mise.toml`, `microvms-js/package.json`,
`microvms-py/pyproject.toml`, and `.github/workflows/`, on the principle that every pin is a
deferred upgrade with an owner; (5) gate-coverage differencing — what `mise run check` runs
against what CI runs, and what each omits; (6) prose-versus-code differencing, checking whether
each acceptance paragraph still describes the code beneath it. Passes 5 and 6 produced the
findings the earlier passes did not.

**What this register is not about.** `mise run check` — lint, security, all six Rust tiers,
`schema:check`, `stubs:check`, `model:check`, `live:check`, and the `aarch64-musl` cross-build
(`mise.toml:290-301`) — is green as of 2026-08-25. Nothing below is a failing build or a broken
test. This is structural and deferred-maintenance debt: gates that exist in one place and not
another, pins with a manual tracking obligation, one number written down in several places, and
rationale paragraphs that have outlived the code they explain.

**Deliberate is distinguished from accidental throughout.** Rows marked *deliberate* are
documented refusals with a stated cost — `docs/STRATEGY.md:117-136` declines a turn-boundary
suspend protocol, process-tree fork, AgentCore exec/PTY parity, and, in three words,
"**Not an orchestrator.**" A declined item is still debt in the sense that it is capability a
reader may expect and will not find, but it is not rot, and calling it rot would misprice it. The
accidental rows are the ones where nobody chose the current state.

Ranking is `cost-to-fix × consequence-of-leaving`, so a row rises when the remedy is real work
*and* leaving it costs something continuously. Cheap-and-consequential rows rank above
expensive-and-bounded ones. Category vocabulary is closed — `marker`, `wrong abstraction`,
`error handling`, `dead code adjacent`, `deprecated pattern`, `version pin`, `duplicated logic`,
`missing tests` — and no `marker` row appears, because there are no markers.

## Ranked register

| Rank | Debt item | Category | Cost to fix | Citation |
| --- | --- | --- | --- | --- |
| 1 | `spec:core`, the 51-requirement Z3 tier with a five-variable state model and an unbounded-reachability check, runs as `node ~/workplace/symspec/packages/symspec/dist/cli.mjs` — an absolute path into one developer's home directory. It is in neither `check`'s `depends` nor CI. The strongest claim the project makes about itself is verified on one machine, last recorded green 2026-08-08. The remedy lives outside this repo: symspec v5 is published nowhere, and the 0.1.0 package on npm cannot parse `core.symspec.json` at all. | missing tests | L | `mise.toml:209-227`, `mise.toml:292-301`, `.github/workflows/ci.yml:377-385` |
| 2 | The `model/` crate's verification engine is `stateright = "0.31"`, resolving to 0.31.0 — pre-1.0 by its own book, with no upstream release in roughly thirteen months. Not deprecated and no successor exists, so this is dependency liveness rather than a defect: a load-bearing proof tier sits on an unmaintained-looking crate with no migration target. Cost is L because the only real remedies are vendoring or replacing the engine. | version pin | L | `model/Cargo.toml:10` |
| 3 | The Node binding has no lockfile of any kind — no `package-lock.json`, `pnpm-lock.yaml`, or `yarn.lock` exists anywhere in the tree. Its one devDependency is a caret range, and CI does not read that manifest at all: it runs `npx -y -p @napi-rs/cli@3`, a floating major with auto-install, inside the job that builds a shipped artifact. The Rust side commits `Cargo.lock` and the repo treats a committed lockfile as a supply-chain control. Dependabot watches npm weekly, but a caret range with no lockfile leaves it nothing to pin below 4.0.0. | version pin | M | `microvms-js/package.json:18`, `.github/workflows/ci.yml:343`, `.github/dependabot.yml:21-23` |
| 4 | `cargo test --all` runs zero tests over the two binding crates. Measured this session: `cargo test -p microvms-py -p microvms-js` builds three test binaries and reports `running 0 tests` for each, against 7,616 lines of `src/`. The workflow states it — "`cargo test --workspace` reports 0 tests for these crates" — and the coverage is real but CI-only, under pytest and `node:test`. So the local definition of done compiles the bindings and asserts nothing about them. | missing tests | M | `.github/workflows/ci.yml:299-301`, `mise.toml:146-148`, `.github/workflows/ci.yml:302-344` |
| 5 | The committed `Cargo.lock` at `5e6f752` pins `h2` 0.4.15, which carries a RUSTSEC advisory patched in 0.4.16. `cargo deny check` runs in `mise run security` and in CI, so the gate is red at HEAD; it is green here only because of an uncommitted `cargo update -p h2` → 0.4.19 in the working tree. That update also moved `windows-sys` 0.61.2 → 0.52.0 in three places, a transitive downgrade nobody asked for — evidence that a targeted lockfile bump is not a targeted change. | version pin | S | `Cargo.lock:1576-1577`, `mise.toml:115`, `.github/workflows/ci.yml:173-177` |
| 6 | A stale rationale is holding a test off a working code path. `exit_codes.rs` states in the present tense that core's `aws-config` is pinned `default-features = false`, that `ControlPlane::new` therefore panics with "a http_client is required", that the classified `ERR_CREDENTIALS` path is "currently unreachable", and closes "Restore the credential version once core's manifest is fixed." The manifest is fixed: `default-https-client` is in the feature list and the manifest comment records the repair, and the regression test exists. So a security-adjacent exit row has unit classification coverage and no process-level coverage, for a reason that no longer holds. | missing tests | S | `microvms-cli/tests/exit_codes.rs:138-149`, `microvms-cli/tests/exit_codes.rs:18`, `microvms-core/Cargo.toml:52-64`, `microvms-core/src/control/transport.rs:906` |
| 7 | *Deliberate.* There is no `Sandbox::attach`, so `suspend`, `resume`, and `terminate` address the control plane directly rather than through the type `run` and `build` use. Refused on a stated ground: an attach constructor would manufacture a second initial state that neither `spec/core.symspec.json`'s state model nor `model/src/client.rs`'s `init_states` enumerates, so both proof suites would silently stop covering it. The residual cost is that the attached lifecycle path lies outside both verification surfaces, and the module lists in full what it gives up. | wrong abstraction | L | `microvms-cli/src/commands/lifecycle.rs:15-54`, `model/src/client.rs`, `spec/core.symspec.json` |
| 8 | *Deliberate.* Every platform claim rests on one architecture and one region. `aarch64-unknown-linux-musl` is the shipping target because Lambda MicroVMs are ARM64-only, `agentd`'s tiers run on ubuntu only because the guest is a Unix process model, and the strategy memo says coverage "is still one region" with the adoption-measurement half of its publish action open. Correct given the substrate; the cost is that a platform-behaviour change outside us-east-1 is invisible to every tier. | missing tests | L | `rust-toolchain.toml:10-12`, `.github/workflows/ci.yml:56-62`, `docs/STRATEGY.md:106-115` |
| 9 | `mise run check` has no vulnerability tier. Its `depends` list is `lint, security, test, schema:check, stubs:check, model:check, live:check, build`; `vuln` — grype, trivy, osv-scanner — is absent, and those three are the scanners `deny.toml` names as the owners of advisory scanning. So the only local CVE coverage is whatever `cargo deny` incidentally provides, and the delegated lane runs in CI or on a manual invocation. | missing tests | S | `mise.toml:292-301`, `mise.toml:131-144`, `deny.toml:50-55` |
| 10 | *Deliberate.* Every identity-repair failure in the daemon is logged and then ignored, because the daemon is the only channel into the VM and refusing to serve would strand a VM with work in it. The module lists five things it cannot do, including a `boot_id` bind mount that needs `CAP_SYS_ADMIN` and is refused outright in a container that did not ask for it. The accepted consequence is named in the strategy memo: an unrepaired identity produces "VM-generated keys that repeat across sandboxes, which is a security bug rather than a performance regression." | error handling | M | `agentd/src/identity.rs:27-49`, `agentd/src/identity.rs:51-55`, `docs/STRATEGY.md:86-94` |
| 11 | `mise.toml` floats twelve tool versions to `latest` — `uv`, `ruff`, `semgrep`, `betterleaks`, `syft`, `grype`, `trivy`, `osv-scanner`, `terraform`, `lefthook`, `cargo-deny`, `actionlint` — while CI installs the same scanners at an exact version verified by `sha256sum -c -`. The local gate and the CI gate share a name and run different binaries, so a finding can appear in one and not the other, and neither result reproduces later. `rust = "stable"` floats too, with its reason recorded. | version pin | M | `mise.toml:20-33`, `.github/workflows/ci.yml:150-151`, `.github/workflows/ci.yml:201-206`, `.github/workflows/ci.yml:227-228` |
| 12 | `deny.toml`'s `[advisories]` block asserts a delegation its own config version cannot express: "Advisory scanning is grype/trivy/osv-scanner's lane … Running RustSec here too would give findings a fourth place to be silenced." Under `version = 2` cargo-deny has no key that turns vulnerability scanning off — the v1 `vulnerability = "allow"` field was removed. Verified this session with cargo-deny 0.20.2: `cargo deny check advisories` reports `advisories ok` and exits 0, so the check is live. That mismatch is why row 5's advisory surfaced as a `cargo deny` failure rather than as a scanner finding. | deprecated pattern | S | `deny.toml:50-57`, `mise.toml:115` |
| 13 | maturin's version is pinned at three sites that must move together, and the next release breaks the gate. The stub generator pins `maturin@1.14.1` exactly, CI installs `uvx maturin@1.14` twice (a minor pin that floats the patch), and `pyproject.toml` names the exact patch a fourth time in prose. maturin 1.15.0 moved `generate-stubs` output into the module's package directory, so the pin is load-bearing: bumping it breaks `mise run stubs:check`. The pin is correct; the debt is that it is a manual tracking obligation with no alarm, and neither maturin nor `pyo3-stub-gen` offers a check mode. | version pin | S | `scripts/generate-py-stubs.py:84-89`, `.github/workflows/ci.yml:306`, `.github/workflows/ci.yml:337`, `microvms-py/pyproject.toml:36-42` |
| 14 | The two bindings are enforced asymmetrically over one core. `microvms-py/microvms.pyi` and `py.typed` are committed and gated by `stubs:check` inside `check`, and a typed consumer is checked against the built wheel under `ty@0.0.72`. `microvms-js/index.d.ts` is gitignored, no `tsconfig.json` exists anywhere in the tree, and no gate reads the declarations — yet `package.json` still advertises `"types": "index.d.ts"` to downstream consumers. Not a staleness row, since `napi build` regenerates the file every run; what is unverified is whether the generated types are *usable*. The fix is one typed consumer, not a diff gate. | missing tests | S | `.gitignore:23-29`, `microvms-js/package.json:7`, `mise.toml:179-195`, `.github/workflows/ci.yml:335-340` |
| 15 | The daemon's default port, 9000, is written down three times and no assertion connects them. `microvms-core` declares `DEFAULT_AGENT_PORT` twice — once in `control/`, once in `session/` — each documented as matching the daemon, and `agentd` writes the literal itself in `Config::default`. The two tests that touch these constants each compare a value to its *own* module's constant, so both stay green if either moves alone. This is the exact shape the repo's own lesson names ("a comment explaining a number in terms of a value owned elsewhere"), and the repo has already invented the remedy: a compile-time `const` block that makes disagreement a build error. | duplicated logic | S | `microvms-core/src/control/mod.rs:96-97`, `microvms-core/src/session/proxy.rs:82-83`, `agentd/src/config.rs:84`, `microvms-cli/src/commands/attached.rs:1148-1160` |
| 16 | The two bindings' error contracts diverge, and nothing checks that the divergence stays where it is. napi types the async path over its own closed `Status` enum, so a custom `ERR_*` code survives a synchronous return and collapses to `GenericFailure` through a Promise rejection — measured with a probe addon. Nearly every method is async, so the Node rule is `err.cause.message` while the Python binding's `.code` is reliable everywhere. Upstream-forced and thoroughly documented; the debt is that each suite asserts only its own contract, so no gate would catch the two surfaces drifting apart. | duplicated logic | M | `microvms-js/src/errors.rs:10-45`, `.erpaval/solutions/api-patterns/napi-async-collapses-error-codes.md:11-17`, `microvms-py/tests/test_errors.py`, `microvms-js/__test__/errors.mjs` |
| 17 | The guard over the CLI's dependency allowlist asserts `reason.len() > 25` and nothing else about any reason string, under a comment claiming "a new one cannot be added silently." Length is not meaning: the guard catches an empty justification and cannot catch one that has stopped being true. That is the enforcement ceiling for every prose-accepted debt in this register, and this repo has already shipped one reason that went stale under it. | missing tests | S | `microvms-cli/tests/thinness.rs:212-218`, `microvms-cli/tests/thinness.rs:53`, `microvms-cli/tests/thinness.rs:66` |
| 18 | *Deliberate.* `microvm logs` names an image's build log group and refuses to read it, exiting with an `aws logs` invocation instead. Refused on three grounds, the decisive one being that the transport is single-service by construction — a `const` signing name and one `endpoint_for(region)` — so a CloudWatch reader in core would give the CLI a second path to AWS, which is what the thinness guard exists to forbid. | wrong abstraction | M | `microvms-cli/src/commands/local.rs:83-100`, `microvms-cli/tests/thinness.rs:212-218` |
| 19 | Action pinning is inconsistent inside one file. Two actions are pinned to a commit SHA with the version in a trailing comment; every other `uses:` across both workflows is a mutable tag, including `aquasecurity/trivy-action@v0.36.0` inside the job whose entire output is supply-chain assurance. The same job hash-verifies four downloaded binaries, so the discipline exists and stops at the action boundary. | version pin | S | `.github/workflows/ci.yml:174`, `.github/workflows/ci.yml:181`, `.github/workflows/ci.yml:216` |
| 20 | Three rationale blocks describe code that is no longer there. The action-version header names `checkout@v5`, `upload-artifact@v6`, and `setup-node@v5` and argues at length that "checkout is on v5 rather than v7 … v5 is the smallest version that satisfies the actual requirement" — the file uses `@v7` for all three, so the paragraph argues against the line below it. A CI comment counts "the CLI's five test targets" where `microvms-cli/tests/` holds four. The live workflow says the suite "reports the 34 checks it cannot express as SKIP", which the suite's own source contradicts by marking that list permanently empty. | dead code adjacent | S | `.github/workflows/ci.yml:29-39`, `.github/workflows/ci.yml:94`, `.github/workflows/live-conformance.yml:94`, `conformance/run_rs.py:428-432` |
| 21 | The shipping binary is linked two different ways and only one of them is ever proven per environment. `.cargo/config.toml` selects `linker = "rust-lld"` for the aarch64-musl target, with a comment explaining that the alternative fails naming a missing `cc`. CI installs `gcc-aarch64-linux-gnu` and overrides the choice through `CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER`, and the environment variable wins. So the cross-build gate never exercises the configuration the repo ships, and the local build never exercises CI's. | missing tests | S | `.cargo/config.toml:1-10`, `.github/workflows/ci.yml:358-365` |
| 22 | *Deliberate.* The conformance suite keeps `Results.skipped` and its `skip()` primitive with no live caller — the list is documented as permanently empty and the primitive is exercised only by the offline self-test probe. Retained on the argument that a suite which removed its own ability to report a gap is a suite whose next gap is silent. Real dead code with a stated reason to stay. | dead code adjacent | S | `conformance/run_rs.py:424-437`, `conformance/run_rs.py:497-511`, `conformance/run_rs.py:2159` |

## Explicit markers

There are no explicit markers in this tree. This section records the absence, the command that
establishes it, and the convention that occupies the space a marker would.

The grep, verbatim:

```
grep -rnE '\b(TODO|FIXME|HACK|XXX)\b' --include='*.rs' --include='*.py' .
```

Zero lines. Widened to every tracked file type — excluding `.git`, `target`, `.codegraph`,
`node_modules`, and `.venv` — the only two hits are prose *about* the absence. A
case-insensitive sweep over `.rs` and `.py` returns exactly one line, and it is not a marker:

- `` shared.publish(StreamKind::Stdout, b"xxxxxxxx").await; `` — `agentd/src/exec.rs:3234`

In place of markers, debt is accepted at the site under a doc-comment heading that names the
decision. There are 345 such headings, carried by 77 of the 90 tracked `.rs` files. Quoted
verbatim, the ones whose subject is a refusal, a limit, or an accepted cost:

- `` //! # There is no `Sandbox::attach`, and adding one would cost the proofs `` — `microvms-cli/src/commands/lifecycle.rs:15`
- `//! # What this module cannot do, honestly` — `agentd/src/identity.rs:27`
- `//! # Why a failure is never fatal` — `agentd/src/identity.rs:51`
- `/// # The command choice is constrained by a core defect, and that is recorded rather than hidden` — `microvms-cli/tests/exit_codes.rs:138`
- `/// # Adding a reader to core was assessed and refused, and not on grounds of size` — `microvms-cli/src/commands/local.rs:92`
- `/// # Why this fails rather than returning an empty list` — `microvms-cli/src/commands/local.rs:85`
- `//! # A dollar string's trailing zeros may differ from the Python oracle's, and that is accepted` — `microvms-cli/src/render.rs:27`
- `//! # Runtime-checked rather than typestate, deliberately` — `microvms-core/src/sandbox.rs:19`
- `//! # Where the code lands, and why it is in two places rather than one` — `microvms-js/src/errors.rs:10`
- `/// # Output arrives as text plus a byte count, not as base64, and that is a deliberate limit` — `microvms-cli/src/commands/attached.rs:331`
- `//! # TRAP-11: what is deliberately absent` — `microvms-core/src/control/connector.rs:16`
- `/// # What is deliberately not here` — `microvms-core/src/control/mod.rs:261`
- `//! # What is deliberately *not* an option` — `microvms-cli/src/cli.rs:21`
- `//! # Hand-rolled rather than a dependency` — `microvms-core/src/session/sse.rs:16`

Suppression files follow the same rule, stated in one of them:

- `# Accepted findings, each with its reason. An ignore without a reason is a` / `# finding someone silenced; an ignore with one is a decision someone made.` — `.trivyignore.yaml:2-3`
- `reason = "optional rust_decimal feature, never enabled; not compiled into any artifact"` — `osv-scanner.toml:13`

Declined scope is written the same way, at document level rather than at a call site:

- `**Not an orchestrator.**` — `docs/STRATEGY.md:136`
- `**Not fork.**` — `docs/STRATEGY.md:127`
- `**Not a turn-boundary suspend protocol.**` — `docs/STRATEGY.md:119`
- `**Not AgentCore parity on exec and PTY.**` — `docs/STRATEGY.md:133`
- `> **Implemented. This document is kept as history because the reasoning still` / `> applies; the numbers below are outdated.**` — `docs/CLI-COVERAGE-PLAN.md:3-4`

One consequence of the convention is worth stating plainly: an acceptance paragraph is
searchable only if you know the phrasing, whereas `TODO` is one token. Fourteen `#[allow(`
attributes exist in the tracked Rust tree and every one carries a `reason =` string arguing its
specific case, which is the convention working. Row 20 above is the convention failing, and
nothing distinguishes the two states without reading both the prose and the code.

## Pattern-level smells

### The enforcement stops exactly at the crate boundary the project most wants adopted

Every asymmetry in this register lands on the same two crates. The bindings are the surface the
strategy memo exists to make adoptable, and they are the least-gated code in the workspace.
`cargo test --all` runs zero tests against 7,616 lines of them. The Node side has no lockfile,
advertises type declarations no gate reads, and gets its build toolchain from a floating major
installed by `npx -y`. Neither binding's manifest is read by any test, so a dependency either
could re-add — the CLI has a named guard against exactly this — would fail nothing. The Python
side is the counter-example and shows the shape of the fix: a committed stub, a `py.typed`
marker, a regenerate-and-diff gate inside `check`, and a typed consumer checked from outside the
repo's own layout against a built wheel. That is four mechanisms on one binding and roughly zero
on its twin, over one shared core.

Shows up in:

- `.github/workflows/ci.yml:299-301` — the workspace test runner reporting zero tests for both crates, stated in the file
- `microvms-js/package.json:18` and `.github/workflows/ci.yml:343` — a caret range with no lockfile, and CI bypassing the manifest entirely
- `.gitignore:23-29` and `microvms-js/package.json:7` — declarations advertised to consumers, gitignored, and read by nothing
- `mise.toml:179-195` and `.github/workflows/ci.yml:335-340` — the four mechanisms the Python side has
- `microvms-cli/tests/thinness.rs:53` — a manifest guard that exists for the CLI and has no binding equivalent

Cost: M. The cheapest single item is one `tsconfig.json` and one typed `.mts` consumer, which
closes row 14. A committed lockfile for `microvms-js` plus dropping `-y -p @napi-rs/cli@3` in
favour of the manifest closes row 3 and is nearly as cheap.

### Debt is accepted in prose, and prose is the only thing enforcing it

The acceptance-paragraph convention is genuinely better than a `TODO`: each paragraph names the
cost, what would have to change for the answer to flip, and often where the counter-argument
lives. What almost none of them have is a test that goes red when the acceptance stops being
true. The measurable ceiling is one assertion: the dependency-allowlist guard checks
`reason.len() > 25` and nothing else, under a comment claiming a new entry "cannot be added
silently." Length is not meaning. Three sites in this register prove the mechanism fails in
practice — an action-version paragraph arguing against the line below it, a test comment
deferring work on a defect that has been repaired, and a workflow describing SKIP behaviour the
suite's own source marks permanently empty. The pattern is not that the reasoning is wrong. It is
that prose degrades silently while code does not, and this repo has already built the fix
elsewhere: a compile-time `const` block, an exhaustive-match round-trip, a named `RETIRED` array
whose failure message points at the replacement API.

Shows up in:

- `microvms-cli/tests/thinness.rs:212-218` — `reason.len() > 25`, the only assertion over any acceptance reason
- `microvms-cli/tests/exit_codes.rs:138-149` against `microvms-core/Cargo.toml:52-64` — a rationale outliving the defect it describes, with a test still deferred on it
- `.github/workflows/ci.yml:29-39` against `.github/workflows/ci.yml:69` — thirty lines arguing for a version the next line does not use
- `microvms-cli/src/commands/attached.rs:1148-1160` — the remedy the repo already invented: a `const` block, so disagreement is a build error rather than a test failure
- `microvms-cli/tests/thinness.rs:53` — the other remedy: a named retirement whose failure message carries the replacement

Cost: M per site, and the work is adding one assertion beside an existing paragraph rather than
changing behaviour.

### The local gate and the CI gate share one name and are two different gates

`mise run check` is documented as the definition of done, and it is not a subset or a superset of
CI — the two overlap partially and each holds ground the other does not. `check` omits the
vulnerability tier entirely, so grype, trivy, and osv-scanner never run locally even though
`deny.toml` names them as the owners of advisory scanning. `check` also omits the bindings, whose
only tests live in a CI job. Going the other way, `check` runs `live:check`, `model:check`, and
`stubs:check`. Where both do run the same gate, they run different binaries: `mise` resolves
twelve tools including every scanner to `latest`, while CI installs the same scanners at exact
versions verified against a SHA-256. Even the shipping binary is linked differently — the repo
selects `rust-lld` and CI overrides it with `aarch64-linux-gnu-gcc` through an environment
variable that takes precedence. A contributor who runs `check` green and a reviewer who reads a
green CI badge are looking at two different claims.

Shows up in:

- `mise.toml:292-301` — `check`'s `depends`, with no `vuln` and no binding suite
- `mise.toml:20-33` against `.github/workflows/ci.yml:201-206` — `latest` locally, exact version plus hash in CI, for the same scanners
- `.cargo/config.toml:8-10` against `.github/workflows/ci.yml:358-365` — two linkers for one artifact, the env var winning
- `deny.toml:50-55` — advisory scanning delegated to three scanners that `check` does not run
- `.github/workflows/ci.yml:302-344` — the binding tier that exists only here

Cost: M. Adding `vuln` to `check` is one line and would make the local gate slower and honest;
reconciling the linker choice is a comment or a removed environment variable. Reconciling the
tool versions is the expensive half, because pinning `mise` tools reintroduces the manual refresh
obligation row 13 already carries.

### One number, written down in several places, guarded in one of them

The repo has diagnosed this class itself and written down the detection method: a comment
explaining a number in terms of a value owned elsewhere is the signature, and the greps are
"twice the", "four times", "matching the", "same as the daemon's". Two pairs found that way got
guards — the Dockerfile `AGENTD_PORT` against the client's port, and the SSE keepalive interval
against the client's stream idle timeout, the latter refusing equality as well as excess. Running
the same grep now still returns hits. The clearest is the default port itself: `microvms-core`
declares `DEFAULT_AGENT_PORT = 9000` in two separate modules, each doc'd as matching the daemon,
and `agentd` writes the literal a third time in `Config::default`. The two tests that mention
these constants each compare a value to its own module's constant, so both stay green if either
moves alone. Nothing anywhere compares core's number to the daemon's.

Shows up in:

- `microvms-core/src/control/mod.rs:96-97` and `microvms-core/src/session/proxy.rs:82-83` — the same constant declared twice in one crate
- `agentd/src/config.rs:84` — the third copy, as a bare literal in the type that owns the truth
- `microvms-core/src/control/artifact.rs:395-430` — the pair that *was* guarded, with equality refused and the failure naming both numbers
- `microvms-cli/src/commands/attached.rs:1148-1160` — the strongest available remedy, a compile-time bound rather than a runtime assertion
- `.erpaval/solutions/architecture-patterns/an-absent-value-is-not-a-neutral-one.md:39-48` — the repo's own detection method, which found the last two and has not been re-run

Cost: S. One `const` block asserting the two core constants against each other, and one test
comparing core's default to `agentd`'s `Config::default()`.

### The strongest verification tiers are the least reachable, and the newest one is unmaintained upstream

This project verifies itself unusually hard: a stateright model over every reachable state,
proptest confinement properties, turmoil fault simulation, a panic guard, a schema-artifact
check, and two Z3-backed symspec requirement documents. Reachability is inversely correlated with
strength. The tiers inside `cargo test` run everywhere. The six-requirement daemon spec needs a
global npm install plus a downloaded embedding model and sits outside `check`. The 51-requirement
core spec — the only one with a state model and an unbounded-reachability tier — runs from an
absolute path inside one developer's home directory, is in neither `check` nor CI, and its CI job
was deleted rather than pointed at the registry package, on the sound argument that a green job
labelled "requirements" verifying six of fifty-seven is worse than no job. Underneath the tier
that *is* always reachable, the engine is pre-1.0 with no upstream release in thirteen months.
Both exclusions are argued well and both leave the same hole: the claims this project most wants
believed are checked by one person on one machine, or by a crate with no maintenance signal.

Shows up in:

- `mise.toml:209-227` — the absolute home-directory path, the cooperative-cancellation timeout, and the "verified 2026-08-08" note
- `mise.toml:197-207` — the 0.1.0 task, scoped to six requirements, also outside `check`
- `mise.toml:292-301` — `check`'s `depends`, with neither `spec` nor `spec:core`
- `.github/workflows/ci.yml:377-385` — the deleted job and the stated condition for its return
- `model/Cargo.toml:10` — the engine, pinned at a pre-1.0 version with no successor

Cost: L. The remedy for the spec tier lives outside this repo — publishing symspec v5 to a
registry, or vendoring its `dist/cli.mjs` — and the owner's stated preference is to publish. The
remedy for the engine is to keep watching it, since vendoring or replacing a model checker is a
larger project than the tier it serves.

## See also

- [impact analysis](impact-analysis.md) — 16 shared source citations
- [contract map](contract-map.md) — 13 shared source citations
- [business logic](business-logic.md) — 11 shared source citations
- [debugging guide](debugging-guide.md) — 11 shared source citations
- [processes](../behavior/processes.md) — 10 shared source citations
