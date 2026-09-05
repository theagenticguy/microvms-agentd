---
title: Learn
description: Tutorials that take you from an empty machine to a project running inside a Lambda MicroVM, and task-shaped pages for operating one.
editUrl: false
sidebar:
  order: 0
---

This topic gets a command running inside a MicroVM and then keeps the VM, the image, and the bill under control.

## 1. The tutorials

The tutorials are one path, in order. Each ends with more than the one before it, and every command shown is a command you can run.

1. [Install the CLI](/learn/tutorial/install/): `cargo binstall microvms-cli`, where the daemon binary comes from, the three libraries, and `microvm manifest` as the check that needs no credentials.
2. [Run your first command in a MicroVM](/learn/tutorial/first-run/): the AWS prerequisites, the environment values the CLI reads, `microvm doctor`, and `microvm quickstart`, then what happened step by step and what it cost.
3. [Keep a VM running and work inside it](/learn/tutorial/long-lived-vm/): `run --keep --vm-name`, then `exec`, `cp`, `suspend`, `resume`, and `terminate` against a name instead of an identifier triple.
4. [Run a project through a VM](/learn/tutorial/run-a-project/): `microvm run . --image ... --exec ...`, what is packed and what stays home, `artifacts` globs, and `build --project` for an image that already carries your dependencies.
5. [Drive it from code](/learn/tutorial/from-code/): the same lifecycle as a Rust crate, a Python package, and a Node package, with the typed example the repository ships.

## 2. The operations pages

The operations pages are task-shaped. They assume the CLI is installed and the AWS prerequisites exist, and each answers a question you arrived with.

| Page                                                                                         | Answers                                                                                        |
| -------------------------------------------------------------------------------------------- | ---------------------------------------------------------------------------------------------- |
| [Write a guest Dockerfile](/learn/operations/write-a-guest-dockerfile/)                      | How to start from the generated stanza, and which traps cost a server-side build cycle          |
| [Embed agentd in your own image](/learn/operations/embed-agentd-in-your-image/)              | How to append the daemon to a task image your own harness drives                                |
| [Run coding agents on Bedrock](/learn/operations/run-coding-agents-on-bedrock/)              | How Claude Code and Codex CLI run headless inside a VM with no vendor API key                    |
| [Remote dev with code-server](/learn/operations/remote-dev-with-code-server/)                | How to reach VS Code in a browser through `port-forward`, on a VM that suspends when you leave   |
| [Prefetch S3 content at image build](/learn/operations/prefetch-s3-at-build/)                | How to bake an S3 prefix into the snapshot so a launched VM makes no S3 call                     |
| [Read the cost report](/learn/operations/read-the-cost-report/)                              | What each line means, why a total may read "at least", and how to plan with `microvm cost`       |
| [Debug a failed build](/learn/operations/debug-a-failed-build/)                              | Where the reason lives, how to read the build log, and what `ERR_BUILD_WEDGED` means             |
| [Recover a leaked VM](/learn/operations/recover-a-leaked-vm/)                                | What `ls` and `history` say you left behind, and how to ask the account directly                 |
| [Configure the project file](/learn/operations/configure-the-project-file/)                  | Every `microvm.toml` key, which source wins, and what the loader refuses                         |
| [Drive it from a script or an agent](/learn/operations/drive-it-from-a-script-or-an-agent/)  | The one-envelope rule, the exit codes, the manifest, and the streaming exception                 |
| [Run the live suite](/learn/operations/run-the-live-suite/)                                  | What `mise run check` proves, what `mise run live` adds, what it costs, and how to leave the account clean |

## 3. Before you start

Every command takes `--json` and then writes exactly one JSON envelope on stdout; progress goes to stderr. A success envelope carries `type` and `data`. A failure envelope carries a stable `code`, an `exitCode` that matches `$?`, a `finding` naming the section of [Platform](/internals/platform/) that measured the behavior, and `suggestions`. Branch on `code`, never on the `error` text. The one exception is `exec --stream`, which writes NDJSON events and the envelope last, under its own `type`. [Drive it from a script or an agent](/learn/operations/drive-it-from-a-script-or-an-agent/) develops this.

Teardown is the default. `microvm run` builds an image, launches a VM, runs your command, reports the cost, and tears the VM down, so an interrupted session does not leave a billable VM behind. `--keep` opts out and hands you the identifiers you have just taken responsibility for. The image is the durable artifact: its snapshot has a one-week minimum retention, so deleting it early saves nothing and reusing it with `--image` is the economical habit.

If you are an AI agent rather than a person reading a page, start with `microvm manifest`. It prints every command, flag, response type, and exit code this binary accepts, as JSON, on a machine with no credentials and no network. [For agents](/agents/) names which surface answers which question. Come back here for the worked paths.
