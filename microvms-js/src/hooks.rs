// SPDX-License-Identifier: Apache-2.0
//! The two hook-timeout families, kept as two classes (BIND-2).
//!
//! # Why two classes and not two numbers
//!
//! The ceilings are 60x apart: the `run`/`resume`/`suspend`/`terminate` family caps at 60
//! seconds and the `ready`/`validate` image family at 3600. In Rust the core makes that S1 —
//! two types with no conversion in either direction, so a 3600-second build timeout cannot
//! reach a field that caps at 60.
//!
//! Two `number` fields would give that away, and worse than in Python: they can be
//! *transposed*, which is the specific mistake the two types exist to prevent and the one a
//! numeric parameter cannot see. So both are `#[napi]` classes. napi v3 generates real
//! TypeScript classes, so `tsc` rejects one where the other is wanted, and at runtime napi's
//! argument conversion rejects a non-instance before any Rust runs — including the
//! structurally identical `{ seconds: 30 }`, which is what a `#[napi(object)]` would have
//! accepted.
//!
//! The range check inside each constructor is the core's `try_new`, message and all: each
//! refusal names **both** ceilings, because the caller who hits it is nearly always someone
//! who picked a number from the other family.

use microvms_core::{BuildHookTimeout as CoreBuild, RunHookTimeout as CoreRun};
use napi_derive::napi;

use crate::errors::js;

/// A timeout for the `run`, `resume`, `suspend`, or `terminate` hook: 1..=60 seconds.
///
/// A distinct class from [`BuildHookTimeout`] and deliberately not interchangeable with it.
#[napi]
#[derive(Clone, Copy)]
pub struct RunHookTimeout {
    pub(crate) inner: CoreRun,
}

#[napi]
impl RunHookTimeout {
    /// A run-family timeout, or a refusal naming **both** ceilings.
    #[napi(constructor)]
    pub fn new(seconds: u32) -> napi::Result<RunHookTimeout, String> {
        Ok(RunHookTimeout {
            inner: CoreRun::try_new(seconds).map_err(js)?,
        })
    }

    /// The service ceiling for this family: 60.
    #[napi(getter)]
    pub fn max_secs(&self) -> u32 {
        CoreRun::MAX_SECS
    }

    #[napi(getter)]
    pub fn seconds(&self) -> u32 {
        self.inner.as_secs()
    }

    #[napi(js_name = "toString")]
    pub fn display_string(&self) -> String {
        self.inner.to_string()
    }
}

/// A timeout for the `ready` or `validate` image-build hook: 1..=3600 seconds.
#[napi]
#[derive(Clone, Copy)]
pub struct BuildHookTimeout {
    pub(crate) inner: CoreBuild,
}

#[napi]
impl BuildHookTimeout {
    /// A build-family timeout, or a refusal naming both ceilings.
    #[napi(constructor)]
    pub fn new(seconds: u32) -> napi::Result<BuildHookTimeout, String> {
        Ok(BuildHookTimeout {
            inner: CoreBuild::try_new(seconds).map_err(js)?,
        })
    }

    /// The service ceiling for this family: 3600.
    #[napi(getter)]
    pub fn max_secs(&self) -> u32 {
        CoreBuild::MAX_SECS
    }

    #[napi(getter)]
    pub fn seconds(&self) -> u32 {
        self.inner.as_secs()
    }

    #[napi(js_name = "toString")]
    pub fn display_string(&self) -> String {
        self.inner.to_string()
    }
}
