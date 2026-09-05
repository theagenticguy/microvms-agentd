---
title: Drive it from a script or an agent
description: The one-envelope rule, the stable codes and exit integers a script branches on, the manifest that describes the surface, the streaming exception, and the token-lean output for a consumer paying per token.
editUrl: false
sidebar:
  order: 10
---

```bash
microvm manifest                          # the whole surface, as JSON, with no credentials
microvm --json run --exec "make test"     # one envelope on stdout, progress on stderr
microvm --dense ls                        # tab-separated, token-lean
```

Every command is built to be driven by something that is not a person at a terminal. At the end of this page you will parse every command the same way, branch on stable codes, discover the surface without parsing help text, and know the one place stdout carries more than one object.

## 1. One envelope on stdout

`--json` is a global flag, so `microvm --json ls` and `microvm ls --json` are the same invocation. It emits the typed JSON envelope on stdout instead of human output and wins over every other format, including an interactive terminal. Progress always goes to stderr, and `--quiet` suppresses progress but never a warning, so a leaked resource is still reported in quiet mode.

A success envelope carries `status`, `apiVersion`, `type`, and `data`. `type` is the discriminant to branch on first, and each command's `data` keys are published in the manifest as `responseKeys`:

```json
{
  "status": "ok",
  "apiVersion": "1",
  "type": "microvm.run",
  "data": {}
}
```

A failure envelope carries `status`, `apiVersion`, `error`, `code`, `exitCode`, `finding`, `suggestions`, and `data`. Every field is always present, so a consumer never guards against a missing key:

```json
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

Branch on `code`, never on `error`. `data` carries partial results on the failure path, most importantly `leaked`, the identifiers a teardown could not delete. `data.kind` carries the daemon's own status name when the exit code is coarser than the failure: several wire kinds collapse onto `ERR_PROTOCOL`, because a shell branching on `$?` cannot act differently on a 400 than on a 409, and a consumer that can reads `data.kind`. A request rejected locally reports no `data.kind`, because nothing reached the daemon. `apiVersion` is bumped when a field's meaning changes, never when a command is added. [Envelope](/reference/envelope/) and [Response types](/reference/response-types/) are the generated references.

## 2. The exit codes

`exitCode` in the envelope matches `$?`, and the table is append-only because consumers branch on it. Rows are split by what the caller should do next. `ERR_RETRYABLE` (3) means run the identical command again; `ERR_CREDENTIALS` (4) means fix an identity, and waiting will not help. `ERR_TIMEOUT` (10) means a client-side deadline elapsed and the VM and the exec are untouched, so a poll is safe. `ERR_EXEC_FAILED` (13) means the sandbox worked and your command exited non-zero, which is the one non-zero exit that says nothing is wrong with the platform, the credentials, or the CLI; a CI caller needs to tell "your tests failed" from "we never got a VM". `ERR_PRECONDITION` (12) means run `microvm doctor`. `ERR_NAME_TAKEN` (14), `ERR_CONFIG` (15), and `ERR_SYNC` (16) are refused locally with no AWS call made. `ERR_UNEXPECTED` (1) is a bug in this CLI and never the platform. clap's own usage errors exit 2, which is `ERR_INVALID_ARG`, so a caller reading `$?` sees the same number either way. [Exit codes](/reference/exit-codes/) has every row with its `finding`.

## 3. Discover the surface

`microvm manifest` emits the whole command surface, its exit codes, and its envelope schema, generated from the CLI's own argument tree rather than written down. It is always JSON, and it needs no credentials, no region, and no network, so it doubles as a liveness check. Each command's entry carries its parameters with `type`, `default`, `choices`, `required`, and `positional`, its `responseType` and `responseKeys`, and for `exec` an `alternateResponse` naming when it applies. The `conventions` list is the contract in prose. `microvm manifest --dense` prints one line per command with its parameters.

`microvm constants --emit-json` emits every service constraint this client believes, unwrapped by an envelope, for the drift gate that compares them against the pinned service model.

## 4. The one exception: `exec --stream`

`exec --stream` is the one invocation that writes more than one object to stdout. Under `--json` it emits NDJSON, one event object per line, and then the envelope as the final line:

```json
{"event":"output","stream":"stdout","offset":0,"bytes":12,"text":"hello world\n","lossy":false}
{"event":"exit","exitCode":0,"signal":null,"truncated":false,"writersMayBeAlive":false,"offset":12}
{"status":"ok","apiVersion":"1","type":"microvm.exec.stream","data":{"execId":"x-1","events":2,"bytes":12,"nextOffset":12,"gaps":0,"exitCode":0,"truncated":false}}
```

Three things keep the two contracts distinguishable. The discriminant differs: a streamed exec's final envelope has `type` `microvm.exec.stream`, so a consumer learns which parse applies from the field it already reads first. The manifest publishes it, as `exec`'s `alternateResponse` with `when: "--stream"`. And the envelope is written compact once a stream has started, because "the last line is the envelope" is only true if the envelope is one line.

Event kinds are `output` (with `stream`, `offset`, `bytes`, `text`, `lossy`), `gap` (with `from` and `to`, the only report of lost bytes), and `exit`. The envelope's keys summarize the stream rather than repeating it: `events` and `bytes` let a caller assert it read everything, and `nextOffset` is where `--from-offset` would resume. A stream cut before its exit event reports `exitCode: null` rather than `0`, because zero would turn a truncated stream into a passing build, and the command exits `ERR_EXEC_FAILED`. A stream that fails part-way through gets the failure envelope as its compact last line. The stream chunks are the command's output, so they cannot go to stderr; `microvm exec --stream build.sh > log` has to write the log.

## 5. Token-lean output

`--dense` is the other global flag: token-lean output, for a consumer paying per token. It renders tab-separated text, one field per column; a dense failure is the code, then the message, tab-separated, so field one is always the code. `--json` wins over `--dense`, and `--dense --json` together emit the compact one-line JSON document rather than the pretty one. Neither depends on whether stdout is a terminal; without either, a terminal gets a human rendering and a pipe gets plain text.

## 6. Retries and idempotency

Pass your own `--exec-id` on an `exec` you may have to retry: a start carrying a known id returns the original exec without spawning a second child, so a caller whose process died between sending the start and reading the answer sends the identical start again. `ERR_RETRYABLE` means exactly that, run the identical command again. `build --reuse` makes a repeated build idempotent through a content-hash image name, and `run` defaults to a per-invocation image name because reusing one is how a `clientToken` replay wedges an image. A `--vm-name` is local state, refused with `ERR_NAME_TAKEN` before any call if a live VM holds it.

## 7. Extracting a field without jq

The shipped scripts read envelope fields with a python3 one-liner, which every machine that runs the examples already has:

```bash
jqr() { python3 -c "import json,sys; print(json.load(sys.stdin)['data']$1)"; }
LAUNCH=$(microvm run --json --keep --image "$IMAGE_ARN")
EP=$(echo "$LAUNCH"  | jqr "['endpoint']")
TOK=$(echo "$LAUNCH" | jqr "['agentToken']")
ID=$(echo "$LAUNCH"  | jqr "['microvmId']")
```

:::agent

**For an agent.** Prefer `microvm manifest` to any page here wherever the two could disagree; it is generated from the source of truth. Read `type` before `data`, `code` before `error`, and `data.leaked` on every failure. Treat `ERR_EXEC_FAILED` as the workload's result and every other non-zero exit as a problem with the run itself. Use `--exec-id` on anything you might retry, and `--dense` when you are paying per token and do not need the envelope's structure.

:::

[For agents](/agents/) names which surface answers which question, and the daemon's own machine surface is `GET /v1/schema`, committed as the [wire schema](/reference/wire-schema/).
