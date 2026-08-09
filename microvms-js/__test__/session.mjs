// SPDX-License-Identifier: Apache-2.0
//
// The session surface (`src/session.rs`), the region closed set (`src/region.rs`), and the two hook
// timeout families (`src/hooks.rs`).
//
// # What a unit run can say about a session
//
// Constructing one does not talk to the VM — deliberately, because a constructor that probed would
// make "do I have a session" mean "is the VM up", and those are different questions with different
// answers during a launch. So `Session.direct(...)` is fully testable offline, and so is the
// *argument* half of every method on it.
//
// What a unit run cannot say is whether a `run` starts a process, because that needs the daemon. So
// three things are asserted and no more: the **construction** contract, the **argument** contract
// (which shapes napi accepts before any Rust runs, and which it refuses), and the **failure
// taxonomy** of a request that cannot connect — because a caller's retry logic branches on it, and
// getting that wrong turns a dead VM into an infinite loop.
//
// A `127.0.0.1:9` endpoint appears throughout rather than a mock: a refused connection is a real
// transport failure with a real answer, reachable without inventing a fake daemon whose agreement
// with the real one nobody would be testing.

import assert from 'node:assert/strict';
import { test } from 'node:test';

import {
  BuildHookTimeout,
  Region,
  RunHookTimeout,
  Session,
  sessionConstants,
} from '../index.js';
import { codeOf, wireKindOf } from './support/sse.mjs';

const SUPPORTED = ['us-east-1', 'us-east-2', 'us-west-2', 'eu-west-1', 'ap-northeast-1'];

/** A session against an endpoint nothing is listening on. */
function offline() {
  return Session.direct('http://127.0.0.1:9', 'agent-token');
}

// -- construction --------------------------------------------------------------

test('a direct session reports its endpoint and the default port', async () => {
  // The conformance shape: no proxy headers, no control plane, no credentials. `direct` is a
  // supported path rather than a test hatch — it is what a caller inside the VM or on a tunnel uses
  // — so it has to work with nothing configured.
  const session = Session.direct('http://127.0.0.1:9000', 'agent-token');
  assert.equal(await session.endpoint(), 'http://127.0.0.1:9000');
  assert.equal(await session.port(), JSON.parse(sessionConstants()).defaultAgentPort);
});

test('a direct session has no minter, so nothing mints', async () => {
  // `null` and not zero, and the difference is the point. Zero would say "this session mints and has
  // not yet"; `null` says "this session does not mint". A direct session sends no proxy headers at
  // all, and a monitor watching for a stale token (STATE-8) must not read a direct session as one
  // that never refreshed.
  assert.equal(await offline().proxyMintCount(), null);
});

test('there is no constructor, so a session comes from direct or from a sandbox', () => {
  // Two doors, both named. A `new Session(...)` would be a third, and the thing it would most
  // plausibly take is a proxy token — which is exactly what TRAP-9 says a caller must not hand in,
  // because minting happens inside every request and a token passed in is one that expires mid-run.
  assert.throws(() => new Session('http://127.0.0.1:9000', 'token'));
  assert.throws(() => new Session());
});

test('no proxy token is reachable anywhere on the session surface', async () => {
  // TRAP-7 by absence: there is nothing to treat as a string. The core's `ProxyToken` has no
  // `Display`, no `as_str`, and no `Deref`, and the binding adds no accessor — so "log the auth
  // token" is as inexpressible here as it is in Rust.
  const session = offline();
  for (const name of ['proxyToken', 'token', 'authToken', 'agentToken', 'proxyAuth', 'headers']) {
    assert.equal(session[name], undefined, `${name} is reachable`);
  }
  // The one observable that *is* exposed is a count, which carries no secret.
  assert.equal(await session.proxyMintCount(), null);
});

// -- the command contract ------------------------------------------------------

test('a non-command throws synchronously, before any request is built', () => {
  // `throws` and not `rejects`, and the difference is the point: napi's argument conversion runs
  // **before** the future is built, so a bad command is a synchronous error rather than a rejected
  // Promise. That is strictly stronger — a caller who forgot to `await` still sees it, and no
  // request was ever built.
  const session = offline();
  for (const bad of [3, 3.5, null, true, { cmd: 'ls' }, [1, 2]]) {
    assert.throws(
      () => session.run(bad),
      `run(${JSON.stringify(bad)}) was accepted`,
    );
    assert.throws(
      () => session.runSync(bad),
      `runSync(${JSON.stringify(bad)}) was accepted`,
    );
  }
});

test('both command spellings are accepted and neither is whitespace split', async () => {
  // `run("ls -la")` is a **one-element** argv, which is `session.py`'s own rule: splitting on spaces
  // is how `/opt/my app/bin/tool` becomes two arguments nobody meant. There is no daemon to read the
  // built request back from, so what is asserted is the reachable half — both spellings get past the
  // conversion and fail on the *wire* rather than on the type, which is what says the argument
  // itself was accepted.
  const session = offline();
  for (const command of ['ls -la', ['ls', '-la']]) {
    await assert.rejects(
      () => session.run(command),
      (error) => {
        assert.equal(wireKindOf(error), 'Transport', 'the command was refused rather than sent');
        return true;
      },
    );
  }
});

test('an empty argv reaches the daemon rather than being refused locally', async () => {
  // BIND-2: a check here would be the copy nothing else tests. An empty argv is a real mistake and
  // the daemon is what refuses it — the core has no local guard, so neither does the binding.
  // Documented as a test because "why is this not validated" is the obvious question, and the answer
  // is that the refusal belongs in one place.
  await assert.rejects(
    () => offline().run([]),
    (error) => {
      assert.equal(wireKindOf(error), 'Transport', 'an empty argv was refused locally');
      return true;
    },
  );
});

test('exec options are a named bag, so no two can be transposed', async () => {
  // `user` and `group` are both plausible integers, and positionally they transpose silently. An
  // options object makes that unwriteable, and every documented name really is read.
  await assert.rejects(
    () =>
      offline().run(['ls'], {
        shell: false,
        cwd: '/tmp',
        env: { KEY: 'value' },
        user: 1000,
        group: 1000,
        timeoutSec: 30,
        stdin: true,
        execId: 'x-0000000000000009',
      }),
    (error) => {
      // Past the conversion: every field was accepted and the failure is the wire.
      assert.equal(wireKindOf(error), 'Transport');
      return true;
    },
  );
});

test('a supplied exec id is the idempotency key and comes back on the handle', async () => {
  // What a caller whose retry must be safe across its own restart passes. Asserted through the
  // reattach path, which needs no daemon: the handle carries the id it was given, so a second
  // process with the same id addresses the same server-side exec.
  const handle = await offline().exec('x-00000000000000ff');
  assert.equal(handle.execId, 'x-00000000000000ff');
});

test('the octal mode is a string because a number would be ambiguous', async () => {
  // `"0755"` and not `0o755` or `755`. A number cannot distinguish the two readings, and they
  // differ: `755` decimal is not a mode anyone means. A string is the daemon's own shape, so nothing
  // here converts.
  const session = offline();
  assert.throws(() => session.uploadFile('/tmp/x', new Uint8Array([1]), 0o755));
  await assert.rejects(
    () => session.uploadFile('/tmp/x', new Uint8Array([1]), '0755'),
    (error) => {
      assert.equal(wireKindOf(error), 'Transport');
      return true;
    },
  );
});

test('file transfer takes bytes rather than a string, so no encoding is implied', () => {
  // An upload is bytes; a string would need an encoding this layer must not pick. Same reasoning as
  // the output events — the file's contents are whatever they are, and a silent UTF-8 encode would
  // corrupt anything that was not text.
  const session = offline();
  assert.throws(() => session.uploadFile('/tmp/x', 'text'));
  assert.throws(() => session.uploadTar('/tmp', 'not bytes'));
});

// -- the failure taxonomy of an unreachable daemon -----------------------------

test('every single-shot request rejects a refused connection as retryable', async () => {
  // The branch a caller's retry logic reads, checked across the surface rather than once. A refused
  // connection says nothing about the VM — it is exactly what a VM that has just reached RUNNING
  // does for a moment before the proxy path is wired up — so every one of these has to be retryable.
  // One reporting it as fatal would make a caller give up on a VM about to come good; one reporting
  // a genuine 401 as retryable would loop until the deadline. The pair is why the taxonomy exists.
  const session = offline();
  const calls = {
    health: () => session.health(),
    run: () => session.run(['true']),
    runSync: () => session.runSync(['true']),
    kill: () => session.kill('x-0000000000000001'),
    fileExists: () => session.fileExists('/tmp/x'),
    downloadFile: () => session.downloadFile('/tmp/x'),
    downloadTar: () => session.downloadTar('/tmp'),
    uploadFile: () => session.uploadFile('/tmp/x', new Uint8Array([1])),
    uploadTar: () => session.uploadTar('/tmp', new Uint8Array([1])),
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

test('waitUntilReady swallows the refusal and reports its own deadline instead', async () => {
  // **The deliberate exception to the rule above**, which is why it is a separate test. A VM that
  // has just reached RUNNING commonly refuses a connection or two before the proxy path is wired up,
  // so a connection error on the way to bootstrap is *expected* rather than exceptional and the poll
  // loop keeps going. What surfaces is the deadline.
  //
  // The other half of that design — a *fatal* error ending the wait at once — needs a daemon
  // answering 401 and so belongs to the conformance suite. Retrying a 401 until the deadline is the
  // mistake the retryable split exists to prevent, and this test covers only the retryable side.
  await assert.rejects(
    () => offline().waitUntilReady(1),
    (error) => {
      assert.equal(codeOf(error), 'ERR_TIMEOUT', 'the transport failure surfaced instead');
      return true;
    },
  );
});

test('a rejection names the method and path it was attempting', async () => {
  // The message says *which* request failed, which is what makes a log line actionable. "error
  // sending request" alone leaves a reader unable to tell a health probe from a file download — and
  // during a launch those mean quite different things.
  await assert.rejects(
    () => offline().health(),
    (error) => {
      assert.match(error.message, /GET/);
      assert.match(error.message, /\/v1\/health/);
      return true;
    },
  );
});

test('a non-finite ready timeout is refused before the poll loop starts', async () => {
  // `NaN` would compare false against every deadline, so the loop would poll forever — a hang rather
  // than an error. Refused by the core's `duration_of_secs_f64`, not by a check here.
  for (const bad of [-1, Number.NaN, Number.POSITIVE_INFINITY]) {
    await assert.rejects(
      () => offline().waitUntilReady(bad),
      (error) => {
        assert.equal(codeOf(error), 'ERR_INVALID_ARG');
        assert.equal(wireKindOf(error), undefined, 'a local refusal claimed a daemon status');
        return true;
      },
      `waitUntilReady(${bad}) was accepted`,
    );
  }
});

// -- the region closed set (`src/region.rs`) -----------------------------------

test('each named factory answers its own region and reports supported', () => {
  // Five factories, five distinct regions, no two aliased. Aliasing is the mistake worth checking:
  // `usEast2()` returning `UsEast1` would send every call to the wrong region's endpoint and nothing
  // local would object.
  const built = [
    ['us-east-1', Region.usEast1()],
    ['us-east-2', Region.usEast2()],
    ['us-west-2', Region.usWest2()],
    ['eu-west-1', Region.euWest1()],
    ['ap-northeast-1', Region.apNortheast1()],
  ];
  assert.deepEqual(
    built.map(([name]) => name),
    SUPPORTED,
  );
  for (const [name, region] of built) {
    assert.equal(region.name, name);
    assert.equal(region.isSupported, true);
    assert.equal(region.toString(), name);
  }
});

test('supported is exactly the five named factories', () => {
  // One list, reachable two ways, and they have to agree. A region present in one and absent from
  // the other is a region that is either unreachable or undocumented.
  assert.deepEqual(
    Region.supported().map((region) => region.name),
    SUPPORTED,
  );
  assert.ok(Region.supported().every((region) => region.isSupported));
});

test('parse accepts every supported name and round trips it', () => {
  // The positive half of the refusal, so the guard is not a blanket no. `parse` is the boundary a
  // region name arrives at from an environment variable or a config file, and a refusal that also
  // rejected the good names would be worse than no check.
  for (const name of SUPPORTED) {
    const parsed = Region.parse(name);
    assert.equal(parsed.name, name);
    assert.equal(parsed.isSupported, true);
    // Round trip through the string form, which is what a config file round trip looks like.
    assert.ok(Region.parse(parsed.toString()).equals(parsed));
  }
});

test('parse refuses everything outside the five without normalising', () => {
  // Exact matching, deliberately. A `parse` that trimmed or case-folded would accept two spellings
  // of one region, and whichever consumer keyed on the raw string would split the group. More to
  // the point: the whole value of this refusal is that it is the *only* warning before the
  // null-message denial, and a normalising parse quietly widens the set it guards.
  const spellings = [
    'eu-central-1', // on the list until 2026-08-07, and does not carry MicroVMs
    'us-east-3',
    'US-EAST-1',
    'us-east-1 ',
    ' us-east-1',
    'useast1',
    'us_east_1',
    '',
    'local',
  ];
  for (const name of spellings) {
    assert.throws(
      () => Region.parse(name),
      (error) => {
        assert.equal(codeOf(error), 'ERR_INVALID_ARG');
        assert.equal(wireKindOf(error), undefined, 'nothing reached the daemon');
        return true;
      },
      `Region.parse(${JSON.stringify(name)}) was accepted`,
    );
  }
});

test('the refusal names the null-message trap and offers the supported set', () => {
  // "AccessDeniedException" alone reads as an IAM problem — someone would spend an hour reading a
  // policy that is correct — and it is the word *null* that says otherwise. The five names have to
  // be there too, or the refusal tells a caller they are wrong without telling them what is right.
  assert.throws(
    () => Region.parse('eu-central-1'),
    (error) => {
      assert.match(error.message, /AccessDeniedException/);
      assert.match(error.message, /null/);
      for (const name of SUPPORTED) {
        assert.ok(error.message.includes(name), `${name} missing: ${error.message}`);
      }
      return true;
    },
  );
});

test('unlisted normalises a supported name back to the real region', () => {
  // One region, one value, however it was reached. Without this, `unlisted("us-east-1")` would be a
  // *second* value for a supported region — equal to nothing and reporting `isSupported` false
  // about a region that is — so anything keyed by region would hold two entries for one place.
  for (const name of SUPPORTED) {
    const hatched = Region.unlisted(name);
    assert.ok(hatched.equals(Region.parse(name)));
    assert.equal(hatched.isSupported, true);
  }
});

test('unlisted accepts a name parse refuses, which is the whole point', () => {
  // The two doors differ, and in exactly one direction. `parse` is for a name that must be checked;
  // `unlisted` is for a name someone chose anyway. If `unlisted` refused too there would be no
  // hatch, and if `parse` accepted there would be no guard.
  assert.throws(() => Region.parse('me-south-1'));
  const hatched = Region.unlisted('me-south-1');
  assert.equal(hatched.name, 'me-south-1');
  assert.equal(hatched.isSupported, false);
});

test('an unlisted name is carried verbatim, including a spelling parse would reject', () => {
  // No normalising on the way through, because the caller may know something this client does not.
  // A name is what goes into the endpoint's middle segment, so altering it would address a different
  // region than the one asked for — and the reason to use this door is that the client's list is out
  // of date rather than the caller's.
  for (const name of ['EU-CENTRAL-1', 'eu-central-1', 'some-future-region-9']) {
    assert.equal(Region.unlisted(name).name, name);
  }
});

test('equals compares regions by value where === compares by reference', () => {
  // A method and not `===`, because JS compares class instances by reference: two `Region.usEast1()`
  // calls are different objects. `equals` is what a JS reader reaches for, and both halves are
  // asserted so it is not a constant.
  assert.notEqual(Region.usEast1(), Region.usEast1(), 'instances are unexpectedly interned');
  assert.ok(Region.usEast1().equals(Region.usEast1()));
  assert.ok(!Region.usEast1().equals(Region.usWest2()));
  // Every pair of distinct regions is unequal, so `equals` is not "always true".
  const regions = Region.supported();
  for (let i = 0; i < regions.length; i += 1) {
    for (let j = i + 1; j < regions.length; j += 1) {
      assert.ok(!regions[i].equals(regions[j]), `${regions[i].name} == ${regions[j].name}`);
    }
  }
});

test('an unlisted region is not equal to a supported one', () => {
  // `isSupported` is part of the identity, not a label bolted on. Two values spelling the same name
  // but disagreeing about support would make it arbitrary which one a consumer held — and that
  // decides whether they get the warning.
  const unlisted = Region.unlisted('eu-central-1');
  for (const supported of Region.supported()) {
    assert.ok(!unlisted.equals(supported));
  }
});

test('Region has no constructor, so an unchecked name is not writable', () => {
  // With one, `new Region("eu-central-1")` would be the shortest path in the module and would bypass
  // both doors — the check and the visible opt-in — at once.
  assert.throws(() => new Region('us-east-1'));
  assert.throws(() => new Region());
});

// -- the hook timeouts (`src/hooks.rs`) ----------------------------------------

test('the two hook families have the ceilings the service documents', () => {
  // 60 and 3600, sixty times apart — which is why they are two types.
  assert.equal(new RunHookTimeout(1).maxSecs, 60);
  assert.equal(new BuildHookTimeout(1).maxSecs, 3600);
});

test('each family accepts its whole documented range including the boundary', () => {
  // An off-by-one at the ceiling would refuse a legal value, and 3600 being legal for one family and
  // refused for the other is the asymmetry that makes them two types.
  for (const seconds of [1, 30, 60]) {
    assert.equal(new RunHookTimeout(seconds).seconds, seconds);
  }
  for (const seconds of [1, 60, 3600]) {
    assert.equal(new BuildHookTimeout(seconds).seconds, seconds);
  }
});

test('each family refuses zero and everything above its own ceiling', () => {
  // Zero as well as the overshoots: a zero-second hook cannot complete. 3600 is in the run family's
  // list on purpose — it is the *build* family's ceiling, so it is the number someone reaches for
  // after reading the other type's documentation.
  for (const seconds of [0, 61, 3600, 100000]) {
    assert.throws(
      () => new RunHookTimeout(seconds),
      (error) => {
        assert.equal(codeOf(error), 'ERR_INVALID_ARG');
        return true;
      },
      `RunHookTimeout(${seconds}) was accepted`,
    );
  }
  for (const seconds of [0, 3601, 100000]) {
    assert.throws(() => new BuildHookTimeout(seconds), `BuildHookTimeout(${seconds}) was accepted`);
  }
});

test('the refusal names both ceilings because the caller picked the other one', () => {
  // Telling someone "the limit is 60" answers a question they did not ask. A caller who passes 3600
  // to the run family is nearly always someone who read the build family's limit, so the message
  // names both — which turns the refusal into an instruction.
  assert.throws(
    () => new RunHookTimeout(3600),
    (error) => {
      assert.match(error.message, /60/);
      assert.match(error.message, /3600/);
      return true;
    },
  );
});

test('the two timeouts are nominal rather than structural, so neither substitutes', () => {
  // The `#[napi]`-class-versus-`#[napi(object)]` decision, asserted. Separate prototypes mean napi
  // extracts them by identity, so passing one where the other is wanted is refused before any Rust
  // runs — and an object literal of the same shape is not a timeout at all.
  const run = new RunHookTimeout(30);
  const build = new BuildHookTimeout(30);
  assert.notEqual(
    Object.getPrototypeOf(run),
    Object.getPrototypeOf(build),
    'the two timeouts share a prototype, so they are interchangeable',
  );
  assert.ok(!(run instanceof BuildHookTimeout));
  assert.ok(!(build instanceof RunHookTimeout));
  // Same seconds, different types — which is exactly the pair a structural check would confuse.
  assert.equal(run.seconds, build.seconds);
  assert.notEqual(run.maxSecs, build.maxSecs);
});

// -- the session constants -----------------------------------------------------

test('both proxy headers are published, because one without the other is rejected', () => {
  // TRAP-7: they go out together or the request is refused indistinguishably. Published so a
  // harness asserting against the wire contract does not hardcode a spelling, and checked as a pair
  // because sending one is the failure mode.
  const constants = JSON.parse(sessionConstants());
  assert.match(constants.proxyAuthHeader, /proxy-auth/i);
  assert.match(constants.proxyPortHeader, /proxy-port/i);
  assert.notEqual(constants.proxyAuthHeader, constants.proxyPortHeader);
});

test('the refresh window is inside the token lifetime with room to spare', () => {
  // A long run crosses the sixty-minute ceiling mid-flight, so the refresh has to precede it. A
  // window at or past the lifetime would mint a replacement only after the old token had already
  // expired — a 401 in the middle of a working stream. Asserted as an inequality with margin, so the
  // relationship is what is under test.
  const { maxTokenLifetimeSeconds, defaultRefreshAfterSeconds } = JSON.parse(sessionConstants());
  assert.equal(maxTokenLifetimeSeconds, 3600);
  assert.ok(defaultRefreshAfterSeconds > 0);
  assert.ok(defaultRefreshAfterSeconds < maxTokenLifetimeSeconds);
  // Half the lifetime of headroom, so one missed refresh is survivable.
  assert.ok(maxTokenLifetimeSeconds - defaultRefreshAfterSeconds >= maxTokenLifetimeSeconds / 2);
});

test('the default agent port is the one a direct session lands on', async () => {
  // One number, reachable two ways. A published constant disagreeing with the session's own default
  // would send a harness validating the contract to a different port than the client uses.
  const session = Session.direct('http://127.0.0.1:9000', 'agent-token');
  assert.equal(await session.port(), JSON.parse(sessionConstants()).defaultAgentPort);
});

test('the phase and stream vocabularies are closed and published', () => {
  // The two sets a caller branches on, published so nothing hardcodes a spelling — and they are the
  // same words the exec events use, because the two have to share one vocabulary.
  const constants = JSON.parse(sessionConstants());
  assert.deepEqual(constants.phases, ['running', 'exited', 'acked']);
  assert.deepEqual(constants.streamKinds, ['stdout', 'stderr']);
});
