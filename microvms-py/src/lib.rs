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

/// The MicroVMs client, as Python sees it.
#[pymodule]
fn microvms(module: &Bound<'_, PyModule>) -> PyResult<()> {
    module.add("__version__", microvms_core::VERSION)?;
    module.add_function(wrap_pyfunction!(core_version, module)?)?;
    errors::register(module)?;
    region::register(module)?;
    hooks::register(module)?;
    cost::register(module)?;
    exec::register(module)?;
    session::register(module)?;
    sandbox::register(module)?;
    Ok(())
}
