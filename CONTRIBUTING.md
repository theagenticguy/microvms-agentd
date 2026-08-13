# Contributing

This project is a reference contract for running an untrusted workload inside an
AWS Lambda MicroVM. It is not a product. Every claim in it has been checked, so
reviews are judged on evidence rather than taste. Read
`docs/STRATEGY.md` for what the project is for, `docs/PROTOCOL.md` for the wire
contract you must not silently change, and `docs/TRUST.md` for the threat model.

## Running the verification tiers

## The command surface

`mise` runs every project command. Your change is ready when `mise run check`
passes:

```bash
mise run install   # once per clone: installs the git hooks
mise run check     # every local gate. ~45s, no network, no AWS, no cost.
mise run live      # the real-AWS suites. BILLABLE, ~15 min. Deliberate only.
mise tasks         # everything else
```

The pre-push hook and CI both run `check`. `live` is never wired to a hook
because it launches real MicroVMs and costs money on every push. People disable
a gate like that with `--no-verify`, and `--no-verify` also skips the checks
worth having. The hook does print an advisory when the daemon has changed since
the last recorded live run. No local tier can see a platform change, and every
AWS-facing defect this project has hit was invisible until a real run.

`check` composes the tiers below, and each tier is runnable alone when you
want a tighter loop.

There are five tiers, and each one catches a defect class the others cannot
see. `cargo test --all` runs all of them; run them individually while
iterating.

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
aarch64-unknown-linux-musl` is the only setup step. You do not need an external
cross toolchain.

Python tooling covers the live suite's driver and the two drift gates. There is
no Python client any more; `clients/python` is in git history, and the clients
are the `microvm` CLI and the `microvms-py` / `microvms-js` bindings.

```bash
uvx ruff check conformance scripts && uvx ruff format --check conformance scripts
./conformance/run_rs.py --self-test    # the live suite's offline half. Free.
./scripts/check-live-rates --twin-only # the pinned rate tables agree. Offline, free.
```

Requirements verification (Node ≥ 22):

```bash
npm install -g symspec
symspec download-model                        # sha256-pinned local model, then offline
symspec check spec/agentd.symspec.json --strict
```

`--strict` fails when the document could not be verified, not only when
something was proven wrong. If you add a requirement whose wording shares
vocabulary with no peer, symspec says so. The usual fix is a glossary link
committed in the document rather than a looser gate.

CI (`.github/workflows/ci.yml`) runs fmt, clippy, `cargo test --all`, the schema
staleness check, the aarch64-musl cross-compile, and symspec strict. It does not
run the Python tests or ruff, so run those yourself before opening a PR.

## Prove your guard fires

**A new test that guards an invariant is not accepted until you have watched it
fail.** Break the invariant deliberately in the code under test, confirm that
*your specific test* fails, restore the code, confirm green, and say in the PR
which break each new test caught.

This requirement exists because two tests in this repo's own history passed
against broken code:

- A tar property asserted only that nothing landed outside the extraction root.
  Removing the `?` from `parts.pop()?` turned `../x` into `x`. That member is
  one the contract says to refuse, but it was extracted under a different name,
  and the whole proptest suite stayed green, because a filesystem walk cannot
  see a rewrite that lands inside the root. The properties now assert the
  expected *verdict* computed from the generated input, and the same break
  fails immediately.
- In the Python predecessor, `test_create_token_is_not_a_permanent_key` passed
  against broken code because it varied an input that nothing varies in reality.
  In reality, a content-derived `clientToken` is a permanent idempotency key
  that wedges an image in `CREATING`. That behavior cost roughly 15 hours and
  two wedged images to discover.

The same shape appears in the model tier. A property written as
`cfg.attacker_allowed || !state.breached` is vacuously true in exactly the
configuration where it should fail. State safety properties unconditionally and
let configurations differ. The model deliberately runs one configuration with
the deployment invariant broken and asserts that stateright *finds* the attack
path. That run demonstrates what breaking the invariant costs instead of
asserting that the invariant is fine.

There is one more trap if you touch `turmoil_transport`. A simulator has two
clocks. Everything inside the simulation runs on virtual time, but a spawned
child does not. `sleep 2` in `/bin/sh` takes two real seconds while the
simulation may advance thirty virtual ones. A server-side deadline measured in
virtual time against real pipe I/O therefore expires in milliseconds of wall
clock. Never pace a child with `sleep`. Instead, block it on `read` and release
it with an explicit stdin write, so the harness controls when the child
proceeds. Do not loosen an assertion to match a simulator artifact, because
that encodes the artifact as expected behavior.

## The live conformance suite

`conformance/run_rs.py` tests the real binary against real AWS, which makes it
the ground truth anchor; the model tier checks a model, not the binary itself.
It is also the only part of this repo that spends money. `mise run live` runs
it alongside the rate-drift check and the leak check. To run it by hand:

```bash
terraform -chdir=conformance/infra init
terraform -chdir=conformance/infra apply
cargo build --release -p agentd --target aarch64-unknown-linux-musl
cargo build --release -p microvms-cli
conformance/run_rs.py \
  --binary target/aarch64-unknown-linux-musl/release/agentd \
  --microvm-binary target/release/microvm
```

`conformance/run.py`, the 56-check Python oracle, and the standalone
suspend/resume probe were here until the Rust port drove this suite green
against real AWS on the same commit. Both are in git history. The
suspend/resume assertions they fed still run inside this suite. The 34
protocol-detail checks that only the Python client could express do not run,
because the `microvm` CLI has no `cp`, `ack`, `exec --stream`, `stdin`, or
`health` subcommand. The suite prints each of those as SKIP with the missing
subcommand named, so a SKIP becomes a PASS the day the CLI grows the
subcommand.

`--self-test` is the offline half. It drives the envelope-to-exception mapping
against a stub `microvm` and touches no account, so it is free and belongs in
any PR that changes `conformance/`.

It needs real AWS credentials in a Lambda MicroVMs region, and it creates real
resources: an S3 artifact, a MicroVM image build (up to a 45-minute timeout), and
a running MicroVM. A run costs money whether it passes or fails. `--keep` skips
teardown and leaks everything, so use it only while debugging a failure you
cannot reproduce otherwise.

**Verify teardown independently. Do not trust a success message.** The scripts
delete the MicroVM, the image, and the log group in `finally`, and
`terraform destroy` handles the stack. However, the service creates
`/aws/lambda-microvms/<image-name>` itself, so Terraform never owns that log
group, and `destroy` reports success while the group survives. Six leaked that
way before anyone noticed. After a run, list MicroVMs, images, and log groups
under `/aws/lambda-microvms/` and confirm nothing tagged
`agentd:purpose=conformance` remains.

## Platform claims need a date, a region, and an API version

`docs/PLATFORM.md` records observations of someone else's system, and those
drift. Every entry carries when it was measured, in which region, and under
which API version, and says explicitly whether it is our measurement or AWS
documentation. Without those three, the next reader cannot tell whether the
claim is still true. If you contradict an existing entry, do not delete it.
Add your measurement with its date so the drift is visible.

## Comments

Comments explain constraints and why a choice was made, especially where the
obvious alternative is wrong. A comment should not restate what the next line
does. If a comment would be invalidated by a rename, delete it. The best
examples in the tree record a defect the code is defending against: the
`panic = "unwind"` note in `Cargo.toml`, or the loopback entry in
`docs/PLATFORM.md` explaining why a source-address rule is wrong rather than
merely weak.

## Commit messages

Explain why, name what was measured, and state what is unverified. See
`git log` for examples; every commit here states its evidence and its gaps.

## What this project will not accept

Per `docs/STRATEGY.md`, these are declined regardless of quality:

- **An orchestrator.** Scheduling, pooling, and task routing belong to the
  consumer.
- **A fork or process-tree snapshot implementation.** This is unavailable above
  the hypervisor. A measurement of the best guest-side approximation is
  welcome, but an implementation is not.
- **AgentCore parity on exec or PTY.** AgentCore already covers this ground.

A turn-boundary suspend protocol is also out. `idlePolicy` already
auto-suspends, so wiring an existing hook to a suspend call is a short example
rather than a feature.

## Reporting security issues

See `SECURITY.md`. Do not open a public issue for a vulnerability.
