# Remote dev: code-server in a MicroVM, over `port-forward`

This example runs [code-server](https://github.com/coder/code-server) —
VS Code in a browser — inside a Lambda MicroVM and reaches it through
`microvm port-forward`. You get a disposable, suspendable dev box whose whole
lifecycle is four CLI commands, and whose idle time costs roughly snapshot
storage instead of compute.

```bash
# from the repo root, with the Getting-started prerequisites in place:
bash examples/code-server-remote-dev/run.sh
# then open http://127.0.0.1:8080
```

The script builds the image on first use (several minutes, server-side),
launches a named VM, starts code-server inside it, and forwards local
port 8080. Re-running it reattaches to the same VM instead of launching a
second one. Ctrl-C stops the forward and nothing else.

## The image

`Dockerfile` starts from the platform's `al2023-1` base pair (same pinned
digest and same `FROM` rules as
[coding-agents-on-bedrock](../coding-agents-on-bedrock/README.md), which
explains them), carries the agentd daemon exactly as the client's default
Dockerfile does, then installs code-server from its release RPM. The RPM is
self-contained — it bundles its own Node — so the image adds only git and the
shell basics beside it. The version is pinned by an `ARG` default; edit it to
bump, and the content-hash-keyed image name makes the edited file build fresh
under a new name.

## The launch: named, kept, suspendable

```bash
microvm run --keep --vm-name code-server-dev --image "$IMAGE_ARN" --config microvm.toml
```

`--vm-name` registers the name in the local registry, so everything after the
launch addresses the VM as `--name code-server-dev` — no endpoint, token, or
id to paste. `microvm.toml` carries the four choices that make this a dev box
rather than a batch runner:

- **`memory = 2048`.** An IDE with language servers is a steadier workload
  than a peaky agent session, so this keeps the default baseline and its
  8 GiB ceiling rather than the low minimum the coding-agents example picks.
- **`egress = true`.** git, package registries, and the extension
  marketplace all need outbound network.
- **`shell = true`.** The VM launches with the `SHELL_INGRESS` connector, so
  `microvm shell --name code-server-dev` opens a real PTY beside the IDE —
  handy for anything the editor's own terminal is the wrong tool for, and it
  works even while the browser tab is closed.
- **`auto-resume = true` + `max-idle-sec = 900`.** The suspend loop, below.

The `[env]` table sets `HOME=/workspace` (code-server runs demoted and needs
somewhere uid 1000 can write settings and extensions) and an explicit `PATH`
— the daemon spawns execs with a minimal environment, a lesson the
coding-agents example documents in detail.

## The suspend/resume loop is what makes this cheap

Suspend is a freeze and restore, not a stop and start: `docs/PLATFORM.md`
measured that the filesystem, running processes, and the endpoint URL all
survive a suspend/resume cycle. For a dev box that means unsaved buffers and
the terminal you left open come back exactly as you left them.

The loop wires itself: an open editor tab holds a live WebSocket, which is
inbound traffic, and inbound traffic resets the platform's idle timer
(`docs/PLATFORM.md` measured that even an outside health poll does). So the
VM stays `RUNNING` while you work, suspends about 15 minutes after you close
the tab, and — because the config sets `auto-resume` — resumes on the next
request through the endpoint. Reload the tab (with the forward still up, or
re-run `run.sh`) and you are back. A suspended VM bills roughly snapshot
storage alone, which is what makes leaving it suspended overnight reasonable.

One ceiling to plan around: the platform caps any single VM's life at eight
hours (`maximumDurationInSeconds`, max 28800 — a service-model constraint
recorded in `docs/PLATFORM.md`). This is a work-session dev box, not a pet
server. Get work out before the ceiling with git from inside the IDE, or from
outside with:

```bash
microvm cp --tar vm:/workspace ./workspace-backup.tar --name code-server-dev
```

## Why `--auth none` is sound here

Three layers already gate the path to the IDE. The local listener binds
`127.0.0.1`, so nothing off your machine reaches the forward. Every request
the forward relays crosses the endpoint proxy, which requires a JWE auth
token scoped to this specific MicroVM and port set
(`docs/PLATFORM.md`, "Endpoint authentication") — `port-forward` mints and
attaches it for you. And inside the guest, code-server itself binds loopback
only. A password prompt on top of that would authenticate nothing the proxy
token has not already authenticated, so the example turns it off rather than
teach a password that does no work.

The demotion matters more: `run.sh` starts code-server with `--user 1000`,
because an IDE hands a terminal to whoever reaches it, and that terminal
should not be root's.

## Teardown

```bash
microvm terminate --name code-server-dev
```

The image persists deliberately (its snapshot has a one-week minimum
retention, so reuse is cheaper than rebuild); delete it with
`aws lambda-microvms delete-microvm-image` when you are done with the recipe.
