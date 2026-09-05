---
title: microvms-agentd
description: Run commands and move files in and out of AWS Lambda MicroVMs. A daemon baked into the VM image, a CLI and three libraries that drive it, and the measured platform behavior behind both.
---

<div class="rfc-title-block">

<p class="rfc-memo-title">A Control Daemon for Lambda MicroVMs</p>

<p class="rfc-brand">EXEC · FILES · HEALTH</p>

</div>

## What microvms-agentd is

AWS Lambda MicroVMs launch a Firecracker VM from a container image and give it one per-instance
HTTPS endpoint. There is no exec API, no file API, and no way to ask what is running inside. This
project supplies all three. `agentd` is a small daemon baked into your VM image as the container
`CMD`. The `microvm` command line, and the Rust, Python, and Node libraries built over the same
crate, talk to it through the endpoint the platform provides. One command builds an image, launches a
VM from it, runs your command inside, reports what the run cost, and tears the VM down.

## The problem it solves

Every team that adopts the platform ends up writing the same in-VM daemon. Two open-source harnesses
that run coding agents inside Lambda MicroVMs each carry one baked into their task images, and each
rediscovered the same platform behavior on its own. The platform also has sharp edges that a first
integration walks into: a `clientToken` replay that wedges an image in `CREATING` for fifteen hours,
a memory request that selects a VM four times the size asked for, a run-hook payload that arrives
wrapped one JSON layer deeper than the caller wrote it.

This repository is that daemon and its client built once and verified. Each platform finding was
measured, recorded with its date, region, and API version, and then closed in the client: illegal
states either do not construct, because regions and sizes are closed enums, or are rejected locally
with an error that names the finding, before any billable call is made.

## What it is made of

The **control plane**, in `microvms-core`, wraps the service API: image builds from a Dockerfile with
local pre-flight of the platform's build traps, launch with per-VM secrets through the one-shot
`runHookPayload`, suspend and resume, and teardown that never raises and reports any identifier it
could not confirm deleted.

The **session plane** talks to `agentd` through the VM's authenticated endpoint. Exec is detached and
idempotent on a caller-minted id, with start, poll, and ack as separate steps so output is never
destroyed unread. Streaming is SSE with resume from a byte cursor. File transfer is streamed, and tar
extraction is confined with `openat2` so a hostile archive cannot write outside its target. HTTP and
WebSocket port forwarding reach servers inside the guest. The token the endpoint requires rotates
hourly, and a detached exec outlives it by design.

**The daemon** is a static `aarch64-unknown-linux-musl` binary, because Lambda MicroVMs are ARM64
only and a static binary bakes into any base image with no interpreter and no dynamic loader. It
serves the platform's lifecycle hooks, the exec and file routes, and `GET /v1/health`, which reports
its version, its bootstrap state, disk pressure, and what identity repair did at startup. Its trust
contract rests on one invariant the image builder owns: `ENTRYPOINT []` and `CMD ["/agentd"]`, so no
workload runs before the platform's run hook lands.

**The CLI envelope** is what makes `microvm` usable from a script or an agent. Every command takes
`--json` and then writes exactly one JSON document to stdout; progress goes to stderr. Success carries
`type` and `data`; failure carries a stable `code`, a mapped `exitCode`, and `suggestions`. The one
exception is `exec --stream`, which emits NDJSON events and the envelope last. `microvm manifest`
prints the whole command surface, generated from the CLI's own argument tree, and the
[Reference](/reference/) tier of this site is generated from that output.

**The libraries** expose the same lifecycle from code with the same defaults and the same guardrails.
`microvms-core` is the Rust crate the CLI is a thin layer over. The Python package `microvms` and the
Node package `@theagenticguy/microvms` are thin bindings over it, and both stubs are generated from
the Rust source so the type system carries the trap closures rather than a comment.

**The conformance suite** runs the whole surface against real VMs. It is separate from the offline
gate because it creates MicroVMs and costs money; `mise run check` is the free definition of done and
`mise run live` is the paid proof. Beneath both sit Z3 proofs over the formal requirements, stateright
models of the daemon and client lifecycles, property tests, network-fault simulation, and a drift gate
that compares the client's hardcoded service constraints against the pinned botocore model.

## Where to start

Install the command line and run its own preflight, then work through the tutorials in order. The
daemon binary is not on the install list: `run`, `build`, and `quickstart` provision the release asset
for their own version, verify it, and cache it under `~/.microvm`.

```bash
cargo binstall microvms-cli           # installs the `microvm` binary, prebuilt
microvm doctor                        # credentials, region, the three environment values
microvm quickstart                    # build, launch, run a hello-world, report the cost, tear down
```

If you are an AI agent rather than a person, read [For agents](/agents/) first. It names the contract
that outranks every page here, the assumptions to drop, and how to fetch this site as Markdown instead
of HTML.

<div class="rfc-toc">

- **1. [Learn](/learn/)**: from an install to a working VM, then task-shaped how-tos
  - 1.1. [Install the CLI and create the AWS prerequisites](/learn/tutorial/install/)
  - 1.2. [Run your first command in a MicroVM](/learn/tutorial/first-run/)
  - 1.3. [Keep a VM alive and work with it by name](/learn/tutorial/long-lived-vm/)
  - 1.4. [Run a project through a VM](/learn/tutorial/run-a-project/)
  - 1.5. [Write a guest Dockerfile](/learn/operations/write-a-guest-dockerfile/) and
    [embed agentd in your own image](/learn/operations/embed-agentd-in-your-image/)
- **2. [Reference](/reference/)**: generated from `microvm manifest`, the binary's own contract
  - 2.1. One page per command, such as [run](/reference/commands/run/), [exec](/reference/commands/exec/),
    and [build](/reference/commands/build/)
  - 2.2. [Exit codes](/reference/exit-codes/), the catalog a caller branches on
  - 2.3. [The envelope](/reference/envelope/) and the [response types](/reference/response-types/)
  - 2.4. [The wire schema](/reference/wire-schema/) the daemon serves at `GET /v1/schema`
  - 2.5. Annotated from the source tree: [CLI](/reference/cli/), [public API](/reference/public-api/),
    [RPC tools](/reference/rpc-tools/)
- **3. [Internals](/internals/)**: the measured findings and the reasoning behind the design
  - 3.1. [Platform](/internals/platform/): every measured claim about Lambda MicroVMs, dated and scoped
  - 3.2. [Protocol](/internals/protocol/): the daemon's wire contract and the defects that shaped it
  - 3.3. [Trust](/internals/trust/): the in-VM trust boundary and the one invariant the daemon cannot enforce
  - 3.4. [Strategy](/internals/strategy/): how the verification stack fits together, and what is declined
  - 3.5. [Embedding](/internals/embedding/): baking agentd into your own image and driving it from your own harness
  - 3.6. [Harness capabilities](/internals/harness-capabilities/): what agent harnesses require, mapped and ranked
  - 3.7. Generated from the source tree: [system overview](/internals/architecture/system-overview/),
    [processes](/internals/behavior/processes/), [state machines](/internals/behavior/state-machines/)
- **Appendix A. [Glossary](/glossary/)**: the project's vocabulary, each term linked to the page that develops it

</div>

## How to read this site

Two kinds of document live under Internals and they are not equally reliable. The hand-written
documents carry measured platform findings and design rationale; they predate the generated tree and
win wherever the two disagree. The generated categories were produced by a per-file documentation pass
over the codebase, and every factual claim there carries a `path:line` citation that was
machine-verified against the source when the page was generated.

The Reference tier is generated from `microvm manifest`, so a command, flag, exit code, or response
type on those pages is the one the binary ships. Where this site and the binary disagree, the binary
is right and a page is stale.

Section numbers live in the heading text, as in `## 2. Gotchas`, so the page, the contents, the search
index, and the Markdown this site serves to an agent all agree on which section is which. The cover
page's own headings carry no number, so the numbers above belong to the tiers they index.

Every citation on this site is a link into the repository pinned to the commit the page was built
from. A line number points into that commit, so read it as a pointer into the source rather than a
permanent address: the commit pin is what lets you see what the claim was made about.

Every page is also served as Markdown at its own path with `.md` appended, and links that twin from
its `<head>`. [`llms.txt`](/llms.txt) indexes the corpus, [`llms-full.txt`](/llms-full.txt) is every
page in one file, and [`llms-small.txt`](/llms-small.txt) is the same corpus with non-essential content
removed. The daemon's wire schema is published as [`schema.json`](/schema.json).
