#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Claude Code and Codex CLI, inside a Lambda MicroVM, against Bedrock.
#
# What this does, end to end:
#   1. builds a MicroVM image carrying agentd + both CLIs (first run only),
#   2. launches a VM from it with outbound network (--egress),
#   3. mints a short-lived Bedrock API key locally and copies it in,
#   4. runs each agent headless inside the VM through `microvm exec`,
#   5. terminates the VM.
#
# Prerequisites (see the repo README's Getting started):
#   - `microvm` on PATH and the aarch64 agentd binary built,
#   - MICROVM_BUCKET / MICROVM_BUILD_ROLE_ARN / MICROVM_EXECUTION_ROLE_ARN set,
#   - AWS credentials that can call lambda-microvms and bedrock:InvokeModel,
#   - uv on PATH (mints the Bedrock bearer token).
#
# Model access: the account must have Bedrock access to the two models below;
# override with CLAUDE_MODEL / CODEX_MODEL.
set -euo pipefail

REGION="${AWS_REGION:-us-east-1}"
AGENTD="${AGENTD:-target/aarch64-unknown-linux-musl/release/agentd}"
# Opus 5 via the global inference profile; Sol carries a 1M-token context
# window on Bedrock (context length is a model property, not a model-id
# variant — there is no separate "-1m" id).
CLAUDE_MODEL="${CLAUDE_MODEL:-global.anthropic.claude-opus-5}"
CODEX_MODEL="${CODEX_MODEL:-openai.gpt-5.6-sol}"
HERE="$(cd "$(dirname "$0")" && pwd)"

jqr() { python3 -c "import json,sys; print(json.load(sys.stdin)['data']$1)"; }

# ── 1. the image: build once per content hash, reuse otherwise ──────────────
# The image snapshot has a one-week minimum retention, so rebuilding on every
# run costs money for nothing. Reusing a FIXED name is the trap: deleting
# and recreating an image under the same name can serve a stale snapshot
# (measured; it is the same class of hazard as the clientToken replay in
# docs/PLATFORM.md). `build --reuse` closes both at once: it keys the image
# name to a hash of the build inputs (the agentd binary AND the Dockerfile —
# strictly better than the Dockerfile-only hash this script used to compute),
# skips the build when that name already exists, and reports the image ARN
# either way. On a hit the envelope carries `reused: true` and returns in
# seconds; on a miss the server-side build takes several minutes.
echo "resolving image (builds server-side on first run, several minutes)..." >&2
BUILT=$(microvm build "$AGENTD" --json --reuse \
  --name coding-agents \
  --dockerfile "$HERE/Dockerfile" \
  --region "$REGION")
IMAGE_ARN=$(echo "$BUILT" | jqr "['imageIdentifier']")

# ── 2. launch, keep, with egress ────────────────────────────────────────────
# --egress is what lets the guest reach bedrock-runtime; without it the VM has
# no outbound network and both agents fail on their first model call.
echo "launching..." >&2
LAUNCH=$(microvm run --json --keep --egress --image "$IMAGE_ARN" --region "$REGION")
EP=$(echo "$LAUNCH"    | jqr "['endpoint']")
TOK=$(echo "$LAUNCH"   | jqr "['agentToken']")
ID=$(echo "$LAUNCH"    | jqr "['microvmId']")
ATTACH=(--endpoint "$EP" --agent-token "$TOK" --microvm-id "$ID" --region "$REGION")
trap 'microvm terminate "$ID" --region "$REGION" >/dev/null || true' EXIT
echo "microvm: $ID" >&2

# ── 3. credentials: a short-lived Bedrock API key, never baked in ──────────
# aws-bedrock-token-generator turns the caller's AWS credentials into a
# bearer token (~12 h). Both CLIs take it: Claude Code as
# AWS_BEARER_TOKEN_BEDROCK, Codex as the API key for Bedrock's
# OpenAI-compatible endpoint. The token reaches the VM as a file copied over
# the authenticated channel, so it is never in an image, a command line, or
# a daemon log.
BEARER=$(uvx --from aws-bedrock-token-generator@latest --quiet python -c \
  "from aws_bedrock_token_generator import provide_token; print(provide_token(region='$REGION'))")

# PATH is load-bearing: the daemon spawns execs with a minimal environment,
# and Claude Code's Bash tool snapshots the shell it starts from. Without an
# explicit PATH the agent's subshells find no ls, wc, or python3 (exit 127 on
# everything) while the daemon's own execs work fine.
ENVFILE=$(mktemp)
cat > "$ENVFILE" <<ENV
export HOME=/workspace
export PATH=/usr/local/bin:/usr/bin:/bin
export AWS_REGION=$REGION
export AWS_BEARER_TOKEN_BEDROCK=$BEARER
export CLAUDE_CODE_USE_BEDROCK=1
export ANTHROPIC_MODEL=$CLAUDE_MODEL
export OPENAI_API_KEY=$BEARER
ENV
microvm cp "$ENVFILE" vm:/workspace/.agent-env --mode 0600 "${ATTACH[@]}" --json >/dev/null
rm -f "$ENVFILE"

# Codex reads its provider from config.toml. Current Codex speaks only the
# Responses wire API, and on Bedrock that lives on the Mantle host
# (bedrock-mantle.<region>.api.aws), not on bedrock-runtime; the
# bedrock-runtime /openai/v1 surface is chat-completions only, which Codex
# dropped. Same bearer token works on both hosts.
CODEXCFG=$(mktemp)
cat > "$CODEXCFG" <<CFG
model = "$CODEX_MODEL"
model_provider = "bedrock"
[model_providers.bedrock]
name = "Amazon Bedrock (Mantle)"
base_url = "https://bedrock-mantle.$REGION.api.aws/openai/v1"
env_key = "OPENAI_API_KEY"
wire_api = "responses"
CFG
microvm exec "mkdir -p /workspace/.codex" "${ATTACH[@]}" --json >/dev/null
microvm cp "$CODEXCFG" vm:/workspace/.codex/config.toml --mode 0600 "${ATTACH[@]}" --json >/dev/null
rm -f "$CODEXCFG"

# ── 4. run both agents, headless, inside the VM ─────────────────────────────
echo "--- claude code ---"
microvm exec ". /workspace/.agent-env && claude -p 'Write a one-line bash command that counts files in /usr/bin, then run it with your Bash tool and report the number.' --allowedTools Bash" \
  "${ATTACH[@]}" --timeout 240

echo "--- codex ---"
microvm exec ". /workspace/.agent-env && codex exec --skip-git-repo-check -s workspace-write 'Create hello.py that prints hello from a microvm, run it, and show the output.'" \
  "${ATTACH[@]}" --timeout 240

echo "--- proof of work ---"
microvm exec "cat /workspace/hello.py 2>/dev/null; ls -la /workspace" "${ATTACH[@]}" --timeout 60

# ── 5. teardown happens in the trap ─────────────────────────────────────────
echo "terminating $ID..." >&2
