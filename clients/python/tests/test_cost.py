"""Cost accounting: the rates, the labelling rules, and the unknown.

Two things get locked here. The arithmetic — every rate against
`docs/PLATFORM.md`'s own worked examples rather than against a re-derivation, so a
transcription error in either place shows up as a disagreement — and the two rules
from the plan: seconds are measured, dollars are estimated, and unknown is never
zero.

No AWS and no network: cost is pure arithmetic over a pinned table, which is what
makes it testable at all.
"""

from __future__ import annotations

from datetime import date, timedelta
from decimal import Decimal

import pytest

from microvms_agentd.cost import (
    RATES,
    SECONDS_PER_MONTH,
    STALE_AFTER,
    BillingLine,
    CostPhase,
    Duration,
    EstimatedUSD,
    Provenance,
    StaleRateTable,
    Unpriced,
    compare_residency,
    estimate_run,
    run_report,
)
from microvms_agentd.sizing import size_class_for

#: A month as AWS's GB-month rate defines it. Every "per month" figure in
#: `docs/PLATFORM.md` is this many seconds, not 30 days.
MONTH_SEC = 730 * 3600


def _amount(item) -> Decimal:  # type: ignore[no-untyped-def]
    assert isinstance(item.amount, EstimatedUSD), f"{item.phase} should be priced"
    return item.amount.amount


# -- the rate table ------------------------------------------------------------


def test_every_rate_matches_the_documented_table() -> None:
    # Transcribed from `docs/PLATFORM.md`, "What actually costs money", us-east-1.
    # Asserted literally rather than computed, because the whole value of pinning a
    # rate table is that it can be diffed against the page it came from.
    assert RATES.region == "us-east-1"
    assert RATES.vcpu_second == Decimal("0.0000276944")
    assert RATES.gb_second == Decimal("0.0000036667")
    assert RATES.storage_gb_month == Decimal("0.08")
    assert RATES.snapshot_read_gb == Decimal("0.00155")
    assert RATES.snapshot_write_gb == Decimal("0.0038")
    assert RATES.minimum_retention == timedelta(weeks=1)
    assert RATES.retrieved == date(2026, 8, 7)
    assert "aws.amazon.com" in RATES.source_url


def test_the_documented_billing_facts_are_data_not_prose() -> None:
    # Four arithmetic mistakes someone would otherwise make by analogy with Lambda
    # Functions: one blended GB-second, a per-invoke charge, a rounded-up billing
    # increment, and a free tier. `None` for the increment is "not published" —
    # distinct from 1.0, which would overcharge every sub-second exec in a report.
    assert RATES.bills_vcpu_and_memory_separately
    assert RATES.per_request == Decimal("0")
    assert RATES.free_tier is False
    assert RATES.minimum_billing_increment_sec is None


def test_a_rate_table_older_than_ninety_days_warns() -> None:
    # A silently stale price is the same failure class as a silently stale schema,
    # which this repo already fails CI on.
    just_past = RATES.retrieved + STALE_AFTER + timedelta(days=1)
    assert RATES.is_stale(just_past)
    with pytest.warns(StaleRateTable, match="days ago"):
        report = run_report(
            size=2048, running=Duration.measured(60), launched=False, today=just_past
        )
    # Carried on the report as well as warned, because a warnings filter or a CLI
    # that only writes stderr each lose it on their own.
    assert report.staleness is not None
    assert RATES.source_url in report.staleness


def test_a_fresh_rate_table_is_silent() -> None:
    # The complement of the test above. A warning that fires on a fresh table is a
    # warning everyone learns to filter, and then the stale case goes unseen too.
    fresh = RATES.retrieved + STALE_AFTER - timedelta(days=1)
    assert not RATES.is_stale(fresh)
    assert RATES.staleness(fresh) is None
    report = run_report(size=2048, running=Duration.measured(60), launched=False, today=fresh)
    assert report.staleness is None


# -- measured versus estimated -------------------------------------------------


def test_a_duration_cannot_be_created_without_saying_how_it_is_known() -> None:
    # The labelling rule has to be unwriteable-around, not documented: an
    # unlabelled duration would always end up labelled as the stronger claim.
    with pytest.raises(TypeError):
        Duration(60)  # type: ignore[call-arg]
    assert Duration.measured(60).provenance is Provenance.MEASURED
    assert Duration.projected(60).provenance is Provenance.PROJECTED


def test_every_dollar_figure_renders_as_an_estimate() -> None:
    # A figure copied out of a terminal loses its docstring, so the label travels
    # in `__str__` rather than sitting in the type name alone.
    assert "estimated" in str(EstimatedUSD(Decimal("1.23")))
    report = run_report(size=2048, running=Duration.measured(3600), launched=False)
    text = report.render()
    assert "estimates derived from published" in text
    assert "only Cost Explorer knows" in text


def test_an_estimate_is_never_fully_measured_and_a_timed_run_is() -> None:
    # The distinction survives into the report rather than stopping at the
    # duration, so a consumer can refuse to print an estimate as a receipt.
    assert not estimate_run(size=2048, running_seconds=3600, launched=False).fully_measured
    assert run_report(size=2048, running=Duration.measured(3600), launched=False).fully_measured


def test_a_measured_second_still_yields_an_estimated_dollar() -> None:
    # The asymmetry is the point: we can time a phase exactly and still only infer
    # its price, so there is no ActualUSD for a measured duration to produce.
    report = run_report(size=2048, running=Duration.measured(3600), launched=False)
    assert report.fully_measured
    assert all(isinstance(i.amount, EstimatedUSD) for i in report.items)
    assert "estimated" in str(report.total)


# -- unknown is not zero -------------------------------------------------------


def test_the_image_build_is_unpriced_rather_than_free() -> None:
    # AWS does not publish whether the server-side build is billed as compute. The
    # build starts a real MicroVM, so reporting $0.00 would understate the run in
    # the direction that flatters us.
    report = run_report(size=2048, image_gb=2.0, image_build=Duration.measured(300))
    build = report.by_phase(CostPhase.IMAGE_BUILD)
    assert len(build) == 1
    assert isinstance(build[0].amount, Unpriced)
    assert "does not publish" in build[0].amount.reason
    assert build[0].line is None, "no billing line exists to attribute it to"


def test_an_unpriced_line_makes_the_total_a_lower_bound() -> None:
    # A plain sum cannot express "everything priceable, plus a build whose price
    # AWS withholds", and the natural way to force it to is to drop the line.
    report = run_report(size=2048, image_gb=2.0, image_build=Duration.measured(300))
    assert not report.complete
    assert report.total.is_lower_bound
    assert "at least" in str(report.total)
    assert "unpriced" in str(report.total)


def test_an_unknown_amount_refuses_to_add_to_an_estimate() -> None:
    # The last line of defence. Even a consumer who ignores `complete` cannot sum
    # an unknown into a dollar figure by accident.
    with pytest.raises(TypeError):
        EstimatedUSD(Decimal("1")) + Unpriced("no rate")  # type: ignore[operator]


def test_a_report_with_no_image_is_complete() -> None:
    # The complement: a run against an existing image pays no build, so
    # incompleteness has to be a property of the phases present rather than a
    # permanent disclaimer nobody reads.
    report = run_report(size=2048, running=Duration.measured(60), launched=False)
    assert report.complete
    assert not report.total.is_lower_bound


# -- per-phase attribution -----------------------------------------------------


def test_running_bills_vcpu_and_memory_as_two_separate_lines() -> None:
    # Two entries rather than one blended GB-second, because that is how the
    # pricing page prices them and a blended figure cannot be reconciled against a
    # Cost Explorer breakdown that keeps them apart.
    report = run_report(size=2048, running=Duration.measured(3600), launched=False)
    lines = {i.line for i in report.by_phase(CostPhase.RUNNING)}
    assert lines == {BillingLine.VCPU, BillingLine.MEMORY}


def test_running_bills_the_baseline_and_never_the_peak() -> None:
    # `sizing` locks that billing follows the baseline; this locks that cost obeys
    # it. The 2 GB class reports 8 GB in the guest, so reading the peak would
    # over-state the memory line exactly 4x.
    size = size_class_for(2048)
    report = run_report(size=size, running=Duration.measured(3600), launched=False)
    (memory,) = [i for i in report.by_phase(CostPhase.RUNNING) if i.line is BillingLine.MEMORY]
    assert _amount(memory) == Decimal("2") * Decimal("3600") * RATES.gb_second
    (vcpu,) = [i for i in report.by_phase(CostPhase.RUNNING) if i.line is BillingLine.VCPU]
    assert _amount(vcpu) == Decimal("1") * Decimal("3600") * RATES.vcpu_second


def test_a_month_of_running_costs_about_a_hundred_dollars() -> None:
    # `docs/PLATFORM.md`: "roughly $100 a month to leave the same VM running at
    # baseline". An order-of-magnitude anchor on the compute rates, independent of
    # the exact rate literals above.
    report = run_report(size=2048, running=Duration.projected(MONTH_SEC), launched=False)
    assert Decimal("80") < report.total.priced.amount < Decimal("120")


def test_a_suspended_vm_pays_storage_and_no_compute() -> None:
    # A suspended guest is frozen rather than stopped, so there is no compute line
    # at all — not a compute line multiplied by zero, which would reappear the
    # moment someone changed how a duration is derived.
    report = run_report(size=2048, suspended=Duration.measured(MONTH_SEC), launched=False)
    lines = {i.line for i in report.by_phase(CostPhase.SUSPENDED)}
    assert lines == {BillingLine.SNAPSHOT_STORAGE}
    assert BillingLine.VCPU not in {i.line for i in report.items}
    # "about $0.16 a month" for a suspended 2 GB VM.
    assert Decimal("0.10") < report.total.priced.amount < Decimal("0.25")


def test_image_storage_bills_the_one_week_minimum_for_a_sixty_second_image() -> None:
    # The line item that dominates a create-and-destroy suite. `docs/PLATFORM.md`:
    # a 2 GB image deleted sixty seconds after creation still bills about a week,
    # "roughly four cents". Not applying the minimum understates the floor by four
    # orders of magnitude and makes the compute look like the cost driver.
    report = run_report(
        size=2048,
        image_gb=2.0,
        image_build=Duration.measured(300),
        image_retained=Duration.measured(60),
        launched=False,
    )
    (storage,) = report.by_phase(CostPhase.IMAGE_STORAGE)
    week = Decimal(int(timedelta(weeks=1).total_seconds()))
    assert _amount(storage) == Decimal("2") * week / SECONDS_PER_MONTH * RATES.storage_gb_month
    assert Decimal("0.03") < _amount(storage) < Decimal("0.05"), "roughly four cents"
    assert "minimum retention" in storage.note, "the floor must be visible, not just applied"


def test_a_long_hold_bills_actual_time_rather_than_the_minimum() -> None:
    # The floor is a floor, not a flat fee. A month-long hold that billed one week
    # would understate a warm pool's storage by 4x.
    report = run_report(
        size=2048, image_gb=2.0, image_retained=Duration.measured(MONTH_SEC), launched=False
    )
    (storage,) = report.by_phase(CostPhase.IMAGE_STORAGE)
    assert _amount(storage) == Decimal("2") * RATES.storage_gb_month
    assert "minimum retention" not in storage.note


def test_a_suspend_resume_cycle_bills_a_write_and_a_read() -> None:
    # Asymmetric rates — the write is ~2.5x the read — so a cycle costed with one
    # rate twice is wrong in whichever direction it picked.
    report = run_report(size=2048, suspend_resume_cycles=1, launched=False)
    (write,) = report.by_phase(CostPhase.SUSPEND)
    (read,) = report.by_phase(CostPhase.RESUME)
    assert write.line is BillingLine.SNAPSHOT_WRITE
    assert read.line is BillingLine.SNAPSHOT_READ
    assert _amount(write) == Decimal("2") * RATES.snapshot_write_gb
    assert _amount(read) == Decimal("2") * RATES.snapshot_read_gb
    # "about $0.011 for a 2 GB VM" per cycle.
    assert Decimal("0.010") < report.total.priced.amount < Decimal("0.012")


def test_cycles_scale_the_transition_cost() -> None:
    # The churn argument only works if churn actually accumulates.
    one = run_report(size=2048, suspend_resume_cycles=1, launched=False)
    ten = run_report(size=2048, suspend_resume_cycles=10, launched=False)
    assert ten.total.priced.amount == one.total.priced.amount * 10


# -- estimate mode -------------------------------------------------------------


def test_an_estimate_labels_every_duration_projected() -> None:
    # An estimate is spent before the money is, so nothing in it can claim to have
    # been timed.
    report = estimate_run(size=2048, running_seconds=3600, suspended_seconds=600, image_gb=2.0)
    timed = [i.duration for i in report.items if i.duration is not None]
    assert timed, "the estimate must attribute durations, not just totals"
    assert all(d.provenance is Provenance.PROJECTED for d in timed)


def test_an_estimate_and_a_measured_run_agree_on_the_arithmetic() -> None:
    # Same rates, same phases: only the labels differ. If the two paths could
    # disagree numerically, the label would be hiding a second implementation.
    projected = estimate_run(size=2048, running_seconds=3600, launched=False)
    measured = run_report(size=2048, running=Duration.measured(3600), launched=False)
    assert projected.total.priced == measured.total.priced


def test_an_estimate_accepts_a_baseline_mib_and_rejects_an_off_table_one() -> None:
    # Routed through `sizing.size_class_for`, so a cost figure for a size the
    # platform would refuse never gets produced — it would look like an answer.
    assert estimate_run(size=1024, running_seconds=60, launched=False).size.baseline_gb == 1.0
    with pytest.raises(ValueError, match="not a documented size class baseline"):
        estimate_run(size=1500, running_seconds=60)


def test_a_negative_duration_is_rejected() -> None:
    # An inverted clock would silently render as a credit on the report.
    with pytest.raises(ValueError, match="cannot be negative"):
        Duration.measured(-1)


# -- the warm-pool comparison --------------------------------------------------


def test_suspended_is_two_orders_of_magnitude_cheaper_than_running() -> None:
    # The whole argument for a warm suspended pool, and the reason the strategy
    # memo can decline to build the scheduler and still hand over the numbers.
    comparison = compare_residency(size=2048, hold_seconds=MONTH_SEC, cycles=1)
    assert comparison.ratio > 100


def test_the_comparison_includes_the_per_cycle_cost() -> None:
    # Without it the honest conclusion inverts: "suspend constantly" reads as free
    # when each cycle pays a write plus a read.
    comparison = compare_residency(size=2048, hold_seconds=MONTH_SEC, cycles=1)
    expected = Decimal("2") * (RATES.snapshot_write_gb + RATES.snapshot_read_gb)
    assert comparison.per_cycle.amount == expected
    assert "avoid churn" in comparison.render()


def test_churn_below_break_even_costs_more_than_leaving_the_vm_running() -> None:
    # The number a pool scheduler needs, and the one a bare "100x cheaper"
    # headline hides. Just under the break-even hold, a cycle loses money; just
    # over, it saves.
    comparison = compare_residency(size=2048, hold_seconds=MONTH_SEC, cycles=1)
    break_even = comparison.break_even_seconds
    assert break_even > 0

    def suspended_beats_running(hold: float) -> bool:
        c = compare_residency(size=2048, hold_seconds=hold, cycles=1)
        return c.suspended.total.priced.amount < c.running.total.priced.amount

    assert not suspended_beats_running(break_even * 0.9)
    assert suspended_beats_running(break_even * 1.1)


def test_the_comparison_excludes_the_image_so_the_ratio_is_not_diluted() -> None:
    # Image build and storage are identical either way, so including them would
    # shrink the ratio the comparison exists to show — and drag an unpriced build
    # into a figure whose whole job is to be comparable.
    comparison = compare_residency(size=2048, hold_seconds=MONTH_SEC)
    phases = {i.phase for i in comparison.running.items} | {
        i.phase for i in comparison.suspended.items
    }
    assert CostPhase.IMAGE_BUILD not in phases
    assert CostPhase.IMAGE_STORAGE not in phases
    assert comparison.running.complete and comparison.suspended.complete


def test_a_smaller_baseline_is_a_real_cost_lever() -> None:
    # Baseline is the rate paid for every running second, so picking a class is a
    # cost decision rather than a cosmetic one — which is what makes the CLI's
    # adequate-over-cheap default a deliberate trade rather than an oversight.
    small = run_report(size=512, running=Duration.projected(MONTH_SEC), launched=False)
    large = run_report(size=8192, running=Duration.projected(MONTH_SEC), launched=False)
    assert large.total.priced.amount == small.total.priced.amount * 16
