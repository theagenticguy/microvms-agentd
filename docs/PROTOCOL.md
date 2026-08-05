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
| `POST /v1/exec/{id}/ack` | bearer | release output, enter TTL collection |
| `POST /v1/exec/{id}/kill` | bearer | signal escalation to the process group |
| `PUT /v1/fs/tar` | bearer | streaming tar upload and confined extraction |
| `GET /v1/fs/tar?path=` | bearer | streaming tar download |
| `PUT /v1/fs/file` | bearer | write one file |
| `GET /v1/fs/file?path=` | bearer | read one file |
| `GET /v1/health` | none | liveness, version, bootstrap state |

## Rules that exist because a defect proved them necessary

Each of these was bought with a real bug during the Harbor PR #2469 integration.
They are protocol contract, not implementation preference.

**Bootstrap is one-shot, and a replay of the identical token succeeds.** A first
`/run` installs the token and returns 200. A later `/run` carrying the same token
returns 200, because the platform may retry its own hook and must not be told the
VM is broken. A later `/run` carrying a different token returns 409 and changes
nothing. The model in `model/` checks this over every interleaving, including a
racing in-VM caller.

**Control routes answer 503 before bootstrap.** Not 404, and not a connection
drop.

**A missing or malformed body key is 400, never 404.** Clients map 404 onto
"file not found", so the wrong code turns a protocol typo into a phantom absent
artifact — which is exactly how one defect hid.

**Authorization is decided before any body byte is read.** An unauthenticated
caller must not be able to make the daemon allocate. Rejected requests still
drain a small body so pooled client connections keep working; larger ones close.

**Token comparison happens on bytes, in constant time.** Comparing `str` values
raises on non-ASCII input in some languages, and any caller controls the header —
that was a trivially reachable denial of the connection.

**No exception on the parse, auth, or routing path may drop a connection.** A
catch-all returns 500. Raw TLS handshake bytes get a 400 and a debug log, since
something in the platform's path probes the port with TLS first.

**`cwd` is omitted when unset.** When the client sends no working directory, the
daemon emits no `cd` prefix and the child inherits the daemon's own working
directory, which is the image `WORKDIR` because the daemon is the container
`CMD`. Forcing `/` breaks prebuilt-image tasks and defeats any harness that
discovers the image workdir with `pwd`.

**Exec is idempotent on a caller-minted `exec_id`.** A retried `/exec/start`
returns success without spawning a second child. Polling is read-only. Output
lives until the caller acks, and only acked entries are collected. The Python
predecessor unlinked output files at child exit, which destroyed anything a
backgrounded grandchild wrote afterward.

**No shell unless asked.** An argv array execs directly. `shell: true` wraps in
`sh -c` with the command as a single argument. The predecessor's brace-group
wrapper turned empty and comment-terminated commands into syntax errors and let
an unbalanced `}` escape the group.

**Tar extraction mirrors the CPython `data` filter contract.** In-tree symlinks
are preserved, because harnesses legitimately pack them; absolute link targets
are refused; relative targets must resolve under the root, resolved with
`normpath` semantics rather than `realpath` so a symlink written earlier in the
same archive cannot redirect a later member; symlinks resolve relative to their
own directory while hard links resolve against the archive root; member count and
total size are capped; modes are applied after content lands.

**Bodies stream to disk, and caps are enforced on the wire.** The predecessor
buffered whole archives in memory on a VM whose baseline can be 512 MiB, where an
OOM-killed daemon is unrecoverable. It also measured archive size inside the gzip
`with` block, where the stream is unflushed: `tell()` reported 10 bytes for a
327-byte archive, making the guard nearly decorative.

**Output is bounded with an explicit truncation marker,** and a post-exit linger
deadline bounds how long the daemon waits on grandchildren still holding the pipe.

## Trust boundary

The platform's `/run` hook arrives from `127.0.0.1` and is indistinguishable at
the socket level from a request sent by a process inside the VM (measured; see
`PLATFORM.md`). Source-address filtering is therefore wrong rather than merely
unverified.

The defenses that remain, all of them checked in `model/`:

1. Bootstrap is one-shot, so a losing racer never replaces the winner's token.
2. A post-bootstrap hijack attempt is refused at the hook with 409 and at the
   control API with 401.
3. The agent token never enters an exec'd child's environment.

The residual risk is stated plainly rather than dismissed: the daemon being the
container `CMD` and the harness issuing its first exec only after readiness is an
*unenforced* invariant. A base image that starts its own background process
before bootstrap breaks it. `model/` includes that configuration and reports the
counterexample path, so the cost of the invariant is a checked fact rather than a
paragraph. Enforcing it belongs to whoever builds the image, not to this daemon.
