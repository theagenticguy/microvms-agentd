---
title: Embed agentd in your own image
description: Append the daemon to an arbitrary task image so your own harness can drive it over the published wire protocol, and the pointers a harness client needs.
editUrl: false
sidebar:
  order: 2
---

```bash
microvm dockerfile --workdir /workspace > Dockerfile
# edit: insert your RUN layers between the chmod line and the ENV lines
microvm build --dockerfile Dockerfile --name my-task-image
```

The platform has no exec API: a MicroVM exposes one HTTPS endpoint and forwards it to whatever the image's `CMD` is listening on. Every harness that runs commands inside a VM therefore ships a daemon in its task image, and before agentd each one wrote its own. At the end of this page you will have an image with agentd as its `CMD` and know which surfaces your client reads. The reasoning behind each rule is in [Embedding](/internals/embedding/).

## 1. The recipe

`microvm dockerfile` prints the stanza that wraps a base image with agentd, emitted by the same generator the default `microvm build` uses, so appending your layers to it is the default build plus your layers. Any task image is the same shape: take the stanza, add the layers your workload needs, keep the daemon lines intact. [Write a guest Dockerfile](/learn/operations/write-a-guest-dockerfile/) walks the stanza line by line.

## 2. The lines that must survive your edits

`ENTRYPOINT []` plus `CMD ["/agentd"]` is the deployment invariant the trust boundary rests on: no task workload runs before the platform's run hook lands, and an omitted `cwd` inherits the image `WORKDIR`. The `FROM` must match the managed base's registry ref, and a `WORKDIR` is required because the managed base declares none. Both are enforced by the client before any AWS call. No secret goes in the image; per-VM credentials travel through `runHookPayload` at launch.

## 3. What your client implements

The full route table and request shapes are in [Protocol](/internals/protocol/), and the same contract is served as JSON Schema at `GET /v1/schema` on any running daemon, unauthenticated, so a client can fetch it before it holds a token. The committed copy is the [wire schema](/reference/wire-schema/). The shape of a client, in one line each:

- **Bootstrap.** The platform delivers your `runHookPayload` to the daemon's `/run` hook, and agentd installs `agent_token` from it once. Until then every control route answers 503, so a client can tell "not yet bootstrapped" from "broken".
- **Auth.** Every `/v1/` route except `/v1/health` and `/v1/schema` takes `Authorization: Bearer <agent_token>`.
- **Exec.** The client mints the `exec_id`, which is what makes a retry safe: a start carrying a known id returns the original exec without spawning a second child. Poll is read-only; ack releases the buffered output; output lives until the ack.
- **Streaming.** Server-sent events from a byte cursor, with an explicit `gap` event when a reattach falls past the retained window and a typed `exit` event that distinguishes a finished command from a cut connection.
- **Files.** One file per request, streamed, or a directory tree as tar with confined extraction.
- **Health.** `GET /v1/health` is unauthenticated and reports version, bootstrap state, disk pressure, and identity repair.
- **The proxy.** Every request crosses the platform's endpoint proxy, which wants two headers, and the token it wants is capped at sixty minutes, so mint inside the request path and refresh well under the ceiling. A detached exec is polled and acked under the next token without loss.
- **The keepalive is yours.** Idleness is measured by inbound traffic through the proxy, which terminates outside the VM, so an outside poll of `/v1/health` resets the timer and a guest-side request cannot.

## 4. Get the daemon binary yourself

The CLI provisions its own version's release asset. For a build you manage, fetch and verify it the same way:

```bash
gh release download --repo theagenticguy/microvms-agentd --pattern agentd
gh attestation verify agentd --repo theagenticguy/microvms-agentd
chmod +x agentd
```

Pass it as the positional argument to `build` or `run`, or as `$MICROVM_AGENTD`. `microvm doctor --binary ./agentd` checks that it is the ARM64 build before a host-architecture mistake costs a build cycle. [Harness capabilities](/internals/harness-capabilities/) maps what the hand-rolled daemons needed onto what agentd covers.
