# microvms-agentd · CLI

The `microvm` binary has eighteen subcommands, declared as one `clap` `Subcommand` enum in `microvms-cli/src/cli.rs` and built from `microvms-cli/Cargo.toml:20-22`.

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

`AttachFlags` — the identifiers that address a VM this invocation did not launch: the explicit triple, or a registered name standing in for it. Carried by `exec`, `health`, `ack`, `stdin`, and `cp`. `microvms-cli/src/cli.rs`, `AttachFlags`.

Flags:

- `--endpoint <ENDPOINT>` — the VM's endpoint, as reported by `run`. Required unless `--name` is given.
- `--agent-token <AGENT_TOKEN>` — the agent token delivered to the VM at launch. Required unless `--name` is given.
- `--microvm-id <MICROVM_ID>` — the MicroVM id, needed to mint the endpoint proxy token. Required unless `--name` is given.
- `--name <NAME>` — the name `run --keep --vm-name` registered, standing in for the whole triple. Resolved through the local name registry with zero AWS calls: the record carries the endpoint, the agent token, the MicroVM id, and the launch region (used as the region default when no `--region` flag is given). Conflicts with the explicit triple. A name this state directory never registered fails locally with `ERR_PRECONDITION`.
- `--port <PORT>` — the daemon's port inside the guest.
- `--state-dir <STATE_DIR>` — where the local state lives: the name registry, and exec's per-VM history. Defaults to `$MICROVM_STATE_DIR` or `~/.microvm/runs`.

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

- `[BINARY_OR_DIR]` — the aarch64 agentd binary to bake in as the image CMD (ignored when `--image` names an image to launch instead), or a directory to sync. The two readings cannot collide: a path is a directory or it is not. See "Sync mode" below. `microvms-cli/src/cli.rs`, `RunArgs::binary`.
- `--image <IDENTIFIER>` — launch this existing image instead of building one. Takes an ARN or a bare image name: a name is resolved to its ARN through the account's image listing (exact match, every page read) before the launch, with a progress line naming the resolved ARN. An identifier already shaped like an ARN passes through with zero extra calls. A name that resolves to nothing fails locally with `ERR_PRECONDITION` naming the name and suggesting `microvm build` — the service's own answer to a bare name is HTTP 400 "Malformed ARN", which says nothing about names. `microvms-cli/src/commands/lifecycle.rs:283-300`, resolution in `microvms-core/src/control/image.rs:411-475`.
- `--image-version <VERSION>` — launch this exact image version instead of the image's latest active one. Omitted takes whatever `latestActiveImageVersion` is at the moment the call lands, which is right for the ordinary case and wrong for the two that matter: a canary wants the version it just built rather than whatever became latest while it was starting, and a rollback wants the known-good version, which "latest" cannot name once a bad version is the latest one. A version the control plane has set `INACTIVE` refuses to launch when named here — measured, the answer is HTTP 404 `No active version found for MicroVM image <arn> and version <v>`, which is what makes a retire real rather than advisory. Free text rather than a closed set, because a version's legal values are an account fact only `ListManagedMicrovmImageVersions` can answer; the constraint that *is* knowable is checked before any call, so an empty version, one over 2048 characters, or one containing whitespace anywhere fails locally with `ERR_INVALID_ARG` and the reason. A version pasted from a terminal carries a trailing newline, which is that case. `microvms-core/src/control/mod.rs`'s `require_valid_version`, wiring in `microvms-cli/src/commands/lifecycle.rs`.
- `--artifact-uri <S3_URI>` — where the build artifact already is; `microvms-core` builds the artifact bytes and takes the URI but does not upload. `microvms-cli/src/cli.rs:368-369`.
- `--exec <COMMAND>` — a shell command to run in the VM. When it is omitted, the run only launches and tears down, which is how you check that an image boots. `microvms-cli/src/cli.rs:374-375`.
- `--name <NAME>` — image name; defaults to a per-invocation name, because reusing a name can trigger a `clientToken` replay that wedges the image. `microvms-cli/src/cli.rs:379-380`.
- `--vm-name <NAME>` — register a local name for the kept VM, so later commands can say `--name <NAME>` (attached commands) or use the name as the positional (suspend, resume, terminate, history) instead of pasting identifiers. Requires `--keep`. The name is a purely local fact in the state directory's registry (`<state-dir>/names/<NAME>.json`, written owner-only because the record carries the agent token), costs zero AWS calls, and is released when a terminate is accepted. Names take ASCII letters, digits, `-` and `_`, at most 128 bytes, and never a MicroVM id prefix (`microvm-` is the service's real prefix, measured live; `mvm-` is the test fixtures') — that exclusion is what lets every identifier-taking command tell a name from a MicroVM id. A name registered to a live VM is refused locally with `ERR_NAME_TAKEN` (exit 14) before any billable call. `microvms-cli/src/cli.rs`, `RunArgs::vm_name`; registry in `microvms-cli/src/ledger.rs`, `Names`.
- `--memory <MEMORY>` — baseline MiB, selecting a documented size class; default `2048`. Closed set: `512`, `1024`, `2048`, `4096`, `8192`. `microvms-cli/src/cli.rs:387-388`.
- `--dockerfile <DOCKERFILE>` — a Dockerfile to use instead of the library's default; its `FROM` must match the base. `microvms-cli/src/cli.rs:391-392`.
- `--log-group <GROUP>` / `--log-stream <STREAM>` — build-log destination, applied when this invocation builds; same semantics as `build`'s flags (the stream is a prefix the client suffixes with `/<16 hex>` per build). Both are also `microvm.toml` keys (`log-group`, `log-stream`), with the flag winning per knob; a stream with no group from either layer is refused locally. `microvms-cli/src/cli.rs`, merge in `microvms-cli/src/commands/lifecycle.rs`'s `merge_config`.
- `--repair-identity` — widen the guest so `sethostname` and the `boot_id` bind mount work. `microvms-cli/src/cli.rs:397-398`.
- `--egress` — give the VM outbound network; omitted by default. `microvms-cli/src/cli.rs:401-402`.
- `--launch-env <KEY=VALUE>` — set one launch-environment variable for every exec in the VM; repeatable. Delivered in the same `runHookPayload` as the agent token, at launch, so it never touches the shared image snapshot and never touches disk. The daemon applies it as the *base* environment of every exec, with `exec --env` on the same key winning. Same parser as `exec --env`, so the first `=` splits, an empty VALUE is legal, and a missing `=` or empty KEY is refused at parse time. The whole payload shares a 4096-byte ceiling with the token, checked locally before the launch: an over-budget env fails with the byte count and the env's share of it, rather than as an AWS `ValidationException` after the call. One bearer token fits with room to spare; a set of AWS session credentials does not, so large material belongs on `microvm cp` after bootstrap or on a role the workload assumes. `microvms-cli/src/cli.rs`, wiring in `microvms-cli/src/commands/lifecycle.rs`.
- `--keep` — leave the VM and image running; both keep billing until you tear them down. `microvms-cli/src/cli.rs:405-406`.
- `--timeout <TIMEOUT>` — how long to wait for the exec, in seconds; default `300`. `microvms-cli/src/cli.rs:409-410`.
- `--max-idle-sec <MAX_IDLE_SEC>` — suspend the VM after this much inbound-traffic idleness; default `600`. `microvms-cli/src/cli.rs:413-414`.
- `--suspended-sec <SUSPENDED_SEC>` — terminate the VM after this long suspended; a resume attempted after this window fails because the VM no longer exists. Default `600`. `microvms-cli/src/cli.rs:417-418`.
- `--max-duration-sec <MAX_DURATION_SEC>` — hard ceiling on the VM's life; refused above 28800 before any call. Default `3600`. `microvms-cli/src/cli.rs:421-422`.
- `--port <PORT>` — the daemon's port inside the guest. `microvms-cli/src/cli.rs:425-426`.
- `--state-dir <STATE_DIR>` — where the run ledger is written; defaults to `$MICROVM_STATE_DIR` or `~/.microvm/runs`. `microvms-cli/src/cli.rs:429-430`.
- `--config <PATH>` — read this project config file instead of `./microvm.toml`. Naming a file that does not exist is refused with `ERR_CONFIG`: a typed path that is wrong must not silently become "no config". Conflicts with `--no-config`. `microvms-cli/src/cli.rs`, `ConfigFlags`.
- `--no-config` — ignore any `microvm.toml`, even a malformed one; flags and built-in defaults apply. `microvms-cli/src/cli.rs`, `ConfigFlags`.
- Plus `RegionFlags` and `InfraFlags`. `microvms-cli/src/cli.rs:432-436`.

Precedence, when a config file is in play: a typed flag beats the file, and the file beats the built-in default. "Typed" is read off the parse (`clap`'s `value_source`), not off the value, so `--memory 2048` overrides a file that says `4096` even though 2048 is also the default. The merge happens in exactly one place (`merge_config` in `microvms-cli/src/commands/lifecycle.rs`, per-knob precedence in `config::pick`) and its outcome is reported in the success envelope's `resolvedConfig` key — each knob's winning value and the source it came from (`flag`, `config`, `env`, or `default`; `env` appears only on the region, the one knob whose chain continues past the file into `$AWS_REGION`/`$AWS_DEFAULT_REGION`) — so a caller never has to re-derive which source won. One deliberate pairing rule: a typed `BINARY` positional with no typed `--image` suppresses the file's `image`, because `run` builds exactly when the merged image is absent, and a file that silently won that pair would run the caller's tests against a stale pinned image. A positional that names a directory does not suppress it — sync mode launches, so the file's pinned image is exactly what `run .` wants. See "Project config" below for the file itself.

Sync mode (issue #72): a positional that names a directory switches `run` into a pack-run-collect round trip — `microvm run . --image ci-image --exec "make test"` is the headline spelling. The tree is packed locally (deterministic member order; `.git`, `target`, `node_modules`, and `.venv` skipped whole; sockets, fifos, and devices skipped individually; symlinks preserved as links, never followed), uploaded to `/workspace` in the guest, and the exec runs with `/workspace` as its working directory. The pack is budgeted against the daemon's own caps — 512 MiB of file bytes, 100 000 members — during the walk, so an over-budget tree is `ERR_SYNC` naming the offending subtree before any archive bytes are allocated or any AWS call is made.

Afterwards — including when the exec exited non-zero, because a failing run's report is the artifact CI most wants — the workdir comes back and the members matching the config file's `artifacts` globs are written into the directory. Only glob-matched regular-file members land, and never under `.git`: symlinks, hardlinks, specials, unmatched members, anything attempting traversal outside the directory, and any `.git` path are skipped, because the returned archive is the VM's word, the VM is where untrusted work runs, and a workload-written `.git/hooks/pre-commit` would execute on the host at the caller's next commit. With no `artifacts` globs configured the workdir is not downloaded at all, and the `sync` key says so in a `note`. A download failure does not fail the run — by then the exec's result is already in hand, and discarding a green test run because `make clean` removed the workdir would be the report lying — the error lands in `sync.error` and the exec's own exit code stands.

Sync mode launches an existing image (`--image`, or `image` pinned in microvm.toml); a directory with nothing supplying an image is refused with `ERR_PRECONDITION` before any call. A local pack or extraction failure is `ERR_SYNC` (exit 16). The envelope's `sync` key reports the workdir, uploaded bytes and member count, and each artifact brought back with its size — or `error` / `note` for the two no-artifact shapes. `microvms-cli/src/sync.rs`; wiring in `microvms-cli/src/commands/lifecycle.rs`.

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
- `--base-image-version <VERSION>` — pin the managed base image to one version instead of taking the service's default. Without this a build floats: the managed base's version list is not static — `al2023-1` carried one version in June and two by July — so two builds of identical inputs weeks apart can sit on different bases and neither recorded which. The build succeeds either way; the difference shows up in the guest. The legal values come from `ListManagedMicrovmImageVersions`, which `microvm doctor` prints as its `base-image-versions` check, and they are bare integers for a managed base (`0`, `1`) where a custom image's versions are `1.0`. A bogus pin is refused by the service before anything is created (HTTP 400 `No managed MicroVM Image with arn <base-arn> and version 999 is available`), but it costs the artifact upload first, so the `Version` shape's own constraints — non-empty, at most 2048 characters, no whitespace anywhere — are checked locally before the upload. Note that the value comes back **normalised**: a build pinned with `1` reads back `baseImageVersion: "1.0"` from `GetMicrovmImageVersion`, so the echoed value cannot be fed back into a request. `microvms-cli/src/commands/lifecycle.rs`'s `BuildSpec`, guard in `microvms-core/src/control/image.rs`'s `create_image`.
- `--log-group <GROUP>` — CloudWatch log group for the build's logs, instead of the service-created `/aws/lambda-microvms/<image-name>`. Letters, digits, and `_ - / . #` only, up to 512 characters, validated locally before the artifact upload. The build role must be able to write to whatever this names (`logs:CreateLogGroup`/`CreateLogStream`/`PutLogEvents`); a group outside a granted prefix builds with **no logs at all**, the same silent outcome as the wrong-prefix policy in `docs/PLATFORM.md`.
- `--log-stream <STREAM>` — log stream name **prefix** inside `--log-group`; requires it. The platform's `logging.logStream` member is an exact stream name — prefixes are unsupported — and one image build is three VMs writing three streams (docker build, Graviton 3 snapshot, Graviton 4 snapshot), so a fixed configured name would collapse every build's logs into one indistinguishable stream. The client therefore appends `/<16 hex>` of fresh randomness per build attempt, and the envelope reports the resolved exact name as `logStream` — the only place it exists. No `:` or `*` (the shape's pattern is `[^:*]*`); up to 495 characters (the platform's 512 minus the suffix's 17). See `docs/PLATFORM.md`, "An image build is three VMs and three log streams".
- `--repair-identity` — widen the guest so `sethostname` and the `boot_id` bind mount work. `microvms-cli/src/cli.rs:462-463`.
- `--reuse` — reuse an existing image whose build inputs match, instead of building. Computes a sha256 over the build inputs (the daemon binary's bytes and the Dockerfile), derives the image name `<name>-<hash12>` — where the prefix is `--name` or the stable stem `microvm-cli` — and checks the listing for that exact name. A hit skips the build entirely and reports the existing image with `reused: true` in the envelope; a miss builds under the derived name, so the next invocation with the same inputs hits. The hash is in the name because recreating an image under a previously-used fixed name can serve a stale snapshot (measured; the same hazard class as the clientToken replay in `docs/PLATFORM.md`) — content-keying gives both properties at once: unchanged inputs reuse their image, changed inputs get a fresh name and a fresh build. `--memory` is not part of the identity, so a reused image keeps the size class it was created with; the envelope's `size` is the requested class and the text says so. `microvms-cli/src/commands/lifecycle.rs:501`, the hash at `microvms-core/src/control/artifact.rs:91`.
- `--port <PORT>` — the daemon's port inside the guest. `microvms-cli/src/cli.rs:466-467`.
- Plus `RegionFlags` and `InfraFlags`. `microvms-cli/src/cli.rs:469-473`.

The success envelope always carries `reused` (`false` for a plain build) and `logStream` (`null` when no `--log-stream` was configured; a reused image also reports `null`, because no build ran and no stream was resolved), so a consumer never guards for either key. `microvms-cli/src/commands/mod.rs`.

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

It also reports `busy` and `execs`, which is what makes this the command an orchestrator loops on to hold a long-running VM alive. The platform measures idleness by inbound traffic through the endpoint proxy, and that proxy terminates outside the guest, so a request sent from *inside* the VM cannot reset the idle timer — only a poll from outside counts, and this poll is that traffic. `busy` is what makes the loop informed rather than unconditional: it is true only while some exec is actually running, so an exec that exited and is waiting to be acked reads false. `execs` counts every registered entry in any phase, so `busy: false` with a non-zero count is a VM holding unacked output somebody still has to collect before terminating it. See `docs/PROTOCOL.md`, "Idle policy, and why liveness is a field rather than a route".

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

- `<MICROVM_ID>` — the MicroVM to freeze: a MicroVM id, or a name `run --keep --vm-name` registered (resolved locally, zero extra calls; an unknown name fails with `ERR_PRECONDITION` before any call). Required.
- `--timeout <TIMEOUT>` — how long to wait for the state transition, in seconds; default `300`. `microvms-cli/src/cli.rs:681-682`.
- Plus `RegionFlags`. `microvms-cli/src/cli.rs:684-685`.

## resume

```
microvm resume [OPTIONS] <MICROVM_ID>
```

Thaws a suspended MicroVM and reports its endpoint. Past the launch-time `suspendedDurationSeconds` window the VM has been terminated, so the resume fails.

`microvms-cli/src/commands/lifecycle.rs:795`

Flags:

- `<MICROVM_ID>` — the MicroVM to thaw: a MicroVM id, or a registered name (resolved locally). Required.
- `--timeout <TIMEOUT>` — how long to wait for RUNNING, in seconds; default `300`. `microvms-cli/src/cli.rs:695-696`.
- Plus `RegionFlags`. `microvms-cli/src/cli.rs:698-699`.

## terminate

```
microvm terminate [OPTIONS] <MICROVM_ID>
```

Tears down a MicroVM and optionally its image and build log group. When part of the teardown fails, the command still exits successfully and reports the leaked identifier.

`microvms-cli/src/commands/lifecycle.rs:840`

Flags:

- `<MICROVM_ID>` — the MicroVM to terminate: a MicroVM id, or a registered name (resolved locally). An accepted terminate releases the VM's registered name whichever spelling addressed it, so the name is reusable. Required.
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

## history

```
microvm history [OPTIONS] <VM_ID>
```

Prints what was asked of one MicroVM and what the platform reported back: `imageBuilt` (when `run`'s own invocation built the image — a standalone `build` has no VM id and writes no history), `launched`, `exec` per exec, `suspended`, `resumed`, and `terminated` with the teardown's own verdict. The record is an append-only JSONL file at `<state-dir>/history/<vm-id>.jsonl`, one camelCase object per line with a per-VM `seq` counter and an epoch-seconds `at`, appended by `run`, `exec`, `suspend`, `resume`, and `terminate` with the same swallowed-failure discipline as the ledger — an unwritable state directory never fails a teardown.

Unlike the ledger `ls` reads, nothing ever deletes a history file: the record survives terminate on purpose, because a caller attesting over a run needs it precisely after the VM is gone. A VM with no history file is a clean empty result rather than an error — asking about a VM this state dir never saw is a question, not a mistake. A truncated last line (a process killed mid-append) is reported as an unreadable record rather than skipped.

What the record does not prove: it shows what was asked of the VM and what the daemon and the control plane reported back — identifiers, endpoints, exit codes, teardown verdicts — not what a process inside the guest did between execs. Every value is the platform's; nothing the guest printed ever reaches a history line, because a record built from guest output would be a record the workload can forge.

`microvms-cli/src/commands/local.rs`, storage in `microvms-cli/src/history.rs`.

Flags:

- `<VM_ID>` — the MicroVM whose history to print: a MicroVM id, or a registered name (resolved locally; an unregistered name reads as a clean empty history, this command's usual answer for an unseen VM). Required.
- `--state-dir <STATE_DIR>` — where the histories live; defaults to `$MICROVM_STATE_DIR` or `~/.microvm/runs`.

## logs

```
microvm logs [OPTIONS] <IMAGE_NAME>
```

Names an image's build log group, `/aws/lambda-microvms/<image-name>`. That log group holds the only evidence a failed build leaves behind.

The failure envelope's `data` also carries `streams`: three objects labelling the build's log topology by role — `docker-build` (zip pull and docker image build), `snapshot-graviton3`, and `snapshot-graviton4` (the snapshot VMs are the ones that start the app, so application startup logs land there). One build is three VMs and three streams, with random service-chosen stream names by default; a configured `logStream` collapses all three into one exact stream, distinguished across builds only by the per-build `/<16 hex>` suffix this client appends, and the resolved name is on the build envelope's `logStream`. The point of returning the topology here is that an agent handed this envelope knows what to look for inside the group without measuring the platform itself. See `docs/PLATFORM.md`, "An image build is three VMs and three log streams".

`microvms-cli/src/commands/local.rs:228`

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

Checks every prerequisite and says which one is wrong. The checks cover the project config file, credentials, the region, the three Terraform outputs, whether the stack is applied, the managed base images AWS publishes, and whether the daemon binary is aarch64.

The config check runs first because it is the one check that is entirely local, and a malformed file fails `run` before anything else would. `doctor --config ci.toml` validates the exact file `run --config ci.toml` would read, through the same loader, so the two commands cannot disagree about which config applies. A broken file — unparseable, an unknown key, a value outside its flag's domain — is a fatal fail naming the reason; an absent `./microvm.toml` is an advisory pass, because a project configured by flags is not a finding. A valid file's pass line names which knobs it pins. `microvms-cli/src/commands/doctor.rs`'s `check_config`.

`microvms-cli/src/commands/doctor.rs:32`

Two of the checks are about the managed base rather than about this machine, and both are advisory.

`managed-bases` reads `ListManagedMicrovmImages` and answers whether AWS publishes a base this client does not know about. It reports rather than offers, and the reason is a limitation in the shape: `ManagedMicrovmImageSummary` carries an ARN and two timestamps and nothing else — no registry reference, no architecture, no working directory. A `BaseImage` in `microvms-core` pairs the ARN *with* the Dockerfile `FROM` and with whether the image declares a `WORKDIR`, because `require_matching_from` compares a caller's Dockerfile against the first and `require_workdir` refuses inheritance on the second. Neither guard has an input from this listing, and the registry ref is not derivable from the ARN — `al2023-1` pairs with `public.ecr.aws/amazonlinux/amazonlinux:2023-minimal` and nothing in the ARN says so. So a discovered base cannot be built from through this client, and the check says which one it knows.

`base-image-versions` reads `ListManagedMicrovmImageVersions` and prints the values `build --base-image-version` may take. That is the actionable half: an unpinned build takes whatever the service currently defaults to, and that default has already moved once.

Both are advisory because nothing about a build depends on either read succeeding, and a `doctor` that failed on a listing would add a way to be wrong rather than report one. `microvms-cli/src/commands/doctor.rs`'s `check_managed_bases`.

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

Emits every service constraint this client believes, for the drift gate that `scripts/check-model-drift.py` runs against the pinned botocore model.

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

## Project config: microvm.toml

`run` and `doctor` read an optional `./microvm.toml` beside the invocation (or the file `--config <PATH>` names). Every key in it already exists as a `run` flag; the file adds no capability, only persistence — `microvm run` in a configured project needs zero flags. `microvms-cli/src/config.rs`.

```toml
image = "coding-agents"       # run --image
binary = "target/agentd"     # run [BINARY]
exec = "make test"           # run --exec
memory = 4096                # run --memory; same closed set as the flag
region = "us-west-2"         # any region string, resolved where the flag's is
egress = true                # run --egress
max-idle-sec = 600           # run --max-idle-sec
suspended-sec = 600          # run --suspended-sec
max-duration-sec = 3600      # run --max-duration-sec
artifacts = ["dist/**"]      # run <DIR> artifact globs (issue #72); validated here
log-group = "/aws/lambda-microvms/ci-builds"  # run --log-group; the build role must be able to write here
log-stream = "ci-image"      # run --log-stream; a PREFIX — the client appends /<16 hex> per build (issue #98)
[env]                        # run --launch-env, as a table; per-key merge, flag wins its key
CI = "1"
```

Key names are the flag names with `-` for `_`, so the file reads like the command line it replaces. All keys are optional; an absent key means the flag or the built-in default decides. A relative `binary` path resolves against the config file's own directory, not the process cwd — `--config /repo/microvm.toml` exists precisely for the invoke-from-elsewhere case, and `target/agentd` must mean the same binary from anywhere. An unknown key is refused by name rather than silently ignored — `memroy = 4096` launching a 2 GB VM is the failure that closes. A value outside its flag's domain (`memory = 1500`, an artifact glob that will not compile) is refused with the same closed-set reasoning the parser enforces, so the file cannot be a side door past the flag domains.

On Windows, two `binary` shapes are refused at load because they mean two things at once (issue #87): a rooted path with no drive (`/opt/agentd` — Windows parses it as relative, so the file-directory join would silently re-anchor it onto the config file's drive) and a drive with no root (`C:agentd` — resolves against that drive's current directory at spawn time). The remedy differs per intent — write the drive letter, or write a genuinely relative path — so the loader refuses rather than guesses. On Unix neither shape exists and `/opt/agentd` is simply absolute.

`--timeout` is deliberately not a config key: it is a client-side wait, not a property of the VM the project wants to pin.

A file that cannot be used is `ERR_CONFIG` (exit 15), refused locally before any billable call. The remedy differs from `ERR_INVALID_ARG`'s — edit (or `--no-config` bypass) a file the invocation may never have named, rather than edit the command line — which is why it has its own row.

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
| `history` | `microvm.history` |
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

The table has seventeen rows, 0 through 16, and is append-only because consumers branch on the values. The rows are split by what the caller should do next; a distinction that does not change the caller's next action does not get its own integer. `microvms-cli/src/exit.rs:171-256`, with the enum's explicit discriminants at `microvms-cli/src/exit.rs:76-100`.

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
| 14 | `ERR_NAME_TAKEN` | the VM name is registered to a live VM; refused locally, before any AWS call | |
| 15 | `ERR_CONFIG` | the project config file is missing, malformed, or out of domain; refused locally — fix the file, or pass `--no-config` | |
| 16 | `ERR_SYNC` | `run <DIR>` could not pack the directory or write an artifact back; the failure is on this machine's filesystem, not the platform's | |

Row 0 is the only one with no `ERR_*` string, because a success envelope has no `code` field to put one in. `microvms-cli/src/exit.rs:50-53`.

`ERR_NAME_TAKEN`, `ERR_CONFIG`, and `ERR_SYNC` are the rows with no core `ErrorKind` behind them: the name registry, the config file, and `run <DIR>`'s pack/extract are the CLI's own filesystem work, so these refusals can only arise locally. Each is distinct from `ERR_INVALID_ARG` because the next actions differ — an invalid argument is fixed by editing the flag, a taken name by terminating the VM that holds it or picking another name, a broken config by editing (or `--no-config` bypassing) a file the invocation may never have named, and a sync failure by fixing the local file or disk the pack or extraction named. `ERR_SYNC` is also distinct from `ERR_EXEC_FAILED`: a CI caller needs to tell "the sync plumbing failed here" from "your tests failed" from "the platform failed".

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
