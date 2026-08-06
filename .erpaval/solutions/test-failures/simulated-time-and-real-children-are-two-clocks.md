---
title: A deterministic simulator has two clocks, and a spawned child obeys the wrong one
category: test-failures
tags: [turmoil, dst, testing, tokio, subprocess, virtual-time]
session: session-7ef43d
date: 2026-08-05
---

## Lesson

Under a simulator with virtual time (turmoil, madsim), everything inside the
simulation runs on the paused clock: client `sleep`, server timers, keep-alive
intervals, timeouts. A spawned child process does not. `sleep 2` in `/bin/sh` is
two *real* seconds. Measured in this project: 2 real seconds of child sleep
elapsed while the simulation advanced 30 virtual ones.

Two consequences, both of which cost a debugging round:

A child paced by `sleep` is unsynchronized with every assertion. A client that
"waits 4 seconds" for the second half of a command's output waits a few real
milliseconds and sees nothing.

Worse, any server-side deadline measured against real work becomes almost
instant. Our `output_linger` (how long to keep draining an exec's pipes after the
child exits) is a virtual 5 seconds against a real pipe drain, so it expires in
milliseconds of wall time. The waiter abandons a still-writing child, and the exec
reports missing output with a "writers may still be alive" flag. That is
simulator-induced truncation on the daemon side, and the tempting fix — loosening
the assertion until it matches — would have encoded the simulator's artifact as
the expected behavior.

## The fix

Never pace a child with `sleep` in a simulated test. Make children block on
`read`, released by an explicit stdin write, so the harness is the clock: the
ordering a scenario needs is *caused* rather than hoped for, and it holds under
any tick duration. Where a child cannot be paced that way, raise the server-side
deadline far past the scenario so a virtual timer cannot cut a real operation
short.

## Generalizes to

Any deadline in the system under test that is measured in simulated time but
bounds real work: filesystem I/O through a blocking pool, subprocess drains,
anything on `spawn_blocking`. Ask of each timer whether both sides of the
comparison live on the same clock.

Related: [[proptest-and-dst-tiers-need-verdict-assertions]] — the same instinct
applies, which is to distrust a test that passes for a reason you have not
verified.
