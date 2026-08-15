# What agent harnesses require of this platform

This document maps the sandbox contracts of three agent harnesses onto this
platform's current surface, and ranks the gaps. The harnesses are Harbor
(agent evaluation), Omnigent (server-managed agent sessions), and the Vercel
Sandbox / eve shape (agent-tool sandboxes). None of them becomes a
dependency: their contracts were read from their source and public docs, and
what this platform ships stays generic: capabilities any workload can use,
never integrations that track a harness's release cadence.

Sources: Harbor read at `harbor-framework/harbor` (local checkout,
`feat/lambda-microvm-environment`); Omnigent read at PR #2217's branch;
Vercel Sandbox from vercel.com/docs/sandbox (retrieved 2026-08-14, SDK
v2.9.2) and eve.dev/docs/sandbox; this repo read on `main` at d80150a.

## The three contracts, compressed

**Harbor** drives a sandbox through one abstract class: exec (shell command,
cwd, per-exec env dict, run-as-user with HOME/groups fixup, timeout with
process-group kill and code 124, full stdout/stderr capture), file transfer
(single files with mode, directory trees with tar fidelity: modes, symlinks,
empty dirs), lifecycle (build-from-Dockerfile with content-addressed reuse,
readiness signal, terminate/suspend), and per-instance credential bootstrap
that never bakes a secret into a shared image. Commands must be startable
idempotently under retry and outlive any auth-token ceiling, which is why
Harbor's own MicroVM provider hand-rolled a start/poll/ack daemon. Optional
tiers Harbor degrades around: multi-container, GPUs, Windows, dynamic
network policy, live output streaming.

**Omnigent** needs lifecycle more than exec. Its host dials out to the
server over a WebSocket tunnel, so the sandbox needs no inbound API at all;
what it needs is: create (or reserve-an-id-then-create), idempotent
terminate, suspend-to-snapshot that preserves the whole process tree,
idempotent resume under the same identity, cheap liveness, a per-launch
env/secret channel delivered before the workload starts, and a published
lifetime cap so launch-token TTLs can be derived above it. Its provider plus
a deploy shim hand-rolled a lifecycle-hooks HTTP server, a 16 KB
`runHookPayload` env channel, idle-policy derivation math, and
NotFound/Conflict-to-success mappings, all of which become deletable if the
platform supplies those natively. An exec daemon also lets Omnigent use its simpler
exec-model launcher, where repo clone and config injection are shared
framework code instead of in-image shell scripts.

**Vercel Sandbox / eve** is an API shape rather than a consumer: named,
persistent-by-default sandboxes (stop auto-snapshots; the next call
resumes), blocking and detached exec with a durable command id, replayable
buffered output, live log streaming, kill with signal, sudo, cwd/env, batch
file writes as gzipped tarballs, a `node:fs/promises`-shaped metadata
surface, declared ports mapping to public URLs, SNI-domain and CIDR egress
policy applied to the live session, and snapshot/fork. Agent frameworks
layered on it need far less: eve's backend adapter is about ten session
methods (run, spawn with byte streams and kill, read/write file, remove,
resolve path, set network policy, stop), and the AI SDK's canonical
sandbox tool needs only create, blocking exec with captured output, and
teardown.

## Where the platform already meets them

The daemon and client cover more of these contracts than any of the three
providers' hand-rolled daemons did, with proofs behind each behavior:

- Idempotent detached exec: caller-minted exec id, retry-safe start,
  read-only poll, explicit ack, TTL only after ack, so unread output is never
  destroyed (`agentd/src/exec.rs`). This is precisely the start/poll/ack
  model Harbor's provider built, plus SSE streaming with byte-cursor resume
  and explicit gap events, which Harbor's daemon lacks.
- Per-exec `env`, `cwd`, `user`/`group`, `timeout_sec`, `stdin`, and shell
  vs argv mode are all in the wire protocol and applied by the daemon
  (`protocol/src/exec.rs:116`, `agentd/src/exec.rs:999-1019`), and exposed
  by the Rust, Python, and Node clients.
- Kill with SIGTERM-grace-SIGKILL to the process group; per-command timeout
  enforced daemon-side when requested.
- File transfer: streamed single-file read/write with mode-at-open, tar
  upload/download with a confined extraction path (lexical resolution,
  symlink and bomb defenses, member/size caps), disk-pressure refusal with
  the real numbers (`agentd/src/fs.rs`).
- Per-VM secret bootstrap through `runHookPayload` with one-shot semantics
  and traffic ordering guaranteed by the platform; the token never enters a
  child's environment.
- Suspend/resume that preserves memory, filesystem, token, running
  processes, and exec records (measured, `docs/PLATFORM.md`); local
  refusal of illegal transitions with zero billable calls.
- Teardown that never raises, reports leaked identifiers, and a local
  ledger (`microvm ls`) that records leaks before attempting deletes.
- Image builds from a Dockerfile with local pre-flight of the two platform
  traps (FROM/base agreement, WORKDIR requirement) and clientToken replay
  protection.
- A machine-readable manifest, one JSON envelope per invocation, stable
  error codes, and append-only exit codes: the agent-friendly CLI surface
  none of the three harnesses' providers had to build against before.

## The gaps, ranked

Ranked by how many harnesses need it, times how much hand-rolled code it
deletes, over the cost of building it here.

**1. Expose per-exec env (and user) through the CLI.** The daemon applies
`env` per request and the bindings expose it; the CLI hardcodes
`env: HashMap::new()` (`microvms-cli/src/commands/lifecycle.rs:677-707`).
Every harness passes env per exec (Harbor merges three layers of it on
every call), and the PATH failure the coding-agents example documents is
this gap biting a real workload. `exec --env KEY=VALUE` (repeatable) plus
`--user`/`--group` makes the CLI equal to the bindings. Smallest change,
highest reach.

**2. Ship the platform daemon as the reusable answer to "no exec API".**
Harbor and Omnigent each carry a several-hundred-line stdlib Python daemon
baked into task images. agentd already does everything those daemons do,
better tested. What is missing is packaging: a documented recipe (and a
`Dockerfile` stanza helper) for appending agentd to an arbitrary task image,
so a harness provider is a thin client over the published wire protocol
instead of a daemon author. The coding-agents example is the seed; this is
its generalization.

**3. Image name resolution and content-addressed reuse in the CLI.**
`run --image` passes the identifier verbatim to the service, which rejects
bare names ("Malformed ARN"); both Harbor's provider and our example resolve
ARNs by listing, and both key image names to content hashes to avoid the
stale-snapshot-on-name-reuse hazard. Resolve names client-side and offer
`build --reuse` keyed on artifact content hash.

**4. A per-launch environment channel. Shipped.** Omnigent's whole
hooks-server shim existed because the platform offers no per-launch env
vars; the `runHookPayload` is the only per-VM secret channel and it carried
exactly one token. The run hook now accepts an optional `env` map in the
same payload and the daemon applies it as the base environment of every
later exec, with the per-request `env` winning on a shared key.
`RunRequest::with_launch_env`, `run --launch-env KEY=VALUE`, and both
bindings expose it. Two things the design pinned rather than left open: the
token never becomes part of that base environment, proven by a test that
asserts a child's whole environment equals the launch map; and only the
first successful bootstrap sets it, so a caller who cannot win the token
cannot rewrite the environment either. The payload budget is 4096 bytes and
not 4 KB of headroom — it is shared with the token, and `microvms-core`
refuses an over-budget payload locally, naming the env's share of it, since
AWS's own answer arrives after the call and botocore does not check.
Credential-scale material still belongs on the file path or a role.

**5. Session-lifetime alignment for long execs.** Harbor's daemon exists
partly because commands must outlive the 60-minute proxy-token ceiling. Our
detached exec already survives it (state lives in the daemon; the client
re-minted token reattaches), but nothing documents or tests the
reattach-after-token-rotation path explicitly. A conformance check plus a
doc section turns an accident of design into a contract.

**6. Idle-signal correctness for outbound-tunnel workloads. Shipped, with
the naive half ruled out.** The platform measures idleness only by inbound
endpoint traffic. Omnigent's host holds an outbound tunnel and receives
none, so auto-suspend can freeze a VM mid-turn; multi-hour agent runs past
400 minutes are the case that hurts.

The parenthetical above — "a trivial periodic authenticated request" — is
the thing that does not work, and the reason is measured rather than
argued. The endpoint proxy terminates *outside* the VM and forwards over
loopback (`docs/PLATFORM.md`), so a request an in-VM process sends to the
daemon's port is generated on the far side of the meter and never passes
through it. A keepalive route inside the guest would answer 200 and change
nothing, discovered as a suspend during exactly the long run it was added
to protect.

So `GET /v1/health` now carries `busy` and `execs`, and the consumer is an
orchestrator outside the VM whose own poll *is* the inbound traffic. That
also keeps the assertion repeated and explicitly the caller's, which is
what rules out the daemon self-keepaliving: a hung process would otherwise
bill silently to the 8-hour ceiling. `busy` is "producing", not
"unfinished" — an exited exec awaiting an ack reads false — and `execs`
counts every registered entry so a caller can tell a drained VM from one
holding output nobody read. Not measured: that a poll from outside does in
fact reset the timer. It is inbound endpoint traffic by construction, so it
should, but nobody has run a VM to the edge of its idle window while
polling and watched it survive.

**7. An eve backend adapter (separate package, later).** Ten session
methods over the Node binding makes this platform a pinnable eve backend:
real VMs where the consolidator today accepts a pure-JS bash interpreter.
Worth doing as its own repo once 1-3 land; it depends on eve's types, so it
can never live here.

## Explicit non-goals

- **Vercel wire compatibility.** The valuable seam is eve's adapter, not
  Vercel's REST surface. Snapshot/fork-on-stop, the heart of Vercel's
  persistence model, needs snapshot-to-image, which is the standing AWS
  platform ask (`docs/STRATEGY.md`), not something this client can build.
- **Multi-container.** `ResourcesList.max = 1` is a platform constant.
  Harbor treats single-container as an accepted tier; Omnigent needs one
  container; nothing here changes.
- **Harness provider classes.** The Harbor `BaseEnvironment` subclass, the
  Omnigent launcher, and the eve backend all import their harness's
  packages, so they live in those ecosystems (or standalone adapter repos),
  never here. This repo's deliverable is the daemon, the clients, and the
  published behavior they can rely on.
- **GPUs, Windows, dynamic network policy.** Not offered by the platform;
  harnesses that need them reject the environment up front, which is the
  correct degradation.

## What this changes next

Items 1 and 3 are CLI work measured in hours and unblock every harness
equally. Item 2 is documentation plus a small helper. Item 7 waits for the
first three.

Items 4 and 6 have shipped, and the three design decisions this section
predicted they would need were the right three. Payload budget: the token
and the env share 4096 bytes, checked locally before the launch because
neither AWS nor botocore gives a caller a signal in time. Env precedence:
the launch env is the base and the per-request map is overlaid, which leaves
the existing per-request contract unchanged for anyone who sends no launch
env. Busy semantics: producing rather than unfinished, reported to an
orchestrator *outside* the VM, because the guest-side keepalive this
document floated cannot work against a proxy that terminates outside the
guest. Item 5 is still open: the reattach-after-token-rotation path works by
design and nothing tests or documents it explicitly.

Reading `readTextFile` from the AI SDK sandbox contract while item 4 was
being built turned up one gap this document had missed. That method takes
1-based inclusive `startLine`/`endLine` and returns through EOF when
`endLine` is past the end, and `GET /v1/fs/file` had no way to express it —
a harness implementing it over this daemon would read whole files and slice
them client-side, which on a multi-megabyte file is the transfer this route
exists to avoid. The route now takes `start_line` and `end_line` with
exactly those semantics, still streamed, with the un-ranged read
byte-identical to what it was.
