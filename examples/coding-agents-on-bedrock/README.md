# Claude Code and Codex CLI in a MicroVM, on Bedrock

This example runs two coding agents headless inside a Lambda MicroVM, with
model access through Bedrock and no vendor API key anywhere. `run.sh` is the
whole thing; this file explains each decision it encodes.

```bash
# from the repo root, with the Getting-started prerequisites in place:
examples/coding-agents-on-bedrock/run.sh
```

The script builds the image on first use (several minutes, server-side),
launches a VM, injects short-lived credentials, runs Claude Code and then
Codex against Bedrock inside the VM, prints their output, and terminates the
VM. Subsequent runs reuse the image and go straight to launch.

## The image

`Dockerfile` starts from the platform's `al2023-1` base pair, carries the
agentd daemon exactly as the client's default Dockerfile does, then adds
Node 22, python3, and the two CLIs:

```dockerfile
RUN dnf install -y nodejs22 npm python3 git tar gzip which findutils procps-ng
RUN npm install -g @anthropic-ai/claude-code @openai/codex
```

Two constraints are load-bearing. The `FROM` must be the registry ref paired
with the managed base (`microvms-core` refuses a Dockerfile whose `FROM`
disagrees with the `baseImageArn`), and the image must declare a `WORKDIR`
because the base leaves it empty and the client requires one. The image
contains no secret of any kind: the daemon's token arrives per-VM through the
`runHookPayload` at launch, and model credentials arrive later over the
authenticated channel.

## Launch

```bash
microvm run --keep --egress --image "$IMAGE_ARN"
```

`--egress` matters: it requests the outbound-network connector, and without
it the guest cannot reach Bedrock. Both agents would install fine and then
fail on their first model call. `--keep` leaves the VM running and reports
the three values every attached command needs (endpoint, agent token,
MicroVM id). The image goes by ARN: `RunMicrovm` answers 400 "Malformed ARN"
for a bare name, so the script resolves the ARN from the image listing.

## Credentials, without a vendor key

The script mints a short-lived Bedrock bearer token from the caller's own
AWS credentials (the `aws-bedrock-token-generator` package; roughly 12-hour
lifetime) and delivers it as a file with `microvm cp --mode 0600`. Both
agents consume the same token:

- **Claude Code** has a native Bedrock mode: `CLAUDE_CODE_USE_BEDROCK=1`
  plus `AWS_BEARER_TOKEN_BEDROCK`, with the model chosen by
  `ANTHROPIC_MODEL` (an inference-profile id, e.g.
  `global.anthropic.claude-opus-5`).
- **Codex CLI** has no Bedrock mode, but Bedrock exposes an
  OpenAI-compatible surface. Current Codex speaks only the Responses wire
  API, which lives on the Mantle host
  (`base_url = https://bedrock-mantle.<region>.api.aws/openai/v1`), not on
  `bedrock-runtime` (that host's `/openai/v1` is chat-completions only,
  which Codex dropped). A five-line `config.toml` defines the provider with
  the bearer token as the API key (default model `openai.gpt-5.6-sol`,
  which carries a 1M-token context window on Bedrock).

The token never appears in the image, a command line, or a daemon log; it
lives in `/workspace/.agent-env` inside one VM, and expires on its own.

The env file also sets `PATH` explicitly, and that line is load-bearing. The
daemon spawns execs with a minimal environment, and Claude Code's Bash tool
snapshots the shell it starts from: without an exported `PATH` the agent's
subshells find no `ls`, `wc`, or `python3` (every command exits 127) even
though the daemon's own execs resolve them fine. Codex probes absolute paths
when lookup fails, so it limps through; Claude Code does not.

## The user, and why root silently breaks the agent

The Dockerfile creates uid/gid 1000 (by appending to `/etc/passwd` directly —
`useradd` is not in the minimal base), `chown`s `/workspace` to it, and every
agent exec passes `--user 1000 --group 1000`. Omit that and the failure is
worse than an error: Claude Code's `--dangerously-skip-permissions` refuses
to run as root, and under `acceptEdits` the agent then denies its own Bash,
Grep, and WebFetch calls and returns a confident report built on zero tool
calls. Measured on this task shape: as uid 0 the agent made no shell calls
at all; as uid 1000 it made 147.

The daemon itself stays root; demotion is per-command, which is why the
script can still run `chown -R 1000:1000 /workspace` as root after the
credential copies (the daemon writes `cp` files as root-owned `0600`, which
a demoted agent cannot read).

## The runs

Each agent gets one non-interactive task through `microvm exec`, demoted:

```bash
microvm exec ". /workspace/.agent-env && claude -p '<task>' --allowedTools Bash" --user 1000 --group 1000 ...
microvm exec ". /workspace/.agent-env && codex exec --skip-git-repo-check -s workspace-write '<task>'" --user 1000 --group 1000 ...
```

Claude Code is asked to write and run a shell one-liner; Codex is asked to
create and execute a Python file. A final `exec` cats the file Codex created,
so the transcript carries proof the agents actually did filesystem work
inside the VM rather than just answering in prose.

For longer agent sessions, the same primitives compose: `exec --detach`
starts a run and returns, `exec --poll <id>` reads it back, `cp --tar` moves
a whole project in and out, and `suspend`/`resume` freeze a VM between turns
with its filesystem and processes intact.

## Cost and cleanup

The VM terminates on script exit (a trap, so failures tear down too). The
image persists deliberately: its snapshot has a one-week minimum retention,
so keeping and reusing it is cheaper than rebuilding. The image name is
keyed to the Dockerfile's content hash: an unchanged Dockerfile reuses its
image, a changed one builds fresh under a new name. Recreating an image
under a previously used name can serve a stale snapshot, which is why the
script never reuses a fixed name across Dockerfile edits. Delete old images
with `aws lambda-microvms delete-microvm-image` when you are done.
