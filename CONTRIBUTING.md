# Contributing

This project is a reference contract for running an untrusted workload inside an
AWS Lambda MicroVM, not a product. Its value is that every claim in it has been
checked, so the review bar is about evidence rather than taste. Read
`docs/STRATEGY.md` for what the project is for, `docs/PROTOCOL.md` for the wire
contract you must not silently change, and `docs/TRUST.md` for the threat model.

## Running the verification tiers

## The command surface

`mise` is the front door. One command is the definition of done:

```bash
mise run install   # once per clone: installs the git hooks
mise run check     # every local gate. ~45s, no network, no AWS, no cost.
mise run live      # the real-AWS suites. BILLABLE, ~15 min. Deliberate only.
mise tasks         # everything else
```

`check` is what the pre-push hook runs, and what CI runs. `live` is never wired
to a hook: it launches real MicroVMs, and a gate that spends money on every push
is a gate people disable with `--no-verify`, which then also skips the checks
worth having. The hook does print an advisory when the daemon has changed since
the last recorded live run, because no local tier can see a platform change —
every AWS-facing defect this project has hit was invisible until a real run.

The tiers below are what `check` composes, and each is runnable alone when you
want a tighter loop.

Five tiers, each owning a defect class the others cannot see. `cargo test --all`
runs all of them; run them individually while iterating.

```bash
cargo test -p agentd --lib             # daemon unit tests
cargo test -p agentd-model             # stateright: every reachable bootstrap/exec state
cargo test --test proptest_tar         # tar confinement + CPython data-filter parity
cargo test --test turmoil_transport    # deterministic network and time faults
cargo test --test panic_guard          # a panicking handler must not kill the daemon
cargo test --test schema_artifact      # docs/schema.json matches the served protocol
cargo test --all                       # all of the above
```

Then the gates that are not `cargo test`:

```bash
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo run -p agentd --bin schema -- --check   # regenerate with the same command minus --check
cargo build --release -p agentd --target aarch64-unknown-linux-musl
```

`aarch64-unknown-linux-musl` is the shipping target and CI builds it on every
push. `.cargo/config.toml` pins `rust-lld`, so `rustup target add
aarch64-unknown-linux-musl` is the only setup step — you do not need an external
cross toolchain.

Python client:

```bash
uv run --with pytest --with httpx --with boto3 pytest clients/python -q
uvx ruff check clients/python && uvx ruff format --check clients/python
```

Requirements verification (Node ≥ 22):

```bash
npm install -g symspec
symspec download-model                        # sha256-pinned local model, then offline
symspec check spec/agentd.symspec.json --strict
```

`--strict` fails when the document could not be verified, not only when something
was proven wrong. If you add a requirement whose wording shares vocabulary with no
peer, symspec says so; the fix is usually a glossary link committed in the
document, not a looser gate.

CI (`.github/workflows/ci.yml`) runs fmt, clippy, `cargo test --all`, the schema
staleness check, the aarch64-musl cross-compile, and symspec strict. It does not
run the Python tests or ruff, so run those yourself before opening a PR.

## Prove your guard fires

**A new test that guards an invariant is not accepted until you have watched it
fail.** Break the invariant deliberately in the code under test, confirm that
*your specific test* fails, restore the code, confirm green, and say in the PR
which break each new test caught.

This is not ceremony. Two tests in this repo's own history passed against broken
code:

- A tar property asserted only that nothing landed outside the extraction root.
  Removing the `?` from `parts.pop()?` turned `../x` into `x` — a member the
  contract says to refuse, extracted under a different name — and the whole
  proptest suite stayed green, because a filesystem walk cannot see a rewrite
  that lands inside the root. The properties now assert the expected *verdict*
  computed from the generated input, and the same break fails immediately.
- In the Python predecessor, `test_create_token_is_not_a_permanent_key` passed
  against broken code because it varied an input that nothing varies in reality.
  The real behavior — a content-derived `clientToken` is a permanent idempotency
  key that wedges an image in `CREATING` — cost roughly 15 hours of two wedged
  images to discover.

The same shape appears in the model tier: a property written as
`cfg.attacker_allowed || !state.breached` is vacuously true in exactly the
configuration where it should fail. State safety properties unconditionally and
let configurations differ. The model deliberately runs one configuration with the
deployment invariant broken and asserts stateright *finds* the attack path, which
prices the invariant instead of asserting it is fine.

One more trap, if you touch `turmoil_transport`: a simulator has two clocks.
Everything inside the simulation runs on virtual time; a spawned child does not.
`sleep 2` in `/bin/sh` is two real seconds while the simulation may advance
thirty virtual ones, and a server-side deadline measured in virtual time against
real pipe I/O expires in milliseconds of wall clock. Never pace a child with
`sleep` — block it on `read` and release it with an explicit stdin write, so the
harness is the clock. Do not loosen an assertion to match a simulator artifact;
that encodes the artifact as expected behavior.

## The live conformance suite

`conformance/run.py` is the ground truth anchor, because the model is not the
binary. It is also the only part of this repo that spends money.

```bash
terraform -chdir=conformance/infra init
terraform -chdir=conformance/infra apply
cargo build --release -p agentd --target aarch64-unknown-linux-musl
conformance/run.py --binary target/aarch64-unknown-linux-musl/release/agentd
conformance/probe_suspend_resume.py --binary <same path>   # the platform probe
```

It needs real AWS credentials in a Lambda MicroVMs region, and it creates real
resources: an S3 artifact, a MicroVM image build (up to a 45-minute timeout), and
a running MicroVM. A run costs money whether it passes or fails. `--keep` skips
teardown and leaks everything, so use it only while debugging a failure you
cannot reproduce otherwise.

**Verify teardown independently. Do not trust a success message.** The scripts
delete the MicroVM, the image, and the log group in `finally`, and
`terraform destroy` handles the stack — but the service creates
`/aws/lambda-microvms/<image-name>` itself, so Terraform never owns it and
`destroy` reports success while the group survives. Six leaked that way before
anyone noticed. After a run, list MicroVMs, images, and log groups under
`/aws/lambda-microvms/` and confirm nothing tagged `agentd:purpose=conformance`
remains.

## Platform claims need a date, a region, and an API version

`docs/PLATFORM.md` records observations of someone else's system, and those drift.
Every entry carries when it was measured, in which region, and under which API
version, and says explicitly whether it is our measurement or AWS documentation.
A claim without those three is not usable by the next reader, who cannot tell
whether it is still true. If you contradict an existing entry, do not delete it —
add your measurement with its date so the drift is visible.

## Comments

Comments explain constraints and why a choice was made, especially where the
obvious alternative is wrong. They never narrate what the next line does. If a
comment would be invalidated by a rename, delete it. The best examples in the
tree are the ones that record a defect the code is defending against: the
`panic = "unwind"` note in `Cargo.toml`, or the loopback entry in
`docs/PLATFORM.md` explaining why a source-address rule is wrong rather than
merely weak.

## Commit messages

Explain why, name what was measured, and admit what is unverified. See
`git log` — every commit here states its evidence and its gaps.

## What this project will not accept

Per `docs/STRATEGY.md`, these are declined regardless of quality:

- **An orchestrator.** Scheduling, pooling, and task routing belong to the
  consumer.
- **A fork or process-tree snapshot implementation.** Unavailable above the
  hypervisor; a measurement of the best guest-side approximation is welcome, an
  implementation is not.
- **AgentCore parity on exec or PTY.** Taken ground.

A turn-boundary suspend protocol is also out: `idlePolicy` already auto-suspends,
so wiring an existing hook to a suspend call is a short example, not a feature
here.

## Reporting security issues

See `SECURITY.md`. Do not open a public issue for a vulnerability.
