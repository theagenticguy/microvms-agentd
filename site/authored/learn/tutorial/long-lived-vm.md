---
title: Keep a VM running and work inside it
description: Launch a VM with --keep and a local name, run commands in it with exec, move files with cp, freeze it with suspend, thaw it with resume, and release it with terminate.
editUrl: false
sidebar:
  order: 3
---

`microvm run` tears the VM down when the command finishes. This tutorial keeps one, gives it a local name, and works inside it with the attached commands.

At the end of this page you will have run commands in a named VM, streamed and detached one, copied files in and out, suspended and resumed it, and terminated it, and you will know what `microvm ls` reports if any of that is interrupted.

You need a working first run, so [complete that tutorial](/learn/tutorial/first-run/) first.

## 1. Launch and name it

```bash
microvm run --keep --vm-name dev
```

`--keep` leaves the VM and image running; you are then paying for them. `--vm-name` registers a local name for the kept VM, so later commands can say `--name dev` instead of pasting the endpoint, agent token, and MicroVM id triple.

The name is a purely local fact. The registry is one file per name in the CLI's state directory (`~/.microvm/runs/names/<NAME>.json`, or `$MICROVM_STATE_DIR`), written owner-only because the record carries the agent token, and resolving a name costs zero AWS calls. Names take ASCII letters, digits, `-`, and `_`, up to 128 bytes, and never the `microvm-` prefix the service uses for ids, which is what lets every identifier-taking command tell a name from an id. A name already registered to a live VM is refused locally with `ERR_NAME_TAKEN` (exit 14) before any billable call.

The explicit `--endpoint`, `--agent-token`, and `--microvm-id` flags still work everywhere, for a VM some other machine launched. The `run` envelope reports all three, plus `kept: true` and `vmName`.

## 2. Run commands in it

```bash
microvm exec --name dev "python3 -V"
```

`exec` runs one command in a MicroVM that is already running. `--cwd` sets the working directory, `--env KEY=VALUE` sets one variable for the command and is repeatable, `--user` and `--group` run it as a numeric uid and gid, and `--timeout` (default 300 seconds) bounds the wait. The daemon spawns execs with a minimal environment, so a command that needs `PATH` or `HOME` gets them from `--env`, or for every exec in the VM from `run --launch-env KEY=VALUE`.

`--stream` streams output as it arrives rather than waiting for the whole thing. Under `--json` this is the one invocation that writes more than one object to stdout: NDJSON events, then the envelope last, with type `microvm.exec.stream`. `--from-offset` resumes a stream at a byte offset.

`--stdin` gives the command a stdin pipe, feeds it this process's stdin, and then closes it:

```bash
echo "hello" | microvm exec --name dev --stdin "cat"
```

`--detach` starts the command and returns immediately, without waiting and without acking. `--poll <EXEC_ID>` reads an existing exec's status and output instead of starting anything, and `ack <EXEC_ID>` releases a finished exec's buffered output, which starts its collection clock:

```bash
microvm exec --name dev --detach --json "make test"    # the envelope carries execId
microvm exec --name dev --poll <EXEC_ID>
microvm ack <EXEC_ID> --name dev
```

Polling is terminal-only: a running exec reports `phase: running` with no partial stdout, so stream it or have it write to a file if you want to watch. Output lives in the daemon until it is acked, so nothing a slow reader has not seen is destroyed, and a detached exec outlives the hourly rotation of the endpoint's proxy token by design. `--exec-id` supplies your own id, so a retry of the same start returns the original exec instead of spawning a second child.

## 3. Move files in and out

```bash
microvm cp ./data.csv vm:/tmp/data.csv --name dev
microvm cp vm:/tmp/result.json ./result.json --name dev
microvm cp --tar ./project.tar vm:/workspace --name dev
microvm cp --tar vm:/workspace ./workspace-backup.tar --name dev
```

`vm:/path` names the VM side; anything else is a local path. `--tar` moves a whole directory tree as an uncompressed tar archive, and extraction in the guest is confined so a hostile archive cannot write outside its target. `--mode` sets the permissions of an uploaded file, octal as a string (`644`, `0755`). The daemon runs as root, so an uploaded file is root-owned; a workload you run demoted with `--user` needs a `chown` exec before it can read one.

## 4. Freeze and thaw it

```bash
microvm suspend dev
microvm resume dev
```

`suspend`, `resume`, and `terminate` take the MicroVM id as their positional argument, and a registered name stands in for it: a bare name is resolved through the local registry with zero AWS calls, and anything shaped like an id passes through.

Suspend is a freeze. Memory, the filesystem, the agent token, and the endpoint survive, and a running process resumes mid-flight. A suspended VM pays snapshot storage only, and each suspend/resume cycle pays a snapshot write plus a read, so a long suspension is cheap and constant cycling is the habit to avoid.

The platform also suspends on its own. `--max-idle-sec` (default 600) suspends the VM after that much inbound-traffic idleness, `--suspended-sec` (default 600) terminates it after that long suspended, and `--auto-resume` lets the platform resume a suspended VM on an incoming request. A resume past the suspended window cannot work: the CLI refuses it with `ERR_WINDOW_CLOSED` (exit 8), and no call extends the window once the VM is launched. `--max-duration-sec` (default 3600) is the hard ceiling on the VM's life and is refused above 28800 (eight hours) before any call.

Idleness is measured by inbound traffic through the endpoint proxy, which terminates outside the VM. So a keepalive has to come from outside too: `microvm health --name dev` polls `/v1/health`, which is unauthenticated and resets the idle timer, and reports `busy` and `execs` so the poll can stop once the VM is drained. A guest process cannot keep its own VM alive, and a multi-hour exec with no outside traffic is frozen at the idle window with its process intact.

## 5. A shell and a browser

Launch with `--shell` and `microvm shell --name dev` opens an interactive root shell in the VM, a real PTY with job control, signals, and resize. `microvm port-forward 8080:8080 --name dev` serves a guest port on localhost so a browser here reaches a server in the VM, and `microvm tunnel 5432 --name dev` does the same for arbitrary TCP so `psql` or `ssh` here reaches a server there. Both bind `127.0.0.1` by default, deliberately. [Remote dev with code-server](/learn/operations/remote-dev-with-code-server/) builds a dev box on these.

## 6. Release it

```bash
microvm terminate dev
```

The name is released when the terminate is accepted. `--wait` waits for `TERMINATED` rather than returning as soon as the call is accepted. `--delete-image --image-identifier <ARN>` also deletes the image, and `--image-name <NAME>` lets the CLI name its build log group, which the service created and Terraform never owns. Deleting the image early saves nothing, because its snapshot has a one-week minimum retention.

## 7. If something is interrupted

`microvm ls` lists what this CLI created and could not confirm it deleted, from the local ledger, with zero AWS calls. `microvm history dev` prints what was asked of one MicroVM and what the platform reported back, and the record survives terminate on purpose. [Recover a leaked VM](/learn/operations/recover-a-leaked-vm/) is the runbook.

## 8. From another machine

The registry record is the export format. `attach` registers a name for a running MicroVM this state directory did not launch, from a record file or the explicit triple:

```bash
ssh other cat ~/.microvm/runs/names/dev.json | microvm attach --from -
```

:::agent

**For an agent.** Capture `execId` from a `--detach --json` start, poll it with `--poll`, and `ack` it when you have read the output. Pass your own `--exec-id` on a start you may have to retry, so a retry returns the original exec rather than spawning a second child. Read `phase` on a poll before treating empty stdout as the command's answer.

:::

Next: [run a project through a VM](/learn/tutorial/run-a-project/).
