#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# S3 prefetch at image-build time: bake the data into the snapshot, so a
# launched VM's "first S3 call" is a local disk read.
#
# What this does, end to end:
#   1. renders the Dockerfile with your PREFETCH_URI baked in,
#   2. builds a MicroVM image — the prefetch runs server-side, during the
#      build's snapshot pass, before the snapshot is captured,
#   3. launches a VM WITHOUT --egress and proves the prefetched tree is
#      already there, readable, with no network and no S3 call,
#   4. terminates the VM.
#
# Usage:
#   PREFETCH_URI=s3://my-bucket/my-prefix/ bash examples/s3-prefetch-at-build/run.sh
#   PREFETCH_NO_SIGN=1 for a public bucket (no credentials involved at all).
#
# Prerequisites (see the repo README's Getting started):
#   - `microvm` on PATH and the aarch64 agentd binary built,
#   - MICROVM_BUCKET / MICROVM_BUILD_ROLE_ARN / MICROVM_EXECUTION_ROLE_ARN set,
#   - AWS credentials that can call lambda-microvms,
#   - an S3 prefix the image build can read (see the README's credentials
#     section for what "can read" means at build time).
set -euo pipefail

REGION="${AWS_REGION:-us-east-1}"
AGENTD="${AGENTD:-target/aarch64-unknown-linux-musl/release/agentd}"
PREFETCH_NO_SIGN="${PREFETCH_NO_SIGN:-0}"
HERE="$(cd "$(dirname "$0")" && pwd)"

if [ -z "${PREFETCH_URI:-}" ]; then
  echo "set PREFETCH_URI to the s3:// prefix to bake in, e.g." >&2
  echo "  PREFETCH_URI=s3://my-bucket/models/ $0" >&2
  exit 2
fi

jqr() { python3 -c "import json,sys; print(json.load(sys.stdin)['data']$1)"; }

# ── 1. render the Dockerfile ────────────────────────────────────────────────
# The URI is baked in as ENV rather than passed at launch, deliberately: the
# prefetch runs during the build, and the image name is keyed to the
# Dockerfile's content hash — so each distinct URI gets its own image, and
# re-running with the same URI reuses the already-prefetched one.
RENDERED=$(mktemp)
trap 'rm -f "$RENDERED"' EXIT
sed -e "s@^ENV PREFETCH_URI=.*@ENV PREFETCH_URI=\"$PREFETCH_URI\"@" \
    -e "s@^ENV PREFETCH_NO_SIGN=.*@ENV PREFETCH_NO_SIGN=\"$PREFETCH_NO_SIGN\"@" \
    "$HERE/Dockerfile" > "$RENDERED"

# ── 2. build: the prefetch happens here, server-side ────────────────────────
# If the sync fails (bad URI, unreadable bucket), the wrapper exits before
# starting agentd, the build's ready hook times out, and the build FAILS —
# check `microvm logs` / the build log group for the wrapper's own output.
echo "resolving image (the prefetch runs inside the build; several minutes on first use)..." >&2
BUILT=$(microvm build "$AGENTD" --json --reuse \
  --name s3-prefetch \
  --dockerfile "$RENDERED" \
  --region "$REGION")
IMAGE_ARN=$(echo "$BUILT" | jqr "['imageIdentifier']")

# ── 3. launch with no egress, and prove the data is already there ──────────
# microvm.toml pins egress = false: the point of the recipe is that the
# launched VM needs no outbound network to have the data.
echo "launching (no egress)..." >&2
LAUNCH=$(microvm run --json --keep --image "$IMAGE_ARN" \
  --config "$HERE/microvm.toml" --region "$REGION")
EP=$(echo "$LAUNCH"  | jqr "['endpoint']")
TOK=$(echo "$LAUNCH" | jqr "['agentToken']")
ID=$(echo "$LAUNCH"  | jqr "['microvmId']")
ATTACH=(--endpoint "$EP" --agent-token "$TOK" --microvm-id "$ID" --region "$REGION")
trap 'rm -f "$RENDERED"; microvm terminate "$ID" --region "$REGION" >/dev/null || true' EXIT
echo "microvm: $ID" >&2

echo "--- prefetched at build, read at launch, zero S3 calls ---"
# As uid 1000, which also proves the chown in the wrapper did its job.
microvm exec "du -sh /opt/prefetch && ls -la /opt/prefetch | head -20" \
  --user 1000 --group 1000 "${ATTACH[@]}" --timeout 60

# ── 4. teardown happens in the trap ─────────────────────────────────────────
echo "terminating $ID..." >&2
