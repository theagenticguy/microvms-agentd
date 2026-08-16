// SPDX-License-Identifier: Apache-2.0
//! The five regions that carry MicroVMs, as a closed enum (TRAP-6).
//!
//! The Python client checks a region against a frozenset at construction time
//! (`sandbox.py:314` `require_supported_region`) — strength S2 on the spec's
//! ladder: the mistake can be written down, and a runtime guard rejects it. Here it
//! is S1 for the five names: `Region::UsEast1` and its four siblings are the only
//! values a caller can *type*, so a typo'd region is a compile error rather than a
//! test that has to run.
//!
//! # Why refusing matters more than it looks
//!
//! Measured 2026-08-07: a region that does not carry MicroVMs answers
//! `AccessDeniedException` with the message field **null**, which is
//! indistinguishable from a genuine IAM denial except that a real denial names the
//! principal and the action. Nothing between the caller and that answer objects —
//! the service model's `endpointPrefix` is `lambda`, so an endpoint resolves for any
//! region and a client constructs happily. The first API call is the only reporter
//! and it reports the wrong cause, sending someone to audit a policy that is fine.
//!
//! No API answers the question either, and the two that look like they might
//! disagree with each other: `get_available_endpoints` returns an empty list while
//! `get_available_regions` returns all 34 Lambda regions. So the list is kept by
//! hand, and keeping it right is the whole correctness condition — in both
//! directions. A *missing* region refuses a launch AWS would have accepted, which is
//! the safer direction and still wrong; that is what [`Region::unlisted`] is for. An
//! *extra* region is worse, because it re-opens the null-message trap for a name
//! nothing will reject.
//!
//! `eu-central-1` was on this list until 2026-08-07 and does **not** carry MicroVMs:
//! it was one of three regions measured returning the null-message denial.

use std::fmt;
use std::str::FromStr;

use crate::error::Error;

/// An AWS region, closed over the five that run MicroVMs plus a named escape hatch.
///
/// See the module docs for why the five are an enum. [`Region::Unlisted`] is
/// deliberately a variant rather than a hidden flag: a reader of a call site can
/// see that someone opted into the trap, and a `match` over regions cannot forget
/// the case exists.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub enum Region {
    UsEast1,
    UsEast2,
    UsWest2,
    EuWest1,
    ApNortheast1,
    /// A region this client has not seen carry MicroVMs.
    ///
    /// **You are opting into the null-message trap.** If this region does not run
    /// MicroVMs, the first control-plane call answers `AccessDeniedException` with a
    /// null message and you will spend the next hour reading an IAM policy that is
    /// correct. Constructible only through [`Region::unlisted`], which says so at
    /// the call site.
    ///
    /// It exists because AWS adds regions faster than this list is re-read, and a
    /// client that refuses a region AWS has just launched in is its own kind of
    /// wrong. The override costs exactly the diagnostic above.
    Unlisted(String),
}

/// The five regions that answered `ListMicrovms` when this was measured
/// (2026-08-07), smallest-latency-first order not implied — this is the order
/// `MICROVM_REGIONS` is documented in.
///
/// Not model-backed and it cannot be: see the module docs. `constants::as_json`
/// publishes this set so `scripts/check-model-drift.py` can compare the Rust and
/// Python clients against each other, which is the only check available for a value
/// no service model states.
pub const MICROVM_REGIONS: [Region; 5] = [
    Region::UsEast1,
    Region::UsEast2,
    Region::UsWest2,
    Region::EuWest1,
    Region::ApNortheast1,
];

impl Region {
    /// The wire spelling, which is also the endpoint's middle segment.
    pub fn as_str(&self) -> &str {
        match self {
            Region::UsEast1 => "us-east-1",
            Region::UsEast2 => "us-east-2",
            Region::UsWest2 => "us-west-2",
            Region::EuWest1 => "eu-west-1",
            Region::ApNortheast1 => "ap-northeast-1",
            Region::Unlisted(name) => name,
        }
    }

    /// Opts into a region this client has not seen carry MicroVMs.
    ///
    /// **This is the escape hatch, and it costs you the diagnostic.** If the region
    /// does not run MicroVMs, the first control-plane call answers
    /// `AccessDeniedException` with a null message field, which reads as a genuine
    /// IAM denial (`docs/PLATFORM.md`, "Calling an unpriced region returns
    /// `AccessDeniedException` with a null message"). Use it when AWS has launched
    /// MicroVMs somewhere new, and add the region to [`MICROVM_REGIONS`] in the same
    /// change.
    ///
    /// A supported name passed here comes back as its proper variant rather than as
    /// [`Region::Unlisted`], so `unlisted("us-east-1") == Region::UsEast1` and
    /// nothing downstream has to handle two spellings of one region.
    pub fn unlisted(name: impl Into<String>) -> Region {
        let name = name.into();
        match Self::supported(&name) {
            Some(known) => known,
            None => Region::Unlisted(name),
        }
    }

    /// The variant for a supported name, or `None` for anything else.
    ///
    /// The single reader of the five spellings: [`FromStr`] and [`Region::unlisted`]
    /// both come through here, so there is no second table to drift from the first.
    fn supported(name: &str) -> Option<Region> {
        MICROVM_REGIONS
            .into_iter()
            .find(|region| region.as_str() == name)
    }

    /// Whether this is one of the five, rather than an opted-into unlisted name.
    pub fn is_supported(&self) -> bool {
        !matches!(self, Region::Unlisted(_))
    }
}

impl fmt::Display for Region {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Region {
    type Err = Error;

    /// Parses one of the five, and refuses everything else.
    ///
    /// The refusal is the point: this is the boundary a region name arrives at from
    /// a CLI flag or an environment variable, where it is still a string and the
    /// enum cannot help. A caller who genuinely wants a sixth region reaches for
    /// [`Region::unlisted`] and says so.
    fn from_str(name: &str) -> Result<Region, Error> {
        if let Some(known) = Region::supported(name) {
            return Ok(known);
        }
        let offered = MICROVM_REGIONS
            .iter()
            .map(Region::as_str)
            .collect::<Vec<_>>()
            .join(", ");
        Err(Error::invalid_arg(format!(
            "region {name:?} is not one this client has seen carry MicroVMs ({offered}). \
             Refused here because the first API call is where the evidence disappears: an \
             unsupported region answers AccessDeniedException with a null message, which is \
             indistinguishable from a real IAM denial (docs/PLATFORM.md, 'Calling an unpriced \
             region returns AccessDeniedException with a null message'). If AWS has since \
             launched MicroVMs here, use Region::unlisted({name:?}) and add the region to \
             MICROVM_REGIONS."
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::ErrorKind;

    /// The five spellings, written out. A generated list would agree with a typo,
    /// and a typo here is the *extra region* direction of the correctness condition
    /// — the direction that re-opens the null-message trap.
    #[test]
    fn the_five_supported_regions_are_the_measured_ones() {
        let names: Vec<&str> = MICROVM_REGIONS.iter().map(Region::as_str).collect();
        assert_eq!(
            names,
            [
                "us-east-1",
                "us-east-2",
                "us-west-2",
                "eu-west-1",
                "ap-northeast-1"
            ]
        );
    }

    /// TRAP-6, the falsification case. `eu-central-1` is the specific region that
    /// was on this list and does not carry MicroVMs, so it is the one a regression
    /// would most plausibly re-add. The message must name the trap — both halves,
    /// because "AccessDeniedException" alone reads as an IAM problem and it is the
    /// word *null* that says otherwise.
    #[test]
    fn eu_central_one_is_refused_naming_the_null_message_trap() {
        let err = "eu-central-1"
            .parse::<Region>()
            .expect_err("eu-central-1 does not carry MicroVMs");
        assert_eq!(err.kind(), ErrorKind::InvalidArg);
        let message = err.to_string();
        assert!(
            message.contains("AccessDeniedException"),
            "must name the exception: {message}"
        );
        assert!(
            message.contains("null"),
            "must say the message field is null: {message}"
        );
        assert!(
            message.contains("eu-central-1"),
            "must name the rejected region: {message}"
        );
    }

    /// Every supported name parses back to its own variant. The round trip is what
    /// makes `as_str` and the parse table one table rather than two.
    #[test]
    fn each_supported_region_round_trips_through_its_wire_name() {
        for region in MICROVM_REGIONS {
            let parsed: Region = region.as_str().parse().expect("supported name parses");
            assert_eq!(parsed, region);
            assert!(parsed.is_supported());
        }
    }

    /// The escape hatch produces a distinguishable value rather than a silent pass:
    /// `is_supported` is false, so a caller — or a cost table that has no rates for
    /// the region — can still tell the difference.
    #[test]
    fn the_escape_hatch_is_visible_in_the_value_it_produces() {
        let region = Region::unlisted("eu-central-1");
        assert_eq!(region, Region::Unlisted("eu-central-1".to_string()));
        assert_eq!(region.as_str(), "eu-central-1");
        assert!(!region.is_supported());
    }

    /// A supported name handed to the escape hatch comes back as its proper variant.
    /// Otherwise `unlisted("us-east-1")` would be a second, unequal spelling of a
    /// region the client fully supports, and every `match` downstream would need to
    /// handle both.
    #[test]
    fn the_escape_hatch_normalises_a_supported_name_to_its_variant() {
        assert_eq!(Region::unlisted("us-east-1"), Region::UsEast1);
        assert!(Region::unlisted("ap-northeast-1").is_supported());
    }

    /// Nothing plausible-looking sneaks through. These are the shapes a typo takes:
    /// a real AWS region that lacks MicroVMs, a near-miss on a supported name, a
    /// case change, and the empty string.
    #[test]
    fn near_misses_and_other_aws_regions_are_all_refused() {
        for name in [
            "eu-central-1",
            "ap-southeast-2",
            "sa-east-1",
            "us-east-3",
            "us-west-1",
            "US-EAST-1",
            "us-east-1 ",
            "",
        ] {
            let err = name
                .parse::<Region>()
                .expect_err("only the five measured regions parse");
            assert_eq!(err.kind(), ErrorKind::InvalidArg, "{name}");
        }
    }
}
