# microvms-agentd · Debugging guide

Something is broken. This tells you where to look first.

Almost every failure mode below was found against real AWS after the offline tiers were
green, so the operational knowledge is already written down in four places rather than in
anyone's head:

- `docs/PLATFORM.md` — measured platform behavior, each entry carrying a date, a region, and an
  API version. Contradictions are appended rather than deleted, so the file reads as a log of
  surprises.
- `EXIT_TABLE` in `microvms-cli/src/exit.rs:173-258` — fourteen rows. Each carries an integer, an
  `ERR_*` code, a `meaning` (what to do next), and a `finding` (the `docs/PLATFORM.md` section
  that measured it). The `finding` column turns an exit code into a documentation lookup.
- Trap messages in the library. `microvms-core` writes the finding into the message itself, so
  the failure explains itself without a doc lookup — see
  `microvms-core/src/control/image.rs:371-386`.
- `.erpaval/solutions/` — twelve compounded lessons, each the conclusion of a real debugging
  session.

No `.rs` or `.py` file in the workspace carries a `TODO`, `FIXME`, `HACK`, `INCIDENT`, or
`POSTMORTEM` marker; the repo convention is that comments record constraints and defects
defended against rather than narration. The four sources above are the history.

Two operational facts before you start. `mise run check` is the free offline gate — lint,
security, all six Rust tiers, schema and stub freshness, model drift, live wiring, and the
release cross-compile (`mise.toml:290-301`). `mise run live` is BILLABLE, takes about fifteen
minutes against real AWS, and is never a first debugging step (`mise.toml:428-429`); after any
live run, teardown is verified separately by `mise run live:verify-clean`
(`mise.toml:416-426`), because the service creates log groups under
`/aws/lambda-microvms/` that outlive `terraform destroy` (`docs/PLATFORM.md:195-201`).

## Failure-mode index

| Symptom | Likely surface | First check | Citation |
| --- | --- | --- | --- |
| `AccessDeniedException` whose `message` field is null | Not IAM. The region does not price MicroVMs — only five do. The service model's `endpointPrefix` is `lambda`, so a client constructs and resolves for any region and the first API call is the only reporter | Read the `message` field. A real denial names the principal and the action; this one is `None`. Then `microvm doctor`, whose region check runs first for exactly this reason | `docs/PLATFORM.md:146-168`, `microvms-core/src/region.rs:73`, `microvms-cli/src/exit.rs:340-344` |
| Image stuck in `CREATING`; builds never start; `updatedAt` never advances past `createdAt` | The `clientToken` replay. A `clientToken` is a permanent idempotency key, so a create whose token repeats an earlier one is replayed as a no-op. The image cannot be deleted (`CREATING` forbids it) and its only version cannot be dropped either. Two were wedged about 15 hours | `ListMicrovmImageBuilds` — every build `PENDING` is the signature. Record the identifier and build under a fresh `--name`; waiting does not help | `docs/PLATFORM.md:549-565`, `microvms-core/src/control/image.rs:366-386`, `microvms-cli/src/exit.rs:354-357` |
| A VM reaches a terminal state before `RUNNING`; the client reports a connection error | A lifecycle hook failed. `PENDING → RUNNING → SUSPENDING/SUSPENDED → TERMINATING → TERMINATED`; anything terminal before `RUNNING` died during startup, and the platform terminates it before forwarding any traffic | `GetMicrovm`'s `stateReason` — the only evidence that outlives the VM. The client already puts the state and the reason both in the message | `docs/PLATFORM.md:186-193`, `microvms-core/src/control/microvm.rs:482-499`, `microvms-cli/src/exit.rs:216-221` |
| `CREATE_FAILED` with a fully green build log, every docker layer succeeding, and no error line anywhere | The guest's `AGENTD_PORT` disagrees with the create call's `hooks.port`. The build-time `ready` and `validate` hooks are dialled on the create call's port, so a daemon listening elsewhere answers none of them. An unset `AGENTD_PORT` is the same failure, with nothing in the Dockerfile to point at | Fetch `GetMicrovmImageVersion` and compare `hooks.port` against the Dockerfile's `ENV AGENTD_PORT`. `GetMicrovmImage` structurally cannot say why | `docs/PLATFORM.md:1328-1350`, `agentd/src/config.rs:116-120`, `.erpaval/solutions/architecture-patterns/an-absent-value-is-not-a-neutral-one.md:19-24` |
| A 45-minute build ends as `Ready hook invocation timed out after PT5M`, saying nothing about architecture | A host-architecture daemon binary. MicroVMs are ARM64-only, so an x86-64 `CMD` cannot exec and surfaces only as the hook never answering | `microvm doctor --binary <path>`. It reads twenty bytes of ELF header and compares `e_machine` against `0xB7`; a script or wrapper is caught as "not an ELF binary" | `microvms-cli/src/commands/doctor.rs:8-15`, `microvms-cli/src/commands/doctor.rs:25-29`, `microvms-cli/src/commands/doctor.rs:422-436` |
| Every failed build reports `reason=unknown` and the log group holds nothing at all | The build role's log permissions, not a silent service. Logs go to `/aws/lambda-microvms/<image-name>`, not the plausible `/aws/lambda/microvms/*`. The caller's own policy is discarding the evidence | `microvm logs <image-name>` names the group; an empty group beside `reason=unknown` is the prefix signature. Unknown alone is not the same as empty | `docs/PLATFORM.md:567-575`, `microvms-core/src/control/image.rs:55`, `agentd/src/main.rs:84-87` |
| A build says `The container image build failed.` and nothing else | `stateReason` lives on the **build** only. `GetMicrovmImage` reports `CREATE_FAILED` with no reason member at all, and `ListMicrovmImageVersions` reported `null` across three separate failures | `ListMicrovmImageBuilds`, and expect a **list**: each failed version produced two builds, one per Graviton generation, with identical reasons. Then read `snapshotBuild`'s shape — absent means the Dockerfile broke before anything installed, `codeInstallSizeInBytes` alone means code installed and the daemon never became ready | `docs/PLATFORM.md:577-605`, `docs/PLATFORM.md:1283-1303` |
| Every control request answers 503 | Not bootstrapped. The run hook has not landed, so the control API is closed. Deliberately not 401 (which sends a client chasing credentials) and never 404 (which clients map onto "file not found") | `GET /v1/health` — unauthenticated on purpose so it answers in exactly this window — and read `bootstrapped`. `NotBootstrapped` is retryable: the platform is about to deliver the token | `agentd/src/auth.rs:69-80`, `microvms-core/src/error.rs:244-248`, `protocol/src/health.rs:20` |
| Requests answer 401 after a successful launch | The presented bearer is not the one the run hook installed. Fatal — waiting does not fix it, and it is classified `ERR_CREDENTIALS` rather than `ERR_PROTOCOL` for that reason | Read the `run` envelope's `agentToken`. The `ERR_CREDENTIALS` remedy differs for this case and for an unresolvable credential chain, so read the `suggestions` line rather than assuming | `agentd/src/auth.rs:74-79`, `microvms-core/src/error.rs:220-221`, `microvms-cli/src/exit.rs:337-339` |
| `resume` is refused and no flag reopens the window | The launch-time `idlePolicy` terminated the suspended VM once `suspendedDurationSeconds` passed. The client refuses before calling `ResumeMicrovm` | The error names the elapsed time and the window. A longer window is set at launch with `--suspended-sec`; no call extends the current one. The policy is also readable from `GetMicrovm`, which returns all three members in `RUNNING` and in `SUSPENDED` alike | `microvms-core/src/sandbox.rs:900-925`, `docs/PLATFORM.md:612-621`, `docs/PLATFORM.md:652-674` |
| A multi-hour agent run is auto-suspended while it is busy | Idleness is measured by inbound traffic through the endpoint proxy, and that proxy terminates outside the VM. Traffic a guest process sends to the daemon's own port never crosses the thing doing the measuring, so an in-VM keep-alive is not implementable | Poll `GET /v1/health` from **outside** the VM. Measured: a polled VM stayed `RUNNING` through 311 seconds against a 60-second window while an unpolled control suspended at 66 | `docs/PLATFORM.md:623-650`, `agentd/src/routes.rs:314-338` |
| A long-running trial dies mid-flight with what looks like a dead daemon | An expired proxy token. The service caps a JWE at sixty minutes, shorter than a long agent run, and the rejection is indistinguishable from a daemon that died | Minting happens inside the request path and `DEFAULT_REFRESH_AFTER` is thirty minutes — half the ceiling rather than marginally under it, so a request in flight across the rollover still holds about thirty minutes of life. A mint failure is retryable on purpose | `docs/PLATFORM.md:501-514`, `microvms-core/src/session/proxy.rs:21-37`, `microvms-core/src/session/proxy.rs:111` |
| Writes are refused with **507** naming byte counts | Disk pressure past the configured reserve. 507 rather than 500 deliberately: a 500 is indistinguishable from a daemon defect, so a client retries it, which is correct for a defect and actively harmful for a full disk | Read the free-space numbers in the response body, then `GET /v1/health` → `disk.under_pressure`. `disk: null` means unmeasurable, which is deliberately not zero | `agentd/src/fs.rs:97-124`, `agentd/src/disk.rs:142-159`, `protocol/src/health.rs:21-34` |
| An exec result carries `truncated: true` | The per-stream output cap. Default 8 MiB, sized well under a 512 MiB baseline VM, because an OOM-killed daemon is unrecoverable — there is no supervisor inside the VM to restart it | `AGENTD_MAX_OUTPUT_BYTES` against the volume the command emits. Past the cap the daemon keeps reading and discarding rather than stopping, so the writer never blocks in the kernel | `agentd/src/config.rs:25-27`, `agentd/src/config.rs:87`, `protocol/src/exec.rs:66-73` |
| An exec result carries `writers_may_be_alive: true` and the output looks cut short | A grandchild still holds the inherited pipe past `output_linger`. EOF arrives when the *last* writer closes, so a command that backgrounds a server or a log tailer keeps writing after the direct child exits | `AGENTD_OUTPUT_LINGER_SECS` (default 5). Under a simulated-time test this flag is a false positive — see the two-clocks incident below | `agentd/src/config.rs:28-31`, `agentd/src/exec.rs:1259-1296`, `.erpaval/solutions/best-practices/pipes-not-tempfiles-for-subprocess-output.md:11-27` |
| A stream delivers a `gap` event with `from`/`to` offsets | The subscriber lagged the bounded broadcast channel, or the replay ring evicted the bytes. Classified `ERR_PLATFORM` and not retryable — the bytes are gone. The gap is a typed event rather than a log line precisely so a cursor cannot advance silently past dropped data | Re-GET from the last offset actually received. Then check `AGENTD_STREAM_CHANNEL_CAPACITY` and `AGENTD_STREAM_BUFFER_BYTES` against the output rate | `agentd/src/exec.rs:617-630`, `microvms-core/src/error.rs:265-267`, `microvms-core/src/error.rs:395` |
| A stdin write answers 409, or 410, or 408 — and the three mean different things | 409 `Conflict` is "you did not ask for stdin", fixed at start time. 410 `StdinClosed` is a lifecycle fact: EOF already arrived or the child stopped reading, and a retry never succeeds. 408 `RequestTimeout` is the child not draining within the write timeout — retryable, and some bytes may already have landed | Read `data.kind`, not the exit code: all three collapse onto `ERR_PROTOCOL` except 408, which is `ERR_RETRYABLE`. The daemon keeps its stdin handle open across a 408 so a retry can succeed | `microvms-core/src/error.rs:229-243`, `agentd/src/config.rs:58-62`, `protocol/src/exec.rs:276-286` |
| A tar upload answers 400 naming one member | That member violated the data-filter contract — an escaping path, a symlink out of the root, a refused type. The refused name travels with the refusal, because a 400 saying only "bad archive" sends the caller re-reading their whole tree | Read the member name in the body. 413 is a different answer (over `max_tar_members` or `max_tar_bytes`) and 507 a third (the filesystem filled partway through) | `agentd/src/fs.rs:126-142`, `agentd/src/fs.rs:158-182`, `agentd/src/config.rs:37-40` |
| A `nameFilter` listing returns an empty first page while matches exist | `maxResults` is applied **before** `nameFilter`, so the service pages over the unfiltered collection and then filters the page. Measured: `nameFilter=bonk&maxResults=1` over 22 images took 26 pages to yield the 10 matches, and page one held zero items | The only termination condition is an absent `nextToken`. A loop that stops on an empty `items` finds nothing at all. `nameFilter` is also a substring match, so the exact-name comparison has to happen client-side across every page | `docs/PLATFORM.md:704-723` |
| `403 AccessDeniedException` naming an image resource that exists and is permitted | The ARN separator. Customer image ARNs use a **colon** before the name; the slash form is evaluated by IAM as a resource no policy matches, so the answer is a permissions message about a resource that is fine | Read the separator before widening any policy. An *unencoded* slash is a third failure: the raw `/` splits into extra path segments and the gateway answers 404 with an HTML body | `docs/PLATFORM.md:747-767` |
| A 5xx on `GetMicrovmImage` with an nginx HTML body | The gateway in front of the service, not the service. Observed once where an immediate hand-signed repeat of the identical URL answered 403 five times out of five | Retry past a 5xx on this operation and never past a 4xx — a 4xx is the answer | `docs/PLATFORM.md:769-776` |
| A poll against a running exec returns `phase: running` with no partial stdout | Polling is terminal-only by design. A detached exec does survive the 60-minute proxy-token ceiling — 450 of 450 ticks recovered across the boundary, the straddling pair a nominal 10-second gap — but its output is readable only at the end | Stream the exec, or have the command write to a file and fetch that file. A 75-minute detached exec also needs outside polling, or the idle window suspends it regardless of how healthy the exec is | `docs/PLATFORM.md:1192-1227` |
| A client sees a transport error it cannot tell from a dead VM | A panicking handler. Without the outermost `CatchPanicLayer` the panic reaches hyper and the connection drops; with it the client gets a 500 and the connection survives. It does not undo the panic — any `std::sync::Mutex` the handler held is now poisoned | Grep the daemon log for `recovering a poisoned lock`. Locks recover rather than propagate, because `.expect()` on a poisoned token lock closes the whole control API forever | `agentd/src/routes.rs:86-101`, `agentd/src/state.rs:73-92`, `agentd/tests/panic_guard.rs:11-26` |
| A connection is refused a second or two after the VM reaches `RUNNING` | Expected. The endpoint proxy path is not wired up the instant the state flips. Classified `Transport`, retryable because it says nothing about the daemon's state | Retry. If it persists past a few attempts, go to the terminal-state row and read `stateReason` | `microvms-core/src/error.rs:251-256` |
| `resume` returns 200 but the control API stays closed | The VM resumed without an installed token, which contradicts the measured suspend/resume behavior — the in-memory token, the filesystem, exec records, and backgrounded processes all survive a normal cycle | Grep the daemon log for `resumed WITHOUT an installed token`. That line means the resume behaved like a cold start and every in-flight exec record is gone | `agentd/src/routes.rs:276-290`, `docs/PLATFORM.md:418-451` |
| A Node caller reads `err.code` and gets `GenericFailure` | napi-rs types the async path over its own closed `Status` enum, so a custom code survives a synchronous throw and is collapsed on a Promise rejection. Nearly every binding method is async | Read `err.cause.message` for the `ERR_*` code and `err.cause.cause.message` for the fine-grained wire kind | `.erpaval/solutions/api-patterns/napi-async-collapses-error-codes.md:11-19` |
| `terraform destroy` reports success and the account is still billing | The service creates `/aws/lambda-microvms/<image-name>` itself, so Terraform never owns it. Separately, an image refuses deletion while its VM is still terminating, so one teardown pass is not enough | `mise run live:verify-clean` asks the account directly and separates leak / standing / pending. `microvm ls` alarms on every run whose ledger has a non-empty `leaked` list | `docs/PLATFORM.md:195-201`, `scripts/verify-clean.py:7-28`, `microvms-cli/src/main.rs:222-246` |

## Log and error surfaces

| Surface | Where it emits | What to grep for | Citation |
| --- | --- | --- | --- |
| Daemon structured log | JSON to **stdout**, which is where the platform's CloudWatch capture reads from. Level from `AGENTD_LOG`, defaulting to `info`; targets on | `agentd listening` for the bind line with `addr` and `version`; `recovering a poisoned lock` for a handler panic; `exec exceeded its timeout and its process group was signalled`; `process group survived SIGTERM; escalating to SIGKILL`; `ignoring unparseable configuration value` for a bad `AGENTD_*` | `agentd/src/main.rs:84-97`, `agentd/src/main.rs:79` |
| Daemon `warn` sites | The same stdout JSON stream. 38 `warn`/`error` calls, all in the daemon — `agentd/src/fs.rs` 19, `agentd/src/exec.rs` 14, `agentd/src/routes.rs` 13, and one to four each in `serve`, `main`, `identity`, `config`, `state`, `disk` | `tar member refused`, `archive over cap`, `refusing a write: the target filesystem is under the disk reserve`, `exec stream subscriber lagged`, `spawn failed`, `bootstrap refused: a different token is already installed`, `run hook body is not JSON` | `agentd/src/fs.rs:161-178`, `agentd/src/exec.rs:624`, `agentd/src/routes.rs:186-231` |
| Build log group | `/aws/lambda-microvms/<image-name>`, created by the service. Not `/aws/lambda/microvms/*` | An *empty* group beside `reason=unknown` is the IAM-prefix signature, not a silent service | `microvms-core/src/control/image.rs:55`, `microvms-core/src/control/image.rs:76-81` |
| `microvm logs <image-name>` | A success that names the group and hands you the read: `data.tailCommand` is the working `aws logs tail` invocation (AWS CLI v2 only — the subcommand does not exist in v1), and `data.lines` is explicitly `null`, never `[]`, because an empty list reads as "there are no logs" when this client did not read the group. CloudWatch is not in the transport's dependency set; the read runs under your own identity, granted by the stack's `logs_read_policy_arn` | `data.logGroup`, `data.tailCommand`, `data.tailRequires`, `data.streams` | `microvms-cli/src/commands/local.rs:228` |
| `GET /v1/health` | The daemon, on the unauthenticated router so it answers during the pre-bootstrap window | `bootstrapped`, `disk.under_pressure`, `disk.available_bytes`, `identity_degraded`, `identity_repaired`, `busy`, `execs`. `disk: null` means unmeasurable, not zero | `agentd/src/routes.rs:314-338`, `protocol/src/health.rs:11-45` |
| `GET /v1/schema` and `docs/schema.json` | The daemon serves the same list the router is assembled from, so a route cannot be served unless it appears in the list and a listed route with no handler panics at startup | The 18 endpoints and their `auth` field, which is what splits the Bearer-guarded router from the open one | `agentd/src/routes.rs:36-59`, `agentd/src/routes.rs:110-139` |
| `microvms-agentd-version` response header | Stamped on **every** response by middleware applied outside `route_layer` — handler bodies, the auth middleware's 401/503, the body-limit layer's 413, and the 404 fallback | The header's presence. A version header a client only sometimes receives is one it cannot use as a precondition | `agentd/src/routes.rs:142-164` |
| Daemon error-body slug | The response body of a failing control route, as `{"error": "<slug>", "detail": "..."}` | The slug paired with the status: `malformed_request`, `unknown_exec`, `spawn_failed`, `still_running`, `already_acked`, `stdin_not_requested`, `stdin_closed`, `stdin_write_timeout`, `stdin_write_too_large`, `stdin_write_failed` | `protocol/src/exec.rs:266-286` |
| Exec result flags | The poll and stream payloads | `truncated` (the per-stream cap was hit) and `writers_may_be_alive` (the linger deadline expired with a writer still holding the pipe). Both are explicit rather than inferred from a short log | `protocol/src/exec.rs:66-73` |
| SSE event names | The `/v1/exec/{id}/stream` frames | `output`, `gap`, `exit`. A `gap` frame carries `from`/`to` byte offsets and is the only honest report of lost bytes | `protocol/src/exec.rs:256-258`, `agentd/src/exec.rs:656-658` |
| CLI failure envelope | Exactly one JSON object on **stdout**. `finding`, `suggestions`, and `data` are unconditional keys — present and empty, never absent | `code` for the `ERR_*` string, `exitCode` (which must agree with `$?`), `finding` for the `docs/PLATFORM.md` section, `data.kind` for the daemon-chosen status the exit code collapses, `data.leaked` for identifiers teardown could not delete | `microvms-cli/src/envelope.rs:320-339`, `microvms-cli/src/exit.rs:260-278` |
| CLI human failure rendering | stdout on the plain path. Deterministic: sorted data keys, byte-identical across two renders | First line `error ERR_*: <message>`; then `see docs/PLATFORM.md, '<finding>'`; then `hint:` lines; then `<key>: <value>` per sorted data key | `microvms-cli/src/envelope.rs:345-377` |
| CLI progress and warnings | **stderr**, always, so stdout stays exactly one document. `--quiet` suppresses progress and never a warning | A leak warning survives `--quiet`, because a leak nobody is told about is the one thing silence must not buy | `microvms-cli/src/envelope.rs:481-493`, `microvms-cli/tests/exit_codes.rs:154-190` |
| `exec --stream` NDJSON | The one invocation allowed more than one object on stdout: every line before the last is an event, the last is the envelope, under its own discriminant `microvm.exec.stream` | Branch on `type`. A streamed exec that fails before any event writes exactly one document | `microvms-cli/tests/exit_codes.rs:232-278` |
| Exit code in `$?` | The process. Fourteen rows, 0 through 13, append-only, `#[repr(u8)]` with explicit discriminants so a variant inserted in the middle cannot silently renumber the contract | The integer, then `Exit::row()`'s `meaning` and `finding`. Thirteen distinct non-zero codes; no two rows share one | `microvms-cli/src/exit.rs:78-102`, `microvms-cli/src/exit.rs:173-258` |
| Run ledger on disk | One JSON file per invocation under `$MICROVM_STATE_DIR`, else `~/.microvm/runs`. Written **before** each delete is attempted, and its file is refused deletion while `leaked` is non-empty | `leaked` — the operator's to-do list. For a `CREATING` image and a service-created log group the identifier *is* the remedy, because there is no second way to find them. A write failure is swallowed, so an unwritable state dir costs the `ls` entry and nothing else | `microvms-cli/src/ledger.rs:1-22`, `microvms-cli/src/ledger.rs:37-49`, `microvms-cli/src/seam.rs:450-459` |
| `microvm ls` | stdout. Rows marked as alarms plus a trailing count | "N run(s), M with something still billing" | `microvms-cli/src/main.rs:209-247` |
| `microvm doctor` | A **success** envelope with `ok: false` plus exit `ERR_PRECONDITION`, because the check succeeded — it found what was wrong | `checks[]` per named check. Advisory checks do not fail the run; the fatal ones do | `microvms-cli/src/commands/doctor.rs:62-83` |
| `mise run live:verify-clean` | stdout, exit 0 clean and 1 leaked | Three outcomes, not two: **leak** (still billing and nothing intends to keep it), **standing** (the Terraform stack, possibly on purpose), **pending** (a delete in flight — re-run in a minute) | `scripts/verify-clean.py:7-28`, `mise.toml:416-426` |
| Guest OOM counters | In-guest, readable with no extra privileges | `dmesg`, and `/sys/fs/cgroup/memory.events` → `oom`, `oom_kill`, `oom_group_kill`. Poll these rather than discovering a kill after the fact | `docs/PLATFORM.md:375-393` |

## First-checks ladder

Cheapest first. Steps 1 through 6 cost nothing and make no billable AWS call. Step 10 spends
money.

1. **Read the exit integer, then its row.** The integer is coarse by design; the row carries
   the `meaning` (what to do next) and the `finding` (which measurement explains it).
   `ERR_RETRYABLE` means run the identical command again, `ERR_CREDENTIALS` means waiting
   never helps, and `ERR_EXEC_FAILED` means the sandbox worked and your command exited
   non-zero — which is the one non-zero exit that says nothing is wrong with the platform.
   `microvms-cli/src/exit.rs:173-258`
2. **Read the envelope's `finding`, `suggestions`, and `data.kind`.** All three keys are
   always present. `data.kind` is the distinction the exit code deliberately collapses: five
   wire kinds share `ERR_PROTOCOL`, so a 400 and a 409 arrive with the same integer and
   different `data.kind`. Two failures sharing `ERR_CREDENTIALS` also get different
   `suggestions`, so read the line rather than assuming which one you have.
   `microvms-cli/src/exit.rs:336-365`
3. **Run `microvm doctor --binary target/aarch64-unknown-linux-musl/release/agentd`.** It is
   the only command that must work with nothing configured, and its check order is the
   diagnosis order: region first (a wrong region produces the null-message denial that reads
   as IAM), then whether the credential chain resolves at all — which spends no API call, so
   `doctor` cannot fail on a throttle — then the three Terraform outputs by name, then whether
   the stack is actually applied, then the managed bases, then the binary's architecture last
   because it is the one failure that costs a full build cycle.
   `microvms-cli/src/commands/doctor.rs:36-60`
4. **Do not trust `terraform.tfstate` on disk as evidence the stack exists.** A destroyed
   stack leaves the file behind with an empty resource list, which is exactly the state that
   produces "bucket does not exist" three minutes into a build. `doctor` asks
   `terraform output` instead, which needs no credentials.
   `microvms-cli/src/commands/doctor.rs:333-341`
5. **Run `microvm ls` before anything else touches the account.** A non-empty `leaked` list
   from an earlier invocation is both a bill and a clue — the ledger is written before each
   delete is attempted, so the identifiers survive a process that died inside the call.
   `microvms-cli/src/ledger.rs:11-22`
6. **Run `mise run check`.** It is offline, free, and the definition of done: lint, security,
   all six Rust tiers, `schema:check`, `stubs:check`, `model:check`, `live:check`, and the
   release cross-compile. A drifted generated artifact — the served schema, the Python stub,
   a hardcoded API constraint against botocore's model — fails here rather than in
   production. `mise.toml:290-301`
7. **If the VM is reachable: `GET /v1/health`.** One call answers six questions.
   `bootstrapped` false plus 503s everywhere means the run hook has not landed;
   `disk.under_pressure` means writes are about to be refused with 507; `identity_degraded`
   means this VM still shares a value from the image with every sibling restored from the same
   snapshot; `busy` and `execs` say whether an outside poll should keep the VM alive.
   `agentd/src/routes.rs:314-338`
8. **Read the daemon's own log in `/aws/lambda-microvms/<image-name>`, raising `AGENTD_LOG` if
   `info` is not enough.** The daemon writes JSON to stdout and the platform captures it from
   there. If that group is empty while a build reports `reason=unknown`, the cause is the build
   role's log prefix and not the service. `agentd/src/main.rs:84-97`
9. **If a build failed: fetch the reason from `ListMicrovmImageBuilds`, and read
   `snapshotBuild`'s shape beside it.** `GetMicrovmImage` structurally cannot say why, and
   `ListMicrovmImageVersions` said nothing across three measured failures. Expect a list of
   builds, one per Graviton generation. A missing `snapshotBuild` means the Dockerfile broke
   before anything installed; `codeInstallSizeInBytes` alone with no snapshots means code
   installed and the daemon never became ready, which points at the daemon rather than the
   build. `docs/PLATFORM.md:577-605`, `docs/PLATFORM.md:1283-1303`
10. **Only now spend money: `mise run live`, then `mise run live:verify-clean`.** The live
    tier is billable and takes about fifteen minutes, and it is the only tier that can catch a
    fake more forgiving than the real daemon. Teardown reporting success and the account being
    clean are different questions, so the leak check runs independently of the code that did
    the cleanup, and expect to run `--delete` more than once because an image refuses deletion
    while its VM is still terminating. `mise.toml:428-429`, `scripts/verify-clean.py:7-28`

## Known incident patterns

These recur. Each is recorded in `.erpaval/solutions/` or in `docs/PLATFORM.md` with a date.

- **The green run that measured nothing:** the most common pattern in this project's history.
  The first OOM probe allocated with `python3`, which `amazonlinux:2023-minimal` does not have;
  it reported `command not found` with exit 127 and every downstream check passed. The second
  allocated with `dd` into `/dev/shm`, which is tmpfs and capped near half of RAM, so `dd`
  stopped at 64 MiB against a 1 GiB request and exited 0. Signal: a suite that passes while a
  condition you expected to observe never appears. Mitigation: assert on the verdict computed
  from the input, never on the absence of failure. `docs/PLATFORM.md:395-412`
- **Containment without a verdict:** removing the `?` from `parts.pop()?` in the tar
  path-resolution loop turned `../x` into `x` instead of an error, and the whole proptest suite
  passed because a filesystem walk cannot see it — the archive landed entirely inside the root.
  Signal: a policy test that only checks where files ended up. Mitigation: compute the expected
  status from the generated member and assert on it, which makes the same break shrink to a
  one-member archive. `.erpaval/solutions/test-failures/proptest-and-dst-tiers-need-verdict-assertions.md:11-33`
- **The guard never watched failing:** five distinct shapes found in one session. A bare
  `compile_fail` block passes for any build error including a typo in the doctest, so each is
  pinned to a measured rustc error code. A fake that models the failure *event* cannot catch
  lateness, because a client refreshing too late presents a token with no life left rather than
  an expired one — the fake had to measure the remaining margin. Uniform proptest draws
  almost never land in the narrow band where a rounding bug lives. And a guard can require
  the very divergence it should catch. Signal: a guard you have never seen red. Mitigation:
  break the invariant, watch that specific test fail, restore.
  `.erpaval/solutions/test-failures/guards-that-passed-against-broken-code.md:13-37`
- **The ordering defect no guard can see:** every guard fired and every refusal test passed,
  yet the S3 upload ran *before* the guards refused, so a rejected request still cost a PUT. A
  test asserting "the bad request is refused", or even "zero control-plane calls", stays green
  across the broken ordering, because an upload is not a wire call and had no recorder.
  Signal: a side effect with no channel to assert absence through. Mitigation: give the side
  effect its own recorder — a separate `uploads` vec, deliberately not mixed into `calls` — and
  assert it is empty on a request the library refuses. Falsification is a pure reorder, and
  both call sites need the break run separately.
  `.erpaval/solutions/best-practices/ordering-defects-need-their-own-recorder-channel.md:9-30`
- **The fake more forgiving than the real server:** 310 fake-backed tests were green over a
  client whose auth-header injection replaced the request headers, stripping content-type,
  where the real daemon's typed extractor answered 400. Separately, the `run` envelope
  published a null `agentToken` through 139 CLI tests because nothing round-tripped the
  envelope into a second command. Signal: a live tier failing a request every local tier
  accepts. Mitigation: run the live tier before trusting a transport that has only ever spoken
  to fakes, because a fake that accepts what the real server rejects converts integration bugs
  into production bugs.
  `.erpaval/solutions/test-failures/guards-that-passed-against-broken-code.md:39-47`
- **Two clocks in one test:** under a virtual-time simulator everything inside the simulation
  runs on the paused clock and a spawned child does not — measured here, 2 real seconds of child
  sleep elapsed while the simulation advanced 30 virtual ones. A server-side deadline measured
  against real work therefore expires in milliseconds of wall time: `output_linger` is a virtual
  5 seconds against a real pipe drain, so the waiter abandons a still-writing child and the exec
  reports missing output with `writers_may_be_alive` set. Signal: simulator-induced truncation
  that looks like a daemon defect. Mitigation: never pace a child with `sleep` in a simulated
  test — make it block on `read` released by an explicit stdin write, so the harness is the
  clock. Loosening the assertion to match would encode the artifact as expected behavior.
  `.erpaval/solutions/test-failures/simulated-time-and-real-children-are-two-clocks.md:13-39`
- **The absent value that is not neutral:** an agreement guard has to decide separately what a
  missing value means, and the deciding question is what the *consumer* does with absence. A
  consumer that errors out makes absence safe to pass; a consumer with a silent fallback makes
  absence a disagreement in disguise, because the fallback is a third value nobody wrote down.
  Signal: a constant whose docstring justifies it in terms of another component's default —
  "four times the daemon's fifteen-second SSE keepalive". Mitigation: grep docstrings for
  "twice the", "four times", "matching the", "same as the daemon's"; and treat an
  *unparseable* value wherever the absent one lands, since `env_parse` warns and keeps the
  default. `.erpaval/solutions/architecture-patterns/an-absent-value-is-not-a-neutral-one.md:12-37`
- **A constructor that only ever ran under a fake:** `aws-config` built with
  `default-features = false` looks right when you have hand-rolled an HTTP client, and it is
  wrong — the credential chain does its own HTTP for IMDS, SSO, and STS, and `load()` panics
  with `"a http_client is required"` before asking any credential question. Nothing caught it
  because all 300 tests constructed through the injectable transport, and the one constructor
  that talks to the world had no test at all. Signal: a `new()` that touches the real
  environment with no test calling it. Mitigation: `default-https-client` stays on
  (`microvms-core/Cargo.toml:59-62`), and one test constructs the real transport and accepts
  either `Result` flavor — a panic is the bug
  (`microvms-core/src/control/transport.rs:899-908`).
  `.erpaval/solutions/api-patterns/aws-config-needs-its-own-http-client.md:11-22`
- **The credential in a derived `Debug`:** three of six token-carrying types in this workspace
  leaked secrets through `#[derive(Debug)]` while their three siblings hand-wrote redaction —
  the invariant was known and still missed half its sites, because a derive is the default and
  nothing flags it. Signal: any struct holding a token, an `Authorization` header, or a hook
  payload. Mitigation: hand-write `Debug` printing names and lengths only, add a per-type guard
  that formats with `{:?}` and asserts the secret absent, and redact *all* header values rather
  than an allowlist. `.erpaval/solutions/best-practices/credential-structs-never-derive-debug.md:10-20`
- **The golden figure taken from the plan:** the plan pinned a 2 GB break-even at about 1357s
  and the oracle prints 1371.2916483478837. A golden test built from the plan's number would
  have been the one check that agreed with a plausible wrong answer, since the port and its test
  derived from the same mistaken source. Signal: a pinned figure or output-contract string whose
  provenance is a document rather than an execution. Mitigation: capture every golden by running
  the oracle, paste it verbatim, and cite it.
  `.erpaval/solutions/best-practices/run-the-oracle-never-rederive-goldens.md:10-19`
- **The tokio traps in the exec path:** `child.id()` returns `None` once the child has been
  polled to completion, so a pgid read lazily in the kill path yields `None`, the group signal
  never goes out, and a kill test asserting only on the HTTP status still passes while the
  process tree survives. Privilege demotion uses `Command::uid()/gid()` and never `pre_exec`,
  because running interpreted code between fork and exec is unsafe with threads. And a
  `std::sync::Mutex` guard is never held across an await. Signal: a kill that reports success
  over a live process tree. Mitigation: capture the pgid immediately after spawn and assert on
  the observable kill outcome.
  `.erpaval/solutions/best-practices/pipes-not-tempfiles-for-subprocess-output.md:32-47`
- **The simulator that needs two specific bounds:** making the serve path generic over
  `axum::serve::Listener` costs nothing in production and buys deterministic network simulation
  — but omitting `L::Addr: Debug` fails with an E0277 saying `Serve<L, Router, Router> is not a
  future`, which points nowhere near the missing bound, and `turmoil::Builder::enable_tokio_io()`
  is required whenever the served code registers a signal handler or graceful shutdown panics
  inside the host. Signal: either of those two errors while adding a simulated test.
  `.erpaval/solutions/api-patterns/axum-listener-trait-enables-turmoil.md:36-42`
- **The security control that breaks the platform:** both the platform's lifecycle hooks and the
  harness's control requests arrive from `127.0.0.1`, because the endpoint proxy terminates
  outside the VM and forwards over loopback. A source-address rule rejecting loopback callers on
  the bootstrap route therefore rejects the platform's own legitimate bootstrap and breaks every
  launch; an attempt at one broke 39 tests, and those failures were reporting a real defect.
  Mitigation: the one-shot bootstrap is the only available defense on that route, and its
  sufficiency is checked in the `model/` crate. `docs/PLATFORM.md:463-485`
- **The probe that looks like an attack:** the daemon receives raw TLS handshake bytes on its
  plaintext port before bootstrap — `code 400, message Bad request version ("\x13\x01\x13\x02...")`
  is a ClientHello reaching a plaintext HTTP server. Something in the platform's path probes the
  port with TLS first. Signal: that line in the logs. Mitigation: none needed — the correct
  response is a 400 and a debug-level log, and it must not take the listener down.
  `docs/PLATFORM.md:487-499`
- **The hostile header that killed the handler:** `hmac.compare_digest` raises `TypeError` on a
  `str` containing non-ASCII, so `Bearer tökén` took down a handler thread and the client got
  `RemoteDisconnected` rather than a status it could act on. Any caller controls that header.
  Mitigation: the comparison runs on raw bytes and never decodes, and authorization is decided
  before a single body byte is read — buffering first let an unauthorized request force a 256 MB
  allocation on a VM whose baseline can be 512 MiB. `agentd/src/auth.rs:2-14`
- **Silent until the disk is already full:** `anthropics/claude-code#59856`, cited by number in
  source, filled two 10 GB disks to 100% and the first symptom was `useradd` failing rather than
  the workload — by which point every writer in the sandbox was broken, including the ones that
  cannot report anything. Mitigation: refuse a write that would cross a reserve before it
  starts, answer 507 with the actual free bytes, and put the number on `/v1/health` so the curve
  is visible while there is still time. `agentd/src/config.rs:98-105`,
  `protocol/src/health.rs:21-34`

## See also

- [impact analysis](impact-analysis.md) — 18 shared source citations
- [business logic](business-logic.md) — 17 shared source citations
- [contract map](contract-map.md) — 16 shared source citations
- [processes](../behavior/processes.md) — 15 shared source citations
- [tech debt](tech-debt.md) — 11 shared source citations
