---
tags: [microvms-agentd, live-conformance, verification, fixtures]
modules: [conformance/run_rs.py, CLAUDE.md, microvms-cli/src/guards.rs]
---

# Live verification is the task-level definition of done (Laith, 2026-08-28)

`mise run check` is the definition of done for a *change*; a task touching the platform
surface is done only after a live AWS exercise of the new path plus a permanent check in
`conformance/run_rs.py`. Policy is in the repo's CLAUDE.md — strongly worded, at Laith's
direction, after the named-VMs feature shipped fully green locally and broke on its first
live run.

**Why:** the fixtures' MicroVM ids are spelled `mvm-*`; the real service's are
`microvm-*`. A passthrough keyed on the fixture prefix passed 196 in-crate tests, all
integration tests, and the behavioral guards — and refused every real VM id. A fixture
convention is not a service fact; only the service can state its own prefixes, error
spellings, and timing.

**How to apply:** after the local gate, drive the real binary through the new path
against the conformance account (`build --reuse` makes repeat image builds ~free), then
add a named check to run_rs.py and bump the count in its header AND mise.toml's
live:conformance-rs description. Also: `mise run check` does not rebuild
`target/release/microvm` — rebuild explicitly before a live run or you test a stale binary.
