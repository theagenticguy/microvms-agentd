// SPDX-License-Identifier: Apache-2.0
//! The cost types, wrapped without loosening any of them (BIND-5).
//!
//! # What "without loosening" means here concretely
//!
//! `microvms-core`'s cost module spends three of its closures on making mistakes
//! *unavailable* rather than rejected: [`microvms_core::cost::EstimatedUsd`] has no `f64`
//! door, [`microvms_core::cost::DurationP`] is an enum whose variant *is* the provenance,
//! and `Amount::Unpriced` is a distinct variant from `Estimated(ZERO)`. A binding is
//! exactly where those get given back, and each one has a specific spelling that would do
//! it:
//!
//! * `__float__` / `__int__` / `__index__` on [`EstimatedUsd`] — absent. So is
//!   `__add__`, `__radd__`, `__mul__`, and every other numeric dunder: Python has no
//!   `Add<Amount>` compile error to lean on, so the only faithful port of "an estimate
//!   adds only to an estimate" is a type with no `+` at all. The amount comes out as
//!   `.amount`, a **string**, so a caller who wants a number types `Decimal(...)`
//!   themselves and a reviewer sees them do it.
//! * `#[pyo3(transparent)]` on the wrappers, or a `FromPyObject` that accepts a bare
//!   float — absent. Only the pyclass extracts, so a Python caller cannot pass `12.5`
//!   where an `EstimatedUsd` is wanted.
//! * A [`Duration`] constructor that defaults its provenance — absent. `Duration` has no
//!   `#[new]` at all; `Duration.measured(secs)` and `Duration.projected(secs)` are the
//!   only doors, so the Python `TypeError` `cost.py` relied on becomes "there is no
//!   constructor to call wrong".
//! * `Unpriced` folded into `Amount` as a null `usd` — absent. `Amount.usd` is `None`
//!   for an unpriced line and `to_dict()` omits the key entirely, which is `cli.py`'s
//!   own rule: a null gets summed as zero by anything permissive.
//!
//! # Every class is `frozen`
//!
//! These are value objects. `frozen` means no `__setattr__` from Python, no interior
//! mutability, and shared access without a borrow check — which is what a wrapper around
//! an immutable core newtype wants. It also removes a whole failure mode: a caller cannot
//! mutate a `LineItem`'s amount after the total was computed over it.
//!
//! # Extraction is opt-in, per type, and the split is the input/output split
//!
//! PyO3 0.29 is phasing out the blanket `FromPyObject` for `Clone` pyclasses, and the
//! opt-ins are `from_py_object` and `skip_from_py_object`. That is not a deprecation to
//! silence — it is the lever that decides whether a type can be *passed in* by value, and
//! this module takes a position on each one rather than letting a default answer:
//!
//! * `from_py_object` on [`PyDuration`], [`PySizeClass`], [`PyRateTable`] — the three
//!   types that really are inputs. A report builder takes them, so extraction has to
//!   exist. It stays safe because the *only* way a caller obtains one is a constructor
//!   this module wrote, and none of those accepts a bare number where a labelled value
//!   belongs.
//! * `skip_from_py_object` on [`PyEstimatedUsd`], [`PyUnpriced`], [`PyAmount`],
//!   [`PyLineItem`], [`PyTotal`], [`PyCostReport`] — the six that are only ever *results*.
//!   No function here takes a dollar figure, and refusing extraction says so in the type
//!   rather than in a docstring: a future signature that tried to accept an `EstimatedUsd`
//!   by value would not compile, which is the closest thing to "the core has no
//!   constructor for a bill" that a binding can hold.
//!
//! # The JSON shape is the Python oracle's, not a re-invention
//!
//! [`CostReport::to_dict`] emits the `report_to_dict` shape the deleted Python client's
//! `cli.py:688` produced — camelCase keys, `estimated: true` as a literal field,
//! quantities and dollars as strings, and an unpriced line item with no `usd` key. It was
//! a transcription rather than a design, because the two clients had to be diffable
//! against each other; the shape stays because consumers now read it.
//!
//! (cli.py line numbers resolve at `git show 'c4d396e^:clients/python/src/microvms_agentd/cli.py'` — the retired oracle.)

use std::collections::BTreeMap;

use microvms_core::SizeClass;
use microvms_core::cost::{
    self, Amount, BillingLine, CalendarDate, CostPhase, CostReport as CoreReport, DurationP,
    EstimatedUsd as CoreUsd, LineItem as CoreLineItem, PlanUsage, Provenance, RateTable, RunUsage,
    Total as CoreTotal,
};
use pyo3::prelude::*;
use pyo3::types::{PyDict, PyList};

use crate::errors::PyCoreResult;

// ── Duration and Provenance (COST-1 / BIND-5) ────────────────────────────────

/// Seconds, plus how we know them.
///
/// There is **no** `__new__`: naming a provenance is the only way to build one, and the
/// two named constructors are the only doors. `cost.py` achieved the same thing with a
/// field that had no default, so `Duration(3600)` raised `TypeError` — here there is
/// nothing to call, which is one rung stronger.
#[pyclass(frozen, from_py_object, name = "Duration", module = "microvms")]
#[derive(Clone, Copy)]
pub struct PyDuration {
    inner: DurationP,
}

impl PyDuration {
    pub(crate) fn wrap(inner: DurationP) -> Self {
        Self { inner }
    }
}

#[pymethods]
impl PyDuration {
    /// A timed phase: a clock ran and this is what it read.
    ///
    /// Fallible because the float is — a negative or non-finite figure has no reading as
    /// a duration, and the refusal is the core's `duration_of_secs_f64`.
    #[staticmethod]
    fn measured(seconds: f64) -> PyCoreResult<PyDuration> {
        Ok(PyDuration {
            inner: DurationP::measured_secs_f64(seconds)?,
        })
    }

    /// A hypothetical phase: an estimate's input, or a documented minimum nobody timed.
    #[staticmethod]
    fn projected(seconds: f64) -> PyCoreResult<PyDuration> {
        Ok(PyDuration {
            inner: DurationP::projected_secs_f64(seconds)?,
        })
    }

    /// The span in seconds, without its label.
    ///
    /// A float here and not on the money types, deliberately: seconds really are a
    /// measurement a caller does arithmetic on, and the core's own
    /// `break_even_seconds_f64` makes the same distinction for the same reason.
    #[getter]
    fn seconds(&self) -> f64 {
        self.inner.duration().as_secs_f64()
    }

    /// `"measured"` or `"projected"`, spelled as the Python `StrEnum` member.
    #[getter]
    fn provenance(&self) -> &'static str {
        self.inner.provenance().as_str()
    }

    /// True only for a timed span. What `CostReport.fully_measured` reads.
    #[getter]
    fn is_measured(&self) -> bool {
        self.inner.is_measured()
    }

    fn __str__(&self) -> String {
        self.inner.to_string()
    }

    fn __repr__(&self) -> String {
        format!(
            "Duration(seconds={}, provenance={:?})",
            self.seconds(),
            self.provenance()
        )
    }
}

// ── EstimatedUsd (COST-2 / BIND-5) ───────────────────────────────────────────

/// Dollars derived from published rates. Not the bill.
///
/// # There is no way to get a number out of this
///
/// No `__float__`, no `__int__`, no `__index__`, no `__add__`. [`Self::amount`] answers a
/// **string** — the decimal figure at full precision — so `float(usd)` raises
/// `TypeError`, `usd + usd` raises `TypeError`, and the cheapest laundering spelling
/// (`f"${float(x):.2f}"`) does not run. The core makes this a compile error; the closest
/// Python equivalent is an absence, and this is it.
///
/// A caller who genuinely needs arithmetic writes `Decimal(usd.amount)`, which is one
/// visible step, keeps the type name at the call site, and is a decision a reviewer sees.
#[pyclass(
    frozen,
    skip_from_py_object,
    name = "EstimatedUsd",
    module = "microvms"
)]
#[derive(Clone, Copy)]
pub struct PyEstimatedUsd {
    inner: CoreUsd,
}

impl PyEstimatedUsd {
    pub(crate) fn wrap(inner: CoreUsd) -> Self {
        Self { inner }
    }
}

#[pymethods]
impl PyEstimatedUsd {
    /// The figure as an exact decimal **string**.
    ///
    /// A string and not a float, which is the whole of COST-2 at this boundary. The core
    /// carries a `rust_decimal::Decimal` and the exact spelling of it is what survives
    /// into `Decimal(...)` on the Python side; an f64 would already have lost the
    /// exactness the type exists to hold.
    #[getter]
    fn amount(&self) -> String {
        self.inner.amount().to_string()
    }

    /// The figure at display precision: two places at a dollar or more, six below.
    ///
    /// A create-and-destroy suite's compute cost lives in the sixth decimal place while a
    /// warm pool's monthly bill lives in the second, and one fixed precision cannot show
    /// both.
    #[getter]
    fn display_amount(&self) -> String {
        self.inner.amount_string()
    }

    /// `~$X (estimated)` — the label travels with the figure, because a number copied out
    /// of a terminal loses its docstring.
    fn __str__(&self) -> String {
        self.inner.to_string()
    }

    fn __repr__(&self) -> String {
        format!("EstimatedUsd(amount={:?})", self.amount())
    }
}

// ── Unpriced and Amount (COST-3 / BIND-5) ────────────────────────────────────

/// A quantity we can measure but cannot price, because no rate is published.
///
/// A distinct class rather than `EstimatedUsd("0")`, and the distinction is not cosmetic:
/// zero is a claim about the bill, unpriced is a claim about the documentation. `reason`
/// is required and not optional, so a line item can say *why*.
#[pyclass(frozen, skip_from_py_object, name = "Unpriced", module = "microvms")]
#[derive(Clone)]
pub struct PyUnpriced {
    reason: String,
}

#[pymethods]
impl PyUnpriced {
    #[getter]
    fn reason(&self) -> &str {
        &self.reason
    }

    fn __str__(&self) -> String {
        format!("unpriced — {}", self.reason)
    }

    fn __repr__(&self) -> String {
        format!("Unpriced(reason={:?})", self.reason)
    }
}

/// What a line item's cost can be: an estimate, or unpriced.
///
/// A class rather than a union, because a union in Python is a documentation convention
/// and this has to be something a caller can branch on. `kind` is the stable tag
/// (`"estimated-usd"` / `"unpriced"`), `usd` is `None` for the unpriced case, and
/// `unpriced` is `None` for the priced one — so exactly one of the two is present and a
/// reader who checks either has handled both.
#[pyclass(frozen, skip_from_py_object, name = "Amount", module = "microvms")]
#[derive(Clone)]
pub struct PyAmount {
    inner: Amount,
}

impl PyAmount {
    pub(crate) fn wrap(inner: Amount) -> Self {
        Self { inner }
    }
}

#[pymethods]
impl PyAmount {
    /// `"estimated-usd"` or `"unpriced"` — the tag `cli.py` puts in `amount.kind`.
    #[getter]
    fn kind(&self) -> &'static str {
        self.inner.kind()
    }

    /// The estimate, or `None` when there is no published rate.
    ///
    /// `None` rather than a zero, which is the same decision as the JSON omitting the
    /// `usd` key: a zero gets summed by anything permissive, and that is the one
    /// arithmetic this module refuses to enable.
    #[getter]
    fn usd(&self) -> Option<PyEstimatedUsd> {
        self.inner.estimate().map(PyEstimatedUsd::wrap)
    }

    /// The unpriced half, or `None` when the line was priced.
    #[getter]
    fn unpriced(&self) -> Option<PyUnpriced> {
        self.inner.unpriced_reason().map(|reason| PyUnpriced {
            reason: reason.to_string(),
        })
    }

    fn __str__(&self) -> String {
        self.inner.to_string()
    }

    fn __repr__(&self) -> String {
        format!("Amount(kind={:?})", self.kind())
    }
}

// ── LineItem ─────────────────────────────────────────────────────────────────

/// One phase's one billing line: what was consumed, and what that costs.
///
/// `quantity` is a string for the same reason `EstimatedUsd.amount` is: division by 730
/// hours yields 28 significant digits and the exact figure is what lets a reader check
/// the arithmetic against the rate table, which is the only defence against a rate that
/// drifts without anyone noticing.
#[pyclass(frozen, skip_from_py_object, name = "LineItem", module = "microvms")]
#[derive(Clone)]
pub struct PyLineItem {
    inner: CoreLineItem,
}

impl PyLineItem {
    fn wrap(inner: CoreLineItem) -> Self {
        Self { inner }
    }
}

#[pymethods]
impl PyLineItem {
    /// The lifecycle phase, as the Python `StrEnum` spells it.
    #[getter]
    fn phase(&self) -> &'static str {
        self.inner.phase.as_str()
    }

    /// The AWS billing line, or `None` for a phase with no published rate to attribute
    /// it to.
    #[getter]
    fn line(&self) -> Option<&'static str> {
        self.inner.line.map(BillingLine::as_str)
    }

    /// The consumed quantity, exact, as a string.
    #[getter]
    fn quantity(&self) -> String {
        self.inner.quantity.to_string()
    }

    /// The quantity at display precision, for a column a human scans.
    #[getter]
    fn display_quantity(&self) -> String {
        self.inner.quantity_string()
    }

    #[getter]
    fn unit(&self) -> &str {
        &self.inner.unit
    }

    #[getter]
    fn amount(&self) -> PyAmount {
        PyAmount::wrap(self.inner.amount.clone())
    }

    #[getter]
    fn duration(&self) -> Option<PyDuration> {
        self.inner.duration.map(PyDuration::wrap)
    }

    #[getter]
    fn note(&self) -> &str {
        &self.inner.note
    }

    /// The `cli.py` `_line_to_dict` shape. An unpriced line has **no** `usd` key at all.
    fn to_dict<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyDict>> {
        line_to_dict(py, &self.inner)
    }

    fn __str__(&self) -> String {
        self.inner.to_string()
    }

    fn __repr__(&self) -> String {
        format!(
            "LineItem(phase={:?}, unit={:?}, amount={:?})",
            self.phase(),
            self.unit(),
            self.inner.amount.kind()
        )
    }
}

/// `cli.py:725 _line_to_dict`, transcribed.
///
/// The `usd` key is **absent** for an unpriced line rather than null. That is the one
/// place this shape is load-bearing rather than conventional: a null is summed as zero by
/// anything permissive, and the whole cost module exists to not enable that arithmetic.
fn line_to_dict<'py>(py: Python<'py>, item: &CoreLineItem) -> PyResult<Bound<'py, PyDict>> {
    let amount = PyDict::new(py);
    amount.set_item("kind", item.amount.kind())?;
    match (item.amount.estimate(), item.amount.unpriced_reason()) {
        (Some(usd), _) => amount.set_item("usd", usd.amount().to_string())?,
        (None, Some(reason)) => amount.set_item("reason", reason)?,
        // Unreachable while `Amount` has two variants; written rather than `unreachable!()`
        // because a panic across the FFI boundary is not an ordinary error and a third
        // variant should degrade to a visibly incomplete dict, not abort the interpreter.
        (None, None) => {}
    }

    let duration = match item.duration {
        Some(duration) => {
            let mapped = PyDict::new(py);
            mapped.set_item("seconds", duration.duration().as_secs_f64())?;
            mapped.set_item("provenance", duration.provenance().as_str())?;
            Some(mapped)
        }
        None => None,
    };

    let dict = PyDict::new(py);
    dict.set_item("phase", item.phase.as_str())?;
    dict.set_item("line", item.line.map(BillingLine::as_str))?;
    dict.set_item("quantity", item.quantity.to_string())?;
    dict.set_item("unit", &item.unit)?;
    dict.set_item("amount", amount)?;
    dict.set_item("duration", duration)?;
    dict.set_item("note", &item.note)?;
    Ok(dict)
}

// ── Total (COST-4) ───────────────────────────────────────────────────────────

/// A report's total, which is a lower bound whenever anything is unpriced.
///
/// The core makes `AtLeast { floor, unpriced }` a *different variant* from `Exact`, so a
/// lower bound cannot be read without seeing the reasons. Python has no exhaustive match
/// to lean on, so the port keeps the floor under a name that says what it is:
/// [`Self::floor`], never `total`. A caller reading a field called `total` would have no
/// reason to check `is_lower_bound`.
#[pyclass(frozen, skip_from_py_object, name = "Total", module = "microvms")]
#[derive(Clone)]
pub struct PyTotal {
    inner: CoreTotal,
}

#[pymethods]
impl PyTotal {
    /// Everything that could be priced. For a lower bound this is **not** the total.
    #[getter]
    fn floor(&self) -> PyEstimatedUsd {
        PyEstimatedUsd::wrap(self.inner.floor())
    }

    /// True when line items with no published rate are missing from the floor.
    #[getter]
    fn is_lower_bound(&self) -> bool {
        self.inner.is_lower_bound()
    }

    /// Why each unpriced line could not be priced, in report order. Empty for an exact
    /// total.
    #[getter]
    fn unpriced_reasons(&self) -> Vec<String> {
        self.inner.unpriced_reasons().to_vec()
    }

    /// `at least ~$X (estimated), plus N unpriced (...)` — "at least" leads, because that
    /// is the whole point.
    fn __str__(&self) -> String {
        self.inner.to_string()
    }

    fn __repr__(&self) -> String {
        format!(
            "Total(floor={:?}, is_lower_bound={})",
            self.inner.floor().amount().to_string(),
            self.is_lower_bound()
        )
    }
}

// ── SizeClass and RateTable ──────────────────────────────────────────────────

/// One of the five documented size classes.
///
/// `minimumMemoryInMiB` selects a class; it does not size a VM. Both numbers are on the
/// class or neither, because naming only the peak invites budgeting for memory nobody is
/// billed for and naming only the baseline invites a pressure test against a ceiling four
/// times too low.
#[pyclass(frozen, from_py_object, name = "SizeClass", module = "microvms")]
#[derive(Clone, Copy)]
pub struct PySizeClass {
    pub(crate) inner: SizeClass,
}

#[pymethods]
impl PySizeClass {
    /// The class `minimumMemoryInMiB = mib` selects, or a refusal (TRAP-10).
    ///
    /// Off-table figures are refused rather than snapped to a neighbour: the two
    /// plausible readings differ in both memory and rate, and neither has been measured.
    #[staticmethod]
    fn from_baseline_mib(mib: u32) -> PyCoreResult<PySizeClass> {
        Ok(PySizeClass {
            inner: SizeClass::from_baseline_mib(mib)?,
        })
    }

    /// The platform's default, 2048 MiB. Not the smallest — a 0.5 GB baseline hands
    /// someone a sandbox that OOM-kills a real test suite, and the guest has no swap.
    #[staticmethod]
    fn default_class() -> PySizeClass {
        PySizeClass {
            inner: SizeClass::DEFAULT,
        }
    }

    /// Every class, smallest first.
    #[staticmethod]
    fn all() -> Vec<PySizeClass> {
        SizeClass::ALL
            .into_iter()
            .map(|inner| PySizeClass { inner })
            .collect()
    }

    #[getter]
    fn baseline_mib(&self) -> u32 {
        self.inner.baseline_mib()
    }

    #[getter]
    fn baseline_vcpu(&self) -> f64 {
        self.inner.baseline_vcpu()
    }

    #[getter]
    fn peak_mib(&self) -> u32 {
        self.inner.peak_mib()
    }

    #[getter]
    fn peak_vcpu(&self) -> f64 {
        self.inner.peak_vcpu()
    }

    /// The figure a GB-second rate multiplies. Always the baseline, never the peak.
    #[getter]
    fn baseline_gb(&self) -> f64 {
        self.inner.baseline_gb()
    }

    /// The peak in GB, which is what the guest reports as `MemTotal`.
    #[getter]
    fn peak_gb(&self) -> f64 {
        self.inner.peak_gb()
    }

    /// One line naming both numbers. `cli.py` calls this `describe()`.
    fn describe(&self) -> String {
        self.inner.to_string()
    }

    fn __str__(&self) -> String {
        self.inner.to_string()
    }

    fn __repr__(&self) -> String {
        format!("SizeClass(baseline_mib={})", self.baseline_mib())
    }
}

/// The pinned rate table, and everything it says about itself.
///
/// The rates are exact decimal **strings** for the same reason the money types are: they
/// are figures like `0.0000276944` and an f64 round trip through Python would make a
/// report irreconcilable against the page it came from.
#[pyclass(frozen, from_py_object, name = "RateTable", module = "microvms")]
#[derive(Clone)]
pub struct PyRateTable {
    inner: RateTable,
}

#[pymethods]
impl PyRateTable {
    /// us-east-1, read 2026-08-07, as recorded in `docs/PLATFORM.md`.
    ///
    /// There is deliberately no constructor taking rates. The core's `RateTable` has
    /// private rate fields and exactly two doors — this one and `from_catalog`, which
    /// refuses a catalog whose ARM compute line is missing rather than substituting the
    /// x86 one (COST-9, 17.9% higher). A binding constructor taking five numbers would
    /// reopen precisely that.
    #[staticmethod]
    fn pinned() -> PyRateTable {
        PyRateTable {
            inner: cost::pinned_rates(),
        }
    }

    #[getter]
    fn region(&self) -> String {
        self.inner.region().to_string()
    }

    #[getter]
    fn source_url(&self) -> &str {
        self.inner.source_url()
    }

    /// ISO 8601, matching the Python's `retrieved.isoformat()` so the two are diffable.
    #[getter]
    fn retrieved(&self) -> String {
        self.inner.retrieved().to_string()
    }

    #[getter]
    fn vcpu_second(&self) -> String {
        self.inner.vcpu_second().to_string()
    }

    #[getter]
    fn gb_second(&self) -> String {
        self.inner.gb_second().to_string()
    }

    /// Per GB-month. The one derived figure: the API quotes per GB-hour and this is that
    /// times 730.
    #[getter]
    fn storage_gb_month(&self) -> String {
        self.inner.storage_gb_month().to_string()
    }

    #[getter]
    fn snapshot_read_gb(&self) -> String {
        self.inner.snapshot_read_gb().to_string()
    }

    #[getter]
    fn snapshot_write_gb(&self) -> String {
        self.inner.snapshot_write_gb().to_string()
    }

    /// Snapshot storage bills at least this long however briefly the snapshot exists.
    #[getter]
    fn minimum_retention_seconds(&self) -> f64 {
        self.inner.minimum_retention().as_secs_f64()
    }

    /// Zero: MicroVMs bills per second with no per-request charge.
    #[getter]
    fn per_request(&self) -> String {
        self.inner.per_request().to_string()
    }

    /// True: vCPU and memory are two line items, as the pricing page prices them.
    #[getter]
    fn bills_vcpu_and_memory_separately(&self) -> bool {
        self.inner.bills_vcpu_and_memory_separately()
    }

    /// False: no published MicroVMs free tier. The Lambda one is Functions-only.
    #[getter]
    fn free_tier(&self) -> bool {
        self.inner.free_tier()
    }

    /// `None` means **not published**, not one second. Nothing rounds a duration up,
    /// because inventing an increment would overcharge every short exec.
    #[getter]
    fn minimum_billing_increment_sec(&self) -> Option<f64> {
        self.inner
            .minimum_billing_increment()
            .map(|increment| increment.as_secs_f64())
    }

    /// How many days ago these rates were read, against today UTC.
    fn age_days(&self) -> i64 {
        self.inner.age_days(CalendarDate::today_utc())
    }

    /// The staleness warning, or `None` when the table is fresh.
    fn staleness(&self) -> Option<String> {
        self.inner.staleness(CalendarDate::today_utc())
    }

    fn __repr__(&self) -> String {
        format!(
            "RateTable(region={:?}, retrieved={:?})",
            self.region(),
            self.retrieved()
        )
    }
}

// ── CostReport ───────────────────────────────────────────────────────────────

/// Per-phase attribution for one sandbox, measured or projected.
#[pyclass(frozen, skip_from_py_object, name = "CostReport", module = "microvms")]
#[derive(Clone)]
pub struct PyCostReport {
    inner: CoreReport,
}

impl PyCostReport {
    fn wrap(inner: CoreReport) -> Self {
        Self { inner }
    }
}

#[pymethods]
impl PyCostReport {
    #[getter]
    fn label(&self) -> &str {
        self.inner.label()
    }

    #[getter]
    fn size(&self) -> PySizeClass {
        PySizeClass {
            inner: self.inner.size(),
        }
    }

    #[getter]
    fn rates(&self) -> PyRateTable {
        PyRateTable {
            inner: self.inner.rates().clone(),
        }
    }

    #[getter]
    fn items(&self) -> Vec<PyLineItem> {
        self.inner
            .items()
            .iter()
            .cloned()
            .map(PyLineItem::wrap)
            .collect()
    }

    /// The line items with a published rate.
    #[getter]
    fn priced(&self) -> Vec<PyLineItem> {
        self.inner.priced().cloned().map(PyLineItem::wrap).collect()
    }

    /// The line items with no published rate.
    #[getter]
    fn unpriced(&self) -> Vec<PyLineItem> {
        self.inner
            .unpriced()
            .cloned()
            .map(PyLineItem::wrap)
            .collect()
    }

    /// The total, which is a lower bound whenever anything is unpriced.
    #[getter]
    fn total(&self) -> PyTotal {
        PyTotal {
            inner: self.inner.total(),
        }
    }

    /// False whenever any phase has no published rate.
    #[getter]
    fn complete(&self) -> bool {
        self.inner.is_complete()
    }

    /// True only if every duration was timed. An estimate is never this.
    #[getter]
    fn fully_measured(&self) -> bool {
        self.inner.fully_measured()
    }

    /// The staleness warning the table carried when this was computed.
    #[getter]
    fn staleness(&self) -> Option<&str> {
        self.inner.staleness()
    }

    /// The line items belonging to one phase.
    ///
    /// The string is judged by the core's own [`CostPhase::from_str`], which is where the
    /// closed set lives. This module used to carry its own seven-element table for it — a
    /// parallel list over an enum, which would have gone stale the first time a phase was
    /// added and would have disagreed with the JS binding's identical copy in whichever
    /// direction was edited first.
    fn by_phase(&self, phase: &str) -> PyCoreResult<Vec<PyLineItem>> {
        let phase: CostPhase = phase.parse()?;
        Ok(self
            .inner
            .by_phase(phase)
            .cloned()
            .map(PyLineItem::wrap)
            .collect())
    }

    /// Plain text, leading with what the dollars are rather than with the dollars.
    fn render(&self) -> String {
        self.inner.render()
    }

    /// The `cli.py:688 report_to_dict` shape, key for key.
    fn to_dict<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyDict>> {
        let size = PyDict::new(py);
        size.set_item("baselineMib", self.inner.size().baseline_mib())?;
        size.set_item("baselineVcpu", self.inner.size().baseline_vcpu())?;
        size.set_item("peakMib", self.inner.size().peak_mib())?;
        size.set_item("peakVcpu", self.inner.size().peak_vcpu())?;
        size.set_item("describe", self.inner.size().to_string())?;

        let rates = PyDict::new(py);
        rates.set_item("region", self.inner.rates().region().to_string())?;
        rates.set_item("retrieved", self.inner.rates().retrieved().to_string())?;
        rates.set_item("sourceUrl", self.inner.rates().source_url())?;

        let items = PyList::empty(py);
        for item in self.inner.items() {
            items.append(line_to_dict(py, item)?)?;
        }

        let total = self.inner.total();
        let total_dict = PyDict::new(py);
        total_dict.set_item("priced", total.floor().amount().to_string())?;
        total_dict.set_item("isLowerBound", total.is_lower_bound())?;
        total_dict.set_item("render", total.to_string())?;

        let dict = PyDict::new(py);
        dict.set_item("label", self.inner.label())?;
        dict.set_item("size", size)?;
        dict.set_item("rates", rates)?;
        // Not "cost": these are estimates derived from published rates, and the field
        // name is the only place the distinction survives a copy-paste.
        dict.set_item("estimated", true)?;
        dict.set_item("fullyMeasured", self.inner.fully_measured())?;
        dict.set_item("complete", self.inner.is_complete())?;
        dict.set_item("staleness", self.inner.staleness())?;
        dict.set_item("items", items)?;
        dict.set_item("total", total_dict)?;
        Ok(dict)
    }

    fn __str__(&self) -> String {
        self.inner.render()
    }

    fn __repr__(&self) -> String {
        format!(
            "CostReport(label={:?}, complete={}, fully_measured={})",
            self.label(),
            self.complete(),
            self.fully_measured()
        )
    }
}

/// Running versus suspended for the same VM over the same wall time.
#[pyclass(frozen, name = "ResidencyComparison", module = "microvms")]
pub struct PyResidencyComparison {
    inner: cost::ResidencyComparison,
}

#[pymethods]
impl PyResidencyComparison {
    #[getter]
    fn size(&self) -> PySizeClass {
        PySizeClass {
            inner: self.inner.size(),
        }
    }

    /// The wall time both sides cover. Always projected: a comparison is a hypothetical
    /// about a hold nobody has taken yet.
    #[getter]
    fn hold(&self) -> PyDuration {
        PyDuration::wrap(self.inner.hold())
    }

    #[getter]
    fn cycles(&self) -> u32 {
        self.inner.cycles()
    }

    #[getter]
    fn running(&self) -> PyCostReport {
        PyCostReport::wrap(self.inner.running().clone())
    }

    #[getter]
    fn suspended(&self) -> PyCostReport {
        PyCostReport::wrap(self.inner.suspended().clone())
    }

    /// How many times more the running VM costs, as an exact decimal string.
    #[getter]
    fn ratio(&self) -> String {
        self.inner.ratio().to_string()
    }

    /// One suspend/resume: a snapshot write plus a read, per GB.
    ///
    /// On the comparison because without it the honest conclusion inverts — "suspend
    /// constantly" reads as free.
    fn per_cycle(&self) -> PyCoreResult<PyEstimatedUsd> {
        Ok(PyEstimatedUsd::wrap(self.inner.per_cycle()?))
    }

    /// How long a VM must stay suspended for the cycle to pay for itself, exact, as a
    /// string.
    ///
    /// The number a pool scheduler actually needs, and the one a bare "100x cheaper"
    /// headline hides.
    fn break_even_seconds(&self) -> PyCoreResult<String> {
        Ok(self.inner.break_even_seconds()?.to_string())
    }

    /// The break-even hold as a float, for a JSON envelope. **Lossy, and named so.**
    ///
    /// Seconds, not dollars: no money figure has a float accessor anywhere in this
    /// module.
    fn break_even_seconds_float(&self) -> PyCoreResult<f64> {
        Ok(self.inner.break_even_seconds_f64()?)
    }

    fn render(&self) -> PyCoreResult<String> {
        Ok(self.inner.render()?)
    }

    fn __str__(&self) -> String {
        self.inner
            .render()
            .unwrap_or_else(|error| format!("<unrenderable comparison: {error}>"))
    }
}

// ── the report builders ──────────────────────────────────────────────────────

/// Per-phase attribution for one sandbox's lifecycle.
///
/// Every duration parameter takes a [`PyDuration`] and not a number, which is what keeps
/// the provenance label attached: passing seconds here would need this function to pick a
/// provenance, and the one it would pick is the stronger claim.
///
/// `today` is the core's `CalendarDate::today_utc()` rather than a parameter. The core
/// takes it so a report is a pure function of its inputs and a test does not have to
/// travel in time; a Python caller who needs that reaches for the core through Rust, and
/// exposing a date here would be a knob whose only use is faking staleness.
#[pyfunction]
#[pyo3(signature = (
    size,
    *,
    running=None,
    suspended=None,
    image_build=None,
    image_gb=None,
    image_retained=None,
    suspend_resume_cycles=0,
    snapshot_gb=None,
    launched=true,
    label="run",
    rates=None,
))]
#[allow(
    clippy::too_many_arguments,
    reason = "one keyword-only parameter per \
     RunUsage field; collapsing them into a dict would give up the per-field types that \
     make a Duration impossible to pass as a bare number"
)]
pub(crate) fn run_report(
    size: PySizeClass,
    running: Option<PyDuration>,
    suspended: Option<PyDuration>,
    image_build: Option<PyDuration>,
    image_gb: Option<f64>,
    image_retained: Option<PyDuration>,
    suspend_resume_cycles: u32,
    snapshot_gb: Option<f64>,
    launched: bool,
    label: &str,
    rates: Option<PyRateTable>,
) -> PyCoreResult<PyCostReport> {
    let usage = RunUsage {
        running: running.map(|duration| duration.inner),
        suspended: suspended.map(|duration| duration.inner),
        image_build: image_build.map(|duration| duration.inner),
        image_gb,
        image_retained: image_retained.map(|duration| duration.inner),
        suspend_resume_cycles,
        snapshot_gb,
        launched,
    };
    let table = rates.map_or_else(cost::pinned_rates, |rates| rates.inner);
    Ok(PyCostReport::wrap(cost::run_report(
        size.inner,
        &usage,
        &table,
        CalendarDate::today_utc(),
        label,
    )?))
}

/// What a plan will cost, before spending anything (COST-10).
///
/// Takes plain seconds and marks every one of them projected. That is the difference from
/// [`run_report`]: not the arithmetic, which is shared, but what the durations admit about
/// themselves — and it is why this signature has no `Duration` parameter at all, so an
/// accidentally-measured one is not something a caller can write.
#[pyfunction]
#[pyo3(signature = (
    size,
    *,
    running_seconds=0.0,
    suspended_seconds=0.0,
    image_gb=None,
    image_retained_seconds=None,
    suspend_resume_cycles=0,
    snapshot_gb=None,
    launched=true,
    label="plan",
    rates=None,
))]
#[allow(
    clippy::too_many_arguments,
    reason = "mirrors PlanUsage field for field, \
     deliberately"
)]
pub(crate) fn estimate_run(
    size: PySizeClass,
    running_seconds: f64,
    suspended_seconds: f64,
    image_gb: Option<f64>,
    image_retained_seconds: Option<f64>,
    suspend_resume_cycles: u32,
    snapshot_gb: Option<f64>,
    launched: bool,
    label: &str,
    rates: Option<PyRateTable>,
) -> PyCoreResult<PyCostReport> {
    let plan = PlanUsage {
        running_seconds,
        suspended_seconds,
        image_gb,
        image_retained_seconds,
        suspend_resume_cycles,
        snapshot_gb,
        launched,
    };
    let table = rates.map_or_else(cost::pinned_rates, |rates| rates.inner);
    Ok(PyCostReport::wrap(cost::estimate_run(
        size.inner,
        &plan,
        &table,
        CalendarDate::today_utc(),
        label,
    )?))
}

/// The warm-pool argument, with its own counter-argument attached.
#[pyfunction]
#[pyo3(signature = (size, hold_seconds, cycles=1, *, rates=None))]
pub(crate) fn compare_residency(
    size: PySizeClass,
    hold_seconds: f64,
    cycles: u32,
    rates: Option<PyRateTable>,
) -> PyCoreResult<PyResidencyComparison> {
    let hold = cost::duration_of_secs_f64(hold_seconds)?;
    let table = rates.map_or_else(cost::pinned_rates, |rates| rates.inner);
    Ok(PyResidencyComparison {
        inner: cost::compare_residency(
            size.inner,
            hold,
            cycles,
            &table,
            CalendarDate::today_utc(),
        )?,
    })
}

/// Why the image build has no price, as the reason that lands on the line item.
#[pyfunction]
pub(crate) fn build_unpriced_reason() -> &'static str {
    cost::BUILD_UNPRICED_REASON
}

/// The documented facts a caller may want to assert on, as strings and flags.
///
/// A dict rather than a class because it is a transcription of published constants, and
/// the Python client publishes the same set. `provenances` and `phases` are here so a
/// caller can enumerate the closed sets rather than hardcoding spellings.
#[pyfunction]
pub(crate) fn cost_constants<'py>(py: Python<'py>) -> PyResult<Bound<'py, PyDict>> {
    let dict = PyDict::new(py);
    dict.set_item("secondsPerMonth", cost::SECONDS_PER_MONTH.to_string())?;
    dict.set_item("hoursPerMonth", cost::HOURS_PER_MONTH.to_string())?;
    dict.set_item("staleAfterDays", cost::STALE_AFTER_DAYS)?;
    dict.set_item(
        "minimumRetentionSeconds",
        cost::MINIMUM_RETENTION.as_secs_f64(),
    )?;
    dict.set_item(
        "provenances",
        [
            Provenance::Measured.as_str(),
            Provenance::Projected.as_str(),
        ],
    )?;
    dict.set_item("phases", CostPhase::ALL.map(CostPhase::as_str))?;
    dict.set_item(
        "billingLines",
        [
            BillingLine::Vcpu.as_str(),
            BillingLine::Memory.as_str(),
            BillingLine::SnapshotStorage.as_str(),
            BillingLine::SnapshotRead.as_str(),
            BillingLine::SnapshotWrite.as_str(),
        ],
    )?;
    let mut sizes: BTreeMap<u32, String> = BTreeMap::new();
    for class in SizeClass::ALL {
        sizes.insert(class.baseline_mib(), class.to_string());
    }
    dict.set_item("sizeClasses", sizes)?;
    Ok(dict)
}
