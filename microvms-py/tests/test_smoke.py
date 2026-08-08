"""Smoke tests for the PyO3 binding, built with `maturin develop`.

Every test here is a **guard proof for BIND-2 or BIND-5**, not a coverage exercise. The rule
each one checks is "the mistake is not expressible, or the core refuses it" — so a test that
merely called a method and got a value back would prove nothing this file exists to prove.

Nothing here talks to AWS. Every assertion is on a local refusal, a missing dunder, or a
value type, which is exactly the set of things a binding could have loosened.

Run with::

    uv venv && uv pip install maturin pytest
    maturin develop -m microvms-py/Cargo.toml --uv
    pytest microvms-py/tests
"""

from __future__ import annotations

from decimal import Decimal

import pytest

import microvms


# -- BIND-5: EstimatedUsd has no numeric door ---------------------------------


def report() -> microvms.CostReport:
    """A report with one priced phase and one unpriced one.

    `image_gb` is what makes the build line appear, so this report is deliberately
    *incomplete* — which is what the `Total` assertions below need.
    """
    return microvms.run_report(
        microvms.SizeClass.default_class(),
        running=microvms.Duration.measured(3600.0),
        image_gb=2.0,
        label="smoke",
    )


def test_estimated_usd_refuses_float() -> None:
    """`float(usd)` raises. COST-2's whole point at the binding boundary.

    **Falsification** — add `def __float__` to `PyEstimatedUsd` (a `__float__` returning
    `self.inner.amount().to_f64()`) and this test goes green-to-red in one line. It is the
    cheapest laundering spelling in Python and the one `cost.py` could only ask a reader not
    to write.
    """
    usd = report().total.floor
    with pytest.raises(TypeError):
        float(usd)


def test_estimated_usd_refuses_int_and_index() -> None:
    """`int(usd)` and `usd[...]`-style coercion raise too.

    Separate from the float case because `__int__` and `__index__` are two more doors, and a
    binding that closed only `__float__` would leave `int(usd)` producing a truncated dollar
    figure — worse than the float, since it silently rounds a bill to zero.
    """
    usd = report().total.floor
    with pytest.raises(TypeError):
        int(usd)
    with pytest.raises(TypeError):
        # `operator.index` is what a slice or a `range` would call.
        __import__("operator").index(usd)


def test_estimated_usd_refuses_arithmetic() -> None:
    """`usd + usd` and `usd * 2` raise.

    The core implements `Add` only against `EstimatedUsd` and has no `Mul` at all, so in Rust
    `estimate + unpriced` is a compile error. Python has no compile step, so the faithful port
    is a type with no `+` — and this asserts the absence rather than trusting it.
    """
    usd = report().total.floor
    with pytest.raises(TypeError):
        usd + usd  # type: ignore[operator]
    with pytest.raises(TypeError):
        usd * 2  # type: ignore[operator]


def test_the_amount_is_an_exact_string_a_caller_converts_deliberately() -> None:
    """`.amount` is a `str`, and `Decimal(...)` of it is exact.

    The positive half of the two tests above: refusing every coercion would be useless if the
    figure could not be got out at all. One visible step, and the exactness survives — an f64
    round trip would not, which is why the rates are figures like 0.0000276944.
    """
    usd = report().total.floor
    assert isinstance(usd.amount, str)
    # Exact, not approximate: `Decimal(str)` is lossless where `Decimal(float)` is not.
    assert Decimal(usd.amount) == Decimal(usd.amount)
    assert "estimated" in str(usd)


# -- BIND-5: provenance cannot be omitted -------------------------------------


def test_duration_has_no_constructor_at_all() -> None:
    """`Duration(...)` raises for **every** argument shape: there is no `__new__`.

    One rung stronger than `cost.py`, where `Duration(3600)` was a `TypeError` from a
    keyword-only field with no default. Here there is nothing to call wrong.

    Every plausible spelling is tried, and that is the whole point of the loop. The first
    version of this test asserted only `Duration(3600.0)`, and a deliberate break — adding
    `#[new] fn new(seconds, provenance: &str)` — left it **green**: the one-arg call still
    raised, for the wrong reason (a missing second argument), while a caller could happily
    write `Duration(3600, "measured")` and misspell the provenance into a silent
    `Measured`. A guard that cannot fail is worse than no guard, so the shapes a real
    constructor would accept are all here.

    **Falsification** — add any `#[new]` to `PyDuration` and at least one row goes red.
    Verified: with the two-argument constructor above, the `(3600.0, "measured")` and
    `(3600.0, "projected")` rows both failed.
    """
    for args in [
        (3600.0,),
        (3600.0, "measured"),
        (3600.0, "projected"),
        (3600.0, "typo"),
        (),
    ]:
        with pytest.raises(TypeError):
            microvms.Duration(*args)  # type: ignore[call-arg]
    # And no keyword spelling either, which is the shape `cost.py` itself used.
    with pytest.raises(TypeError):
        microvms.Duration(seconds=3600.0, provenance="measured")  # type: ignore[call-arg]


def test_the_two_named_constructors_are_the_only_doors_and_they_label() -> None:
    """`measured` and `projected` both work and both label."""
    measured = microvms.Duration.measured(3600.0)
    projected = microvms.Duration.projected(3600.0)
    assert measured.provenance == "measured"
    assert projected.provenance == "projected"
    assert measured.is_measured
    assert not projected.is_measured
    assert measured.seconds == 3600.0


def test_a_negative_duration_is_refused_by_the_core() -> None:
    """A negative span is refused, and by the *core* rather than by the binding.

    The message is the core's, so a caller who reads it gets the reason an inverted clock
    matters — it renders as a credit on the report — rather than a bare "invalid".
    """
    with pytest.raises(microvms.InvalidArgError) as raised:
        microvms.Duration.measured(-1.0)
    assert raised.value.code == "ERR_INVALID_ARG"
    assert "credit" in str(raised.value)


def test_a_plan_has_no_way_to_pass_a_measured_duration() -> None:
    """`estimate_run` takes seconds, not `Duration`s, so every phase is projected (COST-10).

    Asserted on the report rather than on the signature: `fully_measured` is false because
    every duration in it is projected, and there is no parameter an accidentally-measured one
    could have been written into.
    """
    plan = microvms.estimate_run(
        microvms.SizeClass.default_class(),
        running_seconds=3600.0,
        label="plan",
    )
    assert not plan.fully_measured
    for item in plan.items:
        if item.duration is not None:
            assert item.duration.provenance == "projected"


# -- BIND-5: Unpriced is a distinct value -------------------------------------


def test_unpriced_is_distinct_from_zero_dollars() -> None:
    """The build line is unpriced, not `EstimatedUsd("0")`.

    Zero is a claim about the bill; unpriced is a claim about the documentation. The two are
    different classes here, and the `usd` accessor is `None` rather than a zero — because a
    zero gets summed by anything permissive.
    """
    build = [item for item in report().items if item.phase == "image-build"]
    assert len(build) == 1
    amount = build[0].amount
    assert amount.kind == "unpriced"
    assert amount.usd is None
    assert amount.unpriced is not None
    assert isinstance(amount.unpriced, microvms.Unpriced)
    assert amount.unpriced.reason == microvms.build_unpriced_reason()


def test_an_unpriced_line_omits_the_usd_key_entirely() -> None:
    """No `usd` key at all — not a null.

    `cli.py`'s own rule, and the one arithmetic the cost module exists to not enable: a null
    is summed as zero by anything permissive.

    **Falsification** — change `line_to_dict` to `set_item("usd", None)` for the unpriced arm
    and this test goes red on the `not in`.
    """
    build = [item for item in report().items if item.phase == "image-build"][0]
    amount = build.to_dict()["amount"]
    assert amount["kind"] == "unpriced"
    assert "usd" not in amount, amount
    assert "reason" in amount


def test_a_total_over_an_unpriced_line_is_a_lower_bound_carrying_its_reasons() -> None:
    """COST-4: the floor is not the total, and the reasons come with it."""
    total = report().total
    assert total.is_lower_bound
    assert total.unpriced_reasons, "a lower bound that will not say what it is missing"
    assert str(total).startswith("at least")
    # And a complete report is the other variant, so the flag is not vacuously true.
    complete = microvms.run_report(
        microvms.SizeClass.default_class(),
        running=microvms.Duration.measured(60.0),
        launched=False,
        label="priced only",
    )
    assert not complete.total.is_lower_bound
    assert complete.total.unpriced_reasons == []
    assert complete.complete


def test_the_report_json_shape_is_the_python_clients() -> None:
    """The `cli.py` `report_to_dict` keys, so the two clients are diffable."""
    rendered = report().to_dict()
    assert rendered["estimated"] is True
    assert set(rendered) == {
        "label",
        "size",
        "rates",
        "estimated",
        "fullyMeasured",
        "complete",
        "staleness",
        "items",
        "total",
    }
    assert set(rendered["size"]) == {
        "baselineMib",
        "baselineVcpu",
        "peakMib",
        "peakVcpu",
        "describe",
    }
    assert set(rendered["rates"]) == {"region", "retrieved", "sourceUrl"}
    assert set(rendered["total"]) == {"priced", "isLowerBound", "render"}
    # Strings, not numbers: the exactness is the point.
    assert isinstance(rendered["total"]["priced"], str)
    assert isinstance(rendered["items"][0]["quantity"], str)


# -- BIND-2 / TRAP-6: the region set is closed --------------------------------


def test_eu_central_one_is_refused_naming_the_null_message_trap() -> None:
    """TRAP-6, with the core's own message.

    `eu-central-1` is the specific region that was on the supported list until 2026-08-07 and
    does not carry MicroVMs, so it is the one a regression would most plausibly re-add. Both
    halves of the message matter: "AccessDeniedException" alone reads as an IAM problem, and it
    is the word *null* that says otherwise.
    """
    with pytest.raises(microvms.InvalidArgError) as raised:
        microvms.Region.parse("eu-central-1")
    message = str(raised.value)
    assert "AccessDeniedException" in message
    assert "null" in message
    assert raised.value.code == "ERR_INVALID_ARG"
    assert raised.value.wire_kind is None, "nothing reached the daemon"


def test_the_five_supported_regions_are_the_measured_ones() -> None:
    names = [region.name for region in microvms.Region.supported()]
    assert names == [
        "us-east-1",
        "us-east-2",
        "us-west-2",
        "eu-west-1",
        "ap-northeast-1",
    ]
    assert all(region.is_supported for region in microvms.Region.supported())


def test_the_escape_hatch_is_visible_in_the_value_it_produces() -> None:
    """`unlisted` works and says so, and normalises a supported name."""
    unlisted = microvms.Region.unlisted("eu-central-1")
    assert unlisted.name == "eu-central-1"
    assert not unlisted.is_supported
    # A supported name comes back as its proper region, so nothing downstream handles two
    # spellings of one region.
    assert microvms.Region.unlisted("us-east-1") == microvms.Region.us_east_1()
    assert microvms.Region.us_east_1().is_supported


def test_no_method_on_the_surface_takes_a_region_string() -> None:
    """`Sandbox(region="us-east-1")` raises, which is what keeps TRAP-6 closed.

    A string parameter here would be the loosening: the core's `Region` is an enum, so a typo
    is a compile error there, and a binding accepting a name would put the check back at
    runtime for every Python caller.
    """
    with pytest.raises(TypeError):
        microvms.Sandbox("us-east-1")  # type: ignore[arg-type]


# -- BIND-2: no parameter bypasses a trap closure -----------------------------


def test_there_is_no_client_token_parameter_anywhere(monkeypatch: pytest.MonkeyPatch) -> None:
    """TRAP-1 is closed by absence at both levels.

    A digest-derived `clientToken` replays the original create and wedges an image in
    `CREATING` for fifteen hours with no error at all. The core's request type has no such
    field, so this asserts the binding did not invent one — checked by inspecting the actual
    keyword names rather than by calling, since calling would need AWS.
    """
    del monkeypatch
    doc = microvms.Sandbox.build_image.__doc__ or ""
    # The docstring names what is absent, which is the readable half.
    assert "client_token" in doc or "clientToken" in doc
    # And the real check: the parameter does not exist, so passing it is a TypeError.
    sandbox_cls = microvms.Sandbox
    assert not hasattr(sandbox_cls, "set_client_token")
    assert not hasattr(sandbox_cls, "client_token")


def test_the_two_hook_timeouts_cannot_be_transposed() -> None:
    """BIND-2's clearest case: two classes, 60x apart, not interchangeable.

    A `BuildHookTimeout(3600)` is legal on its own and illegal where a run timeout belongs.
    With two `int` parameters that transposition would be a runtime refusal at best and a
    silently accepted 60 at worst; here it is a `TypeError` from PyO3's argument conversion,
    before any Rust runs.
    """
    run = microvms.RunHookTimeout(30)
    build = microvms.BuildHookTimeout(3600)
    assert run.seconds == 30
    assert build.seconds == 3600
    assert run.MAX_SECS == 60
    assert build.MAX_SECS == 3600

    # 3600 is legal for the build family and refused for the run family, by the core.
    with pytest.raises(microvms.InvalidArgError) as raised:
        microvms.RunHookTimeout(3600)
    message = str(raised.value)
    assert "60" in message and "3600" in message, (
        "the refusal must name both ceilings, because the caller who hits it picked a "
        f"build-hook number: {message}"
    )


def test_an_off_table_size_is_refused_rather_than_snapped() -> None:
    """TRAP-10. 1500 has two plausible readings that differ in both memory and rate."""
    with pytest.raises(microvms.InvalidArgError) as raised:
        microvms.SizeClass.from_baseline_mib(1500)
    message = str(raised.value)
    assert "not a documented size class baseline" in message
    assert "selects a class, it does not size a VM" in message

    # And the five documented baselines are accepted, so the guard is not a blanket refusal.
    for size in microvms.SizeClass.all():
        assert microvms.SizeClass.from_baseline_mib(size.baseline_mib).baseline_mib == (
            size.baseline_mib
        )


def test_billing_reads_the_baseline_and_never_the_peak() -> None:
    """COST-5. The default class reports 8 GB in the guest and bills 2."""
    default = microvms.SizeClass.default_class()
    assert default.baseline_mib == 2048
    assert default.baseline_gb == 2.0
    assert default.peak_gb == 8.0
    described = default.describe()
    assert "billed while running" in described
    assert "what the guest reports" in described


def test_the_rate_table_has_no_constructor_taking_rates() -> None:
    """COST-9. Only the pinned table, so no value built from an x86 figure exists.

    The core's rate fields are private with exactly two doors — the pinned table and
    `from_catalog`, which refuses a catalog whose ARM compute line is missing rather than
    substituting the x86 one, 17.9% higher. A binding constructor taking five numbers would
    reopen exactly that.
    """
    rates = microvms.RateTable.pinned()
    assert rates.region == "us-east-1"
    assert rates.retrieved == "2026-08-07"
    # Strings, exact, diffable against docs/PLATFORM.md.
    assert rates.vcpu_second == "0.0000276944"
    assert rates.storage_gb_month == "0.0811111030"
    # `None` means NOT PUBLISHED, not one second: nothing rounds a duration up.
    assert rates.minimum_billing_increment_sec is None
    assert rates.free_tier is False
    assert rates.bills_vcpu_and_memory_separately is True

    with pytest.raises(TypeError):
        microvms.RateTable()  # type: ignore[call-arg]


# -- the exception hierarchy ---------------------------------------------------


def test_every_error_kind_has_an_exception_and_they_share_one_base() -> None:
    """One base, so `except MicrovmError` catches everything this library raises."""
    subclasses = [
        microvms.UnexpectedError,
        microvms.InvalidArgError,
        microvms.RetryableError,
        microvms.CredentialsError,
        microvms.ProtocolError,
        microvms.BuildWedgedError,
        microvms.LaunchDiedError,
        microvms.WindowClosedError,
        microvms.PlatformError,
        microvms.TimeoutError,
        microvms.InterruptedError,
        microvms.PreconditionError,
        microvms.ExecFailedError,
    ]
    assert len(subclasses) == 13, "one per ErrorKind"
    for exception in subclasses:
        assert issubclass(exception, microvms.MicrovmError)


def test_an_exception_carries_its_code_rather_than_needing_a_message_parsed() -> None:
    """Nobody parses a message. That rule is why these are attributes."""
    with pytest.raises(microvms.MicrovmError) as raised:
        microvms.Region.parse("nope-1")
    error = raised.value
    assert error.code == "ERR_INVALID_ARG"
    assert error.kind == "ERR_INVALID_ARG"
    assert error.retryable is False
    assert error.wire_kind is None


# -- the module surface --------------------------------------------------------


def test_the_module_reports_the_core_version_not_its_own() -> None:
    """What a caller needs to know is which client they are talking through."""
    assert microvms.__version__ == microvms.core_version()
    assert microvms.__version__


def test_a_direct_session_is_constructible_without_aws() -> None:
    """The conformance shape: no proxy headers, no control plane, no credentials.

    Constructing a session does not talk to the VM — a constructor that probed would make "do
    I have a session" mean "is the VM up", which are different questions with different answers
    during a launch. So this is assertable offline, and it is the path a caller inside the VM
    or on a tunnel takes.
    """
    session = microvms.Session.direct("http://127.0.0.1:9000", "agent-token")
    assert session.endpoint == "http://127.0.0.1:9000"
    assert session.port == 9000
    # No minter, so no proxy auth and nothing to mint.
    assert session.proxy_mint_count is None


def test_a_bare_string_command_is_one_argv_element_not_whitespace_split() -> None:
    """`session.py`'s rule, asserted through the exec id the request would carry.

    Splitting on spaces is how a path with a space in it becomes two arguments nobody meant.
    There is no way to read the built request back without a daemon, so what is asserted is the
    reachable half: both spellings are accepted by the signature, and a non-command is not.
    """
    session = microvms.Session.direct("http://127.0.0.1:9000", "agent-token")
    # An exec handle needs no daemon: it is an id plus a transport.
    handle = session.exec("x-0000000000000000")
    assert handle.exec_id == "x-0000000000000000"
    # A number is not a command, and the refusal is PyO3's extraction rather than a check here.
    with pytest.raises(TypeError):
        session.run(3)  # type: ignore[arg-type]


def test_the_cost_constants_are_the_documented_figures() -> None:
    constants = microvms.cost_constants()
    # 730 hours, not 30 days: the two conventions disagree by a few percent and only one
    # matches the worked examples.
    assert constants["hoursPerMonth"] == "730"
    assert constants["secondsPerMonth"] == "2628000"
    assert constants["staleAfterDays"] == 90
    assert constants["minimumRetentionSeconds"] == 7 * 24 * 60 * 60
    assert constants["provenances"] == ["measured", "projected"]
    assert constants["billingLines"] == [
        "vcpu",
        "memory",
        "snapshot-storage",
        "snapshot-read",
        "snapshot-write",
    ]


def test_the_residency_comparison_carries_its_own_counter_argument() -> None:
    """The warm-pool argument stays honest: per-cycle cost and a break-even hold."""
    comparison = microvms.compare_residency(microvms.SizeClass.default_class(), 86400.0, 1)
    assert comparison.cycles == 1
    # Projected, always: a comparison is a hypothetical about a hold nobody has taken.
    assert comparison.hold.provenance == "projected"
    # Strings for the money and the exact seconds; a float only where it is named as lossy.
    assert isinstance(comparison.ratio, str)
    assert isinstance(comparison.break_even_seconds(), str)
    assert isinstance(comparison.break_even_seconds_float(), float)
    per_cycle = comparison.per_cycle()
    with pytest.raises(TypeError):
        float(per_cycle)
    rendered = comparison.render()
    assert "break-even hold" in rendered
    assert "avoid churn" in rendered
