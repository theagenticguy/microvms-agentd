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

`CreateMicrovmShellAuthToken` exists in the API. It requires a `SHELL_INGRESS`
connector, the documentation scopes it to debugging, and it recommends disabling it
in production. **The claim that it is not programmatically drivable was wrong, and
was measured wrong on 2026-08-15** — see "The shell endpoint is a real PTY over a
WebSocket" below. It is still not a substitute for this project, because it gives one
interactive shell session rather than addressable execs, but a caller who needs a PTY
can drive it from code.

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

That check now exists and is reachable, which it effectively was not before. This client
refuses an over-ceiling payload in `RunHookPayload::for_launch`, before any control-plane
call. It matters more since the payload started carrying a launch `env` map alongside the
token: a bearer token is a few dozen bytes and fit with room to spare, so the ceiling was
unreachable through the typed constructor, while a caller putting credentials in `env`
reaches it easily. Note also that the *daemon* cannot enforce this ceiling at all — an
over-ceiling `runHookPayload` is rejected at `RunMicrovm` and the request never reaches
the guest — so the client is the only place the check can live.

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

### `allowedPorts` is a union of three forms, and the scoping is enforced

Measured 2026-08-15, us-east-1, API version `2025-09-09`. One VM with a listener on
8080, four tokens, varying **only** `allowedPorts` — so the token's scope is the whole
of the difference:

| `allowedPorts` | `GET :8080` through the endpoint |
| --- | --- |
| `[{"port": 9000}]` | **403 `Access to port denied`** |
| `[{"port": 9000}, {"port": 8080}]` | 200, the guest's own server answered |
| `[{"allPorts": {}}]` | 200 |
| `[{"range": {"startPort": 8000, "endPort": 9100}}]` | 200 |

So the documented sentence above — "a token minted for port 9000 cannot reach port
8080" — is exactly true, and it is enforced at the proxy rather than at mint time. The
mint of a token for a port with nothing listening succeeds; a request through it
answers **502** rather than 403, which is the distinction between "not authorized" and
"authorized, nothing there". That pair is the only diagnostic separating a scope
mistake from a dead server, and it is worth more than it looks: **on the WebSocket path
both are close code 1006 with no reason string.**

`PortSpecification` is a Smithy tagged union with three members — `port`, `range`
(`startPort`/`endPort`, both required), and `allPorts` (no members, wire form `{}`).
The wire form is the member name as the sole key. A client emitting a discriminator
field instead — which is what most enum serializers do by default — sends a member the
shape does not declare and is rejected.

**One token can cover several ports, which is what makes a per-port credential helper
possible at all.** A client reaching more than one port on a VM has a choice: one token
per port, or one token naming all of them. Naming them together is fewer control-plane
calls and one refresh schedule instead of several; the cost is a credential whose leak
reaches further, which is why `allPorts` should not be a default.

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

### A guest-side request cannot reset the idle timer

This follows from the loopback measurement above rather than from a separate
experiment, and it is recorded here because the wrong conclusion is the attractive
one. Idleness is measured by inbound traffic through the endpoint proxy. That proxy
terminates *outside* the VM and forwards over loopback ("The platform's own hook
arrives over loopback"), so traffic a guest process sends to the daemon's own port
is generated on the far side of the thing doing the measuring and never passes
through it.

The consequence is a real workload hazard, not a theoretical one. A workload holding
an outbound connection receives no inbound traffic, and neither does one that is
simply computing; multi-hour agent runs have been observed past 400 minutes. Such a
VM can be auto-suspended mid-work while it is busy.

So an in-VM "keep myself alive" route is not implementable against this platform. A
daemon that offered one would answer 200 and change nothing, and the failure would
surface as a suspend during exactly the long run the route was added to protect —
the least debuggable moment available. What works instead is a poll from **outside**
the VM, which is real inbound traffic; `GET /v1/health` carries `busy` and `execs` so
that such a poll can be informed by whether the workload is actually running rather
than being unconditional. See `PROTOCOL.md`, "Idle policy, and why liveness is a
field rather than a route".

**Not measured:** whether a poll of `/v1/health` from outside the VM does in fact
reset the idle timer, and by how much. It is inbound traffic through the endpoint by
construction, so it should, but nobody has run a VM to the edge of its
`maxIdleDurationSeconds` while polling and watched it not suspend. Treat the
orchestrator-poll pattern as sound reasoning from a confirmed measurement rather
than as an observation of the outcome.

## Most public ARM64 base images have no WORKDIR

Measured 2026-08-05. `al2023-minimal`, `python:3.12-slim`, and `node:20-slim` all
leave `WorkingDir` empty. Anything that tests WORKDIR inheritance needs a purpose
-built image with `WORKDIR` set, since there is nothing to inherit otherwise.

## A WebSocket reaches a guest server through the endpoint, and the proxy strips its own subprotocols

> **Merge note.** `feat/live-measurements` adds a section under this same heading from an
> independent run. The two agree on every shared observation — the three-value handshake
> works, the guest sees no `sec-websocket-protocol`, a full-length JWE is token-legal, a
> fourth application value reaches the guest, every failure is 1006 — so either text can be
> kept. **What is only here** is the fourth row of the table below and the paragraph after
> it: the credentials came out of `Session::connect_subprotocols`, and that is what exposed
> the port-scope defect. Keep that part regardless of which prose survives.

Measured 2026-08-15, us-east-1, API version `2025-09-09`, from an existing
`coding-agents-b8ea1298a3b2` image. The guest ran a hand-rolled RFC 6455 echo server on
node 18 (the image has no `ws` package and node has no built-in WebSocket *server*),
logging every request header it received. The host connected with node's global
`WebSocket` — the client shape that matters, because it is the one that cannot set a
request header and is therefore the reason the platform moves auth into subprotocols.

**Every credential in this run came from `Session::connect_subprotocols(port)` and
`connect_headers(port)` through the built napi addon, not from strings assembled by the
test.** That distinction is the whole value of the run: a test that spells the three
values itself measures the platform, and the platform was never in doubt.

| Question | Observation |
| --- | --- |
| Does the upgrade succeed with our helper's output | Yes. `readyState` 1 against `wss://<endpoint>/` |
| What `Sec-WebSocket-Protocol` reaches the guest | **Nothing.** Absent from `req.headers` *and* from `rawHeaders`, so not a normalization artifact |
| Must the guest echo a subprotocol | **No.** A 101 naming none is accepted, and the client still reports `ws.protocol === "lambda-microvms"` |
| Do frames flow both ways | Yes. Three text frames out, three prefixed echoes back, in order |
| Does a full-length JWE survive as a subprotocol name | Yes. An 899-byte auth value, zero non-RFC-7230-token bytes |

The offered list is `["lambda-microvms", "lambda-microvms.authentication.<jwe>",
"lambda-microvms.port.<n>"]` at lengths 15, 899, and 25. The JWE is token-legal by
construction rather than by luck: compact-serialization JWE is base64url segments joined
by `.`, and every one of those bytes is a tchar. No length limit was reached and nothing
is re-encoded, so a caller does not escape or chunk the token.

**The proxy strips its own headers on the plain-HTTPS path too.** The same guest, asked
with `connect_headers(8080)`, answered 200 and reported `x-aws-proxy-auth` absent and
`x-aws-proxy-port` absent from what it received. So neither transport leaks the
credential into the VM, and a server inside needs no MicroVM awareness on either.

**An application subprotocol passes through and the guest may negotiate it.** A fourth
value alongside the three platform ones arrives as `sec-websocket-protocol:
my-app-protocol`, alone, with all three platform values still stripped. If the guest
names it in its 101 the client observes `ws.protocol === "my-app-protocol"`; if the guest
names nothing the client observes `lambda-microvms`, which the proxy supplies on the
guest's behalf. **So client-visible `ws.protocol` is not evidence about the guest** and
must not be used as a negotiation check.

**Every handshake failure is close code 1006 with no reason, and that is the reason the
header path matters.** A token minted for the wrong port, a missing auth value, a dead
TCP connection: all 1006, indistinguishable. The same wrong port on a plain authenticated
`GET` answers 403 `Access to port denied` — or 502 when the scope is right and nothing is
listening. That HTTPS request is the only way to tell a scope mistake from a dead server,
which is why a client offering `connect_subprotocols` should offer `connect_headers`
beside it.

## The guest kernel is 6.1, which `openat2` needs

Measured 2026-08-14, us-east-1, `al2023-1` base: `uname -r` inside a running VM
reports `6.1.166-24.303.amzn2023.aarch64`.

The daemon's tar extraction resolves every member through `openat2` with
`RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS`, which the kernel has supported since
5.6. Recording the measured version means that dependency rests on a number
someone checked rather than on an assumption about what Amazon Linux 2023
ships. A guest older than 5.6 would answer `ENOSYS` and fail extraction rather
than fall back to weaker confinement, which is the intended behavior.

## A WebSocket reaches a guest server through the endpoint, and the proxy strips its own subprotocols

Measured 2026-08-15, us-east-1, API version `2025-09-09`, from an existing
`coding-agents-b8ea1298a3b2` image. The guest ran a hand-rolled RFC 6455 echo server
under node 18 (the image has no `ws` package and node has no built-in WebSocket
*server*), logging every request header it received. The host connected with node 22's
global `WebSocket`, which is the only client shape that matters here because it is the
one that cannot set a request header and therefore the reason the platform moves auth
into subprotocols at all.

The documented three-value handshake works, and everything it implies works:

| Question | Observation |
| --- | --- |
| Does the upgrade succeed | Yes. `open` with `readyState` 1 against `wss://<endpoint>/` |
| What `Sec-WebSocket-Protocol` reaches the guest | **Nothing.** `req.headers['sec-websocket-protocol']` is absent — verified against `rawHeaders`, so it is not a normalization artifact |
| Must the guest echo a subprotocol in its 101 | **No.** A 101 naming none is accepted and the client still reports `ws.protocol === "lambda-microvms"` |
| Does a full-length JWE survive as a subprotocol name | Yes. 868-byte JWE, so the auth subprotocol is 899 bytes |
| Do frames flow both ways | Yes. Six text frames sent and six echoes received in order on one connection |

So the offered list is `["lambda-microvms", "lambda-microvms.authentication.<jwe>",
"lambda-microvms.port.<n>"]` at lengths 15, 899, and 25, and the guest sees none of
them. The three values are consumed by the proxy.

**The JWE is token-legal by construction rather than by luck, which is worth stating
because it is the risk that looked most likely to sink this.** RFC 6455 requires each
subprotocol name to be an RFC 7230 token, and a token excludes the separators. The
minted JWE is compact-serialization JWE — five base64url segments joined by `.` — and
every byte of it is drawn from `[A-Za-z0-9_-]` plus the `.` separators, all of which are
tchars. Checked directly: the 868-byte token contains zero non-token characters. No
length limit was reached at 899 bytes, and nothing in the handshake is base64-re-encoded,
so a caller does not have to escape or chunk the token.

**Two absences are rejections, not degradations.** Dropping the auth subprotocol, and
dropping the bare `lambda-microvms` marker while keeping the other two, both fail the
same way: the browser-shaped client reports an opaque `error` event and then close code
**1006** with no reason string. Port scoping is enforced on this path exactly as it is on
the header path — a token minted for 8081 offered with `lambda-microvms.port.8080` is
also 1006. Since 1006 is what a client is given for a dead TCP connection too, a caller
debugging a handshake gets no signal distinguishing a bad token from a bad port from a
network fault, and must fall back to a plain authenticated HTTPS `GET` on the same port
to tell them apart. That request does answer usefully.

**An application subprotocol passes through, and the guest may negotiate it.** This is
the part the documentation's "Lambda removes MicroVM-specific subprotocols" does not
say, and it is what makes a real protocol possible over this transport. Offering a
fourth value alongside the three platform ones delivers exactly that one to the guest
(`sec-websocket-protocol: my-app-protocol`, with the three platform values still
stripped). If the guest names it in its 101, the client observes
`ws.protocol === "my-app-protocol"`; if the guest names nothing, the client observes
`lambda-microvms`, which the proxy supplies on the guest's behalf. So the client-visible
`ws.protocol` is not evidence about the guest and must not be used as a negotiation
check.

What follows for a caller: a subprotocol helper that returns the three strings is usable
as written, the guest side of a WebSocket application needs no MicroVM awareness at all,
and an application that wants its own subprotocol appends it as a fourth value rather
than replacing any of the three.

## An outside poll of `/v1/health` does reset the idle timer, and the control half proves it

Measured 2026-08-15, us-east-1, API version `2025-09-09`, from an existing
`coding-agents-b8ea1298a3b2` image. Two VMs launched with identical settings —
`maxIdleDurationSeconds: 60` (the model's minimum: `IdlePolicy.maxIdleDurationSeconds`
declares `min: 60`), `suspendedDurationSeconds: 900`, `autoResumeEnabled: false` — each
running a detached `sleep 300`, with `GetMicrovm` sampled every ~20 seconds for about
five minutes. The only difference between them was whether the host polled
`/v1/health` through the endpoint on each sample.

| Elapsed (s) | Polled every ~20s | Control, no polls |
| --- | --- | --- |
| 1 | RUNNING | RUNNING |
| 22–45 | RUNNING | RUNNING |
| 66 | RUNNING | **SUSPENDED** |
| 100–311 | RUNNING throughout | SUSPENDED throughout |

The control is the load-bearing half. It suspended at the first sample past its 60-second
window and stayed suspended for the remaining four minutes, which is what establishes that
the polled VM's 311 seconds of `RUNNING` is the polling and not a lax platform. Both halves
are needed and the first alone would have proved nothing.

**A guest that is busy does not, by itself, keep a VM alive.** The control held a running
`sleep 300` in the daemon the entire time it was suspended. Idle is measured by inbound
traffic through the proxy, exactly as `idlePolicy` documents, and in-guest work is
invisible to it. So a long exec with no outside traffic will be suspended out from under
its caller at the idle window, and the process survives (suspend is a freeze, not a kill)
but nothing external can reach it until someone resumes it.

**This makes the outside-poll pattern sound.** An orchestrator that polls `/v1/health`
on an interval shorter than `maxIdleDurationSeconds` holds a long run alive. `/v1/health`
is the right route for it: it already exists on `main`, it is the one unauthenticated
route, and each poll is one small request. A 20-second interval against a 60-second
window gives two missed polls of margin.

Note what this measurement does *not* cover. It says nothing about whether any
particular field in the health response is present — the route alone is what resets the
timer, and any inbound request through the proxy would presumably do the same, though only
`/v1/health` was measured. A caller adding `busy`/`execs` to the response is choosing
*what the poller learns*, not changing whether the poll keeps the VM alive.

## The 4096-byte `runHookPayload` ceiling is on the whole string, env map included

Measured 2026-08-15, us-east-1, API version `2025-09-09`. The 2026-08-07 bracketing used a
token-only payload, which left open whether a payload carrying an `env` map is measured
differently — a plausible worry, since env is the field a caller is most likely to grow
past the limit. It is not. The ceiling is on the serialized string and nothing about its
contents changes it.

Bracketed the same way, with a deliberately bogus `imageIdentifier` so nothing was created
or billed, against payloads of exactly 4096 and 4097 bytes shaped
`{"agent_token":"...","env":{"FOO":"bar","PAD":"xxx..."}}`:

| Payload length | Result |
| --- | --- |
| 4096 bytes | `Malformed ARN - doesn't start with 'arn:'` — past the length check, failing only on the bogus ARN |
| 4097 bytes | `1 validation error detected: Value at 'runHookPayload' failed to satisfy constraint: Member must have length less than or equal to 4096` |

Byte-identical to the token-only result. So a client that refuses locally above 4096 bytes
is refusing exactly what the platform would refuse, and a local check on the fully
serialized payload — after the env map is folded in, not before — is the correct guard.

## The shell endpoint is a real PTY over a WebSocket, and it is programmatically drivable

Measured 2026-08-15, us-east-1, API version `2025-09-09`. This entry **refutes** the
claim this document opened with, and which `microvms-core/src/control/connector.rs`
records as its reason for omitting a `SHELL_INGRESS` variant: that
`CreateMicrovmShellAuthToken` is a console-only debugging path, not drivable from code.
It is drivable from code, it took one node script, and it provides a capability the exec
API does not have.

**Getting a shell token requires a connector combination the client cannot currently
express, and the failure is late.** Three findings, in the order they were hit:

1. `CreateMicrovmShellAuthToken` on a VM launched with `ALL_INGRESS` only:
   `ValidationException: Shell access requires SHELL_INGRESS network connector to be
   configured on the MicroVM.`
2. `RunMicrovm` **accepts** `[SHELL_INGRESS, ALL_INGRESS]` and the VM reaches `RUNNING`
   with both listed in `GetMicrovm`. The rejection arrives later, from the token call:
   `ValidationException: ALL_INGRESS cannot be combined with other ingress network
   connectors; use HTTP_INGRESS and/or SHELL_INGRESS instead`. So an invalid connector
   set is launchable and bills until something asks for a shell token.
3. **`HTTP_INGRESS` exists** and is not in this client's enum. `[HTTP_INGRESS,
   SHELL_INGRESS]` launches and mints a shell token successfully. `ALL_INGRESS` is
   evidently the union that cannot be intersected, and the finer-grained pair is what a
   VM needs to have both a daemon endpoint and a shell.

**The shell token is the same kind of credential as the ordinary one.** It is a
`TokenParts` map with the single key `X-aws-proxy-auth`, and its value is a compact JWE
with the identical protected header as an ordinary proxy token — `{"kid": "...", "alg":
"dir", "enc": "A256GCM"}`, same `kid`. The lengths differed only by payload (767 vs 823
bytes on the same VM). What differs is the *request*: `CreateMicrovmShellAuthToken` has no
`allowedPorts` parameter at all, only `microvmIdentifier` and `expirationInMinutes`. The
shell is not a port.

**What the endpoint speaks.** Not SSH, and not HTTP: an authenticated HTTPS `GET` with the
shell token answers **502** with an empty body, with or without an `X-aws-proxy-port`
header. It is a **WebSocket on the same endpoint URL**, opened with the same subprotocol
mechanism as any other WebSocket through the proxy but with **no port subprotocol** —
`["lambda-microvms", "lambda-microvms.authentication.<shell-jwe>"]` is sufficient, and
adding `lambda-microvms.port.<n>` neither helps nor hurts.

The session then speaks a small mixed protocol:

- One **text** frame on connect: `{"type":"session_init","session_id":"<uuid>"}`.
- **Binary** frames thereafter, carrying raw terminal bytes in both directions.
- Client input is raw keystrokes as sent, so `"echo hi\n"` runs `echo hi`.
- A **JSON control frame** resizes the terminal: `{"type":"resize","cols":120,"rows":40}`
  is honored, after which `stty size` in the guest reports `40 120`. Before any resize it
  reports `0 0`.
- Close on `exit` is clean: code **1000**, reason **`shell exited`**.

**It is a genuine PTY, which is the capability our exec API lacks.** Observed inside the
session, verbatim: `tty` reports `/dev/pts/2`; `id` reports `uid=0(root) gid=0(root)`;
`TERM=xterm`; the prompt is `λ $` with bracketed-paste sequences (`ESC[?2004h`), so input
is echoed by a line discipline rather than by the application; `sleep 60 &` yields
`[1] 87` and `jobs` reports `[1]+ Running`, so job control is present; and a `0x03`
byte raises SIGINT, after which `echo $?` reports **130**. None of that is reachable
through `POST /v1/exec`, which gives a child pipes and no controlling terminal.

Two sharp edges for anyone building on it. An unrecognized control frame is **not
rejected** — `{"type":"window_size",...}` was delivered into the shell as literal
keystrokes, producing `bash: type:window_sizestty: command not found`, so a typo in a
control message corrupts the terminal instead of erroring. And there is no exit-status
channel: the shell's own exit is a WebSocket close, so a caller wanting a command's
status must ask the shell for it (`echo $?`) and parse it out of the terminal stream.

**What this means for the design.** The docs' framing was right about intent and wrong
about capability, and this document repeated the wrong half. `SHELL_INGRESS` is a
first-class interactive-terminal surface: one session per connection, addressed by nothing,
with output as an unstructured byte stream. It is not a substitute for an exec API — there
are no exec ids, no idempotency, no separated stdout/stderr, no exit codes, and no
concurrent addressable commands. But it is the answer to "can a caller get a PTY", and the
answer is yes, without us building one. A PTY surface in this project would be a
convenience wrapper over this WebSocket rather than new platform capability. Whether to
grow one is now a product decision on a measured capability rather than a guess, and the
connector enum's omission of `SHELL_INGRESS` should be re-justified on the grounds that
actually hold — one interactive session is not programmatic exec — rather than on
"not programmatically drivable", which is false.

## Tagging works on images and not on MicroVMs, and `RunMicrovm` takes no tags

Measured 2026-08-15, us-east-1, API version `2025-09-09`. The operations are
`TagResource`, `UntagResource`, and `ListTags`, and the parameter naming the target is
`--resource` rather than the `--resource-arn` most AWS services use.

An **image** ARN tags and reads back cleanly. Tagging
`arn:aws:lambda:us-east-1:<acct>:microvm-image:<name>` twice accumulated both keys, and
`ListTags` returned `{"Tags": {"probe": "live-measure", "probe2": "second"}}`. The tags also
appear on `GetMicrovmImage` under a `tags` field. `UntagResource --tag-keys` removed them,
leaving `{"Tags": {}}`. Existence is checked: a nonexistent image name answers
`ResourceNotFoundException: MicroVMImage not found for MicroVMImageID: <arn>`.

A **running MicroVM cannot be tagged at all**, and the way it fails is worth recording
because it reveals that the shared Lambda ARN grammar has not been extended for this
resource. Both the constructed ARN
`arn:aws:lambda:us-east-1:<acct>:microvm:microvm-<uuid>` and the bare MicroVM id are
rejected by a regex that enumerates the taggable Lambda resource types — `function`,
`lite-function`, `web-function`, `layer`, `code-signing-config`, `event-source-mapping`,
`capacity-provider`, `network-connector` — and lists **neither `microvm` nor
`microvm-image`**. So the image case works despite the pattern rather than because of it,
and the MicroVM case has no spelling that would satisfy it. Do not spend time hunting for
the right MicroVM ARN form; there is not one.

At create time the two operations differ. `CreateMicrovmImage` accepts `--tags`, so an
image can be born tagged. **`RunMicrovm` has no tags parameter**, which together with the
above means a MicroVM instance cannot be tagged at any point in its life. Cost allocation
by tag therefore cannot attribute per-VM compute; the image is the finest-grained taggable
unit. A caller wanting per-run attribution needs a tagged image per run, which trades
against the one-week snapshot storage minimum.

## Build introspection returns snapshot sizes and a chipset generation, not logs

Measured 2026-08-15, us-east-1, API version `2025-09-09`, against the existing
`coding-agents-b8ea1298a3b2` image.

`ListMicrovmImageBuilds` requires **both** `--image-identifier` and `--image-version`;
omitting the version is a client-side `ParamValidation` failure, so there is no way to list
an image's builds across versions in one call.

The answer for one version is **two builds, one per Graviton generation**, and this is the
useful finding — a single `CreateMicrovmImage` fans out into a build per chipset:

```
buildId=b5855cc4-... buildState=SUCCESSFUL architecture=ARM_64 chipset=GRAVITON chipsetGeneration=4
buildId=ea7ef2ca-... buildState=SUCCESSFUL architecture=ARM_64 chipset=GRAVITON chipsetGeneration=3
```

Both carry the same `createdAt` as the image. So "the build" is a fan-out, and a partially
failed image is a state a caller should expect: one generation could fail while the other
succeeds.

`GetMicrovmImageBuild` adds exactly one thing over the list entry, a `snapshotBuild`
breakdown:

| Field | Value for this image |
| --- | --- |
| `memorySnapshotSizeInBytes` | 579080192 (~552 MiB) |
| `codeInstallSizeInBytes` | 2357084160 (~2.2 GiB) |
| `diskSnapshotSizeInBytes` | 24297472 (~23 MiB) |

**It returns no logs, no failure reason, and no timing.** There is no log field, no
`stateReason`, and no started/finished timestamps — only the one `createdAt`. This confirms
from the other direction why the build log group matters: CloudWatch at
`/aws/lambda-microvms/<image-name>` is the *only* place a failed build's evidence lives,
because build introspection will not tell you why anything failed. The three snapshot sizes
are, however, the quantities the snapshot read/write/storage line items bill on, so they are
what to multiply a storage estimate from rather than the Dockerfile's image size.

`GetMicrovmImageVersion` is the richer call and echoes the whole creation request back:
`baseImageArn` with `baseImageVersion`, `buildRoleArn`, the `codeArtifact` S3 URI,
`egressNetworkConnectors`, `cpuConfigurations`, `resources` with `minimumMemoryInMiB`, and
the full `hooks` structure including the port and every hook's enabled flag and timeout. It
carries both a `state` (`SUCCESSFUL`) and a `status` (`ACTIVE`), which are separate fields.
Reading it is the way to find out what an image was actually built with when the caller no
longer has the request.

## The managed base image has two versions, and its versions are bare integers

Measured 2026-08-15, us-east-1, API version `2025-09-09`.
`ListManagedMicrovmImages` returns exactly one item,
`arn:aws:lambda:us-east-1:aws:microvm-image:al2023-1`, created 2026-06-17 and updated
2026-07-21. So a client hardcoding one managed base is not currently missing anything.

`ListManagedMicrovmImageVersions` — which takes `--image-identifier`, the full ARN, not the
name — returns **two** versions: `"1"` (created 2026-07-21) and `"0"` (created 2026-06-17).
The version strings are bare integers, where a custom image's versions are `"1.0"`. Two
things follow. A client that omits `baseImageVersion` is taking whatever the service
defaults to rather than pinning, and since a second version has already appeared, that
default has already moved once. And version strings are not comparable across managed and
custom images, so code that parses one format will not parse the other.

Worth noting against the above: `GetMicrovmImageVersion` on an image built from this base
reports `baseImageVersion: "1.0"`, not `"1"` or `"0"`. The version the service echoes back
for a base is spelled differently from any version the base's own listing offers, so the
two cannot be compared as strings and a caller should not try.

## Every field `GetMicrovm` returns for a running VM

Measured 2026-08-15, us-east-1, API version `2025-09-09`. Recorded verbatim so the client
can be audited for fields it ignores. A VM launched from a custom image with both
connectors:

```json
{
    "microvmId": "microvm-a991cd0b-3321-3e2c-a982-d5c4db52b17a",
    "state": "RUNNING",
    "endpoint": "30d00336-db07-f4f5-79c1-8571c22bef9e.lambda-microvm.us-east-1.on.aws",
    "imageArn": "arn:aws:lambda:us-east-1:392583147479:microvm-image:coding-agents-b8ea1298a3b2",
    "imageVersion": "1.0",
    "executionRoleArn": "arn:aws:iam::392583147479:role/agentd-conformance-exec-b2111c56",
    "idlePolicy": {
        "maxIdleDurationSeconds": 1800,
        "suspendedDurationSeconds": 600,
        "autoResumeEnabled": false
    },
    "maximumDurationInSeconds": 10800,
    "startedAt": "2026-08-15T17:17:28.601000+00:00",
    "ingressNetworkConnectors": [
        "arn:aws:lambda:us-east-1:aws:network-connector:aws-network-connector:ALL_INGRESS"
    ],
    "egressNetworkConnectors": [
        "arn:aws:lambda:us-east-1:aws:network-connector:aws-network-connector:INTERNET_EGRESS"
    ]
}
```

Eleven fields. Note what is **absent on a healthy VM**: no `stateReason` (it appears only
on a failure, which is why TRAP-8 reads it on the terminal states), and no `runHookPayload`
echo — the launch secret is not readable back out of the control plane, which is the
behavior a per-VM token delivery depends on. There is also no field reporting the size
class: `minimumMemoryInMiB` lives on the image version, not the instance, so the only way
to know a running VM's memory from the API is to fetch its image version.

`autoResumeEnabled` is the field most worth flagging. It is **required** in the
`IdlePolicy` structure and it is echoed here, and a client that always sends `false` is
declining a platform feature — a VM with it enabled resumes itself on an incoming request
rather than needing an explicit `ResumeMicrovm`. Its interaction with the idle timer was
not measured.

## A detached exec survives the 60-minute proxy-token ceiling

Measured 2026-08-15, us-east-1, API version `2025-09-09`, from an existing
`coding-agents-b8ea1298a3b2` image at the 2 GB baseline. A **75-minute** detached exec
(`--detach --exec-id probe4-rotation`) wrote a numbered timestamp every 10 seconds for 450
iterations and then printed a marker. It ran from 17:17:52Z to 18:32:44Z, crossing the
proxy token's 60-minute ceiling at 18:17:52Z, and was reattached and polled afterwards
from a fresh process.

It survived, and the output is continuous across the boundary:

| Check | Observation |
| --- | --- |
| Final poll | `phase: exited`, `exitCode: 0`, `truncated: false` |
| Ticks recovered | 450 of 450, indices contiguous 0..449, plus the `DONE-PROBE4` marker |
| Span | 74.87 minutes |
| Largest gap between consecutive ticks | 11 seconds (nominal 10) |
| Ticks before / after the 60-minute mark | 360 / 90 |
| The pair straddling the boundary | `1786817864 -> 1786817874`, a gap of **10 seconds** |

The straddling pair is the measurement. A token rotation that disturbed the exec would
show up as an outlier gap there, and the gap is nominal — indistinguishable from every
other tick. Nothing in the guest observed the boundary, which follows from where the state
lives: the exec record is in the daemon, keyed by exec id, and the proxy token is a
property of the *caller's* connection. Tokens were re-minted naturally throughout, once per
`microvm` invocation, roughly every eight minutes across the whole run.

**Two conditions this depended on, both worth stating because a caller can get either
wrong.** The VM was launched with `maxIdleDurationSeconds: 1800` and polled from outside
every eight minutes; without that traffic it would have suspended at the idle window
regardless of how healthy the exec was (see the idle-timer entry above). And the exec's
output was only readable at the end: a poll against a *running* exec returns
`{"exec_id":"...","phase":"running"}` with **no partial stdout**, verified directly against
the daemon's route. A caller wanting mid-flight visibility into a long exec must either
stream it or have the command write to a file and fetch that file, which is what this probe
did to observe progress while it ran.

Cost: 75 minutes of a 2 GB / 1 vCPU baseline is $0.1576 of compute at the recorded
us-east-1 rates ($0.0000276944 per vCPU-second and $0.0000036667 per GB-second, so
4500 vCPU-seconds plus 9000 GB-seconds), plus $0.0031 for one 2 GB snapshot read: about
**$0.16**. Wall-clock time is the whole cost of this measurement, and it cannot be
shortened, because the thing being measured is a one-hour boundary.
