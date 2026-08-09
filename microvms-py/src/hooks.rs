// SPDX-License-Identifier: Apache-2.0
//! The two hook-timeout families, kept as two types (BIND-2).
//!
//! # Why two classes and not two integer parameters
//!
//! The ceilings are 60x apart: the `run`/`resume`/`suspend`/`terminate` family caps at 60
//! seconds and the `ready`/`validate` image family at 3600. In Rust the core makes that
//! S1 — [`microvms_core::RunHookTimeout`] and [`microvms_core::BuildHookTimeout`] are
//! separate types with no conversion in either direction, so a 3600-second build timeout
//! cannot reach a field that caps at 60.
//!
//! Two `int` parameters would give that away. `build_image(run_hook_timeout_sec=3600)`
//! would be a runtime refusal instead of an unwriteable statement, and worse, the two
//! numbers can be *transposed* — which is the specific mistake the two types exist to
//! prevent and the one an integer parameter cannot see.
//!
//! So both are `#[pyclass]`es. PyO3 extracts a pyclass by identity, so passing a
//! `BuildHookTimeout` where a `RunHookTimeout` is wanted is a `TypeError` raised by the
//! argument conversion **before** any Rust code runs, and the transposition is
//! inexpressible in Python exactly as it is in Rust. The range check inside each
//! constructor is the core's `try_new`, message and all.

use microvms_core::{BuildHookTimeout, RunHookTimeout};
use pyo3::prelude::*;

use crate::errors::PyCoreResult;

/// A timeout for the `run`, `resume`, `suspend`, or `terminate` hook: 1..=60 seconds.
///
/// A distinct class from [`PyBuildHookTimeout`] and deliberately not interchangeable with
/// it — see the module docs.
#[pyclass(frozen, from_py_object, name = "RunHookTimeout", module = "microvms")]
#[derive(Clone, Copy)]
pub struct PyRunHookTimeout {
    pub(crate) inner: RunHookTimeout,
}

#[pymethods]
impl PyRunHookTimeout {
    /// A run-family timeout, or a refusal naming **both** ceilings.
    ///
    /// The message names the other family's limit too, because the caller who hits this is
    /// nearly always someone who picked a build-hook number: telling them 60 is the limit
    /// answers a question they did not ask.
    #[new]
    fn new(seconds: u32) -> PyCoreResult<PyRunHookTimeout> {
        Ok(PyRunHookTimeout {
            inner: RunHookTimeout::try_new(seconds)?,
        })
    }

    /// The service ceiling for this family: 60.
    #[classattr]
    #[allow(
        non_snake_case,
        reason = "a class attribute, spelled as Python spells a \
         constant rather than as Rust spells a method"
    )]
    fn MAX_SECS() -> u32 {
        RunHookTimeout::MAX_SECS
    }

    #[getter]
    fn seconds(&self) -> u32 {
        self.inner.as_secs()
    }

    fn __str__(&self) -> String {
        self.inner.to_string()
    }

    fn __repr__(&self) -> String {
        format!("RunHookTimeout({})", self.inner.as_secs())
    }
}

/// A timeout for the `ready` or `validate` image-build hook: 1..=3600 seconds.
#[pyclass(frozen, from_py_object, name = "BuildHookTimeout", module = "microvms")]
#[derive(Clone, Copy)]
pub struct PyBuildHookTimeout {
    pub(crate) inner: BuildHookTimeout,
}

#[pymethods]
impl PyBuildHookTimeout {
    /// A build-family timeout, or a refusal naming both ceilings.
    #[new]
    fn new(seconds: u32) -> PyCoreResult<PyBuildHookTimeout> {
        Ok(PyBuildHookTimeout {
            inner: BuildHookTimeout::try_new(seconds)?,
        })
    }

    /// The service ceiling for this family: 3600.
    #[classattr]
    #[allow(
        non_snake_case,
        reason = "a class attribute, spelled as Python spells a \
         constant"
    )]
    fn MAX_SECS() -> u32 {
        BuildHookTimeout::MAX_SECS
    }

    #[getter]
    fn seconds(&self) -> u32 {
        self.inner.as_secs()
    }

    fn __str__(&self) -> String {
        self.inner.to_string()
    }

    fn __repr__(&self) -> String {
        format!("BuildHookTimeout({})", self.inner.as_secs())
    }
}

/// Registers both timeout classes on the module.
pub(crate) fn register(module: &Bound<'_, PyModule>) -> PyResult<()> {
    module.add_class::<PyRunHookTimeout>()?;
    module.add_class::<PyBuildHookTimeout>()?;
    Ok(())
}
