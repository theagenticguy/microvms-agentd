# SPDX-License-Identifier: Apache-2.0
"""The region surface: `src/region.rs`.

TRAP-6 is the whole subject. A region that does not carry MicroVMs answers
`AccessDeniedException` with the message field **null**, which is indistinguishable from a genuine
IAM denial — so the only warning anyone gets is the one this client refuses with, before the call.

`test_smoke.py` covers the headline case (`eu-central-1` refused, five supported names, no region
string parameter). This file covers the parts a single case cannot: that `parse` is *exact* rather
than normalising, that the escape hatch normalises a supported name back to the real thing, and
that a `Region` behaves as a value — equality and hashing — because a caller keys a pool by it.
"""

from __future__ import annotations

import pytest

import microvms

SUPPORTED = ["us-east-1", "us-east-2", "us-west-2", "eu-west-1", "ap-northeast-1"]


def named_constructors() -> list[tuple[str, microvms.Region]]:
    """The five named doors, paired with the wire name each answers."""
    return [
        ("us-east-1", microvms.Region.us_east_1()),
        ("us-east-2", microvms.Region.us_east_2()),
        ("us-west-2", microvms.Region.us_west_2()),
        ("eu-west-1", microvms.Region.eu_west_1()),
        ("ap-northeast-1", microvms.Region.ap_northeast_1()),
    ]


# -- the closed set ------------------------------------------------------------


def test_each_named_constructor_answers_its_own_region_and_reports_supported() -> None:
    """Five constructors, five distinct regions, no two aliased.

    Aliasing is the mistake worth checking: `us_east_2()` returning `UsEast1` would send every
    call to the wrong region's endpoint and nothing local would object. A test that only counted
    `supported()` would not see it.
    """
    built = named_constructors()
    assert [name for name, _ in built] == SUPPORTED
    for name, region in built:
        assert region.name == name
        assert region.is_supported
        assert str(region) == name


def test_supported_is_exactly_the_five_named_constructors() -> None:
    """One list, reachable two ways, and they have to agree.

    `supported()` is what a caller enumerates; the constructors are what they call. A region
    present in one and absent from the other is a region that is either unreachable or
    undocumented.
    """
    enumerated = [region.name for region in microvms.Region.supported()]
    assert enumerated == SUPPORTED
    assert enumerated == [name for name, _ in named_constructors()]
    assert all(region.is_supported for region in microvms.Region.supported())


@pytest.mark.parametrize("name", SUPPORTED)
def test_parse_accepts_every_supported_name_and_round_trips_it(name: str) -> None:
    """The positive half of the refusal, so the guard is not a blanket no.

    `parse` is the boundary a region name arrives at from an environment variable or a config
    file, and a refusal that also rejected the good names would be worse than no check.
    """
    parsed = microvms.Region.parse(name)
    assert parsed.name == name
    assert parsed.is_supported
    # Round trip through the string form, which is what a config file round trip looks like.
    assert microvms.Region.parse(str(parsed)) == parsed


@pytest.mark.parametrize(
    "name",
    [
        "eu-central-1",  # the specific region that was on the list until 2026-08-07
        "us-east-3",  # a plausible-looking region that does not exist
        "US-EAST-1",  # right region, wrong case
        "us-east-1 ",  # a trailing space from a config file
        " us-east-1",
        "useast1",  # the dashes dropped
        "us_east_1",  # underscores, as an env-var name would spell it
        "",
        "local",
    ],
)
def test_parse_refuses_everything_outside_the_five_without_normalising(name: str) -> None:
    """Exact matching, and that is deliberate rather than an oversight.

    A `parse` that trimmed or case-folded would accept two spellings of one region, and whichever
    consumer keyed on the raw string would split the group. More to the point: the whole value of
    this refusal is that it is the *only* warning before the null-message denial, and a
    normalising parse quietly widens the set it is guarding.
    """
    with pytest.raises(microvms.InvalidArgError) as raised:
        microvms.Region.parse(name)
    assert raised.value.code == "ERR_INVALID_ARG"
    assert raised.value.wire_kind is None, "nothing reached the daemon"


def test_the_refusal_names_the_null_message_trap_and_offers_the_supported_set() -> None:
    """Both halves of the message, and the way out.

    "AccessDeniedException" alone reads as an IAM problem — someone would spend an hour reading a
    policy that is correct — and it is the word *null* that says otherwise. The five names have
    to be in there too, or the refusal tells a caller they are wrong without telling them what is
    right.
    """
    with pytest.raises(microvms.InvalidArgError) as raised:
        microvms.Region.parse("eu-central-1")
    message = str(raised.value)
    assert "AccessDeniedException" in message
    assert "null" in message
    for name in SUPPORTED:
        assert name in message, f"{name} missing from the refusal: {message}"


# -- the escape hatch ----------------------------------------------------------


def test_unlisted_produces_a_region_that_says_it_is_unlisted() -> None:
    """The opt-in is visible in the *value*, not only at the call site.

    A caller reaches `unlisted` deliberately; the code downstream did not. `is_supported` being
    false is what lets a wrapper log "you are on your own here" rather than discovering it from
    a null-message denial.
    """
    unlisted = microvms.Region.unlisted("eu-central-1")
    assert unlisted.name == "eu-central-1"
    assert not unlisted.is_supported
    assert str(unlisted) == "eu-central-1"


@pytest.mark.parametrize("name", SUPPORTED)
def test_unlisted_normalises_a_supported_name_back_to_the_real_region(name: str) -> None:
    """One region, one value, however it was reached.

    Without this, `unlisted("us-east-1")` would be a *second* value for a supported region —
    equal to nothing, hashing differently, and reporting `is_supported` false about a region that
    is. Anything keyed by region would then hold two entries for one place.
    """
    hatched = microvms.Region.unlisted(name)
    assert hatched == microvms.Region.parse(name)
    assert hatched.is_supported
    assert hash(hatched) == hash(microvms.Region.parse(name))


def test_unlisted_accepts_a_name_parse_refuses_which_is_the_whole_point() -> None:
    """The two doors differ, and they differ in exactly one direction.

    `parse` is for a name that must be checked; `unlisted` is for a name someone chose anyway.
    If `unlisted` refused too there would be no hatch, and if `parse` accepted there would be no
    guard.
    """
    name = "me-south-1"
    with pytest.raises(microvms.InvalidArgError):
        microvms.Region.parse(name)
    assert microvms.Region.unlisted(name).name == name


def test_an_unlisted_name_is_carried_verbatim_including_a_spelling_parse_would_reject() -> None:
    """No normalising on the way through, because the caller may know something we do not.

    A name is what goes into the endpoint's middle segment, so altering it would address a
    different region than the one asked for — and the whole reason to use this door is that the
    client's list is out of date rather than the caller's.
    """
    for name in ["EU-CENTRAL-1", "eu-central-1", "some-future-region-9"]:
        assert microvms.Region.unlisted(name).name == name


# -- a region is a value -------------------------------------------------------


def test_two_calls_to_one_constructor_compare_equal_and_hash_alike() -> None:
    """Value semantics, which is what makes a region usable as a dict key.

    A pool keyed by region is the obvious use, and with reference equality every lookup would
    miss — silently building one entry per call rather than one per region.
    """
    for _, region in named_constructors():
        twin = microvms.Region.parse(region.name)
        assert region == twin
        assert hash(region) == hash(twin)
    # And as a real key: five regions, five slots, no collisions or duplicates.
    keyed = {region: region.name for _, region in named_constructors()}
    assert len(keyed) == 5
    assert keyed[microvms.Region.us_west_2()] == "us-west-2"


def test_two_different_regions_are_not_equal() -> None:
    """The other half of equality, so `__eq__` is not a constant.

    An `__eq__` returning `True` unconditionally would pass every assertion above.
    """
    regions = [region for _, region in named_constructors()]
    for index, first in enumerate(regions):
        for second in regions[index + 1 :]:
            assert first != second
    assert len({hash(region) for region in regions}) == 5


def test_an_unlisted_region_is_not_equal_to_a_supported_one() -> None:
    """`is_supported` is part of the identity, not a label bolted on.

    Two values that spelled the same name but disagreed about support would be a set that
    deduplicated one of them arbitrarily, and which one survived would decide whether a caller
    got the warning.
    """
    unlisted = microvms.Region.unlisted("eu-central-1")
    for _, supported in named_constructors():
        assert unlisted != supported
    assert len({unlisted, microvms.Region.us_east_1()}) == 2


def test_the_repr_says_both_the_name_and_whether_it_is_supported() -> None:
    """A debug form that answers the question someone is debugging.

    A bare name in a traceback would leave the reader unable to tell an opted-into region from a
    supported one — which is precisely what they are looking at the traceback to find out. So the
    assertion is on the *flag being distinguishable*, which is the property, rather than on the
    exact sentence.

    Note the boolean is Rust-spelled (`is_supported=true`, not `True`): the repr is built with
    `format!`, so it renders Rust's `Display` for a `bool`. Asserted as observed rather than as
    Python convention would have it, because a test written to the convention would be a test
    that fails against the shipped binding — and this is a debug string, not a parsed one.
    """
    supported = repr(microvms.Region.us_east_1())
    unlisted = repr(microvms.Region.unlisted("eu-central-1"))
    assert "us-east-1" in supported
    assert "is_supported=true" in supported
    assert "is_supported=false" in unlisted
    # The load-bearing part: the two are distinguishable at a glance.
    assert supported != unlisted


def test_a_region_cannot_be_constructed_from_a_bare_string_by_calling_the_class() -> None:
    """No `__new__`, so an unchecked name is not writable.

    With a constructor, `Region("eu-central-1")` would be the shortest path in the module and it
    would bypass both doors — the check and the visible opt-in — at once.
    """
    with pytest.raises(TypeError):
        microvms.Region("us-east-1")  # type: ignore[call-arg]
    with pytest.raises(TypeError):
        microvms.Region()  # type: ignore[call-arg]
