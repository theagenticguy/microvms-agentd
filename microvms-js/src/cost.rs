// SPDX-License-Identifier: Apache-2.0
//! The cost types as napi **classes**, which is the whole of BIND-5 on this side.
//!
//! # `#[napi]` classes, never `#[napi(object)]`
//!
//! `#[napi(object)]` converts to and from a plain JS object *by structure*: `{ amount: 1.5 }`
//! would satisfy an `EstimatedUsd` parameter, and a returned one would arrive in JS as a
//! bag of fields with no identity. That is exactly the coercion these types exist to
//! prevent, so every type here is a `#[napi]` **class**. napi v3 generates a real
//! TypeScript class in `index.d.ts`, so an object literal does not satisfy the type at
//! compile time, and at runtime napi's argument conversion rejects a non-instance *before*
//! any Rust runs.
//!
//! # What is absent, and why each absence is the requirement
//!
//! * **`valueOf` and `toJSON` on [`EstimatedUsd`].** JS coerces far more eagerly than
//!   Python: with a `valueOf`, `usd * 2`, `+usd`, and `usd > 1` would all silently produce
//!   numbers, and `JSON.stringify` would emit a bare float with `toJSON`. Neither exists,
//!   so `Number(usd)` is `NaN`, `usd * 2` is `NaN`, and `JSON.stringify(usd)` is `{}` — the
//!   figure only comes out through `.amount`, a **string**.
//! * **`Symbol.toPrimitive`.** Also absent, which is what makes the `NaN` above hold rather
//!   than being a default nobody chose.
//! * **An `add` method.** The core implements `Add` only against itself, and there is no JS
//!   type error to lean on, so the honest port is no addition at all. A caller summing
//!   estimates does it on strings they parsed themselves, visibly.
//! * **A [`Duration`] constructor.** `#[napi(constructor)]` is not on it — `Duration.measured(s)`
//!   and `Duration.projected(s)` are factory methods and the only doors, so
//!   `new Duration(3600)` is a `TypeError` from napi rather than an unlabelled duration.
//! * **A `usd` field on an unpriced amount.** [`Amount::usd`] is `null` and
//!   [`CostReport::to_json`] omits the key *entirely*, per `cli.py`: a null is summed as
//!   zero by anything permissive.
//!
//! # Money and quantities are strings, and that is not a serialization detail
//!
//! The rates are figures like `0.0000276944` and the core holds them as exact decimals. A
//! JS `number` is a double: round-tripping through one loses precisely the exactness that
//! makes a report reconcilable against the rate table it came from. Seconds *are* numbers,
//! because a duration really is a measurement a caller does arithmetic on — the same
//! distinction the core draws by having `break_even_seconds_f64` and no dollar equivalent.

use microvms_core::SizeClass as CoreSizeClass;
use microvms_core::cost::{
    self, Amount as CoreAmount, BillingLine, CalendarDate, CostPhase, CostReport as CoreReport,
    DurationP, EstimatedUsd as CoreUsd, LineItem as CoreLineItem, PlanUsage, Provenance,
    RateTable as CoreRates, RunUsage, Total as CoreTotal,
};
use napi::bindgen_prelude::ClassInstance;
use napi_derive::napi;

use crate::errors::js;

// ── Duration (COST-1 / BIND-5) ───────────────────────────────────────────────

/// Seconds, plus how we know them.
///
/// No constructor: `Duration.measured(s)` and `Duration.projected(s)` are the only doors,
/// so a provenance cannot be omitted. See the module docs.
#[napi]
#[derive(Clone, Copy)]
pub struct Duration {
    pub(crate) inner: DurationP,
}

#[napi]
impl Duration {
    /// A timed phase: a clock ran and this is what it read.
    ///
    /// Fallible because the float is — a negative or non-finite figure has no reading as a
    /// duration, and the refusal is the core's, message and all.
    #[napi(factory)]
    pub fn measured(seconds: f64) -> napi::Result<Duration, String> {
        Ok(Duration {
            inner: DurationP::measured_secs_f64(seconds).map_err(js)?,
        })
    }

    /// A hypothetical phase: an estimate's input, or a documented minimum nobody timed.
    #[napi(factory)]
    pub fn projected(seconds: f64) -> napi::Result<Duration, String> {
        Ok(Duration {
            inner: DurationP::projected_secs_f64(seconds).map_err(js)?,
        })
    }

    /// The span in seconds, without its label.
    #[napi(getter)]
    pub fn seconds(&self) -> f64 {
        self.inner.duration().as_secs_f64()
    }

    /// `"measured"` or `"projected"`.
    #[napi(getter)]
    pub fn provenance(&self) -> String {
        self.inner.provenance().as_str().to_string()
    }

    /// True only for a timed span. What `CostReport.fullyMeasured` reads.
    #[napi(getter)]
    pub fn is_measured(&self) -> bool {
        self.inner.is_measured()
    }

    /// `Ns (provenance)` — the label travels with the figure.
    #[napi(js_name = "toString")]
    pub fn display_string(&self) -> String {
        self.inner.to_string()
    }
}

// ── EstimatedUsd (COST-2 / BIND-5) ───────────────────────────────────────────

/// Dollars derived from published rates. Not the bill.
///
/// # There is no way to get a number out of this
///
/// No `valueOf`, no `toJSON`, no `Symbol.toPrimitive`, no `add`, and no constructor. So
/// `Number(usd)` is `NaN`, `usd * 2` is `NaN`, `+usd` is `NaN`, and `JSON.stringify(usd)`
/// answers `{}`. The figure comes out through [`Self::amount`], a **string** — one visible
/// step that keeps the type name at the call site, which is a decision a reviewer can see.
#[napi]
#[derive(Clone, Copy)]
pub struct EstimatedUsd {
    inner: CoreUsd,
}

impl EstimatedUsd {
    pub(crate) fn wrap(inner: CoreUsd) -> Self {
        Self { inner }
    }
}

#[napi]
impl EstimatedUsd {
    /// The figure as an exact decimal **string**.
    ///
    /// A string and not a number: a JS double cannot hold `0.0000276944` summed a few
    /// thousand times without drifting in the direction of a bill nobody can reproduce.
    #[napi(getter)]
    pub fn amount(&self) -> String {
        self.inner.amount().to_string()
    }

    /// The figure at display precision: two places at a dollar or more, six below.
    #[napi(getter)]
    pub fn display_amount(&self) -> String {
        self.inner.amount_string()
    }

    /// `~$X (estimated)`.
    #[napi(js_name = "toString")]
    pub fn display_string(&self) -> String {
        self.inner.to_string()
    }
}

// ── Unpriced and Amount (COST-3) ─────────────────────────────────────────────

/// A quantity we can measure but cannot price, because no rate is published.
///
/// A distinct class from [`EstimatedUsd`], not `EstimatedUsd("0")`: zero is a claim about
/// the bill, unpriced is a claim about the documentation.
#[napi]
#[derive(Clone)]
pub struct Unpriced {
    reason: String,
}

#[napi]
impl Unpriced {
    #[napi(getter)]
    pub fn reason(&self) -> String {
        self.reason.clone()
    }

    #[napi(js_name = "toString")]
    pub fn display_string(&self) -> String {
        format!("unpriced — {}", self.reason)
    }
}

/// What a line item's cost can be: an estimate, or unpriced.
///
/// `kind` is the stable tag, and exactly one of `usd`/`unpriced` is non-null — so a reader
/// who checks either has handled both.
#[napi]
#[derive(Clone)]
pub struct Amount {
    inner: CoreAmount,
}

impl Amount {
    pub(crate) fn wrap(inner: CoreAmount) -> Self {
        Self { inner }
    }
}

#[napi]
impl Amount {
    /// `"estimated-usd"` or `"unpriced"`.
    #[napi(getter)]
    pub fn kind(&self) -> String {
        self.inner.kind().to_string()
    }

    /// The estimate, or `null` when there is no published rate.
    ///
    /// `null` rather than a zero — the one arithmetic this module refuses to enable.
    #[napi(getter)]
    pub fn usd(&self) -> Option<EstimatedUsd> {
        self.inner.estimate().map(EstimatedUsd::wrap)
    }

    /// The unpriced half, or `null` when the line was priced.
    #[napi(getter)]
    pub fn unpriced(&self) -> Option<Unpriced> {
        self.inner.unpriced_reason().map(|reason| Unpriced {
            reason: reason.to_string(),
        })
    }

    #[napi(js_name = "toString")]
    pub fn display_string(&self) -> String {
        self.inner.to_string()
    }
}

// ── LineItem ─────────────────────────────────────────────────────────────────

/// One phase's one billing line: what was consumed, and what that costs.
#[napi]
#[derive(Clone)]
pub struct LineItem {
    inner: CoreLineItem,
}

impl LineItem {
    fn wrap(inner: CoreLineItem) -> Self {
        Self { inner }
    }
}

#[napi]
impl LineItem {
    #[napi(getter)]
    pub fn phase(&self) -> String {
        self.inner.phase.as_str().to_string()
    }

    /// The AWS billing line, or `null` for a phase with no published rate to attribute it
    /// to.
    #[napi(getter)]
    pub fn line(&self) -> Option<String> {
        self.inner.line.map(|line| line.as_str().to_string())
    }

    /// The consumed quantity, exact, as a string.
    ///
    /// Exact so a reader can check the arithmetic against the rate table, which is the only
    /// defence against a rate that drifts without anyone noticing.
    #[napi(getter)]
    pub fn quantity(&self) -> String {
        self.inner.quantity.to_string()
    }

    /// The quantity at display precision, for a column a human scans.
    #[napi(getter)]
    pub fn display_quantity(&self) -> String {
        self.inner.quantity_string()
    }

    #[napi(getter)]
    pub fn unit(&self) -> String {
        self.inner.unit.clone()
    }

    #[napi(getter)]
    pub fn amount(&self) -> Amount {
        Amount::wrap(self.inner.amount.clone())
    }

    #[napi(getter)]
    pub fn duration(&self) -> Option<Duration> {
        self.inner.duration.map(|inner| Duration { inner })
    }

    #[napi(getter)]
    pub fn note(&self) -> String {
        self.inner.note.clone()
    }

    /// The `cli.py` `_line_to_dict` shape as a JSON **string**.
    ///
    /// A string rather than an object because the unpriced case must **omit** the `usd` key
    /// entirely, and a `#[napi(object)]` return type cannot express an absent key — an
    /// `Option` field serializes as `null`, which is the one value that gets summed as zero
    /// by anything permissive. A caller does `JSON.parse`, which is one visible step.
    #[napi]
    pub fn to_json(&self) -> String {
        line_json(&self.inner)
    }

    #[napi(js_name = "toString")]
    pub fn display_string(&self) -> String {
        self.inner.to_string()
    }
}

/// `cli.py:725 _line_to_dict`, as a JSON string.
///
/// Hand-assembled rather than through a serde derive, because the shape's one load-bearing
/// property — the `usd` key being *absent* rather than null for an unpriced line — is
/// exactly what a derive over an `Option` field would get wrong.
///
/// (cli.py line numbers resolve at `git show 'c4d396e^:clients/python/src/microvms_agentd/cli.py'` — the retired oracle.)
fn line_json(item: &CoreLineItem) -> String {
    let amount = match (item.amount.estimate(), item.amount.unpriced_reason()) {
        (Some(usd), _) => format!(
            r#"{{"kind":"estimated-usd","usd":{}}}"#,
            quote(&usd.amount().to_string())
        ),
        // No `usd` key at all.
        (None, Some(reason)) => {
            format!(r#"{{"kind":"unpriced","reason":{}}}"#, quote(reason))
        }
        // Unreachable while `Amount` has two variants, and written rather than
        // `unreachable!()` because a panic across the napi boundary is not an ordinary
        // error — a third variant should render as visibly incomplete, not abort Node.
        (None, None) => format!(r#"{{"kind":{}}}"#, quote(item.amount.kind())),
    };
    let duration = match item.duration {
        Some(duration) => format!(
            r#"{{"seconds":{},"provenance":{}}}"#,
            duration.duration().as_secs_f64(),
            quote(duration.provenance().as_str())
        ),
        None => "null".to_string(),
    };
    format!(
        r#"{{"phase":{},"line":{},"quantity":{},"unit":{},"amount":{amount},"duration":{duration},"note":{}}}"#,
        quote(item.phase.as_str()),
        item.line
            .map_or_else(|| "null".to_string(), |line| quote(line.as_str())),
        quote(&item.quantity.to_string()),
        quote(&item.unit),
        quote(&item.note),
    )
}

/// A JSON string literal, via serde_json — the writer that defines the grammar
/// this string lands in.
fn quote(value: &str) -> String {
    serde_json::to_string(value).expect("a string serializes")
}

// ── Total (COST-4) ───────────────────────────────────────────────────────────

/// A report's total, which is a lower bound whenever anything is unpriced.
///
/// The floor is under a name that says what it is — never `total`. A caller reading a field
/// called `total` would have no reason to check `isLowerBound`.
#[napi]
#[derive(Clone)]
pub struct Total {
    inner: CoreTotal,
}

#[napi]
impl Total {
    /// Everything that could be priced. For a lower bound this is **not** the total.
    #[napi(getter)]
    pub fn floor(&self) -> EstimatedUsd {
        EstimatedUsd::wrap(self.inner.floor())
    }

    /// True when line items with no published rate are missing from the floor.
    #[napi(getter)]
    pub fn is_lower_bound(&self) -> bool {
        self.inner.is_lower_bound()
    }

    /// Why each unpriced line could not be priced, in report order.
    #[napi(getter)]
    pub fn unpriced_reasons(&self) -> Vec<String> {
        self.inner.unpriced_reasons().to_vec()
    }

    /// `at least ~$X (estimated), plus N unpriced (...)`.
    #[napi(js_name = "toString")]
    pub fn display_string(&self) -> String {
        self.inner.to_string()
    }
}

// ── SizeClass ────────────────────────────────────────────────────────────────

/// One of the five documented size classes.
///
/// `minimumMemoryInMiB` selects a class; it does not size a VM. Both numbers are on the
/// class or neither.
#[napi]
#[derive(Clone, Copy)]
pub struct SizeClass {
    pub(crate) inner: CoreSizeClass,
}

#[napi]
impl SizeClass {
    /// The class `minimumMemoryInMiB = mib` selects, or a refusal (TRAP-10).
    ///
    /// Off-table figures are refused rather than snapped to a neighbour: the two plausible
    /// readings differ in both memory and rate, and neither has been measured.
    #[napi(factory)]
    pub fn from_baseline_mib(mib: u32) -> napi::Result<SizeClass, String> {
        Ok(SizeClass {
            inner: CoreSizeClass::from_baseline_mib(mib).map_err(js)?,
        })
    }

    /// The platform's default, 2048 MiB. Not the smallest — a 0.5 GB baseline hands someone
    /// a sandbox that OOM-kills a real test suite, and the guest has no swap.
    #[napi(factory)]
    pub fn default_class() -> SizeClass {
        SizeClass {
            inner: CoreSizeClass::DEFAULT,
        }
    }

    /// Every class, smallest first.
    #[napi]
    pub fn all() -> Vec<SizeClass> {
        CoreSizeClass::ALL
            .into_iter()
            .map(|inner| SizeClass { inner })
            .collect()
    }

    #[napi(getter)]
    pub fn baseline_mib(&self) -> u32 {
        self.inner.baseline_mib()
    }

    #[napi(getter)]
    pub fn baseline_vcpu(&self) -> f64 {
        self.inner.baseline_vcpu()
    }

    #[napi(getter)]
    pub fn peak_mib(&self) -> u32 {
        self.inner.peak_mib()
    }

    #[napi(getter)]
    pub fn peak_vcpu(&self) -> f64 {
        self.inner.peak_vcpu()
    }

    /// The figure a GB-second rate multiplies. Always the baseline, never the peak.
    #[napi(getter)]
    pub fn baseline_gb(&self) -> f64 {
        self.inner.baseline_gb()
    }

    /// The peak in GB, which is what the guest reports as `MemTotal`.
    #[napi(getter)]
    pub fn peak_gb(&self) -> f64 {
        self.inner.peak_gb()
    }

    /// One line naming both numbers.
    #[napi]
    pub fn describe(&self) -> String {
        self.inner.to_string()
    }
}

// ── RateTable ────────────────────────────────────────────────────────────────

/// The pinned rate table, and everything it says about itself.
#[napi]
#[derive(Clone)]
pub struct RateTable {
    pub(crate) inner: CoreRates,
}

#[napi]
impl RateTable {
    /// us-east-1, read 2026-08-07, as recorded in `docs/PLATFORM.md`.
    ///
    /// There is deliberately no constructor taking rates. The core's table has private rate
    /// fields and exactly two doors — this one and `from_catalog`, which refuses a catalog
    /// whose ARM compute line is missing rather than substituting the x86 one, 17.9% higher
    /// (COST-9). A constructor taking five numbers would reopen precisely that.
    #[napi(factory)]
    pub fn pinned() -> RateTable {
        RateTable {
            inner: cost::pinned_rates(),
        }
    }

    #[napi(getter)]
    pub fn region(&self) -> String {
        self.inner.region().to_string()
    }

    #[napi(getter)]
    pub fn source_url(&self) -> String {
        self.inner.source_url().to_string()
    }

    /// ISO 8601, matching the Python's `retrieved.isoformat()`.
    #[napi(getter)]
    pub fn retrieved(&self) -> String {
        self.inner.retrieved().to_string()
    }

    #[napi(getter)]
    pub fn vcpu_second(&self) -> String {
        self.inner.vcpu_second().to_string()
    }

    #[napi(getter)]
    pub fn gb_second(&self) -> String {
        self.inner.gb_second().to_string()
    }

    /// Per GB-month. The one derived figure: the API quotes per GB-hour, and this is that
    /// times 730.
    #[napi(getter)]
    pub fn storage_gb_month(&self) -> String {
        self.inner.storage_gb_month().to_string()
    }

    #[napi(getter)]
    pub fn snapshot_read_gb(&self) -> String {
        self.inner.snapshot_read_gb().to_string()
    }

    #[napi(getter)]
    pub fn snapshot_write_gb(&self) -> String {
        self.inner.snapshot_write_gb().to_string()
    }

    /// Snapshot storage bills at least this long however briefly the snapshot exists.
    #[napi(getter)]
    pub fn minimum_retention_seconds(&self) -> f64 {
        self.inner.minimum_retention().as_secs_f64()
    }

    /// Zero: MicroVMs bills per second with no per-request charge.
    #[napi(getter)]
    pub fn per_request(&self) -> String {
        self.inner.per_request().to_string()
    }

    /// True: vCPU and memory are two line items, as the pricing page prices them.
    #[napi(getter)]
    pub fn bills_vcpu_and_memory_separately(&self) -> bool {
        self.inner.bills_vcpu_and_memory_separately()
    }

    /// False: no published MicroVMs free tier. The Lambda one is Functions-only.
    #[napi(getter)]
    pub fn free_tier(&self) -> bool {
        self.inner.free_tier()
    }

    /// `null` means **not published**, not one second. Nothing rounds a duration up, because
    /// inventing an increment would overcharge every short exec.
    #[napi(getter)]
    pub fn minimum_billing_increment_sec(&self) -> Option<f64> {
        self.inner
            .minimum_billing_increment()
            .map(|increment| increment.as_secs_f64())
    }

    /// How many days ago these rates were read, against today UTC.
    #[napi]
    pub fn age_days(&self) -> i64 {
        self.inner.age_days(CalendarDate::today_utc())
    }

    /// The staleness warning, or `null` when the table is fresh.
    #[napi]
    pub fn staleness(&self) -> Option<String> {
        self.inner.staleness(CalendarDate::today_utc())
    }
}

// ── CostReport ───────────────────────────────────────────────────────────────

/// Per-phase attribution for one sandbox, measured or projected.
#[napi]
#[derive(Clone)]
pub struct CostReport {
    inner: CoreReport,
}

impl CostReport {
    fn wrap(inner: CoreReport) -> Self {
        Self { inner }
    }
}

#[napi]
impl CostReport {
    #[napi(getter)]
    pub fn label(&self) -> String {
        self.inner.label().to_string()
    }

    #[napi(getter)]
    pub fn size(&self) -> SizeClass {
        SizeClass {
            inner: self.inner.size(),
        }
    }

    #[napi(getter)]
    pub fn rates(&self) -> RateTable {
        RateTable {
            inner: self.inner.rates().clone(),
        }
    }

    #[napi(getter)]
    pub fn items(&self) -> Vec<LineItem> {
        self.inner
            .items()
            .iter()
            .cloned()
            .map(LineItem::wrap)
            .collect()
    }

    /// The line items with a published rate.
    #[napi(getter)]
    pub fn priced(&self) -> Vec<LineItem> {
        self.inner.priced().cloned().map(LineItem::wrap).collect()
    }

    /// The line items with no published rate.
    #[napi(getter)]
    pub fn unpriced(&self) -> Vec<LineItem> {
        self.inner.unpriced().cloned().map(LineItem::wrap).collect()
    }

    /// The total, which is a lower bound whenever anything is unpriced.
    #[napi(getter)]
    pub fn total(&self) -> Total {
        Total {
            inner: self.inner.total(),
        }
    }

    /// False whenever any phase has no published rate.
    #[napi(getter)]
    pub fn complete(&self) -> bool {
        self.inner.is_complete()
    }

    /// True only if every duration was timed. An estimate is never this.
    #[napi(getter)]
    pub fn fully_measured(&self) -> bool {
        self.inner.fully_measured()
    }

    /// The staleness warning the table carried when this was computed.
    #[napi(getter)]
    pub fn staleness(&self) -> Option<String> {
        self.inner.staleness().map(str::to_string)
    }

    /// The line items belonging to one phase.
    ///
    /// The string is judged by the core's own [`CostPhase::from_str`], which is where the
    /// closed set lives. This file used to carry its own seven-element table for it, as did
    /// `microvms-py/src/cost.rs` — two parallel lists over one enum, which would have gone
    /// stale the first time a phase was added and would have disagreed with each other in
    /// whichever direction was edited first.
    #[napi]
    pub fn by_phase(&self, phase: String) -> napi::Result<Vec<LineItem>, String> {
        let phase: CostPhase = phase.parse().map_err(js)?;
        Ok(self
            .inner
            .by_phase(phase)
            .cloned()
            .map(LineItem::wrap)
            .collect())
    }

    /// Plain text, leading with what the dollars are rather than with the dollars.
    #[napi]
    pub fn render(&self) -> String {
        self.inner.render()
    }

    /// The `cli.py:688 report_to_dict` shape as a JSON **string**.
    ///
    /// A string for the same reason [`LineItem::to_json`] is: the unpriced line item omits
    /// its `usd` key, which no typed return shape can express.
    #[napi]
    pub fn to_json(&self) -> String {
        let size = self.inner.size();
        let rates = self.inner.rates();
        let total = self.inner.total();
        let items = self
            .inner
            .items()
            .iter()
            .map(line_json)
            .collect::<Vec<_>>()
            .join(",");
        // One `format!` per fragment rather than one `concat!`ed template: `format_args!`
        // cannot capture a named variable when the template comes from a macro expansion,
        // and the alternative — positional `{}` for all sixteen values — is a template
        // nobody can check against its arguments by eye.
        let size_json = format!(
            r#"{{"baselineMib":{},"baselineVcpu":{},"peakMib":{},"peakVcpu":{},"describe":{}}}"#,
            size.baseline_mib(),
            size.baseline_vcpu(),
            size.peak_mib(),
            size.peak_vcpu(),
            quote(&size.to_string()),
        );
        let rates_json = format!(
            r#"{{"region":{},"retrieved":{},"sourceUrl":{}}}"#,
            quote(&rates.region().to_string()),
            quote(&rates.retrieved().to_string()),
            quote(rates.source_url()),
        );
        let total_json = format!(
            r#"{{"priced":{},"isLowerBound":{},"render":{}}}"#,
            quote(&total.floor().amount().to_string()),
            total.is_lower_bound(),
            quote(&total.to_string()),
        );
        let label = quote(self.inner.label());
        let fully_measured = self.inner.fully_measured();
        let complete = self.inner.is_complete();
        let staleness = self
            .inner
            .staleness()
            .map_or_else(|| "null".to_string(), quote);
        // `estimated` is a literal `true` and not the word "cost": these are estimates
        // derived from published rates, and the field name is the only place that
        // distinction survives a copy-paste.
        format!(r#"{{"label":{label},"size":{size_json},"rates":{rates_json},"estimated":true,"#)
            + &format!(
                r#""fullyMeasured":{fully_measured},"complete":{complete},"staleness":{staleness},"#
            )
            + &format!(r#""items":[{items}],"total":{total_json}}}"#)
    }
}

/// Running versus suspended for the same VM over the same wall time.
#[napi]
pub struct ResidencyComparison {
    inner: cost::ResidencyComparison,
}

#[napi]
impl ResidencyComparison {
    #[napi(getter)]
    pub fn size(&self) -> SizeClass {
        SizeClass {
            inner: self.inner.size(),
        }
    }

    /// The wall time both sides cover. Always projected: a comparison is a hypothetical
    /// about a hold nobody has taken yet.
    #[napi(getter)]
    pub fn hold(&self) -> Duration {
        Duration {
            inner: self.inner.hold(),
        }
    }

    #[napi(getter)]
    pub fn cycles(&self) -> u32 {
        self.inner.cycles()
    }

    #[napi(getter)]
    pub fn running(&self) -> CostReport {
        CostReport::wrap(self.inner.running().clone())
    }

    #[napi(getter)]
    pub fn suspended(&self) -> CostReport {
        CostReport::wrap(self.inner.suspended().clone())
    }

    /// How many times more the running VM costs, as an exact decimal string.
    #[napi(getter)]
    pub fn ratio(&self) -> String {
        self.inner.ratio().to_string()
    }

    /// One suspend/resume: a snapshot write plus a read, per GB.
    ///
    /// Without it the honest conclusion inverts — "suspend constantly" reads as free.
    #[napi]
    pub fn per_cycle(&self) -> napi::Result<EstimatedUsd, String> {
        Ok(EstimatedUsd::wrap(self.inner.per_cycle().map_err(js)?))
    }

    /// How long a VM must stay suspended for the cycle to pay for itself, exact, as a
    /// string. The number a pool scheduler needs, and the one a "100x cheaper" headline
    /// hides.
    #[napi]
    pub fn break_even_seconds(&self) -> napi::Result<String, String> {
        Ok(self.inner.break_even_seconds().map_err(js)?.to_string())
    }

    /// The break-even hold as a number. **Lossy, and named so.**
    ///
    /// Seconds, not dollars: no money figure has a numeric accessor anywhere in this file.
    #[napi]
    pub fn break_even_seconds_number(&self) -> napi::Result<f64, String> {
        self.inner.break_even_seconds_f64().map_err(js)
    }

    #[napi]
    pub fn render(&self) -> napi::Result<String, String> {
        self.inner.render().map_err(js)
    }
}

// ── the report builders ──────────────────────────────────────────────────────

/// Everything a measured report attributes cost to.
///
/// `#[napi(object)]` **is** right here, unlike for the guarded types: this is an options bag
/// a caller writes as a literal, and every field that carries a closure is a
/// `ClassInstance<Duration>` rather than a number — so the object shape still cannot express
/// an unlabelled duration. `ClassInstance` is what makes that hold: it extracts only from a
/// real `Duration` instance, so `{ running: 3600 }` and `{ running: { seconds: 3600 } }` are
/// both rejected by napi's conversion before any Rust runs. What the object buys is optional
/// fields with JS's own `undefined`, which is how "this phase did not happen" is said.
#[derive(Default)]
#[napi(object)]
pub struct RunUsageOptions<'a> {
    /// Wall-clock time in RUNNING. Bills at baseline whether or not anything is executing —
    /// there is no free I/O wait, which is why suspension rather than idleness is the lever.
    pub running: Option<ClassInstance<'a, Duration>>,
    /// Time held suspended. Pays storage only: a suspended VM is frozen.
    pub suspended: Option<ClassInstance<'a, Duration>>,
    /// How long the image build took, if it was timed. Always unpriced.
    pub image_build: Option<ClassInstance<'a, Duration>>,
    /// The image's size. Passing this adds both an image-storage line *and* the unpriced
    /// build line, so a create-and-destroy report is never complete.
    pub image_gb: Option<f64>,
    /// How long the image was retained. Defaults to the documented one-week minimum, marked
    /// projected — nobody timed that week either.
    pub image_retained: Option<ClassInstance<'a, Duration>>,
    /// Each cycle pays a snapshot write plus a read.
    pub suspend_resume_cycles: Option<u32>,
    /// The suspend snapshot's size. Defaults to the baseline memory footprint.
    pub snapshot_gb: Option<f64>,
    /// Whether a launch happened. A launch reads a snapshot.
    pub launched: Option<bool>,
    pub label: Option<String>,
}

/// Per-phase attribution for one sandbox's lifecycle.
///
/// Every duration is a [`Duration`] class and not a number, which is what keeps the
/// provenance label attached: taking seconds here would need this function to pick a
/// provenance, and the one it would pick is the stronger claim.
#[napi]
pub fn run_report(
    size: &SizeClass,
    options: Option<RunUsageOptions<'_>>,
    rates: Option<&RateTable>,
) -> napi::Result<CostReport, String> {
    let options = options.unwrap_or_default();
    let usage = RunUsage {
        running: options.running.map(|duration| duration.inner),
        suspended: options.suspended.map(|duration| duration.inner),
        image_build: options.image_build.map(|duration| duration.inner),
        image_gb: options.image_gb,
        image_retained: options.image_retained.map(|duration| duration.inner),
        suspend_resume_cycles: options.suspend_resume_cycles.unwrap_or(0),
        snapshot_gb: options.snapshot_gb,
        launched: options.launched.unwrap_or(true),
    };
    let table = rates.map_or_else(cost::pinned_rates, |rates| rates.inner.clone());
    Ok(CostReport::wrap(
        cost::run_report(
            size.inner,
            &usage,
            &table,
            CalendarDate::today_utc(),
            options.label.unwrap_or_else(|| "run".to_string()),
        )
        .map_err(js)?,
    ))
}

/// A plan's phases, in plain seconds, before anything is spent.
///
/// Separate from [`RunUsageOptions`] on purpose (COST-10): every field is a number, so there
/// is no field an accidentally-measured duration could be written into. The wrapping into
/// projected durations happens in the core, in one place.
#[derive(Default)]
#[napi(object)]
pub struct PlanUsageOptions {
    pub running_seconds: Option<f64>,
    pub suspended_seconds: Option<f64>,
    pub image_gb: Option<f64>,
    pub image_retained_seconds: Option<f64>,
    pub suspend_resume_cycles: Option<u32>,
    pub snapshot_gb: Option<f64>,
    pub launched: Option<bool>,
    pub label: Option<String>,
}

/// What a plan will cost, before spending anything (COST-10).
///
/// Takes plain seconds and marks every one projected. That is the difference from
/// [`run_report`]: not the arithmetic, which is shared, but what the durations admit about
/// themselves.
#[napi]
pub fn estimate_run(
    size: &SizeClass,
    options: Option<PlanUsageOptions>,
    rates: Option<&RateTable>,
) -> napi::Result<CostReport, String> {
    let options = options.unwrap_or_default();
    let plan = PlanUsage {
        running_seconds: options.running_seconds.unwrap_or(0.0),
        suspended_seconds: options.suspended_seconds.unwrap_or(0.0),
        image_gb: options.image_gb,
        image_retained_seconds: options.image_retained_seconds,
        suspend_resume_cycles: options.suspend_resume_cycles.unwrap_or(0),
        snapshot_gb: options.snapshot_gb,
        launched: options.launched.unwrap_or(true),
    };
    let table = rates.map_or_else(cost::pinned_rates, |rates| rates.inner.clone());
    Ok(CostReport::wrap(
        cost::estimate_run(
            size.inner,
            &plan,
            &table,
            CalendarDate::today_utc(),
            options.label.unwrap_or_else(|| "plan".to_string()),
        )
        .map_err(js)?,
    ))
}

/// The warm-pool argument, with its own counter-argument attached.
#[napi]
pub fn compare_residency(
    size: &SizeClass,
    hold_seconds: f64,
    cycles: Option<u32>,
    rates: Option<&RateTable>,
) -> napi::Result<ResidencyComparison, String> {
    let hold = cost::duration_of_secs_f64(hold_seconds).map_err(js)?;
    let table = rates.map_or_else(cost::pinned_rates, |rates| rates.inner.clone());
    Ok(ResidencyComparison {
        inner: cost::compare_residency(
            size.inner,
            hold,
            cycles.unwrap_or(1),
            &table,
            CalendarDate::today_utc(),
        )
        .map_err(js)?,
    })
}

/// Why the image build has no price, as the reason that lands on the line item.
#[napi]
pub fn build_unpriced_reason() -> String {
    cost::BUILD_UNPRICED_REASON.to_string()
}

/// The documented cost constants, as a JSON string a caller can assert against.
///
/// A JSON string rather than a typed object because it is a transcription of published
/// facts whose shape may grow, and a `#[napi(object)]` would make every addition a
/// signature change.
#[napi]
pub fn cost_constants() -> String {
    let provenances = [Provenance::Measured, Provenance::Projected]
        .iter()
        .map(|provenance| quote(provenance.as_str()))
        .collect::<Vec<_>>()
        .join(",");
    let phases = CostPhase::ALL
        .iter()
        .map(|phase| quote(phase.as_str()))
        .collect::<Vec<_>>()
        .join(",");
    let lines = [
        BillingLine::Vcpu,
        BillingLine::Memory,
        BillingLine::SnapshotStorage,
        BillingLine::SnapshotRead,
        BillingLine::SnapshotWrite,
    ]
    .iter()
    .map(|line| quote(line.as_str()))
    .collect::<Vec<_>>()
    .join(",");
    let seconds_per_month = quote(&cost::SECONDS_PER_MONTH.to_string());
    let hours_per_month = quote(&cost::HOURS_PER_MONTH.to_string());
    let stale_after = cost::STALE_AFTER_DAYS;
    let retention = cost::MINIMUM_RETENTION.as_secs_f64();
    format!(r#"{{"secondsPerMonth":{seconds_per_month},"hoursPerMonth":{hours_per_month},"#)
        + &format!(r#""staleAfterDays":{stale_after},"minimumRetentionSeconds":{retention},"#)
        + &format!(
            r#""provenances":[{provenances}],"phases":[{phases}],"billingLines":[{lines}]}}"#
        )
}
