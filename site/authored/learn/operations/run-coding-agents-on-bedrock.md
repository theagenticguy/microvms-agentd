---
title: Run coding agents on Bedrock inside a MicroVM
description: Run Claude Code and Codex CLI headless inside a Lambda MicroVM against Bedrock, with credentials minted from your own AWS identity and no vendor API key anywhere.
editUrl: false
sidebar:
  order: 3
---

```bash
# from the repo root, with the first-run prerequisites in place and uv on PATH:
examples/coding-agents-on-bedrock/run.sh
```

The script builds an image carrying both agent CLIs on first use, launches a VM with outbound network, mints a short-lived Bedrock bearer token from your AWS credentials, copies it in over the authenticated channel, drives each agent through `microvm exec`, prints their output, and terminates the VM. At the end of this page both agents will have completed a task inside a VM, and you will know why each line of the script is there.

## 1. Prerequisites

The `microvm` CLI on your `PATH`, the `MICROVM_BUCKET`, `MICROVM_BUILD_ROLE_ARN`, and `MICROVM_EXECUTION_ROLE_ARN` values from [your first run](/learn/tutorial/first-run/), AWS credentials that can call `lambda-microvms` and `bedrock:InvokeModel`, and `uv` on your `PATH` to mint the token. The account needs Bedrock access to the models the script defaults to; `CLAUDE_MODEL` and `CODEX_MODEL` override them.

## 2. The image

`Dockerfile` starts from the platform's `al2023-1` base pair, pinned by digest, carries the daemon exactly as the client's default Dockerfile does, then adds Node 22, python3, and the two CLIs:

```dockerfile
RUN dnf install -y nodejs22 npm python3 git tar gzip which findutils procps-ng \
    && dnf clean all
RUN npm install -g @anthropic-ai/claude-code @openai/codex \
    && npm cache clean --force
```

The script builds it with `--reuse`, so the image name is keyed to a hash of the daemon binary and the Dockerfile. An unchanged Dockerfile reuses its image in seconds; a changed one builds fresh under a new name, which is what avoids serving a stale snapshot under a reused name:

```bash
BUILT=$(microvm build "$AGENTD" --json --reuse \
  --name coding-agents \
  --dockerfile "$HERE/Dockerfile" \
  --region "$REGION")
```

The image contains no secret of any kind.

## 3. Launch with egress and a low floor

```bash
microvm run --json --keep --egress --image "$IMAGE_ARN" --config microvm.toml
```

`--egress` gives the VM outbound network, and without it the guest cannot reach Bedrock; both agents would install fine and then fail on their first model call. `--keep` leaves the VM running and reports the endpoint, agent token, and MicroVM id the attached commands need.

`microvm.toml` pins `memory = 1024`. The minimum you request is your bill floor, and four times it is the guest's always-present ceiling, with no scaling event. Agent sessions are peaky, long stretches of a small steady state punctuated by bursts of build and test work, so a 4 GiB ceiling at half the floor cost of the 2048 default fits them, and the peaks bill only by what is consumed. A steadier or heavier workload should keep the default.

## 4. Credentials, without a vendor key

The script mints a Bedrock bearer token from the caller's own AWS credentials with the `aws-bedrock-token-generator` package (roughly a twelve-hour lifetime) and delivers it as a file:

```bash
microvm cp "$ENVFILE" vm:/workspace/.agent-env --mode 0600 "${ATTACH[@]}" --json >/dev/null
```

The file both agents source:

```bash
export HOME=/workspace
export PATH=/usr/local/bin:/usr/bin:/bin
export AWS_REGION=$REGION
export AWS_BEARER_TOKEN_BEDROCK=$BEARER
export CLAUDE_CODE_USE_BEDROCK=1
export ANTHROPIC_MODEL=$CLAUDE_MODEL
export OPENAI_API_KEY=$BEARER
```

Claude Code has a native Bedrock mode: `CLAUDE_CODE_USE_BEDROCK=1` plus the bearer token, with the model chosen by `ANTHROPIC_MODEL` as an inference-profile id. Codex has no Bedrock mode, and Bedrock exposes an OpenAI-compatible surface, so a short `config.toml` defines a provider with the bearer token as its API key. Current Codex speaks only the Responses wire API, which lives on the Mantle host and not on `bedrock-runtime`:

```toml
model = "$CODEX_MODEL"
model_provider = "bedrock"
[model_providers.bedrock]
name = "Amazon Bedrock (Mantle)"
base_url = "https://bedrock-mantle.$REGION.api.aws/openai/v1"
env_key = "OPENAI_API_KEY"
wire_api = "responses"
```

The `PATH` line matters. The daemon spawns execs with a minimal environment, and Claude Code's Bash tool snapshots the shell it starts from: without an exported `PATH` the agent's subshells find no `ls`, `wc`, or `python3` (every command exits 127) even though the daemon's own execs resolve them. Codex probes absolute paths when lookup fails, so it limps through; Claude Code does not.

The token never appears in the image, a command line, or a daemon log. It lives in one file inside one VM and expires on its own.

## 5. Run as uid 1000, and why root silently breaks the agent

The Dockerfile creates uid and gid 1000 by appending to `/etc/passwd` directly, because `useradd` is not in the minimal base, and hands `/workspace` to it. The daemon runs as root, so everything `cp` wrote is root-owned `0600`, which a demoted agent cannot read; one root exec fixes that before any agent runs:

```bash
microvm exec "chown -R 1000:1000 /workspace" "${ATTACH[@]}" --json >/dev/null
```

Then every agent exec passes `--user 1000 --group 1000`:

```bash
microvm exec ". /workspace/.agent-env && claude -p 'Write a one-line bash command that counts files in /usr/bin, then run it with your Bash tool and report the number.' --allowedTools Bash" \
  --user 1000 --group 1000 "${ATTACH[@]}" --timeout 240

microvm exec ". /workspace/.agent-env && codex exec --skip-git-repo-check -s workspace-write 'Create hello.py that prints hello from a microvm, run it, and show the output.'" \
  --user 1000 --group 1000 "${ATTACH[@]}" --timeout 240
```

Omit the uid and the failure is worse than an error. Claude Code's `--dangerously-skip-permissions` refuses to run as root, and under `acceptEdits` the agent then denies its own Bash, Grep, and WebFetch calls and returns a confident report built on zero tool calls. Measured on this task shape: as uid 0 the agent made no shell calls at all; as uid 1000 it made 147. A final exec cats the file Codex created, so the transcript carries proof the agents did filesystem work inside the VM.

## 6. Longer sessions

The same primitives compose. `exec --detach` starts a run and returns, `exec --poll <EXEC_ID>` reads it back, `cp --tar` moves a whole project in and out, and `suspend` and `resume` freeze a VM between turns with its filesystem and processes intact. A multi-hour run needs an outside keepalive, because idleness is measured outside the VM: poll `microvm health` on an interval under the launch's `--max-idle-sec`. [Keep a VM running and work inside it](/learn/tutorial/long-lived-vm/) covers each.

## 7. Cost and cleanup

The VM terminates on script exit through a trap, so failures tear down too. The image persists deliberately: its snapshot has a one-week minimum retention, so keeping and reusing it is cheaper than rebuilding. Delete old images with `aws lambda-microvms delete-microvm-image` when you are done.

Two open-source harnesses run coding agents inside Lambda MicroVMs the same way, each carrying its own hand-rolled daemon. [Harness capabilities](/internals/harness-capabilities/) maps their contracts onto this platform and ranks what is still missing.
