---
title: Run the live suite
description: What mise run check proves and cannot prove, what mise run live adds and costs, how to run the conformance suite by hand, and how to confirm the account is clean afterwards.
editUrl: false
sidebar:
  order: 11
---

```bash
mise run install         # once per clone: the git hooks
mise run check           # every local gate. Offline, free, no AWS.
mise run live            # the real-AWS suites. BILLABLE, about fifteen minutes. Deliberate only.
mise run live:verify-clean
```

This page is for contributors. At the end of it you will know what `mise run check` proves, why a green `check` says nothing about the platform, what `mise run live` runs and what it costs, and how to leave the account clean.

## 1. Two tiers, split by cost

`mise run check` is the definition of done for a change: lint, security, every Rust test tier, the schema and stub freshness gates, the model-drift gate against the pinned botocore service model, the publish-set check, the live tier's own wiring check, and the release cross-compile. It runs offline, with no network, no credentials, and no money. The pre-push hook and CI both run it.

`mise run live` is the set of suites that talk to real AWS. It creates real MicroVMs in your account and costs money whether it passes or fails, so it is never wired to a hook or to push. A gate that spends money on every push is a gate people disable with `--no-verify`, and `--no-verify` also skips the checks worth having. The hook does print an advisory when the daemon has changed since the last recorded live run.

Never pipe a gate into `head` or `tail`. The pipeline exits with the pager's status, so a failing tier reads as success. Run it bare, or read `${PIPESTATUS[0]}`.

## 2. Why a green check is necessary and never sufficient

The local gate proves the code agrees with itself; only a live run can prove it agrees with AWS. This repository's history is a list of things every local test passed while being wrong: the null-message unsupported-region trap, the `clientToken` replay that wedges an image, the proxy-token port scoping, and an id-prefix defect where fixtures spelled MicroVM ids `mvm-*`, the real service spells them `microvm-*`, and a resolution path keyed on the fixture prefix passed every unit test and refused every real VM on its first live run. A fixture convention is not a service fact.

So a feature that touches the platform surface is not verified until it has run against real AWS, and the closing discipline is four steps: `mise run check` green; a live exercise of the new path itself, the full suite or a targeted round trip with the real binary; a permanent named check in `conformance/run_rs.py`, so the live tier covers the surface on every future run; and `mise run live:verify-clean` afterwards. A pull request that skips the second step says so in its body, in plain words, as an unverified claim. Purely local changes (rendering, docs, `ls`, `history`, `cost`) are exempt; when in doubt, it is not exempt.

## 3. Before a live run

```bash
mise run live:infra      # apply the conformance stack: bucket, build role, execution role
mise run build           # the aarch64 daemon
mise run build:cli       # the host-architecture microvm binary
```

Rebuild `target/release/microvm` explicitly: `check` does not build it, and a live run against a stale binary verifies nothing. `mise run live` depends on all three, so it does them for you; a targeted round trip by hand does not. `build --reuse` makes repeat image builds nearly free, which is what makes a targeted exercise cheap to repeat.

## 4. Run it

```bash
mise run live
```

In order: `live:paths` (the pagination cursor encoding and the colon image ARN against the real signer; read-only and free), `live:versions` (the version and build operations, including the one `PATCH` this client sends; costs one short VM), `live:conformance-rs` (the conformance suite through the real CLI against real VMs), then `live:rates` (the pinned rate table against the AWS Pricing API; free, and placed after the billable suite so a drifted rate cannot abort it mid-flight). The leak check runs in a shell trap so it fires on the failure path too, and on success the task records the commit as live-verified in a per-clone marker the pre-push hook reads.

To run the suite by hand:

```bash
terraform -chdir=conformance/infra init
terraform -chdir=conformance/infra apply
cargo build --release -p agentd --target aarch64-unknown-linux-musl
cargo build --release -p microvms-cli
conformance/run_rs.py \
  --binary target/aarch64-unknown-linux-musl/release/agentd \
  --microvm-binary target/release/microvm
```

`--binary` is the aarch64 daemon baked into the image, and `--microvm-binary` is the host CLI under test. Read the check count off the run's summary block rather than from any prose. `--keep` skips teardown and leaks everything, so use it only while debugging a failure you cannot reproduce otherwise.

`./conformance/run_rs.py --self-test` is the offline half. It drives the envelope-to-exception mapping against a stub `microvm`, touches no account, and belongs in any change to `conformance/`.

## 5. What it costs

The suite creates an S3 artifact, a real image build (with a timeout of up to forty-five minutes), and a running MicroVM. The image is the floor: its snapshot has a one-week minimum retention, so a 2 GB image built and deleted in one run still bills about a week of storage, roughly four cents. `live:versions` launches one short VM of its own. `live:paths` and `live:rates` are free. [Read the cost report](/learn/operations/read-the-cost-report/) has the rates.

## 6. Afterwards

```bash
mise run live:verify-clean
mise run live:destroy
```

Verify teardown independently, and do not trust a success message. The scripts delete the MicroVM, the image, and the log group in `finally`, and `terraform destroy` handles the stack, yet the service creates `/aws/lambda-microvms/<image-name>` itself, so Terraform never owns that log group and `destroy` reports success while the group survives. Six leaked that way before anyone noticed. `live:verify-clean` asks the account directly and separates leak, standing, and pending; [Recover a leaked VM](/learn/operations/recover-a-leaked-vm/) explains the three. `live:destroy` tears the Terraform stack back down when you are done.

## 7. Platform claims need a date, a region, and an API version

A live run that shows the platform behaving differently from [Platform](/internals/platform/) is a finding, and findings are appended rather than corrected. Every entry there carries when it was measured, in which region, under which API version, and whether it is this project's measurement or AWS documentation. If you contradict an existing entry, add your measurement with its date so the drift is visible.

## 8. The docs site

`mise run docs:check` builds and gates the site: install, sync, the brace gate, the build, the typecheck, and the probes over `dist/`. It is out of `check` because it needs a `pnpm install`, and a gate that fails on a fresh clone is a gate people learn to skip. Treat a green `check` as saying nothing about the site.
