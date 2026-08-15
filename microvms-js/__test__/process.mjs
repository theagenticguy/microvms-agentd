// SPDX-License-Identifier: Apache-2.0
//
// The `SandboxProcess`-shaped handle: `src/process.rs`, reached through `Session.spawn`.
//
// # What is covered offline, and what is not
//
// Everything here runs against the loopback SSE server in `support/sse.mjs`, which is what makes
// this suite behavioural rather than a set of argument checks: the daemon's framing is
// transcribed there, and the properties under test are this client's — the demultiplexing, the
// byte cursor across a cut, and what a consumer can and cannot fail to notice about a gap.
//
// Two things are deliberately *not* proven here and are stated rather than implied:
//
// * **The daemon's framing.** `support/sse.mjs` is this suite's transcription of what
//   `microvms-core/src/session/sse.rs` parses. If the daemon changed its frames these tests
//   would stay green and the conformance suite would go red. Same boundary `exec.mjs` states.
// * **A real suspend/resume.** The cut modelled below is a response body that ends without an
//   `exit` frame, which is exactly the condition the core's reconnect keys on — but a genuine
//   MicroVM suspend, and the endpoint proxy's behaviour across one, needs a live VM. What is
//   proven is that a body ending without a terminal frame rejoins at the cursor instead of
//   closing the streams.
//
// `Session.spawn` needs a real start response, so every test here starts one against a scripted
// server rather than against an offline endpoint.

import assert from 'node:assert/strict';
import http from 'node:http';
import { test } from 'node:test';

import { GapPolicy, Session, sessionConstants } from '../index.js';
import { exitFrame, gapFrame, outputFrame } from './support/sse.mjs';

/** A syntactically valid exec id, in the `x-<16 hex>` shape the client mints. */
const EXEC_ID = 'x-00000000000000ff';

/** Starts a loopback server that answers `POST /v1/exec/start` and then scripted attaches.
 *
 * `startSseServer` in `support/sse.mjs` answers *only* the stream route, which is all
 * `exec.mjs` needs because it addresses an exec by id without starting one. `spawn` starts one,
 * so this adds the start route — kept local rather than folded into the shared helper, because
 * the shared helper's single-purpose shape is what makes `exec.mjs` readable.
 *
 * `scripts` is one array of frames per attach, so a two-element `scripts` expresses a **cut and
 * reconnect**: the first response ends with no `exit` frame.
 */
async function startSpawnServer(scripts, { pollPhases = [] } = {}) {
  const requestedPaths = [];
  let attach = 0;
  let poll = 0;

  const server = http.createServer((request, response) => {
    requestedPaths.push(`${request.method} ${request.url}`);
    if (request.url === '/v1/exec/start') {
      request.resume();
      const body = JSON.stringify({ exec_id: EXEC_ID, phase: 'running' });
      response.writeHead(200, {
        'content-type': 'application/json',
        'content-length': Buffer.byteLength(body),
      });
      response.end(body);
      return;
    }
    if (request.url.includes('/kill')) {
      request.resume();
      const body = JSON.stringify({ exec_id: EXEC_ID, killed: true });
      response.writeHead(200, {
        'content-type': 'application/json',
        'content-length': Buffer.byteLength(body),
      });
      response.end(body);
      return;
    }
    if (request.url.includes('/stream')) {
      // A script that ran out answers an empty body, which the core reads as a cut. Better
      // than hanging: a test that over-attaches should fail on its own assertion.
      const frames = scripts[attach++] ?? [];
      const body = frames.join('');
      response.writeHead(200, {
        'content-type': 'text/event-stream',
        'content-length': Buffer.byteLength(body),
      });
      response.end(body);
      return;
    }
    // A poll. The phases script is what lets `wait()` be driven without a daemon.
    request.resume();
    const next = pollPhases[Math.min(poll++, pollPhases.length - 1)] ?? {
      exec_id: EXEC_ID,
      phase: 'running',
    };
    const body = JSON.stringify(next);
    response.writeHead(200, {
      'content-type': 'application/json',
      'content-length': Buffer.byteLength(body),
    });
    response.end(body);
  });

  await new Promise((resolve) => server.listen(0, '127.0.0.1', resolve));
  const { port } = server.address();
  return {
    endpoint: `http://127.0.0.1:${port}`,
    requestedPaths,
    /** The `?offset=` each *attach* asked for, in order. */
    offsetsRequested() {
      return requestedPaths
        .filter((path) => path.includes('/stream'))
        .map((path) => Number(path.split('offset=')[1]));
    },
    async close() {
      await new Promise((resolve) => server.close(resolve));
    },
  };
}

/** Spawns against a scripted server. */
async function spawnAgainst(server, options = {}) {
  const session = Session.direct(server.endpoint, 'agent-token');
  return session.spawn(['bash', '-lc', 'true'], {
    exec: { execId: EXEC_ID, ...(options.exec ?? {}) },
    ...options,
  });
}

/** Reads a `ReadableStream<Uint8Array>` to the end and returns the concatenated text. */
async function readAll(stream) {
  const chunks = [];
  for await (const chunk of stream) chunks.push(Buffer.from(chunk));
  return Buffer.concat(chunks).toString('utf8');
}

/** Whether a `#[napi(object)]` field is absent for this shape.
 *
 * **Measured, and not what the type declaration suggests** — the same finding `exec.mjs` records
 * for `StreamEvent`. `ProcessExit` is a `#[napi(object)]` whose inapplicable fields are
 * `Option::None`, and napi renders a `None` field by **omitting the key** rather than by setting
 * it to `null`. So `exit.signal === null` is false, `'signal' in exit` is false, and
 * `exit.signal == null` is true. `index.d.ts` spells these `signal?: number`, which is the honest
 * TypeScript for that.
 *
 * The distinction matters here more than in most places, because the value being tested for is
 * "there is no exit code": a consumer writing `if (exit.exitCode === null)` would take the
 * *success* branch on a signal death. So this asserts the property that actually holds and that a
 * caller can rely on either way — the field is nullish — rather than a spelling the runtime does
 * not guarantee.
 *
 * Note that `pid` is `null` rather than absent, and the difference is not an inconsistency: it is
 * a `#[napi(getter)]` returning `Option<u32>`, which napi renders as `null`. Only object *fields*
 * are omitted.
 */
function absent(value) {
  return value === undefined || value === null;
}

/** Reads a stream and returns `{ text, error }` rather than throwing.
 *
 * Both halves matter for the gap tests: an errored stream still delivers the bytes that arrived
 * before the error, and asserting only on the throw would not say whether those survived.
 */
async function readCatching(stream) {
  const chunks = [];
  try {
    for await (const chunk of stream) chunks.push(Buffer.from(chunk));
  } catch (error) {
    return { text: Buffer.concat(chunks).toString('utf8'), error };
  }
  return { text: Buffer.concat(chunks).toString('utf8'), error: null };
}

// -- the demultiplexing -------------------------------------------------------

test('one interleaved SSE channel becomes two independent byte streams', async () => {
  // The core property. The daemon publishes both sides into one stream with a `stream`
  // discriminator and one shared offset space; a `SandboxProcess` consumer needs two streams.
  // The verdict is that each side gets *its own* bytes and none of the other's: a demultiplexer
  // that sent every frame to both, or that keyed on the wrong field, would still deliver the
  // right total byte count.
  //
  // **Guard proof.** Sending every frame to `out_tx` regardless of the discriminator — the
  // shape of a bridge that forgot to demultiplex — makes this red, along with the order and
  // both-streams-reconnect tests. Verified.
  const server = await startSpawnServer([
    [
      outputFrame(0, 'out-one\n', 'stdout'),
      outputFrame(8, 'err-one\n', 'stderr'),
      outputFrame(16, 'out-two\n', 'stdout'),
      outputFrame(24, 'err-two\n', 'stderr'),
      exitFrame(32),
    ],
  ]);
  try {
    const proc = await spawnAgainst(server);
    // Read concurrently, which is also the shape that would deadlock if the two channels shared
    // one buffer: with capacity 1 per channel, a stderr nobody reads must not stall stdout.
    const [out, err] = await Promise.all([readAll(proc.stdout), readAll(proc.stderr)]);
    assert.equal(out, 'out-one\nout-two\n', 'stdout carried the wrong bytes');
    assert.equal(err, 'err-one\nerr-two\n', 'stderr carried the wrong bytes');
  } finally {
    await server.close();
  }
});

test('order is preserved within each stream', async () => {
  // Asserted on a sequence long enough that a reordering shows up as a different string rather
  // than as a coincidence. Within-stream order is the guarantee `SandboxProcess` makes; between
  // streams it is not recoverable, because the wire shares one offset space.
  //
  // **Guard proof.** Same breakage as above (one channel for both) makes this red with
  // `01234567` on stdout and nothing on stderr. Verified.
  const frames = [];
  let offset = 0;
  for (let index = 0; index < 8; index += 1) {
    const chunk = `${index}`;
    frames.push(outputFrame(offset, chunk, index % 2 === 0 ? 'stdout' : 'stderr'));
    offset += chunk.length;
  }
  frames.push(exitFrame(offset));

  const server = await startSpawnServer([frames]);
  try {
    const proc = await spawnAgainst(server);
    const [out, err] = await Promise.all([readAll(proc.stdout), readAll(proc.stderr)]);
    assert.equal(out, '0246');
    assert.equal(err, '1357');
  } finally {
    await server.close();
  }
});

test('reading a stream twice hands back the same object', async () => {
  // Two `ReadableStream`s over one channel would split the bytes between their readers, so the
  // getter has to be idempotent. `readonly stdout` in the harness contract is one stream.
  //
  // **Guard proof.** Building a fresh `ReadableStream` in the `Built` arm of `stream_of` makes
  // this red — and would, in production, split one child's output between two readers.
  // Verified.
  const server = await startSpawnServer([[outputFrame(0, 'x', 'stdout'), exitFrame(1)]]);
  try {
    const proc = await spawnAgainst(server);
    assert.equal(proc.stdout, proc.stdout, 'the stdout getter built a second stream');
    assert.equal(proc.stderr, proc.stderr, 'the stderr getter built a second stream');
    assert.notEqual(proc.stdout, proc.stderr, 'both getters returned one stream');
  } finally {
    await server.close();
  }
});

// -- the reconnect, which is the property no other backend has ----------------

test('a cut mid-stream rejoins at the byte cursor rather than closing the streams', async () => {
  // The property this whole handle exists to preserve. A response body that ends with no `exit`
  // frame is exactly what a suspend looks like to the core, and the two available wrong answers
  // both produce a plausible-looking result:
  //
  // * closing the streams there reports a truncated log as a complete one;
  // * reconnecting at zero delivers every byte *and duplicates the ones already seen*.
  //
  // So the verdict is the reassembled text (which catches the duplicate) **and** the offset the
  // second attach asked for (which catches a cursor that was reset).
  //
  // **Guard proof.** Hard-coding `reconnect: false` in `SpawnOptions::stream_options` makes
  // this red, along with the both-streams and gap-cursor tests: stdout reads `AAAA\nBBBB\n`
  // and the suspend has become a silent truncation. Verified.
  const server = await startSpawnServer([
    [outputFrame(0, 'AAAA\n', 'stdout'), outputFrame(5, 'BBBB\n', 'stdout')],
    [outputFrame(10, 'CCCC\n', 'stdout'), exitFrame(15)],
  ]);
  try {
    const proc = await spawnAgainst(server);
    const out = await readAll(proc.stdout);
    assert.equal(
      out,
      'AAAA\nBBBB\nCCCC\n',
      'the two attaches did not reconstruct stdout, so the resume duplicated or lost bytes',
    );
    assert.deepEqual(
      server.offsetsRequested(),
      [0, 10],
      'the reconnect asked for the wrong byte, so the seam is wrong',
    );
  } finally {
    await server.close();
  }
});

test('a cut across both streams rejoins on each without crossing them', async () => {
  // The demultiplexing and the reconnect at once, because that combination is where a
  // per-stream cursor would go wrong: the wire has one offset space, so a client keeping two
  // cursors would ask for the wrong byte after a cut on either side.
  const server = await startSpawnServer([
    [outputFrame(0, 'o1', 'stdout'), outputFrame(2, 'e1', 'stderr')],
    [outputFrame(4, 'o2', 'stdout'), outputFrame(6, 'e2', 'stderr'), exitFrame(8)],
  ]);
  try {
    const proc = await spawnAgainst(server);
    const [out, err] = await Promise.all([readAll(proc.stdout), readAll(proc.stderr)]);
    assert.equal(out, 'o1o2');
    assert.equal(err, 'e1e2');
    assert.deepEqual(server.offsetsRequested(), [0, 4]);
  } finally {
    await server.close();
  }
});

test('a spawn can start reading at an offset a previous process left off at', async () => {
  // With a stable `exec.execId` this is how a spawned process survives *this* process
  // restarting: the exec lives in the VM, and the offset is where the last reader stopped.
  const server = await startSpawnServer([[outputFrame(64, 'tail', 'stdout'), exitFrame(68)]]);
  try {
    const proc = await spawnAgainst(server, { offset: 64 });
    assert.equal(await readAll(proc.stdout), 'tail');
    assert.deepEqual(server.offsetsRequested(), [64]);
  } finally {
    await server.close();
  }
});

// -- the gap, and what a consumer cannot fail to notice -----------------------

test('a gap errors both streams by default and names the range to resume from', async () => {
  // The design decision, asserted as a property of the *consumer* rather than of the handle:
  // the obvious harness-side code is `for await (const chunk of proc.stdout)`, and this test is
  // written that way. It throws. A gap policy that recorded the range out-of-band would leave
  // this loop finishing normally with bytes missing, which is the one failure the cursor
  // protocol exists to prevent.
  //
  // Both streams error, because the wire's offset space is shared and the daemon cannot say
  // which side's bytes were evicted: erroring only one would leave the other looking complete
  // when it may be the truncated one.
  //
  // **Guard proof.** Forcing `error_on_gap = false` in the drive — i.e. making `'event'` the
  // only behaviour — makes this red: both loops finish normally, `out.text` is
  // `'beforeafter'`, and a consumer reading it cannot tell that 894 bytes are missing.
  // Verified.
  const server = await startSpawnServer([
    [
      outputFrame(0, 'before', 'stdout'),
      gapFrame(6, 900),
      outputFrame(900, 'after', 'stdout'),
      exitFrame(905),
    ],
  ]);
  try {
    const proc = await spawnAgainst(server);
    const [out, err] = await Promise.all([
      readCatching(proc.stdout),
      readCatching(proc.stderr),
    ]);

    assert.ok(out.error, 'stdout finished normally with bytes missing from it');
    assert.ok(err.error, 'stderr looked complete while the shared offset space had a hole');
    assert.match(
      out.error.message,
      /\[6, 900\)/,
      `the error must name the lost range so a caller can resume: ${out.error.message}`,
    );
    assert.match(
      out.error.message,
      /offset 900/,
      `the error must name where to resume: ${out.error.message}`,
    );
    // The bytes that did arrive are still delivered: an errored stream is not an empty one, and
    // a caller resuming at 900 needs to know it already has [0, 6).
    assert.equal(out.text, 'before', 'the bytes before the gap were discarded');
  } finally {
    await server.close();
  }
});

test('gapPolicy event keeps the streams open and records the range instead', async () => {
  // The opt-out, for a caller that wants the surviving bytes more than the completeness
  // guarantee. The tradeoff is then visible at the call site, which is the whole reason it is
  // an option rather than the default.
  const server = await startSpawnServer([
    [
      outputFrame(0, 'before', 'stdout'),
      gapFrame(6, 900),
      outputFrame(900, 'after', 'stdout'),
      exitFrame(905),
    ],
  ]);
  try {
    const proc = await spawnAgainst(server, { gapPolicy: GapPolicy.Event });
    const out = await readCatching(proc.stdout);

    assert.equal(out.error, null, 'the event policy still errored the stream');
    assert.equal(out.text, 'beforeafter', 'the surviving bytes did not both arrive');
    assert.deepEqual(
      proc.gaps.map((gap) => [gap.stream, gap.from, gap.to]),
      [['stdout', 6, 900]],
      'the gap was swallowed, so a truncated log reads as a complete one',
    );
  } finally {
    await server.close();
  }
});

test('a gap advances the cursor so a reconnect does not ask for evicted bytes', async () => {
  // The core's rule, through this handle: the daemon has already moved its own cursor past the
  // evicted range, so a client whose cursor did not follow would ask for those bytes again and
  // be told about the same gap forever — a livelock that reads as a slow stream.
  const server = await startSpawnServer([
    [outputFrame(0, 'AA', 'stdout'), gapFrame(2, 900)],
    [outputFrame(900, 'ZZ', 'stdout'), exitFrame(902)],
  ]);
  try {
    const proc = await spawnAgainst(server, { gapPolicy: GapPolicy.Event });
    assert.equal(await readAll(proc.stdout), 'AAZZ');
    assert.deepEqual(
      server.offsetsRequested(),
      [0, 900],
      'the reconnect asked for bytes the daemon had already evicted',
    );
  } finally {
    await server.close();
  }
});

// -- wait, kill, and the identity surface -------------------------------------

test('wait resolves from the daemon record, not from the streams ending', async () => {
  // The distinction that makes a suspend/resume recoverable: "the stream stopped" is the same
  // observation for a cut connection and a finished command, so the exit code cannot come from
  // there. Here the stream is *already over* and the daemon still reports running once before
  // exiting, which a `wait` reading the stream's end would answer too early.
  //
  // **Guard proof.** Replacing `handle.wait(timeout)` with a single `handle.poll()` — which is
  // what a wait keyed on the stream ending amounts to — makes this red with exit code 0 for a
  // command the daemon had not finished reporting on. Verified.
  const server = await startSpawnServer([[outputFrame(0, 'hi', 'stdout'), exitFrame(2)]], {
    pollPhases: [
      { exec_id: EXEC_ID, phase: 'running' },
      {
        exec_id: EXEC_ID,
        phase: 'exited',
        exit_code: 3,
        signal: null,
        stdout: '',
        stderr: '',
        truncated: false,
        writers_may_be_alive: false,
      },
    ],
  });
  try {
    const proc = await spawnAgainst(server);
    assert.equal(await readAll(proc.stdout), 'hi');
    const exit = await proc.wait(30);
    assert.equal(exit.exitCode, 3, 'the exit code did not come from the daemon record');
    assert.ok(absent(exit.signal), 'a clean exit reported a signal');
  } finally {
    await server.close();
  }
});

test('a signal death reports a null exit code rather than inventing one', async () => {
  // The one place this shape and the harness's `{ exitCode: number }` differ, deliberately. The
  // two available lies are `0` — a killed build reported as passing — and `128 + signo`, a
  // number the daemon never published and which is indistinguishable from a child that really
  // exited with it. A wrapper that must produce a number picks at its own boundary.
  const server = await startSpawnServer([[exitFrame(0)]], {
    pollPhases: [
      {
        exec_id: EXEC_ID,
        phase: 'exited',
        exit_code: null,
        signal: 9,
        stdout: '',
        stderr: '',
        truncated: false,
        writers_may_be_alive: false,
      },
    ],
  });
  try {
    const proc = await spawnAgainst(server);
    const exit = await proc.wait(30);
    assert.ok(
      absent(exit.exitCode),
      `a signal death was reported as exit code ${exit.exitCode}, so a killed build reads as a passing one`,
    );
    assert.equal(exit.signal, 9, 'the signal is what makes a null exit code actionable');
  } finally {
    await server.close();
  }
});

test('kill is idempotent, so a harness can call it in a finally without guarding', async () => {
  const server = await startSpawnServer([[outputFrame(0, 'x', 'stdout'), exitFrame(1)]]);
  try {
    const proc = await spawnAgainst(server);
    await proc.kill();
    await proc.kill();
    const kills = server.requestedPaths.filter((path) => path.includes('/kill'));
    assert.equal(kills.length, 2, 'the second kill did not reach the daemon');
  } finally {
    await server.close();
  }
});

test('a spawned process exposes its exec id and no pid', async () => {
  // The exec id is the idempotency key and the reattach handle. `pid` is `null` because the
  // daemon publishes none — the harness contract marks it optional for exactly this case, and a
  // fabricated number would invite a caller to signal it through a channel that does not exist.
  const server = await startSpawnServer([[exitFrame(0)]]);
  try {
    const proc = await spawnAgainst(server);
    assert.equal(proc.execId, EXEC_ID);
    assert.equal(proc.pid, null);
    assert.deepEqual(proc.gaps, []);
  } finally {
    await server.close();
  }
});

test('a process has no constructor, so a handle cannot exist without an exec', async () => {
  // Same rule as `ExecHandle`: a process object with no transport behind it is one whose every
  // method fails in a way that looks like a dead VM.
  const { ExecProcess } = await import('../index.js');
  assert.throws(() => new ExecProcess());
});

// -- the shape-compatibility claim, asserted structurally ---------------------

test('the handle is structurally usable as an AI SDK SandboxProcess', async () => {
  // The compatibility claim, checked rather than only documented — and checked *structurally*,
  // because this crate deliberately depends on no harness package: their type is one consumer
  // of this handle's contract, not its definition.
  //
  // The wrap below is the whole adapter an external provider writes, so if this stops
  // type-checking in spirit, that provider breaks.
  const server = await startSpawnServer([
    [outputFrame(0, 'o', 'stdout'), outputFrame(1, 'e', 'stderr'), exitFrame(2)],
  ]);
  try {
    const proc = await spawnAgainst(server);
    const asSandboxProcess = {
      pid: proc.pid ?? undefined,
      stdout: proc.stdout,
      stderr: proc.stderr,
      wait: () => proc.wait(),
      kill: () => proc.kill(),
    };

    assert.ok(
      asSandboxProcess.stdout instanceof ReadableStream,
      'stdout is not a web ReadableStream, so a harness cannot pipe it',
    );
    assert.ok(asSandboxProcess.stderr instanceof ReadableStream);
    assert.equal(typeof asSandboxProcess.wait, 'function');
    assert.equal(typeof asSandboxProcess.kill, 'function');
    // `getReader()` is what a harness actually calls, and a `ReadableStream` that could not
    // produce one would satisfy `instanceof` and fail at use.
    const reader = asSandboxProcess.stdout.getReader();
    const first = await reader.read();
    assert.equal(Buffer.from(first.value).toString('utf8'), 'o');
    reader.releaseLock();
  } finally {
    await server.close();
  }
});

// -- the WebSocket connect credentials (feature 1, through this binding) ------

test('connect subprotocols are the platform three, in order, for the requested port', async () => {
  // A direct session has no proxy, so the credential path cannot be driven end-to-end without
  // the control plane — that half is core's (`session::tests`). What *is* assertable here is the
  // published wire contract and the direct-session answer, and the contract is read out of
  // `sessionConstants()` rather than spelled locally: the platform matches these strings
  // exactly, so a test asserting its own copy would assert only that the copy is
  // self-consistent.
  const constants = JSON.parse(sessionConstants());
  assert.equal(constants.wsSubprotocol, 'lambda-microvms');
  assert.equal(constants.wsAuthSubprotocolPrefix, 'lambda-microvms.authentication.');
  assert.equal(constants.wsPortSubprotocolPrefix, 'lambda-microvms.port.');

  const session = Session.direct('http://127.0.0.1:9', 'agent-token');
  // `null` rather than a three-element list with an empty token: the subprotocol form exists
  // only for a request through the endpoint proxy, and offering the base value with no
  // credential would open a handshake refused for a reason naming neither the token nor the
  // port.
  assert.equal(await session.connectSubprotocols(8080), null);
  // Empty headers *is* the true answer for a direct session, because that is what its requests
  // carry. The asymmetry with the above is deliberate.
  assert.deepEqual(await session.connectHeaders(8080), {});
});
