---
title: Internals
description: Why the system is shaped the way it is. The measured platform findings, the trust contract, the wire protocol, and the generated documentation of the source tree.
---

These pages explain why the system is shaped the way it is, and they record what was measured on the
platform it runs on. Someone who wants to install the CLI, run a command in a VM, or bake the daemon
into an image is served by [Learn](/learn/). Someone who wants the exact spelling of a flag, an exit
code, or a response type is served by [Reference](/reference/), which is generated from
`microvm manifest`.

## 1. The premise

Lambda MicroVMs hand a VM one HTTPS endpoint and forward it to whatever the image's `CMD` is listening
on. There is no exec API and no file API. That is why a daemon is baked into the image, and it is why
the one invariant the daemon cannot check about itself is stated on every page that touches trust:
`ENTRYPOINT []` and `CMD ["/agentd"]`, with no init system and no other process started first. The
pair is what guarantees no workload runs before the platform's run hook lands and the token arrives. A
base image that starts a background process before the daemon binds its listener can win the
one-shot bootstrap for itself, and the platform's real hook then gets the 409. Enforcing the invariant
belongs to whoever builds the image, because a daemon cannot inspect its own image.

Every claim about the platform on these pages is an observation of someone else's system, so each
carries the date, the region, and the API version it was measured under. Where two findings disagree,
the newer one is appended after the older and both stay, on purpose. A platform claim without a date,
a region, and an API version is not a claim on this site.

## 2. The chapters

The hand-written documents come first. They predate the generated tree, they carry the measured
findings and the design rationale, and they win any disagreement with the pages generated from the
source tree.

| Document                                                    | What it settles                                                                                                                         |
| ----------------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------- |
| [Platform](/internals/platform/)                            | Every measured claim about Lambda MicroVMs: hooks, `runHookPayload`, memory baseline versus peak, endpoint auth, `clientToken`, idle policy, cost |
| [Protocol](/internals/protocol/)                            | The daemon's wire contract under `/v1/`, the rules a defect made necessary, streaming and stdin, reconnect at a cursor                 |
| [Trust](/internals/trust/)                                  | The threat model, why source-address filtering is wrong, the five defenses, the unenforced invariant, identity repair, tunnel identity |
| [Embedding](/internals/embedding/)                          | Baking `agentd` into your own image with `microvm dockerfile`, the client a harness implements, the proxy-token reality, the `AGENTD_*` knobs |
| [Strategy](/internals/strategy/)                            | The diagnosis, the guiding policy, the coherent actions, and what is deliberately not being done                                        |
| [Harness capabilities](/internals/harness-capabilities/)    | What Harbor, Omnigent, and the Vercel Sandbox shape require of a sandbox platform, mapped onto this one, with the gaps ranked           |
| [CLI coverage plan](/internals/cli-coverage-plan/)          | The plan that took live conformance through the CLI to full coverage. Implemented; kept for its reasoning                               |

The generated categories follow. Each was produced by a per-file documentation pass over the codebase,
and every factual claim carries a `path:line` citation that was machine-verified against the source
when the page was generated.

| Category                                                        | What it holds                                                                                                          |
| --------------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------- |
| [Architecture](/internals/architecture/system-overview/)        | The system overview, the module map of each crate, and the data flow of `microvm run`, `exec --stream`, and `cp --tar` |
| [Behavior](/internals/behavior/processes/)                      | The core flows from image build to interrupt teardown, and the state machines with their proved invariants             |
| [Analysis](/internals/analysis/risk-hotspots/)                  | Where the live rounds found bugs and coverage is thinnest, knowledge concentration by artifact, and dead-code candidates |
| [Diagrams](/internals/diagrams/architecture/components/)        | Component, dependency, and sequence diagrams, rendered to SVG at build time                                            |
| [Insights](/internals/insights/impact-analysis/)                | Impact analysis, the debugging guide, the contract map, the business rules, and the tech-debt register                 |

Three annotated reference pages from the same pass sit in the Reference tier rather than here, beside
the pages generated from the manifest: [CLI](/reference/cli/), [Public API](/reference/public-api/),
and [RPC tools](/reference/rpc-tools/). The [glossary](/glossary/) defines the vocabulary these pages
use and links each term to the page that develops it.

## 3. How to read a citation

Every factual claim in a generated page names the code that supports it, in repo-relative `path:line`
form, and on this site each of those citations is a link into the repository pinned to the commit the
page was built from. A line number points into that commit, so treat it as a pointer rather than a
permanent address: a refactor aims it at different code while it still reads as authoritative, and the
commit pin is what lets you open the link and see what the claim was made about.

When a hand-written document and a generated page disagree, the hand-written document is right. When a
generated page and the source at the pinned commit disagree, the page is stale and the fix is to
regenerate it rather than edit it. When a measurement on Platform and the platform itself disagree,
re-measure and append the new finding with its date, region, and API version.
