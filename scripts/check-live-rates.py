#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.11"
# dependencies = ["boto3>=1.40"]
# ///
# SPDX-License-Identifier: Apache-2.0
"""Fail if the pinned rate table no longer matches the AWS Pricing API.

The rate table `microvms-core` ships is pinned by hand, and a 90-day staleness
warning was the only thing standing between a caller and a silently stale price.
That warning cannot tell you *whether* a rate moved — only that nobody has looked.
This script looks, because the Pricing API carries the MicroVM line items directly:
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

**Storage is quoted per GB-hour and the table holds per GB-month.** The conversion
goes through `HOURS_PER_MONTH` below, which is the same 730 hours
`microvms-core::cost::SECONDS_PER_MONTH` assumes: two conventions for the same month
is how the pinned table came to be 1.37% low.

Regional spread is not a rounding detail. eu-west-1 is 5.3% over us-east-1 on
compute and 19% on snapshot storage; ap-northeast-1 is 16.4% and 20%. A caller in
Tokyo reading the us-east-1 table understates their snapshot write bill by 22.6%.
Only us-east-1 is pinned, and only us-east-1 is compared — see `main`.

Provenance
----------

This was `clients/python/src/microvms_agentd/pricing.py` until the Rust port went
live-green (Python oracle 56/56, Rust CLI 38/38). The Python client is in git
history; this script is the whole of what the gate needed from it, with the pinned
figures inlined as literals rather than imported.

Usage:
    scripts/check-live-rates.py              # fetch and compare. Needs credentials.
    scripts/check-live-rates.py --twin-only  # the twin cross-check alone. Offline, free.
"""

from __future__ import annotations

import argparse
import json
import re
import sys
from dataclasses import dataclass
from decimal import Decimal
from pathlib import Path
from typing import Any

SERVICE_CODE = "AWSLambda"

#: Where the Pricing API itself is served. Only three regions serve it
#: (us-east-1, ap-south-1, eu-central-1), and the region being *priced* is a filter
#: value rather than an endpoint — asking a us-east-1 endpoint about
#: ap-northeast-1 is the normal case, not a mistake.
PRICING_API_REGION = "us-east-1"

#: Pricing coverage and service availability coincide, measured 2026-08-07: the same
#: five regions that price MicroVMs are the five that answer `ListMicrovms`, and the
#: unpriced ones answer `AccessDeniedException` with a *null* message field that is
#: indistinguishable from a genuine IAM denial. So the list this script needs for
#: explaining an empty catalog is the same list `microvms-core` keeps for refusing a
#: launch.
#:
#: A literal here rather than a read of the Rust constant, because it is only ever
#: used to *write* the error and never to refuse: the fetch runs first and this is
#: consulted only when the catalog came back empty. So a region AWS adds after today
#: returns rates from this script rather than a refusal, and the cost of the list
#: being stale is one misleading sentence. `scripts/check-model-drift.py` is what holds
#: it equal to `microvms-core`'s.
MICROVM_REGIONS = (
    "ap-northeast-1",
    "eu-west-1",
    "us-east-1",
    "us-east-2",
    "us-west-2",
)

#: Strips the location prefix off a usage type: `USW2-Lambda-...` -> `Lambda-...`.
#: Anchored on the `Lambda-` that follows so it cannot eat part of a name that
#: happens to start with capitals.
_LOCATION_PREFIX = re.compile(r"^[A-Z0-9]+-(?=Lambda-)")

#: Hours in the month AWS's GB-month rate assumes. The same 730 as
#: `microvms-core::cost::HOURS_PER_MONTH`; see the module docstring, because the
#: second 730 is where the drift got in.
HOURS_PER_MONTH = Decimal(730)


# ── the pinned table, and its twin ───────────────────────────────────────────

#: The pinned rates, read 2026-08-07 in us-east-1 and recorded in `docs/PLATFORM.md`
#: under "What actually costs money". Four appear on the Lambda pricing page as
#: written; `storage_gb_month` is *derived*, because the API quotes snapshot storage
#: per GB-hour and the table holds per GB-month.
#:
#: **These literals are a deliberate second copy of `microvms-core/src/cost.rs`'s
#: `pinned_rates()`.** That is the point: a drift check that imported the values it
#: checks would compare a table against itself and pass by construction. Two
#: independent readers is the same pattern this repo's harbor-harvest sibling uses
#: for its tool/library twins — port a change to both, never unify them. `verify_twin`
#: below is what keeps them equal, and it runs on every invocation including
#: `--twin-only`.
PINNED_REGION = "us-east-1"
PINNED_RETRIEVED = "2026-08-07"
PINNED: dict[str, Decimal] = {
    "vcpu_second": Decimal("0.0000276944"),
    "gb_second": Decimal("0.0000036667"),
    # $0.0001111111 per GB-hour x 730 hours. Was 0.08 — a plausible-looking round
    # number that understated every stored GB by 1.37%, which is the whole argument
    # for deriving it from the API figure rather than reading a page.
    "storage_gb_month": Decimal("0.0811111030"),
    "snapshot_read_gb": Decimal("0.00155"),
    "snapshot_write_gb": Decimal("0.0038"),
}

#: Where the twin lives, and the function whose body must carry the same figures.
TWIN_PATH = Path("microvms-core/src/cost.rs")
TWIN_FN = "pub fn pinned_rates()"

#: The twin's field names, in the spelling `cost.rs` uses. Identical to `PINNED`'s
#: keys today; kept as an explicit map so a Rust-side rename is a failure here that
#: names the field rather than a comparison that quietly stops happening.
TWIN_FIELDS = {
    "vcpu_second": "vcpu_second",
    "gb_second": "gb_second",
    "storage_gb_month": "storage_gb_month",
    "snapshot_read_gb": "snapshot_read_gb",
    "snapshot_write_gb": "snapshot_write_gb",
}


def verify_twin(repo: Path) -> list[str]:
    """Reads `pinned_rates()`'s literals out of `cost.rs` and compares them to `PINNED`.

    **Why a source read rather than `cargo run -q -p microvms-cli -- cost --json`.**
    Both were available and this one is sturdier for three reasons. The envelope does
    not carry the rates — it carries *amounts*, so the CLI path would have to divide a
    line item by its quantity to recover each rate, which means a change in the cost
    arithmetic (baseline swapped for peak, a minimum-retention floor applied where it
    was not) would surface here as a rate that drifted. That is a true failure reported
    against the wrong file. It also needs a toolchain and a build to answer a question
    about five literals, where this needs neither and so runs in a checkout that has
    no cargo. And it compares the literals themselves, which is exactly the claim: the
    two pinned tables must be equal.

    The cost of the trade is that this reads Rust as text, so a reformat of
    `pinned_rates()` breaks it. That failure is an exit 1 naming the field it could not
    find, never a silent pass — which is the only property that matters, and it is the
    same discipline `scripts/check-model-drift.py` states for a missing model.

    Returns the disagreements as text. Empty means the twins match.
    """
    path = repo / TWIN_PATH
    if not path.is_file():
        return [
            f"no {TWIN_PATH} to compare against. The pinned figures in this script are a "
            "deliberate second copy of that file's `pinned_rates()`, and a copy with "
            "nothing to disagree with is how the original went 1.37% stale."
        ]
    source = path.read_text()
    start = source.find(TWIN_FN)
    if start < 0:
        return [
            f"{TWIN_PATH} has no {TWIN_FN!r}. Either it was renamed, or the pinned table "
            "moved — and a cross-check that cannot find its twin has stopped happening. "
            "Point TWIN_FN at the new name; do not delete the check."
        ]
    end = source.find("\n}", start)
    body = source[start : end if end > 0 else len(source)]

    findings: list[str] = []
    for field, rust_field in TWIN_FIELDS.items():
        found = re.search(rf"\b{re.escape(rust_field)}\s*:\s*dec!\(([0-9.]+)\)", body)
        if found is None:
            findings.append(
                f"{TWIN_PATH}'s {TWIN_FN} has no `{rust_field}: dec!(...)` literal, which "
                f"this script compares as {field!r}. See the note above PINNED."
            )
            continue
        theirs = Decimal(found.group(1))
        if theirs != PINNED[field]:
            findings.append(
                f"{field}: this script pins {PINNED[field]:f} and {TWIN_PATH} pins "
                f"{theirs:f}. The two tables must not disagree — one of them is pricing "
                "a bill nobody pays. Update both in the same commit."
            )

    for label, literal, rust in (
        ("region", PINNED_REGION, "Region::UsEast1"),
        ("retrieved", PINNED_RETRIEVED, "CalendarDate::from_ymd(2026, 8, 7)"),
    ):
        if rust not in body:
            findings.append(
                f"{TWIN_PATH}'s {TWIN_FN} no longer says {rust!r}, so this script's pinned "
                f"{label} {literal!r} is comparing against a table it cannot see."
            )
    return findings


# ── the catalog ──────────────────────────────────────────────────────────────


class PricingError(Exception):
    """Base for every way the rate catalog can fail to answer."""


class RegionNotPriced(PricingError):
    """The Pricing API has no MicroVM line items for this region.

    Which, measured, also means the region does not run MicroVMs at all — see
    `MICROVM_REGIONS`. Raised rather than returning a partial table, because a table
    with holes prices a run at less than it costs.
    """


class RateCatalogChanged(PricingError):
    """A line item is present but not the shape this script knows how to read.

    A missing ARM rate, a unit that changed, two products where there was one, a
    region that answered about a different region. Every one of them is a reason to
    stop rather than to substitute: the failure mode of guessing is a plausible
    number, which is the only kind nobody checks.
    """


@dataclass(frozen=True)
class RateLine:
    """One pinned rate and the catalog entry that fills it.

    `unit` is asserted against what the API returns rather than assumed. It is the
    only signal available if AWS restates storage per GB-month: the number would
    change by 730x and every arithmetic check downstream would still pass, because
    they all read this same table.
    """

    #: The `PINNED` key this fills.
    field: str
    #: The region-independent `group` attribute, which is what we filter on.
    group: str
    #: The us-east-1 spelling of `usagetype`, i.e. the canonical form after the
    #: location prefix is stripped. Cross-checked against `group`, never trusted
    #: across regions on its own.
    usagetype: str
    #: The unit the API must report for this line.
    unit: str
    #: True for storage, quoted per GB-hour where the pinned table holds per GB-month.
    per_hour: bool = False
    #: The x86 sibling, where one exists. Probed *only* when the ARM line is missing,
    #: so the error can name the rate it is refusing to substitute rather than leaving
    #: the reader to wonder whether one exists.
    x86_group: str | None = None


#: The five figures the pinned table needs, out of the seven line items the catalog
#: carries. The two we skip are the x86 compute rates, which a MicroVM can never be
#: billed at.
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


def pricing_client() -> Any:
    """A Pricing API client, imported lazily so `--twin-only` needs no boto3 import."""
    import boto3
    from botocore.config import Config as BotoConfig

    return boto3.Session(region_name=PRICING_API_REGION).client(
        "pricing", config=BotoConfig(retries={"max_attempts": 10, "mode": "standard"})
    )


def products(client: Any, region: str, group: str) -> list[dict[str, Any]]:
    """Every product for one group in one region. Normally exactly one.

    Filtering on `group` *and* `regionCode` is what keeps this cheap: five single-item
    calls instead of paging 10,471 Lambda products to find 35 MicroVM ones. The token
    loop stays anyway — the filters are applied server-side, so an empty first page
    with a `NextToken` would otherwise read as "this region does not price MicroVMs".
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


def price(product: dict[str, Any], line: RateLine, region: str) -> Decimal:
    """The one USD figure on a product, with every assumption about it checked.

    Each check is a way the catalog could change under us and still parse. The region
    check in particular: the API answers a filter it does not understand by ignoring
    it, so a typo'd field name would return us-east-1 rates labelled with whatever
    region the caller asked for.
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
    # which float cannot hold, and the whole money path is Decimal for that reason.
    return Decimal(usd)


def missing(client: Any, region: str, line: RateLine) -> PricingError:
    """The error for a line the catalog no longer has.

    Split out because the ARM case needs to say more than "missing": the x86 sibling
    is right there, 17.9% higher, and the tempting fix is to use it.
    """
    if line.x86_group is None:
        return RateCatalogChanged(
            f"{region} prices MicroVMs but has no {line.group} line item, so "
            f"{line.field} cannot be filled"
        )
    siblings = products(client, region, line.x86_group)
    if not siblings:
        return RateCatalogChanged(
            f"{region} has neither {line.group} nor {line.x86_group}, so {line.field} "
            "cannot be filled"
        )
    x86 = price(
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
    """The five pinned rate fields for one region, live.

    All or nothing. A partial table would price a run at less than it costs, and the
    caller has no way to see which field was quietly left at a stale value.
    """
    api = client if client is not None else pricing_client()
    found: dict[str, Decimal] = {}
    failures: list[PricingError] = []
    for line in MICROVM_LINES:
        candidates = products(api, region, line.group)
        if len(candidates) > 1:
            raise RateCatalogChanged(
                f"{line.group} in {region} returned {len(candidates)} products where one group "
                "in one region has always been one product; the catalog gained a dimension "
                "this script does not know how to choose between"
            )
        if not candidates:
            failures.append(missing(api, region, line))
            continue
        rate = price(candidates[0], line, region)
        # The API quotes storage per GB-hour. Multiplying here rather than at every use
        # site keeps the table one convention deep, which is the whole reason
        # HOURS_PER_MONTH exists.
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
            "a region since, the list in MICROVM_REGIONS here and in microvms-core is what "
            "needs updating; this call attempts the fetch regardless of it, so a genuinely "
            "new region will already have returned rates rather than reaching this error."
        )
    if failures:
        raise failures[0]
    return found


# ── the drift report ─────────────────────────────────────────────────────────

#: How far a pinned rate may sit from the live one before it counts as drift. The
#: pinned snapshot rates are three-significant-figure roundings of ten-digit API
#: figures — 0.00155 for 0.0015467699 is 0.21% high — so a tolerance under that
#: reports drift on a table that is correct, and a check that always fires is a check
#: nobody reads. AWS price changes land in whole percentage points, well clear of this.
DRIFT_TOLERANCE = Decimal("0.005")


@dataclass(frozen=True)
class RateDrift:
    """One pinned rate against its live value.

    Every line is reported, drifted or not. A drift check that printed only its
    findings could not be told apart from one whose credentials silently failed over
    to zero line items.
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
        # No zero guard: a pinned rate of zero would mean the table claims a billable
        # line item is free, and ZeroDivisionError is a better outcome than a drift
        # report that quietly skipped it.
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


def check_drift(*, client: Any = None) -> tuple[RateDrift, ...]:
    """The pinned table against live Pricing API values, one entry per rate.

    This is what replaces the 90-day staleness warning with a measurement. The warning
    could only ever say that nobody had looked; this says whether anything moved, and
    by how much.

    Compares against `PINNED_REGION`, so checking a table pinned to us-east-1 never
    reports the 5-19% regional spread as drift.
    """
    live = fetch_rates(PINNED_REGION, client=client)
    return tuple(
        RateDrift(
            field=line.field,
            usagetype=line.usagetype,
            pinned=PINNED[line.field],
            live=live[line.field],
        )
        for line in MICROVM_LINES
    )


def render_drift(drifts: tuple[RateDrift, ...]) -> str:
    """Plain text for the `live:rates` task, leading with what a reader must do."""
    moved = [d for d in drifts if d.drifted]
    lines = [
        f"pinned rate table for {PINNED_REGION} (retrieved {PINNED_RETRIEVED}) vs the "
        f"aws pricing api",
        f"tolerance {(DRIFT_TOLERANCE * 100).normalize():f}%, which is the rounding in the "
        f"pinned figures rather than a licence to drift",
    ]
    lines += [f"  {d}" for d in drifts]
    if moved:
        fields = ", ".join(d.field for d in moved)
        lines.append(
            f"{len(moved)} rate(s) moved: {fields} — update microvms-core/src/cost.rs, the "
            "PINNED table here, and docs/PLATFORM.md in the same commit"
        )
    else:
        lines.append(f"all {len(drifts)} rates match the api within tolerance")
    return "\n".join(lines)


def main(argv: list[str] | None = None) -> int:
    """Exit 1 on drift, so the `live:rates` task fails rather than reports.

    A stale rate table is the same failure class as a stale generated schema, which
    this repo already fails on: a consumer trusts a pinned price *because* it is
    pinned, and nothing in a diff reveals that it describes a bill nobody pays.

    Takes no `--region`. There is exactly one pinned table and it is us-east-1; a flag
    that fetched some other region's table and then compared it against itself would
    pass by construction, which is the one thing a drift check must not do.
    """
    parser = argparse.ArgumentParser(
        description="Compare the pinned MicroVM rate table against the AWS Pricing API."
    )
    parser.add_argument(
        "--twin-only",
        action="store_true",
        help=(
            "run only the cost.rs cross-check and exit. Offline and free, so a "
            "credential-less checkout can still prove the two pinned tables agree"
        ),
    )
    args = parser.parse_args(argv)

    repo = Path(__file__).resolve().parent.parent
    # First, and on every path including --twin-only. A pinned figure that disagrees
    # with its twin is already wrong, whatever the API says, and finding that out
    # before the fetch keeps a two-table problem from being read as a one-table one.
    twin = verify_twin(repo)
    for finding in twin:
        print(f"TWIN {finding}")
    if not twin:
        print(f"twin ok: {len(TWIN_FIELDS)} pinned rate(s) agree with {TWIN_PATH}")
    if args.twin_only:
        return 1 if twin else 0
    if twin:
        return 1

    drifts = check_drift()
    print(render_drift(drifts))
    return 1 if any(d.drifted for d in drifts) else 0


if __name__ == "__main__":
    sys.exit(main())
