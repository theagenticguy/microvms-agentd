---
title: Four ways a guard passed against broken code in one session
category: test-failures
tags: [guard-proof, compile-fail, proptest, turmoil, fakes, oracle]
session: session-fa0814
date: 2026-08-08
---

# Four ways a guard passed against broken code in one session

Every one found only because the guard-proof rule (break it, watch it fail,
restore) was actually run. A guard that was never watched failing is a guess.

1. **A bare ` ```compile_fail ` block passes for ANY build error**, including a
   typo in the doctest itself. All five COST-1/COST-2 compile-fail guards would
   have stayed green with the forbidden coercions restored. Fix: pin each to
   the rustc error code measured off a real attempt —
   ` ```compile_fail,E0277 ` (microvms-core/src/cost.rs, five sites).

2. **A fake that models the failure event cannot catch lateness.** The TRAP-9
   turmoil guard passed twice against `refresh = ceiling`: a client that
   refreshes too late never presents an *expired* token, it presents one with
   no life left. The fake proxy had to measure the REMAINING MARGIN
   ("token 3008s old, 592s of 3600s left"), not reject expiry
   (microvms-core/tests/turmoil_client.rs).

3. **Uniform proptest draws never land in a narrow band.** The TRAP-10 guard
   passed against a deliberately snapping `from_baseline_mib` because a uniform
   u32 essentially never falls in 1..8192 where rounding is even possible.
   Weight the generator into the band the bug lives in
   (microvms-core/src/sizing.rs:417).

4. **A guard test can REQUIRE the divergence it should catch.** The dense-output
   test in microvms-cli/tests/exit_codes.rs asserted the trailing total row that
   the Python oracle does not emit — so the parity bug survived review. When a
   guard pins an output contract, pin it to the ORACLE's literal output, never
   to what the implementation currently prints.
