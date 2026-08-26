# CLAUDE.md

Verified client stack + in-VM daemon for AWS Lambda MicroVMs, in Rust. Source-only:
nothing publishes to crates.io/PyPI/npm. Read `docs/PLATFORM.md` (measured platform
findings), `docs/PROTOCOL.md` (wire contract — never change silently), `docs/TRUST.md`
(threat model), `docs/STRATEGY.md` (scope; orchestrators and fork-snapshots are declined).

## Commands

`mise` is the front door.

```bash
mise run install     # once per clone: git hooks (lefthook)
mise run check       # THE definition of done: lint, security, test, schema:check,
                     #   stubs:check, model:check, live:check, build. Offline, free.
mise run live        # real-AWS conformance + rates + leak check. BILLABLE (~15 min).
                     #   Never run casually; never wire to a hook.
mise tasks           # everything else
```

Never pipe a gate into `head` or `tail`. The pipeline exits with the pager's status, so a
failing tier reads as success — `mise run check | tail` returns 0 while `[security] ERROR
task failed` scrolls past. Run it bare, or read `${PIPESTATUS[0]}`.

Two names in that list mean less than they look like:

- `model:check` asserts hardcoded API constants still match botocore's service model. It
  does not touch the stateright models — those run as `cargo test -p agentd-model`.
- `check` has no vulnerability tier: `vuln` (grype/trivy/osv-scanner) is not in its
  `depends`. Advisories still fail the gate through `cargo deny check` inside `security`,
  which is how a transitive RUSTSEC hit surfaces. Fix those with `cargo update -p <crate>`
  and read the resulting `Cargo.lock` diff before committing — a targeted bump in this
  workspace also re-resolves neighbouring edges.

Individual tiers:

```bash
cargo test --all                             # all six Rust tiers
cargo test -p agentd --lib                   # daemon unit tests
cargo test -p agentd-model                   # stateright model checking
cargo test --test proptest_tar               # tar confinement properties
cargo test --test turmoil_transport          # simulated network/time faults
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo run -p agentd --bin schema -- --check  # docs/schema.json freshness
cargo build --release -p agentd --target aarch64-unknown-linux-musl  # shipping target
uvx ruff check . && uvx ruff format --check .  # selection lives in ruff.toml
./scripts/check-lint-coverage.py             # proves that selection is not empty
./conformance/run_rs.py --self-test          # live suite's offline half; free
./scripts/check-live-rates.py --twin-only    # pinned rate tables agree; offline
```

Pass ruff a `.`, never a directory list. It discovers by extension, so naming
`conformance scripts` silently lints `run_rs.py` and nothing else — the extensionless
PEP 723 gates in `scripts/` are invisible to that form, which is why the selection lives in
`ruff.toml` and `check-lint-coverage.py` exists to prove the walk found something.

Bindings: `microvms-py` builds via maturin (`maturin develop --uv`, pytest in
`microvms-py/tests/`); `microvms-js` via `npm run build` / `npm test` in `microvms-js/`
(napi-rs, Node >= 22.13).

## Requirements workflow (symspec, EARS)

`spec/core.symspec.json` — 51 formal requirements for microvms-core, with a state model
and three lifecycle invariants proved in Z3 (`mise run spec:core`; uses the v5 CLI by
absolute path, not the `symspec` on PATH). `spec/agentd.symspec.json` — daemon
bootstrap/control-token requirements (`mise run spec`).

Both spec tasks sit outside `mise run check` and both need a working environment before
they say anything: `spec:core`'s runner is an absolute path into a developer's home
directory, so it is `MODULE_NOT_FOUND` on a machine that lacks that checkout, and
`spec/agentd.symspec.json` declares `schemaVersion` (document format v2) against a CLI that
requires `docVersion` (v3), which fails `ERR_SCHEMA_VERSION` with no read-compatibility.
Treat a green `check` as saying nothing about the 57 requirements, and reach for
`cargo test -p agentd-model` for the claims that are runnable everywhere.

Per CONTRIBUTING.md:

```bash
npm install -g symspec && symspec download-model
symspec check spec/agentd.symspec.json --strict
```

`--strict` fails when a claim could NOT be verified, not only when disproven. New
requirements sharing vocabulary with no peer need a glossary link, not a looser gate.

## Layout

- `protocol/` — daemon<->client wire types; drift is a compile error
- `agentd/` — the in-VM daemon (exec, file transfer, one-shot bootstrap)
- `model/` — stateright models of daemon and client lifecycle
- `microvms-core/` — the client library; the type system carries every trap closure
- `microvms-cli/` — the `microvm` binary: 17 commands, JSON envelopes, `manifest`.
  No lib target; allowlisted deps (6), asserted by `tests/thinness.rs`. The count is
  compile-enforced by `RESPONSE_TYPES: [_; 17]` in `src/commands/mod.rs`.
- `microvms-py/`, `microvms-js/` — thin PyO3 / napi-rs bindings over core. Only the
  Python side has a drift gate: `microvms.pyi` is checked by `stubs:check`, while
  `microvms-js/index.d.ts` is gitignored and nothing compares it to the crate.
- `conformance/` — `run_rs.py`, the live suite (billable); `--self-test` is free. Read the
  check count off a run's summary block, which derives it, rather than from any prose.
- `spec/` — formal requirements
- `docs/` — hand-written and authoritative at the top level (`PROTOCOL`, `PLATFORM`,
  `TRUST`, `STRATEGY`, `EMBEDDING`, `HARNESS-CAPABILITIES`); generated under
  `architecture/`, `reference/`, `behavior/`, `analysis/`, `diagrams/`, `insights/`, every
  claim carrying a machine-verified `path:line` citation. `docs/README.md` is the index.
  Those citations anchor to line numbers, so a refactor silently aims them at the wrong
  code while they still read as authoritative — regenerate the affected files instead of
  editing a stale one, and let the hand-written document win any disagreement.

Dependency direction: cli -> core -> protocol; bindings -> core; agentd -> protocol.

## Rules

- A new guard test is not done until you have watched it fail: break the invariant,
  see YOUR test go red, restore, state in the PR which break it caught.
- Platform claims in `docs/PLATFORM.md` need a date, a region, and an API version;
  contradictions are appended, never deleted.
- After any live run, verify teardown independently (`mise run live:verify-clean`);
  service-created log groups under `/aws/lambda-microvms/` outlive `terraform destroy`.
- Comments record constraints and defects defended against, never narration.
- `.erpaval/` holds ERPAVal session packets (`sessions/`), specs, and compounded
  lessons (`solutions/`) — consult solutions before re-deriving a fix.
- No `.rs` or `.py` file carries a `TODO` / `HACK` / `FIXME` marker, so grepping for them
  finds nothing and proves nothing. The debt register is `docs/insights/tech-debt.md`,
  declined scope is `docs/STRATEGY.md`, and defect classes already paid for are
  `.erpaval/solutions/`.
- `scripts/generate-py-stubs.py` pins `maturin@1.14.1` on purpose. Later maturin writes
  `generate-stubs` output into the module's package directory instead of `--out`, so
  bumping the pin breaks `stubs:check` rather than improving it.

## CodeGraph

A codegraph index exists (`.codegraph/`, gitignored); use `codegraph` for impact and
reference queries across the workspace. `codegraph explore` returns verbatim line-numbered
source plus blast radius and flags symbols with no covering tests, which beats grep for
anything structural. Subagents should invoke it by absolute path — the mise shim is not
always on a subshell's `PATH`.

Two edges where the index misleads, both worth knowing before you trust a count:

- `-k route` only sees `.route()` calls with a literal path argument. The daemon builds its
  routes from a schema, so a route query returns test fixtures, never the real 18-endpoint
  surface. Use `docs/schema.json` for that census.
- Symbol lookup is name-resolved, not crate-qualified. `Sandbox`, `Session`, and
  `Duration` each exist in several crates, so callers and coverage cross-attribute between
  them and can imply an edge the asserted dependency direction forbids. Rank with the
  counts, then confirm each edge from a `use` line or call site.
