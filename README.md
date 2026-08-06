# microvms-agentd

An exec-and-file-transfer daemon for AWS Lambda MicroVMs, and the verification
harness that keeps it honest.

The service gives you an isolated Firecracker VM and no way to run anything in
it: there is no exec API and no file-transfer API. Every harness that wraps
Lambda MicroVMs therefore writes an in-VM daemon to supply both, and the first
one we wrote — 787 lines of Python inside Harbor PR #2469 — accumulated 28 review
findings across six rounds, nearly all of them in that daemon or in the service's
lifecycle semantics. This project exists so the next team does not repeat it.

**Status: proven against the real service.** Most recently on 2026-08-06 the daemon was
cross-compiled to a 1.41 MB static `aarch64-unknown-linux-musl` binary, baked into
a MicroVM image as the container `CMD`, launched in us-east-1, and driven through
every protocol rule via the platform's own endpoint: 56 checks passed, none
failed, and teardown left the account clean. 155 Rust tests across six local
tiers and 83 Python client tests are green alongside it.

That live run covers the things only the real service can answer, including
Server-Sent Events surviving the endpoint proxy, stdin round-tripping through a
child, and a suspend/resume cycle preserving everything.

Three rounds of live runs found six defects no local tier could have caught, all of
them wrong assumptions about the service rather than bugs in the daemon's logic:
the lifecycle hooks live under a fixed `/aws/lambda-microvms/runtime/v1/` prefix,
two of them (`ready`, `validate`) are called at image-build time, `runHookPayload`
arrives wrapped in an envelope rather than as the request body, network connectors
are ARNs, `CreateMicrovmAuthToken` returns a header map, and a guest running as
root still needs `additionalOsCapabilities: ["ALL"]` before `sethostname` or a
bind mount over `boot_id` will work. Each is recorded in
`docs/PLATFORM.md` with its date, and the transport tier was corrected so it fails
against the old behavior.

Not yet done: a repeat run in a second region, the CI cross-compile job has never
executed (there is no git remote), and `Sandbox` in the Python client is verified
only against fakes plus the conformance run.

## Suspend and resume preserve everything

Measured 2026-08-05, and it inverted what this project previously assumed.
`SuspendMicrovm` then `ResumeMicrovm` is a freeze and restore: the in-memory agent
token, the filesystem, exec records including unacked output, and even running
background processes all survive, and the endpoint URL is unchanged. The evidence
is a ticker writing epoch seconds once a second, showing a single 46-second gap
across a 40-second suspension and then resuming its count.

So a warm pool of suspended sandboxes needs no token re-delivery and no
re-bootstrap: suspend an idle VM instead of terminating it, and the next task lands
in one that still has its filesystem, installed tools, and credentials. The one
caveat is that a guest process observes the suspension as a single jump in wall
time, so anything holding a timeout, lease, or session across a suspend sees it
expire at once.

## What is in the repo

```
agentd/   the daemon: axum router, one-shot bootstrap, exec engine, fs engine,
          disk-pressure guard, identity repair, generated schema
          tests/panic_guard.rs       — a panic must not strand the VM
          tests/proptest_tar.rs      — tar confinement properties
          tests/schema_artifact.rs   — the published schema cannot go stale
          tests/turmoil_transport.rs — deterministic transport faults
clients/  python/ — session library: proxy-token refresh, streaming with
          offset reconnect, typed error taxonomy, AWS lifecycle helper
conformance/  live AWS suite plus the suspend/resume probe, and its Terraform
model/    stateright model of bootstrap + exec lifecycle, with safety properties
spec/     symspec requirements document, Z3-verified consistent
docs/     PROTOCOL.md (wire contract), PLATFORM.md (measured AWS behavior),
          TRUST.md (threat model and identity repair), STRATEGY.md, schema.json
```

### `agentd/` — the daemon

A static binary intended to run as the container `CMD`. With the workspace's
size-tuned profile the release build is 1.41 MB for
`aarch64-unknown-linux-musl` — the shipping target, since MicroVMs are ARM64
only — and 1.98 MB on x86-64.

The module seams follow the defect classes rather than the HTTP surface:
`state.rs` owns the one-shot bootstrap, `auth.rs` decides authorization before any
body byte is read, `exec.rs` owns idempotent exec with pipe-based output capture,
`fs.rs` owns streaming tar with CPython `data`-filter parity, and `serve.rs` is
generic over the listener so the transport tier can substitute a simulated network
without touching production code.

### `model/` — the state machine, checked

`cargo test -p agentd-model` enumerates every reachable state of the bootstrap and
exec lifecycle and checks seven safety properties plus six coverage properties.
The safety properties are the claims that were argued in prose during the PR:

- an in-VM attacker never obtains authority over the control API,
- bootstrap is one-shot, so a losing racer never replaces the winner's token,
- only the installed token is ever accepted,
- the control API stays closed until bootstrap completes,
- exec output is never released before the caller acks it,
- a retried `/exec/start` never spawns a second child,
- there is at most one exec entry per caller-minted id.

The coverage properties exist because a safety property can pass over a state
space that never reached the interesting states. That failure mode is not
hypothetical: in the PR, a test named
`test_create_token_is_not_a_permanent_key` passed against broken code because it
varied an input that nothing varies in reality.

The model also runs a second configuration where the deployment invariant is
broken — a base image that starts its own process before bootstrap — and asserts
that stateright *finds* the attack path. That prices the invariant instead of
asserting it is fine.

### `spec/` — the requirements, proved consistent

`spec/agentd.symspec.json` holds the bootstrap and authorization requirements in
EARS form, checked by [symspec](https://github.com/theagenticguy/symspec), which
hands them to Z3. It currently reports `verified: true`, which that tool treats as
an earned claim rather than the absence of findings: every requirement shares
vocabulary with a peer, every opposition candidate is triaged, and a
cross-requirement comparison actually ran.

Getting there required one committed glossary link, recorded in the document:
"install the agent token" and "accept the bootstrap request" name one action, so
the solver compares them instead of treating them as unrelated. That is the
neurosymbolic split working as intended — a local model proposes the synonym, a
human commits it, and only then does the sound layer decide.

### `docs/PLATFORM.md` — what we measured, with dates

Observations of someone else's system, each carrying its date, region, and API
version. The load-bearing ones:

- The platform's own `/run` hook arrives from `127.0.0.1`, indistinguishable at
  the socket level from an in-VM process. A source-address rule on that route is
  wrong, not merely unverified, and would break every launch.
- A `clientToken` derived from stable content is a *permanent* idempotency key.
  Deleting and recreating an image under the same name replays the original create
  as a no-op, wedging the image in `CREATING` where it cannot be deleted.
- Build logs go to `/aws/lambda-microvms/<image-name>`. The wrong prefix in an IAM
  policy produces builds with no logs, and every failure then reports
  `reason=unknown`.

## Verification stack

Five tiers, each owning a defect class the others cannot see:

| Tier | Tool | Owns |
| --- | --- | --- |
| Requirements | symspec + Z3 | contradictions between stated requirements |
| Design | stateright | unsafe interleavings of hooks, client, and attacker |
| Wire contract | schemars + JSON Schema | client/server drift on request and response shapes; `docs/schema.json` is generated and CI fails if it goes stale |
| Transport | turmoil | deterministic network and time faults: framing, mid-body disconnect, token expiry mid-exec |
| Filesystem | proptest | tar extraction confinement and stdlib `data`-filter parity |

Live conformance against real AWS remains the ground truth anchor, because the
model is not the binary. The bridge is a conformance suite that pins the real
daemon to the same properties the model proves.

## Why Rust

The Python predecessor was constrained by its deployment: a single file baked into
a harness layer, running under whatever `python3` the task's base image happened
to provide, which forced compatibility work back to 3.8. A static musl binary
deletes that constraint instead of accommodating it, and it moves HTTP/1.1 framing
into hyper, which is where roughly a quarter of the PR's defects lived.

## Running the checks

```bash
cargo test --all                                 # all four tiers, ~11s
cargo clippy --all-targets -- -D warnings
symspec check spec/agentd.symspec.json --strict   # requirements verification
```

`cargo test --all` runs 155 tests across six targets: 107 daemon unit tests, 5
panic-containment tests, 8 tar-confinement properties, 11 schema-artifact checks,
18 transport-fault simulations, and 6 model checks. Every guard in the suite was
verified to fail against the code without its fix; a property that passes either
way is a false answer, not a passing test.

CI (`.github/workflows/ci.yml`) runs the Rust gates, the schema staleness check,
the symspec requirements gate, and an `aarch64-unknown-linux-musl` cross-compile.
It does **not** yet run the Python client's tests or ruff — run those locally per
`CONTRIBUTING.md` until a job exists. CI has also never executed, because the
repository has no remote.

## License

Apache-2.0.
