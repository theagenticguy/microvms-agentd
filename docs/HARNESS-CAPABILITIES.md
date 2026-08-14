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

**4. A per-launch environment channel.** Omnigent's whole hooks-server shim
exists because the platform offers no per-launch env vars; the 4 KB
`runHookPayload` is the only per-VM secret channel and it currently carries
exactly one token. Extending the daemon's run-hook handling to accept an
optional caller-supplied env map (delivered through the same payload,
applied to subsequent execs as base environment) gives both Omnigent's
entrypoint model and our own example a clean credential path without files.
The 4 KB payload budget is real; large secrets stay on the file path or a
role.

**5. Session-lifetime alignment for long execs.** Harbor's daemon exists
partly because commands must outlive the 60-minute proxy-token ceiling. Our
detached exec already survives it (state lives in the daemon; the client
re-minted token reattaches), but nothing documents or tests the
reattach-after-token-rotation path explicitly. A conformance check plus a
doc section turns an accident of design into a contract.

**6. Idle-signal correctness for outbound-tunnel workloads.** The platform
measures idleness only by inbound endpoint traffic. Omnigent's host holds an
outbound tunnel and receives none, so auto-suspend can freeze a VM
mid-turn. The daemon can see exec activity; exposing a "busy" signal (or
documented keepalive recipe, a trivial periodic authenticated request)
prevents mid-work freezes without platform changes.

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
equally. Item 2 is documentation plus a small helper. Items 4-6 are daemon
work with real design decisions (payload budget, env precedence, busy
semantics) and deserve their own proposals. Item 7 waits for the first
three.
