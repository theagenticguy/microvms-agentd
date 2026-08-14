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
out of one. This project exists to provide those two operations.

`CreateMicrovmShellAuthToken` exists in the API, but it is an interactive
debugging tool rather than a substitute. It requires a `SHELL_INGRESS`
connector. The documented flow is
`ctr task exec -t --exec-id shell <id> /bin/sh` through a console terminal or
WebSocket. The documentation scopes it to debugging and recommends disabling it
in production. Despite what the name suggests, it does not provide a
programmatic exec path.

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

Measured 2026-08-05. Finding this cost a full build-and-run cycle. The platform
wraps the string passed to `RunMicrovm` as `runHookPayload` inside an outer JSON
object rather than delivering it as the request body, so the body is:

```json
{"runHookPayload": "{\"agent_token\": \"...\"}"}
```

The caller's own JSON is one `serde_json`/`json.loads` deeper. A daemon that reads
its fields from the top level answers 400, and the platform then terminates the VM
with `Run lifecycle hook returned HTTP status 400. Please check your hook endpoint
and application logs for more details.` before forwarding any traffic. Because no
traffic was ever forwarded, the failure is invisible from outside the VM, and the
VM is gone before you can look inside it. Read `GetMicrovm`'s `stateReason` first
when a launch dies young.

## The `runHookPayload` ceiling is 4096 bytes, and the service model states it

Measured 2026-08-07, us-east-1, API version `2025-09-09`. The real ceiling is 4096
bytes. `STRATEGY.md` asserted a 16 KB `runHookPayload` and `TRUST.md` repeated it
while flagging it as unmeasured, so the documented figure was four times the real
one. Because the error overstated the limit, a reader planning from it would try to
fit four times the secret material that actually fits. Both files were corrected
2026-08-07.

The boundary was bracketed from both sides by calling `RunMicrovm` with a deliberately
bogus `imageIdentifier`, so nothing could be created and nothing was billed:

| Payload length | Result |
| --- | --- |
| 4096 bytes | passes the length check, fails only on the bogus ARN (`Malformed ARN - doesn't start with 'arn:'`) |
| 4097 bytes | `1 validation error detected: Value at 'runHookPayload' failed to satisfy constraint: Member must have length less than or equal to 4096` |

So 4096 is inclusive. The bogus-ARN technique generalizes to other request-validation
boundaries. To probe one without creating a billable resource, make one other field
invalid in a way that fails later than the constraint under test, then read which
error comes back.

botocore does not enforce this client-side. The oversized request goes to the wire and
the server rejects it, so a caller building a payload gets no local signal that it is too
large. A length check before the call is worth having.

The figure was also available without measuring. The botocore service model for
`lambda-microvms` version `2025-09-09` declares
`RunMicrovmRequestRunHookPayloadString` with `max: 4096`. That model is a
machine-readable statement of the service's constraints. This project had been
restating those constraints in prose by hand, and one of them was wrong by 4x. The
model states other useful constraints, none of them measured by us:

| Constraint | Value |
| --- | --- |
| `Architecture` enum | exactly `['ARM_64']`, so a MicroVM cannot be x86 |
| `Capability` enum | exactly `['ALL']` |
| `run`, `resume`, `suspend`, `terminate` hook timeouts | max 60 seconds |
| `ready` and `validate` image-build hook timeouts | max 3600 seconds, 60x the run-time hooks |
| `maximumDurationInSeconds` | max 28800 (8 hours) |
| `ImageName` | max 64 chars, pattern `[a-zA-Z0-9-_]+` |

The 60x gap between the two hook families follows from what they are for. A build hook
waits on a Dockerfile, while a run hook waits on a daemon that is already booted. A
daemon that takes more than 60 seconds to answer `/run` fails the launch, and there is
no way to ask for more time.

A drift checker is being added at `scripts/check-model-drift` and wired into
`mise run check`. With it in place, a documented constraint that no longer matches
the shipped model fails the check mechanically instead of waiting for someone to
notice the prose.

## Calling an unpriced region returns `AccessDeniedException` with a null message

Measured 2026-08-07 by calling `ListMicrovms` in eight regions. The five regions that
price MicroVMs all answered successfully: us-east-1, us-east-2, us-west-2, eu-west-1,
ap-northeast-1. eu-central-1, ap-southeast-2, and sa-east-1 each returned
`AccessDeniedException` with the message field `None`.

That response is indistinguishable from a genuine IAM denial, so someone who typos a
region ends up auditing a policy that is fine. The null message is the way to tell
them apart, because a real denial names the principal and the action.

Nothing earlier in the call path catches it either. `boto3.client("lambda-microvms",
region_name=...)` constructs successfully for any region and resolves to
`https://lambda.<region>.amazonaws.com`, because the service model's `endpointPrefix` is
`lambda`. So the first API call is the only thing that reports the problem, and it
reports the wrong cause.

Two resolver calls disagree with each other, so both results are recorded here.
`endpoint_resolver.get_available_endpoints("lambda-microvms")` returns an empty list.
`session.get_available_regions("lambda-microvms")` returns 34 regions, the full
Lambda set, since resolution keys off the shared `lambda` prefix. Neither answer is the
five-region truth, so do not use either as a support check. Keep the supported list
explicitly and validate the caller's region against it before the first call.

## Network connectors are ARNs

Measured 2026-08-05. `ingressNetworkConnectors` takes
`arn:aws:lambda:<region>:aws:network-connector:aws-network-connector:ALL_INGRESS`,
not the bare string `ALL_INGRESS`, which is rejected with
`Malformed network connector ARN`. Egress uses the same shape with
`INTERNET_EGRESS`, and omitting egress entirely is how you get a VM with no
outbound network.

## `CreateMicrovmAuthToken` returns a header map

Measured 2026-08-05. The `authToken` field is a map of header name to value, not a
string. The API is shaped this way to allow schemes that need more than one header.
Read `authToken["X-aws-proxy-auth"]`. Requests also need `X-aws-proxy-port` naming
which of the token's allowed ports the request targets.

## MicroVM states, and terminal states reached before `RUNNING`

`PENDING → RUNNING → SUSPENDING/SUSPENDED → TERMINATING → TERMINATED`. A VM that
reaches any terminal state *before* `RUNNING` died during startup, which for a
hook-serving daemon almost always means a lifecycle hook failed. Poll for
`RUNNING` and fail fast on the terminal states with `stateReason` attached.
Polling through them wastes minutes and then reports a connection error that hides
the cause.

## The build log group survives Terraform

Measured 2026-08-05. The service creates `/aws/lambda-microvms/<image-name>`
itself, so a Terraform stack never owns it and `terraform destroy` leaves it
behind. The leftover group costs only storage, but it means a clean stack destroy
does not leave a clean account. Query for the log group separately, or delete it in
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

Two things make this easy to miss. First, the filesystem write succeeds, so identity
repair looks like it works until you check the two steps that need the kernel's
permission rather than the filesystem's. Second, the daemon logs the failure and
keeps serving, which produces a healthy-looking VM whose hostname and `boot_id` are
shared with every sibling from the same snapshot. Logging and continuing is still
the right behavior, because a daemon that stopped serving on this failure would
strand the VM.

`ALL` is the only accepted value in the `2025-09-09` API; there is no way to
request `CAP_SYS_ADMIN` alone. A caller who does not need hostname or `boot_id`
repair should leave it unset rather than widen the guest for nothing.

This was found by a live run after the unit tests passed, because those tests
inject a fake layout and a fake platform. The guard had been verified against
fakes at every tier but never against the real platform, and the real platform
was where it failed.

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
exactly backwards. The request is what you are billed for, and the extra memory is
burst headroom charged only for the seconds it is actually consumed. Corrected
2026-08-07 after reading the pricing page rather than inferring from the size.

Three consequences follow for a caller. You cannot use this field to *constrain* a VM, so a
memory-pressure test must generate pressure against what the guest reports rather
than what was requested. Guest swap is absent (`SwapTotal: 0 kB`), so pressure goes
straight to the OOM killer with no paging phase. And picking a small baseline is a
real cost lever rather than a cosmetic one, since baseline is the rate you pay for
every running second.

## What actually costs money

Measured 2026-08-07 from the AWS Pricing API, us-east-1, with live
`pricing.get_products(ServiceCode="AWSLambda")` calls filtered to usage types
containing `MicroVM`.

**Get the rates from the Pricing API, not from the pricing page.** An earlier version
of this section said MicroVMs "has no standalone pricing page: the rates appear only
inside worked examples on the Lambda pricing page". The first half is still true and the
second is wrong: the Pricing API carries MicroVM rates directly, as seven named usage
types under `AWSLambda`, so they are queryable rather than only readable out of prose.
Corrected 2026-08-07 after querying the API rather than continuing to restate the page.
A caller who needs current rates should query, since a hand-copied table drifts and this
one did.

The seven line items, us-east-1, exactly as returned:

| Usage type | Rate | Unit |
| --- | --- | --- |
| `Lambda-MicroVM-vCPU-Second-ARM` | 0.0000276944 | per vCPU-second |
| `Lambda-MicroVM-vCPU-Second` | 0.0000326557 | per vCPU-second |
| `Lambda-MicroVM-Memory-GB-Second-ARM` | 0.0000036667 | per GB-second |
| `Lambda-MicroVM-Memory-GB-Second` | 0.0000043235 | per GB-second |
| `Lambda-MicroVM-Snapshot-Read-GB` | 0.0015467699 | per GB |
| `Lambda-MicroVM-Snapshot-Write-GB` | 0.0037977138 | per GB |
| `Lambda-MicroVM-Snapshot-Storage-GB-Hour` | 0.0001111111 | per GB-hour |

Data transfer is not among them and bills at standard AWS rates, including MicroVM to
your own VPC.

**Snapshot storage was understated.** This section listed $0.08 per GB-month. The API
prices storage per GB-hour, and $0.0001111111 per GB-hour is $0.0811111030 at AWS's own
730-hour month, so the old figure was 1.37% low. Corrected 2026-08-07. The one-week
minimum retention still applies. The rest of the old table survived the check: read was
0.00155 against 0.0015467699 (0.21% high), write 0.0038 against 0.0037977138 (0.06%
high), and both compute rates matched to the digit.

**There are two compute rates 17.9% apart, and only the ARM one can ever apply.** The
service model's `Architecture` enum has exactly one member, `ARM_64`, so a MicroVM
cannot be x86 and the non-ARM line items are unreachable for this service. The old table
used the ARM figures and was correct by luck rather than by construction. The Pricing API
returns both rates, and the Lambda pricing page gives a reader no obvious signal about
which applies, so someone pricing a fleet by hand can land on the non-ARM column and
overstate compute by 17.9%.

**Only five regions price MicroVMs, and rates vary by region.** us-east-1,
us-east-2, us-west-2, eu-west-1, and ap-northeast-1 return the seven line items;
eu-central-1, ap-southeast-2, and sa-east-1 return none. us-east-2 and us-west-2 are
identical to us-east-1 on every line item. The other two are not:

| Region | ARM compute | Snapshot read | Snapshot write | Snapshot storage |
| --- | --- | --- | --- | --- |
| us-east-2, us-west-2 | same | same | same | same |
| eu-west-1 | +5.3% | +6.0% | +7.0% | +19.0% |
| ap-northeast-1 | +16.4% | +19.9% | +22.6% | +20.0% |

So a Tokyo caller who estimates from us-east-1 rates understates their bill by up to
22%, and the largest gaps are on the snapshot dimensions rather than on compute, which
matters most for a design that leans on a suspended pool.

One measurement trap produces a confident wrong answer. us-east-1 usage types are
unprefixed while every other region carries a location prefix,
`USW2-Lambda-MicroVM-Snapshot-Read-GB` and so on. Comparing raw `usagetype` strings
across regions therefore matches nothing outside us-east-1. The first pass at this table
came out as NaNs, which reads as "no regional variation" rather than as a join bug. Strip
the prefix before comparing.

vCPU and memory bill as two separate line items rather than one blended
GB-second, and there is no per-request charge: instances bill per second. No
MicroVMs free tier is published; the Lambda free tier is Functions-only. No
minimum billing increment is published.

Three cost behaviors affect a create-and-destroy test suite like this repo's:

**Image storage has a one-week minimum.** A 2 GB image deleted sixty seconds after
creation still bills about a week of storage, roughly four cents. Our conformance
suite builds a fresh image per run, so "it costs pennies" is right per run but the
floor is the image rather than the compute.

**Idle time while RUNNING is billed at baseline.** This differs from AgentCore
Runtime, which charges no CPU during I/O wait. On raw MicroVMs, wall-clock time in
`RUNNING` costs baseline whether or not anything is executing, so suspension is the
only way to stop paying.

**A suspended VM is cheap to keep, but each suspend/resume cycle has a fixed
cost.** A suspended 2 GB VM pays
only snapshot storage, about $0.16 a month. Leaving the same VM running at baseline
costs roughly $100 a month. That difference of two orders of magnitude is what makes
a warm suspended pool viable. But each suspend/resume cycle pays a snapshot write
plus a read, about $0.011 for a 2 GB VM, so the thing to avoid is suspending and
resuming constantly rather than suspending for a long time.

**Not published:** whether the server-side image build is billed as compute. The
build starts a real MicroVM to run the Dockerfile, so it plausibly is, but AWS does
not say and we have not measured it. Do not assume either way. The Pricing API does not
settle it either, and that is now a checked finding rather than an assumption: none of
the seven MicroVM usage types names a build, so if build time is billed it arrives on one
of the existing compute or snapshot dimensions rather than on a line item of its own. A
reader auditing a bill for a distinct build charge will not find one, which is not the
same as the build being free.

## Seeing an OOM: the process case works, the VM case is still unmeasured

Measured 2026-08-07, us-east-1, via `conformance/probe_oom.py` (deleted with the
Python client after the Rust port went live-green; the probe is in git history and
this entry is its result). The customer
question is "is there a `dmesg`?" and it splits in two.

**A process killed inside a living VM should be visible in two places, and the
plumbing for both is confirmed present.** What was actually measured: `dmesg` runs in
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
could not make one happen.** Two attempts failed, for the following reasons:

1. The first probe allocated with `python3`, which the `amazonlinux:2023-minimal`
   base image does not have. It reported `command not found` with exit code 127 and
   every downstream check passed. The probe measured nothing while looking like a
   clean result. This project has hit the same failure mode repeatedly, where a
   run reports green without ever exercising the behavior under test.
2. The second allocated with `dd` into `/dev/shm`, which is tmpfs and therefore
   capped near half of RAM. `dd` stopped at 64 MiB against a 1 GiB request and
   exited 0, so again no memory pressure. tmpfs limits are a filesystem ceiling,
   not a memory one.

So `stateReason` was `null` and the state `RUNNING` throughout, which says only
that we never applied real pressure. A future probe needs an allocator that touches
anonymous memory the kernel must back and cannot silently cap. A small static
binary shipped in the image is the obvious answer, since the guest has no
interpreter and no compiler.

What the runs *did* establish: the daemon survived 64 MiB of output under
concurrent allocation with `truncated: true` on the result, so the output cap holds
under pressure, and `/v1/health` stayed reachable and bootstrapped throughout.

## Suspend/resume is a freeze and restore, not a stop and start

Measured 2026-08-05, us-east-1, `al2023-1` base, 1024 MiB baseline, via
`conformance/probe_suspend_resume.py` (deleted with the Python client after the Rust
port went live-green, so the probe is in git history and this entry is its result).
The same assertions now run inside `conformance/run_rs.py`'s suspend/resume section.
`SuspendMicrovm`, held 45 seconds, then `ResumeMicrovm`. Everything survived:

| What | Result |
| --- | --- |
| In-memory agent token | survived — `/v1/health` reports `bootstrapped: true` |
| Filesystem | survived |
| Exec records, including unacked output | survived |
| A backgrounded process | survived and kept running after resume |
| Endpoint URL | unchanged |

A ticker writing `date +%s` once a second provided the direct evidence. The
largest gap between consecutive ticks was 51 seconds, which matches the suspension
plus transition time, and the tick file grew by 6 lines over 6 seconds after resume.
The guest is frozen rather than killed, and its processes continue from the point
where they stopped.

Two consequences follow. Pause/resume needs no token re-delivery and no
re-bootstrap, which makes a warm suspended sandbox pool viable. Suspend an idle VM
instead of terminating it, and the next task lands in a VM that still has its
filesystem, its installed tools, and its credentials. Separately, a guest process
that measures wall time sees the suspension as a single jump, so anything holding a
timeout, a lease, or a TLS session across a suspend will observe it expire at once.

This corrects an earlier claim in the daemon's own resume-hook docstring, which
asserted that bootstrap state being in memory made a resumed VM unable to serve the
control API. That claim was inferred from where the state lives rather than
measured, and the measurement showed it was wrong.

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

A source-address rule that rejects loopback callers on the bootstrap route would
reject the platform's own legitimate bootstrap and break every launch. Do not
implement it. This inverts the usual intuition, in which a loopback filter looks
like a safe extra control. An earlier attempt broke 39 tests, and those failures
were reporting a real defect rather than a harness artifact.

Because in-VM traffic is indistinguishable from platform traffic at the socket
level, the one-shot bootstrap is the only available defense on that route. Its
sufficiency is checked mechanically in `model/`.

## Something probes the port with TLS before bootstrap

Measured in the same 2026-08-04 run. The daemon receives raw TLS handshake bytes
on its plaintext port:

```
code 400, message Bad request version ("\x13\x01\x13\x02...")
```

That is a TLS ClientHello reaching a plaintext HTTP server. Something in the
platform's path probes the port with TLS first. The probe is harmless. The correct
response is a 400 and a debug-level log, and it must not take the listener down.
It is documented here because it looks like an attack in logs.

## Endpoint authentication

Documented (`microvms-networking.html`): every request to a MicroVM endpoint
requires an `X-aws-proxy-auth` JWE scoped to a specific MicroVM ID, a specific
port set, and an expiry of at most 60 minutes, minted by
`create-microvm-auth-token`.

There is no unauthenticated internet path to the daemon's port. Port scoping is
the useful part: a token minted for port 9000 cannot reach port 8080, so a task
workload and a control plane can share a VM with access handed out to only one.

The 60-minute ceiling means a long-running trial will mint a fresh token
mid-flight. Token minting therefore sits inside the retry path, and boto/HTTP
errors from minting must be handled wherever a request can be retried.

## `clientToken` is a permanent idempotency key

Measured 2026-08-02, us-east-1. A `clientToken` derived from a
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

Measured 2026-08-05. The prefix is not `/aws/lambda/microvms/*`. An IAM policy granting the
wrong prefix produces server-side builds with no logs at all, and every failure
then reports `reason=unknown` — which reads as the service failing to populate
`stateReason` when it is really the caller's own policy discarding the logs.

Build roles also need ECR permissions if any task points `docker_image` at a
same-account ECR repository; without them the build fails outright.

## `idlePolicy`

Documented, and confirmed useful in practice. Idle time is measured by inbound
traffic through the proxy, so an abandoned VM auto-suspends and then terminates
rather than billing to the 8-hour `maximumDurationInSeconds` ceiling.

Clients that suspend deliberately to preserve state hit a sharp edge. The
launch-time `idlePolicy` terminates a suspended VM after
`suspended_timeout_sec`, so a "resume later" affordance silently stops working
once that window passes. State the window wherever a resume path is offered.

## Most public ARM64 base images have no WORKDIR

Measured 2026-08-05. `al2023-minimal`, `python:3.12-slim`, and `node:20-slim` all
leave `WorkingDir` empty. Anything that tests WORKDIR inheritance needs a purpose
-built image with `WORKDIR` set, since there is nothing to inherit otherwise.
