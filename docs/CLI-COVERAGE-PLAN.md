# Plan: full live coverage through the `microvm` CLI

**Goal.** `conformance/run_rs.py` currently expresses 38 of 72 named checks and
prints 34 as SKIP, each naming the missing subcommand. This plan adds the five
CLI surfaces that close every SKIP, so the Rust suite alone gives live AWS
coverage of everything the retired Python oracle covered. The daemon needs no
changes — every capability already exists in `microvms-core` (`session/files.rs`,
`session/exec.rs` streaming and stdin, `protocol::Health`) and is exercised
today by the fake-backed and turmoil tiers; what is missing is only the CLI
door, and the live rounds proved (twice) that a path never driven end-to-end
against real AWS is a path with latent bugs.

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
  PATH, and core's tar handling stays the only extractor — the hostile-archive
  checks assert core's refusal through the CLI's `data.kind`, so the CLI must
  not pre-validate archives (that would test the CLI's copy of the guard, not
  core's).
- **The manifest is generated**, so each subcommand lands with its options,
  exit codes, and envelope keys appearing in `microvm manifest` for free — the
  manifest cross-check test enforces this structurally.
- **One envelope on stdout.** `exec --stream` is the hard case: stream chunks
  are OUTPUT, not progress. Piped/`--json` mode emits one NDJSON line per event
  on stdout and the final envelope last (documented in the manifest as
  `responseType: exec.stream`, a deliberate, named exception to the
  one-envelope rule — the envelope's `data` carries the event count and exit),
  while TTY mode renders live via ratatui. The envelope-purity test gains a
  stream variant: every stdout line before the last parses as an event; the
  last parses as the envelope.
- **Guard proofs per surface.** Each check that goes SKIP→live keeps run.py's
  original falsification (e.g. the byte-scan for tar confinement, the
  wc -l tick-count for stdin EOF). Break the guard, watch it fail, restore.
- **agentToken handling.** The new attach-shaped commands (`cp`, `stdin`,
  `ack`, `health`, `exec --poll`) take the same `--endpoint/--agent-token/
  --microvm-id` triple `exec` takes today, reusing `seam::attach_session`.

## Waves

**W1 — `microvm health` + exec identity (`--exec-id`, `--poll`) + `microvm ack`.**
Smallest surface, unblocks 9 checks, and `health` is needed by W3's cap trio.
`run_sync` grows a sibling path in core? No — core already exposes
`Session::health`, `Session::exec(id)`, `ExecHandle::{poll,ack}`; this wave is
CLI-only. Exit-code note: `--poll` on a running exec returns OK with
`phase: running`; polling is not a failure.

**W2 — `microvm cp`.** `cp <local> vm:<remote>` / `cp vm:<remote> <local>`,
`--tar` for directory round trips (pack local→upload, download→unpack via
core's confined extractor), `--mode` for permissions. 11 checks. The four
hostile archives are driven by handing the CLI a pre-built malicious tar file;
the expected failure is core's ProtocolError surfacing as `data.kind:
protocol_error` with exit 5.

**W3 — `microvm exec --stream`.** NDJSON event lines + final envelope (above).
Reuses core's cursor-reconnect stream; `--from-offset` exposed for the
resume-at-cursor check. Closes 7 including the cap trio (with W1's health).

**W4 — stdin: `microvm exec --stdin` (streams local stdin to the child, EOF on
close) and `microvm stdin <exec-id> [--eof]` for the detached case.** 7 checks
including the opt-in refusal (running `stdin` against an exec started without
`--stdin` must surface 410/StdinClosed as `data.kind`).

**W5 — conformance flip + live proof.** Delete each closed entry from
`UNSUPPORTED` in run_rs.py and add the check bodies (the assertions are
copyable from git history: `conformance/run.py` at commit `c4d396e^`). The
suite's summary line changes from "38 of 72" to "72 of 72". Run
`mise run live` twice: once to shake out the new paths (the live tier has
caught a bug in every new transport path so far — plan for one fix round),
once green to record the marker. Update mise task description, README, and
CHANGELOG coverage notes.

## Effort and order-of-magnitude cost

Each wave is a bounded Act task with the existing packet discipline (W1/W3
small, W2/W4 medium). Live proof runs cost what today's tier costs — roughly
two short-lived 1 GiB MicroVMs per run, cents per round — plus one extra
shake-out round. Nothing here is speculative: every check's assertion text and
falsification already exists in git history, and every core capability is
already tested locally; the work is five CLI doors and the conformance flip.

## What this plan does not do

- No daemon changes; no new wire routes; no protocol crate changes.
- No `microvm shell` or interactive TTY into the VM — the platform's shell-auth
  path stays unreachable (TRAP-11).
- No parallel-exec orchestration or watch modes; the suite needs determinism.
