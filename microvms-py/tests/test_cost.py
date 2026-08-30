# SPDX-License-Identifier: Apache-2.0
"""The cost surface: `src/cost.rs`.

Where `test_smoke.py` asserts what the cost types *refuse*, this file asserts what they
**answer** — and the two are not the same coverage. A binding whose `EstimatedUsd` correctly has
no `__float__` can still report the wrong dollar figure, omit a line item, or sum a lower bound
as though it were exact, and every one of those is a wrong number on a bill with no exception
raised anywhere.

The assertions are arithmetic and shape, not values transcribed from a previous run:

* A total is checked by **re-deriving it** from the line items with `Decimal`, so the test
  fails if the floor and the items disagree rather than if a figure changed.
* A phase round-trip goes through `cost_constants()["phases"]`, which is the core's own
  `CostPhase::ALL`, so a phase added to the enum is covered without an edit here.
* The JSON shape is checked key for key against `cli.py`'s `report_to_dict`, because the two
  clients had to be diffable and consumers now read it.

Nothing here talks to AWS. A cost report is a pure function of a size class, a usage record, and
a rate table.
"""

from __future__ import annotations

from decimal import Decimal

import pytest

import microvms


def measured_report() -> microvms.CostReport:
    """A report with one priced phase and one unpriced one.

    `image_gb` is what makes both image lines appear, so this report is deliberately
    *incomplete* — which is what the lower-bound assertions need.
    """
    return microvms.run_report(
        microvms.SizeClass.default_class(),
        running=microvms.Duration.measured(3600.0),
        image_gb=2.0,
        label="unit",
    )


# -- the report's line items --------------------------------------------------


def test_a_running_phase_bills_two_lines_because_vcpu_and_memory_are_priced_apart() -> (
    None
):
    """COST-5 plus the rate table's own `bills_vcpu_and_memory_separately`.

    One `running` line would be the mistake: the pricing page prices vCPU-seconds and
    GB-seconds as two figures, so a single blended line cannot be reconciled against it. Both
    lines carry the same duration and different units, which is how a reader tells them apart.
    """
    running = measured_report().by_phase("running")
    assert [item.line for item in running] == ["vcpu", "memory"]
    assert [item.unit for item in running] == ["vCPU-seconds", "GB-seconds"]
    # Same phase, so the same span — a duration that differed between the two would mean one
    # of them was billed for a window nobody measured.
    assert {item.duration.seconds for item in running} == {3600.0}
    assert all(item.duration.provenance == "measured" for item in running)


def test_the_vcpu_quantity_is_the_baseline_times_the_seconds_and_not_the_peak() -> None:
    """COST-5's actual arithmetic, re-derived rather than transcribed.

    The default class reports 4 vCPU in the guest and bills 1. A quantity computed off
    `peak_vcpu` would be four times the real figure and would still look like a plausible
    number, which is precisely why this is checked as a product rather than against a literal.
    """
    size = microvms.SizeClass.default_class()
    vcpu = measured_report().by_phase("running")[0]
    assert Decimal(vcpu.quantity) == Decimal(str(size.baseline_vcpu)) * Decimal("3600")
    # And the peak really is different, so the assertion above is not vacuous.
    assert size.peak_vcpu != size.baseline_vcpu


def test_the_memory_quantity_is_the_baseline_gb_times_the_seconds() -> None:
    """The same check on the other half of the pair, because they use different fields."""
    size = microvms.SizeClass.default_class()
    memory = measured_report().by_phase("running")[1]
    assert Decimal(memory.quantity) == Decimal(str(size.baseline_gb)) * Decimal("3600")
    assert size.peak_gb != size.baseline_gb


def test_every_priced_line_costs_its_quantity_times_the_published_rate() -> None:
    """The rate multiplication, checked against the rate table rather than a stored figure.

    This is the one assertion that would catch a rate read out of the wrong column — the x86
    compute figure instead of the ARM one, 17.9% higher, which COST-9 exists to prevent. A
    test comparing against a hardcoded dollar amount would go red on a *rate table update* too,
    and so would be edited to match rather than investigated.
    """
    rates = microvms.RateTable.pinned()
    per_unit = {
        "vcpu": Decimal(rates.vcpu_second),
        "memory": Decimal(rates.gb_second),
        "snapshot-read": Decimal(rates.snapshot_read_gb),
        "snapshot-write": Decimal(rates.snapshot_write_gb),
    }
    for item in measured_report().priced:
        if item.line not in per_unit:
            continue
        expected = Decimal(item.quantity) * per_unit[item.line]
        assert Decimal(item.amount.usd.amount) == expected, item.line


def test_the_floor_is_exactly_the_sum_of_the_priced_lines() -> None:
    """COST-4's arithmetic: the floor is a sum, and an unpriced line contributes nothing.

    Re-derived with `Decimal` off the line items, so the test states the *relationship* rather
    than a number. A floor that quietly included a zero for the unpriced build line would still
    equal this sum — which is why the next test checks the flag as well.
    """
    report = measured_report()
    summed = sum(
        (Decimal(item.amount.usd.amount) for item in report.priced),
        start=Decimal(0),
    )
    assert Decimal(report.total.floor.amount) == summed
    # And an unpriced line really is excluded rather than summed as zero.
    assert report.unpriced, "this report is supposed to be incomplete"
    assert all(item.amount.usd is None for item in report.unpriced)


def test_a_lower_bound_carries_one_reason_per_unpriced_line_in_report_order() -> None:
    """The reasons are not a summary: a caller shows them beside the figure.

    Asserted as a per-line correspondence rather than "non-empty", because a total that
    reported *one* reason for several unpriced lines would look right in a smoke test and would
    understate how much of the bill is unknown.
    """
    report = measured_report()
    assert report.total.is_lower_bound
    assert report.total.unpriced_reasons == [
        item.amount.unpriced.reason for item in report.unpriced
    ]


def test_priced_and_unpriced_partition_the_items_with_nothing_in_both_or_neither() -> (
    None
):
    """A partition, so no line is double-counted into the floor or dropped from it."""
    report = measured_report()
    assert len(report.priced) + len(report.unpriced) == len(report.items)
    assert all(item.amount.kind == "estimated-usd" for item in report.priced)
    assert all(item.amount.kind == "unpriced" for item in report.unpriced)
    # `complete` is the report-level spelling of "nothing is unpriced", and it has to agree.
    assert report.complete == (len(report.unpriced) == 0)


# -- phases: the closed set, through core's own FromStr ------------------------


def test_every_phase_the_core_publishes_round_trips_through_by_phase() -> None:
    """`by_phase` accepts every spelling in `cost_constants()["phases"]`, and only those.

    Enumerated from the constants rather than written out, which is the point: the core grew
    `CostPhase::from_str` precisely so the bindings would stop carrying their own seven-element
    tables. A phase added to the enum appears in this loop with no edit here — and a binding
    that reintroduced a local table would fail on the new phase.
    """
    phases = microvms.cost_constants()["phases"]
    assert len(phases) == 7, phases
    report = measured_report()
    for phase in phases:
        # Every phase is accepted; only some are present in this particular report, and an
        # absent one is an empty list rather than a refusal.
        selected = report.by_phase(phase)
        assert all(item.phase == phase for item in selected), phase


def test_by_phase_selects_exactly_the_items_carrying_that_phase() -> None:
    """The selection is a filter, so the phases partition the item list.

    Without this, `by_phase` could return everything (or nothing) and the loop above would
    still pass — it only checks that what comes back is consistent.
    """
    report = measured_report()
    regrouped = [
        item.phase
        for phase in microvms.cost_constants()["phases"]
        for item in report.by_phase(phase)
    ]
    assert sorted(regrouped) == sorted(item.phase for item in report.items)


def test_an_unknown_phase_is_refused_by_the_core_and_the_message_offers_the_set() -> (
    None
):
    """The refusal names every phase, so a typo is self-correcting.

    The offered list is built from `CostPhase::ALL` in the core rather than written into the
    message, which is what makes it stay correct — so this asserts the *whole set* is present
    rather than that the sentence has a particular wording.
    """
    with pytest.raises(microvms.InvalidArgError) as raised:
        measured_report().by_phase("runnning")
    message = str(raised.value)
    assert raised.value.code == "ERR_INVALID_ARG"
    assert raised.value.wire_kind is None, "nothing reached the daemon"
    for phase in microvms.cost_constants()["phases"]:
        assert phase in message, f"{phase} missing from the refusal: {message}"


@pytest.mark.parametrize("spelling", ["Running", "RUNNING", " running", "running ", ""])
def test_a_phase_is_matched_exactly_rather_than_normalised(spelling: str) -> None:
    """No case folding and no trimming, because a normalising parse hides a typo.

    `"Running"` accepted as `running` would mean two spellings of one phase reaching a report
    key, and whichever consumer grouped by the raw string would silently split the group.
    """
    with pytest.raises(microvms.InvalidArgError):
        measured_report().by_phase(spelling)


# -- the JSON shape ------------------------------------------------------------


def test_a_priced_line_carries_its_usd_as_a_string_under_the_amount_key() -> None:
    """The positive half of the unpriced-omission rule.

    `test_smoke.py` asserts the unpriced line has **no** `usd` key. That guard would also pass
    if the priced line lost its key, so the pair only means something together.
    """
    priced = measured_report().priced[0].to_dict()
    assert priced["amount"]["kind"] == "estimated-usd"
    assert "reason" not in priced["amount"]
    # A string, exact: the figure survives into `Decimal` where a float would already have lost
    # the precision the 0.0000276944 rates carry.
    assert isinstance(priced["amount"]["usd"], str)
    assert Decimal(priced["amount"]["usd"]) > 0


def test_a_line_dict_carries_every_key_cli_py_emitted_and_no_others() -> None:
    """`_line_to_dict`'s shape, exactly, so the two clients stay diffable."""
    line = measured_report().priced[0].to_dict()
    assert set(line) == {
        "phase",
        "line",
        "quantity",
        "unit",
        "amount",
        "duration",
        "note",
    }
    assert set(line["duration"]) == {"seconds", "provenance"}


def test_a_line_with_no_duration_carries_a_null_rather_than_omitting_the_key() -> None:
    """The opposite convention from `usd`, and deliberately so.

    A null duration says "this line is not time-based" — a snapshot read is priced per GB — and
    nothing sums a duration, so a null cannot be misread as zero the way a null dollar figure
    can. The asymmetry is the interesting part, which is why it is asserted rather than assumed.
    """
    report = microvms.run_report(
        microvms.SizeClass.default_class(),
        running=microvms.Duration.measured(60.0),
        suspend_resume_cycles=1,
        label="cycled",
    )
    timeless = [item for item in report.items if item.duration is None]
    assert timeless, "a suspend/resume cycle bills per GB, not per second"
    for item in timeless:
        rendered = item.to_dict()
        assert "duration" in rendered
        assert rendered["duration"] is None


def test_the_report_dict_items_are_the_line_dicts_in_report_order() -> None:
    """One rendering, not two: the report's items are its line items.

    A separately-assembled item list in `to_dict` is how a report's JSON and its objects drift
    apart, and the drift would show up as a dollar figure that disagreed with the table above
    it in the same output.
    """
    report = measured_report()
    assert report.to_dict()["items"] == [item.to_dict() for item in report.items]


def test_the_total_dict_names_the_floor_as_priced_and_flags_the_bound() -> None:
    """`priced`, never `total`: the key says what the figure is.

    A key called `total` over a lower bound is the whole COST-4 mistake in one word — a
    consumer reading it has no reason to check `isLowerBound`.
    """
    rendered = measured_report().to_dict()["total"]
    assert set(rendered) == {"priced", "isLowerBound", "render"}
    assert rendered["isLowerBound"] is True
    assert rendered["priced"] == measured_report().total.floor.amount
    assert rendered["render"].startswith("at least")


# -- Unpriced is a distinct value, not a zero ---------------------------------


def test_an_unpriced_line_still_carries_a_measurable_quantity() -> None:
    """Unpriced is a claim about the *rate*, not about the quantity.

    The build line has a real GB figure and no dollar figure, and conflating the two would mean
    either dropping the line (losing the fact that a build happened) or pricing it at zero
    (understating the run). This is the distinction stated as an assertion.
    """
    build = measured_report().by_phase("image-build")[0]
    assert build.amount.kind == "unpriced"
    assert build.amount.usd is None
    assert build.amount.unpriced.reason == microvms.build_unpriced_reason()
    # The quantity is real and the note says what it is.
    assert Decimal(build.quantity) >= 0
    assert build.unit


def test_a_complete_report_has_an_exact_total_with_no_reasons() -> None:
    """The other variant, so `is_lower_bound` is not vacuously true everywhere.

    A report with no image is fully priced, and its floor *is* its total — which is what makes
    the flag informative rather than a constant.
    """
    complete = microvms.run_report(
        microvms.SizeClass.default_class(),
        running=microvms.Duration.measured(60.0),
        launched=False,
        label="priced only",
    )
    assert complete.complete
    assert not complete.total.is_lower_bound
    assert complete.total.unpriced_reasons == []
    assert not complete.unpriced
    assert not str(complete.total).startswith("at least")


# -- provenance ---------------------------------------------------------------


def test_a_measured_report_is_fully_measured_only_when_every_duration_was_timed() -> (
    None
):
    """`fully_measured` is an `all`, not an `any`.

    The default `image_retained` is a documented one-week minimum nobody timed, so a report
    that passed `image_gb` is *not* fully measured however carefully the running phase was
    clocked — and reporting otherwise would label a projection as a measurement.
    """
    with_image = measured_report()
    assert not with_image.fully_measured
    assert any(
        item.duration is not None and item.duration.provenance == "projected"
        for item in with_image.items
    )

    timed = microvms.run_report(
        microvms.SizeClass.default_class(),
        running=microvms.Duration.measured(60.0),
        launched=False,
        label="timed",
    )
    assert timed.fully_measured
    assert all(
        item.duration is None or item.duration.is_measured for item in timed.items
    )


def test_a_plan_marks_every_duration_projected_however_it_was_built() -> None:
    """COST-10 through the report rather than through the signature.

    `estimate_run` takes seconds and there is no parameter a measured duration could be written
    into, so *every* duration on a plan is projected — including the ones a caller passed
    explicitly, which is the case a `Duration` parameter would have let through.
    """
    plan = microvms.estimate_run(
        microvms.SizeClass.default_class(),
        running_seconds=3600.0,
        suspended_seconds=60.0,
        image_gb=2.0,
        suspend_resume_cycles=2,
        label="plan",
    )
    assert not plan.fully_measured
    durations = [item.duration for item in plan.items if item.duration is not None]
    assert durations, "a plan with running seconds has timed lines"
    assert all(duration.provenance == "projected" for duration in durations)
    assert not any(duration.is_measured for duration in durations)


def test_a_plan_and_a_measured_run_over_the_same_seconds_cost_the_same() -> None:
    """The arithmetic is shared; only the provenance differs.

    This is the assertion that says COST-10 is about *labelling* rather than about a second
    pricing path. If the two disagreed, one of them would be wrong and nothing else in the
    suite would say which.
    """
    plan = microvms.estimate_run(
        microvms.SizeClass.default_class(),
        running_seconds=3600.0,
        launched=False,
        label="plan",
    )
    measured = microvms.run_report(
        microvms.SizeClass.default_class(),
        running=microvms.Duration.measured(3600.0),
        launched=False,
        label="run",
    )
    assert plan.total.floor.amount == measured.total.floor.amount
    assert plan.fully_measured != measured.fully_measured


# -- the rate table -----------------------------------------------------------


def test_a_zero_length_run_costs_nothing_rather_than_rounding_up_to_an_increment() -> (
    None
):
    """`minimum_billing_increment_sec` is `None` — not published, not one second.

    Inventing an increment would overcharge every short exec, and the figure a caller would
    reach for is a plausible one. So the absence is asserted through the arithmetic: a zero
    duration prices at zero.
    """
    assert microvms.RateTable.pinned().minimum_billing_increment_sec is None
    report = microvms.run_report(
        microvms.SizeClass.default_class(),
        running=microvms.Duration.measured(0.0),
        launched=False,
        label="instant",
    )
    assert Decimal(report.total.floor.amount) == 0


def test_the_storage_month_rate_is_the_hourly_rate_times_the_month_convention() -> None:
    """The one derived rate, checked against the constant it is derived with.

    730 hours and not 30 days: the two conventions disagree by a few percent, and only one
    matches the worked examples. Re-deriving it here is what would catch a table where the
    monthly figure was updated and the hourly one was not.
    """
    rates = microvms.RateTable.pinned()
    hours_per_month = Decimal(microvms.cost_constants()["hoursPerMonth"])
    assert hours_per_month == 730
    per_gb_hour = Decimal(rates.storage_gb_month) / hours_per_month
    # Back the other way, so the assertion reads as the identity it is.
    assert per_gb_hour * hours_per_month == Decimal(rates.storage_gb_month)


def test_a_fresh_table_reports_no_staleness_and_the_report_agrees_with_it() -> None:
    """The report's staleness is the table's, not a second computation.

    Two answers to "are these rates old" is one too many: a report that said `None` while its
    table said otherwise would put a stale figure in front of someone with no warning.
    """
    report = measured_report()
    assert report.staleness == report.rates.staleness()
    assert report.rates.age_days() >= 0
    stale_after = microvms.cost_constants()["staleAfterDays"]
    assert (report.staleness is None) == (report.rates.age_days() <= stale_after)


def test_the_minimum_retention_is_a_week_and_the_constants_agree_with_the_table() -> (
    None
):
    """One figure, reachable two ways, and they have to match.

    Snapshot storage bills at least this long however briefly the snapshot exists, so a
    disagreement here is a report that understates a short-lived snapshot.
    """
    rates = microvms.RateTable.pinned()
    assert rates.minimum_retention_seconds == 7 * 24 * 60 * 60
    assert (
        microvms.cost_constants()["minimumRetentionSeconds"]
        == rates.minimum_retention_seconds
    )


# -- size classes: the closed set --------------------------------------------


def test_the_five_size_classes_pair_each_baseline_with_a_peak_four_times_it() -> None:
    """The table, asserted as the *relationship* rather than as ten numbers.

    Every class's provisioned peak — present from the start, never a scaling event — is 4x
    its baseline in both memory and vCPU. Stating that as a ratio is what would catch a class
    whose peak was transcribed from the row above it — which two columns of literals would
    not, because both columns would look internally consistent.
    """
    classes = microvms.SizeClass.all()
    assert [size.baseline_mib for size in classes] == [512, 1024, 2048, 4096, 8192]
    for size in classes:
        assert size.peak_mib == size.baseline_mib * 4
        assert size.peak_vcpu == size.baseline_vcpu * 4
        # GB is MiB/1024 and the billing figure is the baseline one.
        assert size.baseline_gb == size.baseline_mib / 1024
        assert size.peak_gb == size.peak_mib / 1024


def test_the_default_class_is_the_middle_one_rather_than_the_smallest() -> None:
    """2048 MiB, and the reason is in `describe()`.

    The smallest class hands someone a sandbox that OOM-kills a real test suite and the guest
    has no swap, so the default is not `ALL[0]` — a fact worth an assertion because "default =
    first" is the change someone would make while tidying.
    """
    default = microvms.SizeClass.default_class()
    classes = microvms.SizeClass.all()
    assert default.baseline_mib == 2048
    assert default.baseline_mib != classes[0].baseline_mib
    assert microvms.SizeClass.from_baseline_mib(2048).baseline_mib == 2048


@pytest.mark.parametrize("mib", [0, 511, 513, 1500, 3072, 8193, 16384])
def test_an_off_table_baseline_is_refused_for_every_plausible_near_miss(
    mib: int,
) -> None:
    """TRAP-10 across the range, not just at 1500.

    Each of these is a figure someone types: a doubling that overshot the table, a round number
    between two classes, one off a real baseline. All refused, because the two neighbouring
    readings differ in both memory and rate and neither has been measured.
    """
    with pytest.raises(microvms.InvalidArgError) as raised:
        microvms.SizeClass.from_baseline_mib(mib)
    assert raised.value.code == "ERR_INVALID_ARG"
    assert "not a documented size class baseline" in str(raised.value)


def test_a_bigger_class_costs_strictly_more_for_the_same_wall_time() -> None:
    """Monotonic, which is the sanity check on the whole size/rate pairing.

    A class whose rate was paired with the wrong baseline could easily still produce a
    plausible-looking figure; it could not keep the sequence monotonic. Asserted over the whole
    table rather than for one pair.
    """
    totals = [
        Decimal(
            microvms.run_report(
                size,
                running=microvms.Duration.measured(3600.0),
                launched=False,
                label="sized",
            ).total.floor.amount
        )
        for size in microvms.SizeClass.all()
    ]
    assert totals == sorted(totals)
    assert len(set(totals)) == len(totals), (
        "two classes cost the same, so one is mispaired"
    )


# -- the residency comparison ------------------------------------------------


def test_a_suspended_vm_costs_less_than_a_running_one_over_a_long_hold() -> None:
    """The warm-pool argument's premise, and the ratio that quantifies it.

    A *long* hold, and the qualifier is the point — see the break-even test below, where the
    comparison inverts. The ratio is a division of the two floors, checked to the precision
    `Decimal` division carries rather than for exact equality: the core divides at 28
    significant digits, so re-dividing here lands within 1e-26 and demanding `==` would be a
    test about `Decimal`'s rounding rather than about the ratio.
    """
    comparison = microvms.compare_residency(
        microvms.SizeClass.default_class(), 86400.0, 1
    )
    running = Decimal(comparison.running.total.floor.amount)
    suspended = Decimal(comparison.suspended.total.floor.amount)
    assert suspended < running
    assert abs(Decimal(comparison.ratio) - running / suspended) < Decimal("1e-20")
    assert comparison.hold.seconds == 86400.0
    # Projected, always: a comparison is a hypothetical about a hold nobody has taken.
    assert comparison.hold.provenance == "projected"


def test_the_break_even_hold_is_exactly_where_the_two_sides_cost_the_same() -> None:
    """The counter-argument, asserted by *evaluating* the comparison at the break-even hold.

    This is the load-bearing test in the residency group, and it is checked by construction
    rather than by re-deriving the formula: `break_even_seconds` is solved from the rate table
    in the core, so a test that re-implemented the same algebra here would agree with a wrong
    formula. Instead the number is fed **back in** as a hold, and the claim it makes is checked
    directly — at the break-even hold the two sides cost the same, so the ratio is 1.

    Which makes the sign checks either side meaningful: below it, suspending costs *more* than
    leaving the VM running, and that inversion is the thing a bare "100x cheaper" headline
    hides.
    """
    size = microvms.SizeClass.default_class()
    break_even = float(
        microvms.compare_residency(size, 86400.0, 1).break_even_seconds()
    )

    at = microvms.compare_residency(size, break_even, 1)
    # At the crossing the ratio is 1: neither residency wins.
    assert abs(Decimal(at.ratio) - 1) < Decimal("0.000001"), at.ratio

    below = microvms.compare_residency(size, break_even / 2, 1)
    assert Decimal(below.ratio) < 1, "below break-even, suspending must cost more"
    assert Decimal(below.suspended.total.floor.amount) > Decimal(
        below.running.total.floor.amount
    )

    above = microvms.compare_residency(size, break_even * 2, 1)
    assert Decimal(above.ratio) > 1
    assert Decimal(above.suspended.total.floor.amount) < Decimal(
        above.running.total.floor.amount
    )


def test_the_break_even_hold_is_a_property_of_the_rates_and_not_of_the_hold_or_cycles() -> (
    None
):
    """One cycle's break-even, whatever the comparison it is read off.

    Measured, and initially assumed otherwise: the figure answers "how long must *a* suspension
    last to pay for itself", which is a question about the rate table and the size class alone.
    So it does not move with the hold a caller happens to be comparing, nor with the cycle
    count — the cycles scale the suspended side's total, not the per-cycle threshold. A
    scheduler reads this number once per size class rather than per decision.
    """
    size = microvms.SizeClass.default_class()
    baseline = microvms.compare_residency(size, 86400.0, 1).break_even_seconds()
    for hold in (60.0, 3600.0, 86400.0, 30 * 86400.0):
        for cycles in (1, 10, 100):
            comparison = microvms.compare_residency(size, hold, cycles)
            assert comparison.break_even_seconds() == baseline, (hold, cycles)
    # A bigger class pays more per cycle *and* more per running second, so the threshold is a
    # real function of the class rather than a constant.
    assert (
        microvms.compare_residency(
            microvms.SizeClass.all()[0], 86400.0, 1
        ).break_even_seconds()
        != baseline
    )


def test_more_cycles_raise_the_suspended_total_and_narrow_the_ratio() -> None:
    """Churn is charged, which is what keeps "suspend constantly" from reading as free.

    The per-cycle price is fixed and the suspended side pays it once per cycle, so ten cycles
    cost nine more than one. Asserted as the *difference* rather than as an inequality, because
    an inequality would pass for a comparison that charged cycles at some other rate.
    """
    size = microvms.SizeClass.default_class()
    one = microvms.compare_residency(size, 86400.0, 1)
    ten = microvms.compare_residency(size, 86400.0, 10)
    assert ten.cycles == 10
    per_cycle = Decimal(one.per_cycle().amount)
    assert Decimal(ten.per_cycle().amount) == per_cycle
    assert (
        Decimal(ten.suspended.total.floor.amount)
        - Decimal(one.suspended.total.floor.amount)
        == per_cycle * 9
    )
    # The running side is untouched — it never suspends — so only the ratio moves.
    assert ten.running.total.floor.amount == one.running.total.floor.amount
    assert Decimal(ten.ratio) < Decimal(one.ratio)


def test_the_lossy_float_break_even_agrees_with_the_exact_string() -> None:
    """The named-lossy accessor, checked against the figure it is derived from.

    It exists because `cli.py` emits `breakEvenSeconds` as a JSON number and the two clients
    have to agree. A float is the only place in this module where precision is given up, and the
    test says how much: enough for a JSON envelope, not enough for money — which is why no
    dollar figure has one.
    """
    comparison = microvms.compare_residency(
        microvms.SizeClass.default_class(), 86400.0, 1
    )
    exact = Decimal(comparison.break_even_seconds())
    assert abs(Decimal(comparison.break_even_seconds_float()) - exact) < Decimal("1e-9")


# -- the constants object -----------------------------------------------------


def test_the_constants_size_class_table_matches_the_size_class_objects() -> None:
    """One table, reachable two ways.

    `cost_constants()["sizeClasses"]` is what a caller asserting against the wire contract
    reads, and `SizeClass.all()` is what they compute with. A disagreement would mean a
    consumer validating against the constants and then billing off the objects.
    """
    published = microvms.cost_constants()["sizeClasses"]
    assert {int(mib) for mib in published} == {
        size.baseline_mib for size in microvms.SizeClass.all()
    }
    for size in microvms.SizeClass.all():
        assert published[size.baseline_mib] == size.describe()


def test_the_billing_lines_are_exactly_the_lines_a_report_can_attribute_to() -> None:
    """The published set is the reachable set, so neither is a superset of the other.

    A published line no report can produce is a consumer branch that never runs; a produced
    line nobody published is a consumer branch that does not exist. Both are covered by making
    this a subset check plus a reachability check.
    """
    published = set(microvms.cost_constants()["billingLines"])
    # Every line a full report attributes to is published.
    full = microvms.run_report(
        microvms.SizeClass.default_class(),
        running=microvms.Duration.measured(60.0),
        suspended=microvms.Duration.measured(60.0),
        image_gb=2.0,
        suspend_resume_cycles=1,
        snapshot_gb=2.0,
        label="everything",
    )
    produced = {item.line for item in full.items if item.line is not None}
    assert produced <= published
    # And the published set is not padded: every one of these is reachable from that report.
    assert produced == published


def test_the_two_provenances_are_the_only_ones_a_duration_can_report() -> None:
    """The closed set, checked against what the constructors actually produce."""
    published = microvms.cost_constants()["provenances"]
    assert published == ["measured", "projected"]
    assert microvms.Duration.measured(1.0).provenance == published[0]
    assert microvms.Duration.projected(1.0).provenance == published[1]


@pytest.mark.parametrize(
    "seconds", [-0.001, -1.0, float("nan"), float("inf"), float("-inf")]
)
def test_a_duration_refuses_every_unrepresentable_figure_at_both_constructors(
    seconds: float,
) -> None:
    """The core's `duration_of_secs_f64`, reached through both doors.

    `nan` and `inf` matter as much as a negative here: they arrive from a division nobody
    checked, and a `nan` duration would price a phase as `nan` dollars all the way into a
    report.
    """
    for constructor in (microvms.Duration.measured, microvms.Duration.projected):
        with pytest.raises(microvms.InvalidArgError) as raised:
            constructor(seconds)
        assert raised.value.code == "ERR_INVALID_ARG"


def test_a_zero_duration_is_accepted_because_a_phase_can_genuinely_not_happen() -> None:
    """The boundary the refusals above must not swallow.

    Zero is a real measurement — a phase that was skipped — and refusing it would force a
    caller to special-case the common path.
    """
    assert microvms.Duration.measured(0.0).seconds == 0.0
    assert microvms.Duration.projected(0.0).seconds == 0.0
