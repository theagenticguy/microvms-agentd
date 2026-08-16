// SPDX-License-Identifier: Apache-2.0
//! The five regions that carry MicroVMs, and the one named way past them (TRAP-6).
//!
//! # Why this is a class with named constructors rather than a string parameter
//!
//! In Rust the five regions are enum variants, so a typo is a compile error — strength S1.
//! Python has no equivalent, so the closest faithful port is a class whose only
//! constructors are the five names, [`PyRegion::parse`] (which refuses everything else),
//! and [`PyRegion::unlisted`] (which says at the call site that someone opted into the
//! trap). A `region="eu-central-1"` string parameter anywhere on this surface would be the
//! loosening, which is why [`crate::sandbox::PySandbox::new`] takes a `Region` object.
//!
//! # What the refusal is worth
//!
//! Measured 2026-08-07: a region that does not carry MicroVMs answers
//! `AccessDeniedException` with the message field **null**, which is indistinguishable
//! from a genuine IAM denial. Nothing between the caller and that answer objects — the
//! service model's `endpointPrefix` is `lambda`, so an endpoint resolves for any region.
//! The first API call is the only reporter and it reports the wrong cause. `eu-central-1`
//! was on the supported list until 2026-08-07 and does not carry MicroVMs.

use std::str::FromStr;

use microvms_core::Region;
use pyo3::prelude::*;

use crate::errors::PyCoreResult;

/// An AWS region, closed over the five that run MicroVMs plus a named escape hatch.
#[pyclass(frozen, from_py_object, name = "Region", module = "microvms")]
#[derive(Clone)]
pub struct PyRegion {
    pub(crate) inner: Region,
}

#[pymethods]
impl PyRegion {
    #[staticmethod]
    fn us_east_1() -> PyRegion {
        PyRegion {
            inner: Region::UsEast1,
        }
    }

    #[staticmethod]
    fn us_east_2() -> PyRegion {
        PyRegion {
            inner: Region::UsEast2,
        }
    }

    #[staticmethod]
    fn us_west_2() -> PyRegion {
        PyRegion {
            inner: Region::UsWest2,
        }
    }

    #[staticmethod]
    fn eu_west_1() -> PyRegion {
        PyRegion {
            inner: Region::EuWest1,
        }
    }

    #[staticmethod]
    fn ap_northeast_1() -> PyRegion {
        PyRegion {
            inner: Region::ApNortheast1,
        }
    }

    /// One of the five, or a refusal naming the null-message trap.
    ///
    /// The boundary a region name arrives at from an environment variable or a config
    /// file, where it is still a string. The refusal is the core's `FromStr`, message and
    /// all — this is a call, not a check written here.
    #[staticmethod]
    fn parse(name: &str) -> PyCoreResult<PyRegion> {
        Ok(PyRegion {
            inner: Region::from_str(name)?,
        })
    }

    /// Opts into a region this client has not seen carry MicroVMs.
    ///
    /// **This costs you the diagnostic.** If the region does not run MicroVMs, the first
    /// control-plane call answers `AccessDeniedException` with a null message, and you
    /// will spend the next hour reading an IAM policy that is correct. Named rather than a
    /// flag so a reader of the call site can see the opt-in.
    ///
    /// A supported name handed here comes back as its proper region, so
    /// `Region.unlisted("us-east-1") == Region.us_east_1()`.
    #[staticmethod]
    fn unlisted(name: &str) -> PyRegion {
        PyRegion {
            inner: Region::unlisted(name),
        }
    }

    /// Every region this client has seen carry MicroVMs.
    #[staticmethod]
    fn supported() -> Vec<PyRegion> {
        microvms_core::region::MICROVM_REGIONS
            .into_iter()
            .map(|inner| PyRegion { inner })
            .collect()
    }

    /// The wire spelling, which is also the endpoint's middle segment.
    #[getter]
    fn name(&self) -> String {
        self.inner.as_str().to_string()
    }

    /// Whether this is one of the five rather than an opted-into unlisted name.
    #[getter]
    fn is_supported(&self) -> bool {
        self.inner.is_supported()
    }

    fn __str__(&self) -> String {
        self.inner.as_str().to_string()
    }

    fn __repr__(&self) -> String {
        format!(
            "Region({:?}, is_supported={})",
            self.inner.as_str(),
            self.inner.is_supported()
        )
    }

    fn __eq__(&self, other: &PyRegion) -> bool {
        self.inner == other.inner
    }

    fn __hash__(&self) -> u64 {
        use std::hash::{DefaultHasher, Hash as _, Hasher as _};
        let mut hasher = DefaultHasher::new();
        self.inner.hash(&mut hasher);
        hasher.finish()
    }
}
