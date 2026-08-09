---
title: Golden figures come from running the oracle, never from the plan
category: best-practices
tags: [porting, oracle, golden-tests, decimal]
session: session-fa0814
date: 2026-08-08
---

# Golden figures come from running the oracle, never from the plan

The plan pinned the 2 GB break-even at ~1357s; the Python oracle prints
1371.2916483478837. A golden test built from the plan's figure would have been
the one check that AGREES with a plausible wrong number — the port and its
test derived from the same mistaken source. Rule: when porting against an
oracle, every pinned figure and every output-contract string is captured by
EXECUTING the oracle (uv run against cost.py), pasted verbatim into the test,
and cited. Same rule killed two wrong assertions about the cost JSON contract
in the CLI lane (T-W3-7 packet). Corollary for renders: totals name PHASES
because cost.py:342 says so, whatever the Rust types would prefer to print.
