# microvms-agentd

[![ci](https://github.com/theagenticguy/microvms-agentd/actions/workflows/ci.yml/badge.svg)](https://github.com/theagenticguy/microvms-agentd/actions/workflows/ci.yml)
[![live conformance](https://github.com/theagenticguy/microvms-agentd/actions/workflows/live-conformance.yml/badge.svg)](https://github.com/theagenticguy/microvms-agentd/actions/workflows/live-conformance.yml)
[![License](https://img.shields.io/badge/License-Apache_2.0-blue.svg)](LICENSE)
[![Rust Edition 2024](https://img.shields.io/badge/rust-edition_2024-orange.svg)](Cargo.toml)
[![Platform: AWS Lambda MicroVMs](https://img.shields.io/badge/platform-AWS_Lambda_MicroVMs-FF9900.svg)](docs/PLATFORM.md)

Run commands and move files in and out of AWS Lambda MicroVMs.

The service gives you an isolated Firecracker VM but no exec API and no
file-transfer API. This project supplies both: `agentd` is a small daemon baked
into your VM image, and the `microvm` CLI (plus Rust, Python, and Node
libraries) talks to it. One command builds an image, launches a VM, runs your
command inside it, reports the cost, and tears everything down.

```bash
microvm run ./agentd --exec "echo hello from a microvm"
```

## Quick start

**What you need:** an AWS account with Lambda MicroVMs access in a service
region (`us-east-1`, `us-east-2`, `us-west-2`, `eu-west-1`, `ap-northeast-1`),
AWS credentials in your environment, and [mise](https://mise.jdx.dev/), which
provides the Rust toolchain, Terraform, and every task below. Everything is
source-only; nothing is published to crates.io, PyPI, or npm (the workspace
declares `publish = false` so a stray `cargo publish` cannot change that). You
build the two binaries yourself, and the build is one task.

**1. Clone and build.**

```bash
git clone https://github.com/theagenticguy/microvms-agentd
cd microvms-agentd
mise install                # toolchains: Rust with the aarch64-musl target, Terraform
mise run build              # agentd, cross-compiled for the VM (aarch64-musl)
mise run install:cli        # installs `microvm` into ~/.cargo/bin
```

The daemon cross-compiles to `aarch64-unknown-linux-musl` because Lambda
MicroVMs are ARM64-only; the CLI installs into `~/.cargo/bin`, which rustup
puts on your PATH, so `microvm` runs bare from here on. The daemon binary
lands at a long path, so give it a name:

```bash
export AGENTD=target/aarch64-unknown-linux-musl/release/agentd
```

**2. Create the AWS prerequisites.**

A MicroVM image build needs an S3 bucket for the code artifact, a build role,
and an execution role. The repo ships a small Terraform stack that creates
exactly those three things:

```bash
mise run live:infra
```

Then export its outputs where the CLI looks for them:

```bash
cd conformance/infra
export MICROVM_BUCKET=$(terraform output -raw s3_bucket)
export MICROVM_BUILD_ROLE_ARN=$(terraform output -raw build_role_arn)
export MICROVM_EXECUTION_ROLE_ARN=$(terraform output -raw execution_role_arn)
cd ../..
```

If you already have a bucket and roles, export those instead; the stack is a
convenience, not a requirement.

**3. Check the machine.**

```bash
microvm doctor --binary $AGENTD
```

`doctor` checks your credentials, the region, the three environment values,
and that the daemon binary is aarch64. When something is wrong it names the
broken prerequisite and suggests the fix.

**4. Run your first command in a MicroVM.**

```bash
microvm run $AGENTD --exec "uname -a && echo hello from inside"
```

This builds an image with `agentd` as its entrypoint, launches a VM from it,
runs the command, prints its output and the run's cost, and tears the VM down.
Teardown is the default so an interrupted session does not leave a billable VM
behind. Expect the first run to take a few minutes; most of it is the image
build, and the image snapshot has a one-week minimum retention, so keep and
reuse it (`--image`) rather than rebuilding.

## Why this exists

Lambda MicroVMs launch a Firecracker VM from a container image and give it a
per-instance HTTPS endpoint, and that is all: no exec API, no file API, no
way to ask what is running inside. Every team that adopts the platform ends
up hand-rolling the same in-VM daemon; Harbor and Omnigent each carry one
baked into their task images (see
[docs/HARNESS-CAPABILITIES.md](docs/HARNESS-CAPABILITIES.md)).

This repo is that daemon and its client built once, fully, and verified: exec
that survives auth-token rotation, tar transfer that cannot escape its target
directory, suspend/resume that preserves running processes, cost reporting
from pinned rates, and a conformance suite that proves all of it against real
VMs. The platform also has sharp edges: error responses that point away from
their causes, a `clientToken` replay that wedges an image in `CREATING` for
fifteen hours, a memory floor that silently doubles. Each one was measured
once, recorded in [docs/PLATFORM.md](docs/PLATFORM.md) with its date and
region, and then closed in the client: illegal states either do not construct
(regions and sizes are closed enums) or are rejected locally with an error
that names the finding, before any billable call.

## How it works

```text
your machine                        AWS
────────────                        ───
microvm CLI ──[control plane]──▶ Lambda MicroVMs API
   │                                 │ build image / run / suspend / terminate
   │                                 ▼
   └──[session, HTTPS + proxy auth]▶ per-VM endpoint ──▶ agentd (in the VM)
                                                          exec · files · health
```

The **control plane** (`microvms-core`) wraps the service API: image builds
from a Dockerfile with local pre-flight of the platform's two build traps,
launch with per-VM secrets through the one-shot `runHookPayload`, suspend and
resume, teardown that never raises and reports leaked identifiers. The
**session plane** talks to `agentd` through the VM's authenticated endpoint:
idempotent detached exec (caller-minted ids, start/poll/ack, output never
destroyed unread), SSE streaming with byte-cursor resume, streamed file
transfer, and tar extraction confined with `openat2` so a hostile archive
cannot write outside its target. The token the endpoint requires rotates
hourly; detached execs outlive it by design.

### Working with a long-lived VM

Pass `--keep` to leave the VM running. The output includes three values every
attached command needs: the endpoint, the agent token, and the MicroVM id.

```bash
microvm run --keep $AGENTD

# capture endpoint, agentToken, and microvmId from the output, then:

microvm exec --endpoint $EP --agent-token $TOK --microvm-id $ID "python3 -V"
microvm cp ./data.csv vm:/tmp/data.csv \
  --endpoint $EP --agent-token $TOK --microvm-id $ID
microvm cp --tar ./project.tar vm:/workspace \
  --endpoint $EP --agent-token $TOK --microvm-id $ID
microvm suspend $ID          # freeze: memory, filesystem, and token survive
microvm resume $ID           # thaw; a running process resumes mid-flight
microvm terminate $ID        # stop paying
```

`exec` also streams (`--stream`), feeds stdin (`--stdin`), starts a command
and returns immediately (`--detach`), and reads an existing exec back
(`--poll <id>`). Suspend and resume preserve memory, the filesystem, and
running processes, and a suspended VM bills at a small fraction of a running
one. If a run is interrupted, `microvm ls` lists what this CLI created and
could not confirm it deleted, so nothing leaks silently.

### Running coding agents inside a MicroVM

[examples/coding-agents-on-bedrock](examples/coding-agents-on-bedrock/) runs
Claude Code and Codex CLI headless inside a MicroVM, against Bedrock, with no
vendor API key anywhere. One script builds an image carrying both CLIs,
launches a VM with `--egress`, mints a short-lived Bedrock bearer token from
your AWS credentials, copies it in over the authenticated channel, and drives
each agent through `microvm exec`:

```bash
examples/coding-agents-on-bedrock/run.sh
```

Claude Code uses its native Bedrock mode (`CLAUDE_CODE_USE_BEDROCK=1`);
Codex talks to Bedrock's OpenAI-compatible Responses endpoint through a
five-line provider config. The example's [README](examples/coding-agents-on-bedrock/README.md)
explains each decision, including the two platform constraints the Dockerfile
has to respect.

Two open-source harnesses run coding agents inside Lambda MicroVMs the same
way, each carrying its own hand-rolled daemon:
**Harbor** ([harbor-framework/harbor#2469](https://github.com/harbor-framework/harbor/pull/2469))
for agent evaluation and **Omnigent**
([omnigent-ai/omnigent#2217](https://github.com/omnigent-ai/omnigent/pull/2217))
for server-managed sessions. Both integrations predate this repo's daemon;
this project is the same architecture built out fully, with the daemon, the
verified client, and the platform findings shared across any harness instead
of rediscovered per integration.
[docs/HARNESS-CAPABILITIES.md](docs/HARNESS-CAPABILITIES.md) maps their
contracts onto this platform and ranks what is still missing.

### Calling it from code

The same lifecycle is available as a library, with the same defaults and the
same guardrails:

- **Rust**: `microvms-core`; the CLI is a thin layer over it.
- **Python**: `microvms-py`, built with maturin. Typed: the wheel ships
  `microvms/__init__.pyi` and a PEP 561 `py.typed`, so mypy, pyright, and `ty`
  see the real signatures and the Rust doc comments rather than `Any`.
- **Node**: `microvms-js`, built with napi-rs. Typed: `napi build` writes
  `index.d.ts` beside the addon.

Both stubs are generated from the Rust source, never hand-written, so the trap
closures are visible to a type checker and not only at runtime: a dollar amount
is a `str` a checker refuses to add, `Duration` has no constructor that omits
provenance, and the two hook timeouts are unrelated classes rather than two
ints. Neither stub can go stale unnoticed: `mise run stubs:check` regenerates
the Python stub and fails on any difference, and Node's `index.d.ts` is
regenerated before every test run. See
[docs/reference/public-api.md](docs/reference/public-api.md) for the surface.

### Using it from scripts and agents

Every command takes `--json` and then emits exactly one JSON envelope on
stdout; progress goes to stderr. Success carries `type` and `data`; failure
carries a stable `code`, a mapped `exitCode`, and `suggestions`. The one
exception is `exec --stream`, which emits NDJSON events and the envelope last.
`microvm manifest` prints the whole command surface as JSON, generated from
the CLI's own argument tree, so a tool can discover the surface without
parsing help text. Details in [docs/reference/cli.md](docs/reference/cli.md).

### What it costs

Every run reports a cost estimate built from pinned, dated, per-region ARM
rates; `mise run live:rates` checks the pinned table against the AWS Pricing
API. Anything the engine cannot price is reported as unpriced with a reason
rather than as zero, and a total containing an unpriced line renders as a
lower bound. Note the image snapshot's one-week minimum retention: deleting an
image early saves nothing, so reuse is the economical habit.

## The workspace

```text
protocol/        daemon↔client wire types; drift is a compile error
agentd/          the in-VM daemon: exec, file transfer, one-shot bootstrap
model/           stateright models of the daemon and client lifecycle
microvms-core/   the client library: control plane, session, cost, sandbox
microvms-cli/    the microvm binary: 17 commands, JSON envelopes, a manifest
microvms-py/     Python binding (PyO3)
microvms-js/     Node binding (napi-rs)
conformance/     the live suite: 77 checks against real AWS, via the CLI
spec/            51 formal requirements; 3 lifecycle invariants proved in Z3
```

## Developing

```bash
mise run install         # git hooks
mise run check           # the definition of done: lint, security, all test
                         # tiers, schema freshness, stub freshness, model
                         # drift, cross-compile
```

Verification runs at six tiers: Z3 proofs over the spec, stateright models,
property tests, network-fault simulation (turmoil), a drift gate comparing the
hardcoded service constraints against the pinned botocore model, and the live
conformance suite. The live tier is separate because it creates real MicroVMs
and costs money:

```bash
mise run live            # conformance (77 checks) + rates + leak check, ~5 min
mise run live:destroy    # tear the Terraform stack back down
```

Supply-chain gates run in `mise run security`, in the git hooks, and in CI:
semgrep, secret scanning over the full git history, SPDX license headers on
every tracked source file, `cargo deny` (dependency licenses against a
measured allowlist, yanked crates, untrusted registries), and actionlint over
the workflows. CI publishes CycloneDX and SPDX SBOMs per commit, three
scanners audit them (grype, trivy, osv-scanner), every accepted finding lives
in an ignore file with its reason, and Dependabot watches cargo, Actions, and
npm weekly. [CONTRIBUTING.md](CONTRIBUTING.md) has the workflow;
[docs/README.md](docs/README.md) indexes the rest of the documentation,
starting with the
[system overview](docs/architecture/system-overview.md) and the
[debugging guide](docs/insights/debugging-guide.md).

## License

Apache-2.0. Every source file carries an `SPDX-License-Identifier` line, and
dependency licenses are enforced against the allowlist in
[deny.toml](deny.toml).
