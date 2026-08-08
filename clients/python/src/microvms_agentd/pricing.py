"""The rate table, fetched from the AWS Pricing API rather than transcribed.

`cost.RATES` is pinned by hand from the Lambda pricing page, and a 90-day warning
was the only thing standing between a caller and a silently stale price. That
warning cannot tell you *whether* a rate moved — only that nobody has looked. This
module looks, because the Pricing API carries the MicroVM line items directly:
seven of them, in five regions, queryable for free.

Three things about that catalog decide the shape of everything below.

**There is an ARM and a non-ARM rate for compute, 17.9% apart.** MicroVMs are
ARM64-only — `lambda-microvms`'s `Architecture` shape is `enum: ['ARM_64']` with no
other member — so only the ARM rate can ever apply and the x86 one is a trap. The
hand-pinned table happened to use the ARM figure, correctly but by luck. A fetch
that fell back to the sibling when the ARM line went missing would overstate every
estimate by 17.9% and look entirely healthy doing it, so a missing ARM rate raises.

**us-east-1 usage types are unprefixed and every other region's are not.**
`Lambda-MicroVM-vCPU-Second-ARM` in us-east-1 is `USW2-Lambda-MicroVM-vCPU-Second-ARM`
in us-west-2, so comparing raw `usagetype` strings across regions matches nothing
and yields a table of holes rather than an error. The `group` attribute *is*
region-independent, which is what we filter on; the prefix strip exists to check
the two agree, so a renamed group cannot quietly price the wrong line.

**Storage is quoted per GB-hour and `RateTable` holds per GB-month.** The
conversion goes through `cost.SECONDS_PER_MONTH` rather than a fresh 730 literal:
two conventions for the same month is how the pinned table came to be 1.37% low.

Regional spread is not a rounding detail. eu-west-1 is 5.3% over us-east-1 on
compute and 19% on snapshot storage; ap-northeast-1 is 16.4% and 20%. A caller in
Tokyo reading the us-east-1 table understates their snapshot write bill by 22.6%.
"""

from __future__ import annotations

import json
import re
from dataclasses import dataclass
from datetime import date
from decimal import Decimal
from typing import Any

from .cost import RATES, SECONDS_PER_MONTH, RateTable
from .sandbox import MICROVM_REGIONS as _MICROVM_REGIONS

SERVICE_CODE = "AWSLambda"

#: Where the Pricing API itself is served. Only three regions serve it
#: (us-east-1, ap-south-1, eu-central-1), and the region being *priced* is a filter
#: value rather than an endpoint — asking a us-east-1 endpoint about
#: ap-northeast-1 is the normal case, not a mistake.
PRICING_API_REGION = "us-east-1"

#: Pricing coverage and service availability coincide, measured 2026-08-07: the same
#: five regions that price MicroVMs are the five that answer `ListMicrovms`, and the
#: unpriced ones answer `AccessDeniedException` with a *null* message field that is
#: indistinguishable from a genuine IAM denial. So the region list `sandbox` keeps
#: for refusing a launch is the same list this module needs for explaining an empty
#: catalog, and it is imported rather than restated: the third copy of it, in
#: `cli.py`, had already drifted to include `eu-central-1`.
#:
#: Here it is only ever used to *write* the error, never to refuse: the fetch runs
#: first and this is consulted only when the catalog came back empty. So a region
#: AWS adds after today returns rates from this module rather than a refusal, and
#: the cost of the list being stale is one misleading sentence.
MICROVM_REGIONS = tuple(sorted(_MICROVM_REGIONS))

#: Strips the location prefix off a usage type: `USW2-Lambda-...` -> `Lambda-...`.
#: Anchored on the `Lambda-` that follows so it cannot eat part of a name that
#: happens to start with capitals.
_LOCATION_PREFIX = re.compile(r"^[A-Z0-9]+-(?=Lambda-)")

#: Hours in the month AWS's GB-month rate assumes, derived from `cost` rather than
#: written again. See the module docstring: the second 730 is where the drift got in.
HOURS_PER_MONTH = SECONDS_PER_MONTH / Decimal(3600)

#: What a fetched table's `source_url` says. Deliberately not an https URL: no page
#: returns these seven figures for one region, and the whole point of the field on
#: a fetched table is that a reader can tell at a glance it was not read off a page
#: by a human. `RateTable.staleness` interpolates it, so it has to read as an
#: instruction on its own.
API_SOURCE = (
    f"aws pricing api: get_products(ServiceCode={SERVICE_CODE}) — re-run `mise run live:rates`"
)


class PricingError(Exception):
    """Base for every way the rate catalog can fail to answer."""


class RegionNotPriced(PricingError):
    """The Pricing API has no MicroVM line items for this region.

    Which, measured, also means the region does not run MicroVMs at all — see
    `MICROVM_REGIONS`. Raised rather than returning a partial table, because a
    table with holes prices a run at less than it costs.
    """


class RateCatalogChanged(PricingError):
    """A line item is present but not the shape this module knows how to read.

    A missing ARM rate, a unit that changed, two products where there was one, a
    region that answered about a different region. Every one of them is a reason to
    stop rather than to substitute: the failure mode of guessing is a plausible
    number, which is the only kind nobody checks.
    """


@dataclass(frozen=True)
class RateLine:
    """One `RateTable` field and the catalog entry that fills it.

    `unit` is asserted against what the API returns rather than assumed. It is the
    only signal available if AWS restates storage per GB-month: the number would
    change by 730x and every arithmetic check downstream would still pass, because
    they all read this same table.
    """

    #: The `RateTable` field this fills.
    field: str
    #: The region-independent `group` attribute, which is what we filter on.
    group: str
    #: The us-east-1 spelling of `usagetype`, i.e. the canonical form after the
    #: location prefix is stripped. Cross-checked against `group`, never trusted
    #: across regions on its own.
    usagetype: str
    #: The unit the API must report for this line.
    unit: str
    #: True for storage, quoted per GB-hour where `RateTable` holds per GB-month.
    per_hour: bool = False
    #: The x86 sibling, where one exists. Probed *only* when the ARM line is
    #: missing, so the error can name the rate it is refusing to substitute rather
    #: than leaving the reader to wonder whether one exists.
    x86_group: str | None = None


#: The five figures `RateTable` needs, out of the seven line items the catalog
#: carries. The two we skip are the x86 compute rates, which a MicroVM can never
#: be billed at.
MICROVM_LINES: tuple[RateLine, ...] = (
    RateLine(
        field="vcpu_second",
        group="AWS-Lambda-MicroVM-vCPU-Second-ARM",
        usagetype="Lambda-MicroVM-vCPU-Second-ARM",
        unit="vCPU-Seconds",
        x86_group="AWS-Lambda-MicroVM-vCPU-Second",
    ),
    RateLine(
        field="gb_second",
        group="AWS-Lambda-MicroVM-Memory-GB-Second-ARM",
        usagetype="Lambda-MicroVM-Memory-GB-Second-ARM",
        unit="GB-Seconds",
        x86_group="AWS-Lambda-MicroVM-Memory-GB-Second",
    ),
    RateLine(
        field="storage_gb_month",
        group="AWS-Lambda-MicroVM-Snapshot-Storage-GB-Hour",
        usagetype="Lambda-MicroVM-Snapshot-Storage-GB-Hour",
        unit="GB-Hours",
        per_hour=True,
    ),
    RateLine(
        field="snapshot_read_gb",
        group="AWS-Lambda-MicroVM-Snapshot-Read-GB",
        usagetype="Lambda-MicroVM-Snapshot-Read-GB",
        unit="GB",
    ),
    RateLine(
        field="snapshot_write_gb",
        group="AWS-Lambda-MicroVM-Snapshot-Write-GB",
        usagetype="Lambda-MicroVM-Snapshot-Write-GB",
        unit="GB",
    ),
)


def canonical_usagetype(usagetype: str) -> str:
    """The us-east-1 spelling of a usage type, whichever region it came from."""
    return _LOCATION_PREFIX.sub("", usagetype)


def _pricing_client() -> Any:
    """A Pricing API client, imported lazily like the rest of the AWS surface.

    Lazy so `cost` and `sizing` stay importable — and the test suite runnable —
    with no boto3 and no credentials.
    """
    import boto3
    from botocore.config import Config as BotoConfig

    return boto3.Session(region_name=PRICING_API_REGION).client(
        "pricing", config=BotoConfig(retries={"max_attempts": 10, "mode": "standard"})
    )


def _products(client: Any, region: str, group: str) -> list[dict[str, Any]]:
    """Every product for one group in one region. Normally exactly one.

    Filtering on `group` *and* `regionCode` is what keeps this cheap: five
    single-item calls instead of paging 10,471 Lambda products to find 35 MicroVM
    ones. The token loop stays anyway — the filters are applied server-side, so an
    empty first page with a `NextToken` would otherwise read as "this region does
    not price MicroVMs".
    """
    collected: list[dict[str, Any]] = []
    token: str | None = None
    while True:
        kwargs: dict[str, Any] = {
            "ServiceCode": SERVICE_CODE,
            "Filters": [
                {"Type": "TERM_MATCH", "Field": "group", "Value": group},
                {"Type": "TERM_MATCH", "Field": "regionCode", "Value": region},
            ],
        }
        if token:
            kwargs["NextToken"] = token
        response = client.get_products(**kwargs)
        collected += [json.loads(raw) for raw in response["PriceList"]]
        token = response.get("NextToken")
        if not token:
            return collected


def _price(product: dict[str, Any], line: RateLine, region: str) -> Decimal:
    """The one USD figure on a product, with every assumption about it checked.

    Each check is a way the catalog could change under us and still parse. The
    region check in particular: the API answers a filter it does not understand by
    ignoring it, so a typo'd field name would return us-east-1 rates labelled with
    whatever region the caller asked for.
    """
    attributes = product.get("product", {}).get("attributes", {})
    got_region = attributes.get("regionCode")
    if got_region != region:
        raise RateCatalogChanged(
            f"asked the pricing api for {line.group} in {region} and it answered about "
            f"{got_region!r}; the rate would have been labelled with the wrong region"
        )
    got_usagetype = canonical_usagetype(attributes.get("usagetype", ""))
    if got_usagetype != line.usagetype:
        raise RateCatalogChanged(
            f"group {line.group} in {region} now carries usagetype "
            f"{attributes.get('usagetype')!r}, which is {got_usagetype!r} once the location "
            f"prefix is stripped, not the expected {line.usagetype!r} — the group may have "
            f"been repointed at a different line item"
        )

    terms = product.get("terms", {}).get("OnDemand", {})
    if len(terms) != 1:
        raise RateCatalogChanged(
            f"{line.usagetype} in {region} has {len(terms)} on-demand terms, expected 1; "
            "picking one arbitrarily would silently choose a rate"
        )
    (term,) = terms.values()
    dimensions = term.get("priceDimensions", {})
    if len(dimensions) != 1:
        raise RateCatalogChanged(
            f"{line.usagetype} in {region} has {len(dimensions)} price dimensions, expected 1 — "
            "a tiered rate cannot be flattened into one number"
        )
    (dimension,) = dimensions.values()

    unit = dimension.get("unit")
    if unit != line.unit:
        raise RateCatalogChanged(
            f"{line.usagetype} in {region} is now quoted per {unit!r}, not {line.unit!r}; "
            "the conversion into the rate table no longer holds and every downstream "
            "figure would still look plausible"
        )
    usd = dimension.get("pricePerUnit", {}).get("USD")
    if usd is None:
        raise RateCatalogChanged(
            f"{line.usagetype} in {region} has no USD price, only "
            f"{sorted(dimension.get('pricePerUnit', {}))}"
        )
    # Decimal from the API's own string. The figures carry ten significant digits,
    # which float cannot hold, and `cost` is Decimal end to end for that reason.
    return Decimal(usd)


def _missing(client: Any, region: str, line: RateLine) -> PricingError:
    """The error for a line the catalog no longer has.

    Split out because the ARM case needs to say more than "missing": the x86
    sibling is right there, 17.9% higher, and the tempting fix is to use it.
    """
    if line.x86_group is None:
        return RateCatalogChanged(
            f"{region} prices MicroVMs but has no {line.group} line item, so "
            f"{line.field} cannot be filled"
        )
    siblings = _products(client, region, line.x86_group)
    if not siblings:
        return RateCatalogChanged(
            f"{region} has neither {line.group} nor {line.x86_group}, so {line.field} "
            "cannot be filled"
        )
    x86 = _price(
        siblings[0],
        RateLine(
            field=line.field,
            group=line.x86_group,
            usagetype=canonical_usagetype(
                siblings[0]["product"]["attributes"].get("usagetype", "")
            ),
            unit=line.unit,
        ),
        region,
    )
    return RateCatalogChanged(
        f"{region} has no {line.group}, only the x86 rate {line.x86_group} at ${x86}. "
        "MicroVMs are ARM64-only — the Architecture shape is enum ['ARM_64'] with no other "
        f"member — so the x86 rate can never apply and is not substituted for {line.field}; "
        "doing so would overstate every compute figure by roughly 18%"
    )


def fetch_rates(region: str, *, client: Any = None) -> dict[str, Decimal]:
    """The five `RateTable` rate fields for one region, live.

    All or nothing. A partial table would price a run at less than it costs, and
    the caller has no way to see which field was quietly left at a stale value.
    """
    api = client if client is not None else _pricing_client()
    found: dict[str, Decimal] = {}
    failures: list[PricingError] = []
    for line in MICROVM_LINES:
        products = _products(api, region, line.group)
        if len(products) > 1:
            raise RateCatalogChanged(
                f"{line.group} in {region} returned {len(products)} products where one group "
                "in one region has always been one product; the catalog gained a dimension "
                "this module does not know how to choose between"
            )
        if not products:
            failures.append(_missing(api, region, line))
            continue
        rate = _price(products[0], line, region)
        # The API quotes storage per GB-hour. Multiplying here rather than at every
        # use site keeps `RateTable` one convention deep, which is the whole reason
        # `SECONDS_PER_MONTH` exists.
        found[line.field] = rate * HOURS_PER_MONTH if line.per_hour else rate

    if len(failures) == len(MICROVM_LINES):
        # Every line missing is the region, not the catalog. See MICROVM_REGIONS for
        # why this message carries the IAM caveat.
        raise RegionNotPriced(
            f"the pricing api has no MicroVM line items for {region!r}. MicroVMs are priced "
            f"in {', '.join(MICROVM_REGIONS)} and, measured 2026-08-07, only those regions "
            "run MicroVMs at all — an unsupported region answers lambda-microvms with "
            "AccessDeniedException and a null message, which reads exactly like an IAM "
            "problem it is not. Check the region before auditing a policy. If AWS has added "
            "a region since, the list in microvms_agentd.pricing.MICROVM_REGIONS is what "
            "needs updating; this call attempts the fetch regardless of it, so a genuinely "
            "new region will already have returned rates rather than reaching this error."
        )
    if failures:
        raise failures[0]
    return found


def fetch_rate_table(region: str, *, client: Any = None, today: date | None = None) -> RateTable:
    """A `RateTable` for one region, built from live Pricing API figures.

    `minimum_retention` and the four documented billing facts are carried over from
    the pinned table rather than fetched: the catalog prices line items and says
    nothing about a one-week storage minimum, a per-request charge, a billing
    increment, or a free tier. So a fetched table is authoritative on *rates* and
    still hand-read on *rules*, and `source_url` says which by naming the API.
    """
    rates = fetch_rates(region, client=client)
    return RateTable(
        region=region,
        source_url=API_SOURCE,
        retrieved=today or date.today(),
        minimum_retention=RATES.minimum_retention,
        **rates,
    )


#: How far a pinned rate may sit from the live one before it counts as drift. The
#: pinned snapshot rates are three-significant-figure roundings of ten-digit API
#: figures — 0.00155 for 0.0015467699 is 0.21% high — so a tolerance under that
#: reports drift on a table that is correct, and a check that always fires is a
#: check nobody reads. AWS price changes land in whole percentage points, well clear
#: of this.
DRIFT_TOLERANCE = Decimal("0.005")


@dataclass(frozen=True)
class RateDrift:
    """One pinned rate against its live value.

    Every line is reported, drifted or not. A drift check that printed only its
    findings could not be told apart from one whose credentials silently failed
    over to zero line items.
    """

    field: str
    usagetype: str
    pinned: Decimal
    live: Decimal

    @property
    def delta(self) -> Decimal:
        return self.live - self.pinned

    @property
    def relative(self) -> Decimal:
        # No zero guard: a pinned rate of zero would mean the table claims a
        # billable line item is free, and ZeroDivisionError is a better outcome than
        # a drift report that quietly skipped it.
        return self.delta / self.pinned

    @property
    def drifted(self) -> bool:
        return abs(self.relative) > DRIFT_TOLERANCE

    def __str__(self) -> str:
        percent = (self.relative * 100).quantize(Decimal("0.01"))
        mark = "DRIFT" if self.drifted else "ok   "
        return (
            f"{mark} {self.field:<18} pinned {self.pinned:<14f} live {self.live:<14f} "
            f"{percent:+}%  [{self.usagetype}]"
        )


def check_drift(*, rates: RateTable = RATES, client: Any = None) -> tuple[RateDrift, ...]:
    """The pinned table against live Pricing API values, one entry per rate.

    This is what replaces the 90-day staleness warning with a measurement. The
    warning could only ever say that nobody had looked; this says whether anything
    moved, and by how much.

    Compares against `rates.region`, so checking a table pinned to us-east-1 never
    reports the 5-19% regional spread as drift.
    """
    live = fetch_rates(rates.region, client=client)
    return tuple(
        RateDrift(
            field=line.field,
            usagetype=line.usagetype,
            pinned=getattr(rates, line.field),
            live=live[line.field],
        )
        for line in MICROVM_LINES
    )


def render_drift(drifts: tuple[RateDrift, ...], *, rates: RateTable = RATES) -> str:
    """Plain text for the `live:rates` task, leading with what a reader must do."""
    moved = [d for d in drifts if d.drifted]
    lines = [
        f"pinned rate table for {rates.region} (retrieved {rates.retrieved.isoformat()}) vs the "
        f"aws pricing api",
        f"tolerance {(DRIFT_TOLERANCE * 100).normalize():f}%, which is the rounding in the "
        f"pinned figures "
        f"rather than a licence to drift",
    ]
    lines += [f"  {d}" for d in drifts]
    if moved:
        fields = ", ".join(d.field for d in moved)
        lines.append(
            f"{len(moved)} rate(s) moved: {fields} — update cost.RATES and docs/PLATFORM.md "
            "in the same commit"
        )
    else:
        lines.append(f"all {len(drifts)} rates match the api within tolerance")
    return "\n".join(lines)


def main(argv: list[str] | None = None) -> int:
    """Exit 1 on drift, so the `live:rates` task fails rather than reports.

    A stale rate table is the same failure class as a stale generated schema, which
    this repo already fails on: a consumer trusts a pinned price *because* it is
    pinned, and nothing in a diff reveals that it describes a bill nobody pays.

    Takes no `--region`. There is exactly one pinned table and it is us-east-1;
    a flag that fetched some other region's table and then compared it against
    itself would pass by construction, which is the one thing a drift check must
    not do. Cross-region rates are `fetch_rate_table`'s job, not this check's.
    """
    import argparse

    argparse.ArgumentParser(
        description="Compare cost.RATES against the AWS Pricing API."
    ).parse_args(argv)
    drifts = check_drift()
    print(render_drift(drifts))
    return 1 if any(d.drifted for d in drifts) else 0


if __name__ == "__main__":  # pragma: no cover
    raise SystemExit(main())
