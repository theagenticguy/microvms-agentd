---
title: Remote dev with code-server over port-forward
description: Run VS Code in a browser against a MicroVM through microvm port-forward, on a named VM that suspends when you close the tab and resumes when you come back.
editUrl: false
sidebar:
  order: 4
---

```bash
# from the repo root, with the first-run prerequisites in place:
bash examples/code-server-remote-dev/run.sh
# then open http://127.0.0.1:8080
```

The script builds an image carrying code-server on first use, launches a named VM, starts the IDE inside it, and forwards local port 8080. Re-running it reattaches to the same VM instead of launching a second one, and Ctrl-C stops the forward and nothing else. At the end of this page you will have a disposable, suspendable dev box whose idle time costs roughly snapshot storage instead of compute.

## 1. The image

`Dockerfile` starts from the platform's `al2023-1` base pair, pinned by digest, carries the daemon exactly as the client's default Dockerfile does, then installs code-server from its release RPM. The RPM bundles its own Node, so the image adds only git and the shell basics beside it. The version is pinned by an `ARG` default; edit it to bump, and the content-hash-keyed image name makes the edited file build fresh under a new name.

## 2. The launch: named, kept, suspendable

```bash
microvm run --keep --vm-name code-server-dev --image "$IMAGE_ARN" --config microvm.toml
```

`--vm-name` registers the name in the local registry, so everything after the launch addresses the VM as `--name code-server-dev`. `microvm.toml` carries the choices that make this a dev box rather than a batch runner:

```toml
memory = 2048
egress = true
shell = true
auto-resume = true
max-idle-sec = 900

[env]
HOME = "/workspace"
PATH = "/usr/local/bin:/usr/bin:/bin"
```

`memory = 2048` keeps the default baseline and its 8 GiB ceiling, because an IDE with language servers is a steadier workload than a peaky agent session. `egress = true` because git, package registries, and the extension marketplace all need outbound network. `shell = true` launches shell-capable, so `microvm shell --name code-server-dev` can open a real PTY beside the IDE. `auto-resume` and `max-idle-sec` wire the suspend loop below. The `[env]` table sets `HOME` somewhere uid 1000 can write settings and extensions, and an explicit `PATH`, because the daemon spawns execs with a minimal environment.

## 3. Start the IDE, detached and demoted

```bash
microvm exec "code-server --bind-addr 127.0.0.1:8080 --auth none /workspace" \
  --detach --user 1000 --group 1000 --name "$VM_NAME" --region "$REGION" \
  --json >/dev/null

microvm exec "timeout 60 bash -c 'until echo > /dev/tcp/127.0.0.1/8080; do sleep 1; done' 2>/dev/null" \
  --name "$VM_NAME" --region "$REGION" --json >/dev/null
```

`--detach` starts code-server and returns; the second exec waits for the listener with bash's `/dev/tcp`, so nothing has to be installed for the wait. `--user 1000` because an IDE hands a terminal to whoever reaches it, and that terminal should not be root's.

`--auth none` is sound here because three layers already gate the path. The local listener binds `127.0.0.1`, so nothing off your machine reaches the forward. Every request the forward relays crosses the endpoint proxy, which requires an auth token scoped to this MicroVM and port set, and `port-forward` mints and attaches it for you. Inside the guest, code-server binds loopback only. A password prompt on top would authenticate nothing the proxy token has not already authenticated.

## 4. The forward

```bash
microvm port-forward 8080:8080 --name code-server-dev
```

`port-forward` serves a guest port on localhost so a browser here reaches a server in the VM. The ports are `LOCAL[:GUEST]`, and a single number uses it on both sides. `--bind` defaults to `127.0.0.1`, deliberately; `--max-connections` stops after serving that many connections instead of running until Ctrl-C.

## 5. The suspend/resume loop is what makes this cheap

Suspend is a freeze and restore. The filesystem, running processes, and the endpoint URL all survive a suspend/resume cycle, so unsaved buffers and the terminal you left open come back exactly as you left them.

The loop wires itself. An open editor tab holds a live WebSocket, which is inbound traffic, and inbound traffic resets the platform's idle timer. So the VM stays `RUNNING` while you work, suspends about fifteen minutes after you close the tab, and, because the config sets `auto-resume`, resumes on the next request through the endpoint. Reload the tab with the forward still up, or re-run the script, and you are back. A suspended VM bills roughly snapshot storage alone.

One ceiling to plan around: the platform caps any single VM's life at eight hours (`--max-duration-sec`, refused above 28800 before any call). This is a work-session dev box, so get work out before the ceiling with git from inside the IDE, or from outside:

```bash
microvm cp --tar vm:/workspace ./workspace-backup.tar --name code-server-dev
```

## 6. A shell beside it

```bash
microvm shell --name code-server-dev
```

A real PTY with job control, signals, and resize, for anything the editor's own terminal is the wrong tool for. It works while the browser tab is closed.

## 7. Teardown

```bash
microvm suspend code-server-dev      # pause it now
microvm terminate code-server-dev    # tear it down
```

`suspend`, `resume`, and `terminate` take the MicroVM id as their positional argument, and the registered name stands in for it. The image persists deliberately, because its snapshot has a one-week minimum retention; delete it with `aws lambda-microvms delete-microvm-image` when you are done with the recipe. The measurements behind the loop, the eight-hour ceiling, and the proxy's port-scoped tokens are in [Platform](/internals/platform/).
