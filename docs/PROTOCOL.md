# Wire protocol v1

The lifecycle hook paths are fixed by the platform, so they stay unversioned.
Everything this project owns lives under `/v1/`. Every response carries a
`microvms-agentd-version` header.

## Routes

Hooks are served under `/aws/lambda-microvms/runtime/v1/`, abbreviated `HOOKS`
below. That prefix is fixed by the service, so a daemon serving a bare `/run`
never gets bootstrapped.

| Route | Auth | Purpose |
| --- | --- | --- |
| `POST HOOKS/ready` | none (platform hook) | image-build readiness probe |
| `POST HOOKS/validate` | none (platform hook) | image-build validation probe |
| `POST HOOKS/run` | none (platform hook) | one-shot token bootstrap from `runHookPayload` |
| `POST HOOKS/suspend` | none (platform hook) | acknowledged and logged |
| `POST HOOKS/resume` | none (platform hook) | acknowledged; signals in-memory state loss |
| `POST HOOKS/terminate` | none (platform hook) | acknowledged; begins graceful shutdown |
| `POST /v1/exec/start` | bearer | start a command under a caller-minted `exec_id` |
| `GET /v1/exec/{id}` | bearer | poll status and output; never mutates |
| `GET /v1/exec/{id}/stream?offset=` | bearer | follow output as SSE from a byte offset |
| `POST /v1/exec/{id}/stdin` | bearer | write to a child's stdin, or signal EOF |
| `POST /v1/exec/{id}/ack` | bearer | release output, enter TTL collection |
| `POST /v1/exec/{id}/kill` | bearer | signal escalation to the process group |
| `PUT /v1/fs/tar` | bearer | streaming tar upload and confined extraction |
| `GET /v1/fs/tar?path=` | bearer | streaming tar download |
| `PUT /v1/fs/file` | bearer | write one file |
| `GET /v1/fs/file?path=` | bearer | read one file |
| `GET /v1/health` | none | liveness, version, bootstrap state |

## Rules that exist because a defect proved them necessary

Each of these rules comes from a real bug found during the Harbor PR #2469
integration. They are part of the protocol contract, so implementations must
follow them.

**Bootstrap is one-shot, and a replay of the identical token succeeds.** A first
`/run` installs the token and returns 200. A later `/run` carrying the same token
returns 200, because the platform may retry its own hook and must not be told the
VM is broken. A later `/run` carrying a different token returns 409 and changes
nothing. The model in `model/` checks this over every interleaving, including a
racing in-VM caller.

**Control routes answer 503 before bootstrap.** They do not answer 404, and
they do not drop the connection.

**A missing or malformed body key is 400, never 404.** Clients map 404 onto
"file not found". Because of that mapping, returning 404 for a protocol typo
makes the client believe an artifact is absent when it is not. One defect went
undetected this way.

**Authorization is decided before any body byte is read.** An unauthenticated
caller must not be able to make the daemon allocate. Rejected requests still
drain a small body so pooled client connections keep working; larger ones close.

**Token comparison happens on bytes, in constant time.** Comparing `str` values
raises on non-ASCII input in some languages. Any caller controls the header, so
a non-ASCII header value could crash the connection.

**No exception on the parse, auth, or routing path may drop a connection.** A
catch-all returns 500. Raw TLS handshake bytes get a 400 and a debug log, since
something in the platform's path probes the port with TLS first.

**`cwd` is omitted when unset.** When the client sends no working directory, the
daemon emits no `cd` prefix, and the child inherits the daemon's own working
directory. Because the daemon is the container `CMD`, that directory is the
image `WORKDIR`. Forcing `/` breaks prebuilt-image tasks and defeats any harness
that discovers the image workdir with `pwd`.

**Exec is idempotent on a caller-minted `exec_id`.** A retried `/exec/start`
returns success without spawning a second child. Polling is read-only. Output
lives until the caller acks, and only acked entries are collected. The Python
predecessor unlinked output files at child exit, which destroyed anything a
backgrounded grandchild wrote afterward.

**A shell wraps the command only when the caller asks for one.** An argv array
execs directly. `shell: true` wraps in
`sh -c` with the command as a single argument. The predecessor's brace-group
wrapper turned empty and comment-terminated commands into syntax errors and let
an unbalanced `}` escape the group.

**Tar extraction mirrors the CPython `data` filter contract.** In-tree symlinks
are preserved, because harnesses legitimately pack them. Absolute link targets
are refused. Relative targets must resolve under the root, using `normpath`
semantics rather than `realpath`. Symlinks resolve relative to their own
directory, while hard links resolve against the archive root. Member count and
total size are capped. Modes are applied after content lands.

**A symlink an archive wrote cannot redirect a later member, and the kernel is
what enforces that.** The `normpath` rule above judges a member at the depth its
name implies, and that is not the same as the depth the write reaches once a
symlink is in the path. So the daemon opens the extraction root once and creates
every member relative to that descriptor with `openat2`, using
`RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS | RESOLVE_NO_MAGICLINKS`. A member whose
path would traverse a symlink is refused with 400 rather than written somewhere
else. `openat2` needs Linux 5.6 or newer. On an older kernel the syscall answers
`ENOSYS` and extraction answers 500, which is a refusal rather than a silent
fall back to the weaker check.

**Bodies stream to disk, and caps are enforced on the wire.** The predecessor
buffered whole archives in memory on a VM whose baseline can be 512 MiB, where an
OOM-killed daemon is unrecoverable. It also measured archive size inside the gzip
`with` block, where the stream is unflushed. There, `tell()` reported 10 bytes
for a 327-byte archive, so the size guard almost never fired.

**Output is bounded, and truncation is marked explicitly.** A post-exit linger
deadline bounds how long the daemon waits on grandchildren still holding the pipe.

## Streaming and stdin

Both features serve one consumer, an agent harness running inside the VM. The
harness emits output for minutes and may need a prompt written to it. Polling
serves neither need well, because it re-sends the whole buffer each time and
truncates at the output cap.

**The stream is a read-only view of the exec.** An exec is a server-side record
keyed by its caller-minted `exec_id`. Attaching, detaching, or dropping a
connection must not affect the command. Both views must keep working, so poll
returns the buffer, stream follows it, and neither disturbs the other.

**Resume is by byte offset.** `?offset=N` yields exactly the bytes after N, so a
client that reconnects can pick up where it left off. For comparison, E2B's
reattach takes no offset, so a reconnecting E2B client loses everything produced
during the gap. A reattach past the retained window gets an explicit `gap` event
naming the missing range. Without that event, the client would keep streaming
and never learn that bytes were skipped.

**SSE is used because it can carry a typed terminal event.** A raw chunked byte
stream cannot distinguish a finished command from a dropped connection, because
the bytes are identical in both cases. The stream therefore emits a typed `exit`
event carrying the status and *then* ends. Keep-alive comments fill silences, so
an agent harness thinking for two minutes does not look like a dead connection.

**stdin is opt-in and a separate request.** A command that does not ask for stdin
gets `Stdio::null()`, so nothing inherits a surprise descriptor. Writing to a
command that did not request stdin returns 409. Writes go to
`POST /v1/exec/{id}/stdin` and are never multiplexed onto the output connection.
Because the two connections are separate, a dropped attach cannot corrupt stdin.
EOF is an explicit signal rather than inferred, because a child reading stdin
cannot exit until the daemon drops its own handle. `Child::wait()` drops the
child's copy of the handle, not the daemon's.

## Reconnect-at-cursor across a real suspend, measured

The claim above — that resume by byte offset is what distinguishes this from a backend
whose reattach loses the gap — was asserted from the daemon's side only. The cut under
unit test is a response body ending without an exit frame, which is the condition the
reconnect keys on, but nothing had put a real platform suspend across a live exec.

Measured 2026-08-15, us-east-1, through `Session.spawn` in `microvms-js` against a
guest ticking once a second and also appending each tick to a file, so the guest's own
record is an oracle independent of the stream under test:

| | Observation |
| --- | --- |
| Before the suspend | `tick-1`, `tick-2`, `tick-3` delivered on the handle's `stdout` |
| Suspend / resume | `SUSPENDED`, held ~20s, `RUNNING` |
| The handle already held | Resumed at **`tick-4`** — contiguous, no restart, no duplicate |
| Gaps recorded | **None**, on either the held handle or a fresh one |
| A fresh handle at `offset: 0` | Replayed **200 of 200** ticks, indices contiguous `1..200` |
| The guest's own file | Agreed with the stream at every point |

So the property holds against the thing it was designed for, and it holds in both
shapes: the handle a caller was already reading recovers at its cursor, and a *new*
handle for the same `exec_id` replays the whole output across the suspension. The exec
itself is untouched by the freeze, which is the `PLATFORM.md` finding this depends on.

The other three `ExecProcess` properties were measured on the same VM. `stdout` and
`stderr` arrive as separate streams carrying only their own bytes, from one interleaved
SSE channel with a per-frame discriminator. `wait()` reports the daemon's real exit code
— 0 for a success, 42 for `exit 42` — because it reads the exec record rather than
inferring from the stream ending. `kill()` terminates the process group and a second
call succeeds rather than 404ing, and `wait()` afterwards reports `signal: 15` with no
exit code, so a killed build cannot read as passing.

## Credentials for a caller's own connection

`Session::connect_headers(port)` and `connect_subprotocols(port)` hand a caller what it
needs to open its own connection to some other port on the same VM — the header pair for
HTTPS, the three subprotocols for a WebSocket.

**Both mint a token scoped to the port they name, and that is load-bearing rather than an
implementation detail.** The service scopes a proxy token at mint time, so a token minted
for the agent port answers 403 `Access to port denied` for any other port and close code
1006 with no reason on a WebSocket. An implementation that reused the session's cached
token would therefore return a *correct-looking* port value behind a credential that does
not authorize it — which is what this client did until the live run of 2026-08-15 caught
it. See `PLATFORM.md`, "`allowedPorts` is a union of three forms".

The scope is extended rather than replaced: a mint for a new port asks for every port
already covered plus that one, so warming the cache for a workload port does not cost the
session access to its own. One extra control-plane call per new port, cached thereafter,
and `proxy_mint_count()` is the observable that says so.

**A caller debugging a failed WebSocket should retry the same port over HTTPS with
`connect_headers`.** Every handshake failure is 1006 and none of them says why; the HTTPS
request distinguishes 403 (wrong scope) from 502 (right scope, nothing listening).

## Trust boundary

The platform's `/run` hook arrives from `127.0.0.1` and is indistinguishable at
the socket level from a request sent by a process inside the VM (measured; see
`PLATFORM.md`). Filtering by source address therefore cannot separate the
platform from an in-VM process, so it provides no protection here.

The remaining defenses, all checked in `model/`, are the following:

1. Bootstrap is one-shot, so a losing racer never replaces the winner's token.
2. A post-bootstrap hijack attempt is refused at the hook with 409 and at the
   control API with 401.
3. The agent token never enters an exec'd child's environment.

One risk remains. The design assumes the daemon is the container `CMD` and that
the harness issues its first exec only after readiness. The daemon does not
enforce this invariant. A base image that starts its own background process
before bootstrap breaks it. `model/` includes that configuration and reports the
counterexample path, so the consequence of breaking the invariant is a checked
result rather than a prediction. Enforcing the invariant is the responsibility
of whoever builds the image, not of this daemon.
