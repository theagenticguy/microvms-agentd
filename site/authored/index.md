---
title: microvms-agentd
description: Run commands and move files in and out of AWS Lambda MicroVMs — a daemon baked into the VM image, a CLI and libraries that drive it, and the measured platform behavior behind both.
editUrl: false
sidebar:
  order: 0
  label: Overview
---

AWS Lambda MicroVMs give you an isolated Firecracker VM with no exec API and no file-transfer API. This
project supplies both. `agentd` is a small daemon baked into your VM image; the `microvm` CLI — plus Rust,
Python, and Node libraries over the same crate — talks to it. One command builds an image, launches a VM,
runs your command inside it, reports the cost, and tears everything down.

```bash
microvm run $AGENTD --exec "echo hello from a microvm"
```

There is nothing to install from a package registry yet, so you build the two binaries yourself — and the
build is one task. [For agents](/agents/) has the sequence that ends in a command running inside a real
VM.

## Start here

- **[For agents](/agents/)** — if you are an AI agent or you are driving one, read this first. It names
  the contract that outranks every page here, the assumptions to drop, and how to fetch this site as
  Markdown instead of HTML.
- **[Platform](/platform/)** — every measured claim about Lambda MicroVMs, each with a date, a region, and
  an API version. The trap findings recorded there are why the rest of this exists.
- **[Protocol](/protocol/)** — the daemon's wire contract, which never changes silently.
- **[Embedding](/embedding/)** — baking `agentd` into your own task image and driving it from your own
  harness, rather than through this CLI.

## Two tiers of document, and which one wins

The hand-written documents under **Authoritative** carry measured platform findings and design rationale.
They predate the generated tree and they win wherever the two disagree.

Everything under Architecture, Reference, Behavior, Analysis, Diagrams and Insights was produced by a
per-file documentation pass over the codebase. Every factual claim in those pages carries a `path:line`
citation that was machine-verified against the source when the page was generated, and on this site every
one of those citations is a link into the repository pinned to the commit the page was built from.

Those citations are the generated tier's value and its expiry date. They anchor to line numbers, so a
refactor aims them at different code while they still read as authoritative. The commit pin is what makes
that checkable rather than invisible: follow the link and you see what the claim was made about.

:::agent

**For an agent.** Treat a page under Authoritative as a claim you can act on, and a page under the
generated categories as a claim plus a citation you should open before acting. When they disagree, the
hand-written page is right.

:::

## The workspace

| Crate                       | What it is                                                                     |
| --------------------------- | ------------------------------------------------------------------------------ |
| `protocol`                  | the daemon-to-client wire types. A drift here is a compile error.               |
| `agentd`                    | the in-VM daemon: exec, file transfer, one-shot bootstrap                       |
| `microvms-core`             | the client library, where the type system carries every trap closure            |
| `microvms-cli`              | the `microvm` binary: JSON envelopes, and `manifest` as the machine-readable contract |
| `model`                     | stateright models of the daemon and client lifecycles                           |
| `microvms-py`, `microvms-js` | thin PyO3 and napi-rs bindings over `microvms-core`                            |

The dependency direction is enforced rather than documented: `cli` depends on `core` depends on
`protocol`, the bindings depend on `core`, and the daemon depends on `protocol`.
[Module map](/architecture/module-map/) walks each crate's contents;
[System overview](/architecture/system-overview/) is how they fit together.

## Reading this site as data

Every page on this site is also served as Markdown: append `.md` to its path. The whole corpus is
available as [`llms.txt`](/llms.txt), [`llms-full.txt`](/llms-full.txt), and
[`llms-small.txt`](/llms-small.txt), and the daemon's wire schema is published as
[`schema.json`](/schema.json). [For agents](/agents/) explains which to fetch for which question.
