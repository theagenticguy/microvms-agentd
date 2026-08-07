---
slug: control-plane-client
sequence: 001
sources:
  - docs/PLATFORM.md            # the trap list; every AC cites a section name
  - clients/python/src/microvms_agentd/sandbox.py    # what already exists
  - clients/python/src/microvms_agentd/transport.py  # proxy auth, header map
  - docs/PROTOCOL.md, docs/TRUST.md                 # the in-VM boundary
  - spec/agentd.symspec.json                        # requirement vocabulary
---

**Status:** COMPLETE

<write_protocol>
Your output file is the single source of truth for your work. Edit it after every meaningful step, before starting the next one. Partial progress written to disk survives timeouts, SendMessage interrupts, and orchestrator context pressure; state held in working memory does not.

The rhythm is: one unit of thought → edit the file with the outcome → next unit. One decision at a time.

Work through your sections in numbered order. For each section:

1. Think through the decision, research finding, or draft. Read adjacent files, run a web search, or consult the framework reference when the answer is not in your head.
2. Edit the file under that section — the claim, the evidence, the user story or HMW or spec statement. Cite sources inline.
3. If the section needs more depth, do another unit of thought and edit again.
4. Move to the next section only after the current one has real content.

Name the tradeoff on every non-obvious call. "Chose JTBD job story over user story for the top-level framing because the goal is reframing around progress, not stakeholder persona" beats "used job story." The synthesizer reads these attributions when composing the final artifact.

Cite adjacent material inline when a decision depends on source evidence — framework file + heading, research synthesis line number, interview quote, or external URL. Reviewers read the citations to verify your reasoning.

Match section length to information density — a settled decision is one sentence; do not pad.

When every section has real content, change the `Status:` line at the top of the file from `IN PROGRESS` to `COMPLETE`.
</write_protocol>

---

## What "unreachable by construction" means here

The mission claim is that the platform's measured traps become unreachable rather
than merely documented. That is three different strengths of claim, and they are
not equal. Every AC below is labelled with the one it achieves.

- **S1 — inexpressible.** The mistake cannot be written down. There is no
  parameter that accepts the wrong value, no string where an ARN belongs, no
  boolean where the API takes one enum. The compiler or the type checker is the
  guard. **This is the goal wherever the API surface allows it.**
- **S2 — expressible, rejected.** The mistake can be written down but the client
  rejects it locally, before any control-plane call, with an error naming the
  `docs/PLATFORM.md` finding. Weaker than S1 because the guard is code that can
  regress, but it still costs the caller seconds instead of a build cycle.
- **S3 — correct by default, overridable.** The default is right and the caller
  can opt out. Weakest: it protects the caller who does nothing and abandons the
  caller who reaches for the override. An S3 AC must say what the override
  costs.

Ranking: S1 > S2 > S3. An AC that can only reach S3 says so and explains why S1
was unavailable — usually because the API takes a free-form value the client
cannot validate offline.

Two further rules apply to every AC.

**A guard that cannot fail is worse than no guard.** Two tests in this
repository once passed against broken code (`docs/PLATFORM.md`,
"Root in the guest is not enough" — the unit tiers injected a fake layout and a
fake platform, so the capability gap was invisible until a live run). Every AC
below therefore carries a *falsification* line: the specific edit to the
implementation that must turn the guard red. An AC whose falsification line is
"remove the feature and the test fails" is not good enough; the break must be
the *plausible* regression, not the absence of the code.

**A test may not assert against a fake that shares the client's own
assumptions.** The fake control plane is a contract recorder — it asserts on
the request the client emitted, in the shape AWS measured, not on values the
client also computed.

## Scope

In scope: everything the client sends to, or reads from, the `lambda-microvms`
control plane and the endpoint proxy — image creation, build waiting, launch,
state polling, proxy-token minting, suspend/resume/terminate, teardown — plus
the CLI over that library.

Out of scope, structurally. A control-plane client sits outside the VM and
cannot influence what the daemon does with a request that has already arrived,
so these two measured findings get no AC and must not be papered over with one:

- **"The platform's own hook arrives over loopback."** The finding is that a
  source-address rule rejecting `127.0.0.1` on the bootstrap route is *actively
  wrong*, not weak — it would reject the platform's own bootstrap. The remedy is
  the daemon's one-shot bootstrap plus the `model/` checks (`docs/TRUST.md`,
  "Why source-address filtering is wrong"). No client-side AC exists, and the
  client must not attempt to compensate.
- **"Something probes the port with TLS before bootstrap."** Raw TLS
  ClientHello bytes reaching the daemon's plaintext port. The required behavior
  — 400, debug log, listener stays up — is entirely in-VM.

Also out of scope: the `runHookPayload` *unwrapping* (the daemon's parse), hook
route *prefixes* (`/aws/lambda-microvms/runtime/v1/<hook>`, the daemon's
router), and the unenforced `CMD` invariant (`docs/PROTOCOL.md`, "Trust
boundary"). The client owns the *config* side of each; the daemon owns the
*serving* side. Where a finding splits that way the AC below names only the
client half.

## The real trap count

`docs/PLATFORM.md` carries seventeen findings. Two are purely in-VM (above).
**Fifteen are client-actionable, not eleven** — correcting the record. Of the
fifteen, thirteen are traps proper; two ("Suspend/resume is a freeze and
restore", "Traffic ordering around the `/run` hook") are enabling measurements
whose sharp edges are still traps a client can close, so they get ACs too.

Nine of the fifteen are **already satisfied** by `sandbox.py` / `transport.py`
today and appear here as requirements the code must keep satisfying — a spec
that pretends nothing exists is a rewrite plan. Six are new or only partly
closed. Marked per AC.

---

## User Story 1 — Build an image without wedging it or under-privileging it

### Acceptance Criteria

**AC-1-1** `[P]` — **S1** — new work
Ubiquitous: The client shall derive every image create token from a value that
is unique per build attempt.
Derives from: "`clientToken` is a permanent idempotency key".
Today: partly satisfied — `sandbox.py:256` folds in `secrets.token_hex(4)`, but
the `client_token=` parameter lets a caller pass a content digest and
reintroduce the wedge. S1 requires removing the caller-supplied override from
the create path so the mistake is inexpressible.
Falsification: replace the per-attempt nonce with a name-derived digest; a test
that creates, deletes, and re-creates an image of the same name against the fake
control plane must observe two distinct tokens and go red on one.

**AC-1-2** `[P]` — **S2** — new work
Unwanted behavior: If an image build remains in `CREATING` past the stall grace
period with every build still `PENDING`, then the client shall reject the build
wait with an error naming the client-token replay signature.
Derives from: "`clientToken` is a permanent idempotency key".
Today: satisfied — `_probe_stalled_build` at `sandbox.py:289`. Kept as a
requirement because it is the only signal that separates a wedged image from a
slow one, and two images were wedged ~15 hours without it.
Falsification: make `_probe_stalled_build` swallow the all-`PENDING` case (its
`except Exception` already swallows everything else); the test must go red
rather than waiting out the full build timeout.

**AC-1-3** `[P]` — **S1** — new work
Optional feature: Where guest identity repair is requested, the client shall set
the additional OS capabilities field to the single value the `2025-09-09` API
accepts.
Derives from: "Root in the guest is not enough: `sethostname` and bind mounts
need `additionalOsCapabilities`".
Today: **contradicted.** `os_capabilities: list[str] | None` accepts any list,
so `["CAP_SYS_ADMIN"]` is expressible and is rejected only by AWS, after an
upload. S1 replaces the list with a boolean intent flag naming what the caller
wants (hostname and `boot_id` repair), not the capability that grants it.
Falsification: pass a capability list other than the accepted value; a type
check must fail. Plus a live-tier assertion that the probe reports
`identity_degraded: false` — the unit tiers cannot see this gap, which is
exactly how it was missed.

**AC-1-4** `[P]` — **S3** — new work
Ubiquitous: The client shall emit the build log group name under the
`/aws/lambda-microvms/<image-name>` prefix and shall delete it during teardown
when asked.
Derives from: "The build log group survives Terraform" and "Build logs go to
`/aws/lambda-microvms/<image-name>`".
Today: satisfied — `Image.build_log_group` and `delete_build_log_group`. S3
because deletion is opt-in (`delete_log_group=False`); the override costs a
storage-only leak that `terraform destroy` will not catch.
Falsification: change the prefix to `/aws/lambda/microvms/`; a test asserting
the emitted name against the measured literal must go red.

**AC-1-5**
Dependencies: AC-1-4 — **S2** — new work
Unwanted behavior: If a build fails and its log group contains no events, then
the client shall emit an error naming the build role's required log-group prefix
rather than reporting the failure reason as unknown.
Derives from: "Build logs go to `/aws/lambda-microvms/<image-name>`" — a wrong
IAM prefix produces builds with no logs, and every failure then reads
`reason=unknown`, which looks like the service failing to populate
`stateReason`.
Today: **new.** Nothing in `sandbox.py` distinguishes an empty log group from a
silent service.
Falsification: give the fake logs client an empty log group on a failed build;
an implementation that forwards `stateReason` verbatim must produce a message
without the prefix, and the test must go red.

**AC-1-6** `[P]` — **S2** — new work
Unwanted behavior: If the caller relies on working-directory inheritance and the
selected base image declares no `WorkingDir`, then the client shall reject the
image build before any upload.
Derives from: "Most public ARM64 base images have no WORKDIR" —
`al2023-minimal`, `python:3.12-slim`, and `node:20-slim` all leave it empty.
Today: partly satisfied — `default_dockerfile(workdir=...)` can set one, but
nothing checks the base. Note a second defect: `default_dockerfile` hardcodes
`FROM public.ecr.aws/amazonlinux/amazonlinux:2023-minimal` while
`DEFAULT_BASE_IMAGE` is `al2023-1`, so the two can disagree silently.
Falsification: request inheritance against a base with an empty `WorkingDir`; an
implementation that only warns must fail the test.

**AC-1-7** `[P]` — **S3** — already satisfied
Ubiquitous: The client shall enable all six lifecycle hooks with an explicit
timeout on every image it creates.
Derives from: "Hooks are served under a fixed prefix, and two of them are
build-time". Client half only — the client owns the hook *config*; the daemon
owns serving the fixed prefix and answering `ready`/`validate` 200 without
regard to bootstrap state.
Today: satisfied — `default_hooks` at `sandbox.py:113`.
Falsification: drop `ready` from the enabled set; a test asserting the recorded
`hooks` map against all six names must go red. This is S3, not S2 — a caller
passing `hooks=` can still ship a partial map.

## User Story 2 — Launch a MicroVM and reach RUNNING, or learn why not

### Acceptance Criteria

**AC-2-1** `[P]` — **S1** — already satisfied
Ubiquitous: The client shall derive every network connector value as a
fully-qualified connector ARN for the request region.
Derives from: "Network connectors are ARNs" — the bare string `ALL_INGRESS` is
rejected with `Malformed network connector ARN`.
Today: satisfied — `ingress_connector_arn` at `sandbox.py:53`. S1 requires that
the launch surface accept no free-form connector string at all, only the
enumerated ingress/egress intents.
Falsification: pass the bare name; a type check must fail. Plus a recorded-
request assertion against the measured ARN literal, which goes red if the
region interpolation breaks.

**AC-2-2** `[P]` — **S3** — already satisfied
Optional feature: Where outbound network access is requested, the client shall
set the egress connector; otherwise the client shall omit the egress field.
Derives from: "Network connectors are ARNs" — omitting egress entirely is how
you get a VM with no outbound network.
Today: satisfied — `egress: bool = False` at `sandbox.py:360`. S3 and
deliberately so: no outbound network is the correct default for a daemon that
needs none, but the cost of the default is that a task workload needing the
internet fails in a way that looks like a broken image.
Falsification: default `egress` to true; a test asserting no egress field in the
recorded request must go red.

**AC-2-3** `[P]` — **S1** — already satisfied
Ubiquitous: The client shall deliver the agent token to the VM as a
JSON-serialized run-hook payload and shall not place the agent token in the
image build artifact.
Derives from: "Traffic ordering around the `/run` hook" (no external traffic is
forwarded until `/run` returns 200, which is what makes launch-time delivery
safe) and "`runHookPayload` arrives wrapped, not as the body" (the payload is a
string the platform wraps).
Today: satisfied — `json.dumps({"agent_token": token})` at `sandbox.py:357`;
`build_artifact` never sees the token.
Falsification: bake the token into the Dockerfile as an `ENV`. A test that
scans the artifact zip bytes for the token must go red. That byte scan is the
guard — asserting only that `runHookPayload` is present would still pass.

**AC-2-4**
Dependencies: AC-2-3 — **S2** — already satisfied
Unwanted behavior: If a MicroVM reaches any terminal state before `RUNNING`,
then the client shall reject the launch with the state and `stateReason`
attached.
Derives from: "MicroVM states, and the one that matters", and the failure mode
in "`runHookPayload` arrives wrapped" — the platform terminates the VM on a 400
from the run hook before forwarding any traffic, so the failure is invisible
from outside and the VM is gone before you can look inside. `stateReason` is
the only evidence left.
Today: satisfied — `_wait_for_running` at `sandbox.py:379`, `TERMINAL_STATES` at
`sandbox.py:37`.
Falsification: remove the terminal-state branch so the loop polls to timeout;
the test must go red on the *message*, asserting that it carries `stateReason`
and not merely that an exception was raised — a timeout also raises.

## User Story 3 — Reach the daemon through the endpoint proxy

### Acceptance Criteria

**AC-3-1** `[P]` — **S1** — already satisfied
Ubiquitous: The client shall read the minted proxy token from the auth-token
header map and shall send both the proxy auth header and the proxy port header
on every endpoint request.
Derives from: "`CreateMicrovmAuthToken` returns a header map" and "Endpoint
authentication". Omitting the port header is a rejection that reads like a bad
token.
Today: satisfied — `transport.py:97` reads `authToken["X-aws-proxy-auth"]`;
`ProxyAuth.headers` emits both.
Falsification: drop the port header from `ProxyAuth.headers`; a test asserting
both header names on a recorded request must go red. Treating `authToken` as a
string must raise a type error against a fake that returns the measured map
shape.

**AC-3-2**
Dependencies: AC-3-1 — **S3** — already satisfied
Ubiquitous: The client shall mint a proxy token inside the request path at an
interval strictly below the sixty-minute service ceiling.
Derives from: "Endpoint authentication" — the 60-minute ceiling means a
long-running trial mints mid-flight, so minting sits inside the retry path.
Today: satisfied — `DEFAULT_REFRESH_AFTER_SEC = 30 * 60` at `transport.py:30`,
half the ceiling so a request in flight still holds ~30 minutes.
Falsification: set the refresh interval to the ceiling; a clock-driven test that
advances past it mid-request must go red. S3 because
`refresh_after_sec` is caller-overridable.

**AC-3-3**
Dependencies: AC-3-1 — **S2** — already satisfied
Unwanted behavior: If minting a proxy token fails, then the client shall reject
the request with a retryable error.
Derives from: "Endpoint authentication" — boto/HTTP errors from minting must be
handled wherever a request can be retried.
Today: satisfied — `AuthTokenMintError` at `transport.py:99`, honored by
`wait_until_ready`'s `exc.retryable` check.
Falsification: mark the mint error non-retryable; a test that throttles the
first mint and expects the second to succeed must go red.

**AC-3-4** `[P]` — **S1** — already satisfied
Ubiquitous: The client shall expose command execution and file transfer only
through the daemon's control API.
Derives from: "The service provides no exec and no file transfer" —
`CreateMicrovmShellAuthToken` exists, requires a `SHELL_INGRESS` connector,
runs through a console terminal or WebSocket, is documented as debugging-only
and recommended disabled in production. The name suggests a programmatic exec
path that it is not.
Today: satisfied by absence — nothing in the client calls it.
Falsification: a test asserts the fake control plane recorded zero calls to
`CreateMicrovmShellAuthToken` and zero `SHELL_INGRESS` connectors across the
full lifecycle; adding such a call must turn it red. Absence-of-call is a weak
guard, which is why it is paired with the connector assertion rather than left
as a docstring.

## User Story 4 — Suspend and resume, including the windows that close

### Acceptance Criteria

**AC-4-1** `[P]` — **S1** — already satisfied
Event-driven: When a suspended MicroVM is resumed, the client shall reuse the
existing agent token and shall not re-deliver a run-hook payload.
Derives from: "Suspend/resume is a freeze and restore, not a stop and start" —
the in-memory token, the filesystem, exec records including unacked output, a
backgrounded process, and the endpoint URL all survived a 45-second suspension.
This finding *corrects* an earlier claim in the daemon's own resume-hook
docstring, which reasoned from where state lives rather than from a measurement.
Today: satisfied — `Sandbox.resume` at `sandbox.py:418` re-delivers nothing.
Falsification: make `resume` mint a new agent token; a test that resumes and
then issues a control request with the pre-suspend token must go red.

**AC-4-2**
Dependencies: AC-4-1 — **S2** — already satisfied
Event-driven: When a suspended MicroVM is resumed, the client shall invalidate
the cached proxy token.
Derives from: "Suspend/resume is a freeze and restore" — the endpoint URL is
unchanged, but a token minted against the pre-suspend instance is not
guaranteed to validate, and that rejection reads exactly like a dead daemon.
Today: satisfied — `Session.rebind` → `ProxyAuth.invalidate`.
Falsification: remove the `invalidate()` call; a test asserting
`ProxyAuth.mint_count` increased across a resume must go red. Asserting the
resume merely succeeded would still pass, because the cached token usually
works.

**AC-4-3**
Dependencies: AC-4-1 — **S2** — new work
Unwanted behavior: If a resume is attempted after the launch-time suspended
duration has elapsed, then the client shall reject the resume with an error
naming the elapsed suspended window.
Derives from: "`idlePolicy`" — the launch-time policy *terminates* a suspended
VM after `suspendedDurationSeconds`, so a "resume later" affordance silently
stops working once that window passes.
Today: **contradicted.** `sandbox.py:429` waits only for `RUNNING`, and
`_wait_for_state` has no terminal-state branch — so a VM the platform already
terminated burns the full 300-second timeout and then reports "never reached
RUNNING", which is the connection-error-hiding-the-cause failure AC-2-4 exists
to prevent, reintroduced on the resume path.
Falsification: have the fake control plane report `TERMINATED` on resume; an
implementation without the window check must time out, and the test asserts the
error names the window and returns in well under the timeout.

**AC-4-4** `[P]` — **S3** — already satisfied
Ubiquitous: The client shall tear down without raising, and shall retry image
deletion while the image or a referencing MicroVM blocks it.
Derives from: "`clientToken` is a permanent idempotency key" (an image in
`CREATING` refuses deletion and its only version cannot be dropped) and "The
build log group survives Terraform".
Today: satisfied — `terminate` at `sandbox.py:446` runs in a `finally` and
suppresses, `delete_image` retries 20 times.
Falsification: let `terminate` propagate; a test that raises inside a `with
Sandbox(...)` block must still see the *original* exception, not a teardown
error, and goes red when teardown replaces it.

## User Story 5 — The CLI, a thin layer over the library

### Acceptance Criteria

**AC-5-1** `[P]` — **S2** — new work
Ubiquitous: The CLI shall emit a stable exit code per documented failure class,
distinct from the code for an unexpected error.
Falsification: a table-driven test maps each induced failure (wedged build,
terminal state before `RUNNING`, mint failure, expired suspended window) to its
code. Collapsing any two codes, or letting an unhandled exception surface as a
handled code, must go red.

**AC-5-2** `[P]` — **S2** — new work
Optional feature: Where JSON output is requested, the CLI shall emit exactly one
envelope object per invocation on stdout, carrying an outcome discriminant, a
payload, and — on failure — a machine-readable code and the `docs/PLATFORM.md`
finding name.
Falsification: induce a failure with progress logging enabled; the test parses
stdout as a single JSON document and goes red if any log line, warning, or
partial write reaches stdout. A CLI that writes progress to stdout passes a
"is the envelope present" test and fails this one.

**AC-5-3**
Dependencies: AC-5-2 — **S2** — new work
Ubiquitous: The CLI shall emit a self-describing manifest enumerating every
command, its options, its exit codes, and its JSON envelope schema.
Falsification: a test cross-checks the manifest against the registered command
tree and the exit-code table from AC-5-1. Adding a command or an exit code
without updating the manifest must go red — the manifest is generated from the
same source, so a hand-maintained copy fails the check.

**AC-5-4**
Dependencies: AC-5-1, AC-5-2 — **S2** — new work
Ubiquitous: The CLI shall reach the control plane and the endpoint proxy only
through the library.
Falsification: this is the AC that catches a second implementation. Two guards,
because neither alone is sufficient. (1) A static check asserts no module under
the CLI package imports `boto3`, `botocore`, or `httpx`, or names a
control-plane operation string — a CLI that reimplements must import a
transport. (2) A behavioral check runs each command with the library's
trap-guard entry points patched to raise, and asserts every command fails; a
command that succeeds reached AWS by another path. Guard (1) alone is defeated
by a re-export; guard (2) alone is defeated by a CLI that calls the library and
*then* also calls boto3 directly.

**AC-5-5**
Dependencies: AC-5-4 — **S1** — new work
Ubiquitous: The CLI shall expose no option that permits a value the library
rejects.
Falsification: a test enumerates the manifest's option domains and asserts each
is a closed set for every option whose library counterpart is S1 — the
capability intent (AC-1-3), the connector intent (AC-2-1), the create token
(AC-1-1). Adding a free-text `--os-capabilities` flag that forwards a raw list
must go red, because the CLI is where an S1 library guard is most easily
downgraded to S3 by a convenience string flag.

**AC-5-6**
Dependencies: AC-5-4 — **S2** — new work
Unwanted behavior: If a command is interrupted after a MicroVM has launched,
then the CLI shall tear down the MicroVM and shall emit the identifiers of every
resource it could not delete.
Derives from: "The build log group survives Terraform" and "`clientToken` is a
permanent idempotency key" — a CLI is the surface most likely to be killed
mid-run, and a leaked image in `CREATING` cannot be deleted at all afterward.
Falsification: send the interrupt signal mid-launch against the fake control
plane; the test asserts a terminate call was recorded *and* that any
undeleted identifier appears in the emitted output. An implementation that tears
down silently passes the first assertion and fails the second, which is the
point — a resource the CLI could not delete is one the operator must know the
identifier of.

---

## Coverage ledger

| `docs/PLATFORM.md` section | AC | Status |
| --- | --- | --- |
| The service provides no exec and no file transfer | AC-3-4 | satisfied |
| Hooks are served under a fixed prefix | AC-1-7 | satisfied (client half) |
| `runHookPayload` arrives wrapped | AC-2-3, AC-2-4 | satisfied (client half) |
| Network connectors are ARNs | AC-2-1, AC-2-2 | satisfied |
| `CreateMicrovmAuthToken` returns a header map | AC-3-1 | satisfied |
| MicroVM states, and the one that matters | AC-2-4 | satisfied |
| The build log group survives Terraform | AC-1-4, AC-4-4 | satisfied |
| Root in the guest is not enough | AC-1-3 | **contradicted** |
| Suspend/resume is a freeze and restore | AC-4-1, AC-4-2 | satisfied |
| Traffic ordering around the `/run` hook | AC-2-3 | satisfied |
| The platform's own hook arrives over loopback | — | out of scope (in-VM) |
| Something probes the port with TLS | — | out of scope (in-VM) |
| Endpoint authentication | AC-3-2, AC-3-3 | satisfied |
| `clientToken` is a permanent idempotency key | AC-1-1, AC-1-2, AC-4-4 | partly |
| Build logs go to `/aws/lambda-microvms/<image-name>` | AC-1-4, AC-1-5 | partly |
| `idlePolicy` | AC-4-3 | **contradicted** |
| Most public ARM64 base images have no WORKDIR | AC-1-6 | partly |

## Vocabulary lock (for symspec)

Subjects: `the client` (library), `the CLI`. Never "the library", "the SDK",
"the sandbox".
Verbs, one meaning each, no synonyms: `reject` (refuse an input or an operation
with an error — never "refuse", "deny", "raise"), `accept`, `deliver`, `emit`
(produce output, an error message, or a request field), `set`, `omit`, `derive`,
`invalidate`, `mint`, `enable`, `expose`, `reach`, `read`, `reuse`, `retry`,
`tear down`.
`shall` is the only normative verb.
