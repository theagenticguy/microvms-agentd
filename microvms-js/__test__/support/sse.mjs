// SPDX-License-Identifier: Apache-2.0
//
// An offline SSE server, so the exec/stream surface has something real to talk to.
//
// Everything else in this suite is a pure call into the addon, but a stream needs a body to
// parse. This is that body: a `node:http` server on loopback answering
// `GET /v1/exec/<id>/stream` with a scripted list of SSE frames.
//
// # What this covers and what it does not
//
// It covers the parts of the stream contract that are *this client's* behaviour: the frames it
// parses, the events it hands to JS, the offset its reconnect asks for, and where a gap becomes a
// rejection. Those are `microvms-core` properties reached **through the binding** — its spawned
// task, its capacity-1 channel, its `AsyncGenerator` — which is the thing under test here.
//
// It is not `agentd`. The frame shapes below are this suite's transcription of what
// `microvms-core/src/session/sse.rs` parses, so nothing here proves the daemon emits them; if the
// daemon's framing changed, these tests would stay green while the conformance suite went red.
// That boundary is stated rather than papered over.

import http from 'node:http';

/** One SSE `output` frame, base64 as the wire carries it. */
export function outputFrame(offset, data, stream = 'stdout') {
  const encoded = Buffer.from(data).toString('base64');
  return (
    `event: output\n` +
    `data: {"offset":${offset},"stream":"${stream}","output":"${encoded}"}\n\n`
  );
}

/** One SSE `gap` frame: `[start, end)` is gone for good. */
export function gapFrame(start, end) {
  return `event: gap\ndata: {"from":${start},"to":${end}}\n\n`;
}

/** The terminal `exit` frame. Its absence is what makes a stream a cut. */
export function exitFrame(total, { exitCode = 0, signal = null } = {}) {
  return (
    `event: exit\n` +
    `data: {"exit_code":${exitCode === null ? 'null' : exitCode},` +
    `"signal":${signal === null ? 'null' : signal},"truncated":false,` +
    `"writers_may_be_alive":false,"offset":${total}}\n\n`
  );
}

/** Starts a loopback server that answers each attach from the next script.
 *
 * `scripts` is one array of frames per attach, so a two-element `scripts` is how a **cut and
 * reconnect** is expressed: the first response ends without an `exit` frame, which is exactly the
 * condition the core's reconnect keys on. `requestedPaths` is what makes the reconnect assertable
 * — the offset a second attach asks for is in the query string, and that number is the point of
 * the cursor.
 *
 * Returns a handle with `endpoint`, `requestedPaths`, `offsetsRequested()`, and `close()`. The
 * caller must `close()`, because a leaked listener makes the *next* test flaky — the worst
 * failure mode a helper can have.
 */
export async function startSseServer(scripts) {
  const requestedPaths = [];
  let attach = 0;

  const server = http.createServer((request, response) => {
    requestedPaths.push(request.url);
    // A script that ran out answers an empty body, which the core reads as a cut. Better than
    // hanging: a test that over-attaches should fail on its own assertion, not on a timeout.
    const frames = scripts[attach++] ?? [];
    const body = frames.join('');
    response.writeHead(200, {
      'content-type': 'text/event-stream',
      'content-length': Buffer.byteLength(body),
    });
    response.end(body);
  });

  await new Promise((resolve) => server.listen(0, '127.0.0.1', resolve));
  const { port } = server.address();

  return {
    endpoint: `http://127.0.0.1:${port}`,
    requestedPaths,
    /** The `?offset=` each attach asked for, in order. */
    offsetsRequested() {
      return requestedPaths.map((path) => Number(path.split('offset=')[1]));
    },
    async close() {
      await new Promise((resolve) => server.close(resolve));
    },
  };
}

/** The `ERR_*` code off a thrown or rejected error.
 *
 * `cause.message` and not `.code`: napi's async rejection path is typed over its own closed
 * `Status` enum, so a custom code survives a synchronous throw and is collapsed on a Promise
 * rejection. The cause's message is the code on every path — see `src/errors.rs`.
 */
export function codeOf(error) {
  return error.cause?.message;
}

/** The daemon-status class, one level deeper than the code.
 *
 * `err.cause.cause.message` is the `WireKind` name, and it is **absent** for a local reject
 * because nothing reached the daemon — inventing a status there would be a claim nobody made.
 */
export function wireKindOf(error) {
  return error.cause?.cause?.message;
}
