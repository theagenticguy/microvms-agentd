---
name: Platform finding
about: You measured AWS Lambda MicroVMs behaving differently from docs/PLATFORM.md
title: 'platform: '
labels: platform
---

`docs/PLATFORM.md` records observations of someone else's system, so it drifts:
AWS changes behavior, and a claim measured once in one region was never a
guarantee. A contradicting measurement is one of the most useful things you can
file here — but only if it carries enough context to be re-checked, since the
existing entry has a date and yours has to be comparable to it.

This form is also the right place for behavior `docs/PLATFORM.md` says nothing
about, if you measured it.

## Which claim, and what you measured instead

<!-- Quote the PLATFORM.md line you are contradicting, or say "not documented". -->

## Measurement context — all four required

- **Date measured:**
- **Region:**
- **API version:** <!-- e.g. 2025-09-09 -->
- **Base image and baseline memory:** <!-- e.g. al2023-1 aarch64, 1024 MiB -->

Without a date and region this cannot be reconciled with the existing entry, and
we will ask for them before acting.

## Reproduction

<!-- The AWS CLI calls, boto3 snippet, or conformance/probe_*.py invocation that
shows it. Something a reader can run to see the same thing. -->

## Evidence

<!-- Raw API response, stateReason, or CloudWatch lines. Redact account IDs,
ARNs you would rather not share, and tokens. Paraphrased evidence is not evidence:
an exact error string is often the whole finding — "Malformed network connector
ARN" is how we learned connectors are ARNs. -->

## What it changes for a consumer

<!-- Does a documented workaround become unnecessary, or does correct client code
become wrong? If it means a daemon behavior is now incorrect, say so — that turns
this into a protocol change rather than a documentation update. -->

## Did any local tier catch it?

<!-- Almost certainly not, which is the point: five defects in this project's
history were wrong assumptions about the service that no local tier could see. If
you can suggest a transport-tier scenario that would fail against the old
behavior, say what it is — that is how the last five were closed rather than just
fixed. -->
