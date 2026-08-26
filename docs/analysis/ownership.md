# microvms-agentd · Ownership

Per-author ownership analysis does not apply to this repository, and this file says so with
numbers before it measures anything else.

The whole history is 114 commits spanning 2026-08-05 to 2026-08-17
(`git rev-list --count HEAD`; `git log --reverse --date=short --pretty=format:%ad` for the
bounds). `git log --pretty=format:'%an <%ae>' | sort | uniq -c` returns three author
identities and only three: 56 commits from `alsaadoonlaith@gmail.com`, 31 from
`9553966+theagenticguy@users.noreply.github.com`, and 27 from `bgagent@noreply.github.com`.
The first two are the same human — `%cn`/`%ce` shows the 31 are committed by
`GitHub <noreply@github.com>`, which marks them as web-flow and merge-queue commits — so the
history is **one human author (87 commits) and one automated agent identity (27)**. No
`CODEOWNERS` file exists at the repository root, under `.github/`, or under `docs/`, so there
is no declarative owner to check the git data against either.

A folder-by-author commit-share table over that history would report the same two numbers on
every row and would name a bus factor of 1 that is true by construction rather than
discovered. This file measures the two things that do vary across subsystems:

- **Churn** — how many times each subsystem has been edited. One "file-touch" is one file
  changed in one non-merge commit, from `git log --pretty=format: --name-only`, counted on the
  first path segment. The total across all history is 896. Counting the same paths with
  `git log --no-merges --oneline -- <path>` reproduces every per-file figure below exactly, so
  the two definitions agree.
- **Symbol density** — how much declared structure each subsystem holds, from
  `codegraph files --format grouped` over the 121 indexed files.

Where the two measures agree, a reader is looking at a subsystem that is both large and
frequently revised. That is the concentration this repository actually has.

## Knowledge concentration by subsystem

`Share` is the subsystem's percentage of the 896 total file-touches. `Symbols` is the sum of
declared symbols across that subsystem's indexed files; a dash means the tree holds no
symbol-bearing indexed files.

| Folder | Churn (file-touches) | Share | Symbols |
| --- | --- | --- | --- |
| `microvms-core/` | 149 | 17% | 1,572 |
| `microvms-cli/` | 137 | 15% | 708 |
| `docs/` | 104 | 12% | — |
| `./` (root config and README) | 100 | 11% | — |
| `microvms-py/` | 72 | 8% | 591 |
| `agentd/` | 71 | 8% | 727 |
| `clients/` (deleted, see below) | 70 | 8% | — |
| `microvms-js/` | 50 | 6% | 374 |
| `.github/` | 31 | 3% | — |
| `scripts/` | 26 | 3% | 170 |
| `.erpaval/` | 23 | 3% | — |
| `conformance/` | 22 | 2% | 96 |
| `protocol/` | 17 | 2% | 88 |
| `model/` | 10 | 1% | 104 |
| `spec/` | 6 | 1% | — |

Three readings of that table matter.

**`clients/` is the largest body of knowledge in this repository that no working tree
contains.** It accrued 70 file-touches, 8% of all churn, and then left:
`git log --oneline --diff-filter=D -- clients` returns `c4d396e`, "Retire the Python client:
the discovery instrument becomes git history". 28 files lived under `clients/python/`,
12 of them under `tests/` and 10 of those test modules covering SSE reconnect, proxy auth,
pricing, sizing, and cost.
`mise.toml:150-153` records what that suite was worth: 83 client-library tests against a fake
daemon over a real loopback socket, and both suites passing against real AWS on the same
commit — Python oracle 56/56, Rust CLI 38/38 — is what ended the oracle's job. Recovering any
of it requires `git show`, and nothing in the tree points a reader at that commit.

**The densest files are also the most-churned files.** `microvms-core/src/cost.rs` holds 242
symbols, the highest in the repository, and 7 commits. `agentd/src/exec.rs` 147, `agentd/src/fs.rs`
137, `microvms-cli/src/guards.rs` 123 with 13 commits, `microvms-py/src/cost.rs` 117,
`microvms-js/src/cost.rs` 106, `microvms-core/src/sandbox.rs` 105 with 11 commits,
`microvms-core/src/control/image.rs` 99 with 11, `microvms-core/src/control/ops.rs` 96 with 9,
`microvms-core/src/session/mod.rs` 94. Six of those ten are in both the churn top-20 and the
density top-10. A change to any of them is a change to a file that is simultaneously the
largest and the least settled thing in its crate.

**`protocol/` and `model/` are the inverse case.** 17 and 10 file-touches, 88 and 104 symbols.
`protocol/` is small and quiet, and it is also the crate both the daemon and every client
compile against — `microvms-cli/tests/dependency_direction.rs` makes the direction
`cli -> core -> protocol` a test rather than a convention. Low churn there is a property of
the wire contract being stable, not of the code being unimportant.

## Where the knowledge lives outside one head

Bus factor 1 is the starting condition here, so the question worth asking is how much of what
one person knows has been written down somewhere a second person can read. The answer is
more than expected, in four layers.

**Measured platform behavior.** `docs/PLATFORM.md` carries 43 H2 headings, 42 of them
distinct — the heading "A WebSocket reaches a guest server through the endpoint, and the proxy
strips its own subprotocols" appears at both `docs/PLATFORM.md:784` and
`docs/PLATFORM.md:853`, so the file holds one duplicated section and a reader counting
findings should count 42. Each is a finding that cannot be derived from this repository's
source, because it describes the AWS service rather than this code. Measurement dates run
2026-06-17 through 2026-08-16, with 22 references to 2026-08-15 alone. Two sections show why
the file cannot be cheaply regenerated:
`docs/PLATFORM.md:46` establishes that `runHookPayload` arrives wrapped rather than as the
request body, which cost a full build-and-run cycle because the platform terminates the VM on
the resulting 400 before the payload can be read; `docs/PLATFORM.md:64` fixes the
`runHookPayload` ceiling at 4096 bytes and notes the service model states it twice,
differently.

**Formal requirements.** `spec/core.symspec.json` holds 51 requirements, every one
`status: approved`, keyed `TRAP` 13 / `STATE` 12 / `COST` 10 / `CLI` 6 / `ARCH` 5 / `BIND` 5,
with `verificationMethod` distributed `test` 38 / `analysis` 9 / `inspection` 4, plus a state
model and one waiver. Its `systemName` field gives the per-subsystem coverage: `microvms-core`
27, cost engine 10, CLI crate 8, language-bindings layer 3, JavaScript binding 1, Python
binding 1, sizing model 1. `spec/agentd.symspec.json` adds 6, all `systemName: agentd`. The 13
`TRAP-*` requirements are `docs/PLATFORM.md`'s findings in enforceable form.

**Compounded lessons.** 12 files under `.erpaval/solutions/`, in four categories:
`api-patterns/` 3, `architecture-patterns/` 2, `best-practices/` 4, `test-failures/` 3. They
carry the failures that cost the most to rediscover — that `aws-config` with
`default-features = false` cannot resolve credentials at all, that a byte-offset cursor is
what separates a working stream reconnect from a broken one, that a deterministic simulator
has two clocks and a spawned child obeys the wrong one.

**Executable gates.** `mise.toml:292-301` defines `check`, the stated definition of done, as
exactly eight tasks: `lint`, `security`, `test`, `schema:check`, `stubs:check`, `model:check`,
`live:check`, `build`. Four of those are drift gates that keep a hand-maintained value honest
against an independent source: `schema:check` (`mise.toml:173`) asserts `docs/schema.json`
still describes what the daemon serves, `stubs:check` (`mise.toml:195`) asserts
`microvms-py/microvms.pyi` still describes the pyo3 surface, `model:check` (`mise.toml:257`)
asserts `microvms-core`'s hardcoded constants still match the pinned botocore service model,
and `live:check` (`mise.toml:288`) asserts the live tier's own wiring, including `mise.toml`
itself. A gate is stronger than a document because it fails rather than being unread.

### What that coverage does not reach

Three subsystems or artifacts sit outside it, each verifiable from the repository.

**Neither symspec gate runs in `check`.** `mise.toml:207` and `mise.toml:227` are the two
spec-verification tasks, and neither appears in `check`'s dependency list at
`mise.toml:292-301`. Their own comments give the reason: `symspec` is a global npm install
plus a downloaded embedding model, and `mise.toml:227` invokes the v5 CLI as
`node ~/workplace/symspec/packages/symspec/dist/cli.mjs` — an absolute path into one
developer's home directory. The strongest externalization in the repository, 57 approved
requirements, is therefore verified by a toolchain a second contributor does not have, and no
unconditional gate reports when the requirements and the code diverge.

**`microvms-js` has no typings drift gate.** `.gitignore:29` ignores
`microvms-js/index.d.ts`, and neither `mise.toml` nor `.github/workflows/ci.yml` mentions
`index.d.ts` anywhere. The Python binding's equivalent artifact is gated by `stubs:check`;
the Node binding's is generated, ignored, and unchecked, so a divergence between the Rust
surface and the TypeScript surface shipped to consumers surfaces at a consumer's keyboard.

**`protocol` and `agentd-model` carry zero formal requirements.** No `systemName` in either
symspec file names them (`protocol/Cargo.toml:2` declares `protocol`, `model/Cargo.toml:2`
declares `agentd-model`). `protocol/` is the crate the daemon and every client both compile
against, and `docs/PROTOCOL.md` states the wire contract must never change silently. The
requirement set that would make a silent change fail does not exist for it; the compile error
from a type change is the whole defense.

## Read first

A second contributor becomes productive by reading these in this order. The first three come
before any code change, because skipping `docs/PLATFORM.md` means re-learning its traps at the
price of live AWS runs.

1. `README.md` — why the project exists. `README.md:203` names the origin: a daemon inside
   Harbor PR #2469, and the review rounds that argued for a verified stack instead.
2. `docs/PLATFORM.md` — every section, each dated and scoped to a region and API version.
   Read it before touching launch, cost, or hook code.
3. `docs/PROTOCOL.md` — wire protocol v1, and which paths the platform fixes versus which
   this project owns. `docs/schema.json` is its generated companion and `schema:check` keeps
   the two in agreement.
4. `docs/TRUST.md` — the boundary contract: what the daemon must refuse, and why the workload
   is untrusted by design.
5. `spec/core.symspec.json` — the 51 requirements, the 13 `TRAP-*` entries first, since each
   is a `docs/PLATFORM.md` finding with a `verificationMethod` attached.
6. `.erpaval/solutions/` — 12 lessons, ordered by whichever subsystem is about to be touched.
   Consulting them before a fix costs minutes; rediscovering one costs a session.
7. `mise.toml` — the command surface. `mise run check` is the local gate; `mise run live` is
   billable and manual.
8. `microvms-core/src/` — the largest crate at 30,903 lines and 1,572 symbols.
   `constants.rs` and `cost.rs` are where the measured platform values land in code.
9. `docs/STRATEGY.md` — scope, audience, and the labeling discipline every claim follows:
   measured, documented, vendor-claimed, or inferred.

## Single points of failure

The entire codebase is effectively one owner (see intro).

The per-path shares below are still worth stating, because with only two identities in the
history the split is between the human author and the `bgagent` automated identity, and which
one holds a file predicts whether any person reviewed it. Read each percentage against its
commit count: on a file with three commits a share above 70% carries little information, so
every bullet names the count. Shares are computed with
`git log --no-merges --pretty=format:%ae -- <path>`.

- `mise.toml` — sole human author (71% of 17 commits). Bring both symspec gates inside
  `check` behind a pinned, repository-local toolchain so the requirement set is verified by
  the command a fresh clone can run, rather than by a path into one home directory.
- `microvms-cli/src/guards.rs` — sole human author (85% of 13 commits). At 123 symbols and the
  highest churn in the CLI crate, this file needs a second reader more than any other; pair a
  review of it with `.erpaval/solutions/test-failures/guards-that-passed-against-broken-code.md`,
  which records four ways its guards passed against broken code.
- `microvms-cli/src/cli.rs` — sole human author (85% of 13 commits). The command surface
  definition is the CLI's contract with every consumer, so changes here belong behind the
  `microvms-cli/tests/manifest.rs` and `thinness.rs` assertions rather than behind review
  alone.
- `microvms-core/src/control/image.rs` — sole human author (82% of 11 commits). 99 symbols
  covering image and version creation, an area where `docs/PLATFORM.md:725` and
  `docs/PLATFORM.md:747` record two service refusals; keep those two sections and this file
  under one change.
- `microvms-core/src/control/ops.rs` — sole human author (78% of 9 commits). Cross-train a
  second reader here before the control-plane call surface grows again, since 96 symbols in
  one file is where a control-plane behavior change hides.
- `microvms-cli/src/commands/lifecycle.rs` — sole human author (75% of 12 commits). The
  lifecycle commands are the operator's path to spending money, so every change here should
  cite the `docs/PLATFORM.md` section whose behavior it depends on.
- `microvms-core/src/sandbox.rs` — sole human author (73% of 11 commits). 105 symbols
  implementing the single-writer state machine that the `model/` crate's stateright model also
  encodes; change the model in the same commit so the two descriptions cannot drift.
- `microvms-core/src/cost.rs` — `bgagent` automated identity (71% of 7 commits). The densest
  file in the repository at 242 symbols, holding a hand-pinned us-east-1 rate table that has
  already drifted once — `microvms-core/src/cost.rs:1018-1021` records that `0.08` was the
  plausible-looking wrong value against the correct `dec!(0.0811111030)` — so keep
  `scripts/check-live-rates.py` in the billable tier and treat a rate edit as a measurement.
- `agentd/src/routes.rs` — `bgagent` automated identity (86% of 7 commits). This file splits
  the 18 daemon endpoints into the Bearer-guarded `control` router and the `open` router at
  `agentd/src/routes.rs:48-56`, which makes it the repository's authorization boundary; it
  should carry a named human reviewer on every change, since no commit on it currently does.
- `docs/reference/cli.md` — sole human author (100% of 9 commits). A hand-written reference
  for a surface that `microvm manifest` already emits machine-readably, so generate the
  overlapping sections or add a drift check, rather than maintaining two descriptions of one
  command set.
- `spec/core.symspec.json` — `bgagent` automated identity (100% of 2 commits). 51 requirements
  in a single file with no second copy and no gate inside `check`; approve changes to it the
  way a schema change is approved, and pair every edit with the `docs/PLATFORM.md` section it
  encodes.
- `scripts/check-model-drift.py` — `bgagent` automated identity (100% of 1 commit).
  `scripts/check-model-drift.py:254` and `:266` hold `PINNED_REGIONS` and
  `PINNED_SIZE_CLASSES` as deliberate hand-maintained copies, and `:57` states they are the
  second reader for two values no AWS service model publishes; any change to the Rust
  constants has to land in this file in the same commit.

## See also

- [system overview](../architecture/system-overview.md) — 6 shared source citations
- [contract map](../insights/contract-map.md) — 4 shared source citations
- [impact analysis](../insights/impact-analysis.md) — 4 shared source citations
- [module map](../architecture/module-map.md) — 3 shared source citations
- [state machines](../behavior/state-machines.md) — 3 shared source citations
