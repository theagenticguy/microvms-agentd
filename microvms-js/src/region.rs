//! The five regions that carry MicroVMs, and the one named way past them (TRAP-6).
//!
//! # Why a class with factory methods rather than a string parameter
//!
//! In Rust the five regions are enum variants, so a typo is a compile error — strength S1.
//! JS has no equivalent, so the closest faithful port is a `#[napi]` class whose only
//! constructors are the five names, [`Region::parse`] (which refuses everything else), and
//! [`Region::unlisted`] (which says at the call site that someone opted into the trap). A
//! `region: string` parameter anywhere on this surface would be the loosening, which is why
//! [`crate::sandbox::Sandbox::create`] takes a `Region` instance.
//!
//! There is no `#[napi(constructor)]`, so `new Region("eu-central-1")` throws rather than
//! producing an unchecked region.
//!
//! # What the refusal is worth
//!
//! Measured 2026-08-07: a region that does not carry MicroVMs answers
//! `AccessDeniedException` with the message field **null**, which is indistinguishable from a
//! genuine IAM denial. Nothing between the caller and that answer objects — the service
//! model's `endpointPrefix` is `lambda`, so an endpoint resolves for any region. The first
//! API call is the only reporter and it reports the wrong cause. `eu-central-1` was on the
//! supported list until 2026-08-07 and does not carry MicroVMs.

use std::str::FromStr;

use microvms_core::Region as CoreRegion;
use napi_derive::napi;

use crate::errors::js;

/// An AWS region, closed over the five that run MicroVMs plus a named escape hatch.
#[napi]
#[derive(Clone)]
pub struct Region {
    pub(crate) inner: CoreRegion,
}

#[napi]
impl Region {
    #[napi(factory)]
    pub fn us_east_1() -> Region {
        Region {
            inner: CoreRegion::UsEast1,
        }
    }

    #[napi(factory)]
    pub fn us_east_2() -> Region {
        Region {
            inner: CoreRegion::UsEast2,
        }
    }

    #[napi(factory)]
    pub fn us_west_2() -> Region {
        Region {
            inner: CoreRegion::UsWest2,
        }
    }

    #[napi(factory)]
    pub fn eu_west_1() -> Region {
        Region {
            inner: CoreRegion::EuWest1,
        }
    }

    #[napi(factory)]
    pub fn ap_northeast_1() -> Region {
        Region {
            inner: CoreRegion::ApNortheast1,
        }
    }

    /// One of the five, or a refusal naming the null-message trap.
    ///
    /// The boundary a region name arrives at from an environment variable or a config file,
    /// where it is still a string. The refusal is the core's `FromStr`, message and all —
    /// this is a call, not a check written here.
    #[napi(factory)]
    pub fn parse(name: String) -> napi::Result<Region, String> {
        Ok(Region {
            inner: CoreRegion::from_str(&name).map_err(js)?,
        })
    }

    /// Opts into a region this client has not seen carry MicroVMs.
    ///
    /// **This costs you the diagnostic.** If the region does not run MicroVMs, the first
    /// control-plane call answers `AccessDeniedException` with a null message, and you will
    /// spend the next hour reading an IAM policy that is correct. Named rather than a flag so
    /// a reader of the call site can see the opt-in.
    ///
    /// A supported name handed here comes back as its proper region, so
    /// `Region.unlisted("us-east-1")` equals `Region.usEast1()`.
    #[napi(factory)]
    pub fn unlisted(name: String) -> Region {
        Region {
            inner: CoreRegion::unlisted(name),
        }
    }

    /// Every region this client has seen carry MicroVMs.
    #[napi]
    pub fn supported() -> Vec<Region> {
        microvms_core::region::MICROVM_REGIONS
            .into_iter()
            .map(|inner| Region { inner })
            .collect()
    }

    /// The wire spelling, which is also the endpoint's middle segment.
    #[napi(getter)]
    pub fn name(&self) -> String {
        self.inner.as_str().to_string()
    }

    /// Whether this is one of the five rather than an opted-into unlisted name.
    #[napi(getter)]
    pub fn is_supported(&self) -> bool {
        self.inner.is_supported()
    }

    #[napi(js_name = "toString")]
    pub fn display_string(&self) -> String {
        self.inner.as_str().to_string()
    }

    /// Whether two regions are the same one.
    ///
    /// A method and not `===`: JS compares class instances by reference, so two
    /// `Region.usEast1()` calls are different objects. Named `equals` because that is what a
    /// JS reader expects to reach for.
    #[napi]
    pub fn equals(&self, other: &Region) -> bool {
        self.inner == other.inner
    }
}
