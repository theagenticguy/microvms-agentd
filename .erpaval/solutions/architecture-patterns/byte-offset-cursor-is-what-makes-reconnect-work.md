---
title: A byte-offset cursor is what separates a working stream reconnect from a broken one
category: architecture-patterns
tags: [sse, streaming, reconnect, api-design, exec, agent-sandbox]
session: session-7ef43d
date: 2026-08-05
---

## Lesson

When streaming the output of a long-running remote command, the stream must be a
*view* onto a server-side object rather than the object itself, and the client
must resume by byte offset. Three design consequences follow, and every mature
sandbox platform has converged on them:

Output lives in a server-side buffer keyed by a caller-minted id, independent of
any connection. A dropped connection cannot kill the command, and reattaching is a
plain GET with `?offset=N`.

The transport must be framed, so a terminal typed event can say "the command
exited with status N". A raw chunked byte stream cannot distinguish a finished
command from a dropped connection — the byte sequences are identical. That is the
argument for SSE over raw streaming, and it is worth the base64 cost of putting
arbitrary bytes in a UTF-8 field.

stdin is a **separate** request, never multiplexed onto the output connection.
Runloop, Daytona, E2B, and Modal all do it this way, and it is what makes a
dropped attach harmless rather than a lost input channel.

## Why the offset specifically

E2B's equivalent reattach (`connect(pid)`) has no offset, and their issue #1352 is
exactly that: output produced during the gap is lost, and the reconnect stalls.
Runloop's `?offset=` cursor plus a client-side auto-reconnect wrapper is the
working shape. Copy that one.

Two implementation details that are easy to get wrong:

Subscribe to the live channel **before** snapshotting the buffered backlog, then
chain backlog then live. Reversing it loses any write landing between the two
operations. Make the ordering structural (subscribe under the same lock the
publisher holds) rather than sequential, so the unsafe order is not expressible
from the call site.

Use a bounded broadcast channel *because* lag is recoverable. A subscriber that
falls behind gets told it lagged, surfaces an explicit gap to the client, and the
client re-reads from its last good offset. Silently advancing the cursor past
dropped data is the failure this design exists to prevent, so a "gap" must be a
visible typed event and not a log line.

## Also worth knowing

Keep-alive comments during silence are not optional. An agent harness can think
for minutes, and an idle connection through a proxy is indistinguishable from a
dead one.

Two views onto one exec must both work: a poll that returns the whole buffer and a
stream that follows it. Detaching from either must not disturb the other, and the
polled result's truncation semantics have to keep working unchanged for existing
callers. That argues for a head-capped buffer for polling plus a tail ring for
streaming, rather than one buffer serving both.
