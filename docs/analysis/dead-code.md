# microvms-agentd · Dead code

The compiler already owns most of this question. `rustc`'s `unused_imports` and `dead_code`
are warn-by-default, Rust 1.97 added `dead_code_pub_in_binary`, and
`cargo clippy --all-targets --all-features` over all seven workspace members exits 0 with
zero warnings — verified on a forced fresh re-check, not a warm cache, with every `.rs` file
in `agentd`, `microvms-cli`, `microvms-core`, `microvms-js`, `microvms-py`, `model`, and
`protocol` touched first. Nothing crate-private is dead, and no import is unused anywhere
`rustc` can see. Restating that would add nothing.

What the compiler cannot decide is what this file covers:

- A `pub` item in `microvms-core` with zero in-repo callers may still be live API. The crate
  has three consumers (`microvms-cli`, `microvms-py`, `microvms-js`), and `protocol` is
  consumed by `agentd` and `microvms-core`.
- Items reached only through `#[pyclass]` / `#[pymethods]` / `#[pyfunction]` / `#[pymodule]`
  or `#[napi]` are dispatched by CPython and Node, never by a Rust caller. They read as
  uncalled and are not dead. `#[derive(Serialize, Deserialize)]` impls, trait impls
  satisfying a bound, and `#[test]` functions read the same way.
- napi-rs renames snake_case Rust to camelCase JS, so a `#[napi]` export exercised only from
  `microvms-js/__test__/*.mjs` is invisible to a snake_case search.

Method: a symbol is a candidate when no edge of kind
`calls | references | instantiates | imports | implements | extends` targets it in the
CodeGraph index (`.codegraph/codegraph.db`, 4,430 nodes / 16,638 edges), and no textual
reference to it — or to its camelCase / `#[napi(js_name)]` / `#[pyo3(name)]` alias — exists
anywhere in the git-tracked tree. `contains` edges are excluded because every symbol is
`contains`-reachable from its own file node. The funnel: 1,398 zero-inbound Rust symbols →
551 after dropping test files and `#[cfg(test)]` modules → 465 after dropping two whole-file
test-only modules → 112 after the alias-aware reference search → 2 after removing 110 trait
and language-protocol members → **1** after hand-dropping a trait associated type
(`microvms-js/src/exec.rs:267`, `type Return = ()` inside
`impl AsyncGenerator for ExecStream`) that the automated filter missed because it matched
trait methods but not associated types. No dead-code analyzer is integrated in this repo
(no `cargo-udeps`, `cargo-machete`, `vulture`, or `knip` in `mise.toml`, `Cargo.toml`,
`deny.toml`, or `.github/workflows/`), so the index is the analyzer.

## Unreferenced exports

| Symbol | Path | Last modified |
| --- | --- | --- |
| `SessionBuilder::with_timeout` | `microvms-core/src/session/mod.rs:508` | 2026-08-15 |

**Confidence: high.** `git grep -n "with_timeout\|withTimeout" -- .` over the whole
git-tracked tree returns exactly one line — the declaration. Every sibling on the same
builder has a caller: `with_minter` (`microvms-core/src/session/mod.rs:479`) from
`microvms-cli/src/seam.rs`, `microvms-core/src/sandbox.rs`, and
`microvms-core/tests/turmoil_client.rs`; `with_proxy_auth`
(`microvms-core/src/session/mod.rs:487`) from `microvms-core/tests/turmoil_client.rs`;
`with_backend` (`microvms-core/src/session/mod.rs:494`) from `microvms-cli/src/guards.rs`
and `microvms-core/tests/turmoil_client.rs`; `with_port`
(`microvms-core/src/session/mod.rs:501`) from `microvms-cli/src/seam.rs` and
`microvms-core/src/sandbox.rs`.

The field it writes is live — only the setter is unreached. `SessionBuilder::build` reads
`self.timeout` at `microvms-core/src/session/mod.rs:517` and `:529`, and `Session::run`
back-fills a per-request `None` from it at `microvms-core/src/session/mod.rs:119-120`.
`Session::builder` is the sole constructor (`microvms-core/src/session/mod.rs:230-231`) and
seeds every path with `DEFAULT_REQUEST_TIMEOUT` (`microvms-core/src/session/mod.rs:237`).

**What would falsify this.** Three conditions hold; any one of them failing moves this row
out of the table.

1. No unseen downstream consumer. `microvms-core` is a library crate, so a `pub` method is
   reachable by anything that depends on it. `CLAUDE.md` states the workspace is
   source-only and nothing publishes to crates.io, PyPI, or npm, so there is no semver
   contract to honour.
2. No binding re-exports it. `microvms-py/microvms.pyi` contains no `with_timeout`, and no
   `withTimeout` exists in `microvms-js`.
3. No host runtime dispatches to it. Its only attribute is `#[must_use]` — no `#[napi]`,
   no `#[pymethods]`.

**Related defect in the same construct.** The builder's doc comment at
`microvms-core/src/session/mod.rs:229` reads "A builder, for the cases that need a port, a
timeout, or a custom backend." The port case and the backend case each have callers; the
timeout case has none. The comment asserts a motivating case that does not exist in the tree.

## Unreferenced files

_none_

All 200 git-tracked files resolve to an inbound reference. Every non-root `.rs` file has a
`mod` declaration or is cargo-auto-discovered by one of the patterns `*/src/lib.rs`,
`*/src/main.rs`, `*/build.rs`, `*/src/bin/*.rs`, `*/tests/*.rs`, `*/examples/*.rs`. Four
files that a basename search calls orphans, each cleared against its real invocation site:

| File | Reached by |
| --- | --- |
| `microvms-py/tests/test_stubs.py` | `pytest microvms-py/tests -q` at `.github/workflows/ci.yml:308`; pytest auto-discovers `test_*.py`, so no file names it |
| `microvms-js/__test__/support/decimal.mjs` | `microvms-js/__test__/cost.mjs:40` |
| `microvms-js/__test__/support/sse.mjs` | `microvms-js/__test__/cost.mjs:41`, `microvms-js/__test__/errors.mjs:40`, `microvms-js/__test__/exec.mjs:44`, `microvms-js/__test__/process.mjs:31`, `microvms-js/__test__/session.mjs:33` |
| `conformance/infra/main.tf` | `terraform -chdir=conformance/infra` at `mise.toml:53`, `mise.toml:93`, `mise.toml:307`, `mise.toml:538` |

Two files are compiled only under `cfg(test)` and are live test code, not dead source:
`microvms-cli/src/guards.rs` (inner `#![cfg(test)]` at `microvms-cli/src/guards.rs:20`, plus
`#[cfg(test)] mod guards;` at `microvms-cli/src/main.rs:36-37`) and
`microvms-core/src/control/fake.rs` (`#[cfg(test)] pub(crate) mod fake;` at
`microvms-core/src/control/mod.rs:928-929`).

## Dead imports

| Path | Symbol | Imported from |
| --- | --- | --- |
| `microvms-cli/src/commands/lifecycle.rs:1168` | `_DocsOnly` (alias of `ControlPlane`) | `microvms_core::control::ControlPlane`, re-bound from `microvms-cli/src/commands/lifecycle.rs:70` |
| `microvms-cli/src/commands/attached.rs:932` | `_DocsOnly` (alias of `ErrorKind`) | `microvms_core::ErrorKind`, re-bound from `microvms-cli/src/commands/attached.rs:40` |

**Confidence: high that nothing names `_DocsOnly`; do not delete either line on its own.**
Both carry `#[allow(unused_imports, reason = …)]`, so `rustc` never reports them, and both
are the only `allow` attributes of this family in the workspace. Neither is independently
removable. Measured by copying the git-tracked tree to a scratch directory, deleting each
three-line construct (doc comment, attribute, `use`), and rebuilding:

- `cargo clippy -p microvms-cli --all-targets` emits two new warnings —
  `unused import: ControlPlane` and `unused import: ErrorKind` — because the `_DocsOnly`
  re-export is what consumes the code-level import at
  `microvms-cli/src/commands/lifecycle.rs:70` and
  `microvms-cli/src/commands/attached.rs:40`. Under `-D warnings` that is a build failure.
- `cargo doc --no-deps -p microvms-cli` emits the same eight warnings with or without the
  constructs, and neither `ControlPlane` nor `ErrorKind` appears among them. The stated
  reason — "Re-exported so `[ControlPlane]` is nameable in this module's docs"
  (`microvms-cli/src/commands/lifecycle.rs:1166`) — is not the mechanism. The intra-doc link
  at `microvms-cli/src/commands/lifecycle.rs:10` resolves from the `:70` import directly.

The two differ in whether the whole construct earns its place:

- `ControlPlane` is a trait, so its name never appears in an expression; the `:70` import is
  what makes `[ControlPlane]` at `microvms-cli/src/commands/lifecycle.rs:10` resolve. Removing
  the pair means removing that doc link. Load-bearing as a unit.
- `ErrorKind` is named nowhere in `microvms-cli/src/commands/attached.rs` except its import
  at `:40` and the two `_DocsOnly` lines at `:930-932`. The only documentation that links
  `[ErrorKind]` is the doc comment justifying the import that makes it resolvable. Removable
  as a unit — `:40`'s `ErrorKind`, plus all three lines at `:930-932`.

**Non-Rust surfaces, both clean.** `uvx ruff check --select F401,F811,F841` over all 17
tracked `.py` files plus `microvms-py/microvms.pyi` reports no findings; F401 is in the
repo's own selected set (`ruff.toml`) and `mise.toml:91` runs `ruff check .` across the whole
repo. The eight `microvms-js/__test__/*.mjs` files have no linter, so their
`import … from '…'` bindings were checked directly for non-comment uses: zero unused.

## See also

- [impact analysis](../insights/impact-analysis.md) — 7 shared source citations
- [contract map](../insights/contract-map.md) — 6 shared source citations
- [processes](../behavior/processes.md) — 5 shared source citations
- [tech debt](../insights/tech-debt.md) — 5 shared source citations
- [risk hotspots](risk-hotspots.md) — 4 shared source citations
