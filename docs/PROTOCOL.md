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
| `POST HOOKS/run` | none (platform hook) | one-shot token bootstrap from `runHookPayload`, plus the optional launch environment |
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
| `GET /v1/fs/file?path=&start_line=&end_line=` | bearer | read one file, or a 1-based inclusive line range of it |
| `GET /v1/health` | none | liveness, version, bootstrap state, exec-activity |

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

## The launch environment

The run-hook payload may carry an `env` map alongside `agent_token`. It becomes
the base environment of every later exec.

```json
{"runHookPayload": "{\"agent_token\": \"...\", \"env\": {\"KEY\": \"VALUE\"}}"}
```

**The per-request `env` wins on a key both set.** The daemon applies the launch
env first and the request's own map second, so a launch env is a default for the
whole VM and a request is the specific thing happening now. A caller who never
sends a launch env sees no change: the child's environment is the request's map
and nothing else, exactly as before.

**The agent token never becomes part of it.** `env_clear()` still runs, so the
daemon's own environment reaches no child, and the launch env and the token are
separate parameters through the whole install path — there is no field a refactor
could forward one into. Proven by a test that spawns `/usr/bin/env` with a launch
env installed and asserts the child's environment is *exactly* that map: an extra
variable of any name fails it, which is what a leak would look like.

**Only the first successful bootstrap sets it.** A replay of the identical token
answers 200 and leaves the installed env alone, and a conflicting token answers
409 and leaves it alone. Without that, a caller who cannot win the token could
still rewrite the environment every later child runs in.

**Every value is a string, and a malformed `env` is 400 naming the problem.** A
non-object `env`, or a value that is a number or a nested object, is refused with
a body that names the key or the shape. The body never quotes a value, because
the payload carries the token.

**Unknown payload keys are ignored.** A 400 at this hook makes the platform
terminate the VM before forwarding any traffic, so a newer client sending a field
this daemon has never heard of must still be able to bootstrap it. Forward
compatibility here is the difference between an ignored field and a dead launch.

**The token and the env share one 4096-byte budget, measured in UTF-8 bytes and
inclusive** (`PLATFORM.md`). The daemon cannot enforce what the platform already
rejected — an over-ceiling payload never reaches the guest — so the check is on
the *client* side, in `microvms-core`'s `RunHookPayload::for_launch`, and it fires
before any control-plane call. botocore does not enforce the ceiling either, so
without that local check there is no signal at all until AWS answers with a
`ValidationException` on a member the caller did not know they were filling. The
refusal names the byte count, the ceiling, and how much of it the env is, because
"4142 bytes, ceiling 4096" alone does not say whether to shorten the token or drop
a variable. One bearer token fits with room to spare; a set of AWS session
credentials does not, and that is what makes this ceiling reachable in practice.

## Line-ranged text reads

`GET /v1/fs/file` takes optional `start_line` and `end_line`. The semantics are
the AI SDK harness contract's `readTextFile`, copied rather than chosen, because
this route is what that method is implemented on top of:

**Both bounds are 1-based and inclusive.** `start_line=2&end_line=4` is three
lines. `start_line` absent means 1 and `end_line` absent means through EOF.

**An `end_line` past the last line reads through EOF without an error.** Lines
1..1000 of a twelve-line file is a 200 carrying twelve lines, never a 416. A
`start_line` past the last line is an empty 200, not a 404: the file is there and
the window is empty, which is a different fact from the file being absent.

**A line owns its terminating newline.** Lines 1..2 and lines 3..5 concatenate
back into the file rather than losing a separator at the seam. A last line with no
trailing newline has none to own and is returned as it is.

**`start_line=0` and `end_line < start_line` are 400.** Neither is 416: 416 is
about a range the file cannot satisfy, and both of these are ranges no file could,
so a client sent to look at the file would be looking in the wrong place. A
non-integer bound is also 400, and it does not masquerade as the missing-`path`
refusal.

**A range still streams.** The read is filtered chunk by chunk and stops reading
once the window closes, so lines 1..5 of a large file cost the first chunk. Nothing
buffers a file to slice it, for the same reason nothing buffers an upload: an
OOM-killed daemon in a MicroVM is unrecoverable.

**With no range the response is byte-identical to what it always was.** The
un-ranged read hands back the reader stream untouched, so the path every existing
caller uses does not acquire the range feature's bug surface.

## Idle policy, and why liveness is a field rather than a route

The platform measures idleness by inbound traffic through the endpoint proxy
(`PLATFORM.md`, "`idlePolicy`"). A workload holding an outbound connection, or one
simply computing for hours, receives none, so auto-suspend can freeze a VM
mid-work. Multi-hour agent runs are the real case.

**A guest-side request cannot fix this, and the daemon does not pretend
otherwise.** The endpoint proxy terminates *outside* the VM and forwards over
loopback — measured, `PLATFORM.md`, "The platform's own hook arrives over
loopback". A request an in-VM process sends to the daemon's own port therefore
never reaches the thing counting traffic. A "keep myself alive" route would be a
keepalive that keeps nothing alive, and it would be discovered as broken by a
multi-hour run auto-suspending mid-work, which is the failure it was added to
prevent.

**So `GET /v1/health` carries `busy` and `execs`, for an orchestrator outside the
VM.** The orchestrator's own poll *is* the inbound traffic, and `busy` is what
makes that poll informed rather than unconditional. The assertion of liveness is
therefore repeated and is explicitly the caller's, which is the property that rules
out the daemon self-keepaliving: a hung process would then bill to the 8-hour
`maximumDurationInSeconds` ceiling with nobody having asked.

**`busy` means producing, not unfinished.** An exec whose child has exited and
whose result is waiting to be acked is not busy — nothing is running, and holding a
VM alive at baseline billing for a command that is over is the mistake this
distinction exists to prevent. `execs` counts every registered entry in any phase,
so `busy: false` with a non-zero count is a VM holding unacked output somebody
still has to collect before terminating it.

Both fields default to `false` and `0` when absent, unlike every other field on
the response. The daemon is baked into an image while a client is installed
separately, so a current client routinely talks to a daemon from whenever that
image was built; a required field would make a health call fail outright against an
older daemon, turning a missing signal into an unreachable VM.

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
