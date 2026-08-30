# microvms-agentd

[![ci](https://github.com/theagenticguy/microvms-agentd/actions/workflows/ci.yml/badge.svg)](https://github.com/theagenticguy/microvms-agentd/actions/workflows/ci.yml)
[![live conformance](https://github.com/theagenticguy/microvms-agentd/actions/workflows/live-conformance.yml/badge.svg)](https://github.com/theagenticguy/microvms-agentd/actions/workflows/live-conformance.yml)
[![release](https://github.com/theagenticguy/microvms-agentd/actions/workflows/release.yml/badge.svg)](https://github.com/theagenticguy/microvms-agentd/actions/workflows/release.yml)
[![docs](https://github.com/theagenticguy/microvms-agentd/actions/workflows/docs.yml/badge.svg)](https://theagenticguy.github.io/microvms-agentd/)
[![OpenSSF Scorecard](https://img.shields.io/badge/dynamic/json?url=https%3A%2F%2Fapi.scorecard.dev%2Fprojects%2Fgithub.com%2Ftheagenticguy%2Fmicrovms-agentd&query=%24.score&label=openssf%20scorecard)](https://scorecard.dev/viewer/?uri=github.com/theagenticguy/microvms-agentd)

[![crates.io](https://img.shields.io/crates/v/microvms-cli.svg?label=crates.io)](https://crates.io/crates/microvms-cli)
[![docs.rs](https://img.shields.io/docsrs/microvms-core?label=docs.rs)](https://docs.rs/microvms-core)
[![PyPI](https://img.shields.io/pypi/v/microvms.svg?label=PyPI)](https://pypi.org/project/microvms/)
[![npm](https://img.shields.io/npm/v/%40theagenticguy%2Fmicrovms.svg?label=npm)](https://www.npmjs.com/package/@theagenticguy/microvms)
[![License](https://img.shields.io/badge/License-Apache_2.0-blue.svg)](LICENSE)

Run commands and move files in and out of AWS Lambda MicroVMs.

The service gives you an isolated Firecracker VM but no exec API and no
file-transfer API. This project supplies both: `agentd` is a small daemon baked
into your VM image, and the `microvm` CLI (plus Rust, Python, and Node
libraries) talks to it. One command provisions the daemon, builds an image,
launches a VM, runs your command inside it, reports the cost, and tears
everything down.

```bash
microvm run --exec "echo hello from a microvm"
```

## Contents

- [Install](#install) — the CLI (binstall or crates.io) and the three libraries
- [Quick start](#quick-start) — AWS prerequisites and `microvm quickstart`
- [Why this exists](#why-this-exists)
- [How it works](#how-it-works) — control plane, session plane, and the
  [long-lived VM](#working-with-a-long-lived-vm),
  [project sync](#running-a-project-through-a-vm),
  [coding agent](#running-coding-agents-inside-a-microvm),
  [library](#calling-it-from-code), [scripting](#using-it-from-scripts-and-agents),
  and [cost](#what-it-costs) paths
- [Writing your own guest Dockerfile](#writing-your-own-guest-dockerfile) — the
  traps that cost a build cycle, and how to spend none of them
- [The workspace](#the-workspace)
- [Developing](#developing)
- [License](#license)

## Install

One artifact gets you running: the `microvm` CLI. The `agentd` daemon binary
that gets baked into your VM images is the CLI's own component — `run`,
`build`, and `quickstart` provision the release asset for their own version
automatically, verify it, and cache it under `~/.microvm`.

**The CLI**, prebuilt from the release assets (seconds), or compiled from
crates.io (minutes):

```bash
cargo binstall microvms-cli           # installs the `microvm` binary, prebuilt
cargo install microvms-cli --locked   # the same binary, compiled locally
```

Release assets cover Linux (x86_64, aarch64, glibc 2.17 floor), macOS (arm64,
x86_64), and Windows (x64), each with a signed build-provenance attestation
beside it. There is deliberately no Homebrew formula: a tap is a second
repository with its own release cadence to maintain, and binstall plus these
assets already cover macOS.

**The daemon binary**, only if you want to manage it yourself (a custom build,
an airgapped machine): it is a static `aarch64-unknown-linux-musl` build,
because Lambda MicroVMs are ARM64-only and a static binary bakes into any base
image with no interpreter and no dynamic loader. Verify it the same way the
CLI does before passing it as the positional or `$MICROVM_AGENTD`:

```bash
gh release download --repo theagenticguy/microvms-agentd --pattern agentd
gh attestation verify agentd --repo theagenticguy/microvms-agentd
chmod +x agentd
```

**The libraries**, for calling the same lifecycle from code:

```bash
cargo add microvms-core                   # Rust; API reference on docs.rs
uv add microvms                           # Python >= 3.9; or pip install microvms
npm install @theagenticguy/microvms       # Node >= 22.13
```

The Python package ships abi3 wheels for Linux (x86_64, aarch64), macOS
(arm64, x86_64), and Windows (x64), so one wheel per platform covers CPython
3.9 and everything after it. The npm package carries all four platform
binaries in one package. All three registries are published through OIDC
trusted publishing from [release.yml](.github/workflows/release.yml): no
long-lived registry token exists anywhere, PyPI uploads carry PEP 740
attestations, and npm publishes with provenance.

Building from source remains one task: `mise install && mise run build`
cross-compiles the daemon, and `mise run install:cli` installs the CLI from
the working tree. `agentd` itself is deliberately not on crates.io, because
the daemon reaches a consumer as a binary inside a task image, and the GitHub
release is that channel.

## Quick start

**What you need:** an AWS account with Lambda MicroVMs access in a service
region (`us-east-1`, `us-east-2`, `us-west-2`, `eu-west-1`, `ap-northeast-1`),
AWS credentials in your environment, and the `microvm` CLI from
[Install](#install). The daemon binary is not on this list: the CLI provisions
it.

**1. Create the AWS prerequisites.**

A MicroVM image build needs an S3 bucket for the code artifact, a build role,
and an execution role. The repo ships a small Terraform stack that creates
exactly those three things (clone the repo and run `mise run live:infra`, or
read [conformance/infra](conformance/infra/) and create equivalents by hand):

```bash
mise run live:infra
cd conformance/infra
export MICROVM_BUCKET=$(terraform output -raw s3_bucket)
export MICROVM_BUILD_ROLE_ARN=$(terraform output -raw build_role_arn)
export MICROVM_EXECUTION_ROLE_ARN=$(terraform output -raw execution_role_arn)
cd ../..
```

If you already have a bucket and roles, export those instead; the stack is a
convenience, and the CLI only reads the three environment values.

**2. Check the machine.**

```bash
microvm doctor
```

`doctor` checks your credentials, the region, and the three environment
values. When something is wrong it names the broken prerequisite and suggests
the fix. (Managing your own daemon binary? `--binary <path>` checks its
architecture too.)

**3. Run your first command in a MicroVM.**

```bash
microvm quickstart
```

This provisions the daemon (this CLI's own release asset, provenance-verified
through `gh` when it is on PATH, checksummed through the release's SHA256SUMS
otherwise), builds an image with it as the entrypoint, launches a VM, runs a
hello-world, prints its output and the run's cost, and tears the VM down.
Teardown is the default so an interrupted session does not leave a billable VM
behind. `quickstart` is exactly `microvm run --exec "echo hello from a
microvm"` — once it works, use `run` and its knobs directly. Expect the first
run to take a few minutes; most of it is the image build, and the image
snapshot has a one-week minimum retention, so keep and reuse it (`--image`)
rather than rebuilding.

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
fifteen hours, a memory request that silently selects a 4x-larger VM. Each one
was measured once, recorded in [docs/PLATFORM.md](docs/PLATFORM.md) with its
date and region, and then closed in the client: illegal states either do not
construct (regions and sizes are closed enums) or are rejected locally with an
error that names the finding, before any billable call.

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
transfer, tar extraction confined with `openat2` so a hostile archive cannot
write outside its target, and HTTP/WebSocket port forwarding into the guest.
The token the endpoint requires rotates hourly; detached execs outlive it by
design.

### Working with a long-lived VM

Pass `--keep` to leave the VM running, and `--vm-name` to register a local
name for it, so no later command needs the endpoint, agent token, and
MicroVM id pasted back in:

```bash
microvm run --keep --vm-name dev

microvm exec --name dev "python3 -V"
microvm cp ./data.csv vm:/tmp/data.csv --name dev
microvm cp --tar ./project.tar vm:/workspace --name dev
microvm suspend dev          # freeze: memory, filesystem, and token survive
microvm resume dev           # thaw; a running process resumes mid-flight
microvm terminate dev        # stop paying; the name is released
```

The name is a purely local fact: the registry lives in the CLI's state
directory, carries the whole identifier triple, and costs zero AWS calls to
resolve. The explicit `--endpoint`/`--agent-token`/`--microvm-id` flags still
work everywhere, for a VM some other machine launched.

`exec` also streams (`--stream`), feeds stdin (`--stdin`), starts a command
and returns immediately (`--detach`), and reads an existing exec back
(`--poll <id>`). Suspend and resume preserve memory, the filesystem, and
running processes, and a suspended VM bills at a small fraction of a running
one. If a run is interrupted, `microvm ls` lists what this CLI created and
could not confirm it deleted, so nothing leaks silently.

### Running a project through a VM

When the positional argument to `run` is a directory rather than a binary,
`run` becomes a pack-run-collect round trip against an existing image:

```bash
microvm run . --image ci-image --exec "make test"
```

The tree is packed locally (`.git`, `target`, `node_modules`, and `.venv`
skipped whole; symlinks preserved as links, never followed), uploaded to
`/workspace` in the guest, and the command runs there. Afterwards, members
matching the `artifacts` globs in `microvm.toml` come back into the local
directory, including when the command failed, because a failing run's report
is the artifact CI most wants. An over-budget tree is refused locally, before
any archive bytes are allocated or any AWS call is made.

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

- **Rust**: [`microvms-core`](https://docs.rs/microvms-core); the CLI is a
  thin layer over it.
- **Python**: [`microvms`](https://pypi.org/project/microvms/), built with
  maturin. Typed: the wheel ships a stub and a PEP 561 `py.typed`, so mypy,
  pyright, and `ty` see the real signatures and the Rust doc comments rather
  than `Any`.
- **Node**: [`@theagenticguy/microvms`](https://www.npmjs.com/package/@theagenticguy/microvms),
  built with napi-rs. Typed: `index.d.ts` ships beside the addon.

Both binding stubs are generated from the Rust source, never hand-written, so
the trap closures are visible to a type checker and not only at runtime: a
dollar amount is a `str` a checker refuses to add, `Duration` has no
constructor that omits provenance, and the two hook timeouts are unrelated
classes rather than two ints. Neither stub can go stale unnoticed: `mise run
stubs:check` regenerates the Python stub and fails on any difference, and
Node's `index.d.ts` is regenerated before every test run. See
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

Sizing follows one rule: the minimum you request is your bill floor (25% of
the provisioned capacity), 4x the minimum is your hard ceiling, and both are
fixed at provision time; there is no scaling event. For peaky workloads
(builds, test runs, agent sessions), pick a low minimum and let peaks ride
the always-present 4x headroom, which bills only by what is consumed.

## Writing your own guest Dockerfile

Start from the client's own stanza rather than from scratch:

```bash
microvm dockerfile
```

prints the Dockerfile that a default build bakes, generated by the same code
that builds it, with the platform constraints annotated inline. Append your
`RUN` layers to that output and pass the result to `--dockerfile`. Each item
below cost a real build cycle to find, and a server-side cycle is roughly
three minutes plus a wedged image name you cannot reuse (`clientToken` is a
permanent idempotency key). `microvm doctor` names every missing prerequisite
and prints the command that fills it, which is faster than reading the rest of
this section.

**Build it locally under arm64 first.** A `dnf` typo or a missing package is
free to find here and expensive to find server-side:

```bash
docker buildx build --platform linux/arm64 -t guest-check -f guest.Dockerfile .
```

With `qemu-aarch64` binfmt registered this builds the real target architecture
on an x86 host. Two errors in the example Dockerfile were caught this way in
seconds.

**`ENV AGENTD_PORT` must be 9000**, the client's `DEFAULT_AGENT_PORT`. The
platform dials its build-time `ready` and `validate` hooks on the port from
the create call, so a guest listening elsewhere answers neither, and the build
fails with `CREATE_FAILED` after a completely clean build log. The daemon's
own `agentd listening` line appears, with the wrong address, and no error line
follows. The client refuses that disagreement locally now, including the case
where the Dockerfile names no port while you have moved the client off the
default with `--port`: an unset variable leaves the daemon on 9000 rather than
on your port.

**Set a `WORKDIR` explicitly.** `al2023-minimal` leaves `WorkingDir` empty, so
an omitted `--cwd` on every later `exec` resolves against `/`. Check that the
user your workload runs as can write to the directory you choose; a root-owned
`WORKDIR` under a non-root workload fails at the first write.

**On `-minimal` bases, spell weak dependencies off as
`--setopt=install_weak_deps=0`.** The integer is what works; `False` is not
accepted.

**Keep `ENTRYPOINT []` and `CMD ["/agentd"]`.** That pair is the trust
boundary: it is what guarantees no workload runs before the platform's run
hook lands and the token arrives. An `ENTRYPOINT` that swallows `CMD` produces
a VM whose daemon never starts, and the symptom is a run-hook timeout that
mentions neither.

**Leave `AGENTD_SSE_KEEPALIVE_SECS` alone** unless you also raise the client's
stream idle timeout. The client treats 60 seconds of silence as a dead
connection because the daemon's keepalive is 15; a longer interval makes
healthy streams look dead. This too is refused locally.

[examples/coding-agents-on-bedrock/Dockerfile](examples/coding-agents-on-bedrock/Dockerfile)
is a working guest Dockerfile that respects all of the above.

## The workspace

```text
protocol/        daemon↔client wire types; drift is a compile error
agentd/          the in-VM daemon: exec, file transfer, one-shot bootstrap
model/           stateright models of the daemon and client lifecycle
microvms-core/   the client library: control plane, session, cost, sandbox
microvms-cli/    the microvm binary: 20 commands, JSON envelopes, a manifest
microvms-py/     Python binding (PyO3)
microvms-js/     Node binding (napi-rs)
conformance/     the live suite: 113 checks against real AWS, via the CLI
spec/            57 formal requirements in symspec, checked with Z3
```

Three crates publish to crates.io (`microvms-protocol`, `microvms-core`,
`microvms-cli`); the bindings publish to PyPI and npm; `agentd` publishes as
an attested release binary. The publish set is an allowlist in
[Cargo.toml](Cargo.toml), with each deliberate absence explained next to it.

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
mise run live            # conformance + rates + leak check, ~5 min
mise run live:destroy    # tear the Terraform stack back down
```

Supply-chain gates run in `mise run security`, in the git hooks, and in CI:
semgrep, secret scanning over the full git history, SPDX license headers on
every tracked source file, `cargo deny` (dependency licenses against a
measured allowlist, yanked crates, untrusted registries), and actionlint over
the workflows. CI publishes CycloneDX and SPDX SBOMs per commit, three
scanners audit them (grype, trivy, osv-scanner), every accepted finding lives
in an ignore file with its reason, and Dependabot watches cargo, Actions, and
npm weekly. [OpenSSF Scorecard](https://scorecard.dev/viewer/?uri=github.com/theagenticguy/microvms-agentd)
audits the repository's own maintenance posture weekly from
[scorecard.yml](.github/workflows/scorecard.yml), and its findings land as
code-scanning alerts. Releases are tag-triggered through
[release.yml](.github/workflows/release.yml): a guard job proves the tag, the
manifests, and the publish set agree before anything irreversible starts, and
every registry is reached through OIDC trusted publishing rather than a stored
token. [CONTRIBUTING.md](CONTRIBUTING.md) has the workflow;
[docs/README.md](docs/README.md) indexes the rest of the documentation,
starting with the
[system overview](docs/architecture/system-overview.md) and the
[debugging guide](docs/insights/debugging-guide.md).

The same documentation is published at
**<https://theagenticguy.github.io/microvms-agentd/>**, built from `docs/` by
`site/` and deployed by `.github/workflows/docs.yml`. Every page is also
served as raw Markdown at its own path with `.md` appended, the whole corpus
comes as `llms.txt`, `llms-full.txt` and `llms-small.txt`, every `path:line`
citation is a commit-pinned link into this repository, and the Mermaid
diagrams are rendered to SVG at build time so they are present in a fetch that
runs no JavaScript.
[For agents](https://theagenticguy.github.io/microvms-agentd/agents/) is the
page that names which surface answers which question. `mise run docs:check`
builds and gates it locally.

## License

Apache-2.0. Every source file carries an `SPDX-License-Identifier` line, and
dependency licenses are enforced against the allowlist in
[deny.toml](deny.toml).
