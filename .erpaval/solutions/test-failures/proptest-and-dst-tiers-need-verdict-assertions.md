---
title: A confinement property that only checks the filesystem measures nothing
category: test-failures
tags: [proptest, testing, verification, tar, security]
session: session-bdf1bf
date: 2026-08-05
---

## Lesson

When property-testing a policy that both *refuses* bad input and *contains* it,
assert the verdict, not only the containment. Checking "nothing landed outside the
root" is not enough, because a sanitizing bug keeps everything inside the root
while silently discarding the policy.

Measured in this session: removing the `?` from `parts.pop()?` in the tar
path-resolution loop turned `../x` into `x` instead of an error. That is a real
policy break — a member the contract says to refuse gets extracted under a
different name — and the whole proptest suite passed, because a filesystem walk
cannot see it. The archive landed entirely inside the root.

The fix is to compute the expected verdict from the generated input and assert on
it:

```rust
// Not just: no path escaped.
// Also: this member SHOULD have been refused, and the response says so.
prop_assert_eq!(actual_status, expected_status,
    "{member} must be refused: got {actual:?}");
```

With the verdict asserted, the same break fails immediately and shrinks to a
one-member archive.

## The same shape in the model tier

A stateright property written as `model.cfg.attacker_allowed || !state.breached`
is vacuously true in exactly the configuration where it should fail, so the
checker reports no discovery and the test fails for the wrong reason. State safety
properties unconditionally and let the two configurations differ, rather than
teaching the property about the config.

## Why it matters

This is the same defect class as a test in Harbor PR #2469 named
`test_create_token_is_not_a_permanent_key`, which passed against broken code
because it varied an input that nothing varies in reality. A green suite that
measures nothing is worse than no suite: it converts an open question into a false
answer.

The generalizable check: break the invariant deliberately, confirm the specific
test fails, and restore. Do it for every guard, not a sample.
