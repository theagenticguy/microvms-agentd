---
title: Capture subprocess output through pipes, not temp files, when grandchildren matter
category: best-practices
tags: [rust, tokio, subprocess, process-group, exec]
session: session-bdf1bf
date: 2026-08-05
---

## Lesson

A daemon that runs arbitrary commands should capture output through inherited
pipes rather than temp files. With pipes, EOF arrives when the *last writer*
closes, so a command that backgrounds a server or log tailer keeps its output
flowing. With temp files keyed to the direct child's exit, that output is lost:
the predecessor unlinked both files the moment the direct child's return code was
set, and any grandchild kept writing into a deleted inode on tmpfs, charged
against VM memory.

Bound the pipe two ways, because unbounded is how a 512 MiB VM dies:

- a per-stream byte cap that truncates and sets an explicit `truncated` flag, and
- a post-child-exit linger deadline, after which reading stops and the result
  reports that writers may still be alive.

Past the cap, keep reading and discarding rather than stopping. Stopping leaves the
writer blocked in the kernel forever once the 64 KiB pipe buffer fills.

Drain both pipes *concurrently with* `child.wait()`. Waiting first and draining
after deadlocks any child that fills the buffer.

## Three tokio traps this hit

`child.id()` returns `None` after the child has been polled to completion, so the
pgid must be captured immediately after spawn. Reading it lazily in the kill path
yields `None`, the group signal never goes out, and a kill test that only asserts
on the status code still passes: it hangs or reports success while the process
tree survives. Assert on the observable kill outcome, not the HTTP status.

Privilege demotion uses `Command::uid()/gid()`, which act between fork and exec in
C. Do not use `pre_exec` for this: running interpreted code between fork and exec
is unsafe with threads and can deadlock the child. Note `Command::groups()` is
still unstable as of rustc 1.96, so supplementary groups are not cleared by the
safe path.

Never hold a `std::sync::Mutex` guard across an await. The exec registry keeps
child handles out of the locked map by having a detached waiter task publish into
an `Arc`, so the lock is never held while waiting.

## Related

[[proptest-and-dst-tiers-need-verdict-assertions]] — the kill-test failure above
is an instance of asserting on the wrong observable.
