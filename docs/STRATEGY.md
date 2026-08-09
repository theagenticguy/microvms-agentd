# Strategy: the trust and preparation contract for coding agents on microVMs

Audience: AWS customers assembling coding-agent products on raw Lambda MicroVMs,
and the open-source ecosystem around them. Not a service, not a business. The
deliverable is a reference implementation plus measured platform knowledge, so each
team does not rediscover the same traps.

Research window 2025-08 through 2026-08-06. Numbers are labeled measured,
documented, vendor-claimed, or inferred. Our own measurements are dated and
reproducible from `conformance/`.

This is the second draft. The first one led with a turn-boundary suspend protocol
as the highest-leverage action; an adversarial review killed it, correctly, and the
record of why is at the end because the reasoning is more useful than the
conclusion.

## Diagnosis

**On raw Lambda MicroVMs, an untrusted model-driven process shares a loopback
namespace with the control plane that governs it, and nobody has written down the
contract for that. Meanwhile the measured cost sink is environment preparation, and
the fork everyone is racing toward cannot be built above the hypervisor.**

Three facts, in descending order of how much they should change what you build.

**The trust boundary is unowned and consequential.** We measured (2026-08-04,
us-east-1) that the platform's own lifecycle hook arrives from `127.0.0.1`,
indistinguishable at the socket level from any process inside the VM. Source-address
filtering is therefore not merely weak, it is wrong — it rejects the platform's own
bootstrap. The only available defense on that route is that bootstrap can succeed
exactly once. Add the endpoint's 60-minute JWE ceiling, which puts token minting
inside every retry path, and the shape is clear: on this substrate a coding agent —
an untrusted, model-driven process that runs arbitrary code by design — lives inside
the same network namespace as the API that controls its sandbox. AgentCore solves
this with an external credential broker, but a customer on raw MicroVMs has no
broker, and the research named credential handling the single most reported
operational failure in agent sandboxes. This is the highest-consequence contract
nobody owns, we have already measured it, and the first draft demoted it to a
sub-clause.

**Preparation, not execution, is the cost.** Environment init is 31–48% of task
lifecycle even with a prebaked image (measured, AgentCgroup over 144 SWE-rebench
tasks), median image 3.5 GB ranging to 17.3 GB, and Blacksmith measured persistent
builder state beating export-based caches by 10–30x. AgentCore caps session storage
at 1 GB, which a mid-size JavaScript monorepo exceeds on `node_modules` alone. The
asymmetry is real and it is about bytes, not about who can run a shell.

**Process-tree fork is unavailable to everyone above the hypervisor.** BranchFS
gives file-level copy-on-write branching in-guest over FUSE with no root, at
sub-350µs per branch independent of base size (documented, arXiv 2602.08199), and
AWS permits FUSE inside a MicroVM. But memory branching is a *proposed* `branch()`
syscall, not mainline. CRIU unprivileged cannot dump shared memory, cannot reach
`/proc/pid/map_files` from a container user namespace, and cannot touch a
seccomp-filtered process (documented limits). The decisive signal: Modal, which
controls its own hosts, still documents that background processes launched via exec
"will not be properly restored." If the provider cannot, a guest-side daemon cannot.

## Guiding policy

Own the contracts that are specific to running an *untrusted* workload on a *raw*
microVM substrate, and prove each one against the real service. Decline everything
that the platform, the harness, or an existing product already does.

The second half is the discipline, and the first draft failed it. Three tests before
anything becomes an action. Does the platform already do this? Does a harness we
don't control have to cooperate for it to work? Is the benefit reachable without
our involvement? A yes to any of those means it is an example to publish, not a
strategy to pursue.

## Coherent actions

**1. The trust and identity contract, as executable specification.** Two halves of
one problem. First, the in-VM boundary: what a daemon on this substrate must
guarantee when the workload is hostile-by-assumption — one-shot bootstrap,
constant-time comparison on bytes, authorization decided before a body is buffered,
the agent token never entering an exec'd child's environment, and a documented
statement of what remains unenforced. All of that is implemented and live-verified
in `microvms-agentd` today; what is missing is the writeup that makes it adoptable
by someone building their own. Second, identity repair for derived VMs: entropy is
partly free because each `RunMicrovm` is a Firecracker restore, so VMGenID bumps and
Linux ≥5.18 reseeds the kernel CSPRNG (documented), but the caller still owns
userspace PRNG pools, `/etc/machine-id`, hostname, `/proc/sys/kernel/random/boot_id`
(read-only, needs a bind mount), `/var/lib/systemd/random-seed`, and any cached
credential or lease. The 4096-byte `runHookPayload` is the only per-VM differentiator
the platform offers, so repair belongs in the run hook. An earlier version of this
memo called that ceiling 16 KB; it is 4096 bytes, measured 2026-08-07 in us-east-1 and
recorded in `PLATFORM.md`. The correction strengthens the argument rather than weakening
it: 4096 bytes is ample for the one 128-bit seed this list needs, and far too small to
be a general secret channel, so the run hook is where per-VM identity belongs and is
not where a credential set can live. Getting this wrong produces VM-generated keys that
repeat across sandboxes, which is a security bug rather than a performance regression.

**2. Content-addressed environment layers, fingerprinted by lockfile.** Attack the
31–48%. Dependencies belong in the image, shared by construction; only working-tree
divergence should be a per-sandbox payload. Nobody exposes `node_modules` or a venv
as a branchable layer with correct invalidation — AgentCore's EFS mount is a shared
directory with no branching semantics, and Blacksmith concedes nobody offers
visibility inside a mount cache. Pair the baked layer with in-guest BranchFS for the
working tree, which is where microsecond branching genuinely applies. Ship the
measured comparison, including the honest cost of the best available fork
approximation.

**3. Publish, then measure adoption.** *Done, in part — kept here because the second
half is not.* The repository now has a remote and CI runs on every push: lint, the
test tiers on three platforms, the security and SBOM gates, the bindings, and the
`aarch64-musl` cross-compile are all green. What has not happened is the measuring.
The genuinely novel asset — the live conformance suite and the platform defects no
local test tier could have caught — is now reachable by the audience this memo names,
and nobody has yet checked whether that audience arrived. Still one region. So this
remains the precondition for the other two, with the cheap half spent and the
question it was asked to answer still open.

## What we are deliberately not doing

**Not a turn-boundary suspend protocol.** `idlePolicy` already auto-suspends on
inbound-traffic idleness (documented, and in `PLATFORM.md`), and Claude Code already
ships `Stop` and `SessionEnd` hooks. So the gap was never a missing convention, only
that nobody wired an existing hook to a suspend call. Furthermore the same benefit
needs zero harness cooperation: an orchestrator can infer quiescence from the
daemon's own exec state plus request idleness. That makes it a 30-line example,
which we should publish, and not an action.

**Not fork.** The process-tree half is unavailable, and the best guest-side
approximation costs materially more than a real copy-on-write fork in both
time-to-ready and memory. We should publish the measurement rather than the
estimate; the first draft asserted a 5–20x multiple with no source, which is exactly
the failure mode this project exists to avoid.

**Not AgentCore parity on exec and PTY.** Taken ground, unwinnable, pointless.

**Not an orchestrator.**

## The AWS ask, and the honest bet

Expose clone or snapshot-to-image on Lambda MicroVMs: capture a running VM's full
state as a reusable launch source. Suspend/resume proves the machinery exists
internally, since restoring a frozen guest with its processes intact is the hard
part and it demonstrably works.

The inverse deserves stating plainly, because the first draft dodged it. AWS shipped
exec in March, session storage in April, and a PTY shell in June: that is a team
executing fast on this exact workload. If they ship clone unprompted, action 2 loses
much of its edge and the platform documentation decays as customers move up to the
managed service. Action 1 gets *more* valuable in that world, not less, because a
hostile workload sharing a namespace with its control plane is a property of the
substrate rather than of the feature set. That asymmetry is the reason action 1 is
first.

## How we would know this worked

Someone outside this repo citing the identity-repair list before shipping rather
than after an incident. A second implementation of the trust contract that we did
not write. And `docs/PLATFORM.md` being wrong less often than AWS's own
documentation, measured honestly rather than assumed.

## What the first draft got wrong

Recorded because the failure is instructive. The first draft led with turn-boundary
suspend and called it the highest-leverage gap. Three defects, all found by an
adversarial pass rather than by us.

It contradicted itself: the same paragraph credited AgentCore with free idle CPU
and then called idle billing an industry bottleneck. It proposed a protocol the
platform and the harnesses already made unnecessary. And it mixed two customer
segments while citing one number for both — 243-second inter-turn idle is human
think time from IDE-attached Copilot telemetry, while 31–48% init is batch
evaluation, and headless agents have no inter-turn gap to reclaim at all. We had
Claude Code and Codex traces in hand and used them only for tool-call duration, not
for the idle distribution the action rested on.

The generalizable lesson: a diagnosis that concludes the world needs the thing you
just built deserves an adversarial pass before it deserves a memo.
