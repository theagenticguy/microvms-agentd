# microvms-agentd · Debugging guide

This guide tells you where to look first when something breaks.

Most of this project's debugging knowledge is already written down, because almost every
failure mode here was found against real AWS after the local tiers were green. That history
lives in four places, and this guide draws from all of them:

- `docs/PLATFORM.md` — every entry is a measured platform behavior with a date, a region, and
  an API version. Most of them are traps.
- `EXIT_TABLE` in `microvms-cli/src/exit.rs:171` — fourteen rows. Each row carries a code, a
  `meaning` (what to do next), and a `finding` (the `docs/PLATFORM.md` section that measured
  it). The `finding` column turns an exit code into a documentation lookup.
- Trap messages in the library. `microvms-core` writes the finding into the error message
  itself, so the message carries the explanation without a doc lookup.
- Git commit bodies. The commits from the live rounds double as postmortems.

The source contains no `TODO`, `FIXME`, `HACK`, `INCIDENT`, or `POSTMORTEM` comments. The
history lives in the four places above instead.

## Failure-mode index

| Symptom | Likely surface | First check | Citation |
| --- | --- | --- | --- |
| `AccessDeniedException` with a **null message** field | Not IAM. The region does not price MicroVMs. Only five do: us-east-1, us-east-2, us-west-2, eu-west-1, ap-northeast-1. `boto3.client()` constructs for any region because the service `endpointPrefix` is `lambda`, so the first API call is the only reporter and it names the wrong cause | Read the message field. A real denial names the principal and the action; this one is `None`. Then `microvm doctor`, whose region check runs first for exactly this reason | `docs/PLATFORM.md:103-125`, `microvms-core/src/region.rs:155-162`, `microvms-core/src/control/ops.rs:331-332`, `microvms-cli/src/commands/doctor.rs:38-39` |
| Image stuck in `CREATING`, builds never start, `updatedAt` never advances past `createdAt` | The `clientToken` replay. A `clientToken` is a **permanent** idempotency key, so a create whose token repeats an earlier one is replayed as a no-op. The image cannot be deleted (CREATING forbids it) and its only version cannot be dropped either (last one). Two were wedged ~15 hours | `aws lambda-microvms list-microvm-image-builds` — all builds `PENDING` is the signature. Waiting does not help; record the identifier and build under a fresh `--name` | `docs/PLATFORM.md:474-489`, `microvms-core/src/control/image.rs:291-322`, `microvms-cli/src/exit.rs:352-355` |
| VM reaches a terminal state before `RUNNING`; client reports a connection error | A lifecycle hook failed. `PENDING → RUNNING → SUSPENDING/SUSPENDED → TERMINATING → TERMINATED`; anything terminal before `RUNNING` died during startup. The platform terminates the VM *before* forwarding any traffic, so the failure is invisible from outside and the VM is gone before you can look inside | `GetMicrovm`'s `stateReason` — it is the only evidence that survives the VM. The client already puts state and reason both in the message | `docs/PLATFORM.md:144-150`, `microvms-core/src/control/microvm.rs:399-413`, `microvms-cli/src/exit.rs:216-219` |
| Daemon answers 400, platform kills the VM with "Run lifecycle hook returned HTTP status 400" | `runHookPayload` arrives **wrapped**. The body is `{"runHookPayload": "{\"agent_token\": \"...\"}"}` — the caller's own JSON is one parse deeper. A daemon reading fields from the top level answers 400 | The daemon logs which of four things was wrong, each with its own `warn` line: not JSON, no `runHookPayload`, payload not an object with `agent_token`, `agent_token` empty | `docs/PLATFORM.md:37-52`, `agentd/src/routes.rs:176-198` |
| Build fails with `reason=unknown` and the log group is empty | The build role's log permissions, not a silent service. Logs go to `/aws/lambda-microvms/<image-name>` — **not** `/aws/lambda/microvms/*`, the plausible spelling. The wrong prefix produces builds with no logs at all, and the caller's own policy is discarding the evidence | `microvm logs <image-name>` names the group. Empty group + unknown reason = the prefix. Unknown alone is not the same as empty — a claim made without seeing the log list is a claim made without evidence | `docs/PLATFORM.md:491-499`, `microvms-core/src/control/image.rs:48-55`, `microvms-core/src/control/image.rs:380-393`, `microvms-cli/src/commands/local.rs:146-163` |
| A 45-minute build ends as a **run-hook timeout**, saying nothing about architecture | A host-architecture daemon binary. MicroVMs are ARM64-only — the service model's `Architecture` enum has exactly one member, `ARM_64` — so an x86-64 CMD cannot exec and surfaces only as the hook never answering | `microvm doctor --binary <path>`. It reads twenty bytes of ELF header and compares `e_machine` against `0xB7`; a shell-script CMD is caught as "not an ELF binary" | `microvms-cli/src/commands/doctor.rs:8-15`, `microvms-cli/src/commands/doctor.rs:283-291`, `docs/PLATFORM.md:85-92` |
| Every control request answers 503 | Not bootstrapped. The run hook has not landed, so the control API is closed. This is not 404 and not a dropped connection — the platform is about to deliver the token | `GET /v1/health` (unauthenticated on purpose, so it works in exactly this window) and read `bootstrapped`. Retry | `agentd/src/auth.rs:70-79`, `microvms-core/src/error.rs:244-248`, `agentd/src/routes.rs:309-315` |
| Requests answer 401 after a successful launch | The presented bearer is not the one the run hook installed. Fatal — waiting does not fix it. One historical cause: the run envelope published `agentToken: null` and the caller authenticated as the literal string `'None'` | Read the run envelope's `agentToken`. Without it `run --keep` hands back a VM you cannot exec into | `microvms-core/src/error.rs:220`, `microvms-cli/src/exit.rs:335-337`, `microvms-cli/src/commands/lifecycle.rs:350-355` |
| Daemon answers 400 "body is not a valid start request" on a request the fakes accepted | Auth-header injection that **replaced** rather than prepended the header vec, stripping the caller's content type. Fakes parse bodies without reading content-type, so 310 tests were green over a client that could not start an exec against the real daemon | Compare the recorded request's headers against what the caller set. Auth headers must prepend; the token-intent marker is the one thing dropped explicitly | `microvms-core/src/session/mod.rs:106-117`, `microvms-core/src/session/mod.rs:699-711` |
| Resume fails and no flag reopens it | The launch-time `idlePolicy` terminated the suspended VM once `suspendedDurationSeconds` passed. The client refuses *before* calling `ResumeMicrovm` because `suspendedDurationSeconds` exists only in the `RunMicrovm` request — `GetMicrovm` does not return it, so the client is the only party that can name the number | The error names the elapsed time and the window. A longer window is set at launch with `--suspended-sec`; no call extends the current one | `docs/PLATFORM.md:500-510`, `microvms-core/src/sandbox.rs:768-792`, `microvms-cli/src/exit.rs:359-361` |
| Healthy-looking VM whose hostname and `boot_id` match every sibling from the same snapshot | Identity repair half-failed. The daemon runs as root and that is still not enough: writing `/etc/machine-id` succeeds while `sethostname` and the bind mount over `/proc/sys/kernel/random/boot_id` both return `EPERM` without `additionalOsCapabilities=["ALL"]` at image creation. Repair logs the failure and keeps serving, which is correct — refusing would strand the VM | `GET /v1/health` → `identity_degraded`. The daemon's own CloudWatch logs name which step got `EPERM` | `docs/PLATFORM.md:160-189`, `agentd/src/main.rs:35-52`, `protocol/src/health.rs:35-39` |
| Writes fail; new work in the guest dies with `No space left on device` | Disk pressure past the reserve. `ENOSPC` arrives *after* the filesystem is full, by which point every other writer in the VM is broken too, and it arrives as a generic io error indistinguishable from "the daemon is broken" — and retrying, correct for the second, makes the first worse | `GET /v1/health` → `disk.under_pressure`. A refused write answers **507** with the free bytes in the body, not 500 and not 413. `disk: null` means unmeasurable, which is deliberately not the same as zero | `agentd/src/disk.rs:4-22`, `agentd/src/fs.rs:68-82`, `agentd/src/fs.rs:140-142`, `protocol/src/health.rs:21-33` |
| Exec came back with a killing signal rather than an exit code | A process was OOM-killed inside a living VM. Guest swap is absent (`SwapTotal: 0`), so pressure goes straight to the OOM killer with no paging phase. `minimumMemoryInMiB` picks a size class and the guest reports the class **peak** (4x the baseline), so pressure must be generated against what the guest reports | The exec result carries `signal` and `truncated`. In-guest: `dmesg` is readable with no extra privileges, and `/sys/fs/cgroup/memory.events` exposes `oom`, `oom_kill`, `oom_group_kill` — poll those rather than discovering the kill afterwards | `docs/PLATFORM.md:330-348`, `docs/PLATFORM.md:191-227`, `agentd/src/exec.rs:1184-1205` |
| Output bytes are simply gone from a stream | `OutputGap` — the replay ring evicted them, or this subscriber lagged the live channel. Not retryable; the bytes do not come back. Classified `ERR_PLATFORM` because it is not the caller's argument, not the daemon refusing, and not transient | The stream emits an explicit `gap` event with `from`/`to` byte offsets. A cursor moves past delivered bytes and past a gap, never past an exit | `microvms-core/src/error.rs:265-267`, `microvms-core/src/error.rs:392-395`, `protocol/src/exec.rs:212-219`, `microvms-core/src/session/sse.rs:252-267` |
| A long-running trial dies mid-flight with what looks like a dead daemon | An expired proxy token. The service caps a JWE at sixty minutes, shorter than a long agent run, and the resulting rejection is indistinguishable from a daemon that died | Minting happens inside the request path, and `DEFAULT_REFRESH_AFTER` is 30 minutes — **half** the ceiling, not 59 minutes, so a request in flight across the rollover still holds ~30 minutes of life. A mint failure is retryable on purpose so a throttle at minute thirty cannot kill a healthy trial | `docs/PLATFORM.md:457-471`, `microvms-core/src/session/proxy.rs:21-37`, `microvms-core/src/session/proxy.rs:62-67` |
| `terraform destroy` reported success and the account is still billing | The service creates `/aws/lambda-microvms/<image-name>` itself, so Terraform never owns it and destroy leaves it behind. Separately, images refuse deletion while their VMs are still terminating, so one teardown pass is not enough | `scripts/verify-clean` asks the account directly rather than trusting teardown, and separates leak / standing / pending. `microvm ls` alarms on every run whose ledger has a non-empty `leaked` list | `docs/PLATFORM.md:152-158`, `scripts/verify-clean:6-26`, `microvms-cli/src/ledger.rs:11-22`, `microvms-cli/src/main.rs:216-243` |
| A connection refused a second or two after the VM reached `RUNNING` | Expected, not a bug. The endpoint proxy path is not wired up the instant the state flips. Classified `Transport`, which is retryable because it says nothing about the daemon's state | Retry. If it persists past a few attempts, go back to the launch-died row and read `stateReason` | `microvms-core/src/error.rs:250-256` |
| Raw TLS handshake bytes on the plaintext port; logs read like an attack | Something in the platform's path probes the port with TLS before bootstrap. `code 400, message Bad request version ("\x13\x01\x13\x02...")` is a ClientHello reaching a plaintext HTTP server | Harmless. The correct response is a 400 and a debug-level log, and it must not take the listener down | `docs/PLATFORM.md:443-455` |
| Bootstrap replay answers 409 on a launch that is otherwise fine | Two different tokens, or a genuine conflict. An *identical* replay is answered 200 deliberately, because the platform may retry its own hook and a 409 there would fail a healthy launch | The daemon logs `bootstrap refused: a different token is already installed` for the real conflict and `identical bootstrap replay accepted` for the benign one | `agentd/src/routes.rs:200-215` |
| Client sees a transport error it cannot tell from a dead VM | A panicking handler. Without the outermost `CatchPanicLayer` the connection just drops; with it the client gets a 500 and the connection survives. It does not undo the panic — any `std::sync::Mutex` the handler held is now poisoned | Grep the daemon log for `recovering a poisoned lock`. Locks recover rather than propagate, because `.expect()` on a poisoned token lock closes the whole control API forever | `agentd/src/routes.rs:86-100`, `agentd/src/state.rs:8-42`, `agentd/src/state.rs:64-80` |

## Log and error surfaces

| Surface | Where it emits | What to grep for | Citation |
| --- | --- | --- | --- |
| Daemon structured log | JSON to **stdout**, which is where the platform's CloudWatch capture reads from. Level from `AGENTD_LOG`, defaulting to `info`. Targets on | `agentd listening` for the bind line with `addr` and `version`; `identity` lines for `EPERM` on repair; `recovering a poisoned lock` for a handler panic; `exec exceeded its timeout` | `agentd/src/main.rs:88-96`, `agentd/src/main.rs:79` |
| Build log group | `/aws/lambda-microvms/<image-name>`, created by the service. **Not** `/aws/lambda/microvms/*` | An *empty* group beside `reason=unknown` is the IAM-prefix signature, not a silent service | `microvms-core/src/control/image.rs:48-55`, `agentd/src/main.rs:84-87` |
| `microvm logs <image-name>` | stderr, as a **failure** rather than an empty success — deliberately, because an empty list reads as "there are no logs" when the real answer is "this client cannot read them" | The named group plus the `aws logs tail` invocation it hands you. CloudWatch is not in the transport's dependency set | `microvms-cli/src/commands/local.rs:139-163` |
| `GET /v1/health` | The daemon, unauthenticated on purpose so it answers during the pre-bootstrap window | `bootstrapped`, `disk.under_pressure`, `disk.available_bytes`, `identity_degraded`, `identity_repaired`. `disk: null` = unmeasurable, not zero | `agentd/src/routes.rs:278-301`, `protocol/src/health.rs:11-43` |
| CLI failure envelope | Exactly one JSON object on **stdout**. `finding`, `suggestions`, and `data` are unconditional keys — empty, never absent | `code` for the `ERR_*` string, `finding` for the `docs/PLATFORM.md` section, `data.kind` for the daemon-chosen status the exit code collapses, `data.leaked` for identifiers teardown could not delete | `microvms-cli/src/envelope.rs:20-30`, `microvms-cli/src/exit.rs:268-276` |
| CLI progress and warnings | **stderr**, always. `--quiet` suppresses progress and does **not** suppress warnings; exactly two things reach `warn` — a stale rate table and a leaked resource | A leak nobody is told about is the failure `--quiet` must not be able to purchase | `microvms-cli/src/envelope.rs:4-18` |
| `exec --stream` NDJSON | stdout, one object per event with the envelope **last** and written compact, under its own discriminant `microvm.exec.stream` | Branch on `type` — the first field — to know which parse applies. A `gap` event with `from`/`to` names lost bytes explicitly | `microvms-cli/src/envelope.rs:32-49`, `protocol/src/exec.rs:212-219`, `microvms-core/src/session/sse.rs:252-267` |
| Daemon error-body slug | The response body of every failing control route, as `{"error": "<slug>", "detail": "..."}` | The slug, which is the part a client branches on, paired with the status: `malformed_request` (400, never 404), `unknown_exec`, `spawn_failed` (500, deliberately not 404), `still_running`, `already_acked`, `stdin_not_requested` (409 — fixable at start time), `stdin_closed` (410 — a retry never succeeds), `stdin_write_timeout` (retryable, some bytes may already have landed), `stdin_write_too_large`, `stdin_write_failed` | `protocol/src/exec.rs:206-247` |
| Exit code in `$?` | The process. Fourteen rows, 0 through 13, append-only | The integer, then `Exit::row()`'s `meaning` for what to do next and `finding` for where it was measured. Thirteen distinct non-zero codes; no two rows share one | `microvms-cli/src/exit.rs:171-256`, `microvms-cli/src/exit.rs:55-66` |
| Run ledger on disk | A file per invocation, written **before** each delete is attempted, and refused deletion while `leaked` is non-empty | `leaked` — the operator's to-do list. For a `CREATING` image and a service-created log group the identifier *is* the remedy; there is no second way to find them | `microvms-cli/src/ledger.rs:1-22`, `microvms-cli/src/ledger.rs:43-45` |
| `microvm ls` | stdout grid | Rows marked as alarms and the trailing "N run(s), M with something still billing" | `microvms-cli/src/main.rs:216-243` |
| `scripts/verify-clean` | stdout, exit 0 clean / 1 leaked | Three outcomes: **leak** (still billing, nothing intends to keep it), **standing** (the Terraform stack, possibly on purpose), **pending** (a delete in flight — re-run in a minute) | `scripts/verify-clean:6-26` |
| `microvm doctor` | A **success** envelope with `ok: false` plus exit `ERR_PRECONDITION`. The check succeeded; it found what was wrong | `checks[]`, each with `name`, `ok`, `fatal`, `detail`, `remedy`. Advisory checks do not fail the run | `microvms-cli/src/commands/doctor.rs:59-79` |
| Guest OOM counters | In-guest, no extra privileges needed | `dmesg`; `/sys/fs/cgroup/memory.events` → `oom`, `oom_kill`, `oom_group_kill`. Poll these rather than discovering a kill after the fact | `docs/PLATFORM.md:330-348` |

## First-checks ladder

The steps are ordered cheapest first. Steps 1 through 3 cost nothing and make no AWS call.
Step 4 onward costs money or minutes.

1. **Read the exit code, then its row.** The integer alone is coarse by design, but the row
   carries a `meaning` (what to do next) and a `finding` (which `docs/PLATFORM.md` section
   measured this). `ERR_RETRYABLE` means run the identical command again; `ERR_CREDENTIALS`
   means no amount of waiting helps; `ERR_EXEC_FAILED` means the sandbox worked fine and your
   command exited non-zero. `microvms-cli/src/exit.rs:171-256`
2. **Read the envelope's `finding` and `suggestions`, and `data.kind` if there is one.** Both
   keys are always present. `data.kind` is the distinction the exit code deliberately
   collapses: `ERR_PROTOCOL` covers five wire kinds, so `Conflict` and `NotFound` arrive with
   the same integer and different `data.kind`. `microvms-cli/src/exit.rs:268-276`
3. **Run `microvm doctor`.** It is the only command that must work with nothing configured,
   and its check order matches the diagnosis order. The region check runs first, because a
   wrong region produces the null-message denial that reads as IAM. Next it checks whether
   the credential chain resolves at all; that check spends no API call, so `doctor` cannot
   fail on a throttle. It then checks the three Terraform outputs by name, then whether the
   stack is actually applied, and finally the daemon binary's architecture.
   `microvms-cli/src/commands/doctor.rs:36-57`
4. **If you are about to build: pass `--binary` to `doctor`.** The check reads twenty bytes
   of ELF header and compares them against `0xB7`. That comparison reports an architecture
   mismatch as a named failure. Without it, the same mismatch surfaces as a 45-minute build
   that ends in a run-hook timeout mentioning nothing about architecture.
   `microvms-cli/src/commands/doctor.rs:8-15`
5. **If a launch died: read `GetMicrovm`'s `stateReason` before anything else.** A VM that
   reached a terminal state before `RUNNING` died during startup, which for a hook-serving
   daemon almost always means a lifecycle hook failed. The `stateReason` is the only evidence
   that outlives the VM. Polling through the terminal states instead wastes minutes and then
   reports a connection error that hides the cause.
   `microvms-core/src/control/microvm.rs:395-413`
6. **If a build hangs: list the builds rather than waiting out the timeout.** All builds
   `PENDING` with `updatedAt` never advancing is the signature of a `clientToken` replay,
   and waiting will not help. An unreadable build list is not evidence of a wedge. If the
   list call was throttled, report the timeout rather than concluding anything from it.
   `microvms-core/src/control/image.rs:291-322`
7. **If the VM is up: `GET /v1/health`.** The route is unauthenticated on purpose, so it
   answers in the pre-bootstrap window where nothing else does. One call returns four
   signals: `bootstrapped` (503s everywhere means the run hook has not landed),
   `disk.under_pressure` (writes are about to be refused with 507), `identity_degraded`
   (repair half-failed and this VM shares its hostname and `boot_id` with every sibling),
   and `identity_repaired`. `agentd/src/routes.rs:278-301`
8. **Read the daemon's own log in `/aws/lambda-microvms/<image-name>`.** The daemon writes
   JSON to stdout, with the level taken from `AGENTD_LOG`. If that group is empty while a
   build reports `reason=unknown`, the cause is the build role's log prefix, not the
   service. `agentd/src/main.rs:84-96`
9. **The build logs survive the CLI's own teardown, so read them before you clean up.** This
   crate cannot delete a log group, because CloudWatch is not in its dependency set. Asking
   it to delete one therefore names the group in `TeardownReport.undeleted` rather than
   removing it. Reporting success instead "would report a clean teardown over six accumulated
   log groups, which is how the leak was found in the first place." That naming is your last
   chance to read the logs before `verify-clean --delete` removes them.
   `microvms-core/src/sandbox.rs:231-236`, `microvms-core/src/sandbox.rs:298-303`
10. **Before you walk away: `scripts/verify-clean`.** Teardown reporting success and the
    account being clean are different questions, and the difference has cost this project
    twice. Expect to run `--delete` more than once, because an image refuses deletion while
    its VM is still terminating. `scripts/verify-clean:6-26`

## Known incident patterns

The source contains no `INCIDENT`, `POSTMORTEM`, `FLAKY`, or `KNOWN BUG` comment tags. The
history is recorded in `docs/PLATFORM.md`, in `EXIT_TABLE`'s `finding` column, in git commit
bodies, and in `.erpaval/solutions/`. The patterns below recur across that history.

- **The green run that never exercised the thing.** This is the most common pattern in this
  project's history. The first OOM probe allocated with `python3`, which
  `amazonlinux:2023-minimal` does not have. The probe reported `command not found` with exit
  127, every downstream check passed, and the run looked clean while measuring nothing.
  Signal: a suite that passes while a condition you expected to observe never appears.
  Mitigation: assert on the verdict, not on the absence of failure. `docs/PLATFORM.md:349-362`
- **The fake more forgiving than the real server.** 310 fake-backed tests were green over a
  client whose auth-header injection replaced the header vec and stripped the caller's
  content type. The real axum extractor answered 400. The fakes parsed bodies without
  reading content-type at all, so they accepted the broken request. Signal: a live tier
  failing a request every local tier accepts. Mitigation: a fake that accepts what the real
  server rejects converts integration bugs into production bugs, so tighten the fake to
  match the real server. `microvms-core/src/session/mod.rs:106-117`,
  `.erpaval/solutions/test-failures/guards-that-passed-against-broken-code.md:38-45`
- **The guard that was never watched failing.** In one session, guards passed against
  deliberately broken code in five distinct ways. A bare ` ```compile_fail ` block passes for
  any build error, including a typo in the doctest. A fake that models the failure event
  cannot catch lateness, because a client refreshing too late presents a token with no life
  left rather than an expired one. Uniform proptest draws essentially never land in the
  narrow band where a rounding bug lives. And a guard test can require the very divergence
  it should catch. Signal: a guard you have never seen red. Mitigation: break the invariant,
  watch the guard fail, then restore the code.
  `.erpaval/solutions/test-failures/guards-that-passed-against-broken-code.md:12-37`
- **Verified in every tier except the one that mattered.** Identity repair had two of three
  steps failing in every real VM without any test noticing. Unit tests inject a tempdir
  layout and a fake platform, so they cannot observe a missing kernel capability. Signal: a
  probe reporting `identity_degraded: true` that the conformance suite could not see, because
  the suite never asserted on the field. Mitigation: assert on the field, and pick the tier
  that can observe the behavior. `docs/PLATFORM.md:186-189`
- **The wrong-cause report.** This is a family of failures where the error points at the
  wrong cause. A null-message `AccessDeniedException` sends someone to audit an IAM policy
  that is fine. A build's `reason=unknown` reads as the service failing to populate
  `stateReason` when the caller's own log prefix is discarding the evidence. An expired
  proxy token is indistinguishable from a daemon that died. A 404 where a 400 belongs reads
  as a missing file, which is how one defect hid for a review round. Mitigation, applied
  throughout: the error message names the finding inline, and `WireKind::from_status` has
  no generic 4xx fallback, so an unexpected status cannot be misread as a known one.
  `microvms-core/src/error.rs:336-356`, `microvms-core/src/error.rs:222-226`
- **A constraint restated by hand, and restated wrong.** `STRATEGY.md` and `TRUST.md` both
  claimed a 16 KB `runHookPayload` ceiling. The real figure is 4096 bytes, a quarter of the
  claim. The error ran in the dangerous direction, telling a reader they could fit four
  times the secret material they actually can. The number was machine-readable in the
  botocore service model the whole time. Mitigation: `scripts/check-model-drift`, wired into
  `mise run check`, fails mechanically when a documented constraint no longer matches the
  shipped model. `docs/PLATFORM.md:54-101`
- **Teardown succeeded and the account kept billing.** `terraform destroy` once reported nine
  resources destroyed while six service-created log groups survived, because Terraform never
  owned them. Separately, an image deletion retried past the point where the log-group delete
  had already run. And `verify-clean` itself once reported clean while a CLI run's log group
  billed, because the prefix list did not know the `microvm-cli` name. A missing prefix
  converts an unknown into a false assurance, which is worse than having no checker.
  Mitigation: query the account independently of the code that did the cleanup, and keep the
  prefix list complete. `scripts/verify-clean:6-26`, `scripts/verify-clean:39-50`
- **Silent until the disk is already full.** In anthropics/claude-code#59856, cited by number
  in source, a sandbox accumulated 121 never-collected session directories and filled two
  10 GB disks to 100%. The first symptom was `useradd: No space left on device`, and by that
  point every writer in the sandbox was broken, including the ones that cannot report
  anything. The contributors were ordinary: a 956 MB cache re-downloaded per run, and an
  unbounded journal. Mitigation: refuse a write before it starts if it would cross a
  reserve, answer 507 with the free bytes, and put the number on `/v1/health` so the curve
  is visible while there is still time. `agentd/src/disk.rs:4-22`,
  `protocol/src/health.rs:21-33`
- **A hostile header that killed the handler.** The Python predecessor's `hmac.compare_digest`
  raised `TypeError` on a `str` containing non-ASCII, so `Bearer tökén` took down the handler
  thread and the client got `RemoteDisconnected` instead of a status it could act on. Any
  caller controls that header. Mitigation: the comparison now runs on raw bytes and never
  decodes. Authorization is also decided before a single body byte is read, because the
  predecessor buffered first, which let an unauthorized request force a 256 MB allocation on
  a 512 MiB VM. `agentd/src/auth.rs:6-14`
- **A security control that breaks the platform.** Both the platform's lifecycle hooks and
  the harness's control requests arrive from `127.0.0.1`, because the endpoint proxy
  terminates outside the VM and forwards over loopback. A source-address rule rejecting
  loopback callers on the bootstrap route would therefore reject the platform's own
  legitimate bootstrap and break every launch. An attempt at such a rule broke 39 tests, and
  those failures were reporting a real defect rather than a harness artifact.
  `docs/PLATFORM.md:418-441`

## See also

- [microvms-agentd · Processes](../behavior/processes.md)
- [microvms-agentd · Contract map](contract-map.md)
- [microvms-agentd · Business logic](business-logic.md)
- [microvms-agentd · Data flow](../architecture/data-flow.md)
- [microvms-agentd · Impact analysis](impact-analysis.md)
