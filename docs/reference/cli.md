# microvms-agentd · CLI

The `microvm` binary has sixteen subcommands, declared as one `clap` `Subcommand` enum in `microvms-cli/src/cli.rs:90-215` and built from `microvms-cli/Cargo.toml:20-22`.

## Global flags

These three are `global = true`, so they parse on either side of the subcommand — `microvm --json ls` and `microvm ls --json` are the same invocation. `microvms-cli/src/cli.rs:61-79`.

Flags:

- `--json` — emit the typed JSON envelope on stdout instead of human output; wins over every other format, including an interactive terminal. `microvms-cli/src/cli.rs:69-70`.
- `--dense` — token-lean output, for a consumer paying per token. `microvms-cli/src/cli.rs:73-74`.
- `--quiet` — suppress progress on stderr; warnings still print. `microvms-cli/src/cli.rs:77-78`.

Format resolution is a total function of the two flags plus whether stdout is a terminal: `--json` first, then `--dense`, then a ratatui surface for a terminal, then plain text for a pipe. `microvms-cli/src/envelope.rs:301-308`.

## Shared flag groups

Three flattened `Args` structs supply the flags that repeat across commands, so a relationship like the region conflict is declared once rather than per command.

`RegionFlags` — flattened into every command that talks to AWS. `microvms-cli/src/cli.rs:289-301`.

Flags:

- `--region <REGION>` — AWS region; defaults to `$AWS_REGION`, then `$AWS_DEFAULT_REGION`, then `us-east-1`. Closed set: `us-east-1`, `us-east-2`, `us-west-2`, `eu-west-1`, `ap-northeast-1`. `microvms-cli/src/cli.rs:292-293`, domain at `microvms-cli/src/cli.rs:258-270`.
- `--unlisted-region <NAME>` — use a region this client has not seen carry MicroVMs; conflicts with `--region`. An unsupported region answers `AccessDeniedException` with a null message, which reads as an IAM denial, so this costs the caller the diagnostic. `microvms-cli/src/cli.rs:299-300`.

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

Builds an image, launches a VM, runs a command, reports the cost, and tears it down — tearing down by default so a closed laptop does not leave a billable VM.

`microvms-cli/src/commands/lifecycle.rs:119`

Flags:

- `[BINARY]` — the aarch64 agentd binary to bake in as the image CMD; ignored when `--image` names an image to launch instead. `microvms-cli/src/cli.rs:353-354`.
- `--image <IDENTIFIER>` — launch this existing image instead of building one. `microvms-cli/src/cli.rs:360-361`.
- `--artifact-uri <S3_URI>` — where the build artifact already is; `microvms-core` builds the artifact bytes and takes the URI but does not upload. `microvms-cli/src/cli.rs:368-369`.
- `--exec <COMMAND>` — a shell command to run in the VM; omitted launches and tears down, which is how you check that an image boots. `microvms-cli/src/cli.rs:374-375`.
- `--name <NAME>` — image name; defaults to a per-invocation name, because reusing one is how a `clientToken` replay wedges an image. `microvms-cli/src/cli.rs:379-380`.
- `--memory <MEMORY>` — baseline MiB, selecting a documented size class; default `2048`. Closed set: `512`, `1024`, `2048`, `4096`, `8192`. `microvms-cli/src/cli.rs:387-388`.
- `--dockerfile <DOCKERFILE>` — a Dockerfile to use instead of the library's default; its `FROM` must match the base. `microvms-cli/src/cli.rs:391-392`.
- `--repair-identity` — widen the guest so `sethostname` and the `boot_id` bind mount work. `microvms-cli/src/cli.rs:397-398`.
- `--egress` — give the VM outbound network; omitted by default. `microvms-cli/src/cli.rs:401-402`.
- `--keep` — leave the VM and image running; you are then paying for them. `microvms-cli/src/cli.rs:405-406`.
- `--timeout <TIMEOUT>` — how long to wait for the exec, in seconds; default `300`. `microvms-cli/src/cli.rs:409-410`.
- `--max-idle-sec <MAX_IDLE_SEC>` — suspend the VM after this much inbound-traffic idleness; default `600`. `microvms-cli/src/cli.rs:413-414`.
- `--suspended-sec <SUSPENDED_SEC>` — terminate the VM after this long suspended; a resume past it cannot work. Default `600`. `microvms-cli/src/cli.rs:417-418`.
- `--max-duration-sec <MAX_DURATION_SEC>` — hard ceiling on the VM's life; refused above 28800 before any call. Default `3600`. `microvms-cli/src/cli.rs:421-422`.
- `--port <PORT>` — the daemon's port inside the guest. `microvms-cli/src/cli.rs:425-426`.
- `--state-dir <STATE_DIR>` — where the run ledger is written; defaults to `$MICROVM_STATE_DIR` or `~/.microvm/runs`. `microvms-cli/src/cli.rs:429-430`.
- Plus `RegionFlags` and `InfraFlags`. `microvms-cli/src/cli.rs:432-436`.

## build

```
microvm build [OPTIONS] <BINARY>
```

Builds a MicroVM image and waits for it to be usable, tearing nothing down — an image is the durable artifact and its one-week minimum snapshot retention means deleting it early saves nothing.

`microvms-cli/src/commands/lifecycle.rs:469`

Flags:

- `<BINARY>` — the aarch64 agentd binary to bake in as the image CMD. Required. `microvms-cli/src/cli.rs:442-443`.
- `--artifact-uri <S3_URI>` — where the build artifact already is, as an `s3://` URI. `microvms-cli/src/cli.rs:445-447`.
- `--name <NAME>` — image name; defaults to a per-invocation name. `microvms-cli/src/cli.rs:450-451`.
- `--memory <MEMORY>` — baseline MiB, selecting a documented size class; default `2048`. `microvms-cli/src/cli.rs:454-455`.
- `--dockerfile <DOCKERFILE>` — a Dockerfile to use instead of the library's default. `microvms-cli/src/cli.rs:458-459`.
- `--repair-identity` — widen the guest so `sethostname` and the `boot_id` bind mount work. `microvms-cli/src/cli.rs:462-463`.
- `--port <PORT>` — the daemon's port inside the guest. `microvms-cli/src/cli.rs:466-467`.
- Plus `RegionFlags` and `InfraFlags`. `microvms-cli/src/cli.rs:469-473`.

## exec

```
microvm exec [OPTIONS] --endpoint <ENDPOINT> --agent-token <AGENT_TOKEN> --microvm-id <MICROVM_ID> [COMMAND]
```

Runs one command in a MicroVM that is already running, in one of four shapes over a single subcommand: start and wait, start and watch (`--stream`), start and feed (`--stdin`), or read an existing exec (`--poll`).

`microvms-cli/src/commands/attached.rs:107`

Flags:

- `[COMMAND]` — a shell command to run in the VM; omitted only with `--poll`. `microvms-cli/src/cli.rs:479-480`.
- `--timeout <TIMEOUT>` — how long to wait for the command, in seconds; default `300`. `microvms-cli/src/cli.rs:483-484`.
- `--cwd <CWD>` — working directory; omitted inherits the image WORKDIR, which is not the same as passing `/`. `microvms-cli/src/cli.rs:490-491`.
- `--exec-id <ID>` — use this exec id instead of a fresh one, making a retry idempotent; the daemon returns success for a known id without spawning a second child. `microvms-cli/src/cli.rs:510-511`.
- `--poll <ID>` — read an existing exec's status and output instead of starting anything; read-only server-side, does not ack. Conflicts with `--exec-id`, `--stream`, `--stdin`, `--cwd`, `--detach`. `microvms-cli/src/cli.rs:519-520`.
- `--detach` — start the command and return immediately, without waiting and without acking; prints the exec id and `phase: running`. Conflicts with `--stream` and `--stdin`. `microvms-cli/src/cli.rs:539-540`.
- `--stream` — stream output as it arrives rather than waiting for the whole thing; under `--json` or into a pipe this writes NDJSON. `microvms-cli/src/cli.rs:549-550`.
- `--from-offset <BYTES>` — resume a stream at this byte offset; requires `--stream`. `microvms-cli/src/cli.rs:557-558`.
- `--stdin` — give the command a stdin pipe, feed it this process's stdin, then close it. `microvms-cli/src/cli.rs:565-566`.
- Plus `AttachFlags` and `RegionFlags`. `microvms-cli/src/cli.rs:568-572`.

## health

```
microvm health [OPTIONS] --endpoint <ENDPOINT> --agent-token <AGENT_TOKEN> --microvm-id <MICROVM_ID>
```

Asks a running MicroVM's daemon whether it is up and what its identity repair did — the one command that reports `identityDegraded` and `diskUnderPressure`, both reasons to drain a VM rather than keep scheduling onto it.

`microvms-cli/src/commands/attached.rs:471`

Flags:

- `AttachFlags` and `RegionFlags` only; this command has no arguments of its own. `microvms-cli/src/cli.rs:575-582`.

## ack

```
microvm ack [OPTIONS] --endpoint <ENDPOINT> --agent-token <AGENT_TOKEN> --microvm-id <MICROVM_ID> <EXEC_ID>
```

Releases a finished exec's buffered output, which starts its collection clock; a second ack is a 409 because the first one released it.

`microvms-cli/src/commands/attached.rs:574`

Flags:

- `<EXEC_ID>` — the exec whose output to release. Required. `microvms-cli/src/cli.rs:587-588`.
- Plus `AttachFlags` and `RegionFlags`. `microvms-cli/src/cli.rs:590-594`.

## stdin

```
microvm stdin [OPTIONS] --endpoint <ENDPOINT> --agent-token <AGENT_TOKEN> --microvm-id <MICROVM_ID> <EXEC_ID>
```

Writes to a running exec's stdin and optionally closes it; only for an exec started with `exec --stdin`, and nothing else closes the pipe.

`microvms-cli/src/commands/attached.rs:612`

Flags:

- `<EXEC_ID>` — the exec to write to; must have been started with `exec --stdin`. Required. `microvms-cli/src/cli.rs:600-601`.
- `--data <DATA>` — what to write; `-` reads this process's stdin, omitted writes nothing. Raw bytes either way — core base64-encodes them for the wire. `microvms-cli/src/cli.rs:607-608`.
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

Freezes a MicroVM, which keeps its memory, filesystem, token, and endpoint — a freeze and restore rather than a stop and start.

`microvms-cli/src/commands/lifecycle.rs:730`

Flags:

- `<MICROVM_ID>` — the MicroVM to freeze. Required. `microvms-cli/src/cli.rs:677-678`.
- `--timeout <TIMEOUT>` — how long to wait for the state transition, in seconds; default `300`. `microvms-cli/src/cli.rs:681-682`.
- Plus `RegionFlags`. `microvms-cli/src/cli.rs:684-685`.

## resume

```
microvm resume [OPTIONS] <MICROVM_ID>
```

Thaws a suspended MicroVM and reports its endpoint; past the launch-time `suspendedDurationSeconds` window the VM is gone rather than slow.

`microvms-cli/src/commands/lifecycle.rs:795`

Flags:

- `<MICROVM_ID>` — the MicroVM to thaw. Required. `microvms-cli/src/cli.rs:691-692`.
- `--timeout <TIMEOUT>` — how long to wait for RUNNING, in seconds; default `300`. `microvms-cli/src/cli.rs:695-696`.
- Plus `RegionFlags`. `microvms-cli/src/cli.rs:698-699`.

## terminate

```
microvm terminate [OPTIONS] <MICROVM_ID>
```

Tears down a MicroVM and optionally its image and build log group, never failing on a teardown failure — it reports the identifier instead.

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

Lists what this CLI created and could not confirm it deleted, reading the local ledger rather than asking AWS — the resources worth asking about are the ones a killed process never got to report.

`microvms-cli/src/commands/local.rs:23`

Flags:

- `--state-dir <STATE_DIR>` — where the ledgers live; defaults to `$MICROVM_STATE_DIR` or `~/.microvm/runs`. `microvms-cli/src/cli.rs:733-734`.

## logs

```
microvm logs [OPTIONS] <IMAGE_NAME>
```

Names an image's build log group, `/aws/lambda-microvms/<image-name>`, which is where a failed build's only evidence lives.

`microvms-cli/src/commands/local.rs:146`

Flags:

- `<IMAGE_NAME>` — the image whose log group to name. Required. `microvms-cli/src/cli.rs:740-741`.
- Plus `RegionFlags`. `microvms-cli/src/cli.rs:743-744`.

## cost

```
microvm cost [OPTIONS]
```

Reports what a run cost or what a plan will cost, with every figure labelled: dollars are estimates derived from published rates and never an invoice, and a line item with no published rate reads `unpriced` rather than `$0.00`.

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

Checks every prerequisite and says which one is wrong — credentials, the region, the three Terraform outputs, whether the stack is applied, and whether the daemon binary is aarch64.

`microvms-cli/src/commands/doctor.rs:32`

Flags:

- `--binary <BINARY>` — the agentd binary to check the architecture of. `microvms-cli/src/cli.rs:798-799`.
- `--infra-dir <INFRA_DIR>` — the Terraform stack directory; defaults to `./conformance/infra`. `microvms-cli/src/cli.rs:802-803`.
- Plus `RegionFlags` and `InfraFlags`. `microvms-cli/src/cli.rs:805-809`.

## manifest

```
microvm manifest [OPTIONS]
```

Emits the whole command surface, its exit codes, and its envelope schema, generated from the registered clap tree rather than written down, so it cannot drift from what the binary accepts.

`microvms-cli/src/commands/local.rs:191`

The command takes no flags and always emits JSON, because the only consumer that asks for a manifest is one that parses it.

## constants

```
microvm constants [OPTIONS]
```

Emits every service constraint this client believes, for the drift gate that `scripts/check-model-drift` runs against the pinned botocore model.

`microvms-cli/src/commands/local.rs:214`

Flags:

- `--emit-json` — emit the raw constants object, unwrapped by an envelope; the one stdout write in this binary that is not an envelope. `microvms-cli/src/cli.rs:830-831`.

## The JSON envelope

Exactly one JSON object on stdout per invocation. Progress goes to stderr, always — `--quiet` suppresses progress but never a warning, because a leak nobody is told about is the failure `--quiet` must not be able to purchase. `microvms-cli/src/envelope.rs:1-18`. `apiVersion` is `"1"`, bumped when a field's meaning changes rather than when a command is added. `microvms-cli/src/envelope.rs:66`.

A success envelope carries `status`, `apiVersion`, `type`, and `data`. `type` is the discriminant to branch on first. `microvms-cli/src/envelope.rs:314-321`.

```
{
  "status": "ok",
  "apiVersion": "1",
  "type": "microvm.run",
  "data": { }
}
```

A failure envelope carries `status`, `apiVersion`, `error`, `code`, `exitCode`, `finding`, `suggestions`, and `data` — every field unconditional, because a key that appears conditionally is a key every consumer has to guard. `microvms-cli/src/envelope.rs:323-342`.

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

Branch on `code`, never on `error`. `microvms-cli/src/manifest.rs:113`.

### data.kind

`data.kind` carries the daemon's own status name when the exit code is coarser than the failure. Five `WireKind`s — `Conflict`, `NotFound`, `ProtocolError`, `StdinClosed`, `TooLarge` — collapse onto `ERR_PROTOCOL`, deliberately, because a shell branching on `$?` cannot act differently on a 400 than on a 409. A consumer that can act differently reads `data.kind`. `microvms-cli/src/exit.rs:39-44`, inserted at `microvms-cli/src/envelope.rs:329-331`, and pinned at `microvms-cli/src/exit.rs:532-548`.

A local reject reports no `data.kind`, because nothing reached the daemon. `microvms-cli/src/envelope.rs:450-453`.

### Response types

Each command declares its `type` discriminant and the `data` keys its success envelope carries. `microvms-cli/src/commands/mod.rs:102-205`.

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

## The NDJSON stream exception

`exec --stream` is the one invocation that writes more than one object to stdout. Under `--json` (or into a pipe asking for it) it emits NDJSON — one JSON object per event, then the envelope as the final line. This is a second, narrower contract rather than a relaxation of the one-envelope rule, and three things keep the two distinguishable. `microvms-cli/src/envelope.rs:35-54`.

First, the discriminant differs: the final envelope's `type` is `microvm.exec.stream`, never `microvm.exec`. A consumer branching on `type` learns which parse applies from the field it already reads first. `microvms-cli/src/commands/mod.rs:222-233`.

Second, the manifest publishes it. `exec`'s entry carries an `alternateResponse` object naming `when: "--stream"`, the `responseType`, the `responseKeys`, and a `stdout` description of the NDJSON shape — generated from the flag's presence in the command tree, so a `--stream` removed from `exec` takes the entry with it. `microvms-cli/src/manifest.rs:46-62`.

Third, the envelope is written compact once a stream has started, because "the last line is the envelope" is only true if the envelope is one line. A pretty-printed document at the end of an NDJSON stream would be seven broken records. `microvms-cli/src/envelope.rs:171-175`.

```
{"event":"output","stream":"stdout","offset":0,"bytes":12,"text":"hello world\n","lossy":false}
{"event":"exit","exitCode":0,"signal":null,"truncated":false,"writersMayBeAlive":false,"offset":12}
{"status":"ok","apiVersion":"1","type":"microvm.exec.stream","data":{"execId":"x-1","events":2,"bytes":12,"nextOffset":12,"gaps":0,"exitCode":0,"truncated":false}}
```

Three event kinds reach a line: `output` (with `stream`, `offset`, `bytes`, `text`, `lossy`), `gap` (with `from` and `to`), and `exit` (with `exitCode`, `signal`, `truncated`, `writersMayBeAlive`, `offset`). `microvms-cli/src/commands/attached.rs:339-378`.

Output arrives as lossy text beside the true byte count rather than as base64, and `lossy` is set when the conversion actually replaced anything — so a consumer is never silently handed altered bytes. The faithful path for exact bytes is the non-JSON one. `microvms-cli/src/commands/attached.rs:325-338`.

The stream envelope's keys are a summary rather than the output — the output was the NDJSON, and repeating it would double a stream's memory cost for a consumer that has already seen every byte. `events` and `bytes` let a caller assert it read everything, and `nextOffset` is where a resume with `--from-offset` would continue. `microvms-cli/src/commands/mod.rs:207-233`.

The events cannot go to stderr instead, which would preserve the simpler rule: they are the command's output, not progress about it. Sending a workload's stdout to the caller's stderr would make `microvm exec --stream build.sh > log` write an empty log, and buffering the events to keep stdout a single document would remove the only reason to stream. `microvms-cli/src/envelope.rs:51-54`.

On the non-JSON formats there is no NDJSON at all: the raw child bytes go to stdout untouched, because that is what they are, and no lossy string conversion is applied. `microvms-cli/src/envelope.rs:232-247`.

A stream that fails part-way through has already written events and no envelope. On the JSON path the failure envelope becomes the stream's compact last line — an NDJSON consumer reading line by line needs a terminating record saying why the events stopped. On the human paths the same failure goes to stderr instead, because appending an error message to the child's raw output would corrupt the file a caller was redirecting into. `microvms-cli/src/main.rs:319-333`.

A stream that ended without an exit event was cut, and `exitCode` is reported as `null` rather than `0`: reporting zero would turn a truncated stream into a passing build, and the command exits `ERR_EXEC_FAILED`. `microvms-cli/src/commands/attached.rs:290-321`.

## Exit codes

Fourteen rows, 0 through 13, append-only because consumers branch on them. Split by what the caller should do next, which is the only distinction worth a separate integer. `microvms-cli/src/exit.rs:171-256`, with the enum's explicit discriminants at `microvms-cli/src/exit.rs:76-100`.

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

`ERR_EXEC_FAILED` has its own code because it is the one non-zero exit that means nothing is wrong with the platform, the credentials, or the CLI — a CI caller needs to tell "your tests failed" from "we never got a VM". `microvms-cli/src/exit.rs:94-99`.

A clap parse failure maps to exit 2 / `ERR_INVALID_ARG`, forwarding clap's own message verbatim including its did-you-mean line. That agrees with clap's own convention deliberately, so a caller who reads `$?` sees the same number either way. `microvms-cli/src/exit.rs:381-384`, `microvms-cli/src/cli.rs:56-59`.

`--help` and `--version` are successes that print themselves and exit 0, never becoming envelopes. `microvms-cli/src/main.rs:78-87`.

### Suggestions

The exit code comes from the failure class and nothing else; what the CLI adds is the suggestion, which is CLI-shaped rather than library-shaped — the library says what went wrong, the CLI says which flag or command addresses it. Two failures sharing `ERR_CREDENTIALS` get different remedies: a 401 names the agent token, an unresolvable credential chain names `microvm doctor` and the unsupported-region null-message signature. `microvms-cli/src/exit.rs:321-368`.

## Conventions

`microvm manifest` publishes six conventions alongside the command tree. `microvms-cli/src/manifest.rs:111-128`.

- Exactly one envelope object on stdout per invocation; progress is on stderr.
- Branch on `code`, never on `error`.
- Dollar figures are estimates derived from published rates, never an invoice.
- An unpriced line item omits `usd` rather than reporting zero.
- `data.kind` carries the daemon's own status name when the exit code is coarser than the failure (`ERR_PROTOCOL` covers five).
- `exec --stream` is the one exception to the first line: it writes NDJSON with the discriminant `microvm.exec.stream`. Every other invocation writes exactly one object.

## Closed option domains

Two options carry a closed set rather than free text, and the parser refuses everything else before any handler runs — the difference between refusing an off-table value here and refusing it in `microvms-core` is a build cycle. `microvms-cli/src/cli.rs:4-19`.

`--memory` accepts exactly `512`, `1024`, `2048`, `4096`, `8192`, the five documented size-class baselines. `microvms-cli/src/cli.rs:225-237`.

`--region` accepts exactly `us-east-1`, `us-east-2`, `us-west-2`, `eu-west-1`, `ap-northeast-1`, the five regions measured to carry MicroVMs. `eu-central-1` is excluded on measurement. `microvms-cli/src/cli.rs:258-270`.

The escape hatch is a separate flag rather than a permissive parser: `--unlisted-region <NAME>` conflicts with `--region` and carries its cost in its help text, so a reader of a command line can see that someone opted in. `microvms-cli/src/cli.rs:31-37`.

Four options are deliberately absent, because `microvms-core` has no parameter for the values they would carry: `--client-token`, `--capabilities`, `--connector`, and `--architecture`. Their absence is asserted over every argument of every subcommand. `microvms-cli/src/cli.rs:21-29`, `microvms-cli/src/cli.rs:1179-1202`.

In the manifest, a boolean flag reports `type: "boolean"` and `choices: null` even though clap gives a `SetTrue` flag the possible values `["true", "false"]` — publishing those would put a `choices` array on every flag and make the closed-domain field unreadable. `microvms-cli/src/manifest.rs:133-152`.

## See also

- [microvms-agentd · Processes](../behavior/processes.md)
- [microvms-agentd · Contract map](../insights/contract-map.md)
- [microvms-agentd · Debugging guide](../insights/debugging-guide.md)
- [microvms-agentd · Data flow](../architecture/data-flow.md)
- [microvms-agentd · Impact analysis](../insights/impact-analysis.md)
