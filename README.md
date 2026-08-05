# microvms-agentd

An exec-and-file-transfer daemon for AWS Lambda MicroVMs, and the verification
harness that keeps it honest.

The service gives you an isolated Firecracker VM and no way to run anything in
it: there is no exec API and no file-transfer API. Every harness that wraps
Lambda MicroVMs therefore writes an in-VM daemon to supply both, and the first
one we wrote — 787 lines of Python inside Harbor PR #2469 — accumulated 28 review
findings across six rounds, nearly all of them in that daemon or in the service's
lifecycle semantics. This project exists so the next team does not repeat it.

**Status: verification harness first, daemon to follow.** What is here now is the
part that is cheapest to get wrong later: a model-checked specification of the
bootstrap and exec lifecycle, a machine-verified requirements document, and the
measured platform facts that constrain any implementation. The daemon itself is
specified in `docs/PROTOCOL.md` and not yet written.

## What is in the repo

```
model/    stateright model of bootstrap + exec lifecycle, with safety properties
spec/     symspec requirements document, Z3-verified consistent
docs/     PROTOCOL.md (wire contract) and PLATFORM.md (measured AWS behavior)
```

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
| Wire contract | schemars + OpenAPI | client/server drift on request and response shapes |
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
cargo test -p agentd-model                    # model checking
symspec check spec/agentd.symspec.json --strict  # requirements verification
```

Both are also wired into CI (`.github/workflows/ci.yml`).

## License

Apache-2.0.
