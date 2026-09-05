---
title: Debug a failed build
description: Where a failed image build's reason lives, how to read the build log, what a wedged image looks like, and which failures the client now refuses before the billable call.
editUrl: false
sidebar:
  order: 7
---

```bash
microvm doctor --binary ./agentd          # every prerequisite, and the binary's architecture
microvm logs <image-name>                 # the build log group and the command that reads it
microvm history <microvm-id-or-name>      # what was asked, and what the platform said back
```

A server-side build cycle is minutes long and its error surfaces point away from their causes. At the end of this page you will know which surface carries a build's reason, how to read the log, what `ERR_BUILD_WEDGED` means for the image name, and which of these failures the client refuses locally now.

## 1. Read the exit code and the envelope first

Every failure envelope carries a stable `code`, an `exitCode` that matches `$?`, a `finding` naming the section of [Platform](/internals/platform/) that measured the behavior, and `suggestions`, with the first suggestion the one most likely to help. Under a human rendering the first line is `error ERR_*: <message>`, then `see docs/PLATFORM.md, '<finding>'`, then `hint:` lines. [Exit codes](/reference/exit-codes/) lists every row with its meaning.

The codes a build produces:

- `ERR_PRECONDITION` (exit 12): a prerequisite is missing; run `microvm doctor`.
- `ERR_INVALID_ARG` (exit 2): the request was refused locally, before any AWS call. The Dockerfile checks below land here.
- `ERR_BUILD_WEDGED` (exit 6): the image build was never scheduled, the `clientToken` replay signature.
- `ERR_LAUNCH_DIED` (exit 7): the MicroVM reached a terminal state before `RUNNING`; read `stateReason`.
- `ERR_CREDENTIALS` (exit 4): an identity is wrong or absent, and waiting will not fix it. An `AccessDeniedException` with a null message is the unsupported-region signature, and the suggestion says to check `--region` first.

## 2. Before building again

`microvm doctor` checks every prerequisite and says which one is wrong, in diagnosis order: the config file, the region, whether the credential chain resolves, the bucket and roles by name, whether the Terraform stack is applied (it asks `terraform output`, because a destroyed stack leaves `terraform.tfstate` behind with an empty resource list), the managed bases, and with `--binary` the daemon's architecture last, because that is the failure that costs a full build cycle.

Then build the Dockerfile locally under arm64 with `docker buildx build --platform linux/arm64`. A `dnf` typo is free to find there. [Write a guest Dockerfile](/learn/operations/write-a-guest-dockerfile/) has the rules.

## 3. Where the reason lives

A failed build's `stateReason` lives on the build record, and nowhere else. `GetMicrovmImage` reports `CREATE_FAILED` and structurally cannot say why; `ListMicrovmImageVersions` reports `FAILED` with a null reason; `ListMicrovmImageBuilds` carries the sentence. Expect a list: each failed version had two builds, one per chipset generation, with the same reason. `GetMicrovmImage`'s `latestFailedImageVersion` names which version to ask about.

Two observed reasons, worth reading for how much they vary:

```text
The container image build failed.                 (a RUN exiting non-zero)
Ready hook invocation timed out after PT5M        (a daemon that never became ready)
```

Read `snapshotBuild`'s shape beside the reason. Absent means the Dockerfile broke before anything installed. `codeInstallSizeInBytes` alone with no snapshots means code installed and the daemon never became ready, which points at the daemon or its `CMD` rather than at the build. The first reason names the failure without naming the cause inside the container, so the build log group is the only place the failing command's own output appears.

## 4. Read the build log

```bash
microvm logs <image-name>
```

`logs` names an image's build log group and prints the `aws logs tail` command that reads it. The group is `/aws/lambda-microvms/<image-name>`, created by the service, and its envelope carries `logGroup`, `tailCommand`, and `tailRequires`: the printed command needs AWS CLI v2, because `aws logs tail` does not exist in v1, and an identity granted the Terraform stack's `logs_read_policy_arn`. `lines` is explicitly `null`, never `[]`, because the CLI did not read the group itself.

An empty group beside every failure reading `reason=unknown` is the IAM-prefix signature. The build role must be granted `/aws/lambda-microvms/*`; a policy on the plausible `/aws/lambda/microvms/*` produces builds with no logs at all, and the caller's own policy is what discarded the evidence.

One build is three VMs writing three streams: docker-build, then a snapshot pass per chipset generation, and the snapshot VMs are the ones that start the app, so the daemon's own startup lines land there. `--log-group` on `run` and `build` sends the logs to a group of your own, and `--log-stream` names a stream prefix inside it; the client appends `/<16 hex>` per build attempt, because the wire member is an exact stream name and a fixed one would collapse every build's streams into one. The resolved exact name is on the `build` envelope as `logStream`. A configured group still has to be somewhere the build role can write.

## 5. `ERR_BUILD_WEDGED` and the `clientToken`

A `clientToken` is a permanent idempotency key. After an image is deleted and recreated under the same name with a token derived from that name, the service replays the original create as a no-op: the image sits in `CREATING` with its builds never scheduled, and `ListMicrovmImageBuilds` shows every build `PENDING` with `updatedAt` never advancing past `createdAt`. An image in `CREATING` cannot be deleted, and its only version cannot be deleted because it is the last one. Two images were wedged this way for roughly fifteen hours before the service timed them out.

Waiting does not help. The suggestion on the envelope is the remedy: record the identifier and build under a fresh `--name`. The client's defaults are shaped by this trap. `run` and `build` default to a per-invocation image name, and `--reuse` keys the name to a hash of the build inputs, so unchanged inputs reuse their image and changed inputs get a fresh name and a fresh build. Recreating an image under a previously used fixed name can also serve a stale snapshot, which is the same hazard class.

## 6. The green log that fails

A guest whose `AGENTD_PORT` disagrees with the create call's port fails the build with `CREATE_FAILED`, a fully populated and green build log, every docker layer succeeding, the daemon's own `agentd listening` line with the wrong address, and no error line anywhere. The build-time hooks are dialled on the create call's port, so a daemon listening elsewhere answers none of them. An unset `AGENTD_PORT` is the same failure when the client has moved off 9000, with nothing in the Dockerfile to point at. Both halves are refused locally now, before the billable call, so a build that reaches the service does not have this failure.

## 7. The daemon that never became ready

`Ready hook invocation timed out after PT5M` after a long build, saying nothing about architecture, is a host-architecture daemon binary. MicroVMs are ARM64-only, so an x86-64 `CMD` cannot exec and surfaces only as the hook never answering. `microvm doctor --binary <path>` reads the ELF header and answers before the build starts; a script or wrapper is caught as not an ELF binary. The same reason appears when an `ENTRYPOINT` swallows the `CMD`, when a start wrapper exits before handing off to the daemon, and when a prefetch or install at app start runs past the five-minute observed ceiling.

## 8. The VM that died before `RUNNING`

`ERR_LAUNCH_DIED` means a lifecycle hook failed after the image built. `GetMicrovm`'s `stateReason` is the only evidence that outlives the VM, and the client puts the state and the reason both in the message. The envelope's `finding` points at the `runHookPayload` measurements, and the suggestion points at `microvm logs <image-name>`, because the hook wrote to the build log group. A connection refused a second or two after the VM reaches `RUNNING` is a different thing: the endpoint proxy is not wired the instant the state flips, and that one is `ERR_RETRYABLE`.

The [debugging guide](/internals/insights/debugging-guide/) has the full failure-mode index, with a citation for every row.
