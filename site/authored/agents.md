---
title: For agents
description: What to unlearn about this project, which surface answers which question, and the shortest path from a clone to a command running inside a MicroVM.
editUrl: false
sidebar:
  order: 1
---

This page is addressed to an AI agent working with microvms-agentd, and it is written so the person
reading over your shoulder can read it too. Every claim on it points at behavior a gate checks.

## 1. `microvm manifest` outranks this page

`microvm manifest` prints the binary's whole contract in one call: every command, its arguments and
flags, its response type, the envelope schema, and the exit-code catalog a caller branches `$?` on.
It is always JSON — the dispatcher forces that for this subcommand whatever `--json` says — and it needs
no credentials, no AWS region, and no network, so it doubles as a liveness check on your build.

The daemon has the same property from the other side: `GET /v1/schema` on any running daemon returns the
wire contract, and the committed copy at [`schema.json`](/schema.json) is byte-compared against it by a
gate in `mise run check`. A page describing a route is a reading of that schema; the schema is the
schema.

:::agent

**For an agent.** Prefer `microvm manifest` and `/v1/schema` to this page wherever the two could
disagree. Both are generated from the source of truth, so they are right and this page is stale. This
page adds what a schema cannot state: what a silence means, and which surface you are on.

:::

## 2. Gotchas

Each of these is a place where this project behaves unlike the system you are pattern-matching it to.

- **Three tiers, and the reliability rule inside one of them.** [Learn](/learn/) is task-shaped.
  [Reference](/reference/) is generated from `microvm manifest`: one page per command at
  `/reference/commands/<name>/`, the [exit codes](/reference/exit-codes/), the envelope, the response
  types, and the wire schema, so a flag or code there is the one the binary ships. [Internals](/internals/)
  is the reasoning, and two kinds of document live there that are not equally reliable. The hand-written
  documents (Platform, Protocol, Trust, Embedding, Strategy, Harness capabilities) carry measured
  findings and design rationale, and they win any disagreement. The generated categories (Architecture,
  Behavior, Analysis, Diagrams, Insights) were produced per-file from the source tree; every factual
  claim there carries a `path:line` citation that was machine-verified at generation time, and every
  citation on this site links to that line pinned to a commit. A pin is not freshness: the citation was
  true at that commit and the line may hold different code in the checkout you are editing.

- **Branch on the exit code, never on the message.** The catalog is append-only and is the contract;
  the prose beside each code is rewritten freely. A matcher over the prose breaks on a wording change
  that broke nothing. `ERR_EXEC_FAILED` in particular means the sandbox worked and your command inside
  it exited non-zero — a successful platform interaction with a failing payload, which is not the same
  condition as `ERR_PROTOCOL` or `ERR_LAUNCH_DIED`.

- **The absent exec API is the premise, not a gap to route around.** The service hands a MicroVM one
  HTTPS endpoint and forwards it to whatever the image's `CMD` is listening on. That is why a daemon is
  baked into the image, and why `ENTRYPOINT []` plus `CMD ["/agentd"]` is a deployment invariant rather
  than a convention: it guarantees no task workload runs before the platform's run hook lands. A base
  image that starts its own background process before bootstrap breaks the trust boundary, and only
  whoever builds the image can enforce that — the daemon cannot.

- **A secret never goes in the image.** The image becomes a shared snapshot, so every VM launched from it
  sees the same bytes. Per-VM credentials travel through `runHookPayload` at launch.

- **`mise run live` spends money.** It creates real MicroVMs in your account and takes roughly a quarter
  of an hour. `mise run check` is the offline, free gate and is the definition of done here; it is what
  you run to find out whether a change is sound. Nothing about `check` passing says anything about the
  formal requirements under `spec/`, which sit outside it and need an environment neither task provides.

- **A platform claim without a date, a region, and an API version is not a claim on this site.**
  Contradictions in Platform are appended and never deleted, so where two findings disagree the newest
  one is last, and both are there on purpose.

## 3. Which surface you are on

Read your own tool list and environment to find your surface. Do not run a command and inspect the
failure — every test below is free.

| Signal                                                      | Surface   | Entry point                                                        |
| ----------------------------------------------------------- | --------- | ------------------------------------------------------------------ |
| a `microvm` binary is on `PATH`, or you can run `mise`       | CLI       | `microvm manifest`, then the command you need                      |
| you are editing Rust, Python, or Node inside this workspace  | library   | `microvms-core`, or the thin bindings over it                      |
| you hold a running VM's endpoint and its agent token         | wire      | `GET /v1/schema`, then the route                                   |
| none of those                                                | read-only | you are reading documentation. Fetch the Markdown, not the HTML.   |

The three active surfaces share one implementation, so a fact learned on any of them transfers: the CLI
is a thin shell over `microvms-core`, the Python and Node bindings are thin shells over the same crate,
and all three speak the one wire protocol to the daemon. The dependency direction is enforced rather
than documented — `cli` depends on `core` depends on `protocol`, bindings depend on `core`, the daemon
depends on `protocol`, and a drift in the wire types is a compile error.

:::agent

**For an agent.** If you can run a shell command, take the CLI surface and start with `microvm manifest`.
It is the only entry point that answers with the whole contract and costs nothing to call.

:::

## 4. The shortest path to a working integration

One artifact gets you running: the `microvm` CLI. The daemon binary that gets baked into VM images is
the CLI's own component, and `run`, `build`, and `quickstart` provision the release asset for their own
version, verify it, and cache it under `~/.microvm`. It is a static `aarch64-unknown-linux-musl` build
because Lambda MicroVMs are ARM64-only.

```bash
cargo binstall microvms-cli           # installs the `microvm` binary, prebuilt
microvm doctor                        # credentials, region, the three environment values
microvm quickstart                    # build, launch, run a hello-world, report the cost, tear down
```

`doctor` is the step to run before anything bills: it names the broken prerequisite and suggests the fix
rather than failing partway through a build. `quickstart` is exactly
`microvm run --exec "echo hello from a microvm"`: it builds an image, launches a VM from it, executes
the command, reports the cost, and tears the VM down. Teardown is the default, so an interrupted session
leaves no billable VM behind. Expect the first run to take minutes, almost all of it the image build;
the snapshot carries a one-week minimum retention, so reuse it with `--image` rather than rebuilding.

A real MicroVM image build needs an S3 bucket, a build role, and an execution role. `mise run live:infra`
creates exactly those three and nothing else; [Install](/learn/tutorial/install/) walks it, and
[Embedding](/internals/embedding/) is the page to read when you are baking the daemon into your own
task image instead. Building from source stays one task, `mise install && mise run build`, for a custom
daemon or an airgapped machine.

Two habits change the shape of what you get back. `--json` wraps every response in the envelope the
manifest declares, and it is what makes output parseable without a schema of your own. `--stream` on
`exec` returns NDJSON as output arrives rather than one document at the end — the one documented
exception to the envelope, declared in the manifest under its own response type.

## 5. Working in this repository

- **Scraping these pages.** Every one of them is served as Markdown and section 7 has the URLs. Scraping
  the rendered HTML buys navigation chrome, a search widget, and a theme toggle to read three paragraphs.

- **Reaching for `mise run live` to check something.** It is billable and slow. Every gate that can run
  offline already does, inside `mise run check`.

- **Piping a gate into `head` or `tail`.** The pipeline exits with the pager's status, so a failing tier
  reads as success: `mise run check | tail` returns 0 while a failed tier scrolls past. Run it bare, or
  read `${PIPESTATUS[0]}`.

- **Grepping for `TODO`, `HACK`, or `FIXME`.** No Rust or Python file in this repository carries one, so
  the search finds nothing and proves nothing. Tech debt is registered on its own page under Internals,
  and declined scope lives in [Strategy](/internals/strategy/).

- **Acting on a `path:line` citation without opening it.** Follow the link. It is pinned to the commit
  the page was generated from, which is what makes it verifiable and also what makes it capable of
  disagreeing with your working tree.

- **Editing a generated page.** The Internals categories (Architecture, Behavior, Analysis, Diagrams,
  Insights) and the annotated reference pages are generated from `docs/`, and `docs/` is generated
  from the source tree. The rest of Reference is generated from `microvm manifest`. An edit to any of
  them is discarded by the next build with no diff to show for it. Regenerate the affected file
  instead, and let a hand-written document win any disagreement.

- **Passing `ruff` a list of directories.** It discovers by extension when it walks one, so naming
  directories silently lints a fraction of the tree and reports success. The selection lives in
  `ruff.toml` and the invocation is `ruff check .`.

## 6. Read next

Every published page, tier by tier, with its rendered URL beside the raw Markdown twin an agent should
fetch instead.

<!--READ-NEXT-->

## 7. The machine surfaces

Any page is available as Markdown: append `.md` to its path. `/reference/cli/` is served at
`/reference/cli.md` as `text/markdown`. This holds for every page on the site, and each page links its
own twin from `<head>` with `rel="alternate" type="text/markdown"`.

The whole site comes three ways. [`llms.txt`](/llms.txt) is the index, and it lists this page first.
[`llms-full.txt`](/llms-full.txt) is every page in one file. [`llms-small.txt`](/llms-small.txt) is the
same corpus with non-essential content removed.

The wire schema is published as a file of its own at [`schema.json`](/schema.json) — the same bytes a
running daemon serves at `GET /v1/schema`, and the same bytes a gate in `mise run check` compares.

:::agent

**For an agent.** When you already know which page you want, fetch that page's `.md` twin rather than
either bundle: it is a fraction of the tokens and the same text. `llms-small.txt` before
`llms-full.txt` when you need the whole corpus — but measure both against your context window first,
because the difference between them is a fraction rather than an order of magnitude.

:::
