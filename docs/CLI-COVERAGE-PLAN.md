# Plan: full live coverage through the `microvm` CLI

> **Implemented. This document is kept as history because the reasoning still
> applies; the numbers below are outdated.**
>
> All five waves shipped and the conformance flip landed. `conformance/run_rs.py`
> expresses every named check with none skipped. It no longer carries an
> `UNSUPPORTED` table or an `unsupported()` helper. Instead, the suite computes
> its own coverage line from what ran (`expressed = passed + failed`, denominator
> plus skips), so no document has to keep a quoted figure in step. As a result,
> the "38 of 72" below describes the state on the day the plan was written and is
> no longer accurate. The numerator is now complete. The denominator settled at
> **75** rather than 72, because two of the original 38 turned out to be weak
> readings off the launch envelope and were split into real checks (see
> `CHANGELOG.md`); it has since grown to **77** as the suite gained checks,
> which is why the paragraph below says to read the figure off a run.
>
> Read this for *why* each surface exists and what the constraints were. For what
> the suite covers today, run it, or read the summary block at the end of
> `conformance/run_rs.py`. That block derives the coverage number from the checks
> that ran instead of quoting a stored figure, so it stays accurate.

**Goal (as written, before implementation).** `conformance/run_rs.py` currently
expresses 38 of 72 named checks and prints 34 as SKIP, each naming the missing
subcommand. This plan adds the five CLI surfaces that close every SKIP, so the
Rust suite alone gives live AWS coverage of everything the retired Python oracle
covered. The daemon needs no changes. Every capability already exists in
`microvms-core` (`session/files.rs`, `session/exec.rs` streaming and stdin,
`protocol::Health`) and is exercised today by the fake-backed and turmoil
tiers. The only missing piece is the CLI surface for each capability. The live
rounds have twice found bugs in paths that had never been driven end-to-end
against real AWS, which is why these paths need live coverage.

## The 34 SKIPs, grouped by the surface that closes them

| New surface | SKIPs closed | The checks, by name (from run_rs.py's UNSUPPORTED table) |
| --- | --- | --- |
| `microvm cp` (+ `--tar`) | 13 | single file write / read / absent-is-404; tree created for the round trip; tar download / upload; symlink survived as symlink / still resolves; nothing escaped the extraction root; 4 hostile archives refused |
| `microvm exec --stream` | 5 | SSE reached us through the proxy; output complete and ordered; no gap for a small stream; terminal exit event carries the real code; exec survives being streamed and stays pollable |
| stdin (`exec --stdin` + `microvm stdin`) | 5 | write accepted; close accepted; child exits once stdin closes; round trip through the child; refusal when the command never asked |
| exec identity (`--exec-id`, `--poll`) + `microvm ack` | 6 | ack accepted; double-ack 409; unknown id 404; retried start accepted / spawned no second child; pre-suspend exec record survives resume |
| `microvm health` | 5 | identity repair completed every step / actually ran; 8 MiB cap trio (noisy exit 0, `truncated` flag, daemon survived — survival is the health probe) |

Totals reconcile: 13+5+5+6+5 = 34 (29 UNSUPPORTED entries + 4 hostile archives + the pre-suspend exec record noted inline at run_rs.py:1034).

## Design constraints that carry over (not negotiable)

- **CLI-2/CLI-5 hold.** Every new subcommand goes through `microvms-core` only;
  no option may accept a value core rejects. `cp --tar` hands core an archive
  PATH, and core's tar handling stays the only extractor. The hostile-archive
  checks assert core's refusal through the CLI's `data.kind`, so the CLI must
  not pre-validate archives; pre-validation would test the CLI's copy of the
  guard rather than core's.
- **The manifest is generated**, so each subcommand lands with its options,
  exit codes, and envelope keys appearing in `microvm manifest` for free. The
  manifest cross-check test enforces this structurally.
- **One envelope on stdout.** `exec --stream` is the hard case because its
  stream chunks are output, not progress reporting. Piped/`--json` mode emits
  one NDJSON line per event on stdout and the final envelope last; the
  envelope's `data` carries the event count and exit. The manifest documents
  this as `responseType: exec.stream`, a deliberate, named exception to the
  one-envelope rule. TTY mode instead renders live via ratatui. The
  envelope-purity test gains a stream variant: every stdout line before the
  last parses as an event, and the last parses as the envelope.
- **Guard proofs per surface.** Each check that goes SKIP→live keeps run.py's
  original falsification (e.g. the byte-scan for tar confinement, the
  wc -l tick-count for stdin EOF). To prove each guard still fires, break it
  deliberately, confirm the check fails, and then restore it.
- **agentToken handling.** The new attach-shaped commands (`cp`, `stdin`,
  `ack`, `health`, `exec --poll`) take the same `--endpoint/--agent-token/
  --microvm-id` triple `exec` takes today, reusing `seam::attach_session`.

## Waves

**W1 — `microvm health` + exec identity (`--exec-id`, `--poll`) + `microvm ack`.**
This is the smallest surface, it unblocks 9 checks, and `health` is needed by
W3's cap trio. This wave is CLI-only: core already exposes `Session::health`,
`Session::exec(id)`, and `ExecHandle::{poll,ack}`, so `run_sync` needs no
sibling path in core. Exit-code note: `--poll` on a running exec returns OK with
`phase: running`, because polling a still-running exec is not a failure.

**W2 — `microvm cp`.** `cp <local> vm:<remote>` / `cp vm:<remote> <local>`,
`--tar` for directory round trips (pack local→upload, download→unpack via
core's confined extractor), `--mode` for permissions. This wave closes 11
checks. The four
hostile archives are driven by handing the CLI a pre-built malicious tar file;
the expected failure is core's ProtocolError surfacing as `data.kind:
protocol_error` with exit 5.

**W3 — `microvm exec --stream`.** This wave emits NDJSON event lines plus the
final envelope, as described above. It reuses core's cursor-reconnect stream
and exposes `--from-offset` for the resume-at-cursor check. It closes 7 checks,
including the cap trio (with W1's health).

**W4 — stdin: `microvm exec --stdin` (streams local stdin to the child, EOF on
close) and `microvm stdin <exec-id> [--eof]` for the detached case.** This wave
closes 7 checks, including the opt-in refusal: running `stdin` against an exec
started without `--stdin` must surface 410/StdinClosed as `data.kind`.

**W5 — conformance flip + live proof.** Delete each closed entry from
`UNSUPPORTED` in run_rs.py and add the check bodies (the assertions are
copyable from git history: `conformance/run.py` at commit `c4d396e^`). The
suite's summary line changes from "38 of 72" to "72 of 72". Run
`mise run live` twice. The first run shakes out the new paths; the live tier
has caught a bug in every new transport path so far, so plan for one fix
round. The second, green run records the marker. Update mise task description, README, and
CHANGELOG coverage notes.

## Effort and order-of-magnitude cost

Each wave is a bounded Act task with the existing packet discipline (W1/W3
small, W2/W4 medium). Live proof runs cost what today's tier costs — roughly
two short-lived 1 GiB MicroVMs per run, at cents per round, plus one extra
shake-out round. Nothing here is speculative. Every check's assertion text and
falsification already exists in git history, and every core capability is
already tested locally. The remaining work is the five CLI surfaces and the
conformance flip.

## What this plan does not do

- It makes no daemon changes, adds no new wire routes, and changes nothing in
  the protocol crate.
- It adds no `microvm shell` or interactive TTY into the VM, because the
  platform's shell-auth path stays unreachable (TRAP-11).
- It adds no parallel-exec orchestration or watch modes, because the suite
  needs determinism.
