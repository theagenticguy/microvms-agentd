// SPDX-License-Identifier: Apache-2.0
//
// The cost surface: `src/cost.rs`.
//
// Where `smoke.mjs` asserts what the cost types *refuse* — no `valueOf`, no `toJSON`, no
// constructor — this file asserts what they **answer**, and the two are not the same coverage. A
// binding whose `EstimatedUsd` correctly coerces to `NaN` can still report the wrong dollar figure,
// omit a line item, or sum a lower bound as though it were exact, and every one of those is a wrong
// number on a bill with nothing thrown anywhere.
//
// # The arithmetic is checked in BigInt, not in Number
//
// See `support/decimal.mjs`. Parsing these strings to `Number` to check them would perform exactly
// the laundering step the types exist to prevent, and would lose the precision that makes a report
// reconcilable against the rate table it came from. Every figure below is compared as an exact
// decimal string.
//
// Nothing here talks to AWS: a cost report is a pure function of a size class, a usage record, and a
// rate table.

import assert from 'node:assert/strict';
import { test } from 'node:test';

import {
  buildUnpricedReason,
  compareResidency,
  costConstants,
  Duration,
  estimateRun,
  RateTable,
  runReport,
  SizeClass,
} from '../index.js';
import {
  compareDecimals,
  decimalsCloseEnough,
  decimalsEqual,
  multiplyDecimals,
  sumDecimals,
} from './support/decimal.mjs';
import { codeOf } from './support/sse.mjs';

/** A report with one priced phase and one unpriced one.
 *
 * `imageGb` is what makes both image lines appear, so this report is deliberately *incomplete* —
 * which is what the lower-bound assertions need.
 */
function measuredReport() {
  return runReport(SizeClass.defaultClass(), {
    running: Duration.measured(3600),
    imageGb: 2,
    label: 'unit',
  });
}

// -- the report's line items ---------------------------------------------------

test('a running phase bills two lines because vcpu and memory are priced apart', () => {
  // One blended `running` line would be the mistake: the pricing page prices vCPU-seconds and
  // GB-seconds as two figures, so a single line cannot be reconciled against it. Both carry the
  // same duration and different units, which is how a reader tells them apart.
  const running = measuredReport().byPhase('running');
  assert.deepEqual(
    running.map((item) => item.line),
    ['vcpu', 'memory'],
  );
  assert.deepEqual(
    running.map((item) => item.unit),
    ['vCPU-seconds', 'GB-seconds'],
  );
  // Same phase, so the same span — a duration differing between the two would mean one was billed
  // for a window nobody measured.
  for (const item of running) {
    assert.equal(item.duration.seconds, 3600);
    assert.equal(item.duration.provenance, 'measured');
  }
  assert.equal(RateTable.pinned().billsVcpuAndMemorySeparately, true);
});

test('the vcpu quantity is the baseline times the seconds and never the peak', () => {
  // COST-5's actual arithmetic. The default class reports 4 vCPU in the guest and bills 1, so a
  // quantity computed off `peakVcpu` would be four times the real figure and would still look like
  // a plausible number — which is why this is checked as a product rather than against a literal.
  const size = SizeClass.defaultClass();
  const [vcpu] = measuredReport().byPhase('running');
  assert.ok(
    decimalsEqual(vcpu.quantity, multiplyDecimals(String(size.baselineVcpu), '3600')),
    `${vcpu.quantity} is not baselineVcpu x 3600`,
  );
  // And the peak really is different, so the assertion is not vacuous.
  assert.notEqual(size.peakVcpu, size.baselineVcpu);
});

test('the memory quantity is the baseline GB times the seconds', () => {
  // The same check on the other half of the pair, because they read different fields.
  const size = SizeClass.defaultClass();
  const memory = measuredReport().byPhase('running')[1];
  assert.ok(decimalsEqual(memory.quantity, multiplyDecimals(String(size.baselineGb), '3600')));
  assert.notEqual(size.peakGb, size.baselineGb);
});

test('every priced line costs its quantity times the published rate', () => {
  // The rate multiplication, checked against the rate table rather than a stored figure — which is
  // the one assertion that would catch a rate read out of the wrong column (the x86 compute figure
  // instead of the ARM one, 17.9% higher, which COST-9 exists to prevent). A test comparing against
  // a hardcoded dollar amount would also go red on a legitimate rate-table update, and so would be
  // edited to match rather than investigated.
  const rates = RateTable.pinned();
  const perUnit = {
    vcpu: rates.vcpuSecond,
    memory: rates.gbSecond,
    'snapshot-read': rates.snapshotReadGb,
    'snapshot-write': rates.snapshotWriteGb,
  };
  for (const item of measuredReport().priced) {
    if (!(item.line in perUnit)) continue;
    const expected = multiplyDecimals(item.quantity, perUnit[item.line]);
    assert.ok(
      decimalsEqual(item.amount.usd.amount, expected),
      `${item.line}: ${item.amount.usd.amount} != ${item.quantity} x ${perUnit[item.line]}`,
    );
  }
});

test('the floor is exactly the sum of the priced lines', () => {
  // COST-4's arithmetic: the floor is a sum, and an unpriced line contributes nothing. Re-derived
  // rather than transcribed, so the test states the relationship. A floor that quietly included a
  // zero for the unpriced build line would still equal this sum — which is why the flag is checked
  // in the next test.
  const report = measuredReport();
  const summed = sumDecimals(report.priced.map((item) => item.amount.usd.amount));
  assert.ok(
    decimalsEqual(report.total.floor.amount, summed),
    `${report.total.floor.amount} != ${summed}`,
  );
  assert.ok(report.unpriced.length > 0, 'this report is supposed to be incomplete');
  for (const item of report.unpriced) assert.equal(item.amount.usd, null);
});

test('a lower bound carries one reason per unpriced line in report order', () => {
  // The reasons are not a summary — a caller shows them beside the figure. Asserted as a per-line
  // correspondence rather than "non-empty", because a total reporting *one* reason for several
  // unpriced lines would look right in a smoke test while understating how much of the bill is
  // unknown.
  const report = measuredReport();
  assert.equal(report.total.isLowerBound, true);
  assert.deepEqual(
    report.total.unpricedReasons,
    report.unpriced.map((item) => item.amount.unpriced.reason),
  );
});

test('priced and unpriced partition the items with nothing in both or neither', () => {
  // A partition, so no line is double-counted into the floor or dropped from it.
  const report = measuredReport();
  assert.equal(report.priced.length + report.unpriced.length, report.items.length);
  for (const item of report.priced) assert.equal(item.amount.kind, 'estimated-usd');
  for (const item of report.unpriced) assert.equal(item.amount.kind, 'unpriced');
  // `complete` is the report-level spelling of "nothing is unpriced", and it has to agree.
  assert.equal(report.complete, report.unpriced.length === 0);
});

test('an unpriced line still carries a measurable quantity', () => {
  // Unpriced is a claim about the *rate*, not about the quantity. The build line has a real GB
  // figure and no dollar figure; conflating them would mean either dropping the line (losing the
  // fact that a build happened) or pricing it at zero (understating the run).
  const [build] = measuredReport().byPhase('image-build');
  assert.equal(build.amount.kind, 'unpriced');
  assert.equal(build.amount.usd, null);
  assert.equal(build.amount.unpriced.reason, buildUnpricedReason());
  assert.ok(compareDecimals(build.quantity, '0') >= 0);
  assert.ok(build.unit.length > 0);
});

test('a complete report has an exact total with no reasons', () => {
  // The other variant, so `isLowerBound` is not vacuously true everywhere. A report with no image
  // is fully priced and its floor *is* its total — which makes the flag informative rather than a
  // constant.
  const complete = runReport(SizeClass.defaultClass(), {
    running: Duration.measured(60),
    launched: false,
    label: 'priced only',
  });
  assert.equal(complete.complete, true);
  assert.equal(complete.total.isLowerBound, false);
  assert.deepEqual(complete.total.unpricedReasons, []);
  assert.equal(complete.unpriced.length, 0);
  assert.ok(!complete.total.toString().startsWith('at least'));
});

// -- phases: the closed set, through core's own FromStr ------------------------

test('every phase the core publishes round trips through byPhase', () => {
  // Enumerated from the constants rather than written out, which is the point: core grew
  // `CostPhase::from_str` precisely so the bindings would stop carrying their own seven-element
  // tables. A phase added to the enum appears in this loop with no edit here — and a binding that
  // reintroduced a local table would fail on the new phase.
  const { phases } = JSON.parse(costConstants());
  assert.equal(phases.length, 7);
  const report = measuredReport();
  for (const phase of phases) {
    for (const item of report.byPhase(phase)) {
      assert.equal(item.phase, phase);
    }
  }
});

test('byPhase selects exactly the items carrying that phase', () => {
  // The selection is a filter, so the phases partition the item list. Without this, `byPhase` could
  // return everything (or nothing) and the loop above would still pass — it only checks that what
  // comes back is self-consistent.
  const report = measuredReport();
  const { phases } = JSON.parse(costConstants());
  const regrouped = phases.flatMap((phase) => report.byPhase(phase).map((item) => item.phase));
  assert.deepEqual(
    regrouped.slice().sort(),
    report.items.map((item) => item.phase).sort(),
  );
});

test('an unknown phase is refused by the core and the message offers the whole set', () => {
  // The offered list is built from `CostPhase::ALL` in the core rather than written into the
  // message, which is what keeps it correct — so this asserts the whole set is present rather than
  // that the sentence has a particular wording.
  const { phases } = JSON.parse(costConstants());
  assert.throws(
    () => measuredReport().byPhase('runnning'),
    (error) => {
      assert.equal(codeOf(error), 'ERR_INVALID_ARG');
      for (const phase of phases) {
        assert.ok(error.message.includes(phase), `${phase} missing: ${error.message}`);
      }
      return true;
    },
  );
});

test('a phase is matched exactly rather than normalised', () => {
  // No case folding and no trimming: `"Running"` accepted as `running` would mean two spellings of
  // one phase reaching a report key, and whichever consumer grouped by the raw string would
  // silently split the group.
  for (const spelling of ['Running', 'RUNNING', ' running', 'running ', '']) {
    assert.throws(
      () => measuredReport().byPhase(spelling),
      `byPhase(${JSON.stringify(spelling)}) was accepted`,
    );
  }
});

// -- the JSON shape ------------------------------------------------------------

test('a priced line carries its usd as a string under the amount key', () => {
  // The positive half of the unpriced-omission rule. `smoke.mjs` asserts the unpriced line has
  // **no** `usd` key; that guard would also pass if the priced line lost its key, so the pair only
  // means something together.
  const parsed = JSON.parse(measuredReport().priced[0].toJson());
  assert.equal(parsed.amount.kind, 'estimated-usd');
  assert.ok(!('reason' in parsed.amount));
  // A string, exact: the figure survives into a decimal library where a JS double would already
  // have lost the precision the 0.0000276944 rates carry.
  assert.equal(typeof parsed.amount.usd, 'string');
  assert.ok(compareDecimals(parsed.amount.usd, '0') > 0);
});

test('a line’s JSON carries every key cli.py emitted and no others', () => {
  // `_line_to_dict`'s shape, exactly, so the two clients stay diffable.
  const parsed = JSON.parse(measuredReport().priced[0].toJson());
  assert.deepEqual(Object.keys(parsed).sort(), [
    'amount',
    'duration',
    'line',
    'note',
    'phase',
    'quantity',
    'unit',
  ]);
  assert.deepEqual(Object.keys(parsed.duration).sort(), ['provenance', 'seconds']);
});

test('a line with no duration carries a null rather than omitting the key', () => {
  // The opposite convention from `usd`, and deliberately so. A null duration says "this line is not
  // time-based" — a snapshot read is priced per GB — and nothing sums a duration, so it cannot be
  // misread as zero the way a null dollar figure can. The asymmetry is the interesting part.
  const report = runReport(SizeClass.defaultClass(), {
    running: Duration.measured(60),
    suspendResumeCycles: 1,
    label: 'cycled',
  });
  const timeless = report.items.filter((item) => item.duration === null);
  assert.ok(timeless.length > 0, 'a suspend/resume cycle bills per GB, not per second');
  for (const item of timeless) {
    const parsed = JSON.parse(item.toJson());
    assert.ok('duration' in parsed);
    assert.equal(parsed.duration, null);
  }
});

test('the report JSON’s items are the line JSONs in report order', () => {
  // One rendering, not two: the report's items are its line items. A separately-assembled item list
  // in `toJson` is how a report's JSON and its objects drift apart, and the drift would show up as
  // a dollar figure disagreeing with the table above it in the same output.
  const report = measuredReport();
  const fromReport = JSON.parse(report.toJson()).items;
  const fromItems = report.items.map((item) => JSON.parse(item.toJson()));
  assert.deepEqual(fromReport, fromItems);
});

test('the total JSON names the floor as priced and flags the bound', () => {
  // `priced`, never `total`: the key says what the figure is. A key called `total` over a lower
  // bound is the whole COST-4 mistake in one word — a consumer reading it has no reason to check
  // `isLowerBound`.
  const report = measuredReport();
  const total = JSON.parse(report.toJson()).total;
  assert.deepEqual(Object.keys(total).sort(), ['isLowerBound', 'priced', 'render']);
  assert.equal(total.isLowerBound, true);
  assert.equal(total.priced, report.total.floor.amount);
  assert.ok(total.render.startsWith('at least'));
});

test('the JSON escaping survives a label that would break a hand-built string', () => {
  // `line_json` and `to_json` are hand-assembled rather than serde-derived, because the shape's
  // load-bearing property — an absent `usd` key — is what a derive over an `Option` would get
  // wrong. Hand-assembly puts the escaping on this file's own `quote`, so a label with a quote or a
  // newline in it is the case that would produce invalid JSON. A caller's label comes from a CLI
  // flag, so it is attacker-adjacent input in the ordinary sense.
  const nasty = 'a "quoted" \\ back\nslash\tand tab';
  const report = runReport(SizeClass.defaultClass(), {
    running: Duration.measured(1),
    launched: false,
    label: nasty,
  });
  // The whole point: it parses, and the label survives byte for byte.
  const parsed = JSON.parse(report.toJson());
  assert.equal(parsed.label, nasty);
});

// -- provenance ----------------------------------------------------------------

test('fullyMeasured is an all, not an any', () => {
  // The default `imageRetained` is a documented one-week minimum nobody timed, so a report that
  // passed `imageGb` is *not* fully measured however carefully the running phase was clocked — and
  // reporting otherwise would label a projection as a measurement.
  const withImage = measuredReport();
  assert.equal(withImage.fullyMeasured, false);
  assert.ok(
    withImage.items.some((item) => item.duration !== null && item.duration.provenance === 'projected'),
  );

  const timed = runReport(SizeClass.defaultClass(), {
    running: Duration.measured(60),
    launched: false,
    label: 'timed',
  });
  assert.equal(timed.fullyMeasured, true);
  for (const item of timed.items) {
    if (item.duration !== null) assert.equal(item.duration.isMeasured, true);
  }
});

test('a plan marks every duration projected however it was built', () => {
  // COST-10 through the report rather than through the signature. `estimateRun` takes numbers and
  // there is no field a measured duration could be written into, so *every* duration on a plan is
  // projected — including the ones a caller passed explicitly, which is the case a `Duration` field
  // would have let through.
  const plan = estimateRun(SizeClass.defaultClass(), {
    runningSeconds: 3600,
    suspendedSeconds: 60,
    imageGb: 2,
    suspendResumeCycles: 2,
    label: 'plan',
  });
  assert.equal(plan.fullyMeasured, false);
  const durations = plan.items.map((item) => item.duration).filter((d) => d !== null);
  assert.ok(durations.length > 0);
  for (const duration of durations) {
    assert.equal(duration.provenance, 'projected');
    assert.equal(duration.isMeasured, false);
  }
});

test('a plan and a measured run over the same seconds cost the same', () => {
  // The assertion that says COST-10 is about *labelling* rather than about a second pricing path.
  // If the two disagreed, one would be wrong and nothing else in the suite would say which.
  const plan = estimateRun(SizeClass.defaultClass(), {
    runningSeconds: 3600,
    launched: false,
    label: 'plan',
  });
  const measured = runReport(SizeClass.defaultClass(), {
    running: Duration.measured(3600),
    launched: false,
    label: 'run',
  });
  assert.ok(decimalsEqual(plan.total.floor.amount, measured.total.floor.amount));
  assert.notEqual(plan.fullyMeasured, measured.fullyMeasured);
});

test('a duration field takes only a real Duration instance, never a number or a literal', () => {
  // The `ClassInstance` decision, asserted. `#[napi(object)]` converts *by structure*, so a plain
  // `{ running: 3600 }` or `{ running: { seconds: 3600 } }` would satisfy an options bag whose field
  // were a number or an object — and either would be an unlabelled duration, which is the thing
  // COST-1 exists to make unwriteable. `ClassInstance` extracts only from a real class instance, and
  // napi refuses the rest before any Rust runs.
  for (const bad of [3600, { seconds: 3600 }, { seconds: 3600, provenance: 'measured' }, '3600', null]) {
    if (bad === null) continue; // null is "this phase did not happen", which is legal.
    assert.throws(
      () => runReport(SizeClass.defaultClass(), { running: bad, label: 'bad' }),
      `running: ${JSON.stringify(bad)} was accepted as a duration`,
    );
  }
  // And a real one works, so the guard is not a blanket refusal.
  assert.ok(runReport(SizeClass.defaultClass(), { running: Duration.measured(1) }));
});

test('an omitted phase is how "this did not happen" is said', () => {
  // What the options bag buys over positional parameters: JS's own `undefined` distinguishes a phase
  // that did not occur from one that took zero seconds. Both are legal and they are different
  // reports.
  const nothing = runReport(SizeClass.defaultClass(), { launched: false, label: 'nothing' });
  const zero = runReport(SizeClass.defaultClass(), {
    running: Duration.measured(0),
    launched: false,
    label: 'zero',
  });
  assert.equal(nothing.byPhase('running').length, 0, 'an omitted phase produced a line');
  assert.ok(zero.byPhase('running').length > 0, 'a zero-length phase was dropped');
  // Both cost nothing, so the difference is in the *attribution* rather than the figure.
  assert.ok(decimalsEqual(nothing.total.floor.amount, zero.total.floor.amount));
});

// -- the rate table ------------------------------------------------------------

test('a zero-length run costs nothing rather than rounding up to an increment', () => {
  // `minimumBillingIncrementSec` is `null` — not published, not one second. Inventing an increment
  // would overcharge every short exec, and the figure someone would reach for is a plausible one. So
  // the absence is asserted through the arithmetic.
  assert.equal(RateTable.pinned().minimumBillingIncrementSec, null);
  const report = runReport(SizeClass.defaultClass(), {
    running: Duration.measured(0),
    launched: false,
    label: 'instant',
  });
  assert.ok(decimalsEqual(report.total.floor.amount, '0'), report.total.floor.amount);
});

test('the month convention is 730 hours and the two constants agree exactly', () => {
  // `storageGbMonth` is the one *derived* rate — the API quotes snapshot storage per GB-hour and
  // the table carries that times 730 — and 730 hours rather than 30 days is the load-bearing part:
  // the two conventions disagree by a few percent and only one matches the worked examples.
  //
  // The hourly rate itself is not on the binding surface, so it cannot be re-derived here. What
  // *can* be checked is the pair of published constants that encode the convention, and they have
  // to be the same month: `secondsPerMonth` must be `hoursPerMonth x 3600`. A table where one was
  // updated to a 30-day month and the other was not would price storage and break-even holds
  // against different calendars.
  const { hoursPerMonth, secondsPerMonth } = JSON.parse(costConstants());
  assert.equal(hoursPerMonth, '730');
  assert.ok(
    decimalsEqual(secondsPerMonth, multiplyDecimals(hoursPerMonth, '3600')),
    `${secondsPerMonth} is not ${hoursPerMonth} hours of seconds`,
  );
  // 30 days would be 720 hours, which is the specific wrong answer.
  assert.ok(!decimalsEqual(hoursPerMonth, '720'));
});

test('the snapshot-storage line costs its GB-months times the published monthly rate', () => {
  // The derived rate as it is actually *used*, which is the reachable half. A storage line priced
  // off a per-hour figure while the quantity was in GB-months would be wrong by 730x — a large
  // enough error to notice, but only if something checks the multiplication.
  const rates = RateTable.pinned();
  const report = runReport(SizeClass.defaultClass(), {
    suspended: Duration.measured(86400),
    snapshotGb: 2,
    launched: false,
    label: 'held',
  });
  const storage = report.items.filter((item) => item.line === 'snapshot-storage');
  assert.ok(storage.length > 0, 'a suspended VM pays snapshot storage');
  for (const item of storage) {
    // `decimalsCloseEnough` rather than exact: the GB-month quantity is itself a 28-digit division
    // by 730, so the exact product overflows `rust_decimal`'s precision and the core's figure is
    // that rounded. See the helper for the measurement and why a looser tolerance would hide a real
    // error.
    assert.ok(
      decimalsCloseEnough(
        item.amount.usd.amount,
        multiplyDecimals(item.quantity, rates.storageGbMonth),
      ),
      `${item.amount.usd.amount} != ${item.quantity} x ${rates.storageGbMonth}`,
    );
  }
});

test('a fresh table reports no staleness and the report agrees with it', () => {
  // The report's staleness is the table's, not a second computation. Two answers to "are these
  // rates old" is one too many: a report saying `null` while its table said otherwise would put a
  // stale figure in front of someone with no warning.
  const report = measuredReport();
  assert.equal(report.staleness, report.rates.staleness());
  assert.ok(report.rates.ageDays() >= 0);
  const { staleAfterDays } = JSON.parse(costConstants());
  assert.equal(report.staleness === null, report.rates.ageDays() <= staleAfterDays);
});

test('the minimum retention is a week and the constants agree with the table', () => {
  // One figure, reachable two ways. Snapshot storage bills at least this long however briefly the
  // snapshot exists, so a disagreement is a report that understates a short-lived snapshot.
  const rates = RateTable.pinned();
  assert.equal(rates.minimumRetentionSeconds, 7 * 24 * 60 * 60);
  assert.equal(
    JSON.parse(costConstants()).minimumRetentionSeconds,
    rates.minimumRetentionSeconds,
  );
});

// -- size classes: the closed set ---------------------------------------------

test('the five size classes pair each baseline with a peak four times it', () => {
  // The table, asserted as the *relationship* rather than as ten numbers. Every class's provisioned
  // peak — present from the start, never a scaling event — is 4x its baseline in both memory and
  // vCPU. Stating that as a ratio is what would catch a class whose peak was transcribed from the
  // row above it, which two columns of literals would not because both columns would look
  // internally consistent.
  const classes = SizeClass.all();
  assert.deepEqual(
    classes.map((size) => size.baselineMib),
    [512, 1024, 2048, 4096, 8192],
  );
  for (const size of classes) {
    assert.equal(size.peakMib, size.baselineMib * 4);
    assert.equal(size.peakVcpu, size.baselineVcpu * 4);
    assert.equal(size.baselineGb, size.baselineMib / 1024);
    assert.equal(size.peakGb, size.peakMib / 1024);
  }
});

test('the default class is the middle one rather than the smallest', () => {
  // The smallest class hands someone a sandbox that OOM-kills a real test suite and the guest has no
  // swap, so the default is not `ALL[0]` — worth an assertion because "default = first" is the
  // change someone would make while tidying.
  const size = SizeClass.defaultClass();
  assert.equal(size.baselineMib, 2048);
  assert.notEqual(size.baselineMib, SizeClass.all()[0].baselineMib);
});

test('an off-table baseline is refused for every plausible near miss', () => {
  // TRAP-10 across the range, not just at 1500. Each of these is a figure someone types: a doubling
  // that overshot the table, a round number between two classes, one off a real baseline. All
  // refused, because the two neighbouring readings differ in both memory and rate and neither has
  // been measured.
  for (const mib of [0, 511, 513, 1500, 3072, 8193, 16384]) {
    assert.throws(
      () => SizeClass.fromBaselineMib(mib),
      (error) => {
        assert.equal(codeOf(error), 'ERR_INVALID_ARG');
        assert.match(error.message, /not a documented size class baseline/);
        return true;
      },
      `fromBaselineMib(${mib}) was accepted`,
    );
  }
});

test('a bigger class costs strictly more for the same wall time', () => {
  // Monotonic, which is the sanity check on the whole size/rate pairing. A class whose rate was
  // paired with the wrong baseline could easily still produce a plausible figure; it could not keep
  // the sequence monotonic.
  const totals = SizeClass.all().map(
    (size) =>
      runReport(size, { running: Duration.measured(3600), launched: false, label: 'sized' }).total
        .floor.amount,
  );
  for (let index = 1; index < totals.length; index += 1) {
    assert.ok(
      compareDecimals(totals[index], totals[index - 1]) > 0,
      `${totals[index]} is not greater than ${totals[index - 1]}`,
    );
  }
});

// -- the residency comparison --------------------------------------------------

test('a suspended VM costs less than a running one over a long hold', () => {
  // A *long* hold, and the qualifier is the point — see the break-even test below, where the
  // comparison inverts.
  const comparison = compareResidency(SizeClass.defaultClass(), 86400, 1);
  const running = comparison.running.total.floor.amount;
  const suspended = comparison.suspended.total.floor.amount;
  assert.ok(compareDecimals(suspended, running) < 0);
  assert.equal(comparison.hold.seconds, 86400);
  // Projected, always: a comparison is a hypothetical about a hold nobody has taken.
  assert.equal(comparison.hold.provenance, 'projected');
  assert.ok(compareDecimals(comparison.ratio, '1') > 0);
});

test('the break-even hold is exactly where the two sides cost the same', () => {
  // The load-bearing test in this group, and it is checked by *construction* rather than by
  // re-deriving the formula: `breakEvenSeconds` is solved from the rate table in the core, so a test
  // that re-implemented the same algebra here would agree with a wrong formula. Instead the number
  // is fed **back in** as a hold and the claim is checked directly — at the break-even hold the two
  // sides cost the same, so the ratio is 1.
  //
  // Which makes the sign checks either side meaningful: below it, suspending costs *more* than
  // leaving the VM running, and that inversion is what a bare "100x cheaper" headline hides.
  const size = SizeClass.defaultClass();
  const breakEven = compareResidency(size, 86400, 1).breakEvenSecondsNumber();

  const at = compareResidency(size, breakEven, 1);
  // Within a rounding of 1, using the lossy accessor because the hold had to be a JS number.
  assert.ok(Math.abs(Number(at.ratio) - 1) < 1e-6, at.ratio);

  const below = compareResidency(size, breakEven / 2, 1);
  assert.ok(compareDecimals(below.ratio, '1') < 0, 'below break-even, suspending must cost more');
  assert.ok(
    compareDecimals(below.suspended.total.floor.amount, below.running.total.floor.amount) > 0,
  );

  const above = compareResidency(size, breakEven * 2, 1);
  assert.ok(compareDecimals(above.ratio, '1') > 0);
  assert.ok(
    compareDecimals(above.suspended.total.floor.amount, above.running.total.floor.amount) < 0,
  );
});

test('the break-even hold is a property of the rates, not of the hold or the cycles', () => {
  // Measured, and initially assumed otherwise: the figure answers "how long must *a* suspension last
  // to pay for itself", which is a question about the rate table and the size class alone. So it does
  // not move with the hold being compared, nor with the cycle count — the cycles scale the suspended
  // side's total, not the per-cycle threshold. A scheduler reads this once per size class rather than
  // per decision.
  const size = SizeClass.defaultClass();
  const baseline = compareResidency(size, 86400, 1).breakEvenSeconds();
  for (const hold of [60, 3600, 86400, 30 * 86400]) {
    for (const cycles of [1, 10, 100]) {
      assert.equal(
        compareResidency(size, hold, cycles).breakEvenSeconds(),
        baseline,
        `hold=${hold} cycles=${cycles}`,
      );
    }
  }
  // A different class pays differently per cycle *and* per running second, so the threshold is a
  // real function of the class rather than a constant.
  assert.notEqual(compareResidency(SizeClass.all()[0], 86400, 1).breakEvenSeconds(), baseline);
});

test('more cycles raise the suspended total and narrow the ratio', () => {
  // Churn is charged, which is what keeps "suspend constantly" from reading as free. Asserted as the
  // *difference* rather than as an inequality, because an inequality would pass for a comparison
  // that charged cycles at some other rate.
  const size = SizeClass.defaultClass();
  const one = compareResidency(size, 86400, 1);
  const ten = compareResidency(size, 86400, 10);
  assert.equal(ten.cycles, 10);
  const perCycle = one.perCycle().amount;
  assert.ok(decimalsEqual(ten.perCycle().amount, perCycle));
  assert.ok(
    decimalsEqual(
      sumDecimals([ten.suspended.total.floor.amount]),
      sumDecimals([
        one.suspended.total.floor.amount,
        multiplyDecimals(perCycle, '9'),
      ]),
    ),
    'nine extra cycles did not cost nine per-cycle prices',
  );
  // The running side never suspends, so only the ratio moves.
  assert.equal(ten.running.total.floor.amount, one.running.total.floor.amount);
  assert.ok(compareDecimals(ten.ratio, one.ratio) < 0);
});

test('the lossy number accessor agrees with the exact string it comes from', () => {
  // It exists because `cli.py` emits `breakEvenSeconds` as a JSON number and the two clients have to
  // agree. A number is the only place in this module where precision is given up, and the test says
  // how much: enough for a JSON envelope, not enough for money — which is why no dollar figure has
  // one.
  const comparison = compareResidency(SizeClass.defaultClass(), 86400, 1);
  const exact = comparison.breakEvenSeconds();
  const lossy = comparison.breakEvenSecondsNumber();
  assert.equal(typeof exact, 'string');
  assert.equal(typeof lossy, 'number');
  assert.ok(Math.abs(lossy - Number(exact)) < 1e-6);
});

// -- the constants object ------------------------------------------------------

test('the billing lines are exactly the lines a report can attribute to', () => {
  // The published set is the reachable set, so neither is a superset. A published line no report can
  // produce is a consumer branch that never runs; a produced line nobody published is a consumer
  // branch that does not exist.
  const published = new Set(JSON.parse(costConstants()).billingLines);
  const full = runReport(SizeClass.defaultClass(), {
    running: Duration.measured(60),
    suspended: Duration.measured(60),
    imageGb: 2,
    suspendResumeCycles: 1,
    snapshotGb: 2,
    label: 'everything',
  });
  const produced = new Set(full.items.map((item) => item.line).filter((line) => line !== null));
  for (const line of produced) assert.ok(published.has(line), `${line} is not published`);
  for (const line of published) assert.ok(produced.has(line), `${line} is not reachable`);
});

test('the two provenances are the only ones a duration can report', () => {
  const { provenances } = JSON.parse(costConstants());
  assert.deepEqual(provenances, ['measured', 'projected']);
  assert.equal(Duration.measured(1).provenance, provenances[0]);
  assert.equal(Duration.projected(1).provenance, provenances[1]);
});

test('a duration refuses every unrepresentable figure at both factories', () => {
  // `NaN` and `Infinity` matter as much as a negative here, and more than in Python: they arrive from
  // a division nobody checked or a `Number(undefined)`, and a `NaN` duration would price a phase as
  // `NaN` dollars all the way into a report.
  for (const seconds of [-0.001, -1, Number.NaN, Number.POSITIVE_INFINITY, Number.NEGATIVE_INFINITY]) {
    for (const factory of ['measured', 'projected']) {
      assert.throws(
        () => Duration[factory](seconds),
        (error) => {
          assert.equal(codeOf(error), 'ERR_INVALID_ARG');
          return true;
        },
        `Duration.${factory}(${seconds}) was accepted`,
      );
    }
  }
});

test('a zero duration is accepted because a phase can genuinely not happen', () => {
  // The boundary the refusals above must not swallow.
  assert.equal(Duration.measured(0).seconds, 0);
  assert.equal(Duration.projected(0).seconds, 0);
});
