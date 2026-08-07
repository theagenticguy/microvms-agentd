"""What a run cost, and what a plan will cost, with every figure labelled.

MicroVMs has no standalone pricing page. The rates appear only inside worked
examples on the Lambda pricing page, which is why this module exists as data
rather than as a paragraph someone re-derives: a consumer building a sandbox
product should be able to ask "what did that run cost" without reading a pricing
page sideways.

Two rules hold everywhere below, and both are enforced by the types rather than
stated in prose a caller can skip.

**Seconds are measured, dollars are estimated.** A `Duration` cannot be
constructed without declaring whether it was timed or projected, and there is no
type in this module for an actual dollar amount — only `EstimatedUSD`, whose
`__str__` says so on every render. Only Cost Explorer knows the bill. That
asymmetry is the point: we can time a suspension to the millisecond and still
only infer its price.

**Unknown is not zero.** `Unpriced` is a distinct member of the `Amount` union,
it refuses to add to `EstimatedUSD`, and a `Total` containing one reads as a lower
bound. AWS does not publish whether the server-side image build is billed as
compute, so a report that summed it as $0.00 would be lying in the direction that
flatters us.

The table carries its own staleness: `RATES` is pinned to a retrieval date and a
table older than `STALE_AFTER` warns, because a silently stale price is the same
failure class as a silently stale schema.

Every rate here comes from `docs/PLATFORM.md`, "What actually costs money". None
of it is re-derived, and billing reads `SizeClass.baseline_*` and never the peak.
"""

from __future__ import annotations

import warnings
from dataclasses import dataclass
from datetime import date, timedelta
from decimal import Decimal
from enum import StrEnum

from .sizing import SizeClass, size_class_for

#: AWS bills storage per GB-month and defines a month as 730 hours, so a
#: partial-month hold is that fraction of the monthly rate. Spelled out because
#: 30-day and calendar-month conventions both give plausible-looking answers that
#: disagree with the worked examples in `docs/PLATFORM.md` by a few percent.
SECONDS_PER_MONTH = Decimal(730 * 3600)

#: How old a rate table may get before it warns. Ninety days is the same order as
#: the interval at which AWS has historically restructured Lambda pricing, and the
#: cost of the warning when nothing changed is one line of output.
STALE_AFTER = timedelta(days=90)


class StaleRateTable(UserWarning):
    """A cost figure was computed from a rate table older than `STALE_AFTER`."""


def _dec(value: float | int | Decimal) -> Decimal:
    """Floats enter the money path exactly once, here, via their str form.

    `Decimal(0.1)` carries the binary error into every downstream figure and
    `Decimal * float` raises, so callers passing seconds and gigabytes as floats
    would otherwise have to know about Decimal.
    """
    return value if isinstance(value, Decimal) else Decimal(str(value))


def _render(amount: Decimal) -> str:
    """Enough places to see a sub-cent figure, few enough to read a monthly one.

    A create-and-destroy test suite's compute cost lives in the sixth decimal
    place while a warm pool's monthly bill lives in the second, and one fixed
    precision cannot show both without either lying or shouting.
    """
    places = Decimal("0.01") if abs(amount) >= 1 else Decimal("0.000001")
    return f"{amount.quantize(places):f}"


def _qty(quantity: Decimal) -> str:
    """A consumed quantity, rounded only for display.

    Division by 730 hours yields 28 significant digits, and a line item nobody can
    scan is a line item nobody checks the rate table against — which is the one
    job the quantity column has.
    """
    return f"{quantity.quantize(Decimal('0.000001')).normalize():f}"


class Provenance(StrEnum):
    """Where a duration came from. There is no third option and no default.

    MEASURED means a clock ran. PROJECTED means a caller supplied a hypothetical,
    which is also what an estimate's durations are and what the documented
    one-week storage minimum is — nobody timed that week either.
    """

    MEASURED = "measured"
    PROJECTED = "projected"


@dataclass(frozen=True)
class Duration:
    """Seconds, plus how we know them.

    `provenance` has no default, so `Duration(3600)` is a TypeError: the whole
    labelling rule collapses the moment an unlabelled duration can be written,
    because the label that goes missing is always the weaker one.
    """

    seconds: float
    provenance: Provenance

    def __post_init__(self) -> None:
        # A negative duration means the caller's own timing is inverted, and it
        # would silently produce a credit on the report.
        if self.seconds < 0:
            raise ValueError(f"a duration cannot be negative: {self.seconds}s")

    @classmethod
    def measured(cls, seconds: float) -> Duration:
        return cls(seconds=seconds, provenance=Provenance.MEASURED)

    @classmethod
    def projected(cls, seconds: float) -> Duration:
        return cls(seconds=seconds, provenance=Provenance.PROJECTED)

    def __str__(self) -> str:
        return f"{self.seconds:g}s ({self.provenance.value})"


@dataclass(frozen=True)
class EstimatedUSD:
    """Dollars derived from published rates. Not the bill.

    There is deliberately no `ActualUSD` alongside this, and no `__float__`: a
    client that never sees an invoice cannot produce an actual, and the cheapest
    way to launder an estimate into one is `f"${float(x):.2f}"`. A caller who
    genuinely needs the number reads `.amount`, which is one visible step and
    keeps the type name in the traceback.
    """

    amount: Decimal

    def __add__(self, other: EstimatedUSD) -> EstimatedUSD:
        # NotImplemented rather than a coercion, so `estimate + Unpriced` raises
        # instead of quietly treating the unknown line item as free.
        if not isinstance(other, EstimatedUSD):
            return NotImplemented
        return EstimatedUSD(self.amount + other.amount)

    def __str__(self) -> str:
        return f"~${_render(self.amount)} (estimated)"


@dataclass(frozen=True)
class Unpriced:
    """A quantity we can measure but cannot price, because no rate is published.

    Distinct from `EstimatedUSD(Decimal(0))` on purpose. Zero is a claim about the
    bill; this is a claim about the documentation.
    """

    reason: str

    def __str__(self) -> str:
        return f"unpriced — {self.reason}"


#: What a line item's cost can be. Any consumer that switches on this union has to
#: handle the unknown case, which is the property the whole module rests on.
Amount = EstimatedUSD | Unpriced


class BillingLine(StrEnum):
    """The line items AWS bills, spelled as separately as AWS bills them.

    vCPU and memory are two entries rather than one blended GB-second because
    that is how the pricing page prices them, and a blended figure cannot be
    reconciled against a Cost Explorer breakdown that keeps them apart.
    """

    VCPU = "vcpu"
    MEMORY = "memory"
    SNAPSHOT_STORAGE = "snapshot-storage"
    SNAPSHOT_READ = "snapshot-read"
    SNAPSHOT_WRITE = "snapshot-write"


class CostPhase(StrEnum):
    """The lifecycle a `Sandbox` goes through, as the phases that cost money."""

    IMAGE_BUILD = "image-build"
    IMAGE_STORAGE = "image-storage"
    LAUNCH = "launch"
    RUNNING = "running"
    SUSPENDED = "suspended"
    SUSPEND = "suspend"
    RESUME = "resume"


@dataclass(frozen=True)
class RateTable:
    """us-east-1 rates, pinned to when they were read and where from.

    The three booleans and the `None` increment are documented *facts* rather than
    settings, and they are fields because each one is a mistake someone would
    otherwise make in arithmetic: blending vCPU into a GB-second, adding a
    per-invoke charge by analogy with Lambda Functions, rounding a 200 ms exec up
    to a 1-second increment, or subtracting a free tier that does not exist.
    """

    region: str
    source_url: str
    retrieved: date
    vcpu_second: Decimal
    gb_second: Decimal
    storage_gb_month: Decimal
    snapshot_read_gb: Decimal
    snapshot_write_gb: Decimal
    #: Snapshot storage bills at least this long however briefly the snapshot
    #: exists, which is why a create-and-destroy suite's floor is its image rather
    #: than its compute.
    minimum_retention: timedelta
    #: MicroVMs bills per second with no per-request charge. The Lambda free tier
    #: is Functions-only and no MicroVMs free tier is published.
    per_request: Decimal = Decimal("0")
    bills_vcpu_and_memory_separately: bool = True
    free_tier: bool = False
    #: `None` means *not published*, not "one second". Nothing here rounds a
    #: duration up, because inventing an increment would overcharge every short
    #: exec in a report and there is no source for one.
    minimum_billing_increment_sec: float | None = None

    def age(self, today: date | None = None) -> timedelta:
        return (today or date.today()) - self.retrieved

    def is_stale(self, today: date | None = None) -> bool:
        return self.age(today) > STALE_AFTER

    def staleness(self, today: date | None = None) -> str | None:
        """The warning text, or None when the table is fresh."""
        if not self.is_stale(today):
            return None
        return (
            f"rate table for {self.region} was retrieved {self.retrieved.isoformat()}, "
            f"{self.age(today).days} days ago (stale after {STALE_AFTER.days}) — re-read "
            f"{self.source_url} before trusting these figures"
        )

    def warn_if_stale(self, today: date | None = None) -> str | None:
        """Emits `StaleRateTable` and returns the text, so a CLI can also show it.

        Both channels because neither alone reaches everyone: a library caller
        with warnings filtered would never see it, and a CLI that only warned on
        stderr would lose it in a pipeline.
        """
        message = self.staleness(today)
        if message is not None:
            warnings.warn(message, StaleRateTable, stacklevel=3)
        return message


#: Read 2026-08-07 from the Lambda pricing page, us-east-1, and recorded in
#: `docs/PLATFORM.md` under "What actually costs money". Every value here appears
#: there; none is derived. Change one and change that document in the same commit.
RATES = RateTable(
    region="us-east-1",
    source_url="https://aws.amazon.com/lambda/pricing/",
    retrieved=date(2026, 8, 7),
    vcpu_second=Decimal("0.0000276944"),
    gb_second=Decimal("0.0000036667"),
    storage_gb_month=Decimal("0.08"),
    snapshot_read_gb=Decimal("0.00155"),
    snapshot_write_gb=Decimal("0.0038"),
    minimum_retention=timedelta(weeks=1),
)


@dataclass(frozen=True)
class LineItem:
    """One phase's one billing line: what was consumed, and what that costs.

    `quantity` and `unit` are kept beside `amount` so a reader can check the
    arithmetic against the rate table instead of trusting the total, which is the
    only defence against a rate that drifts out of date without anyone noticing.
    """

    phase: CostPhase
    #: None only for a phase with no published rate to attribute it to.
    line: BillingLine | None
    quantity: Decimal
    unit: str
    amount: Amount
    duration: Duration | None = None
    note: str = ""

    def __str__(self) -> str:
        consumed = f"{_qty(self.quantity)} {self.unit}"
        head = f"{self.phase.value:<14} {consumed:<26} {self.amount}"
        return f"{head}  [{self.note}]" if self.note else head


@dataclass(frozen=True)
class Total:
    """A report's total, which is a lower bound whenever anything is unpriced.

    A plain sum cannot express "everything we could price, plus a build AWS will
    not tell us the price of", and the natural way to force it to — dropping the
    unpriced line — is exactly the lie this module exists not to tell.
    """

    priced: EstimatedUSD
    unpriced: tuple[LineItem, ...] = ()

    @property
    def is_lower_bound(self) -> bool:
        return bool(self.unpriced)

    def __str__(self) -> str:
        if not self.unpriced:
            return str(self.priced)
        phases = ", ".join(sorted({item.phase.value for item in self.unpriced}))
        return f"at least {self.priced}, plus {len(self.unpriced)} unpriced ({phases})"


@dataclass(frozen=True)
class CostReport:
    """Per-phase attribution for one sandbox, measured or projected.

    Holds the rate table it was computed against rather than reaching for the
    module default, so a report stays readable — and reproducible — after the
    pinned table is updated.
    """

    label: str
    size: SizeClass
    rates: RateTable
    items: tuple[LineItem, ...]
    #: Set when the rate table was stale at computation time. Carried on the
    #: report because the warning has to survive into whatever renders it.
    staleness: str | None = None

    @property
    def priced(self) -> tuple[LineItem, ...]:
        return tuple(i for i in self.items if isinstance(i.amount, EstimatedUSD))

    @property
    def unpriced(self) -> tuple[LineItem, ...]:
        return tuple(i for i in self.items if isinstance(i.amount, Unpriced))

    @property
    def total(self) -> Total:
        priced = EstimatedUSD(sum((i.amount.amount for i in self.priced), Decimal(0)))
        return Total(priced=priced, unpriced=self.unpriced)

    @property
    def complete(self) -> bool:
        """False whenever any phase has no published rate. See `Total`."""
        return not self.unpriced

    @property
    def fully_measured(self) -> bool:
        """True only if every duration was timed. An estimate is never this."""
        durations = [i.duration for i in self.items if i.duration is not None]
        return bool(durations) and all(d.provenance is Provenance.MEASURED for d in durations)

    def by_phase(self, phase: CostPhase) -> tuple[LineItem, ...]:
        return tuple(i for i in self.items if i.phase is phase)

    def render(self) -> str:
        """Plain text for a CLI. Leads with what the dollars are, not the dollars.

        The header is not decoration: a figure copied out of a terminal loses its
        docstring, so the estimate label and the retrieval date travel with it.
        """
        lines = [
            f"{self.label} — {self.size.describe()}",
            f"dollars are estimates derived from published {self.rates.region} rates "
            f"(retrieved {self.rates.retrieved.isoformat()}); only Cost Explorer knows "
            f"the bill",
        ]
        if self.staleness:
            lines.append(f"WARNING: {self.staleness}")
        lines += [f"  {item}" for item in self.items]
        lines.append(f"total: {self.total}")
        return "\n".join(lines)


def _as_size(size: SizeClass | int) -> SizeClass:
    """Accepts a class or the `minimumMemoryInMiB` that selects one.

    Routed through `sizing.size_class_for` so an off-table baseline is rejected
    here too: a cost figure computed from a size the platform would not accept is
    worse than no figure, because it looks like an answer.
    """
    return size if isinstance(size, SizeClass) else size_class_for(size)


def _compute_lines(
    size: SizeClass, duration: Duration, rates: RateTable, phase: CostPhase
) -> list[LineItem]:
    """Compute for one phase, as two line items.

    Both figures read `baseline_*`. The guest reports the peak and bursts to it,
    but the peak is charged only for the seconds above baseline that are actually
    consumed — which this client cannot observe, so it is left out rather than
    guessed at. See `sizing`.
    """
    seconds = _dec(duration.seconds)
    vcpu = _dec(size.baseline_vcpu)
    memory = _dec(size.baseline_gb)
    return [
        LineItem(
            phase=phase,
            line=BillingLine.VCPU,
            quantity=vcpu * seconds,
            unit="vCPU-seconds",
            amount=EstimatedUSD(vcpu * seconds * rates.vcpu_second),
            duration=duration,
            note=f"{vcpu:f} vCPU baseline",
        ),
        LineItem(
            phase=phase,
            line=BillingLine.MEMORY,
            quantity=memory * seconds,
            unit="GB-seconds",
            amount=EstimatedUSD(memory * seconds * rates.gb_second),
            duration=duration,
            note=f"{memory:f} GB baseline, billed separately from vCPU",
        ),
    ]


def _storage_line(phase: CostPhase, gb: float, held: Duration, rates: RateTable) -> LineItem:
    """Snapshot storage for a hold, with the documented minimum retention applied.

    The minimum is on the rate row itself, so it applies to anything stored there.
    `docs/PLATFORM.md` demonstrates it only for images — a 2 GB image deleted after
    sixty seconds still bills about four cents — and says nothing about a suspend
    snapshot released early, so the note names which case is documented and which
    is the rate row read at face value. Not applying it would understate the one
    line item that dominates a create-and-destroy suite.
    """
    floor = _dec(rates.minimum_retention.total_seconds())
    billed = max(_dec(held.seconds), floor)
    quantity = _dec(gb) * billed / SECONDS_PER_MONTH
    note = f"{gb:g} GB held {held}"
    if billed > _dec(held.seconds):
        note += (
            f"; billed {rates.minimum_retention.days}-day minimum retention ({floor:f}s) instead"
        )
    return LineItem(
        phase=phase,
        line=BillingLine.SNAPSHOT_STORAGE,
        quantity=quantity,
        unit="GB-months",
        amount=EstimatedUSD(quantity * rates.storage_gb_month),
        duration=held,
        note=note,
    )


def _transfer_line(
    phase: CostPhase,
    line: BillingLine,
    gb: float,
    count: int,
    rates: RateTable,
    note_suffix: str = "",
) -> LineItem:
    """A snapshot write or read, billed per GB moved with no time component."""
    rate = rates.snapshot_write_gb if line is BillingLine.SNAPSHOT_WRITE else rates.snapshot_read_gb
    quantity = _dec(gb) * _dec(count)
    return LineItem(
        phase=phase,
        line=line,
        quantity=quantity,
        unit="GB",
        amount=EstimatedUSD(quantity * rate),
        note=f"{count} x {gb:g} GB{note_suffix}",
    )


#: Why the image build has no price. Not a caveat in a docstring: it is the
#: `Unpriced.reason` that appears on the line item and in every total that
#: contains it. The build starts a real MicroVM to run the Dockerfile, so it
#: plausibly is billed, but AWS does not say and we have not measured it.
BUILD_UNPRICED_REASON = (
    "AWS does not publish whether the server-side image build is billed as compute; "
    "the build runs a real MicroVM, so treating it as free would understate the run"
)


def _build_line(duration: Duration | None) -> LineItem:
    """The image build: a real phase with a real duration and no published rate.

    An untimed build still gets a line. Omitting it would leave the report looking
    complete, and the report being visibly incomplete is the whole point of the
    phase appearing at all.
    """
    note = "unknown, not zero — see docs/PLATFORM.md, 'Not published'"
    return LineItem(
        phase=CostPhase.IMAGE_BUILD,
        line=None,
        quantity=_dec(duration.seconds) if duration else Decimal(0),
        unit="seconds" if duration else "seconds (untimed)",
        amount=Unpriced(BUILD_UNPRICED_REASON),
        duration=duration,
        note=note,
    )


def run_report(
    *,
    size: SizeClass | int,
    running: Duration | None = None,
    suspended: Duration | None = None,
    image_build: Duration | None = None,
    image_gb: float | None = None,
    image_retained: Duration | None = None,
    suspend_resume_cycles: int = 0,
    snapshot_gb: float | None = None,
    launched: bool = True,
    rates: RateTable = RATES,
    today: date | None = None,
    label: str = "run",
) -> CostReport:
    """Per-phase attribution for one sandbox's lifecycle.

    Every duration is passed as a `Duration`, so a report built from timed phases
    and one built from a plan are the same shape and are told apart by their own
    contents rather than by which function produced them.

    Passing `image_gb` adds both an image-storage line *and* the unpriced build
    line: an image that has storage cost was built, and the build's price is
    unknown, so a create-and-destroy report is never complete. A caller reusing an
    existing image passes no `image_gb` and pays neither.

    `snapshot_gb` defaults to the baseline memory footprint, which is what
    `docs/PLATFORM.md`'s own worked figures use ("a suspended 2 GB VM pays about
    $0.16 a month" is 2 GB at the storage rate). Whether a suspend snapshot is
    baseline-sized or peak-sized is not documented; override it if you have
    measured otherwise.
    """
    resolved = _as_size(size)
    snapshot = resolved.baseline_gb if snapshot_gb is None else snapshot_gb
    items: list[LineItem] = []

    if image_gb is not None:
        items.append(_build_line(image_build))
        held = image_retained or Duration.projected(rates.minimum_retention.total_seconds())
        items.append(_storage_line(CostPhase.IMAGE_STORAGE, image_gb, held, rates))
    elif image_build is not None:
        items.append(_build_line(image_build))

    if launched:
        # A launch reads a snapshot at the same per-GB rate as a resume. Which
        # snapshot's size that read covers is not documented, so it uses the image
        # when the caller named one and the memory footprint otherwise.
        read_gb = image_gb if image_gb is not None else snapshot
        items.append(
            _transfer_line(
                CostPhase.LAUNCH,
                BillingLine.SNAPSHOT_READ,
                read_gb,
                1,
                rates,
                note_suffix=(
                    "; 'launch or resume' shares one rate, and which snapshot a launch "
                    "reads is undocumented"
                ),
            )
        )

    if running is not None:
        # Wall-clock time in RUNNING bills at baseline whether or not anything is
        # executing. Unlike AgentCore Runtime, there is no free I/O wait, which is
        # why suspension rather than idleness is the lever.
        items += _compute_lines(resolved, running, rates, CostPhase.RUNNING)

    if suspended is not None:
        # No compute line at all: a suspended VM is frozen, so it pays storage only.
        items.append(_storage_line(CostPhase.SUSPENDED, snapshot, suspended, rates))

    if suspend_resume_cycles:
        items.append(
            _transfer_line(
                CostPhase.SUSPEND,
                BillingLine.SNAPSHOT_WRITE,
                snapshot,
                suspend_resume_cycles,
                rates,
            )
        )
        items.append(
            _transfer_line(
                CostPhase.RESUME,
                BillingLine.SNAPSHOT_READ,
                snapshot,
                suspend_resume_cycles,
                rates,
            )
        )

    return CostReport(
        label=label,
        size=resolved,
        rates=rates,
        items=tuple(items),
        staleness=rates.warn_if_stale(today),
    )


def estimate_run(
    *,
    size: SizeClass | int,
    running_seconds: float = 0.0,
    suspended_seconds: float = 0.0,
    image_gb: float | None = None,
    image_retained_seconds: float | None = None,
    suspend_resume_cycles: int = 0,
    snapshot_gb: float | None = None,
    launched: bool = True,
    rates: RateTable = RATES,
    today: date | None = None,
    label: str = "estimate",
) -> CostReport:
    """What a plan will cost, before spending anything.

    Takes plain seconds and marks every one of them PROJECTED, so the resulting
    report can never claim to be measured. That is the difference between this and
    `run_report`: not the arithmetic, which is shared, but what the durations
    admit about themselves.
    """
    return run_report(
        size=size,
        running=Duration.projected(running_seconds) if running_seconds else None,
        suspended=Duration.projected(suspended_seconds) if suspended_seconds else None,
        image_gb=image_gb,
        image_retained=(
            None if image_retained_seconds is None else Duration.projected(image_retained_seconds)
        ),
        suspend_resume_cycles=suspend_resume_cycles,
        snapshot_gb=snapshot_gb,
        launched=launched,
        rates=rates,
        today=today,
        label=label,
    )


@dataclass(frozen=True)
class ResidencyComparison:
    """Running versus suspended for the same VM over the same wall time.

    The gap is roughly two orders of magnitude over a month, which is the entire
    argument for a warm suspended pool. `cycles` is here so the argument stays
    honest: each suspend/resume pays a snapshot write plus a read, and a pool that
    churns spends more on transitions than it saves on residency. The conclusion
    to draw is "avoid churn", not "avoid residency".

    Both sides exclude image build and storage. They are the same for either
    choice, and leaving them in would dilute the ratio the comparison exists to
    show.
    """

    size: SizeClass
    hold: Duration
    cycles: int
    running: CostReport
    suspended: CostReport
    rates: RateTable

    @property
    def ratio(self) -> Decimal:
        """How many times more the running VM costs. Zero-safe by construction:
        snapshot storage always has the minimum-retention floor, so the
        denominator is never zero."""
        return self.running.total.priced.amount / self.suspended.total.priced.amount

    @property
    def per_cycle(self) -> EstimatedUSD:
        """One suspend/resume: a snapshot write plus a read, per GB."""
        gb = _dec(self.size.baseline_gb)
        return EstimatedUSD(gb * (self.rates.snapshot_write_gb + self.rates.snapshot_read_gb))

    @property
    def break_even_seconds(self) -> float:
        """How long a VM must stay suspended for the cycle to pay for itself.

        Below this, suspending and resuming costs more than having left the VM
        running — which is the number a pool scheduler actually needs, and the one
        a bare "100x cheaper" headline hides.
        """
        running_per_sec = (
            _dec(self.size.baseline_vcpu) * self.rates.vcpu_second
            + _dec(self.size.baseline_gb) * self.rates.gb_second
        )
        storage_per_sec = (
            _dec(self.size.baseline_gb) * self.rates.storage_gb_month / SECONDS_PER_MONTH
        )
        floor_sec = _dec(self.rates.minimum_retention.total_seconds())
        churn = self.per_cycle.amount
        # Inside the minimum-retention window the storage charge is a constant, so
        # the equation is linear in the hold; past it storage grows with the hold
        # and the slope changes. Solve the constant branch first and only take the
        # other if the answer falls outside the window.
        candidate = (churn + floor_sec * storage_per_sec) / running_per_sec
        if candidate > floor_sec:
            candidate = churn / (running_per_sec - storage_per_sec)
        return float(candidate)

    def render(self) -> str:
        return "\n".join(
            [
                f"{self.size.describe()} held {self.hold}",
                f"  running:   {self.running.total}",
                f"  suspended: {self.suspended.total} ({self.cycles} cycle(s) included)",
                f"  ratio:     {self.ratio.quantize(Decimal('0.1'))}x cheaper suspended",
                f"  per cycle: {self.per_cycle} — break-even hold "
                f"{self.break_even_seconds:.0f}s, so avoid churn rather than residency",
            ]
        )


def compare_residency(
    *,
    size: SizeClass | int,
    hold_seconds: float,
    cycles: int = 1,
    rates: RateTable = RATES,
    today: date | None = None,
) -> ResidencyComparison:
    """The warm-pool argument, with its own counter-argument attached.

    `cycles` defaults to 1 rather than 0 because a suspension that is never
    resumed is a termination, and pricing it as free transitions is how a pool
    design that churns every few seconds looks affordable on paper.
    """
    resolved = _as_size(size)
    hold = Duration.projected(hold_seconds)
    running = run_report(
        size=resolved,
        running=hold,
        launched=False,
        rates=rates,
        today=today,
        label="left running",
    )
    suspended = run_report(
        size=resolved,
        suspended=hold,
        suspend_resume_cycles=cycles,
        launched=False,
        rates=rates,
        today=today,
        label="suspended",
    )
    return ResidencyComparison(
        size=resolved,
        hold=hold,
        cycles=cycles,
        running=running,
        suspended=suspended,
        rates=rates,
    )
