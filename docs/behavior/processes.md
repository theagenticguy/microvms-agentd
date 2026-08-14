# microvms-agentd · Processes

Three initiator families drive every process in this document: the `microvms-core` library's public lifecycle methods, the `agentd` HTTP routes the platform and clients call, and the `microvms-cli` command handlers. The eight processes below carry the most important behavior. The remaining flows are listed in `## Minor flows`.

## Image build and the stalled-build probe

Entry point: `microvms-core/src/sandbox.rs:482`

1. `Sandbox::build_image` records the requested size and hands the request to the control plane. Every local guard runs before the wire call, because the create happens *after* the caller's artifact upload — `microvms-core/src/sandbox.rs:482`.
2. `ControlPlane::create_image` closes the local guards: the image-name check, then `artifact::require_workdir` when `inherit_workdir` is set, then `artifact::require_matching_from` when a Dockerfile was supplied — `microvms-core/src/control/image.rs:156`.
3. The wire body is assembled with the constants injected rather than accepted from the caller. These are the one architecture value, the one OS capability derived from a boolean, and a `clientToken` minted from a label — `microvms-core/src/control/image.rs:165`.
4. `CreateMicrovmImage` goes out through `send_with_retry` and the reply is deserialized into an `Image` — `microvms-core/src/control/image.rs:197`.
5. `ControlPlane::wait_for_image` polls `GetMicrovmImage`, returning on `Image::is_ready` and raising on `Image::is_failed` via `build_failure`, which names the required log-group prefix — `microvms-core/src/control/image.rs:243`.
6. Once elapsed time passes `WaitOpts::stall_grace` (240s by default) the wait runs `probe_stalled_build` exactly once — `microvms-core/src/control/image.rs:257`.
7. `probe_stalled_build` reads the build list through `build_states`. An unreadable list or an empty one returns `Ok`, because neither is evidence of a wedged build; only an all-`PENDING` list raises `ErrorKind::BuildWedged`, which names the `clientToken` replay signature — `microvms-core/src/control/image.rs:291`.
8. Back in `build_image`, `image_exists` is set before any launch, so a teardown can name the image whether or not anything ran from it — `microvms-core/src/sandbox.rs:491`.

### Related

- `microvms-core/src/control/image.rs:329`
- `microvms-core/src/control/image.rs:380`
- `microvms-core/src/control/artifact.rs:44`
- `microvms-core/src/control/transport.rs:237`
- `microvms-core/src/control/token.rs:101`
- `microvms-core/src/sandbox.rs:499`

## Launch and the RUNNING wait

Entry point: `microvms-core/src/sandbox.rs:521`

1. `Sandbox::run` rejects a second launch on the same sandbox. This is STATE-3's local half, and it is checked before any call because a retry loop around a timed-out launch is the plausible mistake — `microvms-core/src/sandbox.rs:525`.
2. The image identifier is resolved from the request or from a previously built image, and a precondition error is raised when neither exists — `microvms-core/src/sandbox.rs:536`.
3. An agent token is minted from `/dev/urandom` unless the caller supplied one, then wrapped in a `RunHookPayload` whose constructor re-checks it even though this code built the JSON — `microvms-core/src/sandbox.rs:548`.
4. `ControlPlane::run_microvm` validates the duration range, splits connector intents into separate ingress and egress members, enforces the connector ceiling, and mints the `clientToken` from a scope label — `microvms-core/src/control/microvm.rs:288`.
5. On acceptance the lifecycle moves to `Pending`, `image_exists` is set, and `suspended_window` is recorded from *this* request. This is the only place the value is knowable, since `GetMicrovm` does not return it — `microvms-core/src/sandbox.rs:568`.
6. `ControlPlane::wait_for_running` polls `GetMicrovm`, failing fast on any terminal state through `reached_terminal_state`, which puts both the state and `stateReason` in the message — `microvms-core/src/control/microvm.rs:348`.
7. Reaching RUNNING sets `token_installed` and increments `bootstrap_count`. The trigger is the platform reporting that the run hook succeeded, not the launch call itself — `microvms-core/src/sandbox.rs:591`.
8. A `ControlPlaneMinter` is built behind an `Arc` and handed to `Session::builder`, so minting can happen inside every later request rather than once here — `microvms-core/src/sandbox.rs:597`.

### Related

- `microvms-core/src/control/microvm.rs:363`
- `microvms-core/src/control/microvm.rs:399`
- `microvms-core/src/sandbox.rs:339`
- `microvms-core/src/sandbox.rs:963`
- `microvms-core/src/session/mod.rs:467`
- `microvms-core/src/control/microvm.rs:93`

## Suspend and resume with the launch-time window

Entry point: `microvms-core/src/sandbox.rs:624`

1. `Sandbox::suspend` resolves the VM id through `require_microvm`, then rejects any lifecycle but RUNNING before making any control-plane call. The absence of a wire call on rejection is the observable the test asserts on — `microvms-core/src/sandbox.rs:628`.
2. The lifecycle moves to `Suspending` before the wire call, then `SuspendMicrovm` goes out — `microvms-core/src/sandbox.rs:639`.
3. `suspended_at` is stamped from the control plane's own clock *after* the call and *before* the wait, because the `idlePolicy` window starts when the platform begins suspending — `microvms-core/src/sandbox.rs:644`.
4. `wait_for_state` is called with `SUSPEND_WANTED`, so TERMINATED counts as a wanted outcome rather than a failure raised out of the middle of a teardown. Any other state is an `ErrorKind::Platform` — `microvms-core/src/sandbox.rs:646`.
5. `Sandbox::resume` rejects a terminated VM first (STATE-11), then any lifecycle but SUSPENDED (STATE-7) — `microvms-core/src/sandbox.rs:708`.
6. `require_open_suspended_window` compares elapsed time against the recorded window and rejects locally once the window has passed. When no window was recorded, which happens on the attach path, it returns `Ok` because no default value would be reliable — `microvms-core/src/sandbox.rs:768`.
7. `ResumeMicrovm` goes out and `wait_for_state` waits for RUNNING with `DEAD_STATES` as `fail_on`. The terminal set is not used here because SUSPENDED, the state the call was made from, is in it — `microvms-core/src/sandbox.rs:725`.
8. `Session::rebind` takes the endpoint the service just reported and invalidates the cached proxy token (STATE-8). `suspended_at` is cleared so the next cycle measures its own window — `microvms-core/src/sandbox.rs:748`.

### Related

- `microvms-core/src/control/microvm.rs:432`
- `microvms-core/src/control/microvm.rs:439`
- `microvms-core/src/control/microvm.rs:479`
- `microvms-core/src/session/mod.rs:267`
- `microvms-core/src/session/proxy.rs:331`
- `microvms-core/src/sandbox.rs:909`

## Teardown

Entry point: `microvms-core/src/sandbox.rs:801`

1. `Sandbox::terminate` returns a `TeardownReport` rather than a `Result`. It runs where a caller's `finally` would, and an error raised there would replace the real failure — `microvms-core/src/sandbox.rs:801`.
2. The session is dropped first, since its only remaining asset is a cached proxy token for a VM that is going away — `microvms-core/src/sandbox.rs:807`.
3. The lifecycle moves to `Terminating` and `was_terminated` is set *before* the call, so a terminate whose call fails still blocks a later resume — `microvms-core/src/sandbox.rs:814`.
4. `TerminateMicrovm` goes out; a failure is pushed onto both `failures` and `undeleted` rather than raised — `microvms-core/src/sandbox.rs:817`.
5. When `wait_for_terminated` was requested, `wait_for_state(["TERMINATED"])` runs; a timeout there is recorded as a failure but **not** a leak, because the platform accepted the terminate — `microvms-core/src/sandbox.rs:825`.
6. The image is deleted second, through `delete_image`'s retry loop. The retries exist because an image in `CREATING` cannot be deleted and a VM still terminating holds a reference — `microvms-core/src/sandbox.rs:854`.
7. `try_delete_image` lists versions, deletes every version but the first (the last one goes with the image), then deletes the image — `microvms-core/src/control/image.rs:424`.
8. The build log group is handled **last**, and named rather than deleted. Because CloudWatch is absent from the dependency set, the group lands in `undeleted` with a failure line explaining why — `microvms-core/src/sandbox.rs:877`.

### Related

- `microvms-core/src/sandbox.rs:290`
- `microvms-core/src/sandbox.rs:319`
- `microvms-core/src/control/image.rs:411`
- `microvms-core/src/control/image.rs:81`
- `microvms-core/src/sandbox.rs:926`
- `microvms-cli/src/commands/lifecycle.rs:370`

## Daemon bootstrap through the run hook

Entry point: `agentd/src/routes.rs:172`

1. `routes::app` builds the router by walking `surface_docs()`, so a route cannot be served unless it is documented. A documented route with no handler panics at startup — `agentd/src/routes.rs:51`.
2. Bearer endpoints go into a `control` router behind `auth::require_token`; the hook routes go into an unauthenticated `open` router, because the platform has no credential to present — `agentd/src/routes.rs:53`.
3. `run_hook` rejects a body that is not JSON with 400 rather than 404, because a client would read 404 as a missing artifact — `agentd/src/routes.rs:176`.
4. The `runHookPayload` member is extracted, then parsed a *second* time. The platform wraps the caller's string, so `agent_token` is one JSON parse deeper than the request body — `agentd/src/routes.rs:184`.
5. An empty `agent_token` is 400. Neither the token nor the payload carrying it is written to the log — `agentd/src/routes.rs:195`.
6. `AppState::bootstrap` takes the token lock through `recover`, installs into an empty slot, or compares in constant time against what is there — `agentd/src/state.rs:163`.
7. The three outcomes map to statuses. `Installed` and `AlreadyIdentical` are both 200, because the platform may retry its own hook and answering 409 would fail a launch that is fine. `Conflict` is 409 — `agentd/src/routes.rs:200`.
8. Every later control request runs `require_token`, which distinguishes not-yet-bootstrapped (503) from a wrong credential (401) using `token_matches`'s three-valued answer — `agentd/src/auth.rs:69`.

### Related

- `agentd/src/routes.rs:110`
- `agentd/src/state.rs:69`
- `agentd/src/state.rs:189`
- `agentd/src/auth.rs:28`
- `agentd/src/auth.rs:92`
- `agentd/src/routes.rs:257`

## Exec start and its detached waiter

Entry point: `agentd/src/exec.rs:331`

1. `start` rejects a malformed body and an empty `exec_id` with 400, then validates `timeout_sec`. Non-finite and non-positive values are rejected before anything is spawned, because rejecting them in the waiter left a running child with nobody to reap it — `agentd/src/exec.rs:353`.
2. `build_command` assembles the child. It uses `sh -c` with one joined script when `shell` is set, calls `env_clear()` so the agent token cannot reach the child, inherits the cwd when none was given, sets `process_group(0)`, and attaches `/dev/null` to stdin unless stdin was requested — `agentd/src/exec.rs:960`.
3. Idempotency is decided under the registry lock before the spawn, so two concurrent retries cannot both find the slot empty; a known id returns success with the existing entry untouched — `agentd/src/exec.rs:366`.
4. `spawn` starts the child and captures the pgid immediately, while `Child::id()` still answers. A lazy read would find nothing for exactly the fast-then-forking commands that most need killing — `agentd/src/exec.rs:1030`.
5. The pipes are taken out of the `Child`, a broadcast channel is created, and `Shared` plus the registry entry are built. Taking the pipes out is also why the daemon must drop its own stdin copy on EOF — `agentd/src/exec.rs:1032`.
6. A detached task runs `super_wait`, which drains both pipes concurrently with `child.wait()` and escalates the process group on the timeout branch. The concurrent drain matters because a child filling a 64 KiB pipe buffer blocks forever if nobody reads — `agentd/src/exec.rs:1145`.
7. After the child exits, `super_wait` lingers for `output_linger` so grandchildren still holding the write end can be read, setting `writers_may_be_alive` when the deadline cuts it short — `agentd/src/exec.rs:1168`.
8. The waiter writes the `terminal` marker *before* `result`, so a stream that sees `Finished` immediately can always find an exit event, then drops stdin and sends `Frame::Finished` — `agentd/src/exec.rs:1093`.

### Related

- `agentd/src/exec.rs:255`
- `agentd/src/exec.rs:1261`
- `agentd/src/exec.rs:1306`
- `agentd/src/exec.rs:1345`
- `agentd/src/exec.rs:932`
- `agentd/src/state.rs:201`

## Proxy-token mint inside the request path

Entry point: `microvms-core/src/session/mod.rs:105`

1. `Transport::request` calls `self.headers(request_token(&request))`, so the mint sits inside the path every request takes. A token minted once at construction expires mid-run, and the resulting rejection looks the same as a dead daemon — `microvms-core/src/session/mod.rs:114`.
2. The private `TOKEN_INTENT` marker header is stripped before the request leaves, and the caller's own headers are appended rather than replacing the vec. An earlier version replaced the vec, which stripped the content type and produced a daemon 400 while every fake-backed test stayed green — `microvms-core/src/session/mod.rs:115`.
3. `ProxyAuth::headers` returns the cached headers when `fresh_headers` finds a token inside the refresh window — `microvms-core/src/session/proxy.rs:350`.
4. Otherwise `mint_lock` is taken and the freshness check is re-run under it, so two concurrent requests on an expired token produce one mint rather than two — `microvms-core/src/session/proxy.rs:356`.
5. `TokenMinter::mint` runs. Whatever the minter's own error kind was, it is reclassified as retryable `WireKind::AuthTokenMint`, so a control-plane throttle at minute thirty does not kill a healthy run — `microvms-core/src/session/proxy.rs:361`.
6. `auth_value()` is checked *before* the token is cached. An `authToken` map missing the auth key is therefore a mint failure, rather than a cached value that fails every request until the window rolls over — `microvms-core/src/session/proxy.rs:377`.
7. The token is cached with the clock reading and `mint_count` is incremented, which is the only externally visible evidence that minting happens in the request path. `headers_from` then adds the port header unless the token already carried one — `microvms-core/src/session/proxy.rs:380`.
8. The bearer is appended last, from the session's own token unless the request's three-valued intent overrode it or asked for no `Authorization` header at all — `microvms-core/src/session/mod.rs:94`.

### Related

- `microvms-core/src/session/proxy.rs:389`
- `microvms-core/src/session/proxy.rs:403`
- `microvms-core/src/session/proxy.rs:133`
- `microvms-core/src/control/microvm.rs:458`
- `microvms-core/src/session/proxy.rs:282`
- `microvms-core/src/session/mod.rs:166`

## CLI run and the interrupt teardown guard

Entry point: `microvms-cli/src/commands/lifecycle.rs:119`

1. `run` resolves the region and image name and prints them before anything is attempted, because the next step is a credential resolution that can hang and an operator needs to know what it stalled on — `microvms-cli/src/commands/lifecycle.rs:140`.
2. Every precondition is checked before anything is created. The required role ARNs are checked, along with the daemon binary's existence when this invocation is the one building, so a missing role is not discovered after a 45-minute build — `microvms-cli/src/commands/lifecycle.rs:146`.
3. The `Sandbox` is constructed *outside* the `select!` and only borrowed inside, which is what lets it survive a cancelled launch future — `microvms-cli/src/commands/lifecycle.rs:172`.
4. `launch_and_exec` is raced against the injected interrupt future; the interrupt arm produces `ErrorKind::Interrupted` with the message explaining that an image left in `CREATING` cannot be deleted afterwards — `microvms-cli/src/commands/lifecycle.rs:190`.
5. The identifiers are read off the sandbox **after** the select, not only inside the cancelled body. Core assigns `microvm` when `RunMicrovm` is accepted, before the RUNNING wait, so an interrupt landing during that wait still leaves a nameable VM — `microvms-cli/src/commands/lifecycle.rs:215`.
6. `tear_down` runs however the block ended. It marks the ledger outstanding *before* attempting the delete, calls `Sandbox::terminate` with image and log-group deletion requested, and warns per leaked identifier on a stream `--quiet` cannot suppress — `microvms-cli/src/commands/lifecycle.rs:227`.
7. `attach_cost` attaches the cost report whichever way the run ended, since an interrupted launch still billed for the seconds it ran — `microvms-cli/src/commands/lifecycle.rs:234`.
8. A failure envelope carries the partial result (`leaked`, `microvmId`, `imageIdentifier`, `undeleted`, `terminateAccepted`), because for an interrupt those identifiers are the operator's to-do list — `microvms-cli/src/commands/lifecycle.rs:236`.

### Related

- `microvms-cli/src/commands/lifecycle.rs:273`
- `microvms-cli/src/commands/lifecycle.rs:370`
- `microvms-cli/src/commands/lifecycle.rs:107`
- `microvms-cli/src/commands/lifecycle.rs:617`
- `microvms-cli/src/ledger.rs:122`
- `microvms-cli/src/main.rs:377`

## Minor flows

- Daemon startup — entry at `agentd/src/main.rs:14`. Builds a current-thread runtime with four blocking threads, runs identity repair before the listener binds, spawns the 30-second exec collector, then serves with graceful shutdown.
- Startup identity repair — entry at `agentd/src/identity.rs:230`. Mints one fresh id and reuses it for machine-id, hostname, and boot_id; deletes the random seed rather than rewriting it; every failure is warned and swallowed.
- Graceful shutdown — entry at `agentd/src/serve.rs:35`. Selects SIGTERM against ctrl-c and drains in-flight requests so a harness polling an exec gets its status rather than a transport error.
- Exec poll — entry at `agentd/src/exec.rs:407`. Reads the registry without mutating it; an acked entry reports no output so the payload cannot contradict the phase.
- Exec SSE stream — entry at `agentd/src/exec.rs:455`. Subscribes and snapshots the replay ring atomically, reads the terminal marker after the snapshot, and sends `x-accel-buffering: no` so a proxy cannot batch a live stream.
- Exec stdin write — entry at `agentd/src/exec.rs:682`. Returns four distinct statuses (409 not-requested, 410 closed, 413 too large, 408 write timeout), and an EOF drops the daemon's own pipe handle.
- Exec ack — entry at `agentd/src/exec.rs:831`. Takes the buffered output once and starts the TTL clock; a still-running or already-acked exec is 409 rather than a 200 with an empty body.
- Exec kill — entry at `agentd/src/exec.rs:886`. SIGTERM to the whole process group, an early exit when the child finishes on its own, then SIGKILL.
- Expired-exec collection — entry at `agentd/src/exec.rs:932`. Retains unacked entries forever and drops acked ones past the TTL, run from the daemon's own interval rather than per request.
- File read — entry at `agentd/src/fs.rs:541`. Streams rather than buffering, rejects a directory up front, and returns 404 only when the path is genuinely absent.
- File write — entry at `agentd/src/fs.rs:588`. Parses the mode and preflights the disk before a single byte lands, and sets the mode at open so the file never exists wider than asked for.
- Tar download — entry at `agentd/src/fs.rs:775`. Estimates the tree against the member and byte caps, then packs into an unlinked spool file inside `spawn_blocking`.
- Tar upload and extraction — entry at `agentd/src/fs.rs:837`. This is the one confined write path. It spools the body under a disk pacer, then extracts with deferred directory modes, rejected device nodes, and lexically resolved link targets.
- Health — entry at `agentd/src/routes.rs:278`. Reports version, bootstrap state, a disk reading that is null when unmeasurable, and the identity-repair verdict.
- Schema publication — entry at `agentd/src/routes.rs:322`. Serves the same `surface_docs()` list the router was assembled from, unauthenticated so a client can negotiate before it holds a token.
- Control-plane send with retry — entry at `microvms-core/src/control/transport.rs:237`. Exponential backoff with jitter over five attempts, gated on `Error::retryable`, with `classify_failure` mapping the seven modeled exception statuses.
- Session readiness wait — entry at `microvms-core/src/session/mod.rs:295`. Polls unauthenticated health, tolerating retryable errors and returning at once on a fatal one, and names the last retryable error in the timeout.
- Client-side exec stream with reconnect — entry at `microvms-core/src/session/exec.rs:378`. One state machine shared by the `Stream` and callback drivers; advances the cursor only past bytes handed over, and past a gap so a reconnect is not told about it forever.
- Client-side wait-then-ack — entry at `microvms-core/src/session/exec.rs:605`. Returns the ack's result, not a post-ack poll, because the poll reports `acked` with no output.
- CLI dispatch — entry at `microvms-cli/src/main.rs:63`. Scans raw tokens for `--json` before the parse so an argument error still produces an envelope, then builds the runtime and dispatches through an exhaustive match.
- CLI build — entry at `microvms-cli/src/commands/lifecycle.rs:469`. Builds an image without launching and publishes the service-created build log group, which no Terraform stack owns.
- CLI attached suspend — entry at `microvms-cli/src/commands/lifecycle.rs:730`. Reads the state with `GetMicrovm` first and rejects locally from anything but RUNNING, which is STATE-5's local half on a path that did not launch.
- CLI attached resume — entry at `microvms-cli/src/commands/lifecycle.rs:795`. Skips the suspended-window check, since a process that did not send the launch cannot know the window; relies on `fail_on: DEAD_STATES` instead.
- CLI attached terminate — entry at `microvms-cli/src/commands/lifecycle.rs:840`. VM, then image with retries, then the log group named rather than deleted, with `leaked` and `undeletedLogGroups` kept as separate claims.
- CLI exec — entry at `microvms-cli/src/commands/attached.rs:107`. One subcommand over four shapes: start and wait, `--stream`, `--stdin`, and `--poll` of an existing exec.
- CLI exec stream — entry at `microvms-cli/src/commands/attached.rs:236`. Drives core's callback loop, emits NDJSON per event plus raw bytes on the human path, and reports `nextOffset` from core's own cursor.
- CLI health — entry at `microvms-cli/src/commands/attached.rs:471`. Warns on degraded identity and disk pressure on stderr while keeping exit 0, because the daemon's contract is to serve anyway.
- CLI ack — entry at `microvms-cli/src/commands/attached.rs:574`. Issues the ack on its own for a detached caller and clears the exec rendering's non-zero report, since this command's `$?` answers whether the output was released.
- CLI stdin — entry at `microvms-cli/src/commands/attached.rs:612`. Rejects a no-op write locally and chunks under the daemon's per-write cap with EOF riding the final chunk.
- CLI cp — entry at `microvms-cli/src/commands/attached.rs:779`. Resolves direction from the `vm:` prefix before opening anything, and does not inspect archives, so the daemon's extractor stays the only one under test.
- CLI doctor — entry at `microvms-cli/src/commands/doctor.rs:32`. Runs region, credential, infra, Terraform, and binary checks in cost order and returns a success envelope with `ok: false` plus a precondition exit code.
- CLI ls — entry at `microvms-cli/src/commands/local.rs:23`. Reads the local ledger and lists what this CLI created and could not confirm it deleted.
- CLI logs — entry at `microvms-cli/src/commands/local.rs:146`. Derives and names the build log group, then reports failure with `lines: null` rather than an empty array, which would read as "the group had no events".
- CLI manifest — entry at `microvms-cli/src/commands/local.rs:191`. Derives the whole command surface from the clap tree and the exit table so it cannot drift from what the binary accepts.
- CLI constants — entry at `microvms-cli/src/commands/local.rs:214`. Emits every service constraint this client believes, for comparison against the pinned service model.
- CLI cost — entry at `microvms-cli/src/commands/cost.rs:27`. Rejects negative durations up front, then renders a report from pinned rates with unpriced line items kept distinct from zero.

## See also

- [microvms-agentd · Debugging guide](../insights/debugging-guide.md)
- [microvms-agentd · Business logic](../insights/business-logic.md)
- [microvms-agentd · Data flow](../architecture/data-flow.md)
- [microvms-agentd · Contract map](../insights/contract-map.md)
- [microvms-agentd · Tech debt](../insights/tech-debt.md)
