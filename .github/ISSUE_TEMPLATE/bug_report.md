---
name: Bug report
about: The daemon or the Python client does something other than what docs/PROTOCOL.md says
title: ''
labels: bug
---

<!-- For a suspected vulnerability, use SECURITY.md instead of this form. Note that
a token holder running arbitrary code as root is by design, not a bug. -->

## What happened, and what the contract says should happen

<!-- Quote the rule from docs/PROTOCOL.md if there is one. -->

## Reproduction

<!-- Smallest sequence of requests or client calls that shows it. A failing test
against this tree is the strongest form of this. -->

## Where it ran

- [ ] Locally against the daemon binary
- [ ] Inside a real Lambda MicroVM

If in a MicroVM: region, API version, base image, baseline memory.

## Versions

- Commit or tag:
- Build target (`x86_64-unknown-linux-gnu`, `aarch64-unknown-linux-musl`, …):
- `clients/python` version, if involved:

## Logs

<!-- Daemon stdout, or the CloudWatch lines from /aws/lambda-microvms/<image-name>.
If a launch died before you could reach it, GetMicrovm's stateReason is where the
answer usually is. Redact tokens. -->

## Which tiers you ran

<!-- `cargo test --all` result, and whether any existing test already catches this.
If none do, that gap is part of the report. -->
