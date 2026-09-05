---
title: Prefetch S3 content at image build
description: Bake an S3 prefix into the image snapshot during the build, so every VM launched from it starts with the data on disk and makes no S3 call at all.
editUrl: false
sidebar:
  order: 5
---

```bash
# from the repo root, with the first-run prerequisites in place:
PREFETCH_URI=s3://my-bucket/models/ bash examples/s3-prefetch-at-build/run.sh
# public bucket, zero credentials:
PREFETCH_URI=s3://a-public-bucket/prefix/ PREFETCH_NO_SIGN=1 bash examples/s3-prefetch-at-build/run.sh
```

A fresh VM's first S3 call carries a measured five-to-ten-second penalty, and a workload that starts by pulling model weights, a dataset, or a toolchain pays that plus the whole transfer on every launch. This recipe moves both to image-build time. At the end of this page you will have an image whose snapshot already holds the prefix, and a VM launched from it without `--egress` that reads the data from disk.

## 1. Why it works

An image build boots the image in a snapshot VM, calls the build-time hooks against the daemon, and captures the memory and disk snapshot only after they answer. So anything that completes before the daemon answers those hooks is inside the snapshot. The wrapper here runs `aws s3 sync` first and only then hands the process to agentd, strictly ordered: the daemon cannot answer a hook until the sync is done, and the snapshot cannot be captured until the daemon answers.

Two shapes of the same rule, observed in [Platform](/internals/platform/). The model allows build hooks an hour, and an observed failure reads `Ready hook invocation timed out after PT5M`, so plan the transfer to fit well inside five minutes and measure before assuming the full hour is reachable. And one build runs the snapshot pass once per chipset generation, so the prefetch downloads twice per image; a launch restores the snapshot rather than re-running `CMD`, so it never prefetches, which is the entire point.

## 2. The Dockerfile pieces

The base and the daemon lines are the same as every other guest Dockerfile. AWS CLI v2 runs the prefetch, and AL2023 packages it as `awscli-2`. The URI and the sign mode are `ENV` rather than `ARG` because they are read at app start, in the snapshot VM, and because baking them in is what keys the image to its content:

```dockerfile
RUN dnf install -y awscli-2 \
    && dnf clean all

ENV PREFETCH_URI=""
ENV PREFETCH_NO_SIGN="0"
```

The start wrapper is materialized with `printf`, because the build context `microvm build` uploads carries exactly the Dockerfile and the daemon binary, so there is no file beside them to `COPY`:

```dockerfile
RUN printf '%s\n' \
    '#!/usr/bin/env bash' \
    'set -euo pipefail' \
    'if [ -n "${PREFETCH_URI:-}" ]; then' \
    '  echo "prefetch: ${PREFETCH_URI} -> /opt/prefetch"' \
    '  args=(s3 sync --no-progress "${PREFETCH_URI}" /opt/prefetch)' \
    '  if [ "${PREFETCH_NO_SIGN:-0}" = "1" ]; then args+=(--no-sign-request); fi' \
    '  aws "${args[@]}"' \
    '  chown -R 1000:1000 /opt/prefetch' \
    '  du -sh /opt/prefetch' \
    'else' \
    '  echo "prefetch: PREFETCH_URI is empty, skipping"' \
    'fi' \
    'exec /agentd' \
    > /start.sh && chmod 0755 /start.sh
```

The `CMD` becomes `["/start.sh"]` with `ENTRYPOINT []` kept. `run.sh` rewrites the two `ENV` lines with your values before building, so each distinct URI is a different Dockerfile hash, a different image name, and a fresh build, and re-running with the same URI reuses the already-prefetched image in seconds.

## 3. Fail loud, never empty

The wrapper is `set -e`. A failed sync means agentd never starts, the ready hook times out, and the build fails with a named reason instead of producing an image whose snapshot silently lacks the data. The failure signature is `stateReason: Ready hook invocation timed out after PT5M` on the build record, and the wrapper's own log lines are in the build log group; `microvm logs <image-name>` prints the `aws logs tail` command that reads them. An empty `PREFETCH_URI` skips the prefetch with a log line, so the Dockerfile also builds as checked in, and `run.sh` refuses to run without a URI so the demo cannot pass by fetching nothing.

## 4. Credentials at build time

The prefetch runs inside the build's snapshot VMs, under whatever network and credentials the build environment provides. What the repository has established: the build's docker-build VM has outbound network, because every `dnf install` in the examples runs there. Whether the snapshot VMs have the same egress, and whether the build role's credentials are visible to the app process there, has not been measured.

`PREFETCH_NO_SIGN=1` for a public bucket is the zero-assumption mode. For a private bucket, the AWS CLI walks its default chain: grant the build role read on the prefix and try it, and if the build fails with the `PT5M` signature and the wrapper's log shows a credential or network error, that is a real finding about the snapshot VMs. The conservative fallback is to run the sync as a docker `RUN` step instead, which executes in the docker-build VM where outbound network is demonstrated and reaches the launched VM through the same disk snapshot.

## 5. Do not prefetch secrets

The snapshot is shared by every VM launched from the image, so anything prefetched into it is readable by all of them. Prefetch data, models, and toolchains; deliver tokens and keys at launch through `runHookPayload` or `microvm cp` after bootstrap, the way [Run coding agents on Bedrock](/learn/operations/run-coding-agents-on-bedrock/) does.

## 6. Prove it at launch

```bash
microvm run --json --keep --image "$IMAGE_ARN" --config microvm.toml
microvm exec "du -sh /opt/prefetch && ls -la /opt/prefetch | head -20" \
  --user 1000 --group 1000 "${ATTACH[@]}" --timeout 60
```

The example's `microvm.toml` pins `egress = false` and `memory = 512`: the launched VM needs no outbound network to have the data, and the demo only proves the tree is present. Running the check as uid 1000 also proves the wrapper's `chown` did its job.

## 7. Rebuilds and cost

The prefetched bytes live in the image snapshot, which bills storage with a one-week minimum retention, and every launch pays a snapshot read scaled by its size. So prefetch what the workload reads at startup, and never the whole bucket. Delete retired images with `aws lambda-microvms delete-microvm-image`. [Read the cost report](/learn/operations/read-the-cost-report/) has the snapshot line items.
