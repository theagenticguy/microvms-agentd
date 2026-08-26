# microvms-agentd · CLI

The `microvm` binary (`microvms-cli/Cargo.toml:18-20`) routes seventeen subcommands, listed below in the lifecycle order its `clap` `Subcommand` enum declares (`microvms-cli/src/cli.rs:93-229`), and every one of them also accepts the three `global = true` flags `--json`, `--dense`, and `--quiet` (`microvms-cli/src/cli.rs:63-81`) plus clap's generated `-h/--help` and `-V/--version` (`microvms-cli/src/cli.rs:50-51`).

## run

```
microvm run [OPTIONS] [BINARY]
```

Build an image, launch a VM, run a command, report the cost, tear it down. `microvms-cli/src/cli.rs:99`

`microvms-cli/src/commands/lifecycle.rs:121`

Flags:

- `[BINARY]` — the aarch64 agentd binary to bake in as the image CMD, ignored when `--image` names an image to launch instead of building one. `microvms-cli/src/cli.rs:384`.
- `--image <IDENTIFIER>` — launch this existing image instead of building one; takes an ARN or a name, and a bare name is resolved to its ARN through the account's image listing before the launch. `microvms-cli/src/cli.rs:398`.
- `--image-version <VERSION>` — launch this exact image version instead of the image's latest active one. Free text, because a version's legal values are an account fact; the `Version` shape is checked in `microvms-core` before any call. `microvms-cli/src/cli.rs:416`.
- `--artifact-uri <S3_URI>` — where the build artifact already is, as an `s3://` URI, for a caller who has uploaded already. `microvms-cli/src/cli.rs:424`.
- `--exec <COMMAND>` — a shell command to run in the VM; omitted launches and tears down, which is how you check that an image boots at all. `microvms-cli/src/cli.rs:430`.
- `--name <NAME>` — image name, defaulting to a per-invocation name because reusing one is how a `clientToken` replay wedges an image. `microvms-cli/src/cli.rs:435`.
- `--memory <MEMORY>` — baseline MiB, which selects a documented size class; defaults to `2048` and accepts only `512`, `1024`, `2048`, `4096`, `8192` (`microvms-cli/src/cli.rs:240-251`). `microvms-cli/src/cli.rs:443`.
- `--dockerfile <DOCKERFILE>` — a Dockerfile to use instead of the library's default, whose `FROM` must match the base. `microvms-cli/src/cli.rs:447`.
- `--repair-identity` — widen the guest so `sethostname` and the `boot_id` bind mount work, which root in the guest is not enough for. `microvms-cli/src/cli.rs:453`.
- `--egress` — give the VM outbound network; omitted by default, because a daemon needs none. `microvms-cli/src/cli.rs:457`.
- `--launch-env <KEY=VALUE>` — set one launch-environment variable for every exec in the VM, repeatable, parsed by the same first-`=` splitter as `exec --env` (`microvms-cli/src/cli.rs:1006`). `microvms-cli/src/cli.rs:478`.
- `--keep` — leave the VM and image running, which means you are then paying for them. `microvms-cli/src/cli.rs:482`.
- `--timeout <TIMEOUT>` — how long to wait for the exec, in seconds; defaults to `300`. `microvms-cli/src/cli.rs:486`.
- `--max-idle-sec <MAX_IDLE_SEC>` — suspend the VM after this much inbound-traffic idleness; defaults to `600`. `microvms-cli/src/cli.rs:490`.
- `--suspended-sec <SUSPENDED_SEC>` — terminate the VM after this long suspended, past which a resume cannot work; defaults to `600`. `microvms-cli/src/cli.rs:494`.
- `--max-duration-sec <MAX_DURATION_SEC>` — hard ceiling on the VM's life, refused above 28800 before any call; defaults to `3600`. `microvms-cli/src/cli.rs:498`.
- `--port <PORT>` — the daemon's port inside the guest. `microvms-cli/src/cli.rs:502`.
- `--state-dir <STATE_DIR>` — where the run ledger is written, defaulting to `$MICROVM_STATE_DIR` or `~/.microvm/runs`. `microvms-cli/src/cli.rs:506`.
- `--region <REGION>` — AWS region, defaulting to `$AWS_REGION`, then `$AWS_DEFAULT_REGION`, then `us-east-1`; accepts only `us-east-1`, `us-east-2`, `us-west-2`, `eu-west-1`, `ap-northeast-1` (`microvms-cli/src/cli.rs:273-284`). `microvms-cli/src/cli.rs:307`.
- `--unlisted-region <NAME>` — use a region this client has not seen carry MicroVMs, which costs you the diagnostic; conflicts with `--region`. `microvms-cli/src/cli.rs:314`.
- `--bucket <BUCKET>` — S3 bucket for the build artifact, defaulting to `$MICROVM_BUCKET`. `microvms-cli/src/cli.rs:365`.
- `--build-role-arn <BUILD_ROLE_ARN>` — build role ARN, defaulting to `$MICROVM_BUILD_ROLE_ARN`. `microvms-cli/src/cli.rs:369`.
- `--execution-role-arn <EXECUTION_ROLE_ARN>` — execution role ARN, defaulting to `$MICROVM_EXECUTION_ROLE_ARN`. `microvms-cli/src/cli.rs:373`.

## build

```
microvm build [OPTIONS] <BINARY>
```

Build a MicroVM image and wait for it to be usable. `microvms-cli/src/cli.rs:107`

`microvms-cli/src/commands/lifecycle.rs:515`

Flags:

- `<BINARY>` — the aarch64 agentd binary to bake in as the image CMD; required. `microvms-cli/src/cli.rs:519`.
- `--artifact-uri <S3_URI>` — where the build artifact already is, as an `s3://` URI. `microvms-cli/src/cli.rs:523`.
- `--name <NAME>` — image name, defaulting to a per-invocation name. `microvms-cli/src/cli.rs:527`.
- `--memory <MEMORY>` — baseline MiB, selecting a documented size class; defaults to `2048`, domain at `microvms-cli/src/cli.rs:240-251`. `microvms-cli/src/cli.rs:531`.
- `--dockerfile <DOCKERFILE>` — a Dockerfile to use instead of the library's default. `microvms-cli/src/cli.rs:535`.
- `--base-image-version <VERSION>` — pin the managed base image to one version instead of taking the service's default, whose legal values come from `ListManagedMicrovmImageVersions`. `microvms-cli/src/cli.rs:553`.
- `--repair-identity` — widen the guest so `sethostname` and the `boot_id` bind mount work. `microvms-cli/src/cli.rs:557`.
- `--reuse` — reuse an existing image whose build inputs match, instead of building, by deriving the image name `<name>-<hash12>` from a sha256 over the daemon binary and the Dockerfile. `microvms-cli/src/cli.rs:574`.
- `--port <PORT>` — the daemon's port inside the guest. `microvms-cli/src/cli.rs:578`.
- `--region <REGION>` — AWS region; closed domain at `microvms-cli/src/cli.rs:273-284`. `microvms-cli/src/cli.rs:307`.
- `--unlisted-region <NAME>` — use a region this client has not seen carry MicroVMs; conflicts with `--region`. `microvms-cli/src/cli.rs:314`.
- `--bucket <BUCKET>` — S3 bucket for the build artifact, defaulting to `$MICROVM_BUCKET`. `microvms-cli/src/cli.rs:365`.
- `--build-role-arn <BUILD_ROLE_ARN>` — build role ARN, defaulting to `$MICROVM_BUILD_ROLE_ARN`. `microvms-cli/src/cli.rs:369`.
- `--execution-role-arn <EXECUTION_ROLE_ARN>` — execution role ARN, defaulting to `$MICROVM_EXECUTION_ROLE_ARN`. `microvms-cli/src/cli.rs:373`.

## exec

```
microvm exec [OPTIONS] --endpoint <ENDPOINT> --agent-token <AGENT_TOKEN> --microvm-id <MICROVM_ID> [COMMAND]
```

Run one command in a MicroVM that is already running. `microvms-cli/src/cli.rs:115`

`microvms-cli/src/commands/attached.rs:103`

Flags:

- `[COMMAND]` — a shell command to run in the VM, omitted only with `--poll`. `microvms-cli/src/cli.rs:591`.
- `--timeout <TIMEOUT>` — how long to wait for the command, in seconds; defaults to `300`. `microvms-cli/src/cli.rs:595`.
- `--cwd <CWD>` — working directory; omitted inherits the image `WORKDIR`, which is not the same as passing `/`. `microvms-cli/src/cli.rs:602`.
- `--env <KEY=VALUE>` — set one environment variable for the command, repeatable, and these flags are the child's whole environment because the daemon starts every exec from an empty one. Split at the first `=`; an empty VALUE is legal, a missing `=` or empty KEY is refused at parse time (`microvms-cli/src/cli.rs:1006`). `microvms-cli/src/cli.rs:618`.
- `--user <UID>` — numeric uid to run the command as; omitted runs as the daemon's own user. `microvms-cli/src/cli.rs:628`.
- `--group <GID>` — numeric gid to run the command as; omitted keeps the daemon's own group. `microvms-cli/src/cli.rs:632`.
- `--exec-id <ID>` — use this exec id instead of a fresh one, making a retry idempotent. `microvms-cli/src/cli.rs:652`.
- `--poll <ID>` — read an existing exec's status and output instead of starting anything; conflicts with `--exec-id`, `--stream`, `--stdin`, `--cwd`, `--detach`, `--env`, `--user`, `--group`. `microvms-cli/src/cli.rs:660-661`.
- `--detach` — start the command and return immediately, without waiting and without acking; conflicts with `--stream` and `--stdin`. `microvms-cli/src/cli.rs:680-681`.
- `--stream` — stream output as it arrives rather than waiting for the whole thing, which under `--json` or into a pipe writes NDJSON declared in the manifest as `responseType: microvm.exec.stream`. `microvms-cli/src/cli.rs:691`.
- `--from-offset <BYTES>` — resume a stream at this byte offset; requires `--stream`. `microvms-cli/src/cli.rs:698-699`.
- `--stdin` — give the command a stdin pipe and feed it this process's stdin, then close it. `microvms-cli/src/cli.rs:707`.
- `--endpoint <ENDPOINT>` — the VM's endpoint, as reported by `run`; required. `microvms-cli/src/cli.rs:345`.
- `--agent-token <AGENT_TOKEN>` — the agent token delivered to the VM at launch; required. `microvms-cli/src/cli.rs:349`.
- `--microvm-id <MICROVM_ID>` — the MicroVM id, needed to mint the endpoint proxy token; required. `microvms-cli/src/cli.rs:353`.
- `--port <PORT>` — the daemon's port inside the guest. `microvms-cli/src/cli.rs:357`.
- `--region <REGION>` — AWS region; closed domain at `microvms-cli/src/cli.rs:273-284`. `microvms-cli/src/cli.rs:307`.
- `--unlisted-region <NAME>` — use a region this client has not seen carry MicroVMs; conflicts with `--region`. `microvms-cli/src/cli.rs:314`.

## health

```
microvm health [OPTIONS] --endpoint <ENDPOINT> --agent-token <AGENT_TOKEN> --microvm-id <MICROVM_ID>
```

Ask a running MicroVM's daemon whether it is up, and what its identity repair did. `microvms-cli/src/cli.rs:123`

`microvms-cli/src/commands/attached.rs:475`

Flags:

- `--endpoint <ENDPOINT>` — the VM's endpoint, as reported by `run`; required. `microvms-cli/src/cli.rs:345`.
- `--agent-token <AGENT_TOKEN>` — the agent token delivered to the VM at launch; required. `microvms-cli/src/cli.rs:349`.
- `--microvm-id <MICROVM_ID>` — the MicroVM id, needed to mint the endpoint proxy token; required. `microvms-cli/src/cli.rs:353`.
- `--port <PORT>` — the daemon's port inside the guest. `microvms-cli/src/cli.rs:357`.
- `--region <REGION>` — AWS region; closed domain at `microvms-cli/src/cli.rs:273-284`. `microvms-cli/src/cli.rs:307`.
- `--unlisted-region <NAME>` — use a region this client has not seen carry MicroVMs; conflicts with `--region`. `microvms-cli/src/cli.rs:314`.

## ack

```
microvm ack [OPTIONS] --endpoint <ENDPOINT> --agent-token <AGENT_TOKEN> --microvm-id <MICROVM_ID> <EXEC_ID>
```

Release a finished exec's buffered output, which starts its collection clock. `microvms-cli/src/cli.rs:131`

`microvms-cli/src/commands/attached.rs:600`

Flags:

- `<EXEC_ID>` — the exec whose output to release; required. `microvms-cli/src/cli.rs:729`.
- `--endpoint <ENDPOINT>` — the VM's endpoint, as reported by `run`; required. `microvms-cli/src/cli.rs:345`.
- `--agent-token <AGENT_TOKEN>` — the agent token delivered to the VM at launch; required. `microvms-cli/src/cli.rs:349`.
- `--microvm-id <MICROVM_ID>` — the MicroVM id, needed to mint the endpoint proxy token; required. `microvms-cli/src/cli.rs:353`.
- `--port <PORT>` — the daemon's port inside the guest. `microvms-cli/src/cli.rs:357`.
- `--region <REGION>` — AWS region; closed domain at `microvms-cli/src/cli.rs:273-284`. `microvms-cli/src/cli.rs:307`.
- `--unlisted-region <NAME>` — use a region this client has not seen carry MicroVMs; conflicts with `--region`. `microvms-cli/src/cli.rs:314`.

## stdin

```
microvm stdin [OPTIONS] --endpoint <ENDPOINT> --agent-token <AGENT_TOKEN> --microvm-id <MICROVM_ID> <EXEC_ID>
```

Write to a running exec's stdin, and optionally close it. `microvms-cli/src/cli.rs:139`

`microvms-cli/src/commands/attached.rs:638`

Flags:

- `<EXEC_ID>` — the exec to write to, which must have been started with `exec --stdin`; required. `microvms-cli/src/cli.rs:742`.
- `--data <DATA>` — what to write, where `-` reads this process's stdin and omitting it writes nothing. `microvms-cli/src/cli.rs:749`.
- `--eof` — close stdin after any `--data` is written, in the same request rather than a second one. `microvms-cli/src/cli.rs:757`.
- `--endpoint <ENDPOINT>` — the VM's endpoint, as reported by `run`; required. `microvms-cli/src/cli.rs:345`.
- `--agent-token <AGENT_TOKEN>` — the agent token delivered to the VM at launch; required. `microvms-cli/src/cli.rs:349`.
- `--microvm-id <MICROVM_ID>` — the MicroVM id, needed to mint the endpoint proxy token; required. `microvms-cli/src/cli.rs:353`.
- `--port <PORT>` — the daemon's port inside the guest. `microvms-cli/src/cli.rs:357`.
- `--region <REGION>` — AWS region; closed domain at `microvms-cli/src/cli.rs:273-284`. `microvms-cli/src/cli.rs:307`.
- `--unlisted-region <NAME>` — use a region this client has not seen carry MicroVMs; conflicts with `--region`. `microvms-cli/src/cli.rs:314`.

## cp

```
microvm cp [OPTIONS] --endpoint <ENDPOINT> --agent-token <AGENT_TOKEN> --microvm-id <MICROVM_ID> <SRC> <DST>
```

Copy a file or a tar archive between here and a running MicroVM. `microvms-cli/src/cli.rs:148`

`microvms-cli/src/commands/attached.rs:805`

Flags:

- `<SRC>` — source, where `vm:/path` reads from the VM and anything else is a local path; required. `microvms-cli/src/cli.rs:770`.
- `<DST>` — destination, where `vm:/path` writes to the VM and anything else is a local path; required. `microvms-cli/src/cli.rs:774`.
- `--tar` — move a whole directory tree, as an uncompressed tar archive, where the `vm:` side is a directory the daemon packs or extracts and the local side is a `.tar` file. `microvms-cli/src/cli.rs:798`.
- `--mode <OCTAL>` — permissions for an uploaded file, octal as a string (`644`, `0755`); conflicts with `--tar`, whose members carry their own modes. `microvms-cli/src/cli.rs:805-806`.
- `--endpoint <ENDPOINT>` — the VM's endpoint, as reported by `run`; required. `microvms-cli/src/cli.rs:345`.
- `--agent-token <AGENT_TOKEN>` — the agent token delivered to the VM at launch; required. `microvms-cli/src/cli.rs:349`.
- `--microvm-id <MICROVM_ID>` — the MicroVM id, needed to mint the endpoint proxy token; required. `microvms-cli/src/cli.rs:353`.
- `--port <PORT>` — the daemon's port inside the guest. `microvms-cli/src/cli.rs:357`.
- `--region <REGION>` — AWS region; closed domain at `microvms-cli/src/cli.rs:273-284`. `microvms-cli/src/cli.rs:307`.
- `--unlisted-region <NAME>` — use a region this client has not seen carry MicroVMs; conflicts with `--region`. `microvms-cli/src/cli.rs:314`.

## suspend

```
microvm suspend [OPTIONS] <MICROVM_ID>
```

Freeze a MicroVM; it keeps its memory, filesystem, token, and endpoint. `microvms-cli/src/cli.rs:156`

`microvms-cli/src/commands/lifecycle.rs:915`

Flags:

- `<MICROVM_ID>` — the MicroVM to freeze; required. `microvms-cli/src/cli.rs:819`.
- `--timeout <TIMEOUT>` — how long to wait for the state transition, in seconds; defaults to `300`. `microvms-cli/src/cli.rs:823`.
- `--region <REGION>` — AWS region; closed domain at `microvms-cli/src/cli.rs:273-284`. `microvms-cli/src/cli.rs:307`.
- `--unlisted-region <NAME>` — use a region this client has not seen carry MicroVMs; conflicts with `--region`. `microvms-cli/src/cli.rs:314`.

## resume

```
microvm resume [OPTIONS] <MICROVM_ID>
```

Thaw a suspended MicroVM and report its endpoint. `microvms-cli/src/cli.rs:162`

`microvms-cli/src/commands/lifecycle.rs:976`

Flags:

- `<MICROVM_ID>` — the MicroVM to thaw; required. `microvms-cli/src/cli.rs:833`.
- `--timeout <TIMEOUT>` — how long to wait for RUNNING, in seconds; defaults to `300`. `microvms-cli/src/cli.rs:837`.
- `--region <REGION>` — AWS region; closed domain at `microvms-cli/src/cli.rs:273-284`. `microvms-cli/src/cli.rs:307`.
- `--unlisted-region <NAME>` — use a region this client has not seen carry MicroVMs; conflicts with `--region`. `microvms-cli/src/cli.rs:314`.

## terminate

```
microvm terminate [OPTIONS] <MICROVM_ID>
```

Tear down a MicroVM, and optionally its image and build log group. `microvms-cli/src/cli.rs:169`

`microvms-cli/src/commands/lifecycle.rs:1017`

Flags:

- `<MICROVM_ID>` — the MicroVM to terminate; required. `microvms-cli/src/cli.rs:847`.
- `--image-identifier <IMAGE_IDENTIFIER>` — the image to delete, if `--delete-image` is given. `microvms-cli/src/cli.rs:851`.
- `--image-name <IMAGE_NAME>` — the image's name, needed to name its build log group, which the service created and `terraform destroy` never removes. `microvms-cli/src/cli.rs:857`.
- `--delete-image` — also delete the image, and name its build log group; requires `--image-identifier`. `microvms-cli/src/cli.rs:860-861`.
- `--wait` — wait for TERMINATED rather than returning as soon as the call is accepted. `microvms-cli/src/cli.rs:865`.
- `--region <REGION>` — AWS region; closed domain at `microvms-cli/src/cli.rs:273-284`. `microvms-cli/src/cli.rs:307`.
- `--unlisted-region <NAME>` — use a region this client has not seen carry MicroVMs; conflicts with `--region`. `microvms-cli/src/cli.rs:314`.

## ls

```
microvm ls [OPTIONS]
```

List what this CLI created and could not confirm it deleted. `microvms-cli/src/cli.rs:177`

`microvms-cli/src/commands/local.rs:23`

Flags:

- `--state-dir <STATE_DIR>` — where the ledgers live, defaulting to `$MICROVM_STATE_DIR` or `~/.microvm/runs`. `microvms-cli/src/cli.rs:875`.

## logs

```
microvm logs [OPTIONS] <IMAGE_NAME>
```

Name an image's build log group, which is where a failed build's only evidence lives. `microvms-cli/src/cli.rs:185`

`microvms-cli/src/commands/local.rs:148`

Flags:

- `<IMAGE_NAME>` — the image whose log group to name; required. `microvms-cli/src/cli.rs:882`.
- `--region <REGION>` — AWS region; closed domain at `microvms-cli/src/cli.rs:273-284`. `microvms-cli/src/cli.rs:307`.
- `--unlisted-region <NAME>` — use a region this client has not seen carry MicroVMs; conflicts with `--region`. `microvms-cli/src/cli.rs:314`.

## cost

```
microvm cost [OPTIONS]
```

What a run cost, or what a plan will cost, with every figure labelled. `microvms-cli/src/cli.rs:193`

`microvms-cli/src/commands/cost.rs:27`

Flags:

- `--estimate` — treat the durations as a plan rather than as timings, so every duration is labelled projected. `microvms-cli/src/cli.rs:895`.
- `--compare` — also print running versus suspended for the same hold, with the break-even. `microvms-cli/src/cli.rs:899`.
- `--memory <MEMORY>` — baseline MiB, selecting a documented size class; defaults to `2048`, domain at `microvms-cli/src/cli.rs:240-251`. `microvms-cli/src/cli.rs:903`.
- `--running-sec <RUNNING_SEC>` — seconds the VM spent, or will spend, RUNNING, billed at baseline whether or not anything is executing; defaults to `0`. `microvms-cli/src/cli.rs:910`.
- `--suspended-sec <SUSPENDED_SEC>` — seconds spent suspended, which is storage only with no compute line at all; defaults to `0`. `microvms-cli/src/cli.rs:914`.
- `--build-sec <BUILD_SEC>` — seconds the image build took, which appears as an unpriced line; defaults to `0`. `microvms-cli/src/cli.rs:921`.
- `--image-gb <IMAGE_GB>` — image size in GB, which adds storage with its one-week minimum retention. `microvms-cli/src/cli.rs:925`.
- `--cycles <CYCLES>` — suspend/resume cycles, each paying a snapshot write plus a read; defaults to `1`. `microvms-cli/src/cli.rs:929`.
- `--hold-sec <HOLD_SEC>` — the hold to compare running against suspended over, in seconds; defaults to `3600`. `microvms-cli/src/cli.rs:933`.

## doctor

```
microvm doctor [OPTIONS]
```

Check every prerequisite and say which one is wrong. `microvms-cli/src/cli.rs:202`

`microvms-cli/src/commands/doctor.rs:32`

Flags:

- `--binary <BINARY>` — the agentd binary to check the architecture of. `microvms-cli/src/cli.rs:940`.
- `--infra-dir <INFRA_DIR>` — the Terraform stack directory, defaulting to `./conformance/infra`. `microvms-cli/src/cli.rs:944`.
- `--region <REGION>` — AWS region; closed domain at `microvms-cli/src/cli.rs:273-284`. `microvms-cli/src/cli.rs:307`.
- `--unlisted-region <NAME>` — use a region this client has not seen carry MicroVMs; conflicts with `--region`. `microvms-cli/src/cli.rs:314`.
- `--bucket <BUCKET>` — S3 bucket for the build artifact, defaulting to `$MICROVM_BUCKET`. `microvms-cli/src/cli.rs:365`.
- `--build-role-arn <BUILD_ROLE_ARN>` — build role ARN, defaulting to `$MICROVM_BUILD_ROLE_ARN`. `microvms-cli/src/cli.rs:369`.
- `--execution-role-arn <EXECUTION_ROLE_ARN>` — execution role ARN, defaulting to `$MICROVM_EXECUTION_ROLE_ARN`. `microvms-cli/src/cli.rs:373`.

## manifest

```
microvm manifest [OPTIONS]
```

Emit the whole command surface, its exit codes, and its envelope schema. `microvms-cli/src/cli.rs:209`

`microvms-cli/src/commands/local.rs:193`

The only subcommand with no flags of its own — a unit variant on `Command`, so it takes the three global flags and nothing else. It is always JSON, because the dispatcher forces `wants_json` for this variant regardless of `--json` (`microvms-cli/src/main.rs:104`).

The exit-code catalogue it publishes has fourteen rows, is append-only, and is the contract a caller branches `$?` on. `microvms-cli/src/exit.rs:173-258`, with the enum's explicit discriminants at `microvms-cli/src/exit.rs:78-101`.

| Exit | Code | Meaning |
| --- | --- | --- |
| 0 | — | the command did what it said |
| 1 | `ERR_UNEXPECTED` | an exception no handler claimed — a bug in this CLI, not the platform |
| 2 | `ERR_INVALID_ARG` | the request was refused locally, before any AWS call |
| 3 | `ERR_RETRYABLE` | a transient condition; run the identical command again |
| 4 | `ERR_CREDENTIALS` | an identity is wrong or absent; waiting will not fix it |
| 5 | `ERR_PROTOCOL` | the daemon rejected the request on its merits |
| 6 | `ERR_BUILD_WEDGED` | the image build was never scheduled — the clientToken replay signature |
| 7 | `ERR_LAUNCH_DIED` | the MicroVM reached a terminal state before RUNNING; read stateReason |
| 8 | `ERR_WINDOW_CLOSED` | the launch-time suspended window passed, so there is nothing to resume |
| 9 | `ERR_PLATFORM` | a control-plane failure with no more specific class |
| 10 | `ERR_TIMEOUT` | a client-side deadline elapsed; the VM and the exec are untouched |
| 11 | `ERR_INTERRUPTED` | interrupted after launch; teardown ran and any leak is named in the payload |
| 12 | `ERR_PRECONDITION` | a prerequisite is missing — run `microvm doctor` |
| 13 | `ERR_EXEC_FAILED` | the sandbox worked and the command in it exited non-zero |

A clap usage failure exits 2, matching clap's own convention, and `--help` / `--version` are successes that print themselves and exit 0. `microvms-cli/src/cli.rs:58-60`, `microvms-cli/src/main.rs:80-89`.

## constants

```
microvm constants [OPTIONS]
```

Emit every service constraint this client believes, for the drift gate. `microvms-cli/src/cli.rs:218`

`microvms-cli/src/commands/local.rs:216`

Flags:

- `--emit-json` — emit the raw constants object, unwrapped by an envelope, for `scripts/check-model-drift.py`, which compares key-for-key against a pinned service model; the global `--json` wraps the same object instead. `microvms-cli/src/cli.rs:962`.

## dockerfile

```
microvm dockerfile [OPTIONS]
```

Print the Dockerfile stanza that wraps any base image with agentd. `microvms-cli/src/cli.rs:228`

`microvms-cli/src/commands/local.rs:251`

Flags:

- `--from <IMAGE_REF>` — the image ref for the `FROM` line, defaulting to the managed al2023 base's pair; only change it when you are also changing `baseImageArn`, because `microvms-core` refuses a Dockerfile whose `FROM` disagrees. `microvms-cli/src/cli.rs:974`.
- `--port <PORT>` — the port agentd listens on inside the guest; defaults to `9000`. `microvms-cli/src/cli.rs:978`.
- `--workdir <DIR>` — a working directory to create and set, strongly recommended because most public ARM64 base images declare no `WorkingDir`. `microvms-cli/src/cli.rs:986`.

## See also

- [processes](../behavior/processes.md) — 6 shared source citations
- [impact analysis](../insights/impact-analysis.md) — 6 shared source citations
- [contract map](../insights/contract-map.md) — 5 shared source citations
- [debugging guide](../insights/debugging-guide.md) — 4 shared source citations
- [tech debt](../insights/tech-debt.md) — 4 shared source citations
