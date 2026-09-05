---
title: Recover a leaked VM
description: What the local ledger says you left behind after an interrupted run, how to terminate it by id, and how to ask the account directly instead of trusting a teardown message.
editUrl: false
sidebar:
  order: 8
---

```bash
microvm ls                                  # what this CLI created and could not confirm it deleted
microvm history <microvm-id-or-name>        # what was asked of one VM and what the platform reported
microvm terminate <microvm-id> --wait       # stop paying
mise run live:verify-clean                  # ask the account, independently of the code that cleaned up
```

Teardown reporting success and the account being clean are different questions. At the end of this page you will know what the ledger says you left behind, how to release it, and how to confirm the account is clean without trusting any message that says so.

## 1. Ask the ledger

```bash
microvm ls
```

`ls` lists what this CLI created and could not confirm it deleted. It reads the local ledger rather than asking AWS, deliberately: the question it answers is "what did I leave behind", and the resources worth asking about are the ones a killed process never got to report, which no listing call can attribute back to a command that died. Each run's ledger is one JSON file under `~/.microvm/runs` (or `--state-dir`, or `$MICROVM_STATE_DIR`), written before each delete is attempted, so the identifiers survive a process that died inside the call. A run with a non-empty `leaked` list is marked as an alarm, the human rendering prints `LEAKED (still billing): <identifier>`, and the trailing count reads "N run(s), M with something still billing".

`--watch` re-reads the ledger on an interval until Ctrl-C, with `--interval-sec` (default 2) and `--max-refreshes`. It makes zero platform calls, and in particular never polls `/v1/health`, which is the call that resets a VM's idle timer, so watching keeps nothing alive and bills for nothing.

Run `ls` before anything else touches the account after an interruption. A `leaked` list is both a bill and a clue.

## 2. Read one VM's history

```bash
microvm history <microvm-id-or-name>
```

`history` prints what was asked of one MicroVM and what the platform reported back, from the local per-VM history appended by `run`, `exec`, `suspend`, `resume`, and `terminate`. The record survives terminate on purpose, because a caller attesting over a run needs it precisely after the VM is gone. It shows what the daemon and the control plane reported, never what a process inside the guest did between execs.

## 3. Terminate by id

```bash
microvm terminate <microvm-id> --wait
microvm terminate <microvm-id> --delete-image --image-identifier <image-arn> --image-name <image-name>
```

`terminate` takes the MicroVM id as its positional argument, or a registered name, which is resolved locally. `--wait` waits for `TERMINATED` rather than returning as soon as the call is accepted. `--delete-image` also deletes the image named by `--image-identifier`, and `--image-name` lets the CLI name its build log group, `/aws/lambda-microvms/<image-name>`, which the service created and Terraform never owns. The teardown envelope carries `leaked` and `undeletedLogGroups`, so a partial success is still machine readable.

An image refuses deletion while its VM is still terminating, so one pass is sometimes not enough. Deleting the image early also saves nothing, because its snapshot has a one-week minimum retention; a leaked image is a small bill and a leaked running VM is the one to hurry for.

## 4. Ask the account, never the teardown

```bash
mise run live:verify-clean
./scripts/verify-clean.py --delete
```

`live:verify-clean` queries the account directly and is independent of the code that did the cleanup, which is the point: `terraform destroy` once reported nine resources destroyed while six service-created log groups survived, because Terraform never owned them. It reports three outcomes rather than two. A **leak** is something still costing money that nothing intends to keep: a live MicroVM, an image, a log group. **Standing** is the Terraform stack, which you may keep applied on purpose. **Pending** is a deletion still in flight, where the right response is to re-run in a minute. Exit 0 when nothing leaked, 1 otherwise. `--delete` removes the leaks and leaves the stack to `terraform destroy`, and expect to run it more than once, because an image refuses deletion while its VM is still terminating.

It only recognizes resources under this project's own name prefixes, so anything else in the account is untouched. It needs the repository (`scripts/verify-clean.py` runs under `uv`) and credentials for the account. [Run the live suite](/learn/operations/run-the-live-suite/) puts it at the end of every billable run.

## 5. Why a VM leaks

`run` tears down by default, and an interruption after launch is `ERR_INTERRUPTED` (exit 11): teardown ran, and any leak is named in the envelope's `data.leaked`. A process killed before it could report leaves the ledger entry that `ls` reads. A run under `--keep` is a leak you asked for, and its name is released when the terminate is accepted, so a `--vm-name` refused with `ERR_NAME_TAKEN` (exit 14) means a live VM still holds it.

Two things that look like leaks are not. A VM suspended past its `--suspended-sec` window was terminated by the platform, and a resume is refused with `ERR_WINDOW_CLOSED` (exit 8) because there is nothing to resume. And a build log group under `/aws/lambda-microvms/` outliving `terraform destroy` is expected; it is why `verify-clean` exists.

## 6. A name registered on another machine

The registry is local, so a name registered elsewhere is unknown here and fails with `ERR_PRECONDITION` naming the directory it looked in. The MicroVM id is accepted directly by every identifier-taking command. To make the name work here, adopt the record:

```bash
ssh other cat ~/.microvm/runs/names/dev.json | microvm attach --from -
```

:::agent

**For an agent.** Read `data.leaked` on every failure envelope before doing anything else, and run `microvm ls --json` at the start of a session that follows an interrupted one. A leaked identifier is the remedy for a `CREATING` image or a service-created log group, because there is no second way to find them. Never conclude the account is clean from a teardown envelope; `mise run live:verify-clean` is the check, and exit 1 from it means something still bills.

:::
