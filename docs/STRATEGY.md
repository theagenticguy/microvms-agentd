# Strategy: the trust and preparation contract for coding agents on microVMs

Audience: AWS customers assembling coding-agent products on raw Lambda MicroVMs,
and the open-source ecosystem around them. The deliverable is a reference
implementation plus measured platform knowledge, so each team does not rediscover
the same traps. It is not a service or a business.

Research window 2025-08 through 2026-08-06. Numbers are labeled measured,
documented, vendor-claimed, or inferred. Our own measurements are dated and
reproducible from `conformance/`.

This is the second draft. The first one led with a turn-boundary suspend protocol
as the highest-leverage action. An adversarial review showed that was wrong, and
the record of why is at the end because the reasoning is more useful than the
conclusion.

## Diagnosis

**On raw Lambda MicroVMs, an untrusted model-driven process shares a loopback
namespace with the control plane that governs it, and nobody has written down the
contract for that. Meanwhile the measured cost sink is environment preparation. The
fork everyone is racing toward cannot be built above the hypervisor.**

Three facts follow, in descending order of how much they should change what you
build.

**The trust boundary is unowned and consequential.** We measured (2026-08-04,
us-east-1) that the platform's own lifecycle hook arrives from `127.0.0.1`,
indistinguishable at the socket level from any process inside the VM. Source-address
filtering on that route is therefore wrong, because it rejects the platform's own
bootstrap. The only available defense on that route is that bootstrap can succeed
exactly once. The endpoint's 60-minute JWE ceiling puts token minting inside every
retry path. Taken together, these facts mean that on this substrate a coding agent
lives inside the same network namespace as the API that controls its sandbox. A
coding agent is an untrusted, model-driven process that runs arbitrary code by
design. AgentCore solves this with an external credential broker, but a customer on
raw MicroVMs has no broker, and the research named credential handling the single
most reported operational failure in agent sandboxes. Nobody owns this contract, we
have already measured it, and the first draft demoted it to a sub-clause.

**Preparation is the cost, and execution is not.** Environment init is 31–48% of
task lifecycle even with a prebaked image (measured, AgentCgroup over 144
SWE-rebench tasks). The median image is 3.5 GB and ranges to 17.3 GB. Blacksmith
measured persistent builder state beating export-based caches by 10–30x. AgentCore
caps session storage at 1 GB, which a mid-size JavaScript monorepo exceeds on
`node_modules` alone. The cost comes from moving bytes rather than from who can run
a shell.

**Process-tree fork is unavailable to everyone above the hypervisor.** BranchFS
gives file-level copy-on-write branching in-guest over FUSE with no root, at
sub-350µs per branch independent of base size (documented, arXiv 2602.08199). AWS
permits FUSE inside a MicroVM. Memory branching, however, is a *proposed* `branch()`
syscall and is not in the mainline kernel. CRIU unprivileged cannot dump shared
memory, cannot reach `/proc/pid/map_files` from a container user namespace, and
cannot touch a seccomp-filtered process (documented limits). Modal, which controls
its own hosts, still documents that background processes launched via exec "will not
be properly restored." If the provider cannot restore them, a guest-side daemon
cannot either.

## Guiding policy

Own the contracts that are specific to running an *untrusted* workload on a *raw*
microVM substrate, and prove each one against the real service. Decline everything
that the platform, the harness, or an existing product already does.

The second half is the discipline, and the first draft failed it. Before anything
becomes an action, it must pass three tests. Does the platform already do this?
Does a harness we don't control have to cooperate for it to work? Is the benefit
reachable without our involvement? If the answer to any of those is yes, the item
should be published as an example rather than pursued as a strategy.

## Coherent actions

**1. The trust and identity contract, as executable specification.** This action
has two halves. The first half is the in-VM boundary: what a daemon on this
substrate must guarantee when the workload is assumed hostile. The guarantees are
one-shot bootstrap, constant-time comparison on bytes, authorization decided before
a body is buffered, the agent token never entering an exec'd child's environment,
and a documented statement of what remains unenforced. All of that is implemented
and live-verified in `microvms-agentd` today. What is missing is the writeup that
makes it adoptable by someone building their own. The second half is identity
repair for derived VMs. Entropy is partly free because each `RunMicrovm` is a
Firecracker restore, so VMGenID bumps and Linux ≥5.18 reseeds the kernel CSPRNG
(documented). The caller still owns userspace PRNG pools, `/etc/machine-id`,
hostname, `/proc/sys/kernel/random/boot_id` (read-only, needs a bind mount),
`/var/lib/systemd/random-seed`, and any cached credential or lease. The 4096-byte
`runHookPayload` is the only per-VM differentiator the platform offers, so repair
belongs in the run hook. An earlier version of this memo called that ceiling 16 KB.
The measured value is 4096 bytes (2026-08-07, us-east-1, recorded in
`PLATFORM.md`). The corrected number supports the same placement. 4096 bytes is
ample for the one 128-bit seed this list needs, and far too small to be a general
secret channel, so the run hook can carry per-VM identity but cannot carry a
credential set. Getting this wrong produces VM-generated keys that repeat across
sandboxes, which is a security bug rather than a performance regression.

**2. Content-addressed environment layers, fingerprinted by lockfile.** This
action attacks the 31–48% init cost. Dependencies belong in the image, where they
are shared by construction. Only working-tree divergence should be a per-sandbox
payload. No current product exposes `node_modules` or a venv as a branchable layer
with correct invalidation. AgentCore's EFS mount is a shared directory with no
branching semantics, and Blacksmith concedes nobody offers visibility inside a
mount cache. Pair the baked layer with in-guest BranchFS for the working tree,
which is where microsecond branching genuinely applies. Ship the measured
comparison, including the cost of the best available fork approximation.

**3. Publish, then measure adoption.** The publishing half is done, and the
measuring half is not, which is why the action stays on this list. The repository
now has a remote and CI runs on every push. Lint, the test tiers on three
platforms, the security and SBOM gates, the bindings, and the `aarch64-musl`
cross-compile are all green. The measuring has not happened. The live conformance
suite, along with the platform defects that no local test tier could have caught,
is now reachable by the audience this memo names. Nobody has yet checked whether
that audience arrived, and coverage is still one region. This action remains the
precondition for the other two: the cheap half is spent, and the question it was
asked to answer is still open.

## What we are deliberately not doing

**Not a turn-boundary suspend protocol.** `idlePolicy` already auto-suspends on
inbound-traffic idleness (documented, and in `PLATFORM.md`), and Claude Code already
ships `Stop` and `SessionEnd` hooks. The gap is that nobody has wired an existing
hook to a suspend call, rather than a missing convention. The same benefit also
needs no harness cooperation, because an orchestrator can infer quiescence from the
daemon's own exec state plus request idleness. That makes it a 30-line example,
which we should publish, and not an action.

**Not fork.** The process-tree half is unavailable, and the best guest-side
approximation costs materially more than a real copy-on-write fork in both
time-to-ready and memory. We should publish the measurement rather than the
estimate. The first draft asserted a 5–20x multiple with no source, which is the
kind of unsourced claim this project exists to avoid.

**Not AgentCore parity on exec and PTY.** AgentCore already holds this ground, we
cannot win it, and winning it would not serve the audience.

**Not an orchestrator.**

## The AWS ask, and the honest bet

The ask is that AWS expose clone or snapshot-to-image on Lambda MicroVMs, meaning
the ability to capture a running VM's full state as a reusable launch source.
Suspend/resume shows the machinery already exists internally. Restoring a frozen
guest with its processes intact is the hard part, and it demonstrably works.

The first draft did not state the inverse case, so this draft does. AWS shipped
exec in March, session storage in April, and a PTY shell in June. That is a team
executing fast on this exact workload. If they ship clone unprompted, action 2
loses much of its edge, and the platform documentation decays as customers move up
to the managed service. Action 1 gains value in that world, because a hostile
workload sharing a namespace with its control plane is a property of the substrate
rather than of the feature set. Because action 1 holds its value under both
outcomes and action 2 does not, action 1 comes first.

## How we would know this worked

Three outcomes would show this worked. Someone outside this repo cites the
identity-repair list before shipping rather than after an incident. A second
implementation of the trust contract appears that we did not write. And
`docs/PLATFORM.md` is wrong less often than AWS's own documentation, measured
rather than assumed.

## What the first draft got wrong

This section is recorded because the failure is instructive. The first draft led
with turn-boundary suspend and called it the highest-leverage gap. It had three
defects, all found by an adversarial pass rather than by us.

First, it contradicted itself. The same paragraph credited AgentCore with free
idle CPU and then called idle billing an industry bottleneck. Second, it proposed
a protocol the platform and the harnesses already made unnecessary. Third, it
mixed two customer segments while citing one number for both. The 243-second
inter-turn idle is human think time from IDE-attached Copilot telemetry. The
31–48% init figure comes from batch evaluation. Headless agents have no inter-turn
gap to reclaim at all. We had Claude Code and Codex traces in hand and used them
only for tool-call duration, not for the idle distribution the action rested on.

The generalizable lesson is that a diagnosis concluding the world needs the thing
you just built deserves an adversarial pass before it deserves a memo.
