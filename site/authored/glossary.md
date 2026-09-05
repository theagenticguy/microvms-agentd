---
title: Glossary
description: The project's vocabulary, one entry each, in the narrow sense this project uses, with the page that develops the term.
---

## 1. How to read this page

The terms are alphabetical, and each entry names the page that develops it. Several of them mean
something looser in the wider literature, so each definition states the narrow sense this project
uses and, where a neighboring term is easy to confuse with it, says which one it is.

## 2. Terms

### Ack

`POST /v1/exec/{id}/ack`, or `microvm ack`: the call that releases a finished exec's buffered output
and starts its collection clock. Output lives until the ack, so nothing a slow reader has not seen is
destroyed; a second ack answers 409, because the first released it and a 200 with an empty body would
read as "the command produced no output". See [Protocol](/internals/protocol/).

### Agent token

The bearer the daemon's control API requires. It arrives inside the `runHookPayload`, is installed by
the one-shot bootstrap, and every `/v1/` route except `/v1/health` and `/v1/schema` takes it as
`Authorization: Bearer`. Comparison is constant-time over bytes, and the token never enters an exec'd
child's environment. It is not the proxy token, which the platform's endpoint wants on the same
request. See [Trust](/internals/trust/).

### Artifacts globs

The `artifacts` patterns in `microvm.toml`. After a sync-mode run, members of the guest's `/workspace`
matching them come back into the local directory, including when the command failed, because a failing
run's report is the artifact CI most wants. See [Run a project through a VM](/learn/tutorial/run-a-project/).

### Baseline and peak

The two memory figures behind `minimumMemoryInMiB`. The request selects a size class whose baseline
is billed while running and whose peak, four times the baseline, is provisioned from the start; the
guest's `/proc/meminfo` reports the peak. Nothing changes size during a run. See
[Platform](/internals/platform/).

### Bootstrap, one-shot

The rule that governs the run hook. The first `/run` carrying a token installs it and answers 200. A
later `/run` carrying the identical token also answers 200, because the platform may retry its own
hook. A later `/run` carrying a different token answers 409 and changes nothing. Until bootstrap
lands, every control route answers 503. See [Trust](/internals/trust/).

### Build role and execution role

Two of the three AWS prerequisites an image build needs, beside an S3 bucket for the code artifact.
The repository's Terraform stack creates exactly those three; the CLI reads them from
`MICROVM_BUCKET`, `MICROVM_BUILD_ROLE_ARN`, and `MICROVM_EXECUTION_ROLE_ARN`, and `microvm doctor`
names whichever is missing. See [Install](/learn/tutorial/install/).

### Byte cursor

The offset an exec stream resumes from. `GET /v1/exec/{id}/stream?offset=N` yields exactly the bytes
after N, so a reconnecting client receives what it has not seen; a reattach past the retained window
gets an explicit `gap` event naming the missing range rather than a silent skip. Measured across a real
suspend and resume, the held handle resumed contiguously. See [Protocol](/internals/protocol/).

### `clientToken`

The idempotency key on the service's create calls, and a permanent one. A token derived from a stable
resource identity replays forever: after an image is deleted and recreated under the same name, the
service replays the original create as a no-op and the image sits in `CREATING` with its builds never
scheduled. The client scopes a create token to a single build attempt. See
[Platform](/internals/platform/).

### Conformance suite

The live tier of verification, run against real MicroVMs by `mise run live`. It exercises the whole
surface through the `microvm` CLI, costs money, and takes roughly a quarter of an hour, which is why
`mise run check` is the offline definition of done and says nothing about it. See
[CLI coverage plan](/internals/cli-coverage-plan/) and [Strategy](/internals/strategy/).

### Control API

The daemon's own routes under `/v1/`: exec, files, health, schema. They are distinct from the platform's
lifecycle hooks, whose paths the platform fixes and which stay unversioned. Control routes answer 503
before bootstrap, never 404 and never a dropped connection. See [Protocol](/internals/protocol/).

### Control plane and session plane

The two halves of the client. The control plane, in `microvms-core`, wraps the service API: build,
launch, suspend, resume, terminate. The session plane talks to `agentd` through the VM's authenticated
endpoint: exec, streaming, files, port forwarding. The CLI and the Python and Node bindings are thin
shells over both. See [System overview](/internals/architecture/system-overview/).

### Deployment invariant

`ENTRYPOINT []` and `CMD ["/agentd"]`, with no init system and no other process started first. It is
what makes "no in-VM workload runs before bootstrap completes" true, what makes an omitted `cwd`
inherit the image `WORKDIR`, and what makes identity repair sound. The daemon cannot enforce it;
whoever builds the image does. See [Trust](/internals/trust/).

### Detached exec

An exec whose record lives in the daemon, keyed by a caller-minted `exec_id`, independent of any
client connection. Start, poll, and ack are separate calls; a retried start returns the original exec
without spawning a second child; a detached exec outlives the proxy token that started it, so start,
rotate, poll, ack is a normal sequence. `microvm exec --detach` starts one and `--poll <id>` reads it
back. See [Embedding](/internals/embedding/).

### Drift gate

The offline check that compares the service constraints hardcoded in the client against the pinned
botocore model, so a limit the service changes cannot stay wrong silently. `microvm constants` emits
every constraint the client believes, for that gate. See [constants](/reference/commands/constants/).

### Egress

Outbound network for a VM, granted at launch through an `INTERNET_EGRESS` network connector and
requested from the CLI with `--egress`. Omitting it is how you get a VM with no outbound network. It is
a launch-time property of the VM, not something the daemon can enforce or relax. See
[Platform](/internals/platform/).

### Envelope

The one JSON document every `microvm` command writes to stdout under `--json`, with progress on
stderr. Success carries `type` and `data`; failure carries a stable `code`, a mapped `exitCode`, and
`suggestions`. The one exception is `exec --stream`, which emits NDJSON events and the envelope last.
See [The envelope](/reference/envelope/).

### Exit-code catalog

The mapping from a failure's stable `code` to the process exit status a caller branches `$?` on. It is
part of the manifest and it is the contract; the prose beside each code is rewritten freely, so a
matcher over the message breaks on a wording change that broke nothing. `ERR_EXEC_FAILED` means the
sandbox worked and the command inside it exited non-zero. See [Exit codes](/reference/exit-codes/).

### Hooks: ready, validate, run

The platform's lifecycle endpoints, served by the daemon under a prefix the platform fixes. `ready` and
`validate` are build-time hooks: the build calls them in the snapshot VM to decide whether the snapshot
it just produced is usable, before any instance exists. `run` fires at launch and delivers the
`runHookPayload`. All three must answer on `AGENTD_PORT`, which must be 9000. See
[Platform](/internals/platform/).

### Identity repair

What the daemon does at startup to the files that are supposed to be unique per machine. One image is
snapshotted once and restored many times, so every VM shares the snapshot's machine-id, hostname,
boot_id, and random seed; the daemon replaces them unless `AGENTD_REPAIR_IDENTITY` opts out, and
`microvm health` reports what it did. See [Trust](/internals/trust/).

### `idlePolicy`

The launch-time policy under which the platform suspends an idle VM and then terminates a suspended
one after its window. Idleness is measured by inbound traffic through the endpoint proxy, so a
computing workload with no inbound requests reads as idle, and a request from inside the guest cannot
reset the timer. The keepalive is yours and it runs outside the VM. See [Platform](/internals/platform/).

### Image and image version

A MicroVM image is built from a Dockerfile and carries a snapshot; a VM launches from an image
version. `minimumMemoryInMiB` lives on the version, so the only way to learn a running VM's memory from
the API is to fetch its version. The snapshot carries a one-week minimum retention, so reuse an image
with `--image` rather than rebuilding. See [Platform](/internals/platform/).

### `microvm manifest`

The command that prints the binary's whole contract as JSON: every command, its flags, its response
type, the envelope schema, and the exit-code catalog. It needs no credentials, no region, and no
network. The Reference tier of this site is generated from it, and it outranks every page here. See
[Reference](/reference/).

### `microvm.toml`

The CLI's configuration file. A typed flag beats the file, and the file beats the built-in default. It
is where `artifacts` globs live and where an `image` can be pinned for sync mode. See
[CLI](/reference/cli/).

### Name registry

The local record `--vm-name` writes when a VM is kept, and `microvm attach` writes for a VM this
machine did not launch. It lives in the CLI's state directory, carries the endpoint, the agent token,
and the MicroVM id, and resolves a `--name` with zero AWS calls. It is a local fact, so a VM launched
elsewhere is addressed by the explicit triple until it is attached. See [CLI](/reference/cli/).

### Network connector

The launch-time grant that gives a VM a network path, spelled as an ARN of the form
`arn:aws:lambda:<region>:aws:network-connector:aws-network-connector:<NAME>`, never as the bare name.
`ALL_INGRESS` and `INTERNET_EGRESS` are the two the client uses; `SHELL_INGRESS` is what `microvm
shell` requires. See [Platform](/internals/platform/).

### Proxy token

The credential the platform's endpoint proxy wants on every request: an `X-aws-proxy-auth` JWE
scoped to one MicroVM id and one set of ports, beside an `X-aws-proxy-port` header naming the target
port. It is minted by `CreateMicrovmAuthToken`, which returns a map of header names rather than a
string, and it expires within sixty minutes, so the client refreshes it at thirty. It is not the agent
token. See [Embedding](/internals/embedding/).

### `--reuse`

The `build --project` flag that names the image after a content hash of the project's dependency files
and skips the build when an image of that name already exists. The lockfile is the identity: an edit
to `uv.lock` alone moved the hash and produced a new image, while unchanged files answered in under a
second with `reused: true`. See [Platform](/internals/platform/).

### `runHookPayload`

The string passed to `RunMicrovm` and delivered to the daemon's `/run` hook at launch, wrapped one JSON
layer deeper than the caller wrote it. It carries the agent token, an optional launch environment, and
the identity seed for a verified tunnel, and it is capped at 4096 bytes. It is the only per-VM secret
channel: a secret baked into the image is shared by every VM restored from the snapshot. See
[Platform](/internals/platform/).

### Snapshot

The captured state of the build VM after the `ready` and `validate` hooks succeed. Every VM launched
from the image is a restore of that snapshot, so every byte in it is identical across VMs, which is
what identity repair exists for and why no secret belongs in an image. See [Trust](/internals/trust/).

### Sync mode

What `microvm run` becomes when its positional argument is a directory rather than a binary: a
pack-run-collect round trip against an existing image. The tree is packed locally, uploaded to
`/workspace`, the command runs there, and the artifacts come back. `microvm sync` pushes a directory
into a running VM uploading only what changed. See [Run a project through a VM](/learn/tutorial/run-a-project/).

### Tar confinement

How the daemon extracts an archive so that no member can write outside its target. The extraction
root is opened once and every member is created relative to that descriptor with `openat2` under
`RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS | RESOLVE_NO_MAGICLINKS`, so a member whose path would traverse
a symlink is refused with 400. It needs Linux 5.6 or newer, and the guest kernel is 6.1. See
[Protocol](/internals/protocol/).

### Two tiers of document

The standing rule about reliability under Internals. The hand-written documents carry measured
findings and design rationale and win any disagreement; the generated categories carry `path:line`
citations pinned to a commit, which is their value and their expiry date. See [Internals](/internals/).
