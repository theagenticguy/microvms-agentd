# Plan: control-plane client, CLI, and cost accounting

Derived from `spec.md` (25 ACs across 5 user stories). This file records the
decisions the spec deliberately left open, so the Act waves have one answer each
rather than inventing three.

## Layout

The monorepo is the point: one place where the in-VM contract and the client-side
contract are coherent. So the control-plane work extends the existing Python
package rather than starting a sibling.

```
clients/python/src/microvms_agentd/
  session.py, exec_handle.py, transport.py, _sse.py   in-VM: talk to the daemon
  sandbox.py                                          control plane: AWS lifecycle
  sizing.py            NEW  size classes, baseline vs peak
  cost.py              NEW  the rate table and per-phase attribution
  cli.py               NEW  the command surface, a thin layer over the above
  errors.py, models.py                                shared
```

One package, not two. A consumer who wants only the library imports
`microvms_agentd`; a consumer who wants a sandbox now runs `microvm`. Splitting
them would double the release surface and let the two drift, which is the failure
the monorepo exists to prevent.

## Decisions

**The CLI is named `microvm`,** exposed as a console script from the same package.
Short, says what it makes. Subcommands: `run`, `build`, `exec`, `suspend`,
`resume`, `terminate`, `ls`, `logs`, `cost`, `doctor`, `manifest`.

**Default baseline is the platform's own default (2 GB / 1 vCPU), not the
smallest class.** This reverses what I intended before reading the pricing. The
smallest class bills less per second, but baseline is also the *floor* of the burst
range, and a CLI that quietly picks 0.5 GB hands someone a sandbox that OOM-kills a
real test suite to save about three cents an hour. Cheap-and-broken is a worse
default than adequate. `--memory` overrides it, and `microvm cost --estimate`
exists so the choice is informed rather than guessed.

**Cost reporting is a feature, not a caveat.** `cost.py` owns a rate table pinned
with its retrieval date, computes per-phase attribution (build, running, suspended,
snapshot transitions, image storage), and both the library and the CLI report it.
Two rules. Every figure is labeled: seconds we timed are measured, dollars are
*derived from published rates* and are an estimate of the bill rather than the bill
itself — only Cost Explorer knows the latter. And the table carries its own
staleness: a rate table older than 90 days warns, because a silently stale price is
the same failure class as a silently stale schema.

**The `resume` bug gets fixed in wave 1, not deferred.** `sandbox.py:429` waits
only for `RUNNING` with no terminal-state branch, so a VM the idle policy already
terminated during suspension burns the full 300-second timeout and then reports
"never reached RUNNING" — the exact cause-hiding failure that `_wait_for_running`
was written to prevent, reintroduced on the resume path. AC-4-3 covers it.

**`os_capabilities` becomes an intent flag.** `list[str]` lets a caller express
`["CAP_SYS_ADMIN"]`, which the API rejects only after an artifact upload. `"ALL"` is
the only accepted value, so the parameter becomes a boolean whose docstring says
what it widens and why. That moves the trap from runtime-rejected to inexpressible,
which is the strongest form the spec defines.

## Waves

**Wave 1 — the library (task 31).** Sizing, the trap closures, the two bug fixes.
Everything in user stories 1 through 4. This is the foundation both other waves
depend on, so it runs alone.

**Wave 2 — CLI and cost, in parallel (tasks 32, 36).** Disjoint files (`cli.py`
versus `cost.py`) with a clean seam: the CLI renders what cost computes. Cost lands
first in dependency order but they can be built together.

**Wave 3 — verification (task 33).** Property tests over token generation and state
handling, a fake control plane so the suite needs no AWS, and per-trap guard proofs.
The rule stands: for each AC, break the guard, watch that specific test fail,
restore. An AC whose test passes either way is not done.

**Wave 4 — live (task 34), then validate and compound (task 35).**

## What this plan does not do

No orchestrator and no warm-pool manager. The suspended-pool economics are
attractive (two orders of magnitude cheaper than running) and the strategy memo
still declines to build the scheduler, because that is a product. `cost.py` gives
someone else the numbers to build it with.

No second language client. Python covers both stated audiences; a TypeScript
client is a v0.3 question driven by an actual request.
