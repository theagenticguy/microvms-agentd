// SPDX-License-Identifier: Apache-2.0
// Smoke tests for the napi-rs binding, run with `node --test __test__`.
//
// Every test here is a guard proof for BIND-2 or BIND-5, not a coverage exercise. The rule
// each one checks is "the mistake is not expressible, or the core refuses it" — so a test
// that merely called a method and got a value back would prove nothing this file exists to
// prove.
//
// JS needs *more* of these than Python does, not fewer, because it coerces far more eagerly:
// `usd * 2`, `+usd`, `usd > 1`, and `JSON.stringify(usd)` are four separate doors a `valueOf`
// or a `toJSON` would open, and each is asserted below.
//
// Nothing here talks to AWS. Build first:
//
//     npx @napi-rs/cli build --manifest-path ../Cargo.toml --package microvms-js \
//         --platform --output-dir .
//     node --test __test__

import assert from 'node:assert/strict';
import { test } from 'node:test';

import {
  BuildHookTimeout,
  buildUnpricedReason,
  compareResidency,
  costConstants,
  coreVersion,
  Duration,
  errorCodes,
  estimateRun,
  RateTable,
  Region,
  runReport,
  RunHookTimeout,
  Session,
  sessionConstants,
  SizeClass,
  wireKinds,
} from '../index.js';

// -- helpers ------------------------------------------------------------------

/** A report with one priced phase and one unpriced one.
 *
 * `imageGb` is what makes the build line appear, so this report is deliberately *incomplete*
 * — which is what the `Total` assertions need.
 */
function report() {
  return runReport(SizeClass.defaultClass(), {
    running: Duration.measured(3600),
    imageGb: 2,
    label: 'smoke',
  });
}

/** The `ERR_*` code off a thrown error.
 *
 * `cause.message` and not `.code`: napi's async rejection path is typed over its own closed
 * `Status` enum, so a custom code survives a synchronous throw and is collapsed on a Promise
 * rejection. The cause's message is the code on every path — see `src/errors.rs`.
 */
function codeOf(error) {
  return error.cause?.message;
}

// -- BIND-5: EstimatedUsd has no numeric door ---------------------------------

test('EstimatedUsd does not coerce to a number', () => {
  const usd = report().total.floor;
  // Four doors, all shut. Each is a separate spelling someone reaches for, and a `valueOf`
  // would open all four at once — which is why they are asserted separately rather than
  // trusted to one check.
  assert.ok(Number.isNaN(Number(usd)), 'Number(usd) produced a number');
  assert.ok(Number.isNaN(+usd), 'unary plus produced a number');
  assert.ok(Number.isNaN(usd * 2), 'multiplication produced a number');
  assert.ok(Number.isNaN(usd - 0), 'subtraction produced a number');
});

test('EstimatedUsd does not serialize as a bare number', () => {
  const usd = report().total.floor;
  // With a `toJSON` this would be `0.02` or similar — a dollar figure in a JSON payload with
  // no label, which is exactly the laundering the type exists to prevent.
  const serialized = JSON.stringify(usd);
  assert.ok(
    !/^-?[0-9]/.test(serialized ?? ''),
    `JSON.stringify produced a bare number: ${serialized}`,
  );
});

test('EstimatedUsd has no addition', () => {
  const usd = report().total.floor;
  // JS `+` on two objects concatenates their string forms rather than throwing, so the
  // assertion is that the result is *not* a number — a real `add` method would be the
  // spelling to check for, and there is none.
  assert.equal(typeof usd.add, 'undefined', 'an add method exists');
  assert.ok(Number.isNaN(Number(usd + usd)), 'usd + usd produced a number');
});

test('the amount is an exact string a caller converts deliberately', () => {
  const usd = report().total.floor;
  assert.equal(typeof usd.amount, 'string');
  assert.equal(typeof usd.displayAmount, 'string');
  // Exact: the string is what survives into a decimal library, where a JS double would
  // already have lost the precision the rates carry.
  assert.ok(/^[0-9]+\.[0-9]+$/.test(usd.amount), usd.amount);
  assert.match(usd.toString(), /estimated/);
});

// -- BIND-5: provenance cannot be omitted -------------------------------------

test('Duration has no constructor, for any argument shape', () => {
  // Every plausible spelling, not just the zero-argument one. A test that checked only
  // `new Duration()` would stay green against a two-parameter constructor taking a
  // provenance string — the exact defect the Python twin's first version had.
  for (const args of [[], [3600], [3600, 'measured'], [3600, 'projected'], [3600, 'typo']]) {
    assert.throws(
      () => new Duration(...args),
      `new Duration(${args.join(', ')}) constructed an unlabelled duration`,
    );
  }
});

test('the two named factories are the only doors and they label', () => {
  const measured = Duration.measured(3600);
  const projected = Duration.projected(3600);
  assert.equal(measured.provenance, 'measured');
  assert.equal(projected.provenance, 'projected');
  assert.equal(measured.isMeasured, true);
  assert.equal(projected.isMeasured, false);
  assert.equal(measured.seconds, 3600);
});

test('a negative or non-finite duration is refused by the core', () => {
  // JS makes this matter more than Python does: `NaN` and `Infinity` are ordinary values a
  // caller reaches by accident, e.g. from `Number(undefined)` or a division.
  for (const bad of [-1, Number.NaN, Number.POSITIVE_INFINITY]) {
    assert.throws(
      () => Duration.measured(bad),
      (error) => {
        assert.equal(codeOf(error), 'ERR_INVALID_ARG');
        return true;
      },
      `Duration.measured(${bad}) was accepted`,
    );
  }
  // And the message is the core's, so a reader learns *why* an inverted clock matters.
  assert.throws(() => Duration.measured(-1), /credit/);
});

test('a plan has no way to pass a measured duration', () => {
  // COST-10: `estimateRun` takes seconds, so there is no field an accidentally-measured
  // duration could be written into.
  const plan = estimateRun(SizeClass.defaultClass(), { runningSeconds: 3600, label: 'plan' });
  assert.equal(plan.fullyMeasured, false);
  for (const item of plan.items) {
    if (item.duration !== null) {
      assert.equal(item.duration.provenance, 'projected');
    }
  }
});

// -- BIND-5: Unpriced is a distinct value -------------------------------------

test('the build line is unpriced rather than zero dollars', () => {
  const build = report().items.filter((item) => item.phase === 'image-build');
  assert.equal(build.length, 1);
  const amount = build[0].amount;
  assert.equal(amount.kind, 'unpriced');
  // `null` rather than a zero: a zero gets summed by anything permissive, and that is the one
  // arithmetic the cost module exists to not enable.
  assert.equal(amount.usd, null);
  assert.notEqual(amount.unpriced, null);
  assert.equal(amount.unpriced.reason, buildUnpricedReason());
});

test('an unpriced line omits the usd key entirely', () => {
  // `cli.py`'s own rule. Not a null — a null is summed as zero.
  const build = report().items.filter((item) => item.phase === 'image-build')[0];
  const parsed = JSON.parse(build.toJson());
  assert.equal(parsed.amount.kind, 'unpriced');
  assert.ok(!('usd' in parsed.amount), JSON.stringify(parsed.amount));
  assert.ok('reason' in parsed.amount);
});

test('a total over an unpriced line is a lower bound carrying its reasons', () => {
  const total = report().total;
  assert.equal(total.isLowerBound, true);
  assert.ok(total.unpricedReasons.length > 0, 'a lower bound that will not say what it misses');
  assert.match(total.toString(), /^at least/);

  // The other variant, so the flag is not vacuously true.
  const complete = runReport(SizeClass.defaultClass(), {
    running: Duration.measured(60),
    launched: false,
    label: 'priced only',
  });
  assert.equal(complete.total.isLowerBound, false);
  assert.deepEqual(complete.total.unpricedReasons, []);
  assert.equal(complete.complete, true);
});

test('the report JSON shape is the Python client’s', () => {
  const parsed = JSON.parse(report().toJson());
  assert.equal(parsed.estimated, true);
  assert.deepEqual(Object.keys(parsed).sort(), [
    'complete',
    'estimated',
    'fullyMeasured',
    'items',
    'label',
    'rates',
    'size',
    'staleness',
    'total',
  ]);
  assert.deepEqual(Object.keys(parsed.size).sort(), [
    'baselineMib',
    'baselineVcpu',
    'describe',
    'peakMib',
    'peakVcpu',
  ]);
  assert.deepEqual(Object.keys(parsed.rates).sort(), ['region', 'retrieved', 'sourceUrl']);
  assert.deepEqual(Object.keys(parsed.total).sort(), ['isLowerBound', 'priced', 'render']);
  // Strings, not numbers: the exactness is the point.
  assert.equal(typeof parsed.total.priced, 'string');
  assert.equal(typeof parsed.items[0].quantity, 'string');
});

// -- BIND-2 / TRAP-6: the region set is closed --------------------------------

test('eu-central-1 is refused naming the null-message trap', () => {
  assert.throws(
    () => Region.parse('eu-central-1'),
    (error) => {
      assert.equal(codeOf(error), 'ERR_INVALID_ARG');
      // Both halves: "AccessDeniedException" alone reads as an IAM problem, and it is the
      // word *null* that says otherwise.
      assert.match(error.message, /AccessDeniedException/);
      assert.match(error.message, /null/);
      // A local reject reached no daemon, so there is no wire kind to report.
      assert.equal(error.cause?.cause, undefined);
      return true;
    },
  );
});

test('the five supported regions are the measured ones', () => {
  assert.deepEqual(
    Region.supported().map((region) => region.name),
    ['us-east-1', 'us-east-2', 'us-west-2', 'eu-west-1', 'ap-northeast-1'],
  );
  assert.ok(Region.supported().every((region) => region.isSupported));
});

test('the escape hatch is visible in the value it produces', () => {
  const unlisted = Region.unlisted('eu-central-1');
  assert.equal(unlisted.name, 'eu-central-1');
  assert.equal(unlisted.isSupported, false);
  // A supported name comes back as its proper region, so nothing downstream handles two
  // spellings of one region. `equals` and not `===`: class instances compare by reference.
  assert.ok(Region.unlisted('us-east-1').equals(Region.usEast1()));
  assert.equal(Region.usEast1().isSupported, true);
});

test('Region has no constructor, so an unchecked name is not writable', () => {
  assert.throws(() => new Region('eu-central-1'));
});

// -- BIND-2: no parameter bypasses a trap closure -----------------------------

test('the two hook timeouts cannot be transposed', () => {
  const run = new RunHookTimeout(30);
  const build = new BuildHookTimeout(3600);
  assert.equal(run.seconds, 30);
  assert.equal(build.seconds, 3600);
  assert.equal(run.maxSecs, 60);
  assert.equal(build.maxSecs, 3600);

  // 3600 is legal for the build family and refused for the run family, by the core, with a
  // message naming *both* ceilings — because the caller who hits it picked a build number.
  assert.throws(
    () => new RunHookTimeout(3600),
    (error) => {
      assert.equal(codeOf(error), 'ERR_INVALID_ARG');
      assert.match(error.message, /60/);
      assert.match(error.message, /3600/);
      return true;
    },
  );

  // And they are nominal, not structural: an object literal of the same shape is not a
  // timeout. This is the `#[napi]`-class-versus-`#[napi(object)]` decision, asserted.
  assert.notEqual(
    Object.getPrototypeOf(run),
    Object.getPrototypeOf(build),
    'the two timeouts share a prototype, so they are interchangeable',
  );
});

test('an off-table size is refused rather than snapped', () => {
  // TRAP-10. 1500 has two plausible readings that differ in both memory and rate.
  assert.throws(
    () => SizeClass.fromBaselineMib(1500),
    (error) => {
      assert.equal(codeOf(error), 'ERR_INVALID_ARG');
      assert.match(error.message, /not a documented size class baseline/);
      assert.match(error.message, /selects a class, it does not size a VM/);
      return true;
    },
  );
  // And the five documented baselines are accepted, so the guard is not a blanket refusal.
  for (const size of SizeClass.all()) {
    assert.equal(SizeClass.fromBaselineMib(size.baselineMib).baselineMib, size.baselineMib);
  }
});

test('billing reads the baseline and never the peak', () => {
  // COST-5. The default class reports 8 GB in the guest and bills 2.
  const size = SizeClass.defaultClass();
  assert.equal(size.baselineMib, 2048);
  assert.equal(size.baselineGb, 2);
  assert.equal(size.peakGb, 8);
  assert.match(size.describe(), /billed while running/);
  assert.match(size.describe(), /what the guest reports/);
});

test('the rate table has no constructor taking rates', () => {
  // COST-9: only the pinned table, so no value built from an x86 figure exists.
  const rates = RateTable.pinned();
  assert.equal(rates.region, 'us-east-1');
  assert.equal(rates.retrieved, '2026-08-07');
  assert.equal(rates.vcpuSecond, '0.0000276944');
  assert.equal(rates.storageGbMonth, '0.0811111030');
  // `null` means NOT PUBLISHED, not one second: nothing rounds a duration up.
  assert.equal(rates.minimumBillingIncrementSec, null);
  assert.equal(rates.freeTier, false);
  assert.equal(rates.billsVcpuAndMemorySeparately, true);

  assert.throws(() => new RateTable());
});

// -- the error contract --------------------------------------------------------

test('every ERR_ code is enumerable and there are thirteen', () => {
  const codes = errorCodes();
  assert.equal(codes.length, 13, 'one per ErrorKind');
  assert.ok(codes.includes('ERR_WINDOW_CLOSED'));
  assert.ok(codes.includes('ERR_INVALID_ARG'));
  assert.ok(codes.every((code) => code.startsWith('ERR_')));
  // Thirteen distinct: two kinds sharing a code would make branching ambiguous.
  assert.equal(new Set(codes).size, 13);
});

test('every wire kind is enumerable and there are thirteen', () => {
  const kinds = wireKinds();
  assert.equal(kinds.length, 13);
  // The load-bearing pair: 400 and 404 are different things, and the conformance oracle
  // asserts on exactly these names.
  assert.ok(kinds.includes('ProtocolError'));
  assert.ok(kinds.includes('NotFound'));
});

test('a synchronous rejection carries the code on .code as well as on the cause', () => {
  // The bonus half of the contract: on a *sync* path `.code` really is the `ERR_*` string,
  // and the uniform `cause.message` rule holds too. Both are asserted so a reader can see
  // which one to rely on.
  assert.throws(
    () => Region.parse('nope-1'),
    (error) => {
      assert.equal(error.code, 'ERR_INVALID_ARG', 'sync .code should be the ERR_ string');
      assert.equal(codeOf(error), 'ERR_INVALID_ARG');
      return true;
    },
  );
});

// -- the module surface --------------------------------------------------------

test('the module reports the core version', () => {
  assert.ok(coreVersion().length > 0);
});

test('a direct session is constructible without AWS', async () => {
  // The conformance shape: no proxy headers, no control plane, no credentials. Constructing
  // a session does not talk to the VM, so this is assertable offline.
  const session = Session.direct('http://127.0.0.1:9000', 'agent-token');
  assert.equal(await session.endpoint(), 'http://127.0.0.1:9000');
  assert.equal(await session.port(), 9000);
  // No minter, so no proxy auth and nothing to mint.
  assert.equal(await session.proxyMintCount(), null);
});

test('an exec handle is reattachable by id without a daemon', async () => {
  const session = Session.direct('http://127.0.0.1:9000', 'agent-token');
  const handle = await session.exec('x-0000000000000000');
  assert.equal(handle.execId, 'x-0000000000000000');
});

test('a command is a string or an argv, and nothing else', () => {
  const session = Session.direct('http://127.0.0.1:9000', 'agent-token');
  // `throws` and not `rejects`, and the difference is the point: napi's argument conversion
  // runs **before** the future is built, so a bad command is a synchronous `TypeError` rather
  // than a rejected Promise. Measured — the first version of this test used `assert.rejects`
  // and failed with `Value is none of these types String, Array<T>` thrown synchronously.
  //
  // That is strictly stronger than a rejection: a caller who forgot to `await` still sees the
  // error, and no request was ever built. Same property the guarded classes rely on.
  for (const bad of [3, { cmd: 'ls' }, null, true]) {
    assert.throws(() => session.run(bad), `run(${JSON.stringify(bad)}) was accepted`);
  }
});

test('the cost constants are the documented figures', () => {
  const constants = JSON.parse(costConstants());
  // 730 hours, not 30 days: the two conventions disagree by a few percent and only one
  // matches the worked examples.
  assert.equal(constants.hoursPerMonth, '730');
  assert.equal(constants.secondsPerMonth, '2628000');
  assert.equal(constants.staleAfterDays, 90);
  assert.equal(constants.minimumRetentionSeconds, 7 * 24 * 60 * 60);
  assert.deepEqual(constants.provenances, ['measured', 'projected']);
  assert.deepEqual(constants.billingLines, [
    'vcpu',
    'memory',
    'snapshot-storage',
    'snapshot-read',
    'snapshot-write',
  ]);
});

test('the session constants are the daemon’s wire contract', () => {
  const constants = JSON.parse(sessionConstants());
  assert.equal(constants.defaultAgentPort, 9000);
  // Both headers, because one without the other is rejected the same indistinguishable way.
  assert.match(constants.proxyAuthHeader, /proxy-auth/i);
  assert.match(constants.proxyPortHeader, /proxy-port/i);
  // The sixty-minute ceiling a long run will cross mid-flight.
  assert.equal(constants.maxTokenLifetimeSeconds, 3600);
  assert.ok(constants.defaultRefreshAfterSeconds < constants.maxTokenLifetimeSeconds);
});

test('the residency comparison carries its own counter-argument', () => {
  const comparison = compareResidency(SizeClass.defaultClass(), 86400, 1);
  assert.equal(comparison.cycles, 1);
  // Projected, always: a comparison is a hypothetical about a hold nobody has taken.
  assert.equal(comparison.hold.provenance, 'projected');
  // Strings for the money and the exact seconds; a number only where it is named as lossy.
  assert.equal(typeof comparison.ratio, 'string');
  assert.equal(typeof comparison.breakEvenSeconds(), 'string');
  assert.equal(typeof comparison.breakEvenSecondsNumber(), 'number');
  const perCycle = comparison.perCycle();
  assert.ok(Number.isNaN(Number(perCycle)), 'per-cycle cost coerced to a number');
  assert.match(comparison.render(), /break-even hold/);
  assert.match(comparison.render(), /avoid churn/);
});
