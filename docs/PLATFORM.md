# AWS Lambda MicroVMs: measured platform behavior

Everything here is an observation of someone else's system, so every entry
carries the date, region, and API version it was measured under. Re-verify before
relying on any of it in a new region or after an API version bump. Where a claim
comes from documentation rather than our own run, it says so.

Measurement context unless stated otherwise: us-east-1, API version `2025-09-09`,
`al2023-minimal` aarch64 base image, measured 2026-08-01 through 2026-08-04
during the Harbor PR #2469 integration.

## The service provides no exec and no file transfer

There is no API to run a command in a MicroVM and no API to move a file into or
out of one. This absence is the entire reason this project exists.

`CreateMicrovmShellAuthToken` exists in the API and is not a substitute. It
requires a `SHELL_INGRESS` connector, the documented flow is
`ctr task exec -t --exec-id shell <id> /bin/sh` through a console terminal or
WebSocket, and the documentation scopes it to debugging while recommending it be
disabled in production. The name suggests a programmatic exec path that it is
not.

## Hooks are served under a fixed prefix, and two of them are build-time

Measured 2026-08-05. The platform calls
`POST /aws/lambda-microvms/runtime/v1/<hook>`, where `<hook>` is one of `ready`,
`validate`, `run`, `resume`, `suspend`, `terminate`. A daemon serving a bare
`/run` is never bootstrapped.

`ready` and `validate` are image-*build* hooks: the build calls them to decide
whether the snapshot it just produced is usable, before any instance exists and
therefore before any token has been delivered. They must answer 200 without
regard to bootstrap state. Gating them on a token fails the build rather than the
run, which is a confusing place to discover the mistake.

## `runHookPayload` arrives wrapped, not as the body

Measured 2026-08-05, and it cost a full build-and-run cycle to find. The string
passed to `RunMicrovm` as `runHookPayload` is not delivered as the request body.
The platform wraps it, so the body is:

```json
{"runHookPayload": "{\"agent_token\": \"...\"}"}
```

The caller's own JSON is one `serde_json`/`json.loads` deeper. A daemon that reads
its fields from the top level answers 400, and the platform then terminates the VM
with `Run lifecycle hook returned HTTP status 400. Please check your hook endpoint
and application logs for more details.` before forwarding any traffic — so the
failure is invisible from outside the VM, and the VM is gone before you can look
inside it. Read `GetMicrovm`'s `stateReason` first when a launch dies young.

## Network connectors are ARNs

Measured 2026-08-05. `ingressNetworkConnectors` takes
`arn:aws:lambda:<region>:aws:network-connector:aws-network-connector:ALL_INGRESS`,
not the bare string `ALL_INGRESS`, which is rejected with
`Malformed network connector ARN`. Egress uses the same shape with
`INTERNET_EGRESS`, and omitting egress entirely is how you get a VM with no
outbound network.

## `CreateMicrovmAuthToken` returns a header map

Measured 2026-08-05. The `authToken` field is a map of header name to value, not a
string: the API is shaped for schemes needing more than one header. Read
`authToken["X-aws-proxy-auth"]`. Requests also need `X-aws-proxy-port` naming
which of the token's allowed ports the request targets.

## MicroVM states, and the one that matters

`PENDING → RUNNING → SUSPENDING/SUSPENDED → TERMINATING → TERMINATED`. A VM that
reaches any terminal state *before* `RUNNING` died during startup, which for a
hook-serving daemon almost always means a lifecycle hook failed. Poll for
`RUNNING` and fail fast on the terminal states with `stateReason` attached;
polling through them wastes minutes and then reports a connection error that hides
the cause.

## The build log group survives Terraform

Measured 2026-08-05. The service creates `/aws/lambda-microvms/<image-name>`
itself, so a Terraform stack never owns it and `terraform destroy` leaves it
behind. It is storage-only cost, but "the stack destroyed cleanly" is not the same
as "the account is clean" — query for the log group separately, or delete it in
teardown.

## Root in the guest is not enough: `sethostname` and bind mounts need `additionalOsCapabilities`

Measured 2026-08-06, us-east-1, `al2023-1` base. The daemon runs as root inside
the MicroVM, and that is still not sufficient for anything requiring
`CAP_SYS_ADMIN`. With no `additionalOsCapabilities` on `CreateMicrovmImage`:

| Operation | Result |
| --- | --- |
| Write `/etc/machine-id` | succeeds |
| `sethostname` | `EPERM` (`Operation not permitted`, os error 1) |
| Bind mount over `/proc/sys/kernel/random/boot_id` | `EPERM` |

Passing `additionalOsCapabilities=["ALL"]` at image creation makes all three
succeed, confirmed by the same probe reporting `identity_degraded: false` where it
previously reported `true`.

Two things make this easy to miss. The filesystem write succeeds, so identity
repair looks like it works until you check the two steps that need the kernel's
permission rather than the filesystem's. And a daemon that logs the failure and
keeps serving — which is the right behavior, since refusing to serve would strand
the VM — produces a healthy-looking VM whose hostname and `boot_id` are shared
with every sibling from the same snapshot.

`ALL` is the only accepted value in the `2025-09-09` API; there is no way to
request `CAP_SYS_ADMIN` alone. A caller who does not need hostname or `boot_id`
repair should leave it unset rather than widen the guest for nothing.

This was found by a live run after the unit tests passed, because those tests
inject a fake layout and a fake platform. It is the clearest case in this project
of a guard that was verified in every tier except the one that mattered.

## `minimumMemoryInMiB` selects a *baseline*, and the guest reports the *peak*

Measured 2026-08-07, us-east-1, `al2023-1`. Requesting
`resources=[{"minimumMemoryInMiB": 512}]` produced a guest reporting
`MemTotal: 2037648 kB` (~2 GB). Requesting 2048 produced `MemTotal: 8209056 kB`
(~8 GB).

Both match AWS's documented sizing table exactly (`microvms-images.html`), which
pairs each baseline with a peak ceiling four times its size:

| Baseline (billed while running) | Peak (burst ceiling) |
| --- | --- |
| 0.5 GB / 0.25 vCPU | 2 GB / 1 vCPU |
| 1 GB / 0.5 vCPU | 4 GB / 2 vCPU |
| 2 GB / 1 vCPU (default) | 8 GB / 4 vCPU |
| 4 GB / 2 vCPU | 16 GB / 8 vCPU |
| 8 GB / 4 vCPU | 32 GB / 16 vCPU |

So `minimumMemoryInMiB` chooses a size class, and the number the guest reports in
`/proc/meminfo` is that class's **peak**, not its baseline. That the guest reports
the peak specifically is our inference from two matching measurements; AWS
documents the table but not the `MemTotal` mapping.

**Billing follows the baseline you requested, not the peak the guest reports.**
AWS: "You pay the baseline rate while your MicroVM is running and only pay for what
you actively use above the baseline, billed per second." An earlier version of this
section said a caller "should not assume they are billed for the request", which was
exactly backwards — the request is what you are billed for, and the extra memory is
burst headroom charged only for the seconds it is actually consumed. Corrected
2026-08-07 after reading the pricing page rather than inferring from the size.

Consequences for a caller. You cannot use this field to *constrain* a VM, so a
memory-pressure test must generate pressure against what the guest reports rather
than what was requested. Guest swap is absent (`SwapTotal: 0 kB`), so pressure goes
straight to the OOM killer with no paging phase. And picking a small baseline is a
real cost lever rather than a cosmetic one, since baseline is the rate you pay for
every running second.

## What actually costs money

Documented 2026-08-07 from the Lambda pricing page and the MicroVMs developer
guide. Rates are us-east-1. MicroVMs has no standalone pricing page: the rates
appear only inside worked examples on the Lambda pricing page, which is why they are
easy to miss.

| Line item | Rate |
| --- | --- |
| vCPU | $0.0000276944 per vCPU-second |
| Memory | $0.0000036667 per GB-second |
| Snapshot storage | $0.08 per GB-month, **1-week minimum retention** |
| Snapshot read (launch or resume) | $0.00155 per GB |
| Snapshot write (suspend) | $0.0038 per GB |
| Data transfer | standard AWS rates, including MicroVM to your own VPC |

vCPU and memory bill as two separate line items rather than one blended
GB-second, and there is no per-request charge — instances bill per second. No
MicroVMs free tier is published; the Lambda free tier is Functions-only. No
minimum billing increment is published.

Three things worth knowing before running a create-and-destroy test suite like this
repo's:

**Image storage has a one-week minimum.** A 2 GB image deleted sixty seconds after
creation still bills about a week of storage, roughly four cents. Our conformance
suite builds a fresh image per run, so "it costs pennies" is right per run but the
floor is the image rather than the compute.

**Idle time while RUNNING is billed at baseline.** This differs from AgentCore
Runtime, which charges no CPU during I/O wait. On raw MicroVMs, wall-clock time in
`RUNNING` costs baseline whether or not anything is executing, so suspension is the
only way to stop paying.

**Suspension is the cheap state, and churn is the cost.** A suspended 2 GB VM pays
only snapshot storage, about $0.16 a month, against roughly $100 a month to leave
the same VM running at baseline — a difference of two orders of magnitude, which is
what makes a warm suspended pool viable. But each cycle pays a write plus a read,
about $0.011 for a 2 GB VM, so the thing to avoid is suspending and resuming
constantly rather than suspending for a long time.

**Not published:** whether the server-side image build is billed as compute. The
build starts a real MicroVM to run the Dockerfile, so it plausibly is, but AWS does
not say and we have not measured it. Do not assume either way.

## Seeing an OOM: the process case works, the VM case is still unmeasured

Measured 2026-08-07, us-east-1, via `conformance/probe_oom.py`. The customer
question is "is there a `dmesg`?" and it splits in two.

**A process killed inside a living VM should be visible twice over, and the
plumbing for it is confirmed present.** What was actually measured: `dmesg` runs in
the guest and is readable with no extra privileges (it returned successfully, empty,
because no OOM occurred), and `/sys/fs/cgroup/memory.events` exists and exposes
`oom`, `oom_kill`, and `oom_group_kill` counters — all reading 0 on an unpressured
VM. Those counters are the right thing for a supervisor to poll rather than
discovering a kill after the fact.

The daemon reports a killing signal on the exec result, so a caller would see
SIGKILL rather than an exit code; that path is covered by unit tests but has not
been exercised by a real OOM. Treat "you will see signal 9 and a dmesg line" as
sound reasoning from confirmed plumbing rather than as an observation.

**Whether a guest-wide OOM populates `stateReason` remains unmeasured, because we
could not make one happen.** Two attempts failed for reasons worth recording rather
than hiding:

1. The first probe allocated with `python3`, which the `amazonlinux:2023-minimal`
   base image does not have. It reported `command not found` with exit code 127 and
   every downstream check passed — the probe measured nothing while looking like a
   clean result. This is the failure mode this project keeps hitting: a green run
   that never exercised the thing.
2. The second allocated with `dd` into `/dev/shm`, which is tmpfs and therefore
   capped near half of RAM. `dd` stopped at 64 MiB against a 1 GiB request and
   exited 0, so again no memory pressure. tmpfs limits are a filesystem ceiling,
   not a memory one.

So `stateReason` was `null` and the state `RUNNING` throughout, which says only
that we never applied real pressure. A future probe needs an allocator that touches
anonymous memory the kernel must back and cannot silently cap — a small static
binary shipped in the image is the obvious answer, since the guest has no
interpreter and no compiler.

What the runs *did* establish: the daemon survived 64 MiB of output under
concurrent allocation with `truncated: true` on the result, so the output cap holds
under pressure, and `/v1/health` stayed reachable and bootstrapped throughout.

## Suspend/resume is a freeze and restore, not a stop and start

Measured 2026-08-05, us-east-1, `al2023-1` base, 1024 MiB baseline, via
`conformance/probe_suspend_resume.py`. `SuspendMicrovm`, held 45 seconds, then
`ResumeMicrovm`. Everything survived:

| What | Result |
| --- | --- |
| In-memory agent token | survived — `/v1/health` reports `bootstrapped: true` |
| Filesystem | survived |
| Exec records, including unacked output | survived |
| A backgrounded process | survived and kept running after resume |
| Endpoint URL | unchanged |

The decisive evidence is a ticker writing `date +%s` once a second: the largest gap
between consecutive ticks was 51 seconds, matching the suspension plus transition
time, and the tick file grew by 6 lines over 6 seconds after resume. So the guest
is frozen rather than killed, and it resumes mid-stride.

Two consequences. Pause/resume needs no token re-delivery and no re-bootstrap,
which makes a warm suspended sandbox pool viable: suspend an idle VM instead of
terminating it and the next task lands in a VM that still has its filesystem, its
installed tools, and its credentials. And a guest process that measures wall time
sees the suspension as a single jump — anything holding a timeout, a lease, or a
TLS session across a suspend will observe it expire at once.

This corrects an earlier claim in the daemon's own resume-hook docstring, which
asserted that bootstrap state being in memory made a resumed VM unable to serve the
control API. That was reasoning from where the state lives rather than from a
measurement, and it was wrong.

## Traffic ordering around the `/run` hook

Documented (`microvms-launching.html`): "Your MicroVM begins receiving external
traffic after the `/run` hook returns HTTP 200. Until then, the endpoint does not
forward requests to your application."

This is what makes it safe to deliver a per-VM secret through `runHookPayload` at
launch instead of baking it into a shared snapshot. It closes the first-writer
race *through the endpoint*. It says nothing about processes already running
inside the VM, which is the subject of the next entry.

## The platform's own hook arrives over loopback

Measured 2026-08-04, us-east-1, by instrumenting the daemon to log
`client_address` on every request and reading the result from CloudWatch:

```
PROBE hook=run            client_address=('127.0.0.1', 36932)   headers={... 'host': 'localhost:9000'}
PROBE control=/exec/start client_address=('127.0.0.1', 36926)
```

The endpoint proxy terminates outside the VM and forwards over loopback. Both the
platform's lifecycle hooks and the harness's control requests arrive from
`127.0.0.1`.

Consequence, and it inverts the intuition: a source-address rule that rejects
loopback callers on the bootstrap route is not a weak control resting on an
unverified assumption. It is actively wrong. It would reject the platform's own
legitimate bootstrap and break every launch. Do not implement it. An earlier
attempt broke 39 tests, and those failures were reporting a real defect rather
than a harness artifact.

Because in-VM traffic is indistinguishable from platform traffic at the socket
level, the one-shot bootstrap is the only available defense on that route. Its
sufficiency is checked in `model/` rather than argued in prose.

## Something probes the port with TLS before bootstrap

Measured in the same 2026-08-04 run. The daemon receives raw TLS handshake bytes
on its plaintext port:

```
code 400, message Bad request version ("\x13\x01\x13\x02...")
```

That is a TLS ClientHello reaching a plaintext HTTP server. Something in the
platform's path probes the port with TLS first. It is harmless, the correct
response is a 400 and a debug-level log, and it must not take the listener down.
It looks like an attack in logs, which is why it is documented here.

## Endpoint authentication

Documented (`microvms-networking.html`): every request to a MicroVM endpoint
requires an `X-aws-proxy-auth` JWE scoped to a specific MicroVM ID, a specific
port set, and an expiry of at most 60 minutes, minted by
`create-microvm-auth-token`.

There is no unauthenticated internet path to the daemon's port. Port scoping is
the useful part: a token minted for port 9000 cannot reach port 8080, so a task
workload and a control plane can share a VM with access handed out to only one.

Operational consequence for clients: the 60-minute ceiling means a long-running
trial will mint a fresh token mid-flight, so token minting sits inside the retry
path and boto/HTTP errors from minting must be handled wherever a request can be
retried.

## `clientToken` is a permanent idempotency key

Measured 2026-08-02, us-east-1, the expensive way. A `clientToken` derived from a
stable resource identity replays forever: after an image is deleted and recreated
under the same name, the service replays the original create as a no-op. The
image sits in `CREATING` with its builds never scheduled
(`list-microvm-image-builds` shows every build `PENDING` with `updatedAt` never
advancing past `createdAt`).

An image in `CREATING` cannot be deleted, and its only version cannot be deleted
because it is the last one. Two images were wedged this way for roughly 15 hours
before the service timed them out.

Client guidance: scope a create token to a single build attempt (fold in a
per-instance random value, not only content-derived digests), and detect the
stalled state by probing `list-microvm-image-builds` after a grace period rather
than burning the full build timeout in silence.

## Build logs go to `/aws/lambda-microvms/<image-name>`

Measured 2026-08-05. Not `/aws/lambda/microvms/*`. An IAM policy granting the
wrong prefix produces server-side builds with no logs at all, and every failure
then reports `reason=unknown` — which reads as the service failing to populate
`stateReason` when it is really the caller's own policy discarding the logs.

Build roles also need ECR permissions if any task points `docker_image` at a
same-account ECR repository; without them the build fails outright.

## `idlePolicy`

Documented, and confirmed useful in practice. Idle time is measured by inbound
traffic through the proxy, so an abandoned VM auto-suspends and then terminates
rather than billing to the 8-hour `maximumDurationInSeconds` ceiling.

Sharp edge for clients that suspend deliberately to preserve state: the
launch-time `idlePolicy` terminates a suspended VM after
`suspended_timeout_sec`, so a "resume later" affordance silently stops working
once that window passes. State the window wherever a resume path is offered.

## Most public ARM64 base images have no WORKDIR

Measured 2026-08-05. `al2023-minimal`, `python:3.12-slim`, and `node:20-slim` all
leave `WorkingDir` empty. Anything that tests WORKDIR inheritance needs a purpose
-built image with `WORKDIR` set, since there is nothing to inherit otherwise.
