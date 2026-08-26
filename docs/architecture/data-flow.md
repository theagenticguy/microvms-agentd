# microvms-agentd · Data flow

Two surfaces trigger work in this system and nothing else does: a CLI invocation, dispatched
through a 17-arm exhaustive match (`microvms-cli/src/main.rs:382-403`), and a daemon HTTP
request, dispatched through a handler table walked from the same list `/v1/schema` publishes
(`agentd/src/routes.rs:110`). The bindings re-enter the same `microvms-core` surfaces the CLI
uses, so they add no distinct flow, and the daemon's only recurring job is a 30-second
expired-exec reaper rather than a request lifecycle (`agentd/src/main.rs:61`,
`agentd/src/exec.rs:951`).

The three flows below are ranked by how much of the client-to-daemon boundary each exercises,
tie-broken by whether it is named after one of the system's core verbs. Flow 1 is the only arm
that launches a VM and the only one that touches all four actors. Flow 2 is the streaming read
path, whose correctness rests on a byte-offset cursor that survives a reconnect through the
endpoint proxy. Flow 3 is the file-transfer path, and it ends in the daemon's one confined
write.

Participants are the workspace crates named in `architecture/module-map.md` plus two external
actors. `microvm CLI` is `microvms-cli`; `agentd` is the in-VM daemon; `AWS MicroVMs` is the
control plane together with its endpoint proxy.

## Flow 1: microvm run — build, launch, bootstrap, exec, tear down

1. `commands::lifecycle::run` resolves region, size class, and image name, then requires every
   infra role before anything is created, so a missing role surfaces immediately rather than
   after a build (`microvms-cli/src/commands/lifecycle.rs:121`, guard at
   `microvms-cli/src/commands/lifecycle.rs:141-166`).
2. It opens a `Sandbox` through the library seam and races `launch_and_exec` against ctrl-c in a
   `tokio::select!`, with the sandbox owned outside the select so a cancelled launch still holds
   the identifiers teardown needs (`microvms-cli/src/commands/lifecycle.rs:170-198`,
   recovery at `microvms-cli/src/commands/lifecycle.rs:213-220`).
3. `launch_and_exec` preflights the build request, uploads the artifact, then `Sandbox::build_image`
   issues `CreateMicrovmImage` and waits for the image to become usable
   (`microvms-cli/src/commands/lifecycle.rs:302-307`, `microvms-core/src/sandbox.rs:551`).
4. `Sandbox::run` refuses a second bootstrap on the same sandbox, mints the agent token, and
   wraps it with the launch env in a typed `RunHookPayload` that checks its 4096-byte budget
   before any call (`microvms-core/src/sandbox.rs:648`, refusal at
   `microvms-core/src/sandbox.rs:652`, payload at `microvms-core/src/sandbox.rs:682`).
5. `ControlPlane::run_microvm` validates the identifier, the duration range, and the role ARN,
   splits ingress and egress connectors by intent, and puts the payload on the wire
   (`microvms-core/src/control/microvm.rs:356`).
6. The platform calls the daemon's run hook over loopback; `run_hook` unwraps the envelope,
   parses the inner payload, and installs the token once — an identical replay is 200 and a
   different token is 409 (`agentd/src/routes.rs:178`, verdicts at
   `agentd/src/routes.rs:213-234`).
7. `ControlPlane::wait_for_running` polls to RUNNING and fails fast on any terminal state; the
   client then polls unauthenticated `/v1/health` until `bootstrapped`
   (`microvms-core/src/control/microvm.rs:435`, `microvms-core/src/session/mod.rs:342`). The
   sandbox marks the token installed only after RUNNING is observed
   (`microvms-core/src/sandbox.rs:722-724`).
8. The optional workload runs through `Session::run_sync` — start, wait, ack — and `tear_down`
   plus `attach_cost` then run however the select ended
   (`microvms-core/src/session/mod.rs:408`, `microvms-cli/src/commands/lifecycle.rs:393`,
   `microvms-cli/src/commands/lifecycle.rs:443`).

```mermaid
sequenceDiagram
    participant CLI as microvm CLI
    participant Core as microvms-core
    participant AWS as AWS MicroVMs
    participant Daemon as agentd
    CLI->>Core: open sandbox, build image
    Core->>AWS: CreateMicrovmImage, wait usable
    AWS-->>Core: image usable
    CLI->>Core: sandbox run(RunRequest)
    Core->>AWS: RunMicrovm with runHookPayload
    AWS->>Daemon: POST runtime/v1/run
    Daemon-->>AWS: 200, token installed once
    AWS-->>Core: RUNNING plus endpoint
    Core->>Daemon: GET /v1/health until bootstrapped
    CLI->>Daemon: POST /v1/exec/start, wait, ack
    Daemon-->>CLI: exit code and output
    CLI->>AWS: TerminateMicrovm, DeleteMicrovmImage
```

## Flow 2: microvm exec --stream — SSE output on a byte-offset cursor

1. `commands::attached::exec` attaches a session from the identifier triple, builds the start
   request under a caller-supplied or minted `exec_id`, starts the command, then branches to
   `stream_exec` (`microvms-cli/src/commands/attached.rs:103`, branch at
   `microvms-cli/src/commands/attached.rs:163-165`).
2. `stream_exec` drives `ExecHandle::for_each_event` with a `FnMut(ExecEvent) -> ControlFlow<()>`
   callback, writes one NDJSON line plus the raw bytes per event, and reports `nextOffset` from
   core's cursor rather than its own tally (`microvms-cli/src/commands/attached.rs:240`, cursor
   read at `microvms-cli/src/commands/attached.rs:281`).
3. `for_each_event` delegates to `for_each_event_async`, whose loop steps the `advance` state
   machine, reads the cursor off the machine, and reports `EndReason::Cut` when a body ends with
   no `exit` event (`microvms-core/src/session/exec.rs:347`, loop at
   `microvms-core/src/session/exec.rs:419-428`).
4. `advance` re-attaches at the last good cursor with a fixed backoff on a retryable failure,
   and errors out past `max_reconnects` instead of looping forever
   (`microvms-core/src/session/exec.rs:460`, backoff and re-attach at
   `microvms-core/src/session/exec.rs:487-491`).
5. `ExecHandle::attach` issues `GET /v1/exec/{id}/stream?offset=N` with
   `accept: text/event-stream`, building its headers inside the request path so a mid-stream
   reconnect re-mints an expired token (`microvms-core/src/session/exec.rs:591`, mint at
   `microvms-core/src/session/exec.rs:600`).
6. `ProxyAuth::headers` serves the cached proxy token, or takes the mint lock and re-checks
   freshness under it so two racing tasks do not burn two control-plane calls
   (`microvms-core/src/session/proxy.rs:432`, double check at
   `microvms-core/src/session/proxy.rs:521-530`).
7. The daemon's `stream` handler snapshots the replay ring, reads the terminal marker after the
   snapshot, and sends the SSE body with a keepalive plus `x-accel-buffering: no` so a buffering
   proxy cannot batch a live stream into one delivery at exit (`agentd/src/exec.rs:455`,
   ordering at `agentd/src/exec.rs:474-479`, header at `agentd/src/exec.rs:489-491`).
8. `build_stream` emits any `gap` first, drains the replayed backlog, then the live broadcast
   channel, and closes the body one step after the terminal `exit` event
   (`agentd/src/exec.rs:560`, ending at `agentd/src/exec.rs:604-615`).

```mermaid
sequenceDiagram
    participant CLI as microvm CLI
    participant Handle as ExecHandle
    participant Proxy as ProxyAuth
    participant AWS as AWS MicroVMs
    participant Daemon as agentd
    CLI->>Handle: for_each_event(offset)
    Handle->>Proxy: headers()
    Proxy->>AWS: CreateMicrovmAuthToken
    AWS-->>Proxy: header map, cached
    Handle->>Daemon: GET /v1/exec/id/stream?offset=N
    Daemon-->>Handle: SSE gap event
    Daemon-->>Handle: SSE output events
    Daemon-->>Handle: SSE exit event, body closes
    Handle-->>CLI: StreamEnd with cursor
```

## Flow 3: microvm cp --tar — an archive into the one confined write path

1. `commands::attached::cp` resolves the direction from the `vm:` prefix before opening
   anything, so two local paths or two remote paths are refused by name rather than guessed at
   (`microvms-cli/src/commands/attached.rs:805`, resolver at
   `microvms-cli/src/commands/attached.rs:902`).
2. It attaches through the helper every command in that file starts with, which resolves the
   region first because the region is what the proxy-token mint's ARN is derived for
   (`microvms-cli/src/commands/attached.rs:73`).
3. The upload arm reads the local archive whole and sends it without inspecting it: the daemon's
   extractor is the only one in the system, and a client-side check would be a second set of
   member rules that could disagree with it
   (`microvms-cli/src/commands/attached.rs:812-835`, stated at
   `microvms-cli/src/commands/attached.rs:798-804`).
4. `Session::upload_tar` delegates to `files::upload_tar`, which builds
   `PUT /v1/fs/tar?path=...` with `content-type: application/x-tar` and the archive bytes as the
   body (`microvms-core/src/session/mod.rs:444`, `microvms-core/src/session/files.rs:98`).
5. `Transport::request` prepends the proxy headers and the session's bearer token to the
   caller's own headers rather than replacing them, which is what keeps the content type on the
   request (`microvms-core/src/session/mod.rs:106`, header assembly at
   `microvms-core/src/session/mod.rs:88`).
6. `auth::require_token` guards the control router before the body is polled, answering 503
   when no token is installed and 401 on a mismatch, then draining a bounded prefix of the
   rejected body so the client sees the status rather than a TCP reset
   (`agentd/src/auth.rs:62`, verdicts at `agentd/src/auth.rs:69-80`, applied at
   `agentd/src/routes.rs:66-69`).
7. `fs::write_tar` refuses a relative extraction root, preflights free disk against that root
   before the body is spooled, then spools the body under the disk pacer
   (`agentd/src/fs.rs:1433`, preflight at `agentd/src/fs.rs:1459-1461`, spool at
   `agentd/src/fs.rs:872`).
8. `extract_into` runs under `spawn_blocking` and holds one confined directory handle for the
   whole extraction: ownership and xattrs are dropped, device and fifo members are refused,
   out-of-tree link targets are refused, and directory modes are replayed after all content
   lands. Success is 204 (`agentd/src/fs.rs:621`, refusals at `agentd/src/fs.rs:702-707` and
   `agentd/src/fs.rs:742`, deferred modes at `agentd/src/fs.rs:810`, dispatch and status at
   `agentd/src/fs.rs:1479-1487`).

```mermaid
sequenceDiagram
    participant CLI as microvm CLI
    participant Core as microvms-core
    participant AWS as AWS MicroVMs
    participant Daemon as agentd
    participant Disk as guest filesystem
    CLI->>Core: upload_tar(remote, archive)
    Core->>AWS: PUT /v1/fs/tar with bearer
    AWS->>Daemon: forwarded request
    Daemon->>Daemon: require_token, then disk preflight
    Daemon->>Disk: spool body, then extract_into
    Disk-->>Daemon: members written, modes replayed
    Daemon-->>Core: 204 No Content
    Core-->>CLI: bytes and paths for the envelope
```

## See also

- [processes](../behavior/processes.md) — 13 shared source citations
- [sequences](../diagrams/behavioral/sequences.md) — 11 shared source citations
- [debugging guide](../insights/debugging-guide.md) — 9 shared source citations
- [impact analysis](../insights/impact-analysis.md) — 9 shared source citations
- [business logic](../insights/business-logic.md) — 8 shared source citations
