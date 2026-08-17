# An ordering defect is invisible to every test that only watches the guard

**Category:** best-practices
**Tags:** ordering, seam-test, guards, cost-honesty, upload, preflight
**Modules:** microvms-cli/src/guards.rs, microvms-cli/src/commands/lifecycle.rs, microvms-core/src/control/image.rs
**Session:** session-bf11b1 (2026-08-17, issues #46/#47, PR #49)

## The trap

Issue #47 was not a missing guard — every guard fired, every refusal test
passed. The defect was *when*: `upload_artifact` ran before `build_image`, so
the S3 PUT happened before the guards refused. A test that asserts "the bad
request is refused" and even "zero control-plane calls" stays green across the
broken ordering, because the upload is not a control-plane call and had no
recorder at all.

## The move

Give the side effect its own recorder channel and assert absence through it.
Here: a `uploads: Mutex<Vec<String>>` on `ScriptedTransport`, filled by the
seam's `put_artifact`, deliberately *not* mixed into `calls` — an upload is not
a wire call, and polluting the existing channel would break every zero-calls
assertion for the wrong reason. The ordering test then asserts
`uploads() == []` on a request the library itself refuses.

Falsification is a pure reorder: swap the two lines back and the test goes red
with the recorded URI as evidence. Both call sites (`build`, and `run`'s build
arm) need the break run separately — one site fixed and one broken passes a
single-site test.

## Second finding, same session

Two helpers deriving the same temp path scheme
(`microvm-guard-<label>-<pid>-<tid>`) collide when one test uses both with the
same label — `FakeBinary` writes a file where `TempDir` wants a directory.
Distinct labels per helper instance inside one test.

## And the restore discipline

Never `git checkout <file>` to undo a falsification break on a file carrying
uncommitted work — it reverts the work too. `cp` the file to /tmp before the
break and `cp` it back, or commit first (here the pre-commit hook compiles the
workspace, so a mid-extraction commit can be impossible — the /tmp copy is the
reliable variant).
