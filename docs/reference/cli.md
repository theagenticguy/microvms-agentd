# microvms-agentd · CLI

The `microvm` binary has seventeen subcommands, declared as one `clap` `Subcommand` enum in `microvms-cli/src/cli.rs:90-227` and built from `microvms-cli/Cargo.toml:20-22`.

## Global flags

These three are `global = true`, so they parse on either side of the subcommand. `microvm --json ls` and `microvm ls --json` are the same invocation. `microvms-cli/src/cli.rs:61-79`.

Flags:

- `--json` — emit the typed JSON envelope on stdout instead of human output; wins over every other format, including an interactive terminal. `microvms-cli/src/cli.rs:69-70`.
- `--dense` — token-lean output, for a consumer paying per token. `microvms-cli/src/cli.rs:73-74`.
- `--quiet` — suppress progress on stderr; warnings still print. `microvms-cli/src/cli.rs:77-78`.

The output format depends only on the two flags and on whether stdout is a terminal. `--json` is checked first, then `--dense`; after that, a terminal gets a ratatui surface and a pipe gets plain text. `microvms-cli/src/envelope.rs:301-308`.

## Shared flag groups

Three flattened `Args` structs supply the flags that repeat across commands, so a relationship like the region conflict is declared once rather than per command.

`RegionFlags` — flattened into every command that talks to AWS. `microvms-cli/src/cli.rs:289-301`.

Flags:

- `--region <REGION>` — AWS region; defaults to `$AWS_REGION`, then `$AWS_DEFAULT_REGION`, then `us-east-1`. Closed set: `us-east-1`, `us-east-2`, `us-west-2`, `eu-west-1`, `ap-northeast-1`. `microvms-cli/src/cli.rs:292-293`, domain at `microvms-cli/src/cli.rs:258-270`.
- `--unlisted-region <NAME>` — use a region this client has not seen carry MicroVMs; conflicts with `--region`. An unsupported region answers `AccessDeniedException` with a null message, which looks like an IAM denial, so the caller loses the real diagnostic. `microvms-cli/src/cli.rs:299-300`.

`AttachFlags` — the three identifiers plus the port that address a VM this invocation did not launch. Carried by `exec`, `health`, `ack`, `stdin`, and `cp`. `microvms-cli/src/cli.rs:311-328`.

Flags:

- `--endpoint <ENDPOINT>` — the VM's endpoint, as reported by `run`. Required. `microvms-cli/src/cli.rs:314-315`.
- `--agent-token <AGENT_TOKEN>` — the agent token delivered to the VM at launch. Required. `microvms-cli/src/cli.rs:318-319`.
- `--microvm-id <MICROVM_ID>` — the MicroVM id, needed to mint the endpoint proxy token. Required. `microvms-cli/src/cli.rs:322-323`.
- `--port <PORT>` — the daemon's port inside the guest. `microvms-cli/src/cli.rs:326-327`.

`InfraFlags` — the three account-specific values the AWS commands need. Carried by `run`, `build`, and `doctor`. `microvms-cli/src/cli.rs:331-344`.

Flags:

- `--bucket <BUCKET>` — S3 bucket for the build artifact; defaults to `$MICROVM_BUCKET`. `microvms-cli/src/cli.rs:334-335`.
- `--build-role-arn <BUILD_ROLE_ARN>` — build role ARN; defaults to `$MICROVM_BUILD_ROLE_ARN`. `microvms-cli/src/cli.rs:338-339`.
- `--execution-role-arn <EXECUTION_ROLE_ARN>` — execution role ARN; defaults to `$MICROVM_EXECUTION_ROLE_ARN`. `microvms-cli/src/cli.rs:342-343`.

## run

```
microvm run [OPTIONS] [BINARY]
```

Builds an image, launches a VM, runs a command, reports the cost, and tears the VM down. Teardown is the default so that a closed laptop does not leave a billable VM.

`microvms-cli/src/commands/lifecycle.rs:119`

Flags:

- `[BINARY]` — the aarch64 agentd binary to bake in as the image CMD; ignored when `--image` names an image to launch instead. `microvms-cli/src/cli.rs:353-354`.
- `--image <IDENTIFIER>` — launch this existing image instead of building one. Takes an ARN or a bare image name: a name is resolved to its ARN through the account's image listing (exact match, every page read) before the launch, with a progress line naming the resolved ARN. An identifier already shaped like an ARN passes through with zero extra calls. A name that resolves to nothing fails locally with `ERR_PRECONDITION` naming the name and suggesting `microvm build` — the service's own answer to a bare name is HTTP 400 "Malformed ARN", which says nothing about names. `microvms-cli/src/commands/lifecycle.rs:283-300`, resolution in `microvms-core/src/control/image.rs:411-475`.
- `--artifact-uri <S3_URI>` — where the build artifact already is; `microvms-core` builds the artifact bytes and takes the URI but does not upload. `microvms-cli/src/cli.rs:368-369`.
- `--exec <COMMAND>` — a shell command to run in the VM. When it is omitted, the run only launches and tears down, which is how you check that an image boots. `microvms-cli/src/cli.rs:374-375`.
- `--name <NAME>` — image name; defaults to a per-invocation name, because reusing a name can trigger a `clientToken` replay that wedges the image. `microvms-cli/src/cli.rs:379-380`.
- `--memory <MEMORY>` — baseline MiB, selecting a documented size class; default `2048`. Closed set: `512`, `1024`, `2048`, `4096`, `8192`. `microvms-cli/src/cli.rs:387-388`.
- `--dockerfile <DOCKERFILE>` — a Dockerfile to use instead of the library's default; its `FROM` must match the base. `microvms-cli/src/cli.rs:391-392`.
- `--repair-identity` — widen the guest so `sethostname` and the `boot_id` bind mount work. `microvms-cli/src/cli.rs:397-398`.
- `--egress` — give the VM outbound network; omitted by default. `microvms-cli/src/cli.rs:401-402`.
- `--keep` — leave the VM and image running; both keep billing until you tear them down. `microvms-cli/src/cli.rs:405-406`.
- `--timeout <TIMEOUT>` — how long to wait for the exec, in seconds; default `300`. `microvms-cli/src/cli.rs:409-410`.
- `--max-idle-sec <MAX_IDLE_SEC>` — suspend the VM after this much inbound-traffic idleness; default `600`. `microvms-cli/src/cli.rs:413-414`.
- `--suspended-sec <SUSPENDED_SEC>` — terminate the VM after this long suspended; a resume attempted after this window fails because the VM no longer exists. Default `600`. `microvms-cli/src/cli.rs:417-418`.
- `--max-duration-sec <MAX_DURATION_SEC>` — hard ceiling on the VM's life; refused above 28800 before any call. Default `3600`. `microvms-cli/src/cli.rs:421-422`.
- `--port <PORT>` — the daemon's port inside the guest. `microvms-cli/src/cli.rs:425-426`.
- `--state-dir <STATE_DIR>` — where the run ledger is written; defaults to `$MICROVM_STATE_DIR` or `~/.microvm/runs`. `microvms-cli/src/cli.rs:429-430`.
- Plus `RegionFlags` and `InfraFlags`. `microvms-cli/src/cli.rs:432-436`.

## build

```
microvm build [OPTIONS] <BINARY>
```

Builds a MicroVM image and waits for it to be usable. Nothing is torn down afterward. The image is the durable artifact, and because its snapshot has a one-week minimum retention, deleting it early saves nothing.

`microvms-cli/src/commands/lifecycle.rs:469`

Flags:

- `<BINARY>` — the aarch64 agentd binary to bake in as the image CMD. Required. `microvms-cli/src/cli.rs:442-443`.
- `--artifact-uri <S3_URI>` — where the build artifact already is, as an `s3://` URI. `microvms-cli/src/cli.rs:445-447`.
- `--name <NAME>` — image name; defaults to a per-invocation name. `microvms-cli/src/cli.rs:450-451`.
- `--memory <MEMORY>` — baseline MiB, selecting a documented size class; default `2048`. `microvms-cli/src/cli.rs:454-455`.
- `--dockerfile <DOCKERFILE>` — a Dockerfile to use instead of the library's default. `microvms-cli/src/cli.rs:458-459`.
- `--repair-identity` — widen the guest so `sethostname` and the `boot_id` bind mount work. `microvms-cli/src/cli.rs:462-463`.
- `--reuse` — reuse an existing image whose build inputs match, instead of building. Computes a sha256 over the build inputs (the daemon binary's bytes and the Dockerfile), derives the image name `<name>-<hash12>` — where the prefix is `--name` or the stable stem `microvm-cli` — and checks the listing for that exact name. A hit skips the build entirely and reports the existing image with `reused: true` in the envelope; a miss builds under the derived name, so the next invocation with the same inputs hits. The hash is in the name because recreating an image under a previously-used fixed name can serve a stale snapshot (measured; the same hazard class as the clientToken replay in `docs/PLATFORM.md`) — content-keying gives both properties at once: unchanged inputs reuse their image, changed inputs get a fresh name and a fresh build. `--memory` is not part of the identity, so a reused image keeps the size class it was created with; the envelope's `size` is the requested class and the text says so. `microvms-cli/src/commands/lifecycle.rs:501`, the hash at `microvms-core/src/control/artifact.rs:91`.
- `--port <PORT>` — the daemon's port inside the guest. `microvms-cli/src/cli.rs:466-467`.
- Plus `RegionFlags` and `InfraFlags`. `microvms-cli/src/cli.rs:469-473`.

The success envelope always carries `reused` (`false` for a plain build), so a consumer never guards for the key. `microvms-cli/src/commands/mod.rs`.

## exec

```
microvm exec [OPTIONS] --endpoint <ENDPOINT> --agent-token <AGENT_TOKEN> --microvm-id <MICROVM_ID> [COMMAND]
```

Runs one command in a MicroVM that is already running. The single subcommand covers four uses. It can start a command and wait for it, start one and stream its output (`--stream`), start one and feed its stdin (`--stdin`), or read an existing exec (`--poll`).

`microvms-cli/src/commands/attached.rs:107`

Flags:

- `[COMMAND]` — a shell command to run in the VM; omitted only with `--poll`. `microvms-cli/src/cli.rs:497-498`.
- `--timeout <TIMEOUT>` — how long to wait for the command, in seconds; default `300`. `microvms-cli/src/cli.rs:501-502`.
- `--cwd <CWD>` — working directory. When omitted, the command inherits the image WORKDIR, which is not the same as passing `/`. `microvms-cli/src/cli.rs:508-509`.
- `--env <KEY=VALUE>` — set one environment variable for the command; repeatable. These flags are the child's whole environment: the daemon starts every exec from an empty one and applies exactly this map, so there is no inherited PATH to append to. Split at the first `=`, so a value may itself contain `=`; an empty VALUE is legal (`--env EMPTY=`), and a missing `=` or an empty KEY is refused at parse time. `microvms-cli/src/cli.rs:524-525`.
- `--user <UID>` — numeric uid to run the command as; omitted runs as the daemon's own user. Numeric because that is the protocol's type and the daemon's mechanism (`Command::uid`, between fork and exec) — a name would need an `/etc/passwd` lookup inside a guest whose base image may not have one. `microvms-cli/src/cli.rs:534-535`.
- `--group <GID>` — numeric gid to run the command as; omitted keeps the daemon's own group. `microvms-cli/src/cli.rs:538-539`.
- `--exec-id <ID>` — use this exec id instead of a fresh one, making a retry idempotent; the daemon returns success for a known id without spawning a second child. `microvms-cli/src/cli.rs:558-559`.
- `--poll <ID>` — read an existing exec's status and output instead of starting anything; read-only server-side, does not ack. Conflicts with `--exec-id`, `--stream`, `--stdin`, `--cwd`, `--detach`, `--env`, `--user`, `--group`. `microvms-cli/src/cli.rs:567-568`.
- `--detach` — start the command and return immediately, without waiting and without acking; prints the exec id and `phase: running`. Conflicts with `--stream` and `--stdin`. `microvms-cli/src/cli.rs:587-588`.
- `--stream` — stream output as it arrives rather than waiting for the whole thing; under `--json` or into a pipe this writes NDJSON. `microvms-cli/src/cli.rs:597-598`.
- `--from-offset <BYTES>` — resume a stream at this byte offset; requires `--stream`. `microvms-cli/src/cli.rs:605-606`.
- `--stdin` — give the command a stdin pipe, feed it this process's stdin, then close it. `microvms-cli/src/cli.rs:613-614`.
- Plus `AttachFlags` and `RegionFlags`. `microvms-cli/src/cli.rs:616-620`.

## health

```
microvm health [OPTIONS] --endpoint <ENDPOINT> --agent-token <AGENT_TOKEN> --microvm-id <MICROVM_ID>
```

Asks a running MicroVM's daemon whether it is up and what its identity repair did. This is the one command that reports `identityDegraded` and `diskUnderPressure`, and either flag is a reason to drain the VM rather than keep scheduling onto it.

`microvms-cli/src/commands/attached.rs:471`

Flags:

- `AttachFlags` and `RegionFlags` only; this command has no arguments of its own. `microvms-cli/src/cli.rs:575-582`.

## ack

```
microvm ack [OPTIONS] --endpoint <ENDPOINT> --agent-token <AGENT_TOKEN> --microvm-id <MICROVM_ID> <EXEC_ID>
```

Releases a finished exec's buffered output, which starts its collection clock. A second ack returns a 409 because the first one already released the output.

`microvms-cli/src/commands/attached.rs:574`

Flags:

- `<EXEC_ID>` — the exec whose output to release. Required. `microvms-cli/src/cli.rs:587-588`.
- Plus `AttachFlags` and `RegionFlags`. `microvms-cli/src/cli.rs:590-594`.

## stdin

```
microvm stdin [OPTIONS] --endpoint <ENDPOINT> --agent-token <AGENT_TOKEN> --microvm-id <MICROVM_ID> <EXEC_ID>
```

Writes to a running exec's stdin and optionally closes it. It only works on an exec started with `exec --stdin`, and it is the only way to close the pipe.

`microvms-cli/src/commands/attached.rs:612`

Flags:

- `<EXEC_ID>` — the exec to write to; must have been started with `exec --stdin`. Required. `microvms-cli/src/cli.rs:600-601`.
- `--data <DATA>` — what to write; `-` reads this process's stdin, and omitting the flag writes nothing. The value is raw bytes either way, and core base64-encodes them for the wire. `microvms-cli/src/cli.rs:607-608`.
- `--eof` — close stdin after any `--data` is written, in the same request rather than a second one. `microvms-cli/src/cli.rs:615-616`.
- Plus `AttachFlags` and `RegionFlags`. `microvms-cli/src/cli.rs:618-622`.

## cp

```
microvm cp [OPTIONS] --endpoint <ENDPOINT> --agent-token <AGENT_TOKEN> --microvm-id <MICROVM_ID> <SRC> <DST>
```

Copies a file or a tar archive between here and a running MicroVM: `cp ./local vm:/remote` writes, `cp vm:/remote ./local` reads.

`microvms-cli/src/commands/attached.rs:779`

Flags:

- `<SRC>` — source; `vm:/path` reads from the VM, anything else is a local path. Required. `microvms-cli/src/cli.rs:628-629`.
- `<DST>` — destination; `vm:/path` writes to the VM, anything else is a local path. Required. `microvms-cli/src/cli.rs:632-633`.
- `--tar` — move a whole directory tree as an uncompressed tar archive; the `vm:` side is a directory the daemon packs or extracts, the local side is a `.tar` file. `microvms-cli/src/cli.rs:656-657`.
- `--mode <OCTAL>` — permissions for an uploaded file, octal as a string; conflicts with `--tar`, since a tar carries its members' own modes. `microvms-cli/src/cli.rs:664-665`.
- Plus `AttachFlags` and `RegionFlags`. `microvms-cli/src/cli.rs:667-671`.

## suspend

```
microvm suspend [OPTIONS] <MICROVM_ID>
```

Freezes a MicroVM, which keeps its memory, filesystem, token, and endpoint. The operation is a freeze and restore rather than a stop and start.

`microvms-cli/src/commands/lifecycle.rs:730`

Flags:

- `<MICROVM_ID>` — the MicroVM to freeze. Required. `microvms-cli/src/cli.rs:677-678`.
- `--timeout <TIMEOUT>` — how long to wait for the state transition, in seconds; default `300`. `microvms-cli/src/cli.rs:681-682`.
- Plus `RegionFlags`. `microvms-cli/src/cli.rs:684-685`.

## resume

```
microvm resume [OPTIONS] <MICROVM_ID>
```

Thaws a suspended MicroVM and reports its endpoint. Past the launch-time `suspendedDurationSeconds` window the VM has been terminated, so the resume fails.

`microvms-cli/src/commands/lifecycle.rs:795`

Flags:

- `<MICROVM_ID>` — the MicroVM to thaw. Required. `microvms-cli/src/cli.rs:691-692`.
- `--timeout <TIMEOUT>` — how long to wait for RUNNING, in seconds; default `300`. `microvms-cli/src/cli.rs:695-696`.
- Plus `RegionFlags`. `microvms-cli/src/cli.rs:698-699`.

## terminate

```
microvm terminate [OPTIONS] <MICROVM_ID>
```

Tears down a MicroVM and optionally its image and build log group. When part of the teardown fails, the command still exits successfully and reports the leaked identifier.

`microvms-cli/src/commands/lifecycle.rs:840`

Flags:

- `<MICROVM_ID>` — the MicroVM to terminate. Required. `microvms-cli/src/cli.rs:705-706`.
- `--image-identifier <IMAGE_IDENTIFIER>` — the image to delete, if `--delete-image` is given. `microvms-cli/src/cli.rs:709-710`.
- `--image-name <IMAGE_NAME>` — the image's name, needed to name its build log group; the service created that group, so `terraform destroy` never removes it. `microvms-cli/src/cli.rs:715-716`.
- `--delete-image` — also delete the image and name its build log group; requires `--image-identifier`. `microvms-cli/src/cli.rs:719-720`.
- `--wait` — wait for TERMINATED rather than returning as soon as the call is accepted. `microvms-cli/src/cli.rs:723-724`.
- Plus `RegionFlags`. `microvms-cli/src/cli.rs:726-727`.

## ls

```
microvm ls [OPTIONS]
```

Lists what this CLI created and could not confirm it deleted. It reads the local ledger rather than asking AWS, because the ledger still names the resources a killed process never got to delete.

`microvms-cli/src/commands/local.rs:23`

Flags:

- `--state-dir <STATE_DIR>` — where the ledgers live; defaults to `$MICROVM_STATE_DIR` or `~/.microvm/runs`. `microvms-cli/src/cli.rs:733-734`.

## logs

```
microvm logs [OPTIONS] <IMAGE_NAME>
```

Names an image's build log group, `/aws/lambda-microvms/<image-name>`. That log group holds the only evidence a failed build leaves behind.

`microvms-cli/src/commands/local.rs:146`

Flags:

- `<IMAGE_NAME>` — the image whose log group to name. Required. `microvms-cli/src/cli.rs:740-741`.
- Plus `RegionFlags`. `microvms-cli/src/cli.rs:743-744`.

## cost

```
microvm cost [OPTIONS]
```

Reports what a run cost or what a plan will cost, with every figure labelled. Dollar figures are estimates derived from published rates, not invoice amounts. A line item with no published rate reads `unpriced` rather than `$0.00`.

`microvms-cli/src/commands/cost.rs:27`

Flags:

- `--estimate` — treat the durations as a plan rather than as timings, so every duration is labelled projected. `microvms-cli/src/cli.rs:753-754`.
- `--compare` — also print running versus suspended for the same hold, with the break-even. `microvms-cli/src/cli.rs:757-758`.
- `--memory <MEMORY>` — baseline MiB, selecting a documented size class; default `2048`. `microvms-cli/src/cli.rs:761-762`.
- `--running-sec <RUNNING_SEC>` — seconds the VM spent, or will spend, RUNNING; billed at baseline whether or not anything is executing. Default `0`. `microvms-cli/src/cli.rs:768-769`.
- `--suspended-sec <SUSPENDED_SEC>` — seconds spent suspended; storage only, no compute line. Default `0`. `microvms-cli/src/cli.rs:772-773`.
- `--build-sec <BUILD_SEC>` — seconds the image build took; appears as an unpriced line. Default `0`. `microvms-cli/src/cli.rs:779-780`.
- `--image-gb <IMAGE_GB>` — image size in GB; adds storage with its one-week minimum retention. `microvms-cli/src/cli.rs:783-784`.
- `--cycles <CYCLES>` — suspend/resume cycles, each paying a snapshot write plus a read; default `1`. `microvms-cli/src/cli.rs:787-788`.
- `--hold-sec <HOLD_SEC>` — the hold to compare running against suspended over, in seconds; default `3600`. `microvms-cli/src/cli.rs:791-792`.

## doctor

```
microvm doctor [OPTIONS]
```

Checks every prerequisite and says which one is wrong. The checks cover credentials, the region, the three Terraform outputs, whether the stack is applied, and whether the daemon binary is aarch64.

`microvms-cli/src/commands/doctor.rs:32`

Flags:

- `--binary <BINARY>` — the agentd binary to check the architecture of. `microvms-cli/src/cli.rs:798-799`.
- `--infra-dir <INFRA_DIR>` — the Terraform stack directory; defaults to `./conformance/infra`. `microvms-cli/src/cli.rs:802-803`.
- Plus `RegionFlags` and `InfraFlags`. `microvms-cli/src/cli.rs:805-809`.

## manifest

```
microvm manifest [OPTIONS]
```

Emits the whole command surface, its exit codes, and its envelope schema. The manifest is generated from the registered clap tree rather than maintained by hand, so it cannot drift from what the binary accepts.

`microvms-cli/src/commands/local.rs:191`

The command takes no flags and always emits JSON, because the only consumer that asks for a manifest is one that parses it.

## constants

```
microvm constants [OPTIONS]
```

Emits every service constraint this client believes, for the drift gate that `scripts/check-model-drift` runs against the pinned botocore model.

`microvms-cli/src/commands/local.rs:214`

Flags:

- `--emit-json` — emit the raw constants object without an envelope. This is the one stdout write in this binary that is not an envelope. `microvms-cli/src/cli.rs:830-831`.

## dockerfile

```
microvm dockerfile [OPTIONS]
```

Prints the Dockerfile stanza that wraps any base image with agentd. The stanza is what `microvm build` bakes when no `--dockerfile` is given, so appending your own `RUN` layers to this output and passing the result to `build --dockerfile` is the default build plus your layers. The text comes from `microvms-core`'s own generator rather than a copy, so it cannot drift from what a default build produces. `microvms-cli/src/commands/local.rs:251`, reusing `microvms-core/src/control/artifact.rs:145`.

The output's comments name the two platform constraints a hand-written wrapper hits. The `FROM` must match the managed base's `docker_ref`, because the build runs the Dockerfile on top of the base that `baseImageArn` names and `microvms-core` refuses a Dockerfile whose `FROM` disagrees. And a `WORKDIR` is required when the base declares none — the managed al2023 base does not, so without one every relative path resolves against `/`. `microvms-core/src/control/artifact.rs:228-244`, `microvms-core/src/control/artifact.rs:196-220`.

This is a local command: no account is involved, and the stanza is built from compile-time constants.

Flags:

- `--from <IMAGE_REF>` — the image ref for the `FROM` line; defaults to the managed al2023 base's pair, `public.ecr.aws/amazonlinux/amazonlinux:2023-minimal`. Only change this when you are also changing `baseImageArn`. `microvms-cli/src/cli.rs:884-891`.
- `--port <PORT>` — the port agentd listens on inside the guest; default `9000`. Reaches both `ENV AGENTD_PORT` and `EXPOSE`. `microvms-cli/src/cli.rs:893-895`.
- `--workdir <DIR>` — a working directory to create and set, as both a `RUN mkdir -p` and a `WORKDIR`. Strongly recommended, because the managed base declares no WorkingDir. `microvms-cli/src/cli.rs:897-903`.

The JSON envelope carries the stanza text plus the base image pair — `baseImageName` for deriving `baseImageArn` and `baseImageDockerRef` for the `FROM` — so a consumer holds both halves of the agreement the platform enforces. `microvms-cli/src/commands/mod.rs:207-217`.

For the full recipe — appending tool layers, building, and driving the daemon from your own harness — see [docs/EMBEDDING.md](../EMBEDDING.md).

## The JSON envelope

Each invocation writes exactly one JSON object on stdout. Progress always goes to stderr. `--quiet` suppresses progress but not warnings, so a leaked resource is still reported even in quiet mode. `microvms-cli/src/envelope.rs:1-18`. `apiVersion` is `"1"`; it is bumped when a field's meaning changes, not when a command is added. `microvms-cli/src/envelope.rs:66`.

A success envelope carries `status`, `apiVersion`, `type`, and `data`. `type` is the discriminant to branch on first. `microvms-cli/src/envelope.rs:314-321`.

```
{
  "status": "ok",
  "apiVersion": "1",
  "type": "microvm.run",
  "data": { }
}
```

A failure envelope carries `status`, `apiVersion`, `error`, `code`, `exitCode`, `finding`, `suggestions`, and `data`. Every field is always present, so a consumer never has to guard against a missing key. `microvms-cli/src/envelope.rs:323-342`.

```
{
  "status": "error",
  "apiVersion": "1",
  "error": "human readable, may be reworded between releases",
  "code": "ERR_PROTOCOL",
  "exitCode": 5,
  "finding": "",
  "suggestions": [],
  "data": { "kind": "Conflict" }
}
```

Branch on `code` rather than `error`; the `error` text may be reworded between releases while `code` is stable. `microvms-cli/src/manifest.rs:113`.

### data.kind

`data.kind` carries the daemon's own status name when the exit code is coarser than the failure. Five `WireKind`s (`Conflict`, `NotFound`, `ProtocolError`, `StdinClosed`, `TooLarge`) all map to `ERR_PROTOCOL`. Collapsing them is deliberate. A shell branching on `$?` cannot act differently on a 400 than on a 409, and a consumer that can act differently reads `data.kind` instead. `microvms-cli/src/exit.rs:39-44`, inserted at `microvms-cli/src/envelope.rs:329-331`, and pinned at `microvms-cli/src/exit.rs:532-548`.

A request rejected locally reports no `data.kind`, because nothing reached the daemon. `microvms-cli/src/envelope.rs:450-453`.

### Response types

Each command declares its `type` discriminant and the `data` keys its success envelope carries. `microvms-cli/src/commands/mod.rs:102-218`.

| Command | `type` |
| --- | --- |
| `run` | `microvm.run` |
| `build` | `microvm.image` |
| `exec` | `microvm.exec` |
| `health` | `microvm.health` |
| `ack` | `microvm.exec` |
| `stdin` | `microvm.stdin` |
| `cp` | `microvm.copy` |
| `suspend` | `microvm.state` |
| `resume` | `microvm.state` |
| `terminate` | `microvm.teardown` |
| `ls` | `microvm.runs` |
| `logs` | `microvm.logs` |
| `cost` | `microvm.cost` |
| `doctor` | `microvm.doctor` |
| `manifest` | `microvm.manifest` |
| `constants` | `microvm.constants` |
| `dockerfile` | `microvm.dockerfile` |

## The NDJSON stream exception

`exec --stream` is the one invocation that writes more than one object to stdout. Under `--json` (or into a pipe asking for it) it emits NDJSON, meaning one JSON object per event and then the envelope as the final line. This is a second, narrower contract that sits alongside the one-envelope rule. Three things keep the two contracts distinguishable. `microvms-cli/src/envelope.rs:35-54`.

First, the discriminant differs. A streamed exec's final envelope has `type` `microvm.exec.stream`, while a non-streamed exec has `microvm.exec`. A consumer branching on `type` learns which parse applies from the field it already reads first. `microvms-cli/src/commands/mod.rs:222-233`.

Second, the manifest publishes it. `exec`'s entry carries an `alternateResponse` object naming `when: "--stream"`, the `responseType`, the `responseKeys`, and a `stdout` description of the NDJSON shape. The entry is generated from the flag's presence in the command tree, so removing `--stream` from `exec` removes the entry too. `microvms-cli/src/manifest.rs:46-62`.

Third, the envelope is written compact once a stream has started, because "the last line is the envelope" is only true if the envelope is one line. A pretty-printed envelope at the end of an NDJSON stream would parse as several broken records. `microvms-cli/src/envelope.rs:171-175`.

```
{"event":"output","stream":"stdout","offset":0,"bytes":12,"text":"hello world\n","lossy":false}
{"event":"exit","exitCode":0,"signal":null,"truncated":false,"writersMayBeAlive":false,"offset":12}
{"status":"ok","apiVersion":"1","type":"microvm.exec.stream","data":{"execId":"x-1","events":2,"bytes":12,"nextOffset":12,"gaps":0,"exitCode":0,"truncated":false}}
```

Three event kinds reach a line: `output` (with `stream`, `offset`, `bytes`, `text`, `lossy`), `gap` (with `from` and `to`), and `exit` (with `exitCode`, `signal`, `truncated`, `writersMayBeAlive`, `offset`). `microvms-cli/src/commands/attached.rs:339-378`.

Output arrives as lossy text beside the true byte count rather than as base64. `lossy` is set when the conversion actually replaced anything, so a consumer can tell when the text differs from the original bytes. The non-JSON path carries the exact bytes. `microvms-cli/src/commands/attached.rs:325-338`.

The stream envelope's keys summarize the stream. The output itself was the NDJSON events, and repeating it in the envelope would double a stream's memory cost for a consumer that has already seen every byte. `events` and `bytes` let a caller assert it read everything, and `nextOffset` is where a resume with `--from-offset` would continue. `microvms-cli/src/commands/mod.rs:207-233`.

The events are the command's output, not progress about it, so they cannot go to stderr even though that would preserve the simpler one-envelope rule. Sending a workload's stdout to the caller's stderr would make `microvm exec --stream build.sh > log` write an empty log. Buffering the events to keep stdout a single document would remove the only reason to stream. `microvms-cli/src/envelope.rs:51-54`.

The non-JSON formats emit no NDJSON at all. The raw child bytes go to stdout untouched, with no lossy string conversion applied. `microvms-cli/src/envelope.rs:232-247`.

A stream that fails part-way through has already written events and no envelope. On the JSON path the failure envelope becomes the stream's compact last line, because an NDJSON consumer reading line by line needs a terminating record saying why the events stopped. On the human paths the same failure goes to stderr instead, because appending an error message to the child's raw output would corrupt the file a caller was redirecting into. `microvms-cli/src/main.rs:319-333`.

A stream that ended without an exit event was cut. `exitCode` is reported as `null` rather than `0`, because reporting zero would turn a truncated stream into a passing build. The command exits `ERR_EXEC_FAILED`. `microvms-cli/src/commands/attached.rs:290-321`.

## Exit codes

The table has fourteen rows, 0 through 13, and is append-only because consumers branch on the values. The rows are split by what the caller should do next; a distinction that does not change the caller's next action does not get its own integer. `microvms-cli/src/exit.rs:171-256`, with the enum's explicit discriminants at `microvms-cli/src/exit.rs:76-100`.

| Exit | Code | Meaning | `docs/PLATFORM.md` finding |
| --- | --- | --- | --- |
| 0 | — | the command did what it said | |
| 1 | `ERR_UNEXPECTED` | an exception no handler claimed — a bug in this CLI, not the platform | |
| 2 | `ERR_INVALID_ARG` | the request was refused locally, before any AWS call | |
| 3 | `ERR_RETRYABLE` | a transient condition; run the identical command again | Endpoint authentication |
| 4 | `ERR_CREDENTIALS` | an identity is wrong or absent; waiting will not fix it | |
| 5 | `ERR_PROTOCOL` | the daemon rejected the request on its merits | |
| 6 | `ERR_BUILD_WEDGED` | the image build was never scheduled — the clientToken replay signature | `clientToken` is a permanent idempotency key |
| 7 | `ERR_LAUNCH_DIED` | the MicroVM reached a terminal state before RUNNING; read stateReason | `runHookPayload` arrives wrapped, not as the body |
| 8 | `ERR_WINDOW_CLOSED` | the launch-time suspended window passed, so there is nothing to resume | `idlePolicy` |
| 9 | `ERR_PLATFORM` | a control-plane failure with no more specific class | |
| 10 | `ERR_TIMEOUT` | a client-side deadline elapsed; the VM and the exec are untouched | |
| 11 | `ERR_INTERRUPTED` | interrupted after launch; teardown ran and any leak is named in the payload | The build log group survives Terraform |
| 12 | `ERR_PRECONDITION` | a prerequisite is missing — run `microvm doctor` | |
| 13 | `ERR_EXEC_FAILED` | the sandbox worked and the command in it exited non-zero | |

Row 0 is the only one with no `ERR_*` string, because a success envelope has no `code` field to put one in. `microvms-cli/src/exit.rs:50-53`.

`ERR_EXEC_FAILED` has its own code because it is the one non-zero exit that means nothing is wrong with the platform, the credentials, or the CLI. A CI caller needs to tell "your tests failed" apart from "we never got a VM". `microvms-cli/src/exit.rs:94-99`.

A clap parse failure maps to exit 2 / `ERR_INVALID_ARG`, forwarding clap's own message verbatim including its did-you-mean line. This deliberately matches clap's own convention, so a caller who reads `$?` sees the same number either way. `microvms-cli/src/exit.rs:381-384`, `microvms-cli/src/cli.rs:56-59`.

`--help` and `--version` are successes that print themselves and exit 0, never becoming envelopes. `microvms-cli/src/main.rs:78-87`.

### Suggestions

The exit code comes from the failure class and nothing else. The CLI adds the suggestion on top. The library reports what went wrong, and the CLI names the flag or command that addresses it. Two failures sharing `ERR_CREDENTIALS` therefore get different remedies. A 401 names the agent token. An unresolvable credential chain names `microvm doctor` and the unsupported-region null-message signature. `microvms-cli/src/exit.rs:321-368`.

## Conventions

`microvm manifest` publishes six conventions alongside the command tree. `microvms-cli/src/manifest.rs:111-128`.

- Exactly one envelope object on stdout per invocation; progress is on stderr.
- Branch on `code`, never on `error`.
- Dollar figures are estimates derived from published rates, never an invoice.
- An unpriced line item omits `usd` rather than reporting zero.
- `data.kind` carries the daemon's own status name when the exit code is coarser than the failure (`ERR_PROTOCOL` covers five).
- `exec --stream` is the one exception to the first line: it writes NDJSON with the discriminant `microvm.exec.stream`. Every other invocation writes exactly one object.

## Closed option domains

Two options carry a closed set rather than free text, and the parser refuses everything else before any handler runs. Refusing an off-table value in the parser reports the error immediately, while refusing it in `microvms-core` would cost a build cycle first. `microvms-cli/src/cli.rs:4-19`.

`--memory` accepts exactly `512`, `1024`, `2048`, `4096`, `8192`, the five documented size-class baselines. `microvms-cli/src/cli.rs:225-237`.

`--region` accepts exactly `us-east-1`, `us-east-2`, `us-west-2`, `eu-west-1`, `ap-northeast-1`, the five regions measured to carry MicroVMs. `eu-central-1` is excluded on measurement. `microvms-cli/src/cli.rs:258-270`.

The escape hatch is a separate flag rather than a permissive parser. `--unlisted-region <NAME>` conflicts with `--region` and carries its cost in its help text, so a reader of a command line can see that someone opted in. `microvms-cli/src/cli.rs:31-37`.

Four options — `--client-token`, `--capabilities`, `--connector`, and `--architecture` — are deliberately absent, because `microvms-core` has no parameter for the values they would carry. A test asserts their absence over every argument of every subcommand. `microvms-cli/src/cli.rs:21-29`, `microvms-cli/src/cli.rs:1179-1202`.

In the manifest, a boolean flag reports `type: "boolean"` and `choices: null` even though clap gives a `SetTrue` flag the possible values `["true", "false"]`. Publishing those would put a `choices` array on every flag and make the closed-domain field unreadable. `microvms-cli/src/manifest.rs:133-152`.

## See also

- [microvms-agentd · Processes](../behavior/processes.md)
- [microvms-agentd · Contract map](../insights/contract-map.md)
- [microvms-agentd · Debugging guide](../insights/debugging-guide.md)
- [microvms-agentd · Data flow](../architecture/data-flow.md)
- [microvms-agentd · Impact analysis](../insights/impact-analysis.md)
