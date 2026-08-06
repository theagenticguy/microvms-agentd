# The trust contract for a control daemon inside a Lambda MicroVM

AWS Lambda MicroVMs give you an isolated Firecracker VM and no way to run anything
in it: no exec API, no file-transfer API. So every team building a coding-agent
sandbox on this substrate writes an in-VM daemon to supply both, and every one of
them inherits the same problem. The workload that daemon serves is a coding agent —
an untrusted, model-driven process that runs arbitrary code by design — and it lives
in the same network namespace as the API that controls its own sandbox. Bedrock
AgentCore addresses this with an external credential broker. A customer on raw
MicroVMs has no broker.

This document is the contract for that boundary, written so it can be implemented
without adopting `microvms-agentd`. Every claim about the platform carries its date
and region or is labeled as AWS documentation. Every claim about this
implementation names the file. Where the design is a judgment call rather than a
forced move, it says so and names the alternative.

## The threat model

The adversary is the workload, not the internet.

The daemon holds a bearer token delivered at launch and uses it to authorize
`/v1/exec/*` and `/v1/fs/*`. The adversary is any process inside the VM that the
harness did not intend to have that token: a background process baked into the base
image, a subprocess the agent spawned, or the agent itself reaching for authority
over its own sandbox rather than merely using it.

It can open a TCP connection to the daemon's port from inside the VM and send any
bytes on it, including the platform's own hook paths and any header value it likes.
It can poll the unauthenticated `/v1/health`. Once it holds the token it can run
commands as root, because that is what the control API is for.

What it cannot do is the part that makes the contract tractable. It cannot reach the
daemon from outside the VM without a platform-minted credential (below). It cannot
find the token in the logs: the token and the payload carrying it are never logged,
and the run-hook handler logs only outcomes (`agentd/src/routes.rs`, `run_hook`). It
cannot read the token off disk, because it never goes to disk — the installed token
lives in a `Mutex<Option<Vec<u8>>>` in process memory and nothing writes it out
(`agentd/src/state.rs`). It cannot make an unauthorized request cause the daemon to
allocate a request body.

One boundary this contract does *not* claim: a workload that already has root in the
VM can read the daemon's memory through `/proc/<pid>/mem` or `ptrace`, and the token
is in that memory. That follows from Linux process semantics rather than from
anything we measured, and nothing here defends against it. Since the control API
grants root by design, a token holder and a root workload are the same principal.
Everything below is about keeping a *non*-token holder from becoming one.

## What the platform gives you for free

Two properties, both from AWS documentation rather than our measurement, that shrink
the problem materially.

Every request to a MicroVM endpoint requires an `X-aws-proxy-auth` JWE scoped to a
specific MicroVM ID and port set, with a maximum lifetime of 60 minutes
(`microvms-networking.html`). There is no unauthenticated internet path to the
daemon's port. Port scoping is the useful half: a token minted for port 9000 cannot
reach port 8080, so a task workload and a control plane can share a VM with external
access handed to only one. The cost is that the 60-minute ceiling puts endpoint-token
minting inside every client retry path.

External traffic begins only after the `/run` hook returns 200
(`microvms-launching.html`). That is what makes it safe to deliver a per-VM secret
through `runHookPayload` at launch instead of baking a shared secret into an image
snapshot. It closes the first-writer race *through the endpoint*, and says nothing
about processes already running inside the VM.

## Why source-address filtering is wrong, not merely weak

The intuitive control is to accept the bootstrap hook only from the platform. Do not
implement it. It is not a weak defense resting on an unverified assumption; it is a
defect that breaks every launch.

Measured 2026-08-04, us-east-1, by instrumenting the daemon to log `client_address`
on every request and reading the result out of CloudWatch:

```
PROBE hook=run            client_address=('127.0.0.1', 36932)   headers={... 'host': 'localhost:9000'}
PROBE control=/exec/start client_address=('127.0.0.1', 36926)
```

The endpoint proxy terminates outside the VM and forwards inward over loopback. The
platform's own lifecycle hooks and the harness's control requests both arrive from
`127.0.0.1`, on ephemeral ports, indistinguishable at the socket level from a
request sent by a process inside the VM. A rule rejecting loopback callers on the
bootstrap route rejects the platform's legitimate bootstrap. We tried it; `PLATFORM.md`
records that the attempt broke 39 tests, and those failures were reporting the real
defect rather than a harness artifact.

One more observation from the same run, because it looks like an attack in a log and
is not: something in the platform's path probes the port with TLS before bootstrap, so
a plaintext HTTP server receives a ClientHello and answers
`400 Bad request version ("\x13\x01\x13\x02...")`. The correct response is a 400 and a
debug-level log line; what it must never do is take the listener down.

## The five defenses that remain

**One-shot bootstrap.** The first `/run` carrying a token installs it and answers
200. A later `/run` carrying the *identical* token also answers 200, because the
platform may retry its own hook and telling it the VM is broken fails a launch that
is fine. A later `/run` carrying a *different* token answers 409 and changes nothing
(`agentd/src/state.rs`, `Bootstrap`; `agentd/src/routes.rs`, `run_hook`). This is the
only defense available on that route, and its sufficiency is checked rather than
argued: `model/src/lib.rs` enumerates every interleaving of platform, client, and
in-VM attacker with stateright and holds `bootstrap is one-shot` and `only the
installed token is accepted` across all of them.

The identical-replay rule is where a naive implementation gets it backwards.
Answering 409 to a replay is safer in isolation and worse in practice: the platform
terminates the VM on a failed run hook before forwarding any traffic, so the failure
is invisible from outside and the VM is gone before you can look inside it. The cost
is a judgment call — replay-200 lets an attacker who somehow learns the harness's
token confirm it by replaying the hook. We accept that, because an attacker holding
the token already has the control API.

**Constant-time comparison, on bytes.** Both the bootstrap check and the per-request
authorization compare with `subtle`'s `ct_eq` over raw bytes, never over decoded
strings (`agentd/src/auth.rs`, `constant_time_eq` and `bearer_bytes`). The bytes part
is not fastidiousness. Python's `hmac.compare_digest` raises `TypeError` on `str`
inputs containing non-ASCII characters; the header is entirely attacker-controlled;
so `Authorization: Bearer tökén` killed the predecessor's handler thread and returned
`RemoteDisconnected` instead of a status the client could act on. That exact input is
now a unit test (`agentd/src/auth.rs`, `a_non_ascii_header_is_compared_not_crashed`)
and a live conformance check (`conformance/run.py`, "non-ASCII token header answered,
not a dropped connection").

Where it is *not* sufficient, stated rather than glossed: length inequality
short-circuits before the constant-time compare, so token length is observable. The
justification is that the length is fixed by whoever minted the token and is not
secret. If your tokens have variable length and their length is meaningful, that
reasoning does not transfer.

**Authorization before body.** The token guard runs as middleware before the request
body is polled, so a rejected request never causes the daemon to buffer
(`agentd/src/auth.rs`, `require_token`). The predecessor buffered first and checked
second, which let an unauthorized caller force a 256 MB allocation on a VM whose
baseline can be 512 MiB — and an OOM-killed daemon inside a MicroVM is unrecoverable,
because there is no supervisor, no SSH, and no console. A rejected request still
drains a bounded prefix of its body (64 KiB by default): leaving unread bytes in the
kernel buffer makes hyper close with a TCP RST, which a pooled client sees as a
transport error instead of the status you just chose. Draining without a cap would
itself be the denial-of-service you just prevented, so past the cap the status goes
out and the connection closes.

**The token is absent from child environments.** Every exec'd child starts from an
empty environment via `env_clear()`, and only the variables the request named are
added (`agentd/src/exec.rs`, `build_command`). Nothing on that path reads `std::env`.
The test proves it by running `/usr/bin/env` in a child and asserting empty output
(`the_agent_token_never_reaches_the_child_environment`). Privilege demotion, when
requested, goes through `Command::uid`/`gid` rather than a `pre_exec` closure, so the
uid change happens in C between fork and exec — a closure there runs in a forked child
of a threaded process, where an allocator lock another thread held at fork time is held
forever.

Two admissions. Demotion is opt-in per request, so the default child is root; and this
defense only matters when the child is *not* the token holder, which is true of a task
subprocess and false of the agent harness itself.

**Honest status codes, as a security property.** The three codes are chosen for what
they leak and how they mislead, not for tidiness. `503` means no token is installed
yet; `401` means a token is installed and yours is wrong; `404` means the route does
not exist. Collapsing 503 into 401 tells a client to go find better credentials when
the real answer is "wait". Collapsing either into 404 is worse, because clients map
404 onto "file not found" and a protocol error then surfaces as a phantom missing
artifact — which is exactly how one defect hid for a review round. `token_matches`
returns `None` for the unbootstrapped case specifically so the caller must decide
(`agentd/src/state.rs`), and the middleware is applied with axum's `route_layer` so an
unmatched path falls through to 404 rather than being answered 401
(`agentd/src/routes.rs`).

The leak this accepts: `GET /v1/health` is unauthenticated and reports
`bootstrapped`, so any in-VM process can detect the pre-bootstrap window precisely
rather than guess at it. That is a judgment call. It does not change the outcome —
one-shot bootstrap means the winner wins regardless of who is watching — but it
removes timing obscurity a different design could have kept. The alternative,
authenticating `/v1/health`, costs an orchestrator its only liveness probe during the
window it most needs one.

## The unenforced invariant

Name it plainly, because it is the load-bearing assumption and no code in the daemon
enforces it.

**The daemon must be the container `CMD`, and the harness must issue its first exec
only after readiness succeeds.** `ENTRYPOINT []` plus `CMD ["/agentd"]`, with no init
system and no other process started first. That is what makes "no in-VM workload runs
before bootstrap completes" true. It is also what makes an omitted `cwd` inherit the
image `WORKDIR`, and what makes identity repair sound.

What breaks it: a base image that starts a background process — an init system, a
preloaded agent, a D-Bus daemon — before the daemon binds its listener. Such a
process can send `/run` first, and one-shot bootstrap then works *for* it: it becomes
the installed token holder and the platform's real hook gets the 409.

The model reports both facts rather than only the reassuring one.
`Config::deployment_invariant_held` passes every safety property over the whole
reachable state space. `Config::deployment_invariant_broken` flips a single flag that
lets the attacker act before bootstrap, and the test asserts stateright *finds* the
counterexample and that its path contains an attacker `RunHook` (`model/src/lib.rs`,
`breaking_the_deployment_invariant_lets_the_attacker_in`). The "attacker never
authorized" property is stated unconditionally on purpose: a property that consults
the config it is meant to discriminate goes vacuous in the very run where it should
fail. One caveat on scope — the model checks the bootstrap and exec half of this
contract. It does not model identity repair, the fs routes, or anything below.

Enforcement belongs to whoever builds the image. It is an image-review property —
inspect the base image's entrypoint, its systemd units, and anything a package
install added — not something a daemon can check about itself.

## Identity repair for derived VMs

One image is snapshotted once and restored N times, so every byte in that snapshot is
identical across every VM, including the files whose only purpose is to be unique per
machine. The security consequence is concrete: `systemd-random-seed` credits
`/var/lib/systemd/random-seed` into the kernel pool at boot, so N VMs credit the same
seed, and a key generated in VM 7 can repeat a key generated in VM 3.

What is free. Each `RunMicrovm` is a Firecracker restore, which bumps VMGenID, and
Linux ≥ 5.18 reseeds the kernel CSPRNG from that notification (documented kernel
behavior). So `getrandom(2)` and `/dev/urandom` are already distinct per VM with no
help from you, and re-seeding from userspace would add nothing while needing
`RNDADDENTROPY`. What VMGenID does *not* touch is any identifier already committed to
a file.

What the caller owns, as a checklist (`agentd/src/identity.rs` implements all of it
and reports each step on `/v1/health`):

1. Mint one fresh 128-bit value from `/dev/urandom` and derive everything from it, so
   a VM's hostname and machine id agree in logs. Read a bounded buffer, not the whole
   file — `/dev/urandom` never reaches EOF.
2. Rewrite `/etc/machine-id` as 32 lowercase hex digits plus a newline. The file is
   0444 on a booted system, so unlink it first and restore the mode afterwards;
   opening it for write is EACCES even as root on some filesystems.
3. Set the hostname.
4. **Delete** `/var/lib/systemd/random-seed`. Do not rewrite it. A rewritten file is
   captured by the next snapshot and recreates the shared-seed problem one generation
   down; an absent file is unambiguous, since systemd's load step treats it as nothing
   to credit and writes a fresh one at shutdown from the already-reseeded pool.
5. Shadow `/proc/sys/kernel/random/boot_id` with a bind mount, formatted `8-4-4-4-12`
   because readers parse the dashes. It cannot be written: procfs refuses even for
   root, since the value is generated per boot and has no backing store.
6. Remove cached per-VM credentials that the snapshot captured. Only a configured
   list can be removed — `/var/lib/dbus/machine-id` by default — and a credential in
   a place nobody named survives.

The sharp edges, all of them in the implementation's own comments and none of them
fixable there. A bind mount needs `CAP_SYS_ADMIN` in the current mount namespace and
is refused outright in a container that did not ask for it. A bind mount is
namespace-local: a child in a fresh mount namespace, or an already-running process
holding an open fd on the original, still sees the snapshot value. Already-read
values cannot be recalled, which is why repair is only sound at all before any
workload starts — the `CMD` invariant again. And a daemon baked into the image that
cached a derived identifier in memory keeps it until it restarts.

Every failure is logged and then ignored, and the daemon serves on. That is
deliberate: a duplicate `machine-id` is a real security problem, and an unreachable
VM with work in it is a worse and unrecoverable one. The condition is surfaced as
`identity_degraded` on `/v1/health` so an orchestrator can drain the VM rather than
discovering the duplication from a repeated key months later. Opting out is supported
rather than a mistake — a fleet keyed by machine id wants stable identity, and
`identity_repaired: false` distinguishes that from a repair that found nothing to do.

The `CAP_SYS_ADMIN` failure mode this section previously listed as expected but
unmeasured has since been measured, and it was real. On 2026-08-06 in us-east-1 a live
run reported `identity_degraded: true`: writing `/etc/machine-id` succeeded, while
`sethostname` and the bind mount over `/proc/sys/kernel/random/boot_id` both returned
`EPERM` even though the daemon runs as root. The MicroVM drops `CAP_SYS_ADMIN` unless
the image is created with `additionalOsCapabilities: ["ALL"]`; with that set, all three
steps succeed and the same probe reports `identity_degraded: false`. Both facts are
recorded in `PLATFORM.md`, and the conformance suite now asserts
`identity_degraded == false`, so the capability requirement cannot be dropped silently.

Two lessons worth carrying rather than just the fix. A partial success is the dangerous
shape: the filesystem write succeeds, so repair looks like it works until you check the
two steps that need the kernel's permission rather than the filesystem's. And the unit
tests could not have caught this, because they inject a `Layout` inside a tempdir and a
fake platform — correct for testing the logic, structurally unable to observe a
capability the real VM lacks.

## What this contract does not cover

**Egress.** Nothing here constrains what the workload can reach. Egress is a
launch-time property of the MicroVM's network connectors, not something a guest daemon
can enforce against a root process; omitting `INTERNET_EGRESS` is how you get a VM with
no outbound network, and that call belongs to whoever calls `RunMicrovm`.

**Secret delivery beyond the run hook.** The `runHookPayload` string is the only
per-VM differentiator the platform offers, and `STRATEGY.md` records a 16 KB ceiling
on it (we have not measured that ceiling ourselves). One bearer token fits. A cloud
credential set, a fleet of per-task secrets, or anything needing rotation mid-session
does not, and this contract has nothing to say about how you get them in. That is
precisely the gap AgentCore's credential broker fills and that a raw-MicroVM customer
has to fill themselves.

**Anything requiring privileges the guest does not have.** The bind mount above is
the visible case. There is no seccomp confinement, no user-namespace isolation of the
workload from the daemon, and no attempt at either.

**Confining `/v1/fs/*` paths.** `PUT /v1/fs/file` writes wherever the caller asks and
`GET` reads wherever the caller asks. A reviewer pushed back on this and we documented
the reasoning rather than changing it (`agentd/src/fs.rs`, module docs): the same
bearer token authorizes `POST /v1/exec/start`, which runs arbitrary commands as root
by design, so a token holder can already reach every byte in the VM with one exec
call. A root prefix would add no security while breaking real behavior — harnesses
write credentials into home directories, drop config into `/etc`, stage scratch in
`/tmp`. If your control API does *not* grant arbitrary exec, this reasoning does not
transfer and you should confine those paths.

The one write path that *is* confined is `PUT /v1/fs/tar`, and the distinction is the
argument: there the member paths come out of an uploaded archive rather than from a
caller who named them, so an archive can carry a path its uploader never intended.
That gap is the entire traversal class, and the extraction rules that close it are in
`PROTOCOL.md`.
