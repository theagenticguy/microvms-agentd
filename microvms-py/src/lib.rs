// SPDX-License-Identifier: Apache-2.0
//! Python bindings over `microvms-core`: a thin wrapper that cannot reopen a trap.
//!
//! # What this crate is, and what it deliberately is not
//!
//! It is a total, thin mapping. Every public core constructor gets one binding
//! constructor, every accessor one getter, and **no arithmetic or coercion surface the core
//! does not have**. That last clause is the whole design constraint, and it is the reason
//! this crate is worth reading rather than skimming: `microvms-core` spends most of its
//! length making mistakes *unavailable* rather than rejected, and a binding is exactly
//! where that gets given back for free.
//!
//! It is not a place where validation lives. There is no local range check, no state check,
//! no region check, no size check anywhere in these files. Every refusal a Python caller
//! sees came from the core, with the core's message naming the `docs/PLATFORM.md` finding
//! that measured the behaviour. Where a gap in the core would let a mistake through, the
//! rule this crate was built under is to note the gap and leave it — because a guard added
//! here is the copy every Python caller hits and the copy nothing else tests.
//!
//! # The four closures a binding could have given away, and what stops each
//!
//! * **A dollar amount as a number.** [`cost::PyEstimatedUsd`] has no `__float__`,
//!   `__int__`, `__index__`, or `__add__`, and `.amount` answers a *string*. `float(usd)`
//!   raises `TypeError`. In Rust the same mistake is a missing impl; the Python equivalent
//!   of a missing impl is an absent dunder, and this is it.
//! * **An unlabelled duration.** [`cost::PyDuration`] has no `__new__` at all —
//!   `Duration.measured(secs)` and `Duration.projected(secs)` are the only doors, so the
//!   provenance cannot be omitted rather than merely defaulting wrong.
//! * **A region that does not carry MicroVMs.** [`region::PyRegion`] is a class whose only
//!   constructors are the five names, a `parse` that refuses everything else, and an
//!   `unlisted` that says at the call site that someone opted into the null-message trap
//!   (TRAP-6). No method on this surface takes a region string.
//! * **Two hook timeouts whose ceilings are 60x apart, transposed.**
//!   [`hooks::PyRunHookTimeout`] and [`hooks::PyBuildHookTimeout`] are separate
//!   `#[pyclass]`es, so passing one where the other is wanted is a `TypeError` from PyO3's
//!   own argument conversion, before any Rust runs.
//!
//! And the one that needed no work at all: there is **no `client_token` parameter** here,
//! because there is no such field on the core's request types. TRAP-1 is closed by absence
//! at both levels.
//!
//! # Sync methods over an async core, with the GIL released
//!
//! Every method blocks on one shared multi-thread tokio runtime, with `py.detach` first.
//! See [`runtime`] for why sync rather than `asyncio`, why one runtime, and why the
//! re-entrancy guard is not optional.
//!
//! # Layout
//!
//! [`runtime`] is the bridge. [`errors`] is the exception hierarchy and the one conversion
//! from a core `Error`. [`region`], [`hooks`], and [`cost`] are the value types.
//! [`session`] and [`exec`] are the in-VM surface; [`sandbox`] is the lifecycle.

mod cost;
mod errors;
mod exec;
mod hooks;
mod region;
mod runtime;
mod sandbox;
mod session;

use pyo3::prelude::*;

/// The core crate's version, for a `doctor` or `manifest` command to report.
///
/// The **core's** version and not this crate's: what a caller needs to know is which client
/// they are talking through, and a binding version that drifted from it would be a second
/// number nobody can act on.
#[pyfunction]
fn core_version() -> &'static str {
    microvms_core::VERSION
}

// ── why this is a `mod` and not a `fn` ──────────────────────────────────────────────
//
// A plain comment rather than a doc comment, and that is load-bearing here: a `#[pymodule]`
// doc comment becomes the module's Python `__doc__`, so it is what `help(microvms)` prints
// and what heads the generated `microvms.pyi`. This paragraph is for a reader of this file,
// not for a caller at a REPL, so it stays out of the docstring. The one line below is the
// docstring, and it is deliberately the whole of it.
//
// This was `fn microvms(module: &Bound<'_, PyModule>)` with a chain of `register(module)?`
// calls. Both forms build the identical module at runtime — the 198 tests that passed before
// this changed pass after it, unaltered. What differs is what pyo3 can *say* about the module
// afterwards.
//
// `#[pyclass]` and `#[pyfunction]` emit introspection records under
// `pyo3/experimental-inspect` either way, so the classes were always describable. The
// module's *membership* is not: `add_class::<T>()` is a call made at import time, and a macro
// cannot see the result of a function it does not run. pyo3 therefore records the `fn` form as
// `{"incomplete":true,"members":[]}`, and `maturin generate-stubs` over that blob emits six
// lines whose entire content is `def __getattr__(name: str) -> Incomplete` — a stub that types
// every name as `Any`, shipped beside a `py.typed` marker promising a checker the opposite.
// The declarative form lists its members in the attribute, so the macro knows all 33 of them
// (26 classes and 7 functions) and the generated stub is the real surface.
//
// The cost is that membership is declared in one place instead of in seven `register`
// functions, which is why those are gone rather than merely unused. The benefit is that
// `microvms.pyi` is a function of this file, and `mise run stubs:check` fails when the two
// disagree.
//
// The exceptions stay imperative in `init` below, and not by preference: `create_exception!`
// builds its type at runtime rather than through the `#[pyclass]` macro, so there is no record
// for `#[pymodule_export]` to carry and no way to declare them here. `./scripts/generate-py-stubs.py`
// reads them out of the built module instead.
/// The MicroVMs client, as Python sees it.
#[pymodule]
mod microvms {
    #[pymodule_export]
    use super::core_version;
    #[pymodule_export]
    use super::cost::{
        PyAmount, PyCostReport, PyDuration, PyEstimatedUsd, PyLineItem, PyRateTable,
        PyResidencyComparison, PySizeClass, PyTotal, PyUnpriced, build_unpriced_reason,
        compare_residency, cost_constants, estimate_run, run_report,
    };
    #[pymodule_export]
    use super::exec::{
        ExecStream, PyExecHandle, PyExecResult, PyExit, PyGap, PyOutputChunk, PyStdinAck,
    };
    #[pymodule_export]
    use super::hooks::{PyBuildHookTimeout, PyRunHookTimeout};
    #[pymodule_export]
    use super::region::PyRegion;
    #[pymodule_export]
    use super::sandbox::{PyBaseImage, PyImage, PySandbox, PyTeardownReport};
    #[pymodule_export]
    use super::session::{PyHealth, PySession, session_constants};

    #[pymodule_init]
    fn init(module: &pyo3::Bound<'_, pyo3::types::PyModule>) -> pyo3::PyResult<()> {
        use pyo3::types::PyModuleMethods;
        module.add("__version__", microvms_core::VERSION)?;
        super::errors::register(module)?;
        Ok(())
    }
}
