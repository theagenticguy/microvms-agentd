---
title: Run your first command in a MicroVM
description: Create the AWS prerequisites, export the values the CLI reads, check the machine with microvm doctor, run microvm quickstart, then read what happened and what it cost.
editUrl: false
sidebar:
  order: 2
---

This tutorial runs a hello-world inside a real Lambda MicroVM and tears it down. Expect the first run to take a few minutes; most of that is the server-side image build.

At the end of this page a command will have run inside a MicroVM, the VM will be gone, you will have read its cost, and you will hold an image you can launch again without rebuilding.

You need the `microvm` CLI, so [install it](/learn/tutorial/install/) first.

## 1. What you need

An AWS account with Lambda MicroVMs access in a service region (`us-east-1`, `us-east-2`, `us-west-2`, `eu-west-1`, `ap-northeast-1`), and AWS credentials in your environment. The daemon binary is not on this list: the CLI provisions it.

A region outside that list is refused locally, because the service answers an unpriced region with an `AccessDeniedException` whose message is null, which reads as an IAM problem and is not one. `--unlisted-region` is the spelled-out opt-in if you know better.

## 2. Create the AWS prerequisites

An image build needs an S3 bucket for the code artifact, a build role, and an execution role. The repository ships a small Terraform stack that creates exactly those. From a clone:

```bash
mise run live:infra
cd conformance/infra
export MICROVM_BUCKET=$(terraform output -raw s3_bucket)
export MICROVM_BUILD_ROLE_ARN=$(terraform output -raw build_role_arn)
export MICROVM_EXECUTION_ROLE_ARN=$(terraform output -raw execution_role_arn)
cd ../..
```

If you already have a bucket and roles, export those instead. The stack is a convenience, and the CLI only reads the environment values, or the matching flags `--bucket`, `--build-role-arn`, and `--execution-role-arn`.

The stack also creates a managed policy for reading build logs (`terraform output -raw logs_read_policy_arn`). Attach it to the identity you run the AWS CLI with, and `microvm logs <image-name>` will hand you a working `aws logs tail` command. That command needs AWS CLI v2; `aws logs tail` does not exist in v1.

## 3. Check the machine

```bash
microvm doctor
```

`doctor` is the one command that must work with nothing configured, because its job is to report what is missing. Its check order is the diagnosis order: the project config file, then the region (a wrong one produces the null-message denial above), then whether the credential chain resolves at all, then the bucket and the two role values by name, then whether the Terraform stack is applied, then the managed base images the service publishes. With `--binary <path>` it also checks a daemon binary's architecture.

When something is wrong, `doctor` names the broken prerequisite and prints the command that fills it. Under `--json` it is a success envelope with `ok: false` and exit code 12 (`ERR_PRECONDITION`), because the check succeeded: it found what was wrong.

## 4. Run it

```bash
microvm quickstart
```

`quickstart` is exactly `microvm run --exec "echo hello from a microvm"` with every decision already made. Once it works, use `run` and its flags directly. Both accept the same `--region`, `--bucket`, and role flags as `doctor`.

## 5. What happened, step by step

1. **The daemon was provisioned.** The CLI fetched its own version's `agentd` release asset, verified it, and cached it under `~/.microvm`.
2. **An image was built.** The CLI wrote the default Dockerfile (the one `microvm dockerfile` prints), zipped it with the daemon into a build artifact, uploaded the artifact to your bucket, and asked the service to build. The image name defaults to a per-invocation name, because reusing one is how a `clientToken` replay wedges an image; [Debug a failed build](/learn/operations/debug-a-failed-build/) explains that trap.
3. **A VM was launched.** The launch carried a per-VM agent token in the platform's one-shot `runHookPayload`, so no secret was ever in the image. The daemon installs the token when the hook lands; until then every control route answers 503.
4. **The command ran.** The CLI opened an authenticated session to the VM's endpoint and ran the exec. Its stdout and stderr came back on the envelope.
5. **The cost was reported and the VM was torn down.** Teardown is the default so an interrupted session does not leave a billable VM behind. If anything could not be deleted, its identifier is named as leaked rather than dropped.

Under `--json` the whole run is one envelope of type `microvm.run`, with `imageIdentifier`, `imageName`, `microvmId`, `execExitCode`, `stdout`, `stderr`, `buildSeconds`, `runningSeconds`, `leaked`, and `cost` among its keys.

## 6. The cost line

Every run reports an estimate built from pinned, dated, per-region ARM rates. Dollar figures are estimates derived from published rates, never an invoice. Anything the engine cannot price is reported as unpriced with a reason rather than as zero, and a total containing an unpriced line renders as a lower bound. The server-side build is the usual unpriced line: AWS does not publish a build rate.

Sizing follows one rule. The baseline you request with `--memory` (default `2048`) is your bill floor while the VM runs, the VM is provisioned at four times that from the start, and usage above the baseline bills per second by what is consumed. There is no scaling event. [Read the cost report](/learn/operations/read-the-cost-report/) covers the whole report.

## 7. Keep the image

The image snapshot has a one-week minimum retention, so deleting an image early saves nothing and rebuilding one costs minutes. Build once under a name, then launch it as often as you like:

```bash
microvm build --reuse --name hello
microvm run --image hello --exec "uname -m"
```

`--reuse` keys the image name to a hash of the build inputs and skips the build when that name already exists, reporting `reused: true`. `--image` takes an ARN or a bare name; a name is resolved to its ARN through the account's image listing before the launch.

:::agent

**For an agent.** Run everything above with `--json` and read the envelope rather than the terminal rendering. `data.execExitCode` is the command's exit, `data.cost.total` is the estimate, and `data.leaked` is empty on a clean run. A failure envelope's `code` is stable and its `exitCode` matches `$?`; `ERR_PRECONDITION` (exit 12) means run `microvm doctor` and read its `checks`.

:::

Next: [keep a VM running and work inside it](/learn/tutorial/long-lived-vm/).
