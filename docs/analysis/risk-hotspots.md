# microvms-agentd · Risk hotspots

Risk here is composed from two measured signals, because the one the default recipe reaches
for is empty. Both gate-level static-analysis passes return zero findings on every file:
`cargo clippy --all-targets --message-format=json` emits 0 diagnostics at `warning` or
`error` level, and the semgrep pass the gate runs over all seven crates
(`mise.toml:112`) reports 0 results. So severity is measured instead as **test-tier reach**,
using CodeGraph's covering-test relation over the 4,430-node index. A symbol counts once, and
only if it carries at least one inbound `calls`/`references`/`instantiates` dependent and
sits outside any `#[cfg(test)]` region. **`error`** means no test file reaches it and its
containing file holds zero in-file `#[test]` functions — no test tier in this repository
reaches that symbol through a source-level edge. **`warn`** means no test file reaches it but
the file does carry an in-file `#[cfg(test)]` module, so a same-file unit test may while no
cross-file tier does. Trend is cross-sectional over the 30-day window: 78 live source files
were touched across 114 commits, per-file commit counts have median 4.0 and population σ
2.7928, so `↑ rising` is 7 or more commits (19 files), `→ flat` is 2 to 6 (58 files), and
`↓ falling` is 1 (1 file). The score is `2 × error + 0.5 × warn + 1 × (trend is rising)`,
ties broken by commit count descending. Totals across the 75 scored files: 278 error-class
and 654 warn-class findings.

Four limitations bound every number below. First, the repository's whole history — 114
commits, 2026-08-05 to 2026-08-17 — fits inside the 30-day window, so there is no earlier
baseline and the trend arrow compares a file against its peers, never against its own past.
Second, the covering-test relation counts test *files*, and Rust's dominant unit-test idiom
here is an in-file `#[cfg(test)] mod tests`; 770 such tests exist across
`microvms-core` (436), `microvms-cli` (172), `agentd` (130), `protocol` (17) and `model` (15),
and none of them can satisfy a cross-file rule. A `warn` count is therefore a statement about
tier reach, not about absence of testing — `microvms-core/src/sandbox.rs` returns an empty
`codegraph affected -d 1` result while holding 27 unit tests of its own. Third, and most
consequential for reading the table: **no source-level edge can cross the PyO3 or napi-rs FFI
boundary**, so the 159 pytest functions in `microvms-py/tests/` and the 168 `test(` calls in
`microvms-js/__test__/` cannot register as covering tests for the Rust code they exercise, and
CodeGraph does not classify a `.mjs` file as a test at all. Every `error` count in a bindings
file measures Rust-tier reach only; the residual risk is narrower than "untested" and is
named per file in the drill-down. The index also resolves references by name, so a
declaration whose name is short or collides with a widely used external type absorbs mentions
that are not references to it. `Duration` (`microvms-js/src/cost.rs:60`) is the clearest
instance: it is the only node of that name in the index, so every `Duration` in the workspace
resolves onto it, and it is credited with coverage from
`microvms-core/tests/turmoil_client.rs`, a file containing zero references to `microvms-js`.
`live` (`microvms-js/src/session.rs:274`) and `data` (`microvms-py/src/exec.rs:236`) are
likewise sole holders of their names. So per-symbol dependent counts below are what the index
records rather than verified call sites, and the direction of the bias on `error` counts is
toward understating them. Fourth, ownership carries no bus-factor information: one
human authors 87 of 114 commits under two identities and a `bgagent` bot the other 27, every
file has one or two distinct authors, and a `bgagent` top-owner share marks a file that
arrived inside a large squashed commit rather than one with a second maintainer. Two files
are excluded from scoring because they are test code that lives under `src/`:
`microvms-cli/src/guards.rs` (`#![cfg(test)]` at `microvms-cli/src/guards.rs:20`, declared at
`microvms-cli/src/main.rs:36-37`) and `microvms-core/src/control/fake.rs` (declared at
`microvms-core/src/control/mod.rs:928-929`). That exclusion matters: `guards.rs` is the
joint-highest-churn source file in the repository at 13 commits, so a churn-only ranking puts
a file that never ships in a binary at the top.

| File | Trend | Open findings | Top owner | Citation |
| --- | --- | --- | --- | --- |
| `microvms-py/src/cost.rs` | ↑ rising | 0 warn, 66 error | Laith Al-Saadoon 57% | `microvms-py/src/cost.rs` (1,123 LOC) |
| `microvms-js/src/cost.rs` | → flat | 0 warn, 65 error | Laith Al-Saadoon 60% | `microvms-js/src/cost.rs` (1,038 LOC) |
| `microvms-js/src/session.rs` | → flat | 0 warn, 25 error | Laith Al-Saadoon 60% | `microvms-js/src/session.rs` (609 LOC) |
| `microvms-core/src/cost.rs` | ↑ rising | 93 warn, 0 error | bgagent 71% | `microvms-core/src/cost.rs` (4,127 LOC) |
| `microvms-py/src/exec.rs` | ↑ rising | 0 warn, 22 error | Laith Al-Saadoon 71% | `microvms-py/src/exec.rs` (631 LOC) |
| `microvms-js/src/exec.rs` | → flat | 0 warn, 19 error | Laith Al-Saadoon 60% | `microvms-js/src/exec.rs` (456 LOC) |
| `microvms-js/src/sandbox.rs` | → flat | 0 warn, 16 error | Laith Al-Saadoon 60% | `microvms-js/src/sandbox.rs` (623 LOC) |
| `microvms-js/src/process.rs` | ↓ falling | 0 warn, 12 error | Laith Al-Saadoon 100% | `microvms-js/src/process.rs` (541 LOC) |
| `microvms-py/src/session.rs` | → flat | 0 warn, 11 error | Laith Al-Saadoon 67% | `microvms-py/src/session.rs` (605 LOC) |
| `microvms-core/src/sandbox.rs` | ↑ rising | 40 warn, 0 error | Laith Al-Saadoon 73% | `microvms-core/src/sandbox.rs` (2,371 LOC) |
| `agentd/src/fs.rs` | ↑ rising | 38 warn, 0 error | bgagent 57% | `agentd/src/fs.rs` (2,628 LOC) |
| `microvms-py/src/sandbox.rs` | → flat | 0 warn, 10 error | Laith Al-Saadoon 67% | `microvms-py/src/sandbox.rs` (780 LOC) |

The shape of that list is the finding. Nine of the twelve rows are binding files, and the
reason is structural rather than per-file: all 18 files under `microvms-py/src/` and
`microvms-js/src/` — 3,856 and 3,773 LOC respectively — contain zero `#[cfg(test)]` modules
and zero `#[test]` functions, while the other five crates hold 770 between them — an average
of 19 per file in `microvms-core`, 11 in `microvms-cli`, 10 in `agentd`. The three
non-binding rows (`microvms-core/src/cost.rs`, `microvms-core/src/sandbox.rs`,
`agentd/src/fs.rs`) are the opposite case: heavily unit-tested files whose churn is rising
and whose public surface no cross-file tier reaches.

## Per-file drill-down

### 1. `microvms-py/src/cost.rs`

**What's there.** The PyO3 mirror of the cost engine, whose stated job is wrapping the core
types "without loosening any of them (BIND-5)" by making the unsafe spellings absent rather
than rejected — no `__float__`, `__int__`, `__index__`, `__add__` or any numeric dunder on
`EstimatedUsd`, no `#[pyo3(transparent)]`, and no defaulting `Duration` constructor
(`microvms-py/src/cost.rs:2-24`). Concretely it is 111 symbols of `#[pyclass(frozen,
from_py_object)]` wrappers: `PyDuration` (`microvms-py/src/cost.rs:89`), `PySizeClass`
(`microvms-py/src/cost.rs:488`), `PyRateTable` (`microvms-py/src/cost.rs:576`), `PyLineItem`
(`microvms-py/src/cost.rs:313`) and `PyCostReport` (`microvms-py/src/cost.rs:696`).

**Recent activity.** 7 commits in the 30-day window, `↑ rising` — at the rising threshold of
7 and joint-highest among all binding files.

**Owners.** Laith Al-Saadoon 57% (4 of 7 commits); `bgagent` 43% (3 of 7). Two identities,
one of them a bot, so no second human reviewer is implied.

**Findings.** 66 error, 0 warn. All 66 of the file's 66 symbols that carry inbound dependents
are error-class, the highest count in the repository, and `codegraph affected
microvms-py/src/cost.rs -d 1 --json` returns an empty `affectedTests` array against 2
dependents traversed in total. The most-depended-on uncovered symbols are `PySizeClass` (10
dependents, `microvms-py/src/cost.rs:488`), the `seconds` getter (6 dependents,
`microvms-py/src/cost.rs:126`), `PyDuration` (5, `microvms-py/src/cost.rs:89`) and
`PyRateTable` (5, `microvms-py/src/cost.rs:576`). The mitigation is real and it is dynamic:
`microvms-py/tests/test_cost.py` holds 39 pytest functions over 753 lines, and the generated
stubs are gated by `stubs:check` (`mise.toml:179-195`, listed in `check` at `mise.toml:297`).
What no tier covers is a Rust-level refactor of the absences the module docs enumerate — a
`__float__` accidentally reintroduced on `EstimatedUsd` is caught only if a Python test
happens to assert its absence, after a full native rebuild.

### 2. `microvms-js/src/cost.rs`

**What's there.** The napi-rs mirror of the same engine, where the entire BIND-5 requirement
reduces to one decision: every type is a `#[napi]` **class** and never `#[napi(object)]`,
because an object converts by structure and `{ amount: 1.5 }` would satisfy an `EstimatedUsd`
parameter (`microvms-js/src/cost.rs:2-12`). `valueOf`, `toJSON`, `Symbol.toPrimitive` and any
`add` method are absent by design, so `Number(usd)` is `NaN` and the figure leaves only
through `.amount` as a string (`microvms-js/src/cost.rs:16-25`).

**Recent activity.** 5 commits, `→ flat`. It ranks second on score with no rising bonus,
entirely on finding count.

**Owners.** Laith Al-Saadoon 60% (3 of 5); `bgagent` 40% (2 of 5).

**Findings.** 65 error, 0 warn, out of 66 symbols with inbound dependents — and the 66th is a
measurement artifact, not a covered symbol. The one symbol credited with coverage is
`Duration` (`microvms-js/src/cost.rs:60`), attributed to
`microvms-core/tests/turmoil_client.rs`, which contains zero references to `microvms-js` in
either spelling. `Duration` is the only node of that name in the entire index, so every
`Duration` mention in the workspace — including every `std::time::Duration` — resolves onto
this one napi class. The real figure for this file is 66 of 66. Highest-dependent uncovered
symbols are `SizeClass` (27 dependents, `microvms-js/src/cost.rs:422`), `Amount` (5,
`microvms-js/src/cost.rs:187`), the `CostReport::wrap` constructor (4,
`microvms-js/src/cost.rs:619`), `Total` (3, `microvms-js/src/cost.rs:383`) and `all` (3,
`microvms-js/src/cost.rs:450`).
`microvms-js/__test__/cost.mjs` exercises this surface with 42 `test(` calls over 733 lines,
but CodeGraph does not classify `.mjs` as a test file, so those never appear in `affected`
output for any path. This crate's `index.d.ts` is gitignored (`.gitignore:29`) and no drift
gate for it appears anywhere in `mise.toml`, unlike its Python twin whose stubs are checked
at `mise.toml:179-195` — the one asymmetry on this list that is a gap in the gate rather than
a limit of the measurement.

### 3. `microvms-js/src/session.rs`

**What's there.** The control API of one running MicroVM on the Node side, where the binding
inherits its exclusion guarantees from the core: `Sandbox` owns its `Session` by value, hands
out only `Option<&Session>`, `Session` is not `Clone`, and no accessor exposes the agent
token, so a second independent session against the same VM cannot be constructed
(`microvms-js/src/session.rs:4-12`). The mutex is tokio's rather than `std`'s because every
method is `async` and holds the guard across an `await`, reproducing the `&mut self` exclusion
that `suspend`/`resume`/`terminate` require (`microvms-js/src/session.rs:14-18`).

**Recent activity.** 5 commits, `→ flat`.

**Owners.** Laith Al-Saadoon 60% (3 of 5); `bgagent` 40% (2 of 5).

**Findings.** 25 error, 0 warn, from 27 symbols with inbound dependents out of 33 total. The
two the index ranks highest by dependent count are the lock-acquiring internals rather than
the public methods — `Live::session` at 18 (`microvms-js/src/session.rs:244`) and the `live()`
guard-taker at 17 (`microvms-js/src/session.rs:274`) — though both names are short enough that
those counts are inflated by the name-resolution caveat above; `into_request` (3,
`microvms-js/src/session.rs:142`), `in_sandbox` (3, `microvms-js/src/session.rs:264`) and
`port` (3, `microvms-js/src/session.rs:306`) follow. Those two functions are where the
"held for exactly one method call and no more" invariant in the doc comment at
`microvms-js/src/session.rs:272-273` actually lives, and a lock-scope regression in them is
the class of defect a dynamic suite detects only as a hang. `codegraph affected
microvms-js/src/session.rs -d 1` does return two tests —
`agentd/tests/turmoil_transport.rs` and `microvms-core/tests/turmoil_client.rs` — reached
through the core types this file wraps, not through the binding surface itself.

### 4. `microvms-core/src/cost.rs`

**What's there.** The cost engine proper, and the largest file in the workspace at 4,127
lines: rate data carried as types rather than prose because MicroVMs publishes no standalone
pricing page, with two invariants enforced by shape instead of at runtime — "seconds are
measured, dollars are estimated" via a `DurationP` enum whose every variant names its
provenance (COST-1), and "unknown is not zero" via `Amount::Unpriced` as a distinct variant
that forces a match arm (COST-3), promoting to `Total::AtLeast` so a floor cannot be read
without its reasons (COST-4) (`microvms-core/src/cost.rs:2-27`). Central types are
`RateTable` with its `region`/`source_url`/`retrieved` provenance fields
(`microvms-core/src/cost.rs:849`), `CalendarDate` (`microvms-core/src/cost.rs:209`),
`LineItem` (`microvms-core/src/cost.rs:1440`), `CostReport`
(`microvms-core/src/cost.rs:1478`) and `RunUsage` (`microvms-core/src/cost.rs:1730`).

**Recent activity.** 7 commits, `↑ rising`.

**Owners.** `bgagent` 71% (5 of 7); Laith Al-Saadoon 29% (2 of 7). The only top-5 file whose
top owner is the bot, which means most of its 4,127 lines landed inside large squashed
commits.

**Findings.** 93 warn, 0 error — the largest warn count in the repository, and every one of
them is a tier-reach statement rather than an absence of tests: the file carries 65 in-file
`#[test]` functions, so no symbol qualifies for the error class. Highest-dependent uncovered
symbols are `RateTable` (15 dependents, `microvms-core/src/cost.rs:849`), `CalendarDate`
(14, `microvms-core/src/cost.rs:209`), `LineItem` (13, `microvms-core/src/cost.rs:1440`),
`today_utc` (12, `microvms-core/src/cost.rs:251`), `CostReport` (11,
`microvms-core/src/cost.rs:1478`) and `RunUsage` (11, `microvms-core/src/cost.rs:1730`).
`codegraph affected microvms-core/src/cost.rs -d 1` reaches 8 test files, the broadest of any
file on this list. Two specifics deserve attention: `today_utc`
(`microvms-core/src/cost.rs:251`) reads the wall clock and floors on integer division, so it
is the file's one ambient-time dependency and cannot be exercised deterministically by a
`turmoil` tier that controls virtual time only; and `RateTable::retrieved`
(`microvms-core/src/cost.rs:849`) makes rate freshness a data property, which the separate
`scripts/check-live-rates.py --twin-only` cross-check exists to verify (`mise.toml:412-414`)
rather than any Rust test tier.

### 5. `microvms-py/src/exec.rs`

**What's there.** One exec plus the SSE stream exposed as a Python iterator, which the module
docs single out as the only shape on the surface that needed real work: `ExecStream::new`
spawns a driver onto the shared runtime with an owned `ExecHandle` and a bounded `mpsc`
sender, and `__next__` blocks on `recv` (`microvms-py/src/exec.rs:4-15`). The channel bound
is deliberately 1, because the daemon's SSE body is the backpressure signal and an unbounded
channel would buffer a fast producer's whole output inside the binding — the failure the
core's byte-offset cursor exists to make unnecessary (`microvms-py/src/exec.rs:11-15`);
dropping the iterator drops the receiver, the next `send` fails, and the drive ends on
`ControlFlow::Break` (`microvms-py/src/exec.rs:18-20`).

**Recent activity.** 7 commits, `↑ rising` — tied with `microvms-py/src/cost.rs` for the
hottest binding file.

**Owners.** Laith Al-Saadoon 71% (5 of 7); `bgagent` 29% (2 of 7).

**Findings.** 22 error, 0 warn, covering all 22 of the file's symbols that have inbound
dependents (58 symbols total, so most of the surface has no recorded dependent at all).
`codegraph affected microvms-py/src/exec.rs -d 1 --json` returns an empty `affectedTests`
array. Ranked by dependents: `PyExecResult` (5, `microvms-py/src/exec.rs:64`), a `wrap`
constructor (5, `microvms-py/src/exec.rs:78`), the `seconds` getter (3,
`microvms-py/src/exec.rs:629`), `PyStdinAck` (2, `microvms-py/src/exec.rs:173`), `data` (2,
`microvms-py/src/exec.rs:236`) and `PyExecHandle` (2, `microvms-py/src/exec.rs:485`). The
drop-and-break lifecycle described at `microvms-py/src/exec.rs:18-20` is the specific
untested-at-Rust-tier surface worth attention, because a leaked driver task shows up as a
hang or a stray thread rather than a failing assertion.
`microvms-py/tests/test_exec.py` covers the behaviour dynamically with 29 pytest functions
over 655 lines. `PyExecResult` keeping `exit_code` and `signal` as `Option<i32>` rather than
sentinel integers (`microvms-py/src/exec.rs:64-68`) is the kind of distinction that a
Python-only test can assert but no Rust tier here defends.

## Reproduction

Every number above traces to one of these, run from the repository root at commit
`5e6f752a4c2a75f88522a38ce40d0a444f23ebc4`:

- Trend and ownership — `git log --since=30.days.ago --name-only --pretty=format:'---%H|%an|%ae'`,
  keeping `.rs`/`.py` paths outside `tests/`, `conformance/`, `scripts/`, `examples/`,
  `benches/` that still exist on disk (78 files; 15 touched paths have since been deleted,
  including the retired `clients/python/` package).
- Static-analysis baseline — `cargo clippy --all-targets --message-format=json` (0
  diagnostics) and the semgrep invocation at `mise.toml:112` (0 results).
- Covering-test relation — reverse traversal of `calls`/`references`/`instantiates` edges in
  `.codegraph/codegraph.db` at depth 1. The rule was validated against 126 markers harvested
  from 33 `codegraph explore` calls: depth 1 agrees on 124 of 126, and the 2 disagreements are
  test-glob edge cases (`microvms-js/__test__/*.mjs` is not treated as a test;
  `microvms-py/examples/typed_usage.py` is). Aligning on those reproduces the tool at 126/126.
- In-file unit-test census — count of `#[test]` and `tokio::test` attributes per
  `*/src/**/*.rs`.
- Per-file coverage cross-check — `codegraph affected <path> -d 1 --json`. Depth matters: at
  the default `-d 5` every path returns the same saturated 19-file test set, so the default
  invocation is not a discriminator.

## See also

- [business logic](../insights/business-logic.md) — 6 shared source citations
- [impact analysis](../insights/impact-analysis.md) — 6 shared source citations
- [contract map](../insights/contract-map.md) — 5 shared source citations
- [public api](../reference/public-api.md) — 5 shared source citations
- [dead code](dead-code.md) — 4 shared source citations
