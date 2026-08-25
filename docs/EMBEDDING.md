# Embedding agentd in your own image and driving it from your own harness

The platform has no exec API: a MicroVM exposes one HTTPS endpoint and forwards
it to whatever the image's `CMD` is listening on. Every harness that wants to
run commands inside a VM therefore ships a daemon in its task image, and before
agentd each harness wrote its own — evaluation harnesses and session servers
each carry a several-hundred-line stdlib Python daemon baked into their images
(`docs/HARNESS-CAPABILITIES.md`, gap 2). agentd supersedes those daemons. This
document is the recipe for appending it to an arbitrary task image, and the
orientation a harness client needs to drive it over the published wire
protocol. The protocol itself is in `docs/PROTOCOL.md` and, machine-readably,
at `GET /v1/schema` on any running daemon; nothing here duplicates either.

## The recipe

`microvm dockerfile` prints the stanza that wraps a base image with agentd —
the same Dockerfile the default `microvm build` bakes, emitted by the same
generator (`microvms-core/src/control/artifact.rs:145`), so appending your own
layers to it *is* the default build plus your layers.

```
microvm dockerfile --workdir /workspace > Dockerfile
# edit: insert your RUN layers between the chmod line and the ENV lines
microvm build ./agentd --dockerfile Dockerfile --name my-task-image
```

The stanza's comments name the two platform constraints a hand-written wrapper
hits, both enforced by microvms-core before any AWS call:

1. **The `FROM` must match the managed base's `docker_ref`.** The build runs
   the Dockerfile on top of the base that `baseImageArn` names, and a mismatch
   builds against a base none of the measured platform behaviour applies to —
   so `require_matching_from` refuses it
   (`microvms-core/src/control/artifact.rs:228-244`).
2. **A `WORKDIR` is required when the base declares none.** The managed al2023
   base, like most public ARM64 bases, leaves `WorkingDir` empty, so "inherit
   the image WORKDIR" inherits `/` and every relative path in your commands
   resolves somewhere you did not mean
   (`microvms-core/src/control/artifact.rs:196-220`).

The worked example is
[`examples/coding-agents-on-bedrock/Dockerfile`](../examples/coding-agents-on-bedrock/Dockerfile):
the stanza's lines, plus `dnf install` and `npm install -g` layers that put two
coding-agent CLIs in the image, plus a `/workspace` WORKDIR. Any task image is
the same shape — take the stanza, add the layers your workload needs, keep the
daemon lines intact.

Two lines in the stanza are load-bearing and must survive your edits.
`ENTRYPOINT []` plus `CMD ["/agentd"]` is the deployment invariant the trust
boundary rests on: it guarantees no task workload runs before the platform's
run hook lands, and it is what makes an omitted `cwd` inherit the image
`WORKDIR` (`microvms-core/src/control/artifact.rs:132-144`,
`docs/PROTOCOL.md`, "Trust boundary"). A base image that starts its own
background process before bootstrap breaks the invariant, and enforcing it
belongs to whoever builds the image — the daemon cannot.

One thing never goes in the image: a secret. The image becomes a shared
snapshot, so every VM launched from it sees the same bytes; per-VM credentials
travel through `runHookPayload` at launch instead
(`microvms-core/src/control/artifact.rs:15-23`).

## The wire contract a harness client implements

The full route table, request shapes, and the defect-driven rules are in
`docs/PROTOCOL.md`; the same contract is served as JSON Schema at
`GET /v1/schema`, unauthenticated, so a client can fetch it before it holds a
token. What follows is the shape of the client, not the contract itself.

**Bootstrap.** The platform delivers your `runHookPayload` string to the
daemon's `/run` hook; agentd parses it as JSON and installs `agent_token`
(`agentd/src/routes.rs:166-216`). The install is one-shot: a replay of the
identical token answers 200 (the platform may retry its own hook), a different
token answers 409 and changes nothing. Until it lands, every control route
answers 503 — not 404, not a dropped connection — so a client can distinguish
"not yet bootstrapped" from "broken" (`agentd/src/auth.rs:62-80`). The payload
is capped at 4096 bytes (`microvms-core/src/constants.rs:61`).

**Auth.** Every `/v1/` route except `/v1/health` and `/v1/schema` takes
`Authorization: Bearer <agent_token>` — the same token the payload delivered.
Comparison is constant-time over bytes (`agentd/src/auth.rs:28`).

**Exec.** The client mints the `exec_id` and sends it in `POST /v1/exec/start`.
That is what makes a retry safe: a start carrying a known id returns success
without spawning a second child, decided under the registry lock
(`agentd/src/exec.rs:364-367`), so a harness whose process died between sending
the start and reading the answer sends the identical start again and gets the
original exec. `GET /v1/exec/{id}` polls, read-only, repeatable. `POST
/v1/exec/{id}/ack` releases the buffered output and starts the collection
clock; a second ack is 409, because the first released it and a 200 with an
empty body would read as "the command produced no output". Output lives until
the ack, so nothing a slow reader has not seen is destroyed. `POST
/v1/exec/{id}/kill` signals the process group, SIGTERM then SIGKILL after a
grace period (`agentd/src/exec.rs:900-931`). `POST /v1/exec/{id}/stdin` writes
to a child that was started with `stdin: true` and carries the explicit EOF
signal; an exec that never asked for stdin answers 409.

**Streaming.** `GET /v1/exec/{id}/stream?offset=N` follows output as SSE from
a byte cursor. A reconnecting client passes the offset it read to and receives
exactly what it has not seen; a reattach past the retained window gets an
explicit `gap` event naming the missing byte range rather than silently
skipping (`agentd/src/exec.rs:436-524`). The stream ends with a typed `exit`
event, which is what distinguishes a finished command from a cut connection —
the reason this is SSE and not a chunked byte stream.

**Files.** `PUT`/`GET /v1/fs/file` move one file, streamed, with a mode
applied at open. `PUT`/`GET /v1/fs/tar` move directory trees; extraction is
confined by lexical resolution with symlink and bomb defenses and member/size
caps (`agentd/src/fs.rs:4-41`), and a write that would push the filesystem
under the disk reserve is refused with 507 naming the real free space
(`agentd/src/fs.rs:66-91`).

**Health.** `GET /v1/health` is unauthenticated and reports version, bootstrap
state, disk pressure, and the identity-repair flags — the conditions that are
reasons to drain a VM rather than schedule more work onto it.

## The proxy-token reality

The daemon's endpoint sits behind the platform's proxy, and the proxy wants two
headers on every request: `X-aws-proxy-auth` carrying a minted JWE, and
`X-aws-proxy-port` naming which allowed port this request targets — omitting
the second is rejected in a way that reads like a bad token
(`microvms-core/src/session/proxy.rs:5-13`). The token comes from
`CreateMicrovmAuthToken`, and the response's `authToken` is a **map of header
name to value**, not a string; read it as a string and every request fails.

The service caps a token at sixty minutes
(`microvms-core/src/session/proxy.rs:63`). That is not a choice, and it is
shorter than a long agent run, so a client that mints once at construction
expires mid-run with a rejection indistinguishable from a dead daemon. The
pattern that works is minting inside the request path with a refresh interval
well under the ceiling — this repo's clients refresh at half of it, thirty
minutes, so a request in flight across the rollover still holds a token with
about thirty minutes of life (`microvms-core/src/session/proxy.rs:29-37`). A
mint failure is retryable; treat it that way, because a control-plane throttle
at minute thirty must not kill a healthy run.

Token rotation costs nothing on the daemon side. All exec state — the records,
the buffered output, the stream cursors — lives in the daemon, keyed by
`exec_id`, so a detached exec started under one proxy token is polled and acked
under the next one. Start, rotate, poll, ack is a normal sequence, not a
recovery path. This is a tested contract, not an inference: the live suite's
`reattach after token rotation` section starts a detached exec, drops every
piece of client state except the endpoint, the agent token, and the MicroVM id,
reattaches under freshly minted proxy tokens, and asserts that the output
produced *before* the reattach comes back whole — nothing buffered under one
token is lost to the next (`conformance/run_rs.py`, `drive_token_rotation`).

## The idle keepalive is yours, and it must run outside the VM

The platform measures idleness by inbound traffic through the endpoint proxy
and suspends a VM whose window elapses without any. Your harness — the
orchestrator outside the VM — owns the keepalive: poll `GET /v1/health` on an
interval well under the launch's `maxIdleDurationSeconds`, and each poll is the
inbound traffic that resets the timer. Measured, both halves: a polled VM
outlives its idle window and the same VM suspends once the polling stops
(`docs/PLATFORM.md`, "An outside poll of `/v1/health` does reset the idle
timer"; asserted every live run by `conformance/run_rs.py`,
`drive_idle_keepalive`).

An in-guest keepalive **cannot** work, and it is worth knowing why before
someone builds one: the endpoint proxy terminates *outside* the VM and forwards
over loopback, so a request a guest process sends to the daemon's own port is
generated on the far side of the meter and never crosses it. A guest-side
keepalive route would answer 200 and change nothing, and the failure would
surface as a suspend during exactly the long run it was added to protect
(`docs/HARNESS-CAPABILITIES.md`, gap 6). Neither does in-guest *work*: a VM
running a multi-hour exec with no outside traffic is suspended mid-work at the
idle window. The process survives — suspend is a freeze, not a kill — but
nothing external can reach it until someone resumes it.

`/v1/health` is the right route for the poll: unauthenticated, one small
request, and it carries `busy` and `execs` so the poll is informed rather than
unconditional — an orchestrator can stop keeping a drained VM alive instead of
billing it to the duration ceiling.

## What the hand-rolled daemons needed, and where agentd covers it

The two daemon shapes this supersedes are described in
`docs/HARNESS-CAPABILITIES.md`; neither project is a dependency of this repo,
so the rows are the generic needs.

| Need | Who had it | agentd |
| --- | --- | --- |
| Start/poll/ack exec that outlives an auth-token ceiling | evaluation harnesses | caller-minted `exec_id`, idempotent start, read-only poll, explicit ack, TTL only after ack (`agentd/src/exec.rs`) |
| Idempotent start under retry | evaluation harnesses | a known id returns success without spawning a second child (`agentd/src/exec.rs:364-367`) |
| Per-exec env, cwd, user/group, timeout | evaluation harnesses | in the wire protocol and applied by the daemon; the child's environment starts empty, so the token never leaks into it |
| File and directory-tree transfer with tar fidelity | evaluation harnesses | streamed file routes plus confined tar extraction (`agentd/src/fs.rs`) |
| Per-instance credential bootstrap, no secret in the shared image | both | one-shot `runHookPayload` bootstrap with replay semantics (`agentd/src/routes.rs:166-216`) |
| Lifecycle hooks answered so the platform can manage the VM | session servers | ready/validate/run/suspend/resume/terminate all served (`agentd/src/routes.rs:112-118`) |
| A liveness probe cheaper than an exec | session servers | unauthenticated `GET /v1/health` |
| Live output streaming with resume | neither had it | SSE with byte-cursor resume and explicit gap events (`agentd/src/exec.rs:436-524`) |

## Configuration knobs

Every `AGENTD_*` variable is read at startup by `Config::from_env`
(`agentd/src/config.rs:116-152`); an unset or unparseable value keeps the
default rather than refusing to boot, because a daemon that will not start
strands the VM with no way in. Set them as `ENV` lines in your Dockerfile —
the stanza already sets the first two.

| Variable | Default | What it bounds |
| --- | --- | --- |
| `AGENTD_PORT` | `9000` | the port the control API and hooks listen on (`agentd/src/config.rs:15`) |
| `AGENTD_LOG` | `info` | the tracing filter, standard `EnvFilter` syntax (`agentd/src/main.rs:91`) |
| `AGENTD_MAX_BODY_BYTES` | 512 MiB | largest request body accepted on the wire (`agentd/src/config.rs:17-19`) |
| `AGENTD_MAX_OUTPUT_BYTES` | 8 MiB | per-stream cap on captured exec output; exceeding it truncates and marks the result (`agentd/src/config.rs:25-27`) |
| `AGENTD_OUTPUT_LINGER_SECS` | `5` | how long to keep reading pipes after the child exits, for grandchildren holding them (`agentd/src/config.rs:28-31`) |
| `AGENTD_EXEC_TTL_SECS` | `900` | how long an acked exec entry is retained before collection (`agentd/src/config.rs:32-33`) |
| `AGENTD_STREAM_BUFFER_BYTES` | 1 MiB | bytes of recent output kept for stream replay; a reattach past it gets a gap event (`agentd/src/config.rs:41-45`) |
| `AGENTD_STREAM_CHANNEL_CAPACITY` | `256` | slots in an exec's live fan-out channel; a lagging subscriber re-reads the ring instead of losing output (`agentd/src/config.rs:46-49`) |
| `AGENTD_SSE_KEEPALIVE_SECS` | `15` | interval between SSE keep-alive comments, so a silent exec does not look like a dead connection (`agentd/src/config.rs:50-53`) |
| `AGENTD_MAX_STDIN_WRITE_BYTES` | 1 MiB | largest single decoded stdin write (`agentd/src/config.rs:54-57`) |
| `AGENTD_DISK_RESERVE_BYTES` | 256 MiB | free bytes a write target must keep; a write that would cross it is refused with 507. Zero disables the guard (`agentd/src/config.rs:63-69`) |
| `AGENTD_REPAIR_IDENTITY` | `true` | whether to replace image-derived identity at startup, because N VMs restored from one snapshot share machine-id, hostname, and boot_id. `0`/`false`/`no`/`off` opt out (`agentd/src/config.rs:70-78`) |
