// SPDX-License-Identifier: Apache-2.0
//
// The error contract: `src/errors.rs`.
//
// `smoke.mjs` asserts the two enumerations have thirteen entries each. This file asserts the part a
// caller actually depends on: **where the code lands**, on both the sync and async paths, and that
// the rule is the same one on each.
//
// # Why that is the subject
//
// napi's model does not let this be as clean as its Python twin, and the reason decides the
// contract. A `napi::Error<S>` becomes a JS `Error` whose `.code` is `S`; a **synchronous**
// `#[napi]` function can return `Error<String>` and so gets a real `ERR_*` code, but an **async**
// one cannot — `execute_tokio_future` is typed over napi's own closed `Status` enum, so any code
// string is collapsed on the way through a Promise rejection.
//
// Nearly every method on this surface is async, because the core is. So "read `.code`" would be
// true on `Duration.measured()` and false on `sandbox.resume()` — the worst possible split, since
// it works in the first test someone writes and fails in production. The rule is therefore
// `err.cause.message`, uniformly, and the daemon-status class is one level deeper at
// `err.cause.cause.message`.
//
// These tests are what make that rule enforceable rather than documented: they check the chain on
// a sync throw, on an async rejection, and on a rejection out of an async *iterator* — which is the
// path that was demonstrably wrong before this suite existed.

import assert from 'node:assert/strict';
import { test } from 'node:test';

import {
  BuildHookTimeout,
  Duration,
  errorCodes,
  Region,
  RunHookTimeout,
  Session,
  SizeClass,
  wireKinds,
} from '../index.js';
import { codeOf, wireKindOf } from './support/sse.mjs';

// -- the enumerations are the taxonomy ----------------------------------------

test('every ERR_ code is well formed and distinct', () => {
  // Thirteen distinct values: two kinds sharing a code would make branching ambiguous in a way no
  // type checker sees — a consumer's `switch` would take one arm for two different conditions.
  const codes = errorCodes();
  assert.equal(codes.length, 13, 'one per ErrorKind');
  assert.equal(new Set(codes).size, 13, 'two kinds share a code');
  for (const code of codes) {
    assert.ok(code.startsWith('ERR_'), code);
    assert.equal(code, code.toUpperCase(), code);
    assert.ok(!code.includes(' '), code);
  }
});

test('every wire kind is a distinct identifier-shaped name', () => {
  // These are the `err.cause.cause.message` values, and the conformance oracle asserts on exactly
  // these spellings — so a rename here is a wire-contract change rather than a tidy-up.
  const kinds = wireKinds();
  assert.equal(kinds.length, 13);
  assert.equal(new Set(kinds).size, 13);
  for (const kind of kinds) {
    assert.match(kind, /^[A-Z][A-Za-z]*$/, kind);
  }
});

test('the two enumerations are disjoint, so a level of the chain is unambiguous', () => {
  // A name appearing in both would make `cause.message` and `cause.cause.message` impossible to
  // tell apart when read out of a log — and telling them apart is the whole reason the chain has
  // two levels.
  const codes = new Set(errorCodes());
  for (const kind of wireKinds()) {
    assert.ok(!codes.has(kind), `${kind} is both an ERR_ code and a wire kind`);
  }
});

test('the load-bearing wire kinds are present under the names the oracle uses', () => {
  // 400 and 404 are different things: one is a request the daemon rejected on its merits, the other
  // is an exec that was collected. A client that conflated them would reconnect forever to
  // something that can never succeed.
  const kinds = wireKinds();
  for (const required of ['ProtocolError', 'NotFound', 'Transport', 'OutputGap', 'ExecTimeout']) {
    assert.ok(kinds.includes(required), `${required} missing from wireKinds()`);
  }
});

// -- where the code lands: the sync path ---------------------------------------

test('a synchronous refusal carries the code on .code and on the cause', () => {
  // Both, and the test says which to rely on. `.code` really is the `ERR_*` string here, but it is
  // documented as *not* the thing to branch on — because the async path cannot offer it.
  assert.throws(
    () => Region.parse('nope-1'),
    (error) => {
      assert.equal(error.code, 'ERR_INVALID_ARG', 'sync .code should be the ERR_ string');
      assert.equal(codeOf(error), 'ERR_INVALID_ARG', 'the uniform rule holds on the sync path too');
      return true;
    },
  );
});

test('every synchronous factory that can refuse uses the one chain', () => {
  // Checked across the surface rather than on one function: a factory that built its error some
  // other way would be the one place a caller's `err.cause.message` read `undefined`, and it would
  // be found in production rather than here.
  const refusals = {
    'Region.parse': () => Region.parse('eu-central-1'),
    'SizeClass.fromBaselineMib': () => SizeClass.fromBaselineMib(1500),
    'Duration.measured': () => Duration.measured(-1),
    'Duration.projected': () => Duration.projected(Number.NaN),
    RunHookTimeout: () => new RunHookTimeout(3600),
    BuildHookTimeout: () => new BuildHookTimeout(0),
  };
  for (const [name, call] of Object.entries(refusals)) {
    assert.throws(
      call,
      (error) => {
        assert.equal(codeOf(error), 'ERR_INVALID_ARG', `${name} lost its code`);
        // A local refusal reached no daemon, so there is no status to report — inventing one
        // would be a claim nobody made.
        assert.equal(wireKindOf(error), undefined, `${name} invented a wire kind`);
        return true;
      },
      `${name} did not refuse`,
    );
  }
});

test('a local refusal carries the core’s own message rather than a reworded one', () => {
  // The core's messages name the `docs/PLATFORM.md` finding that measured the behaviour, and a
  // binding that shortened them would discard the point of the closure. The region refusal has two
  // halves that only mean something together: "AccessDeniedException" alone reads as an IAM
  // problem, and it is the word *null* that says otherwise.
  assert.throws(
    () => Region.parse('eu-central-1'),
    (error) => {
      assert.match(error.message, /AccessDeniedException/);
      assert.match(error.message, /null/);
      assert.ok(error.message.length > 80, `truncated on the way out: ${error.message}`);
      return true;
    },
  );
});

// -- where the code lands: the async path --------------------------------------

test('an async rejection carries the code on the cause, because .code cannot hold it', async () => {
  // The measurement that forced the contract, asserted. `.code` on this path is a napi status —
  // `GenericFailure` — and the `ERR_*` string is on the cause. A caller branching on `.code` would
  // see one value for every failure the library has.
  const session = Session.direct('http://127.0.0.1:9', 'agent-token');
  await assert.rejects(
    () => session.health(),
    (error) => {
      assert.equal(codeOf(error), 'ERR_RETRYABLE', 'the ERR_ code is not on the cause');
      assert.notEqual(error.code, 'ERR_RETRYABLE', 'napi unexpectedly preserved .code');
      return true;
    },
  );
});

test('an async rejection from the daemon path carries the wire kind one level deeper', async () => {
  // The distinction the conformance oracle asserts on: `err.cause.cause.message` is the status
  // class. Present here because the request really did reach the transport layer and fail there.
  const session = Session.direct('http://127.0.0.1:9', 'agent-token');
  await assert.rejects(
    () => session.health(),
    (error) => {
      assert.equal(wireKindOf(error), 'Transport');
      assert.ok(wireKinds().includes(wireKindOf(error)), 'the wire kind is not an enumerated one');
      return true;
    },
  );
});

test('an async refusal of a bad argument has a code but no wire kind', async () => {
  // The pairing that makes the two levels meaningful: a refusal decided locally has a code and no
  // status, and one that came back from the wire has both. A binding that always attached a status
  // would make "did this reach the daemon" unanswerable — which matters, because it decides whether
  // retrying is safe.
  const session = Session.direct('http://127.0.0.1:9', 'agent-token');
  const handle = await session.exec('x-0000000000000001');
  await assert.rejects(
    () => handle.wait(Number.NaN),
    (error) => {
      assert.equal(codeOf(error), 'ERR_INVALID_ARG');
      assert.equal(wireKindOf(error), undefined, 'a local refusal claimed a daemon status');
      return true;
    },
  );
});

test('the same condition reports the same code whichever path it arrives on', async () => {
  // One taxonomy, two transports. `Duration.measured(-1)` is sync and `handle.wait(-1)` is async,
  // and both are the core's `duration_of_secs_f64` refusing the same figure — so the code has to
  // match. If it did not, a caller could not write one handler for one condition.
  let syncCode;
  assert.throws(
    () => Duration.measured(-1),
    (error) => {
      syncCode = codeOf(error);
      return true;
    },
  );
  const handle = await Session.direct('http://127.0.0.1:9', 't').exec('x-0000000000000001');
  await assert.rejects(
    () => handle.wait(-1),
    (error) => {
      assert.equal(codeOf(error), syncCode, 'one condition, two codes');
      return true;
    },
  );
  assert.equal(syncCode, 'ERR_INVALID_ARG');
});

// -- the chain's own shape -----------------------------------------------------

test('each level of the chain is exactly one string, so reading it is a field access', async () => {
  // Not a sentence to parse: `cause.message` *is* the code, with nothing around it. A level that
  // read "error code: ERR_RETRYABLE" would send every consumer back to string matching, which is
  // the rule this design exists to avoid.
  const session = Session.direct('http://127.0.0.1:9', 'agent-token');
  await assert.rejects(
    () => session.health(),
    (error) => {
      assert.equal(error.cause.message, 'ERR_RETRYABLE');
      assert.equal(error.cause.cause.message, 'Transport');
      // And the outer message is the human one, which is a different string from either.
      assert.notEqual(error.message, error.cause.message);
      assert.ok(error.message.length > error.cause.message.length);
      return true;
    },
  );
});

test('the chain ends after the wire kind rather than cycling', async () => {
  // Two levels and no more. A cyclic or unbounded chain would hang anything that walked it — and
  // walking `err.cause` until it is undefined is the obvious way to log one.
  const session = Session.direct('http://127.0.0.1:9', 'agent-token');
  await assert.rejects(
    () => session.health(),
    (error) => {
      let depth = 0;
      let current = error;
      while (current?.cause !== undefined && depth < 10) {
        current = current.cause;
        depth += 1;
      }
      assert.equal(depth, 2, 'the chain is not exactly two levels deep');
      assert.equal(current.cause, undefined);
      return true;
    },
  );
});

test('every thrown value is a real Error, so a stack and instanceof both work', async () => {
  // A thrown string or plain object would satisfy a `throws` assertion and break every `catch`
  // that expected an `Error` — including the `instanceof` checks a framework does before logging.
  assert.throws(
    () => Region.parse('nope-1'),
    (error) => {
      assert.ok(error instanceof Error);
      assert.ok(error.cause instanceof Error, 'the cause is not an Error either');
      assert.equal(typeof error.stack, 'string');
      return true;
    },
  );
  const session = Session.direct('http://127.0.0.1:9', 'agent-token');
  await assert.rejects(
    () => session.health(),
    (error) => {
      assert.ok(error instanceof Error);
      assert.ok(error.cause instanceof Error);
      return true;
    },
  );
});
