"""Rates fetched from the Pricing API: the ARM refusal, the region prefix, drift.

No AWS and no network. Every test drives `FakePricing`, which replays the catalog
shape recorded live on 2026-08-07 — seven MicroVM line items per region, us-east-1
unprefixed and everywhere else prefixed, storage quoted per GB-hour. That shape
*is* the thing under test: the module's whole job is to survive it, and a fake that
smoothed it out would prove nothing.

The recorded figures are the real ones, so `test_the_pinned_table_matches_the_recorded_api`
locks the pinned table against them offline. `mise run live:rates` does the same
against the live API; this catches a hand-edit to `cost.RATES` without waiting for
someone to have credentials.
"""

from __future__ import annotations

from datetime import date, timedelta
from decimal import Decimal
from typing import Any

import pytest

from microvms_agentd.cost import RATES, SECONDS_PER_MONTH, Duration, run_report
from microvms_agentd.pricing import (
    API_SOURCE,
    DRIFT_TOLERANCE,
    HOURS_PER_MONTH,
    MICROVM_LINES,
    MICROVM_REGIONS,
    RateCatalogChanged,
    RateDrift,
    RegionNotPriced,
    canonical_usagetype,
    check_drift,
    fetch_rate_table,
    fetch_rates,
    main,
    render_drift,
)

#: Exactly what `get_products(ServiceCode="AWSLambda")` returned for MicroVM usage
#: types on 2026-08-07, us-east-1: group -> (usagetype, unit, USD). Seven entries,
#: including the two x86 compute rates that a MicroVM can never be billed at — they
#: are in the fake precisely so a test can prove we do not reach for them.
US_EAST_1 = {
    "AWS-Lambda-MicroVM-vCPU-Second-ARM": (
        "Lambda-MicroVM-vCPU-Second-ARM",
        "vCPU-Seconds",
        "0.0000276944",
    ),
    "AWS-Lambda-MicroVM-vCPU-Second": (
        "Lambda-MicroVM-vCPU-Second",
        "vCPU-Seconds",
        "0.0000326557",
    ),
    "AWS-Lambda-MicroVM-Memory-GB-Second-ARM": (
        "Lambda-MicroVM-Memory-GB-Second-ARM",
        "GB-Seconds",
        "0.0000036667",
    ),
    "AWS-Lambda-MicroVM-Memory-GB-Second": (
        "Lambda-MicroVM-Memory-GB-Second",
        "GB-Seconds",
        "0.0000043235",
    ),
    "AWS-Lambda-MicroVM-Snapshot-Read-GB": (
        "Lambda-MicroVM-Snapshot-Read-GB",
        "GB",
        "0.0015467699",
    ),
    "AWS-Lambda-MicroVM-Snapshot-Write-GB": (
        "Lambda-MicroVM-Snapshot-Write-GB",
        "GB",
        "0.0037977138",
    ),
    "AWS-Lambda-MicroVM-Snapshot-Storage-GB-Hour": (
        "Lambda-MicroVM-Snapshot-Storage-GB-Hour",
        "GB-Hours",
        "0.0001111111",
    ),
}

#: ap-northeast-1's ARM compute and snapshot rates, recorded the same day. Kept as
#: real figures rather than a multiplier so the regional-spread test asserts against
#: what AWS charges rather than against arithmetic this file did itself.
AP_NORTHEAST_1_RATES = {
    "AWS-Lambda-MicroVM-vCPU-Second-ARM": "0.0000322421",
    "AWS-Lambda-MicroVM-Memory-GB-Second-ARM": "0.0000042688",
    "AWS-Lambda-MicroVM-Snapshot-Read-GB": "0.0018548941",
    "AWS-Lambda-MicroVM-Snapshot-Write-GB": "0.0046556039",
    "AWS-Lambda-MicroVM-Snapshot-Storage-GB-Hour": "0.0001333333",
}

#: The location prefix each region's usage types carry, us-east-1's being absent.
#: This asymmetry is the bug the coordinator hit first: comparing raw `usagetype`
#: across regions matches nothing and yields a table of NaNs rather than an error.
PREFIXES = {
    "us-east-1": "",
    "us-east-2": "USE2-",
    "us-west-2": "USW2-",
    "eu-west-1": "EU-",
    "ap-northeast-1": "APN1-",
}


class FakePricing:
    """The Pricing API's response shape, with the awkward parts intact.

    Records every `(region, group)` asked for, because several tests are about what
    the module *declined* to look up — the x86 rate on the happy path, in
    particular. A fake that only served answers could not witness that.
    """

    def __init__(
        self,
        *,
        catalog: dict[str, dict[str, tuple[str, str, str]]] | None = None,
        pages: int = 1,
    ) -> None:
        self.catalog = catalog if catalog is not None else {"us-east-1": dict(US_EAST_1)}
        self.pages = pages
        self.asked: list[tuple[str, str]] = []

    # PascalCase because botocore's kwargs are, and a fake that renamed them would
    # accept calls the real client rejects. See the per-file ignore in pyproject.
    def get_products(
        self, *, ServiceCode: str, Filters: list[dict[str, str]], NextToken: str | None = None
    ) -> dict[str, Any]:
        assert ServiceCode == "AWSLambda"
        by_field = {f["Field"]: f["Value"] for f in Filters}
        region, group = by_field["regionCode"], by_field["group"]
        if NextToken is None:
            self.asked.append((region, group))
        entry = self.catalog.get(region, {}).get(group)
        # The real API pages a filtered query at 100 items, so a single match never
        # spans pages — but an empty page followed by a token is what a filter
        # change would look like, and `pages` exists to replay it.
        page = 1 if NextToken is None else int(NextToken)
        products = [] if entry is None or page < self.pages else [self._product(region, group)]
        response: dict[str, Any] = {"PriceList": [_json(p) for p in products]}
        if page < self.pages:
            response["NextToken"] = str(page + 1)
        return response

    def _product(self, region: str, group: str) -> dict[str, Any]:
        usagetype, unit, usd = self.catalog[region][group]
        sku = f"{region}-{group}"
        return {
            "product": {
                "productFamily": "Serverless",
                "sku": sku,
                "attributes": {
                    "regionCode": region,
                    "servicecode": "AWSLambda",
                    "usagetype": PREFIXES[region] + usagetype,
                    "group": group,
                    "locationType": "AWS Region",
                },
            },
            "terms": {
                "OnDemand": {
                    f"{sku}.JRTCKXETXF": {
                        "sku": sku,
                        "offerTermCode": "JRTCKXETXF",
                        "priceDimensions": {
                            f"{sku}.JRTCKXETXF.6YS6EN2CT7": {
                                "unit": unit,
                                "beginRange": "0",
                                "endRange": "Inf",
                                "pricePerUnit": {"USD": usd},
                                "description": f"${usd} per {unit} for {usagetype}",
                            }
                        },
                    }
                }
            },
        }


def _json(product: dict[str, Any]) -> str:
    import json

    return json.dumps(product)


def _catalog(*regions: str) -> dict[str, dict[str, tuple[str, str, str]]]:
    """The recorded us-east-1 catalog replayed for each named region.

    Rates are us-east-1's except where `AP_NORTHEAST_1_RATES` overrides, because
    most tests are about structure and only one is about the regional spread.
    """
    built = {}
    for region in regions:
        entries = dict(US_EAST_1)
        if region == "ap-northeast-1":
            for group, usd in AP_NORTHEAST_1_RATES.items():
                usagetype, unit, _ = entries[group]
                entries[group] = (usagetype, unit, usd)
        built[region] = entries
    return built


# -- the pinned table, checked offline against the recorded API -----------------


def test_the_pinned_table_matches_the_recorded_api() -> None:
    # `mise run live:rates` does this against the live API and needs credentials.
    # This does it against figures recorded on 2026-08-07 and needs nothing, so a
    # hand-edit to `cost.RATES` fails in `check` rather than waiting for someone to
    # run the live tier.
    fetched = fetch_rates("us-east-1", client=FakePricing())
    for line in MICROVM_LINES:
        pinned = getattr(RATES, line.field)
        # Decimal throughout rather than `pytest.approx`, which cannot mix a float
        # tolerance with a Decimal expected value — and a float tolerance is what
        # `cost._dec` exists to keep out of the money path in the first place.
        assert abs(fetched[line.field] - pinned) / pinned < DRIFT_TOLERANCE, line.field


def test_snapshot_storage_is_the_hourly_rate_times_a_seven_hundred_thirty_hour_month() -> None:
    # The pinned table read 0.08 — a round number nobody questioned, and 1.37% low.
    # Asserted through `SECONDS_PER_MONTH` rather than against a literal, because
    # the defect was a second month convention, not a typo.
    fetched = fetch_rates("us-east-1", client=FakePricing())
    hourly = Decimal("0.0001111111")
    assert HOURS_PER_MONTH == SECONDS_PER_MONTH / Decimal(3600) == Decimal(730)
    assert fetched["storage_gb_month"] == hourly * Decimal(730)
    assert RATES.storage_gb_month == fetched["storage_gb_month"]
    assert RATES.storage_gb_month != Decimal("0.08"), "the old hand-read value was 1.37% low"


# -- ARM is the only architecture a MicroVM can be -----------------------------


def test_only_the_arm_rate_is_ever_read() -> None:
    # MicroVMs are ARM64-only: the `Architecture` shape is `enum: ['ARM_64']` with
    # no other member. The x86 rates are in the fake catalog, so this witnesses that
    # the happy path never even asks for them — 17.9% is not a rounding error.
    fake = FakePricing()
    fetch_rates("us-east-1", client=fake)
    groups = {group for _, group in fake.asked}
    assert "AWS-Lambda-MicroVM-vCPU-Second" not in groups
    assert "AWS-Lambda-MicroVM-Memory-GB-Second" not in groups
    assert "AWS-Lambda-MicroVM-vCPU-Second-ARM" in groups


def test_a_missing_arm_rate_raises_rather_than_falling_back_to_x86() -> None:
    # The one substitution that would look entirely healthy: the x86 rate parses,
    # is the same shape, and is 17.9% higher. Every estimate would inflate and
    # nothing would say so.
    catalog = _catalog("us-east-1")
    del catalog["us-east-1"]["AWS-Lambda-MicroVM-vCPU-Second-ARM"]
    with pytest.raises(RateCatalogChanged) as caught:
        fetch_rates("us-east-1", client=FakePricing(catalog=catalog))
    message = str(caught.value)
    assert "ARM64-only" in message
    assert "0.0000326557" in message, "the error must name the rate it refuses to substitute"
    assert "18%" in message


def test_both_compute_rates_missing_still_names_the_field() -> None:
    # The x86 probe is a courtesy, not a requirement: with neither line present the
    # error still has to say which `RateTable` field cannot be filled.
    catalog = _catalog("us-east-1")
    del catalog["us-east-1"]["AWS-Lambda-MicroVM-Memory-GB-Second-ARM"]
    del catalog["us-east-1"]["AWS-Lambda-MicroVM-Memory-GB-Second"]
    with pytest.raises(RateCatalogChanged, match="gb_second"):
        fetch_rates("us-east-1", client=FakePricing(catalog=catalog))


def test_a_missing_snapshot_rate_names_no_x86_sibling() -> None:
    # Snapshot lines have no architecture variant, so there is nothing to refuse.
    catalog = _catalog("us-east-1")
    del catalog["us-east-1"]["AWS-Lambda-MicroVM-Snapshot-Write-GB"]
    with pytest.raises(RateCatalogChanged) as caught:
        fetch_rates("us-east-1", client=FakePricing(catalog=catalog))
    assert "snapshot_write_gb" in str(caught.value)
    assert "ARM64-only" not in str(caught.value)


# -- the region prefix ---------------------------------------------------------


def test_a_prefixed_usage_type_lines_up_with_the_us_east_1_spelling() -> None:
    # us-east-1 usage types are unprefixed and every other region's are not, so a
    # raw string comparison across regions matches nothing. Stripping is what makes
    # them comparable; getting this wrong produced a table of NaNs, not an error.
    assert canonical_usagetype("USW2-Lambda-MicroVM-vCPU-Second-ARM") == (
        "Lambda-MicroVM-vCPU-Second-ARM"
    )
    assert canonical_usagetype("APN1-Lambda-MicroVM-Snapshot-Read-GB") == (
        "Lambda-MicroVM-Snapshot-Read-GB"
    )
    # Idempotent on the unprefixed form, so one code path serves both.
    assert canonical_usagetype("Lambda-MicroVM-vCPU-Second-ARM") == (
        "Lambda-MicroVM-vCPU-Second-ARM"
    )
    # Anchored on the `Lambda-` that follows, so it cannot eat a leading word from
    # some other service's usage type.
    assert canonical_usagetype("USW2-EC2-Instance") == "USW2-EC2-Instance"


def test_every_priced_region_yields_a_full_table() -> None:
    # All five, because the prefix handling is per-region string work and a bug in
    # one prefix would leave exactly one region silently unpriceable.
    fake = FakePricing(catalog=_catalog(*MICROVM_REGIONS))
    for region in MICROVM_REGIONS:
        table = fetch_rate_table(region, client=fake, today=date(2026, 8, 7))
        assert table.region == region
        for line in MICROVM_LINES:
            assert getattr(table, line.field) > 0, f"{region}/{line.field}"


def test_tokyo_is_materially_more_expensive_than_virginia() -> None:
    # The reason fetching per region matters at all. A Tokyo caller reading the
    # us-east-1 table understates their snapshot write bill by over 20%, which is
    # not a rounding difference and would never show up as staleness.
    fake = FakePricing(catalog=_catalog("us-east-1", "ap-northeast-1"))
    virginia = fetch_rate_table("us-east-1", client=fake)
    tokyo = fetch_rate_table("ap-northeast-1", client=fake)
    assert tokyo.vcpu_second / virginia.vcpu_second > Decimal("1.16")
    assert tokyo.snapshot_write_gb / virginia.snapshot_write_gb > Decimal("1.22")
    assert tokyo.storage_gb_month / virginia.storage_gb_month > Decimal("1.19")


def test_a_region_answering_about_a_different_region_is_refused() -> None:
    # The Pricing API ignores a filter field it does not recognise rather than
    # rejecting it, so a renamed field would return us-east-1 rates labelled with
    # whatever region the caller asked for — priced wrong and confidently regional.
    class Misrouting(FakePricing):
        def get_products(self, **kwargs: Any) -> dict[str, Any]:  # type: ignore[override]
            kwargs["Filters"] = [
                f if f["Field"] != "regionCode" else {**f, "Value": "us-east-1"}
                for f in kwargs["Filters"]
            ]
            return super().get_products(**kwargs)

    with pytest.raises(RateCatalogChanged, match="answered about 'us-east-1'"):
        fetch_rates("eu-west-1", client=Misrouting(catalog=_catalog("us-east-1", "eu-west-1")))


# -- the five-region reality ---------------------------------------------------


def test_an_unpriced_region_raises_and_names_the_five_that_work() -> None:
    # Not an empty table and not a KeyError. And not a lookup against
    # MICROVM_REGIONS either — the fetch is attempted first, so a region AWS adds
    # after this list was written returns rates instead of an error.
    with pytest.raises(RegionNotPriced) as caught:
        fetch_rate_table("eu-central-1", client=FakePricing(catalog=_catalog("us-east-1")))
    message = str(caught.value)
    for region in MICROVM_REGIONS:
        assert region in message
    # The caveat that saves someone auditing a policy that is fine: an unsupported
    # region answers lambda-microvms with AccessDeniedException and a null message.
    assert "AccessDeniedException" in message
    assert "MICROVM_REGIONS" in message, "the error must say what to update"


def test_a_region_missing_only_some_lines_is_a_catalog_change_not_an_unpriced_region() -> None:
    # The two failures are different repairs — update the region list, versus work
    # out what AWS renamed — so collapsing them would send the reader to the wrong
    # one. Four of five present must not read as "this region has no MicroVMs".
    catalog = _catalog("us-east-1")
    del catalog["us-east-1"]["AWS-Lambda-MicroVM-Snapshot-Read-GB"]
    with pytest.raises(RateCatalogChanged):
        fetch_rates("us-east-1", client=FakePricing(catalog=catalog))


def test_the_documented_region_list_is_the_five_that_were_measured() -> None:
    # Pinned so that adding a region is a deliberate edit with a date behind it.
    # Compared as a set: the single definition lives in `sandbox` as a frozenset, so
    # ordering here is an artifact of sorting it rather than anything to pin.
    assert set(MICROVM_REGIONS) == {
        "us-east-1",
        "us-east-2",
        "us-west-2",
        "eu-west-1",
        "ap-northeast-1",
    }


def test_the_region_list_is_not_a_second_copy_of_sandboxs() -> None:
    # The `cli.py` copy had drifted to include `eu-central-1` while `sandbox`'s was
    # correct, so this asserts the identity rather than the contents: a value equal
    # by luck would pass the test above and diverge on the next edit.
    from microvms_agentd import sandbox

    assert set(MICROVM_REGIONS) == set(sandbox.MICROVM_REGIONS)
    assert "eu-central-1" not in MICROVM_REGIONS


# -- catalog shape guards ------------------------------------------------------


def test_a_restated_unit_is_refused_rather_than_silently_rescaled() -> None:
    # The only signal available if AWS restates storage per GB-month: the number
    # moves 730x and every downstream arithmetic check still passes, because they
    # all read this same table.
    catalog = _catalog("us-east-1")
    usagetype, _, usd = catalog["us-east-1"]["AWS-Lambda-MicroVM-Snapshot-Storage-GB-Hour"]
    catalog["us-east-1"]["AWS-Lambda-MicroVM-Snapshot-Storage-GB-Hour"] = (
        usagetype,
        "GB-Months",
        usd,
    )
    with pytest.raises(RateCatalogChanged, match="quoted per 'GB-Months'"):
        fetch_rates("us-east-1", client=FakePricing(catalog=catalog))


def test_a_group_repointed_at_another_line_item_is_refused() -> None:
    # `group` is what we filter on because it is region-independent; `usagetype` is
    # the cross-check. Without it, a group renamed onto a different line item would
    # fill a rate field from the wrong meter and parse perfectly.
    catalog = _catalog("us-east-1")
    _, unit, usd = catalog["us-east-1"]["AWS-Lambda-MicroVM-vCPU-Second-ARM"]
    catalog["us-east-1"]["AWS-Lambda-MicroVM-vCPU-Second-ARM"] = (
        "Lambda-MicroVM-vCPU-Second-GRAVITON",
        unit,
        usd,
    )
    with pytest.raises(RateCatalogChanged, match="repointed"):
        fetch_rates("us-east-1", client=FakePricing(catalog=catalog))


def test_a_tiered_rate_is_refused_rather_than_flattened() -> None:
    # Every MicroVM line is a single 0-to-Inf dimension today. A tier would make
    # "the rate" ambiguous, and picking the first would silently choose one.
    class Tiered(FakePricing):
        def _product(self, region: str, group: str) -> dict[str, Any]:
            product = super()._product(region, group)
            (term,) = product["terms"]["OnDemand"].values()
            (dimension,) = list(term["priceDimensions"].values())
            term["priceDimensions"]["second-tier"] = {**dimension, "beginRange": "1000000"}
            return product

    with pytest.raises(RateCatalogChanged, match="price dimensions"):
        fetch_rates("us-east-1", client=Tiered())


def test_a_paged_empty_first_page_is_followed_rather_than_read_as_unpriced() -> None:
    # A filtered query returns one item today, so paging is defensive — but an empty
    # first page carrying a NextToken would otherwise read as "this region does not
    # price MicroVMs" and send the reader to the region list for nothing.
    fetched = fetch_rates("us-east-1", client=FakePricing(pages=3))
    assert fetched["vcpu_second"] == Decimal("0.0000276944")


# -- the fetched table's own provenance ---------------------------------------


def test_a_fetched_table_says_it_came_from_the_api_not_from_a_page() -> None:
    # `RateTable.staleness` interpolates `source_url` into "re-read <url>", so a
    # fetched table that carried the pricing page URL would tell a reader to go do
    # by hand the thing this module exists to automate.
    table = fetch_rate_table("us-west-2", client=FakePricing(catalog=_catalog("us-west-2")))
    assert table.source_url == API_SOURCE
    assert not table.source_url.startswith("http"), "not a page a human reads"
    assert "live:rates" in table.source_url
    assert table.retrieved == date.today(), "fetched now, not when someone last read a page"


def test_a_fetched_table_carries_the_billing_rules_the_catalog_does_not_price() -> None:
    # The catalog prices line items and says nothing about a one-week storage
    # minimum, a per-request charge, a billing increment, or a free tier. Those stay
    # hand-read, and dropping the retention floor would understate a
    # create-and-destroy suite by four orders of magnitude.
    table = fetch_rate_table("us-east-1", client=FakePricing())
    assert table.minimum_retention == timedelta(weeks=1)
    assert table.per_request == Decimal("0")
    assert table.free_tier is False
    assert table.minimum_billing_increment_sec is None


def test_a_fetched_table_drops_straight_into_a_cost_report() -> None:
    # The point of returning a `cost.RateTable` rather than a dict: every figure in
    # `cost` is computed against whatever table it is handed, so a regional table
    # needs no second arithmetic path.
    tokyo = fetch_rate_table(
        "ap-northeast-1", client=FakePricing(catalog=_catalog("ap-northeast-1"))
    )
    here = run_report(size=2048, running=Duration.measured(3600), launched=False, rates=tokyo)
    there = run_report(size=2048, running=Duration.measured(3600), launched=False)
    assert here.total.priced.amount > there.total.priced.amount
    assert "ap-northeast-1" in here.render()


# -- drift, which is what replaces the staleness warning ----------------------


def test_drift_reports_every_line_not_only_the_moved_ones() -> None:
    # A check that printed only findings cannot be told apart from one whose
    # credentials failed over to an empty catalog: both print nothing and exit 0.
    drifts = check_drift(client=FakePricing())
    assert len(drifts) == len(MICROVM_LINES)
    assert {d.field for d in drifts} == {line.field for line in MICROVM_LINES}
    assert not any(d.drifted for d in drifts)


def test_drift_measures_the_pinned_table_against_live_values() -> None:
    # The measurement the 90-day warning could not make. A 10% move on one rate is
    # reported as 10% on that rate, and the other four stay quiet.
    catalog = _catalog("us-east-1")
    usagetype, unit, _ = catalog["us-east-1"]["AWS-Lambda-MicroVM-Snapshot-Write-GB"]
    catalog["us-east-1"]["AWS-Lambda-MicroVM-Snapshot-Write-GB"] = (usagetype, unit, "0.00418")
    drifts = check_drift(client=FakePricing(catalog=catalog))
    (moved,) = [d for d in drifts if d.drifted]
    assert moved.field == "snapshot_write_gb"
    assert moved.delta > 0
    assert Decimal("0.09") < moved.relative < Decimal("0.11")
    text = render_drift(drifts)
    assert "snapshot_write_gb" in text
    assert "docs/PLATFORM.md" in text, "the reader has to know both places to update"


def test_the_rounding_in_the_pinned_snapshot_rates_is_not_reported_as_drift() -> None:
    # 0.00155 for 0.0015467699 is 0.21% high, by rounding rather than by drift. A
    # tolerance under that reports drift on a correct table, and a check that always
    # fires is a check nobody reads.
    drifts = {d.field: d for d in check_drift(client=FakePricing())}
    read = drifts["snapshot_read_gb"]
    assert read.relative != 0, "the pinned figure really is rounded, so this is a real tolerance"
    assert abs(read.relative) < DRIFT_TOLERANCE
    assert not read.drifted


def test_drift_exits_nonzero_so_the_live_task_fails_rather_than_reports(
    capsys: pytest.CaptureFixture[str], monkeypatch: pytest.MonkeyPatch
) -> None:
    # Same argument as `schema:check`: a stale pinned artifact is worse than none,
    # because a consumer trusts a price *because* it is pinned.
    import microvms_agentd.pricing as pricing

    catalog = _catalog("us-east-1")
    usagetype, unit, _ = catalog["us-east-1"]["AWS-Lambda-MicroVM-vCPU-Second-ARM"]
    catalog["us-east-1"]["AWS-Lambda-MicroVM-vCPU-Second-ARM"] = (usagetype, unit, "0.00005")
    monkeypatch.setattr(pricing, "_pricing_client", lambda: FakePricing(catalog=catalog))
    assert main([]) == 1
    assert "DRIFT" in capsys.readouterr().out

    monkeypatch.setattr(pricing, "_pricing_client", lambda: FakePricing())
    assert main([]) == 0
    assert "match the api within tolerance" in capsys.readouterr().out


def test_a_drift_line_renders_both_figures_and_the_percentage() -> None:
    # A percentage alone cannot be checked against the pricing page, and two raw
    # ten-digit figures cannot be scanned. The line carries both.
    line = str(
        RateDrift(
            field="storage_gb_month",
            usagetype="Lambda-MicroVM-Snapshot-Storage-GB-Hour",
            pinned=Decimal("0.08"),
            live=Decimal("0.0811111030"),
        )
    )
    assert "0.08" in line
    assert "0.0811111030" in line
    assert "+1.39%" in line
    assert "DRIFT" in line, "1.39% is over tolerance — this is the defect we just fixed"
