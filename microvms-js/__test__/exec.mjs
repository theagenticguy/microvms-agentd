// SPDX-License-Identifier: Apache-2.0
//
// The exec and stream surface: `src/exec.rs`.
//
// # What is covered offline, and what is not
//
// An `ExecHandle` is an id plus a transport, so constructing one needs no daemon — and the
// `stream()` path can be driven against the loopback SSE server in `support/sse.mjs`, which makes
// this the one binding module with real behavioural coverage rather than argument checks alone.
// Three groups:
//
// * **Construction and argument validation** — no server. Every numeric option goes through the
//   core's `duration_of_secs_f64`, and JS makes that matter more than Python does: `NaN` and
//   `Infinity` are ordinary values a caller reaches from a division or a `Number(undefined)`.
// * **The event objects** — the tagged union, its `kind` discriminant, and the byte/offset
//   arithmetic a caller resumes from.
// * **The stream contract as the binding drives it** — order, the terminal event, a gap as an
//   event versus as a rejection, `break` stopping the drive, and a cut reconnecting at the cursor.
//   These are `microvms-core` properties, and asserting them *here* is the point: the binding
//   drives them through its own spawned task, capacity-1 channel, and `AsyncGenerator`, so "core's
//   tests pass" is not a statement about this path.
//
// **The boundary, stated honestly.** The SSE server is this suite's transcription of the frame
// shapes the core parses. It is not `agentd`, so nothing here proves the daemon emits those frames
// — that is the conformance suite's job. What these prove is that this client parses, orders, and
// resumes correctly given the framing.
//
// The methods that genuinely need a live daemon (`poll`, `wait`, `ack`, `kill`, `writeStdin`) are
// covered only for argument validation and failure taxonomy, because a fake for them would be a
// fake of the daemon's *state machine* — a second implementation whose agreement with the real one
// is exactly what nobody would be testing.

import assert from 'node:assert/strict';
import { test } from 'node:test';

import { ExecHandle, Session } from '../index.js';
import {
  codeOf,
  exitFrame,
  gapFrame,
  outputFrame,
  startSseServer,
  wireKindOf,
} from './support/sse.mjs';

// A syntactically valid exec id, in the `x-<16 hex>` shape the client mints.
const EXEC_ID = 'x-0000000000000001';

/** An exec handle addressing a scripted server. */
async function handleAgainst(server) {
  return Session.direct(server.endpoint, 'agent-token').exec(EXEC_ID);
}

/** A handle whose endpoint nothing is listening on.
 *
 * Fine for every argument-validation test: those refusals all happen *before* a request is built,
 * which is the property being asserted.
 */
async function offlineHandle() {
  return Session.direct('http://127.0.0.1:9', 'agent-token').exec(EXEC_ID);
}

/** Drains a stream into an array. */
async function drain(stream) {
  const seen = [];
  for await (const event of stream) seen.push(event);
  return seen;
}

/** Whether a `StreamEvent` field is absent for this event's shape.
 *
 * **Measured, and not what the type declaration suggests.** `StreamEvent` is a
 * `#[napi(object)]` whose inapplicable fields are `Option::None`, and napi renders a `None`
 * field by **omitting the key** rather than by setting it to `null` — so a gap event has no
 * `data` property at all, and reading it yields `undefined`. `index.d.ts` spells these
 * `data?: Buffer`, which is the honest TypeScript for that, but the prose in `src/exec.rs` says
 * the other shapes' fields "are `null`".
 *
 * The distinction is not academic for a JS consumer: `'data' in event` is false, `event.data ===
 * null` is false, and `event.data == null` is true. So this helper asserts the property that
 * actually holds and that a caller can rely on either way — the field is nullish — rather than
 * picking one spelling the runtime does not guarantee.
 */
function absent(value) {
  return value === undefined || value === null;
}

// -- construction, with no daemon anywhere ------------------------------------

test('a handle is an id plus a transport and reaches nothing to exist', async () => {
  // The reattach path, which is why the exec id is caller-minted. A constructor that probed would
  // make "do I have a handle" mean "does that exec exist" — different questions, and the second
  // has no answer in the window between a run and the daemon recording it.
  const handle = await offlineHandle();
  assert.equal(handle.execId, EXEC_ID);
});

test('an exec handle has no constructor, so an id is not writable past the session', async () => {
  // `new ExecHandle(...)` would be a handle with no transport behind it — an object every method
  // on which fails in a way that looks like a dead VM.
  assert.throws(() => new ExecHandle(EXEC_ID));
});

test('two handles for one id address the same exec', async () => {
  // The idempotency key survives a rebuild, including across a process restart. Asserted as far as
  // it can be offline: the id is what addresses the exec. A binding that minted its own id per
  // handle would break the reattach and nothing local would object.
  const session = Session.direct('http://127.0.0.1:9', 'agent-token');
  const first = await session.exec(EXEC_ID);
  const second = await session.exec(EXEC_ID);
  assert.equal(first.execId, second.execId);
});

test('a non-finite or negative idle timeout is refused before any request', async () => {
  // JS makes this matter more than Python does: `NaN` and `Infinity` are ordinary values a caller
  // reaches by accident, from a division or `Number(undefined)`. An idle timeout of `NaN` compares
  // false against every deadline, so a stream would never time out — a hang rather than an error,
  // which is the worst way for a bad argument to fail.
  //
  // A synchronous `throws` and not `rejects`, and the difference is the point: `stream()` is a
  // plain function, so the refusal arrives before a Promise exists and a caller who forgot to
  // `await` still sees it.
  const handle = await offlineHandle();
  for (const bad of [-1, -0.001, Number.NaN, Number.POSITIVE_INFINITY, Number.NEGATIVE_INFINITY]) {
    assert.throws(
      () => handle.stream({ idleTimeout: bad }),
      (error) => {
        assert.equal(codeOf(error), 'ERR_INVALID_ARG');
        // A local reject reached no daemon, so there is no wire kind to report.
        assert.equal(wireKindOf(error), undefined);
        return true;
      },
      `idleTimeout: ${bad} was accepted`,
    );
  }
});

test('a non-finite wait timeout rejects with the same code on the async path', async () => {
  // The same refusal, reached through an `async fn` — where napi collapses `.code` to a status and
  // the code lives on the cause. Both paths are asserted because a binding that validated one and
  // not the other would leave the laxer door as the one most callers use.
  const handle = await offlineHandle();
  for (const method of ['wait', 'waitAndAck']) {
    for (const bad of [-1, Number.NaN, Number.POSITIVE_INFINITY]) {
      await assert.rejects(
        () => handle[method](bad),
        (error) => {
          assert.equal(codeOf(error), 'ERR_INVALID_ARG');
          return true;
        },
        `${method}(${bad}) was accepted`,
      );
    }
  }
});

test('a zero timeout is accepted because it means do not wait', async () => {
  // The boundary the refusals must not swallow. Zero is a real request — poll once and give up —
  // and refusing it would force a caller to special-case "I do not want to block".
  const handle = await offlineHandle();
  assert.ok(handle.stream({ idleTimeout: 0 }));
});

test('stream options are optional and default without an argument', async () => {
  // `handle.stream()` is the common call, so every option has to have a default. A required
  // options bag would make the simple case the verbose one.
  const handle = await offlineHandle();
  assert.ok(handle.stream());
  assert.ok(handle.stream({}));
  assert.ok(handle.stream({ offset: 5, reconnect: false, maxReconnects: 3, errorOnGap: true }));
});

test('a negative offset is clamped rather than sent as a negative byte position', async () => {
  // JS has no unsigned integer, so `offset: -1` is writable where in Rust it is not. Clamping to
  // zero is the honest reading — "start at the beginning" — and the alternative would be a query
  // string the daemon refuses. Asserted through the offset the attach actually asks for.
  const server = await startSseServer([[outputFrame(0, 'AA'), exitFrame(2)]]);
  try {
    const handle = await handleAgainst(server);
    await drain(handle.stream({ offset: -5, idleTimeout: 5 }));
    assert.deepEqual(server.offsetsRequested(), [0]);
  } finally {
    await server.close();
  }
});

// -- the event objects ---------------------------------------------------------

test('an output event reports its bytes, its offset, and where a cursor resumes', async () => {
  // `end` is `offset + data.length`, which is the number a resume passes back. Derived rather than
  // stored, so the arithmetic is what is under test — a caller computing it themselves would be
  // maintaining a second cursor, the thing the core's docs single out as the way the two come to
  // disagree exactly when a reconnect happens.
  const server = await startSseServer([[outputFrame(64, 'hello\n'), exitFrame(70)]]);
  try {
    const handle = await handleAgainst(server);
    const [chunk] = await drain(handle.stream({ offset: 64, idleTimeout: 5 }));

    assert.equal(chunk.kind, 'output');
    assert.equal(chunk.stream, 'stdout');
    assert.equal(chunk.offset, 64);
    assert.equal(chunk.end, 70);
    assert.equal(chunk.end, chunk.offset + chunk.data.length);
    assert.ok(Buffer.isBuffer(chunk.data), 'output is arbitrary bytes, never only a string');
    assert.equal(chunk.data.toString(), 'hello\n');
  } finally {
    await server.close();
  }
});

test('output carries both the lossless bytes and the lossy text', async () => {
  // Exec output is arbitrary bytes — a compiler writing a latin-1 filename, a program emitting a
  // partial UTF-8 sequence at a chunk boundary. `data` is the lossless form and `text` is the
  // convenience; a `text`-only event would make the loss invisible.
  const invalid = Buffer.from([0x63, 0x61, 0x66, 0xe9, 0x0a]); // "caf\xe9\n"
  const server = await startSseServer([[outputFrame(0, invalid), exitFrame(invalid.length)]]);
  try {
    const handle = await handleAgainst(server);
    const [chunk] = await drain(handle.stream({ idleTimeout: 5 }));

    assert.deepEqual(Buffer.from(chunk.data), invalid, 'the bytes were altered in transit');
    // Replacing rather than throwing: a stream must not die on one undecodable byte.
    assert.equal(chunk.text, 'caf�\n');
  } finally {
    await server.close();
  }
});

test('a gap is a typed event carrying the range that is gone', async () => {
  // A truncated log has to be distinguishable from a complete one — the whole argument for a typed
  // event over a log line. `offset` inclusive, `end` exclusive, so `end` is where a cursor resumes.
  const server = await startSseServer([
    [outputFrame(0, 'AA'), gapFrame(2, 900), outputFrame(900, 'ZZ'), exitFrame(902)],
  ]);
  try {
    const handle = await handleAgainst(server);
    const seen = await drain(handle.stream({ idleTimeout: 5 }));

    assert.deepEqual(
      seen.map((event) => event.kind),
      ['output', 'gap', 'output', 'exit'],
      'the stream did not continue past the gap',
    );
    const gap = seen.find((event) => event.kind === 'gap');
    assert.equal(gap.offset, 2);
    assert.equal(gap.end, 900);
    // The fields belonging to the other shapes are absent, which is what keeps the union tagged.
    // See `absent` for why that is "omitted" rather than "null".
    assert.ok(absent(gap.data), 'a gap carried output bytes');
    assert.ok(absent(gap.exitCode), 'a gap carried an exit code');
    assert.ok(absent(gap.text));
  } finally {
    await server.close();
  }
});

test('the exit event carries a total rather than a resume position', async () => {
  // `totalOffset` on an exit is a **total**, and `end` is null — because resuming from a total
  // would ask the daemon to replay from the end of a finished stream. The core's
  // `ExecEvent::end()` answers `None` here for exactly that reason, and this is the JS statement
  // of it.
  const server = await startSseServer([[outputFrame(0, 'done\n'), exitFrame(5)]]);
  try {
    const handle = await handleAgainst(server);
    const seen = await drain(handle.stream({ idleTimeout: 5 }));
    const exit = seen.at(-1);

    assert.equal(exit.kind, 'exit');
    assert.equal(exit.totalOffset, 5);
    assert.equal(exit.exitCode, 0);
    assert.ok(absent(exit.signal), 'a clean exit has no signal');
    assert.ok(absent(exit.end), 'an exit offset is not a resume position');
    assert.ok(absent(exit.offset), 'an exit has a total, not a position');
    assert.equal(exit.truncated, false);
    assert.equal(exit.writersMayBeAlive, false);
  } finally {
    await server.close();
  }
});

test('a signal death has a null exit code, and zero is not no-signal', async () => {
  // Two nulls that are not interchangeable with zero. A child killed by SIGKILL has no exit code
  // at all, and reporting 0 there would say it succeeded. Symmetrically `signal: 0` is not "no
  // signal" — which is why both are null rather than sentinel numbers.
  const server = await startSseServer([[exitFrame(0, { exitCode: null, signal: 9 })]]);
  try {
    const handle = await handleAgainst(server);
    const [exit] = await drain(handle.stream({ idleTimeout: 5 }));

    assert.ok(absent(exit.exitCode), 'a signal death must not report exit code 0');
    assert.equal(exit.signal, 9);
  } finally {
    await server.close();
  }
});

test('the kind discriminant is exact, so a switch over it is total', async () => {
  // The TypeScript idiom is a tagged union, and the tag has to be reliable: a gap reporting
  // `kind: "output"` would send a truncation down the happy path. Asserted as the *pairing* of the
  // tag with the fields only that shape has.
  const server = await startSseServer([
    [outputFrame(0, 'AA'), gapFrame(2, 4), outputFrame(4, 'BB'), exitFrame(6)],
  ]);
  try {
    const handle = await handleAgainst(server);
    const seen = await drain(handle.stream({ idleTimeout: 5 }));

    for (const event of seen) {
      assert.ok(['output', 'gap', 'exit'].includes(event.kind), event.kind);
      if (event.kind === 'output') {
        assert.ok(!absent(event.data), 'an output event carries bytes');
        assert.ok(!absent(event.stream), 'an output event names its stream');
        assert.ok(absent(event.totalOffset), 'only an exit carries a total');
      } else if (event.kind === 'gap') {
        assert.ok(absent(event.data));
        assert.ok(absent(event.stream), 'a gap spans both streams, so it names neither');
        assert.ok(absent(event.totalOffset));
      } else {
        assert.ok(!absent(event.totalOffset), 'an exit carries the total published');
        assert.ok(absent(event.data));
        assert.ok(absent(event.end));
      }
    }
  } finally {
    await server.close();
  }
});

// -- the stream contract, as the binding drives it ----------------------------

test('events reach the async iterator in wire order', async () => {
  // Order, asserted on the offsets rather than only on the reassembled text: two chunks
  // concatenated the wrong way round still total the right length. This is the property the
  // binding's capacity-1 channel could plausibly break — a buffered hand-off, or a second consumer
  // task — and reordering a child's stdout is not a subtle failure for whoever reads it.
  const server = await startSseServer([
    [outputFrame(0, 'AAAAA'), outputFrame(5, 'BBBBB'), outputFrame(10, 'CCCCC'), exitFrame(15)],
  ]);
  try {
    const handle = await handleAgainst(server);
    const seen = await drain(handle.stream({ idleTimeout: 5 }));
    const outputs = seen.filter((event) => event.kind === 'output');

    assert.deepEqual(
      outputs.map((event) => event.offset),
      [0, 5, 10],
      'the iterator saw the frames out of order',
    );
    assert.equal(outputs.map((event) => event.text).join(''), 'AAAAABBBBBCCCCC');
    assert.equal(seen.at(-1).kind, 'exit', 'the terminal event has to be delivered');
  } finally {
    await server.close();
  }
});

test('the iterator ends after the terminal event rather than hanging', async () => {
  // Worth its own test because the failure mode is a hang: a channel whose sender was never
  // dropped would leave `next()` awaiting `recv` forever, and a hang in a `for await` reads as a
  // slow daemon rather than as a client bug.
  const server = await startSseServer([[outputFrame(0, 'hi\n'), exitFrame(3)]]);
  try {
    const handle = await handleAgainst(server);
    const seen = await drain(handle.stream({ idleTimeout: 5 }));
    assert.deepEqual(
      seen.map((event) => event.kind),
      ['output', 'exit'],
    );
  } finally {
    await server.close();
  }
});

test('breaking out of the loop stops the stream rather than leaving it reading', async () => {
  // The `Break` path, which is why the driver's callback answers a `ControlFlow`. A `break` drops
  // the iterator, the next channel send fails, and the drive ends — so nothing is left reading a
  // body nobody reads. Asserted through the observable that shows it: one attach, and the loop
  // stopped after one event even though three were scripted.
  const server = await startSseServer([
    [outputFrame(0, 'first\n'), outputFrame(6, 'second\n'), exitFrame(13)],
  ]);
  try {
    const handle = await handleAgainst(server);
    let seen = 0;
    for await (const _event of handle.stream({ idleTimeout: 5 })) {
      seen += 1;
      break;
    }
    assert.equal(seen, 1);
    assert.equal(server.requestedPaths.length, 1, 'a stopped stream reattached');
  } finally {
    await server.close();
  }
});

test('a cut stream reconnects at the cursor, losing and duplicating nothing', async () => {
  // The reconnect property, through the binding's own task and channel. The verdict is the
  // reassembled bytes **and** the offset the second attach asked for — the second half is what
  // makes it a real test, because a client that reconnected at zero would deliver every byte too
  // and only the seam shows the difference.
  //
  // This is also the regression for the driver migration. The binding used to consume a `Stream`;
  // it now drives `for_each_event_async`, and the cursor is read off core's state machine rather
  // than tallied here — so a migration that had dropped the cursor would show up as a second
  // `offset=0`.
  const server = await startSseServer([
    // First attach: two frames, then the body ends with no exit event — a cut.
    [outputFrame(0, 'AAAA\n'), outputFrame(5, 'BBBB\n')],
    // Second attach: the daemon replays from the offset it was asked for.
    [outputFrame(10, 'CCCC\n'), exitFrame(15)],
  ]);
  try {
    const handle = await handleAgainst(server);
    const seen = await drain(handle.stream({ idleTimeout: 5 }));

    assert.equal(
      seen
        .filter((event) => event.kind === 'output')
        .map((event) => event.text)
        .join(''),
      'AAAA\nBBBB\nCCCC\n',
      'the two attaches did not reconstruct the output',
    );
    assert.equal(seen.at(-1).kind, 'exit');
    assert.deepEqual(
      server.offsetsRequested(),
      [0, 10],
      'the reconnect asked for the wrong byte, so the seam is wrong',
    );
  } finally {
    await server.close();
  }
});

test('a gap advances the cursor so a reconnect does not ask for evicted bytes', async () => {
  // The second cursor rule, and the one a locally-tallied cursor gets wrong. The daemon has
  // already moved past the evicted range; if this client's cursor did not follow, a reconnect
  // would ask for those bytes again and be told about the same gap forever — a livelock that looks
  // like a slow stream rather than an error.
  const server = await startSseServer([
    [outputFrame(0, 'AA'), gapFrame(2, 900)],
    [outputFrame(900, 'ZZ'), exitFrame(902)],
  ]);
  try {
    const handle = await handleAgainst(server);
    const seen = await drain(handle.stream({ idleTimeout: 5 }));

    assert.deepEqual(
      seen.map((event) => event.kind),
      ['output', 'gap', 'output', 'exit'],
    );
    assert.deepEqual(server.offsetsRequested(), [0, 900]);
  } finally {
    await server.close();
  }
});

test('reconnect off ends the stream at the cut with no exit event', async () => {
  // For a caller doing its own reconnection, and the ending is *silent* rather than a rejection.
  // The load-bearing part is what a caller can tell afterwards: the iterator ended and no exit
  // event arrived, which is the signature of a cut. Treating "the loop finished" as "the command
  // finished" would pass a CI step on output nobody received — so the absence of the terminal
  // event is the assertion.
  const server = await startSseServer([[outputFrame(0, 'partial')]]);
  try {
    const handle = await handleAgainst(server);
    const seen = await drain(handle.stream({ reconnect: false, idleTimeout: 5 }));

    assert.deepEqual(
      seen.map((event) => event.kind),
      ['output'],
    );
    assert.ok(!seen.some((event) => event.kind === 'exit'));
    assert.equal(server.requestedPaths.length, 1, 'reconnect: false reattached anyway');
  } finally {
    await server.close();
  }
});

test('a starting offset is passed through to the daemon', async () => {
  // What a second process resuming another's stream passes. The offset has to reach the query
  // string unaltered: a client that started at zero regardless would replay output the first
  // process already showed someone.
  const server = await startSseServer([[outputFrame(64, 'tail'), exitFrame(68)]]);
  try {
    const handle = await handleAgainst(server);
    const seen = await drain(handle.stream({ offset: 64, idleTimeout: 5 }));

    assert.equal(seen[0].text, 'tail');
    assert.deepEqual(server.offsetsRequested(), [64]);
  } finally {
    await server.close();
  }
});

test('errorOnGap rejects the iteration and the events before it stay delivered', async () => {
  // What a caller that must have complete output asks for. Two things are asserted and the second
  // is the interesting one: the events *before* the gap stay delivered. That asymmetry is
  // deliberate — the bytes a caller already received are real output, and there is nothing to
  // unwind them with — so the rejection arrives after them rather than instead of them.
  //
  // The cause chain is checked here rather than only the message, and that is the regression for a
  // real defect: this rejection used to be rebuilt from the error's reason string, which dropped
  // the chain and left `err.cause.message` undefined on the one rejection a caller is most likely
  // to branch on — while `src/errors.rs` documents `cause.message` as the uniform rule.
  const server = await startSseServer([[outputFrame(0, 'AA'), gapFrame(2, 900)]]);
  try {
    const handle = await handleAgainst(server);
    const before = [];
    await assert.rejects(
      async () => {
        for await (const event of handle.stream({ errorOnGap: true, idleTimeout: 5 })) {
          before.push(event);
        }
      },
      (error) => {
        assert.equal(codeOf(error), 'ERR_PLATFORM', 'the ERR_ code is on the cause');
        assert.equal(wireKindOf(error), 'OutputGap', 'the wire kind is one level deeper');
        assert.match(error.message, /\[2, 900\)/);
        return true;
      },
    );
    assert.deepEqual(
      before.map((event) => event.kind),
      ['output'],
      'the events before the gap were discarded',
    );
  } finally {
    await server.close();
  }
});

test('a transport failure rejects the iteration rather than ending it silently', async () => {
  // A silent end would read as complete output, which is the failure to avoid. No server: a
  // refused connection is the simplest real transport failure, and it reaches the caller as a
  // rejection carrying the retryable taxonomy — because the exec is still alive server-side and
  // the request can be repeated.
  const handle = await offlineHandle();
  await assert.rejects(
    async () => {
      for await (const _event of handle.stream({ reconnect: false, idleTimeout: 1 })) {
        // Nothing arrives; the attach itself fails.
      }
    },
    (error) => {
      assert.equal(codeOf(error), 'ERR_RETRYABLE');
      assert.equal(wireKindOf(error), 'Transport');
      return true;
    },
  );
});

test('two streams over one handle each get their own events', async () => {
  // Each `stream()` call is a fresh drive with its own task and channel. A shared channel would
  // mean two iterators splitting one event sequence between them — each seeing roughly half the
  // output, with nothing thrown anywhere.
  const server = await startSseServer([
    [outputFrame(0, 'first\n'), exitFrame(6)],
    [outputFrame(0, 'second\n'), exitFrame(7)],
  ]);
  try {
    const handle = await handleAgainst(server);
    const first = await drain(handle.stream({ idleTimeout: 5 }));
    const second = await drain(handle.stream({ idleTimeout: 5 }));

    assert.equal(first.find((event) => event.kind === 'output').text, 'first\n');
    assert.equal(second.find((event) => event.kind === 'output').text, 'second\n');
    assert.equal(server.requestedPaths.length, 2);
  } finally {
    await server.close();
  }
});

test('a stream survives more events than the channel can hold', async () => {
  // The backpressure path, which is the reason core grew an async callback driver. The binding's
  // channel holds **one** event, so a stream of many frames means the driver awaits `send` for all
  // but the first — the case the old synchronous callback could not serve, because its only
  // available send would have parked the runtime thread the driver runs on. Sixty-four frames is
  // comfortably more than the bound; every one arrives, in order, with the terminal event last.
  //
  // Falsification: this is the test that goes red if the drive is ever changed to drop events
  // under a full channel (a `try_send` in place of the awaited `send`) — the count and the offsets
  // both break.
  const frames = [];
  for (let index = 0; index < 64; index += 1) frames.push(outputFrame(index * 2, 'xy'));
  frames.push(exitFrame(128));
  const server = await startSseServer([frames]);
  try {
    const handle = await handleAgainst(server);
    const seen = await drain(handle.stream({ idleTimeout: 5 }));
    const outputs = seen.filter((event) => event.kind === 'output');

    assert.equal(outputs.length, 64, 'events were dropped under backpressure');
    assert.deepEqual(
      outputs.map((event) => event.offset),
      Array.from({ length: 64 }, (_unused, index) => index * 2),
    );
    assert.equal(outputs.map((event) => event.text).join(''), 'xy'.repeat(64));
    assert.equal(seen.at(-1).kind, 'exit');
  } finally {
    await server.close();
  }
});

test('an unknown event name is skipped rather than ending the stream', async () => {
  // Forward compatibility, and the absence of a spurious end-of-stream. A daemon that grows a
  // fourth event type must not truncate this client's output: a frame that decodes to nothing is
  // skipped, so the events either side still arrive — and critically, the unknown frame does not
  // read as the body ending.
  const server = await startSseServer([
    [
      outputFrame(0, 'AA'),
      'event: something-new\ndata: {"whatever":1}\n\n',
      outputFrame(2, 'BB'),
      exitFrame(4),
    ],
  ]);
  try {
    const handle = await handleAgainst(server);
    const seen = await drain(handle.stream({ idleTimeout: 5 }));

    assert.deepEqual(
      seen.map((event) => event.kind),
      ['output', 'output', 'exit'],
    );
    assert.equal(
      seen
        .filter((event) => event.kind === 'output')
        .map((event) => event.text)
        .join(''),
      'AABB',
    );
    assert.equal(server.requestedPaths.length, 1, 'an unknown frame was read as a cut');
  } finally {
    await server.close();
  }
});

test('a stderr chunk reports its own stream in the shared offset space', async () => {
  // One offset space for both streams, which is why a caller holds one cursor. Two cursors could
  // disagree about ordering — and the interleaving of stdout and stderr *is* the information
  // someone reading a build log needs. So `stream` is a label on the chunk, not a separate
  // sequence.
  const server = await startSseServer([
    [outputFrame(0, 'out\n', 'stdout'), outputFrame(4, 'err\n', 'stderr'), exitFrame(8)],
  ]);
  try {
    const handle = await handleAgainst(server);
    const outputs = (await drain(handle.stream({ idleTimeout: 5 }))).filter(
      (event) => event.kind === 'output',
    );

    assert.deepEqual(
      outputs.map((event) => event.stream),
      ['stdout', 'stderr'],
    );
    // Contiguous across the two streams, which is the shared-space claim.
    assert.deepEqual(
      outputs.map((event) => [event.offset, event.end]),
      [
        [0, 4],
        [4, 8],
      ],
    );
  } finally {
    await server.close();
  }
});

// -- the failure taxonomy of an unreachable daemon -----------------------------

test('every single-shot request rejects a refused connection as retryable', async () => {
  // The branch a caller's retry logic reads, checked across the surface rather than once. A refused
  // connection says nothing about the exec — it is what a VM that has just reached RUNNING does for
  // a moment before the proxy path is wired up — so all of these have to be retryable. One
  // reporting it as fatal would make a caller give up on a VM about to come good.
  //
  // `wait` and `waitAndAck` are deliberately not in this list: they *swallow* a retryable failure
  // and keep polling, which is the next test.
  const handle = await offlineHandle();
  const calls = {
    poll: () => handle.poll(),
    ack: () => handle.ack(),
    kill: () => handle.kill(),
    closeStdin: () => handle.closeStdin(),
    writeStdin: () => handle.writeStdin(new Uint8Array([1, 2, 3])),
  };
  for (const [name, call] of Object.entries(calls)) {
    await assert.rejects(
      call,
      (error) => {
        assert.equal(codeOf(error), 'ERR_RETRYABLE', `${name} reported a refusal as fatal`);
        assert.equal(wireKindOf(error), 'Transport', name);
        return true;
      },
      `${name} resolved against a dead endpoint`,
    );
  }
});

test('wait swallows a retryable failure and reports its own timeout instead', async () => {
  // **Measured, and the opposite of what the test above asserts** — which is why it is separate
  // rather than folded in. `wait` polls in a loop, and a dropped connection mid-wait is expected
  // rather than exceptional: a VM under load refuses a connection occasionally, and repeating a
  // read-only poll costs nothing. So a refused connection does not surface at all; the deadline
  // does, as `ERR_TIMEOUT`.
  //
  // That distinction is load-bearing for a caller. `ERR_TIMEOUT` here means "I never got an
  // answer", *not* "the exec failed" — polling is read-only and the output lives until it is
  // acked, so the record is untouched and can be re-polled. A caller that treated this as a
  // command failure would report a failure about a command that may well have succeeded, which is
  // why the message says so and why this asserts that it does.
  const handle = await offlineHandle();
  for (const method of ['wait', 'waitAndAck']) {
    await assert.rejects(
      () => handle[method](1),
      (error) => {
        assert.equal(codeOf(error), 'ERR_TIMEOUT', `${method} surfaced the transport failure`);
        assert.equal(wireKindOf(error), 'ExecTimeout', method);
        // The message has to say the record survived, because that is the caller's next move.
        assert.match(error.message, /re-polled/);
        return true;
      },
      `${method} resolved against a dead endpoint`,
    );
  }
});

test('a rejection names the method and path it was attempting', async () => {
  // The message says *which* request failed, which is what makes a log line actionable. "error
  // sending request" alone leaves a reader unable to tell a poll from an ack.
  const handle = await offlineHandle();
  await assert.rejects(
    () => handle.poll(),
    (error) => {
      assert.match(error.message, /GET/);
      assert.match(error.message, new RegExp(EXEC_ID));
      return true;
    },
  );
});

test('writeStdin takes bytes rather than a string so no encoding is implied', async () => {
  // Same reasoning as the output events: stdin is arbitrary bytes, and a silent UTF-8 encode would
  // corrupt anything that was not text. napi's conversion refuses the string before any Rust runs.
  const handle = await offlineHandle();
  assert.throws(() => handle.writeStdin('text'), 'a string was accepted as stdin bytes');
});
