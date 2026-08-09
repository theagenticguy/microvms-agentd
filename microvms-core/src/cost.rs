// SPDX-License-Identifier: Apache-2.0
//! What a run cost, and what a plan will cost, with every figure labelled.
//!
//! MicroVMs has no standalone pricing page. The rates appear only inside worked
//! examples on the Lambda pricing page, which is why this module exists as data
//! rather than as a paragraph someone re-derives: a consumer building a sandbox
//! product should be able to ask "what did that run cost" without reading a pricing
//! page sideways.
//!
//! Two rules hold everywhere below. The Python client's `cost.py` enforced both at
//! runtime — a `TypeError` from a keyword-only field, a `NotImplemented` from
//! `__add__`. Here they are the shape of the types, so the enforcement happens before
//! the program runs.
//!
//! **Seconds are measured, dollars are estimated.** [`DurationP`] is an enum whose
//! every variant names its provenance, so there is no unlabelled constructor to
//! call — not one that raises, one that does not exist (COST-1). And there is no
//! type here for an actual dollar amount, only [`EstimatedUsd`], which says so on
//! every render. Only Cost Explorer knows the bill. That asymmetry is the point: we
//! can time a suspension to the millisecond and still only infer its price.
//!
//! **Unknown is not zero.** [`Amount::Unpriced`] carries a reason and is a distinct
//! variant, so a consumer matching on an amount has to handle it (COST-3). AWS does
//! not publish whether the server-side image build is billed as compute, so a report
//! that summed it as $0.00 would be lying in the direction that flatters us. A
//! [`Total`] over any unpriced line is a *different variant*, [`Total::AtLeast`],
//! which cannot be read without seeing the reasons beside the floor (COST-4).
//!
//! # What this port strengthens
//!
//! Three closures move up the strength ladder from the Python original.
//!
//! * **No negative duration.** `cost.py` validated `seconds >= 0` in
//!   `__post_init__`, because an inverted clock renders as a credit on the report.
//!   [`DurationP`] wraps [`std::time::Duration`], which cannot be negative, so the
//!   check is gone rather than moved: the only place a sign can go wrong is
//!   [`seconds_of`], the one f64 boundary, and it is fallible.
//! * **No coercion to float.** `cost.py` omitted `__float__` and relied on the
//!   reader not writing `float(x.amount)`. [`EstimatedUsd`] has a private field, no
//!   `From`/`Into<f64>`, no `Deref`, and no `Serialize` that emits a number — the
//!   coercion is not a discouraged spelling, it is absent (COST-2).
//! * **No x86 rate, by construction.** `pricing.py` refused to substitute the x86
//!   compute rate inside its fetch path, which meant a hand-built `RateTable` could
//!   still carry one. Here [`RateTable`]'s rate fields are private and there are
//!   exactly two ways to obtain one — the pinned [`pinned_rates`], and
//!   [`RateTable::from_catalog`], which rejects a catalog whose ARM line is missing
//!   (COST-9). No value of the type exists that was built from an x86 figure.
//!
//! # The table carries its own staleness
//!
//! [`pinned_rates`] is pinned to a retrieval date, and a table older than
//! [`STALE_AFTER_DAYS`] attaches a warning to every report computed from it
//! (COST-7), because a silently stale price is the same failure class as a silently
//! stale schema. The warning is a fallback rather than the defence — it can only say
//! that nobody has looked. [`RateCatalog`] is what looks.
//!
//! Every rate here is recorded in `docs/PLATFORM.md`, "What actually costs money".
//! One of them, [`RateTable::storage_gb_month`], is derived: the API quotes snapshot
//! storage per GB-hour and this table holds per GB-month, so it is the hourly figure
//! times [`HOURS_PER_MONTH`]. Billing reads [`SizeClass::baseline_gb`] and never the
//! peak (COST-5).

use std::fmt;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rust_decimal::Decimal;
use rust_decimal::prelude::{FromStr, ToPrimitive};
use rust_decimal_macros::dec;

use crate::error::Error;
use crate::region::Region;
use crate::sizing::SizeClass;

/// AWS bills storage per GB-month and defines a month as 730 hours, so a
/// partial-month hold is that fraction of the monthly rate.
///
/// Spelled out because 30-day and calendar-month conventions both give
/// plausible-looking answers that disagree with the worked examples in
/// `docs/PLATFORM.md` by a few percent. `730 * 3600`.
pub const SECONDS_PER_MONTH: Decimal = dec!(2628000);

/// Hours in the month AWS's GB-month rate assumes, derived from
/// [`SECONDS_PER_MONTH`] rather than written again.
///
/// The second 730 is where the drift got in: the pinned storage rate read `0.08`
/// until 2026-08-07, a round number that was 1.37% low, because two places in the
/// code held two conventions for the same month.
pub const HOURS_PER_MONTH: Decimal = dec!(730);

/// How old a rate table may get before it warns (COST-7).
///
/// Ninety days is the same order as the interval at which AWS has historically
/// restructured Lambda pricing, and the cost of the warning when nothing changed is
/// one line of output.
pub const STALE_AFTER_DAYS: i64 = 90;

/// Seconds in a day, for the one place a [`Duration`] is quoted in days.
///
/// Not a billing convention — AWS's month is [`SECONDS_PER_MONTH`] and nothing here
/// derives a month from this. It exists so [`RateTable::minimum_retention_days`] reads
/// its own unit off a name rather than off a literal beside a message.
const SECONDS_PER_DAY: u64 = 24 * 60 * 60;

/// Snapshot storage bills at least this long however briefly the snapshot exists
/// (COST-8).
///
/// One week. Why a create-and-destroy suite's floor is its image rather than its
/// compute: a 2 GB image deleted after sixty seconds still bills about four cents.
pub const MINIMUM_RETENTION: Duration = Duration::from_secs(7 * SECONDS_PER_DAY);

// ── the two float boundaries (COST-6) ────────────────────────────────────────

/// Seconds as an exact [`Decimal`], from the one representation that cannot be
/// negative.
///
/// **This is a conversion site, and it is exact.** A [`Duration`] is a whole
/// seconds count plus a nanosecond remainder, both integers, so this is integer
/// arithmetic in decimal rather than a float being reinterpreted — the nanosecond
/// division is by a power of ten and loses nothing.
pub fn seconds_of(duration: Duration) -> Decimal {
    // Nanoseconds are < 1e9 and Decimal holds 28 significant digits, so neither
    // term can overflow and the sum is exact.
    Decimal::from(duration.as_secs()) + Decimal::from(duration.subsec_nanos()) / dec!(1000000000)
}

/// A float from a caller, converted into the money path exactly once (COST-6).
///
/// **This is the only place an f64 becomes a [`Decimal`].** It goes through the
/// float's decimal string rather than its binary value, because
/// `Decimal::try_from(0.1f64)` carries the binary error into every downstream
/// figure. Fallible rather than lossy: `NaN`, an infinity, and a magnitude past
/// 28 digits have no decimal reading, and a money figure derived from one of them
/// would be a number nobody could reconcile.
///
/// Named for gigabytes because that is the one quantity a caller supplies as a
/// float — every duration arrives as a [`Duration`], which [`seconds_of`] converts
/// exactly.
pub fn gb_decimal(gb: f64) -> Result<Decimal, Error> {
    if !gb.is_finite() || gb < 0.0 {
        return Err(Error::invalid_arg(format!(
            "{gb} GB is not a quantity that can be priced: a size must be finite and \
             non-negative, and a negative one would render as a credit on the report"
        )));
    }
    Decimal::from_str(&gb.to_string()).map_err(|source| {
        Error::invalid_arg(format!(
            "{gb} GB cannot be represented exactly in decimal, and money arithmetic here is \
             decimal end to end so that a figure can be reconciled against the rate table"
        ))
        .with_source(source)
    })
}

/// Seconds from a float, refused when the float cannot be a duration.
///
/// The complement of [`gb_decimal`] for the API boundary where seconds arrive as a
/// number — a CLI flag, a JSON field. `Duration::try_from_secs_f64` is what rejects
/// a negative or non-finite figure, which is why [`DurationP`] itself needs no
/// validation: an inverted clock is refused here or it never becomes a duration.
pub fn duration_of_secs_f64(seconds: f64) -> Result<Duration, Error> {
    Duration::try_from_secs_f64(seconds).map_err(|source| {
        Error::invalid_arg(format!(
            "{seconds}s is not a duration: it must be finite and non-negative, and a negative \
             one would silently produce a credit on the report"
        ))
        .with_source(source)
    })
}

// ── display precision ────────────────────────────────────────────────────────

/// Enough places to see a sub-cent figure, few enough to read a monthly one.
///
/// A create-and-destroy test suite's compute cost lives in the sixth decimal place
/// while a warm pool's monthly bill lives in the second, and one fixed precision
/// cannot show both without either lying or shouting. Half-even at the boundary,
/// matching Python's `Decimal.quantize` default, so the two clients render the same
/// figure the same way.
fn render_amount(amount: Decimal) -> String {
    let places = if amount.abs() >= Decimal::ONE { 2 } else { 6 };
    let mut rendered = amount.round_dp(places);
    // Fixed places rather than significant digits: a trailing zero is what says the
    // figure was rounded to the cent rather than happening to land there.
    rendered.rescale(places);
    rendered.to_string()
}

/// A consumed quantity, rounded only for display.
///
/// Division by 730 hours yields 28 significant digits, and a line item nobody can
/// scan is a line item nobody checks the rate table against — which is the one job
/// the quantity column has.
fn render_quantity(quantity: Decimal) -> String {
    quantity.round_dp(6).normalize().to_string()
}

// ── dates, without a date crate ──────────────────────────────────────────────

/// A calendar day, which is the only temporal resolution a rate table has.
///
/// Deliberately not a dependency. `cargo tree` shows `time 0.3` in this workspace,
/// but only as a transitive dependency of `aws-smithy-runtime` — taking it here
/// would mean adding a direct dependency, and the module needs exactly two
/// operations: pin a retrieval date, and subtract two dates to get an age in days.
/// No parsing, no formatting, no zones, no arithmetic on months. So the whole date
/// surface is the proleptic-Gregorian day number below, which is twelve lines and
/// has a pinned test against three known values.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct CalendarDate {
    year: i32,
    month: u32,
    day: u32,
}

impl CalendarDate {
    /// A date from its parts, for the literals in this file.
    ///
    /// `const` so [`pinned_rates`] can pin its retrieval date, which is also why it does
    /// not validate: a `const fn` cannot return a `Result` a caller must handle at
    /// compile time. The literals it is used on are pinned by
    /// `the_pinned_dates_are_real_calendar_days`; anything arriving at runtime goes
    /// through [`CalendarDate::try_from_ymd`].
    pub const fn from_ymd(year: i32, month: u32, day: u32) -> CalendarDate {
        CalendarDate { year, month, day }
    }

    /// A date from parts that came from outside this crate.
    ///
    /// The S2 boundary: a `--today` flag or a JSON field is three integers, and
    /// `2026-02-30` would otherwise produce a day number for March 2nd and an age
    /// two days out.
    pub fn try_from_ymd(year: i32, month: u32, day: u32) -> Result<CalendarDate, Error> {
        let candidate = CalendarDate { year, month, day };
        if !(1..=12).contains(&month) || day == 0 || day > days_in_month(year, month) {
            return Err(Error::invalid_arg(format!(
                "{year:04}-{month:02}-{day:02} is not a calendar day, so it cannot date a rate \
                 table: month must be 1-12 and day must be within that month"
            )));
        }
        Ok(candidate)
    }

    /// Today, UTC.
    ///
    /// UTC rather than local, and no dependency: a rate table's age is measured in
    /// ninety-day units, so the zone can only ever move the answer by a day and
    /// carrying a timezone database to decide it would be absurd. Falls back to the
    /// epoch if the clock is set before 1970, which is a machine whose age
    /// arithmetic is already meaningless and is not worth a `Result` on every
    /// report.
    pub fn today_utc() -> CalendarDate {
        let seconds = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |since| since.as_secs());
        // Integer division floors, and the epoch is midnight, so this is the UTC
        // civil day without any rounding question.
        Self::from_day_number((seconds / 86_400) as i64)
    }

    pub fn year(self) -> i32 {
        self.year
    }

    pub fn month(self) -> u32 {
        self.month
    }

    pub fn day(self) -> u32 {
        self.day
    }

    /// Days since 1970-01-01, negative before it.
    ///
    /// Howard Hinnant's `days_from_civil`, which is the algorithm every date library
    /// uses: it shifts the year to start in March so the leap day lands at the end
    /// of a year and the month-length pattern becomes the linear `(153m + 2) / 5`.
    /// Correct for every proleptic-Gregorian date, and pinned below against
    /// 1970-01-01, a leap day, and this table's own retrieval date.
    pub fn day_number(self) -> i64 {
        let month = i64::from(self.month);
        let day = i64::from(self.day);
        // March-based year, so February's variable length is the last month.
        let year = i64::from(self.year) - i64::from(month <= 2);
        let era = if year >= 0 { year } else { year - 399 } / 400;
        let year_of_era = year - era * 400;
        let shifted = if month > 2 { month - 3 } else { month + 9 };
        let day_of_year = (153 * shifted + 2) / 5 + day - 1;
        let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
        // 719468 is the day number of 1970-01-01 in the era-based count.
        era * 146_097 + day_of_era - 719_468
    }

    /// The inverse of [`CalendarDate::day_number`], for [`CalendarDate::today_utc`].
    fn from_day_number(day_number: i64) -> CalendarDate {
        let shifted = day_number + 719_468;
        let era = if shifted >= 0 {
            shifted
        } else {
            shifted - 146_096
        } / 146_097;
        let day_of_era = shifted - era * 146_097;
        let year_of_era =
            (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
        let year = year_of_era + era * 400;
        let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
        let month_shifted = (5 * day_of_year + 2) / 153;
        let day = day_of_year - (153 * month_shifted + 2) / 5 + 1;
        let month = if month_shifted < 10 {
            month_shifted + 3
        } else {
            month_shifted - 9
        };
        CalendarDate {
            year: (year + i64::from(month <= 2)) as i32,
            month: month as u32,
            day: day as u32,
        }
    }

    /// Days from `earlier` to `self`, negative when `self` is the earlier one.
    pub fn days_since(self, earlier: CalendarDate) -> i64 {
        self.day_number() - earlier.day_number()
    }
}

/// ISO 8601, because that is what the Python client's `retrieved.isoformat()` puts
/// in the report header and the two have to be diffable.
impl fmt::Display for CalendarDate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:04}-{:02}-{:02}", self.year, self.month, self.day)
    }
}

/// Days in a month, for [`CalendarDate::try_from_ymd`]'s validation only.
fn days_in_month(year: i32, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if year % 4 == 0 && (year % 100 != 0 || year % 400 == 0) => 29,
        2 => 28,
        _ => 0,
    }
}

// ── provenance (COST-1) ──────────────────────────────────────────────────────

/// Where a duration came from. There is no third option and no default.
///
/// `Measured` means a clock ran. `Projected` means a caller supplied a
/// hypothetical, which is also what an estimate's durations are and what the
/// documented one-week storage minimum is — nobody timed that week either.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum Provenance {
    Measured,
    Projected,
}

impl Provenance {
    /// The wire spelling, identical to the Python `StrEnum` member.
    pub fn as_str(self) -> &'static str {
        match self {
            Provenance::Measured => "measured",
            Provenance::Projected => "projected",
        }
    }
}

impl fmt::Display for Provenance {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Seconds, plus how we know them (COST-1).
///
/// The labelling rule is the enum: every way of building one of these names a
/// provenance, because there is no other variant to reach and no
/// `From<Duration>`/`Default` to fall into. `cost.py` achieved the same thing with
/// a field that had no default, so `Duration(3600)` raised `TypeError` — the whole
/// rule collapses the moment an unlabelled duration can be written, because the
/// label that goes missing is always the weaker one, and here it cannot be written.
///
/// Wrapping [`std::time::Duration`] also retires the Python's negativity check: a
/// negative duration is not a value this type can hold, so an inverted clock is
/// caught at [`duration_of_secs_f64`] or never enters at all.
///
/// # Provenance cannot be omitted (COST-1)
///
/// "No unlabelled constructor" is a claim about what does not exist, so it is checked
/// by programs that do not build:
///
/// Each carries its expected error code, because a bare `compile_fail` passes for any
/// build failure at all — a typo in the test would leave the guard green.
///
/// ```compile_fail,E0277
/// # use microvms_core::cost::DurationP;
/// # use std::time::Duration;
/// // There is no `From<Duration> for DurationP`, so a bare span cannot become a
/// // labelled one by coercion.
/// let unlabelled: DurationP = Duration::from_secs(3600).into();
/// ```
///
/// ```compile_fail,E0599
/// # use microvms_core::cost::DurationP;
/// // No `Default` either. A default would have to pick a provenance, and the one it
/// // would pick is the stronger claim.
/// let unlabelled = DurationP::default();
/// ```
///
/// The positive case, for contrast — naming the provenance is the only way in, and it
/// is one word:
///
/// ```
/// # use microvms_core::cost::{DurationP, Provenance};
/// # use std::time::Duration;
/// let timed = DurationP::Measured(Duration::from_secs(3600));
/// assert_eq!(timed.provenance(), Provenance::Measured);
/// ```
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum DurationP {
    /// A clock ran and this is what it read.
    Measured(Duration),
    /// A hypothetical: an estimate's input, or a documented minimum nobody timed.
    Projected(Duration),
}

impl DurationP {
    /// A timed phase, from seconds a caller measured as a float.
    ///
    /// Fallible because the float is: see [`duration_of_secs_f64`].
    pub fn measured_secs_f64(seconds: f64) -> Result<DurationP, Error> {
        Ok(DurationP::Measured(duration_of_secs_f64(seconds)?))
    }

    /// A hypothetical phase, from seconds a caller supplied as a float.
    pub fn projected_secs_f64(seconds: f64) -> Result<DurationP, Error> {
        Ok(DurationP::Projected(duration_of_secs_f64(seconds)?))
    }

    /// The span, without its label. Named so a call site that drops the provenance
    /// is visible as one.
    pub fn duration(self) -> Duration {
        match self {
            DurationP::Measured(duration) | DurationP::Projected(duration) => duration,
        }
    }

    /// How this duration is known.
    pub fn provenance(self) -> Provenance {
        match self {
            DurationP::Measured(_) => Provenance::Measured,
            DurationP::Projected(_) => Provenance::Projected,
        }
    }

    /// The span as exact decimal seconds, for the money path.
    pub fn seconds(self) -> Decimal {
        seconds_of(self.duration())
    }

    /// The span as an f64, for a JSON envelope.
    ///
    /// **Lossy, and named so**, exactly like
    /// [`ResidencyComparison::break_even_seconds_f64`]. The exact answer is
    /// [`DurationP::seconds`]; this exists because `cli.py:743` emits a line item's
    /// `duration.seconds` as a JSON *number* and the two clients' envelopes have to be
    /// substitutable. Seconds, not dollars — no money figure has an f64 accessor
    /// (COST-2), and that asymmetry is the whole rule: a consumer doing arithmetic on
    /// a duration should not have to branch on which client produced the envelope,
    /// while a consumer doing arithmetic on a dollar figure should be reading the
    /// string and told so.
    ///
    /// (cli.py line numbers resolve at `git show 'c4d396e^:clients/python/src/microvms_agentd/cli.py'` — the retired oracle.)
    pub fn seconds_f64(self) -> f64 {
        self.duration().as_secs_f64()
    }

    /// True only for a timed span. Read by [`CostReport::fully_measured`].
    pub fn is_measured(self) -> bool {
        matches!(self, DurationP::Measured(_))
    }
}

/// The Python's `f"{seconds:g}s ({provenance})"`, so a report line reads the same in
/// either client.
impl fmt::Display for DurationP {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}s ({})", self.seconds().normalize(), self.provenance())
    }
}

// ── dollars, which are always estimates (COST-2) ─────────────────────────────

/// Dollars derived from published rates. Not the bill.
///
/// There is deliberately no `ActualUsd` alongside this: a client that never sees an
/// invoice cannot produce an actual. And there is deliberately **no way to get an
/// f64 out** (COST-2) — no `From<EstimatedUsd> for f64`, no `Into`, no `Deref`, no
/// `as_f64`. The Python original could only omit `__float__` and hope; the cheapest
/// way to launder an estimate into a bill is `f"${float(x):.2f}"`, and the Rust
/// equivalent does not compile.
///
/// [`EstimatedUsd::amount`] returns a [`Decimal`], which is one visible step and
/// keeps the type name at the call site. A caller who genuinely needs a float
/// reaches for `Decimal::to_f64`, which is two named steps and a `Option` — and
/// which is a decision a reviewer can see.
///
/// [`Add`](std::ops::Add) is implemented only against itself, so summing an amount
/// that might be [`Amount::Unpriced`] is a type error rather than a silent zero.
///
/// # The coercion does not compile (COST-2)
///
/// The requirement is about an impl that is *absent*, so the check has to be a
/// program that fails to build. `compile_fail` doctests are that check, and they are
/// run by `cargo test` like any other:
///
/// The error code is pinned on each one, because a bare `compile_fail` passes for
/// *any* build failure — including a typo in the test itself, which would leave the
/// guard green while measuring nothing.
///
/// ```compile_fail,E0277
/// # use microvms_core::cost::EstimatedUsd;
/// # use rust_decimal_macros::dec;
/// let estimate = EstimatedUsd::new(dec!(1.23));
/// // No `From<EstimatedUsd> for f64`, so no `Into` either. This is the line the
/// // Python client could only ask a reader not to write.
/// let laundered: f64 = estimate.into();
/// ```
///
/// ```compile_fail,E0614
/// # use microvms_core::cost::EstimatedUsd;
/// # use rust_decimal_macros::dec;
/// let estimate = EstimatedUsd::new(dec!(1.23));
/// // No `Deref` and no public field: the figure comes out through `amount()`, which
/// // names the type at the call site.
/// let bare = *estimate;
/// ```
///
/// ```compile_fail,E0308
/// # use microvms_core::cost::{Amount, EstimatedUsd};
/// # use rust_decimal_macros::dec;
/// let estimate = EstimatedUsd::new(dec!(1.23));
/// let unknown = Amount::unpriced("no rate is published");
/// // `Add` is implemented only against `EstimatedUsd`, so the Python's runtime
/// // `TypeError` from `NotImplemented` is a compile error here (COST-4).
/// let wrong = estimate + unknown;
/// ```
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct EstimatedUsd(Decimal);

impl EstimatedUsd {
    /// Zero dollars, which is a legitimate *priced* figure — a zero-length hold, a
    /// zero-cycle comparison — and is not the same claim as [`Amount::Unpriced`].
    pub const ZERO: EstimatedUsd = EstimatedUsd(Decimal::ZERO);

    /// An estimate from a decimal figure already in the money path.
    ///
    /// Takes a [`Decimal`] and not an f64, so this constructor cannot be the second
    /// float boundary: [`gb_decimal`] and [`seconds_of`] are the only two, which is
    /// what makes COST-6's "exactly once" checkable by reading them.
    pub fn new(amount: Decimal) -> EstimatedUsd {
        EstimatedUsd(amount)
    }

    /// The figure, in decimal.
    ///
    /// The one visible step out. Deliberately not `Into<Decimal>`: an explicit call
    /// keeps `EstimatedUsd` in the reader's view at the point the label is dropped.
    pub fn amount(self) -> Decimal {
        self.0
    }

    /// The figure rendered at display precision, without the estimate label.
    ///
    /// For a caller assembling its own sentence. [`fmt::Display`] is what carries
    /// the label, and it is what a report renders through.
    pub fn amount_string(self) -> String {
        render_amount(self.0)
    }
}

impl std::ops::Add for EstimatedUsd {
    type Output = EstimatedUsd;

    /// Only an estimate adds to an estimate.
    ///
    /// There is no `Add<Amount>` and no `Add<Unpriced>`, so the Python's
    /// `NotImplemented` — a runtime `TypeError` — becomes a compile error. This is
    /// the last line of defence for COST-4: even a consumer that ignores
    /// [`CostReport::is_complete`] cannot sum an unknown into a dollar figure.
    fn add(self, other: EstimatedUsd) -> EstimatedUsd {
        EstimatedUsd(self.0 + other.0)
    }
}

impl std::iter::Sum for EstimatedUsd {
    fn sum<I: Iterator<Item = EstimatedUsd>>(iter: I) -> EstimatedUsd {
        iter.fold(EstimatedUsd::ZERO, std::ops::Add::add)
    }
}

/// A figure copied out of a terminal loses its docstring, so the label travels in
/// `Display` rather than sitting in the type name alone.
impl fmt::Display for EstimatedUsd {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "~${} (estimated)", render_amount(self.0))
    }
}

/// What a line item's cost can be (COST-3).
///
/// Any consumer that matches on this has to handle the unknown case, which is the
/// property the whole module rests on. [`Amount::Unpriced`] is distinct from
/// `Amount::Estimated(EstimatedUsd::ZERO)` on purpose: zero is a claim about the
/// bill, unpriced is a claim about the documentation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Amount {
    Estimated(EstimatedUsd),
    /// A quantity we can measure but cannot price, because no rate is published.
    ///
    /// The reason is a field and not optional, so the line item can say *why* rather
    /// than just that it cannot. [`BUILD_UNPRICED_REASON`] is the one this module
    /// raises.
    Unpriced {
        reason: String,
    },
}

impl Amount {
    /// A priced amount from a decimal figure.
    pub fn estimated(amount: Decimal) -> Amount {
        Amount::Estimated(EstimatedUsd::new(amount))
    }

    /// An unpriced amount, with the reason that goes on the line item.
    pub fn unpriced(reason: impl Into<String>) -> Amount {
        Amount::Unpriced {
            reason: reason.into(),
        }
    }

    /// The estimate, when there is one.
    ///
    /// Returns `Option` rather than defaulting to zero, which is the same decision
    /// as `cli.py`'s JSON omitting the `usd` key entirely for an unpriced line: a
    /// null gets summed as zero by anything permissive, and that is the one
    /// arithmetic this module refuses to enable.
    pub fn estimate(&self) -> Option<EstimatedUsd> {
        match self {
            Amount::Estimated(usd) => Some(*usd),
            Amount::Unpriced { .. } => None,
        }
    }

    /// The reason this could not be priced, when it could not.
    pub fn unpriced_reason(&self) -> Option<&str> {
        match self {
            Amount::Estimated(_) => None,
            Amount::Unpriced { reason } => Some(reason),
        }
    }

    /// The stable tag `cli.py` puts in the JSON envelope's `amount.kind`.
    pub fn kind(&self) -> &'static str {
        match self {
            Amount::Estimated(_) => "estimated-usd",
            Amount::Unpriced { .. } => "unpriced",
        }
    }
}

impl fmt::Display for Amount {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Amount::Estimated(usd) => write!(f, "{usd}"),
            Amount::Unpriced { reason } => write!(f, "unpriced — {reason}"),
        }
    }
}

/// One line item a total could not price: which phase it belonged to, and why.
///
/// The phase travels beside the reason because the two answer different questions and
/// a reader needs both. `cost.py:339-343` renders the *phase names* in the total's
/// parenthetical — `plus 1 unpriced (image-build)` — because a reason is a paragraph
/// and a total is one line, while [`Total::unpriced_reasons`] still hands over the
/// paragraphs for the consumer that wants them. Carrying only the reason, which is
/// what this type replaced, made the phase unavailable at render time and the Rust
/// total printed the paragraph.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UnpricedLine {
    /// The phase the line belonged to. What the total's parenthetical names.
    pub phase: CostPhase,
    /// Why no figure could be attached to it.
    pub reason: String,
}

/// A report's total, which is a lower bound whenever anything is unpriced (COST-4).
///
/// Two variants rather than a struct with a flag, and that is the upgrade over the
/// Python's `Total(priced, unpriced=())`: there a consumer could read `.priced` and
/// never look at `.is_lower_bound`, so the honesty rule depended on the caller
/// checking. Here the floor is *inside* [`Total::AtLeast`], beside the reasons, so
/// reaching it means seeing them. A plain sum cannot express "everything we could
/// price, plus a build AWS will not tell us the price of", and the natural way to
/// force it to — dropping the unpriced line — is exactly the lie this module exists
/// not to tell.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Total {
    /// Every line item was priced, so this is the whole estimate.
    Exact(EstimatedUsd),
    /// At least this much, plus line items with no published rate.
    AtLeast {
        /// The sum of the priced lines. Not the total — the total is unknowable.
        floor: EstimatedUsd,
        /// The lines that could not be priced, in report order.
        ///
        /// Non-empty by construction: [`Total::of`] answers [`Total::Exact`] when
        /// there is nothing to name, so an `AtLeast` with an empty list — a lower
        /// bound that will not say what it is missing — is not a value this type
        /// holds.
        unpriced: Vec<UnpricedLine>,
    },
}

impl Total {
    /// The total over a report's amounts, each labelled with the phase it came from.
    ///
    /// The one place a total is computed, so COST-4 holds for every consumer rather
    /// than for the ones that remembered. An unpriced amount routes the whole answer
    /// to [`Total::AtLeast`]; it is never skipped and never summed as zero.
    ///
    /// Takes `(phase, amount)` rather than a bare amount so the phase cannot go
    /// missing on the way in: the rendered total names phases, and a `Total` built
    /// without them could only fall back to printing reasons.
    pub fn of<'a>(amounts: impl IntoIterator<Item = (CostPhase, &'a Amount)>) -> Total {
        let mut floor = EstimatedUsd::ZERO;
        let mut unpriced: Vec<UnpricedLine> = Vec::new();
        for (phase, amount) in amounts {
            match amount {
                Amount::Estimated(usd) => floor = floor + *usd,
                Amount::Unpriced { reason } => unpriced.push(UnpricedLine {
                    phase,
                    reason: reason.clone(),
                }),
            }
        }
        if unpriced.is_empty() {
            Total::Exact(floor)
        } else {
            Total::AtLeast { floor, unpriced }
        }
    }

    /// Everything that could be priced.
    ///
    /// Named `floor` rather than `total` on purpose: for [`Total::AtLeast`] it is a
    /// lower bound, and a consumer reading a field called `total` would have no
    /// reason to check which variant it came from.
    pub fn floor(&self) -> EstimatedUsd {
        match self {
            Total::Exact(usd) => *usd,
            Total::AtLeast { floor, .. } => *floor,
        }
    }

    /// True when line items with no published rate are missing from the floor.
    pub fn is_lower_bound(&self) -> bool {
        matches!(self, Total::AtLeast { .. })
    }

    /// The lines that could not be priced. Empty for [`Total::Exact`].
    pub fn unpriced_lines(&self) -> &[UnpricedLine] {
        match self {
            Total::Exact(_) => &[],
            Total::AtLeast { unpriced, .. } => unpriced,
        }
    }

    /// The reasons the unpriced lines could not be priced, in report order. Empty for
    /// [`Total::Exact`].
    ///
    /// Kept beside [`Total::unpriced_lines`] because the reason is what a consumer
    /// surfacing the *why* wants, while the rendered total names phases.
    pub fn unpriced_reasons(&self) -> Vec<String> {
        self.unpriced_lines()
            .iter()
            .map(|line| line.reason.clone())
            .collect()
    }

    /// The distinct phase names of the unpriced lines, sorted — what the rendered
    /// total names.
    ///
    /// Sorted and deduplicated to match `cost.py:342`'s
    /// `", ".join(sorted({item.phase.value for item in self.unpriced}))`: two unpriced
    /// lines in one phase name it once, and the order does not depend on report order.
    pub fn unpriced_phase_names(&self) -> Vec<&'static str> {
        let mut names: Vec<&'static str> = self
            .unpriced_lines()
            .iter()
            .map(|line| line.phase.as_str())
            .collect();
        names.sort_unstable();
        names.dedup();
        names
    }
}

/// "at least" is the whole point, so it leads.
///
/// The parenthetical names **phases**, not reasons, which is `cost.py:343` verbatim. A
/// reason is a sentence explaining what AWS does not publish, and joining several of
/// them turns a one-line total into a paragraph the two clients cannot both print.
impl fmt::Display for Total {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Total::Exact(usd) => write!(f, "{usd}"),
            Total::AtLeast { floor, unpriced } => write!(
                f,
                "at least {floor}, plus {} unpriced ({})",
                unpriced.len(),
                self.unpriced_phase_names().join(", ")
            ),
        }
    }
}

// ── the rate table ───────────────────────────────────────────────────────────

/// us-east-1 rates, pinned to when they were read and where from.
///
/// The three booleans and the absent increment are documented *facts* rather than
/// settings, and they are fields because each one is a mistake someone would
/// otherwise make in arithmetic: blending vCPU into a GB-second, adding a per-invoke
/// charge by analogy with Lambda Functions, rounding a 200 ms exec up to a 1-second
/// increment, or subtracting a free tier that does not exist.
///
/// # Why the rate fields are private (COST-9)
///
/// The Python's `RateTable` was a plain frozen dataclass, so anyone could construct
/// one — which is how `pricing.py`'s ARM refusal ended up guarding only the fetch
/// path. Here the five rates are private and there are exactly two ways to obtain a
/// table: the pinned [`pinned_rates`], and [`RateTable::from_catalog`], which refuses a
/// catalog missing its ARM compute line rather than substituting the x86 one. So
/// "compute is priced from the ARM rate" is a property of the type rather than of a
/// code path that can be bypassed.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RateTable {
    region: Region,
    source_url: String,
    retrieved: CalendarDate,
    vcpu_second: Decimal,
    gb_second: Decimal,
    storage_gb_month: Decimal,
    snapshot_read_gb: Decimal,
    snapshot_write_gb: Decimal,
    minimum_retention: Duration,
}

impl RateTable {
    /// The region these rates price.
    ///
    /// Not a cosmetic label: eu-west-1 is 5.3% over us-east-1 on compute and 19% on
    /// snapshot storage, and ap-northeast-1 is 16.4% and 20%. A caller in Tokyo
    /// reading the us-east-1 table understates their snapshot write bill by 22.6%,
    /// which is not a rounding difference and would never show up as staleness.
    pub fn region(&self) -> &Region {
        &self.region
    }

    /// Where these figures came from, as an instruction a reader can act on.
    pub fn source_url(&self) -> &str {
        &self.source_url
    }

    /// When they were read.
    pub fn retrieved(&self) -> CalendarDate {
        self.retrieved
    }

    /// Per vCPU-second, billed separately from memory.
    pub fn vcpu_second(&self) -> Decimal {
        self.vcpu_second
    }

    /// Per GB-second of *baseline* memory, billed separately from vCPU.
    pub fn gb_second(&self) -> Decimal {
        self.gb_second
    }

    /// Per GB-month of snapshot storage.
    ///
    /// The one derived figure: the Pricing API quotes this per GB-hour, and this
    /// table holds per GB-month. See [`HOURS_PER_MONTH`].
    pub fn storage_gb_month(&self) -> Decimal {
        self.storage_gb_month
    }

    /// Per GB read on a launch or a resume.
    pub fn snapshot_read_gb(&self) -> Decimal {
        self.snapshot_read_gb
    }

    /// Per GB written on a suspend. Roughly 2.5x the read rate, so a cycle costed
    /// with one rate twice is wrong in whichever direction it picked.
    pub fn snapshot_write_gb(&self) -> Decimal {
        self.snapshot_write_gb
    }

    /// The documented storage minimum (COST-8).
    ///
    /// On the rate row itself, so it applies to anything stored there.
    pub fn minimum_retention(&self) -> Duration {
        self.minimum_retention
    }

    /// The same minimum in whole days, which is the unit the note on a floored line
    /// item quotes.
    ///
    /// Here rather than as an `as_secs() / 86_400` at the one call site, because that
    /// spelling is a second place the seconds-to-days convention lives: the field is
    /// the only thing that knows how long the window is, and a division written beside
    /// the message would keep saying "7-day" after a rate row moved to a fortnight.
    /// Truncating rather than rounding is deliberate — a floor quoted as longer than it
    /// is would overstate the charge a reader is being told about.
    pub fn minimum_retention_days(&self) -> u64 {
        self.minimum_retention.as_secs() / SECONDS_PER_DAY
    }

    /// MicroVMs bills per second with no per-request charge.
    ///
    /// A method returning a constant rather than a field, because it is a documented
    /// fact and not a table entry someone could edit into a different claim. The
    /// Lambda free tier is Functions-only and no MicroVMs free tier is published.
    pub fn per_request(&self) -> Decimal {
        Decimal::ZERO
    }

    /// vCPU and memory are two line items, as the pricing page prices them.
    ///
    /// A blended GB-second figure cannot be reconciled against a Cost Explorer
    /// breakdown that keeps them apart.
    pub fn bills_vcpu_and_memory_separately(&self) -> bool {
        true
    }

    /// No published free tier.
    pub fn free_tier(&self) -> bool {
        false
    }

    /// The minimum billing increment, which is **not published**.
    ///
    /// `None` means not published, not "one second". Nothing here rounds a duration
    /// up, because inventing an increment would overcharge every short exec in a
    /// report and there is no source for one.
    pub fn minimum_billing_increment(&self) -> Option<Duration> {
        None
    }

    /// How many days ago these rates were read.
    pub fn age_days(&self, today: CalendarDate) -> i64 {
        today.days_since(self.retrieved)
    }

    /// Whether the table is past [`STALE_AFTER_DAYS`] (COST-7).
    pub fn is_stale(&self, today: CalendarDate) -> bool {
        self.age_days(today) > STALE_AFTER_DAYS
    }

    /// The warning text, or `None` when the table is fresh.
    ///
    /// Text rather than a bool because the warning has to survive into whatever
    /// renders it, carrying the retrieval date and the URL — a figure copied out of a
    /// terminal loses everything else.
    pub fn staleness(&self, today: CalendarDate) -> Option<String> {
        if !self.is_stale(today) {
            return None;
        }
        Some(format!(
            "rate table for {} was retrieved {}, {} days ago (stale after {STALE_AFTER_DAYS}) — \
             re-read {} before trusting these figures",
            self.region,
            self.retrieved,
            self.age_days(today),
            self.source_url,
        ))
    }
}

/// Read 2026-08-07, us-east-1, and recorded in `docs/PLATFORM.md` under "What
/// actually costs money".
///
/// Four rates appear on the Lambda pricing page as written; `storage_gb_month` is
/// *derived*, because the AWS Pricing API quotes snapshot storage per GB-hour and
/// this table holds per GB-month. Change any of them and change that document in the
/// same commit.
///
/// A drift check against the Pricing API — not [`STALE_AFTER_DAYS`] — is what
/// actually tells you whether a rate moved; the staleness warning only ever said
/// that nobody had looked. See [`RateCatalog`].
///
/// Only the ARM figures are correct here and it is not a preference: MicroVMs are
/// ARM64-only, the x86 compute rates in the same catalog are 17.9% higher, and this
/// table used to hold the right ones by luck rather than by construction (COST-9).
///
/// A function rather than a `static`, because [`Region`] and the URL own heap
/// allocations. Cheap — five decimal copies and one small string — and called once
/// per report.
pub fn pinned_rates() -> RateTable {
    RateTable {
        region: Region::UsEast1,
        source_url: "https://aws.amazon.com/lambda/pricing/".to_string(),
        retrieved: CalendarDate::from_ymd(2026, 8, 7),
        vcpu_second: dec!(0.0000276944),
        gb_second: dec!(0.0000036667),
        // $0.0001111111 per GB-hour x 730 hours. Was 0.08 — a plausible-looking
        // round number that understated every stored GB by 1.37%, which is the whole
        // argument for deriving it from the API figure rather than reading a page.
        storage_gb_month: dec!(0.0811111030),
        snapshot_read_gb: dec!(0.00155),
        snapshot_write_gb: dec!(0.0038),
        minimum_retention: MINIMUM_RETENTION,
    }
}

// ── the catalog, and the ARM-only rule (COST-9) ───────────────────────────────

/// One rate the catalog has to carry, and what it fills.
///
/// The Python held this as `pricing.MICROVM_LINES`, a tuple of `RateLine` records
/// naming a `group`, a `usagetype`, a `unit`, and — for the two compute lines — an
/// `x86_group` probed only when the ARM line was missing. Same shape here, as a
/// closed enum: the catalog has five lines this client reads, and a sixth is not a
/// value that exists.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum CatalogLine {
    VcpuSecond,
    MemoryGbSecond,
    /// Quoted per GB-**hour** by the API. Converted once, in
    /// [`RateTable::from_catalog`].
    SnapshotStorageGbHour,
    SnapshotReadGb,
    SnapshotWriteGb,
}

impl CatalogLine {
    /// Every line, in the order the Python's `MICROVM_LINES` declares them.
    pub const ALL: [CatalogLine; 5] = [
        CatalogLine::VcpuSecond,
        CatalogLine::MemoryGbSecond,
        CatalogLine::SnapshotStorageGbHour,
        CatalogLine::SnapshotReadGb,
        CatalogLine::SnapshotWriteGb,
    ];

    /// The region-independent `group` attribute, which is what a fetch filters on.
    ///
    /// `usagetype` carries a location prefix in every region except us-east-1
    /// (`Lambda-MicroVM-vCPU-Second-ARM` is `USW2-Lambda-MicroVM-vCPU-Second-ARM` in
    /// us-west-2), so comparing raw usage types across regions matches nothing and
    /// yields a table of holes rather than an error. `group` does not.
    pub fn group(self) -> &'static str {
        match self {
            CatalogLine::VcpuSecond => "AWS-Lambda-MicroVM-vCPU-Second-ARM",
            CatalogLine::MemoryGbSecond => "AWS-Lambda-MicroVM-Memory-GB-Second-ARM",
            CatalogLine::SnapshotStorageGbHour => "AWS-Lambda-MicroVM-Snapshot-Storage-GB-Hour",
            CatalogLine::SnapshotReadGb => "AWS-Lambda-MicroVM-Snapshot-Read-GB",
            CatalogLine::SnapshotWriteGb => "AWS-Lambda-MicroVM-Snapshot-Write-GB",
        }
    }

    /// The [`RateTable`] field this fills, as the Python's `RateLine.field` spells
    /// it — so a drift report names the same thing in both clients.
    pub fn field(self) -> &'static str {
        match self {
            CatalogLine::VcpuSecond => "vcpu_second",
            CatalogLine::MemoryGbSecond => "gb_second",
            CatalogLine::SnapshotStorageGbHour => "storage_gb_month",
            CatalogLine::SnapshotReadGb => "snapshot_read_gb",
            CatalogLine::SnapshotWriteGb => "snapshot_write_gb",
        }
    }

    /// The unit the API must report for this line.
    ///
    /// Checked rather than assumed. It is the only signal available if AWS restates
    /// storage per GB-month: the number would change by 730x and every arithmetic
    /// check downstream would still pass, because they all read the same table.
    pub fn unit(self) -> &'static str {
        match self {
            CatalogLine::VcpuSecond => "vCPU-Seconds",
            CatalogLine::MemoryGbSecond => "GB-Seconds",
            CatalogLine::SnapshotStorageGbHour => "GB-Hours",
            CatalogLine::SnapshotReadGb | CatalogLine::SnapshotWriteGb => "GB",
        }
    }

    /// The x86 sibling, for the two compute lines that have one (COST-9).
    ///
    /// Named so the refusal can say what it is refusing. MicroVMs are ARM64-only —
    /// the `Architecture` shape is `enum: ['ARM_64']` with no other member — so the
    /// x86 rate can never apply, and it is 17.9% higher. Snapshot lines have no
    /// architecture variant, so there is nothing to refuse.
    pub fn x86_group(self) -> Option<&'static str> {
        match self {
            CatalogLine::VcpuSecond => Some("AWS-Lambda-MicroVM-vCPU-Second"),
            CatalogLine::MemoryGbSecond => Some("AWS-Lambda-MicroVM-Memory-GB-Second"),
            CatalogLine::SnapshotStorageGbHour
            | CatalogLine::SnapshotReadGb
            | CatalogLine::SnapshotWriteGb => None,
        }
    }

    /// True for storage, quoted per GB-hour where [`RateTable`] holds per GB-month.
    pub fn is_per_hour(self) -> bool {
        matches!(self, CatalogLine::SnapshotStorageGbHour)
    }
}

/// A priced line item as the catalog states it: a group, a unit, and one USD figure.
///
/// The USD figure is a [`Decimal`] and not an f64 because the API quotes it as a
/// string with ten significant digits — parsing it as a float and converting back
/// would lose exactly the precision the string was carrying.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CatalogEntry {
    /// The region-independent `group`. Compared against [`CatalogLine::group`].
    pub group: String,
    /// The unit the API reported. Compared against [`CatalogLine::unit`].
    pub unit: String,
    /// The rate, in USD per that unit.
    pub usd: Decimal,
}

/// What a rate source offers for one region, before it becomes a [`RateTable`].
///
/// This is the parse boundary COST-9 lives at. A transport — the Pricing API, a
/// recorded fixture, a `mise run live:rates` dump — assembles one of these, and
/// [`RateTable::from_catalog`] is the only door from here into a priced report. So
/// the ARM refusal cannot be bypassed by building a table directly, which is what it
/// could be in the Python.
///
/// Deliberately holds *every* entry the source offered, including the x86 compute
/// rates. A catalog that filtered them out could not tell "the ARM line is gone and
/// the x86 one is right there" from "the whole catalog is empty", and those are
/// different repairs: work out what AWS renamed, versus check the region.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct RateCatalog {
    entries: Vec<CatalogEntry>,
}

impl RateCatalog {
    /// An empty catalog, to add entries to.
    pub fn new() -> RateCatalog {
        RateCatalog {
            entries: Vec::new(),
        }
    }

    /// Records one priced line.
    #[must_use]
    pub fn with_entry(
        mut self,
        group: impl Into<String>,
        unit: impl Into<String>,
        usd: Decimal,
    ) -> RateCatalog {
        self.entries.push(CatalogEntry {
            group: group.into(),
            unit: unit.into(),
            usd,
        });
        self
    }

    /// Every entry for a group. Normally exactly one.
    fn matching(&self, group: &str) -> Vec<&CatalogEntry> {
        self.entries
            .iter()
            .filter(|entry| entry.group == group)
            .collect()
    }

    /// Whether the catalog offered nothing at all.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

impl RateTable {
    /// A table for one region, built from a catalog — or a refusal (COST-9).
    ///
    /// All or nothing. A partial table would price a run at less than it costs, and
    /// the caller has no way to see which field was quietly left at a stale value.
    ///
    /// Four ways this refuses, each a way the catalog could change and still parse:
    /// a missing ARM compute line whose x86 sibling is present (the substitution that
    /// would look entirely healthy and inflate every estimate by 17.9%), a missing
    /// line with no sibling, a restated unit, and two products where there was one.
    pub fn from_catalog(
        region: Region,
        source_url: impl Into<String>,
        retrieved: CalendarDate,
        catalog: &RateCatalog,
    ) -> Result<RateTable, Error> {
        let mut rates: Vec<Decimal> = Vec::with_capacity(CatalogLine::ALL.len());
        for line in CatalogLine::ALL {
            let matches = catalog.matching(line.group());
            let entry = match matches.as_slice() {
                [] => return Err(missing_line_error(&region, line, catalog)),
                [only] => *only,
                many => {
                    return Err(Error::invalid_arg(format!(
                        "{} in {region} returned {} products where one group in one region has \
                         always been one product; the catalog gained a dimension this client \
                         does not know how to choose between, and picking one would silently \
                         select a rate",
                        line.group(),
                        many.len(),
                    )));
                }
            };
            if entry.unit != line.unit() {
                return Err(Error::invalid_arg(format!(
                    "{} in {region} is now quoted per {:?}, not {:?}; the conversion into the \
                     rate table no longer holds and every downstream figure would still look \
                     plausible",
                    line.group(),
                    entry.unit,
                    line.unit(),
                )));
            }
            // The API quotes storage per GB-hour. Converting here rather than at
            // every use site keeps `RateTable` one convention deep, which is the
            // whole reason `SECONDS_PER_MONTH` exists.
            rates.push(if line.is_per_hour() {
                entry.usd * HOURS_PER_MONTH
            } else {
                entry.usd
            });
        }
        Ok(RateTable {
            region,
            source_url: source_url.into(),
            retrieved,
            vcpu_second: rates[0],
            gb_second: rates[1],
            storage_gb_month: rates[2],
            snapshot_read_gb: rates[3],
            snapshot_write_gb: rates[4],
            // The catalog prices line items and says nothing about a one-week storage
            // minimum, a per-request charge, a billing increment, or a free tier. So
            // a fetched table is authoritative on *rates* and still hand-read on
            // *rules*, and dropping the retention floor here would understate a
            // create-and-destroy suite by four orders of magnitude.
            minimum_retention: MINIMUM_RETENTION,
        })
    }
}

/// The error for a line the catalog does not have (COST-9).
///
/// Split out because the ARM case needs to say more than "missing": the x86 sibling
/// is right there, 17.9% higher, the same shape, and it parses. The tempting fix is
/// to use it, and every estimate would inflate while nothing said so — so the message
/// names the rate it is refusing and the magnitude of the error that substituting it
/// would introduce.
fn missing_line_error(region: &Region, line: CatalogLine, catalog: &RateCatalog) -> Error {
    if catalog.is_empty() {
        return Error::invalid_arg(format!(
            "the rate catalog has no MicroVM line items for {region} at all, so {} cannot be \
             filled. Measured 2026-08-07, pricing coverage and service availability coincide: a \
             region that does not price MicroVMs does not run them either, and answers \
             lambda-microvms with AccessDeniedException and a null message that reads exactly \
             like an IAM problem it is not. Check the region before auditing a policy",
            line.field(),
        ));
    }
    let Some(x86_group) = line.x86_group() else {
        return Error::invalid_arg(format!(
            "{region} prices MicroVMs but has no {} line item, so {} cannot be filled",
            line.group(),
            line.field(),
        ));
    };
    match catalog.matching(x86_group).first() {
        Some(sibling) => Error::invalid_arg(format!(
            "{region} has no {}, only the x86 rate {x86_group} at ${}. MicroVMs are ARM64-only \
             — the Architecture shape is enum ['ARM_64'] with no other member — so the x86 rate \
             can never apply and is not substituted for {}; doing so would overstate every \
             compute figure by roughly 18%",
            line.group(),
            sibling.usd,
            line.field(),
        )),
        None => Error::invalid_arg(format!(
            "{region} has neither {} nor {x86_group}, so {} cannot be filled",
            line.group(),
            line.field(),
        )),
    }
}

// ── phases and billing lines ─────────────────────────────────────────────────

/// The line items AWS bills, spelled as separately as AWS bills them.
///
/// vCPU and memory are two entries rather than one blended GB-second because that is
/// how the pricing page prices them, and a blended figure cannot be reconciled
/// against a Cost Explorer breakdown that keeps them apart.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum BillingLine {
    Vcpu,
    Memory,
    SnapshotStorage,
    SnapshotRead,
    SnapshotWrite,
}

impl BillingLine {
    /// The wire spelling, identical to the Python `StrEnum` member.
    pub fn as_str(self) -> &'static str {
        match self {
            BillingLine::Vcpu => "vcpu",
            BillingLine::Memory => "memory",
            BillingLine::SnapshotStorage => "snapshot-storage",
            BillingLine::SnapshotRead => "snapshot-read",
            BillingLine::SnapshotWrite => "snapshot-write",
        }
    }
}

impl fmt::Display for BillingLine {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The lifecycle a sandbox goes through, as the phases that cost money.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum CostPhase {
    ImageBuild,
    ImageStorage,
    Launch,
    Running,
    Suspended,
    Suspend,
    Resume,
}

impl CostPhase {
    /// Every phase, in lifecycle order.
    ///
    /// Public because a caller that has to *judge* a phase string needs the list to
    /// refuse with, and the two bindings each grew their own copy of this array before
    /// it existed here — a parallel table that would have gone stale the first time a
    /// phase was added. [`CostPhase::from_str`] is the reader; the round-trip test below
    /// is what stops a variant being added to the enum and forgotten here.
    pub const ALL: [CostPhase; 7] = [
        CostPhase::ImageBuild,
        CostPhase::ImageStorage,
        CostPhase::Launch,
        CostPhase::Running,
        CostPhase::Suspended,
        CostPhase::Suspend,
        CostPhase::Resume,
    ];

    /// The wire spelling, identical to the Python `StrEnum` member.
    pub fn as_str(self) -> &'static str {
        match self {
            CostPhase::ImageBuild => "image-build",
            CostPhase::ImageStorage => "image-storage",
            CostPhase::Launch => "launch",
            CostPhase::Running => "running",
            CostPhase::Suspended => "suspended",
            CostPhase::Suspend => "suspend",
            CostPhase::Resume => "resume",
        }
    }
}

impl FromStr for CostPhase {
    type Err = Error;

    /// A phase from its wire spelling, refusing anything else with the whole list.
    ///
    /// Here rather than in each caller because the callers are *bindings*: a Python or
    /// JS `report.by_phase("running")` has a bare string where Rust has a variant, so
    /// something has to judge it. Both bindings did, each with its own seven-element
    /// array — two parallel tables over a closed set, which is one table too many and
    /// two too many to keep in step with the enum. This is the single reader, and the
    /// list it offers on a refusal is [`CostPhase::ALL`] rather than a written-out
    /// sentence, so a phase added to the enum appears in the message without an edit.
    ///
    /// Note the asymmetry with [`Region::from_str`](crate::Region): a region's refusal
    /// exists because an unlisted region is a *plausible* value this client has no
    /// evidence for, and there is an opt-in. A phase has no opt-in and needs none — the
    /// set is closed by the billing model, so an unrecognized spelling is a typo and
    /// nothing more.
    fn from_str(phase: &str) -> Result<CostPhase, Error> {
        CostPhase::ALL
            .into_iter()
            .find(|candidate| candidate.as_str() == phase)
            .ok_or_else(|| {
                let offered = CostPhase::ALL
                    .iter()
                    .map(|candidate| candidate.as_str())
                    .collect::<Vec<_>>()
                    .join(", ");
                Error::invalid_arg(format!(
                    "{phase:?} is not a cost phase; the phases are {offered}"
                ))
            })
    }
}

/// `pad` rather than `write_str`, because a phase is a column.
///
/// `write_str` ignores the formatter's width entirely, so `{:<14}` on a phase silently
/// produced a ragged table — every plain-text line's dollar figure started at a
/// different offset, and the fix looks like it is already there in the format string.
/// [`fmt::Formatter::pad`] is the one spelling that honours width, precision, and
/// alignment; `write!(f, "{}", ...)` here would recurse straight back into this
/// impl and drop the width again.
impl fmt::Display for CostPhase {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.pad(self.as_str())
    }
}

/// One phase's one billing line: what was consumed, and what that costs.
///
/// `quantity` and `unit` are kept beside `amount` so a reader can check the
/// arithmetic against the rate table instead of trusting the total, which is the only
/// defence against a rate that drifts out of date without anyone noticing.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LineItem {
    pub phase: CostPhase,
    /// `None` only for a phase with no published rate to attribute it to.
    pub line: Option<BillingLine>,
    pub quantity: Decimal,
    pub unit: String,
    pub amount: Amount,
    pub duration: Option<DurationP>,
    pub note: String,
}

impl LineItem {
    /// The quantity at display precision, for a CLI column.
    pub fn quantity_string(&self) -> String {
        render_quantity(self.quantity)
    }
}

impl fmt::Display for LineItem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let consumed = format!("{} {}", self.quantity_string(), self.unit);
        let head = format!("{:<14} {:<26} {}", self.phase, consumed, self.amount);
        if self.note.is_empty() {
            f.write_str(&head)
        } else {
            write!(f, "{head}  [{}]", self.note)
        }
    }
}

// ── the report ───────────────────────────────────────────────────────────────

/// Per-phase attribution for one sandbox, measured or projected.
///
/// Holds the rate table it was computed against rather than reaching for the pinned
/// default, so a report stays readable — and reproducible — after [`pinned_rates`] is
/// updated.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CostReport {
    label: String,
    size: SizeClass,
    rates: RateTable,
    items: Vec<LineItem>,
    staleness: Option<String>,
}

impl CostReport {
    /// What this report is of, for a header.
    pub fn label(&self) -> &str {
        &self.label
    }

    /// The class every compute figure here was billed at (COST-5).
    pub fn size(&self) -> SizeClass {
        self.size
    }

    /// The table these figures were computed against.
    pub fn rates(&self) -> &RateTable {
        &self.rates
    }

    /// Every line item, in lifecycle order.
    pub fn items(&self) -> &[LineItem] {
        &self.items
    }

    /// The staleness warning, when the table was stale at computation time (COST-7).
    ///
    /// Carried on the report because the warning has to survive into whatever renders
    /// it: a library caller with a log filter and a CLI that only wrote stderr would
    /// each lose it on their own.
    pub fn staleness(&self) -> Option<&str> {
        self.staleness.as_deref()
    }

    /// The line items with a published rate.
    pub fn priced(&self) -> impl Iterator<Item = &LineItem> {
        self.items
            .iter()
            .filter(|item| item.amount.estimate().is_some())
    }

    /// The line items with no published rate.
    pub fn unpriced(&self) -> impl Iterator<Item = &LineItem> {
        self.items
            .iter()
            .filter(|item| item.amount.unpriced_reason().is_some())
    }

    /// The total, which is a lower bound whenever anything is unpriced (COST-4).
    pub fn total(&self) -> Total {
        Total::of(self.items.iter().map(|item| (item.phase, &item.amount)))
    }

    /// False whenever any phase has no published rate. See [`Total`].
    pub fn is_complete(&self) -> bool {
        self.unpriced().next().is_none()
    }

    /// True only if every duration was timed. An estimate is never this (COST-10).
    pub fn fully_measured(&self) -> bool {
        let mut durations = self
            .items
            .iter()
            .filter_map(|item| item.duration)
            .peekable();
        durations.peek().is_some() && durations.all(DurationP::is_measured)
    }

    /// The line items belonging to one phase.
    pub fn by_phase(&self, phase: CostPhase) -> impl Iterator<Item = &LineItem> {
        self.items.iter().filter(move |item| item.phase == phase)
    }

    /// Plain text for a CLI. Leads with what the dollars are, not the dollars.
    ///
    /// The header is not decoration: a figure copied out of a terminal loses its
    /// docstring, so the estimate label and the retrieval date travel with it.
    pub fn render(&self) -> String {
        let mut lines = vec![
            format!("{} — {}", self.label, self.size),
            format!(
                "dollars are estimates derived from published {} rates (retrieved {}); only Cost \
                 Explorer knows the bill",
                self.rates.region, self.rates.retrieved,
            ),
        ];
        if let Some(warning) = &self.staleness {
            lines.push(format!("WARNING: {warning}"));
        }
        lines.extend(self.items.iter().map(|item| format!("  {item}")));
        lines.push(format!("total: {}", self.total()));
        lines.join("\n")
    }
}

// ── the line builders ────────────────────────────────────────────────────────

/// Compute for one phase, as two line items (COST-5).
///
/// Both figures read `baseline_*`. The guest reports the peak and bursts to it, but
/// the peak is charged only for the seconds above baseline that are actually consumed
/// — which this client cannot observe, so it is left out rather than guessed at. See
/// [`crate::sizing`], whose 2 GB class reports 8 GB in the guest: reading the peak
/// would overstate the memory line exactly 4x.
fn compute_lines(
    size: SizeClass,
    duration: DurationP,
    rates: &RateTable,
    phase: CostPhase,
) -> Result<[LineItem; 2], Error> {
    let seconds = duration.seconds();
    // `baseline_gb` and `baseline_vcpu` are the accessors whose doc comments say they
    // are the billed figures; `peak_gb`/`peak_vcpu` are deliberately not reachable
    // from here. Both cross the float boundary through `gb_decimal` (COST-6).
    let vcpu = gb_decimal(size.baseline_vcpu())?;
    let memory = gb_decimal(size.baseline_gb())?;
    Ok([
        LineItem {
            phase,
            line: Some(BillingLine::Vcpu),
            quantity: vcpu * seconds,
            unit: "vCPU-seconds".to_string(),
            amount: Amount::estimated(vcpu * seconds * rates.vcpu_second()),
            duration: Some(duration),
            note: format!("{vcpu} vCPU baseline"),
        },
        LineItem {
            phase,
            line: Some(BillingLine::Memory),
            quantity: memory * seconds,
            unit: "GB-seconds".to_string(),
            amount: Amount::estimated(memory * seconds * rates.gb_second()),
            duration: Some(duration),
            note: format!("{memory} GB baseline, billed separately from vCPU"),
        },
    ])
}

/// Snapshot storage for a hold, with the documented minimum retention applied
/// (COST-8).
///
/// The minimum is on the rate row itself, so it applies to anything stored there.
/// `docs/PLATFORM.md` demonstrates it only for images — a 2 GB image deleted after
/// sixty seconds still bills about four cents — and says nothing about a suspend
/// snapshot released early, so the note names which case is documented and which is
/// the rate row read at face value. Not applying it would understate the one line item
/// that dominates a create-and-destroy suite, by four orders of magnitude.
fn storage_line(
    phase: CostPhase,
    gb: Decimal,
    held: DurationP,
    rates: &RateTable,
) -> Result<LineItem, Error> {
    let floor = seconds_of(rates.minimum_retention());
    let held_seconds = held.seconds();
    let billed = held_seconds.max(floor);
    let quantity = gb * billed / SECONDS_PER_MONTH;
    let mut note = format!("{} GB held {held}", gb.normalize());
    if billed > held_seconds {
        // The day count comes off the rate row rather than out of a division here: the
        // field is the only thing that knows how long the window is, and `as_secs() /
        // 86_400` beside the message is a second convention that would keep saying
        // "7-day" after the row moved.
        note.push_str(&format!(
            "; billed {}-day minimum retention ({floor}s) instead",
            rates.minimum_retention_days(),
        ));
    }
    Ok(LineItem {
        phase,
        line: Some(BillingLine::SnapshotStorage),
        quantity,
        unit: "GB-months".to_string(),
        amount: Amount::estimated(quantity * rates.storage_gb_month()),
        duration: Some(held),
        note,
    })
}

/// A snapshot write or read, billed per GB moved with no time component.
fn transfer_line(
    phase: CostPhase,
    line: BillingLine,
    gb: Decimal,
    count: u32,
    rates: &RateTable,
    note_suffix: &str,
) -> LineItem {
    let rate = if line == BillingLine::SnapshotWrite {
        rates.snapshot_write_gb()
    } else {
        rates.snapshot_read_gb()
    };
    let quantity = gb * Decimal::from(count);
    LineItem {
        phase,
        line: Some(line),
        quantity,
        unit: "GB".to_string(),
        amount: Amount::estimated(quantity * rate),
        duration: None,
        note: format!("{count} x {} GB{note_suffix}", gb.normalize()),
    }
}

/// Why the image build has no price.
///
/// Not a caveat in a doc comment: it is the [`Amount::Unpriced`] reason that appears
/// on the line item and in every total that contains it. The build starts a real
/// MicroVM to run the Dockerfile, so it plausibly is billed, but AWS does not say and
/// we have not measured it.
pub const BUILD_UNPRICED_REASON: &str = "AWS does not publish whether the server-side image build is billed as compute; the build \
     runs a real MicroVM, so treating it as free would understate the run";

/// The image build: a real phase with a real duration and no published rate (COST-3).
///
/// An untimed build still gets a line. Omitting it would leave the report looking
/// complete, and the report being visibly incomplete is the whole point of the phase
/// appearing at all.
fn build_line(duration: Option<DurationP>) -> LineItem {
    LineItem {
        phase: CostPhase::ImageBuild,
        line: None,
        quantity: duration.map_or(Decimal::ZERO, DurationP::seconds),
        unit: if duration.is_some() {
            "seconds".to_string()
        } else {
            "seconds (untimed)".to_string()
        },
        amount: Amount::unpriced(BUILD_UNPRICED_REASON),
        duration,
        note: "unknown, not zero — see docs/PLATFORM.md, 'Not published'".to_string(),
    }
}

// ── the reports ──────────────────────────────────────────────────────────────

/// What to attribute cost to, for one sandbox's lifecycle.
///
/// A struct rather than eleven parameters, because the Python's keyword-only
/// signature has no direct Rust equivalent and eleven positional arguments is how a
/// caller passes `image_gb` where `snapshot_gb` was meant. `Default` gives the same
/// "pass only the phases that happened" ergonomics as the Python's defaults.
///
/// Every duration is a [`DurationP`], so a report built from timed phases and one
/// built from a plan are the same shape and are told apart by their own contents
/// rather than by which function produced them.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RunUsage {
    /// Wall-clock time in RUNNING. Bills at baseline whether or not anything is
    /// executing — unlike AgentCore Runtime there is no free I/O wait, which is why
    /// suspension rather than idleness is the lever.
    pub running: Option<DurationP>,
    /// Time held suspended. Pays storage only: a suspended VM is frozen.
    pub suspended: Option<DurationP>,
    /// How long the image build took, if it was timed. Always unpriced (COST-3).
    pub image_build: Option<DurationP>,
    /// The image's size. Passing this adds both an image-storage line *and* the
    /// unpriced build line: an image that has storage cost was built, and the build's
    /// price is unknown, so a create-and-destroy report is never complete. A caller
    /// reusing an existing image passes `None` and pays neither.
    pub image_gb: Option<f64>,
    /// How long the image was retained. Defaults to the documented minimum, marked
    /// [`Provenance::Projected`] — nobody timed that week either.
    pub image_retained: Option<DurationP>,
    /// Each cycle pays a snapshot write plus a read.
    pub suspend_resume_cycles: u32,
    /// The suspend snapshot's size. Defaults to the baseline memory footprint, which
    /// is what `docs/PLATFORM.md`'s own worked figures use ("a suspended 2 GB VM pays
    /// about $0.16 a month" is 2 GB at the storage rate). Whether a suspend snapshot
    /// is baseline-sized or peak-sized is not documented; override it if you have
    /// measured otherwise.
    pub snapshot_gb: Option<f64>,
    /// Whether a launch happened. A launch reads a snapshot.
    pub launched: bool,
}

impl RunUsage {
    /// A usage record for a sandbox that launched, with no phase attributed yet.
    ///
    /// `launched: true` matches the Python's default, and it is the right default:
    /// almost every report is of a VM that ran, and the comparison path that does not
    /// launch says so explicitly.
    pub fn launched() -> RunUsage {
        RunUsage {
            launched: true,
            ..RunUsage::default()
        }
    }
}

/// Per-phase attribution for one sandbox's lifecycle.
///
/// `today` decides staleness (COST-7) and is a parameter rather than a clock read, so
/// a report is a pure function of its inputs and a test does not have to travel in
/// time. Pass [`CalendarDate::today_utc`] from a binary.
pub fn run_report(
    size: SizeClass,
    usage: &RunUsage,
    rates: &RateTable,
    today: CalendarDate,
    label: impl Into<String>,
) -> Result<CostReport, Error> {
    let snapshot = match usage.snapshot_gb {
        Some(gb) => gb_decimal(gb)?,
        None => gb_decimal(size.baseline_gb())?,
    };
    let image_gb = usage.image_gb.map(gb_decimal).transpose()?;
    let mut items: Vec<LineItem> = Vec::new();

    if let Some(image_gb) = image_gb {
        items.push(build_line(usage.image_build));
        let held = usage
            .image_retained
            .unwrap_or(DurationP::Projected(rates.minimum_retention()));
        items.push(storage_line(
            CostPhase::ImageStorage,
            image_gb,
            held,
            rates,
        )?);
    } else if usage.image_build.is_some() {
        items.push(build_line(usage.image_build));
    }

    if usage.launched {
        // A launch reads a snapshot at the same per-GB rate as a resume. Which
        // snapshot's size that read covers is not documented, so it uses the image
        // when the caller named one and the memory footprint otherwise.
        let read_gb = image_gb.unwrap_or(snapshot);
        items.push(transfer_line(
            CostPhase::Launch,
            BillingLine::SnapshotRead,
            read_gb,
            1,
            rates,
            "; 'launch or resume' shares one rate, and which snapshot a launch reads is \
             undocumented",
        ));
    }

    if let Some(running) = usage.running {
        items.extend(compute_lines(size, running, rates, CostPhase::Running)?);
    }

    if let Some(suspended) = usage.suspended {
        // No compute line at all: a suspended VM is frozen, so it pays storage only.
        // Not a compute line multiplied by zero, which would reappear the moment
        // someone changed how a duration is derived.
        items.push(storage_line(
            CostPhase::Suspended,
            snapshot,
            suspended,
            rates,
        )?);
    }

    if usage.suspend_resume_cycles > 0 {
        items.push(transfer_line(
            CostPhase::Suspend,
            BillingLine::SnapshotWrite,
            snapshot,
            usage.suspend_resume_cycles,
            rates,
            "",
        ));
        items.push(transfer_line(
            CostPhase::Resume,
            BillingLine::SnapshotRead,
            snapshot,
            usage.suspend_resume_cycles,
            rates,
            "",
        ));
    }

    Ok(CostReport {
        label: label.into(),
        size,
        rates: rates.clone(),
        items,
        staleness: rates.staleness(today),
    })
}

/// A plan's phases, in plain seconds, before anything is spent.
///
/// Separate from [`RunUsage`] on purpose (COST-10): its fields are `f64` seconds
/// rather than [`DurationP`], so there is no field an accidentally-`Measured`
/// duration could be written into. The wrapping happens in [`estimate_run`], in one
/// place, and every one of them is [`Provenance::Projected`].
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct PlanUsage {
    pub running_seconds: f64,
    pub suspended_seconds: f64,
    pub image_gb: Option<f64>,
    pub image_retained_seconds: Option<f64>,
    pub suspend_resume_cycles: u32,
    pub snapshot_gb: Option<f64>,
    pub launched: bool,
}

impl PlanUsage {
    /// A plan for a sandbox that launches, with no phase attributed yet.
    pub fn launched() -> PlanUsage {
        PlanUsage {
            launched: true,
            ..PlanUsage::default()
        }
    }
}

/// What a plan will cost, before spending anything (COST-10).
///
/// Takes plain seconds and marks every one of them [`Provenance::Projected`], so the
/// resulting report can never claim to be measured. That is the difference between
/// this and [`run_report`]: not the arithmetic, which is shared — it delegates — but
/// what the durations admit about themselves.
pub fn estimate_run(
    size: SizeClass,
    plan: &PlanUsage,
    rates: &RateTable,
    today: CalendarDate,
    label: impl Into<String>,
) -> Result<CostReport, Error> {
    // Zero seconds means "this phase did not happen", matching the Python's
    // `if running_seconds else None`. A zero-length phase priced at zero would put a
    // line on the report claiming a measurement nobody took.
    let projected = |seconds: f64| -> Result<Option<DurationP>, Error> {
        if seconds == 0.0 {
            return Ok(None);
        }
        Ok(Some(DurationP::projected_secs_f64(seconds)?))
    };
    let usage = RunUsage {
        running: projected(plan.running_seconds)?,
        suspended: projected(plan.suspended_seconds)?,
        // No `image_build` duration: a plan has not built anything, so there is
        // nothing to have timed. The unpriced build line still appears whenever
        // `image_gb` is set, because the image will have to be built.
        image_build: None,
        image_gb: plan.image_gb,
        image_retained: plan
            .image_retained_seconds
            .map(DurationP::projected_secs_f64)
            .transpose()?,
        suspend_resume_cycles: plan.suspend_resume_cycles,
        snapshot_gb: plan.snapshot_gb,
        launched: plan.launched,
    };
    run_report(size, &usage, rates, today, label)
}

// ── running versus suspended ─────────────────────────────────────────────────

/// Running versus suspended for the same VM over the same wall time.
///
/// The gap is roughly two orders of magnitude over a month, which is the entire
/// argument for a warm suspended pool. `cycles` is here so the argument stays honest:
/// each suspend/resume pays a snapshot write plus a read, and a pool that churns
/// spends more on transitions than it saves on residency. The conclusion to draw is
/// "avoid churn", not "avoid residency".
///
/// Both sides exclude image build and storage. They are the same for either choice,
/// and leaving them in would dilute the ratio the comparison exists to show — and drag
/// an unpriced build into a figure whose whole job is to be comparable.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResidencyComparison {
    size: SizeClass,
    hold: DurationP,
    cycles: u32,
    running: CostReport,
    suspended: CostReport,
    rates: RateTable,
}

impl ResidencyComparison {
    pub fn size(&self) -> SizeClass {
        self.size
    }

    /// The wall time both sides cover. Always [`Provenance::Projected`]: a comparison
    /// is a hypothetical about a hold nobody has taken yet.
    pub fn hold(&self) -> DurationP {
        self.hold
    }

    pub fn cycles(&self) -> u32 {
        self.cycles
    }

    /// The report for leaving the VM running.
    pub fn running(&self) -> &CostReport {
        &self.running
    }

    /// The report for suspending it, transitions included.
    pub fn suspended(&self) -> &CostReport {
        &self.suspended
    }

    /// How many times more the running VM costs.
    ///
    /// Zero-safe by construction: snapshot storage always has the minimum-retention
    /// floor (COST-8), so the denominator is never zero. Both totals are
    /// [`Total::Exact`] because both sides exclude the unpriced build, which is what
    /// makes reading their floors sound here.
    pub fn ratio(&self) -> Decimal {
        self.running.total().floor().amount() / self.suspended.total().floor().amount()
    }

    /// One suspend/resume: a snapshot write plus a read, per GB.
    ///
    /// Without it the honest conclusion inverts — "suspend constantly" reads as free.
    pub fn per_cycle(&self) -> Result<EstimatedUsd, Error> {
        let gb = gb_decimal(self.size.baseline_gb())?;
        Ok(EstimatedUsd::new(
            gb * (self.rates.snapshot_write_gb() + self.rates.snapshot_read_gb()),
        ))
    }

    /// How long a VM must stay suspended for the cycle to pay for itself.
    ///
    /// Below this, suspending and resuming costs more than having left the VM running
    /// — which is the number a pool scheduler actually needs, and the one a bare "100x
    /// cheaper" headline hides.
    ///
    /// Returns a [`Decimal`] where the Python returned a `float`, so the seconds
    /// figure stays in the same representation as the money it was derived from. See
    /// [`ResidencyComparison::break_even_seconds_f64`] for the lossy step, which is
    /// named as one.
    pub fn break_even_seconds(&self) -> Result<Decimal, Error> {
        let vcpu = gb_decimal(self.size.baseline_vcpu())?;
        let gb = gb_decimal(self.size.baseline_gb())?;
        let running_per_sec = vcpu * self.rates.vcpu_second() + gb * self.rates.gb_second();
        let storage_per_sec = gb * self.rates.storage_gb_month() / SECONDS_PER_MONTH;
        let floor_sec = seconds_of(self.rates.minimum_retention());
        let churn = self.per_cycle()?.amount();
        // Inside the minimum-retention window the storage charge is a constant, so the
        // equation is linear in the hold; past it storage grows with the hold and the
        // slope changes. Solve the constant branch first and only take the other if the
        // answer falls outside the window.
        let candidate = (churn + floor_sec * storage_per_sec) / running_per_sec;
        if candidate > floor_sec {
            return Ok(churn / (running_per_sec - storage_per_sec));
        }
        Ok(candidate)
    }

    /// The break-even hold as an f64, for a JSON envelope.
    ///
    /// **Lossy, and named so.** The exact answer is
    /// [`ResidencyComparison::break_even_seconds`]; this exists because `cli.py` emits
    /// `breakEvenSeconds` as a JSON number and the two clients have to agree on it.
    /// Seconds, not dollars — no money figure has an f64 accessor (COST-2).
    pub fn break_even_seconds_f64(&self) -> Result<f64, Error> {
        let exact = self.break_even_seconds()?;
        exact.to_f64().ok_or_else(|| {
            Error::invalid_arg(format!(
                "the break-even hold of {exact}s does not fit in an f64, so it cannot be reported \
                 as a JSON number"
            ))
        })
    }

    /// Plain text for a CLI, ending with the conclusion the numbers support.
    pub fn render(&self) -> Result<String, Error> {
        Ok([
            format!("{} held {}", self.size, self.hold),
            format!("  running:   {}", self.running.total()),
            format!(
                "  suspended: {} ({} cycle(s) included)",
                self.suspended.total(),
                self.cycles,
            ),
            format!(
                "  ratio:     {}x cheaper suspended",
                self.ratio().round_dp(1)
            ),
            format!(
                "  per cycle: {} — break-even hold {}s, so avoid churn rather than residency",
                self.per_cycle()?,
                self.break_even_seconds()?.round_dp(0),
            ),
        ]
        .join("\n"))
    }
}

/// The warm-pool argument, with its own counter-argument attached.
///
/// `cycles` should be at least 1: a suspension that is never resumed is a
/// termination, and pricing it as free transitions is how a pool design that churns
/// every few seconds looks affordable on paper. Zero is accepted rather than refused
/// because a caller comparing pure residency has a legitimate question, and the
/// per-cycle figure is on the comparison either way.
pub fn compare_residency(
    size: SizeClass,
    hold: Duration,
    cycles: u32,
    rates: &RateTable,
    today: CalendarDate,
) -> Result<ResidencyComparison, Error> {
    let hold = DurationP::Projected(hold);
    let running = run_report(
        size,
        &RunUsage {
            running: Some(hold),
            launched: false,
            ..RunUsage::default()
        },
        rates,
        today,
        "left running",
    )?;
    let suspended = run_report(
        size,
        &RunUsage {
            suspended: Some(hold),
            suspend_resume_cycles: cycles,
            launched: false,
            ..RunUsage::default()
        },
        rates,
        today,
        "suspended",
    )?;
    Ok(ResidencyComparison {
        size,
        hold,
        cycles,
        running,
        suspended,
        rates: rates.clone(),
    })
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;
    use crate::error::ErrorKind;

    /// A month as AWS's GB-month rate defines it. Every "per month" figure in
    /// `docs/PLATFORM.md` is this many seconds, not 30 days.
    const MONTH: Duration = Duration::from_secs(730 * 3600);

    /// A date the pinned table is fresh on, so a staleness warning in a test that is
    /// not about staleness is a failure rather than noise.
    fn fresh_day() -> CalendarDate {
        CalendarDate::from_ymd(2026, 8, 8)
    }

    /// The pinned table, which every arithmetic test computes against.
    fn rates() -> RateTable {
        pinned_rates()
    }

    /// One phase's amount, asserting it was priced at all.
    fn priced(report: &CostReport, phase: CostPhase) -> Decimal {
        let items: Vec<&LineItem> = report.by_phase(phase).collect();
        assert_eq!(items.len(), 1, "{phase} should have exactly one line item");
        items[0]
            .amount
            .estimate()
            .unwrap_or_else(|| panic!("{phase} should be priced"))
            .amount()
    }

    /// A report of one running phase, which is the shape most arithmetic tests want.
    fn running_report(size: SizeClass, duration: DurationP) -> CostReport {
        run_report(
            size,
            &RunUsage {
                running: Some(duration),
                launched: false,
                ..RunUsage::default()
            },
            &rates(),
            fresh_day(),
            "run",
        )
        .expect("a running phase at a documented size prices")
    }

    // -- the rate table --------------------------------------------------------

    /// Every rate as a literal, transcribed from `docs/PLATFORM.md`, "What actually
    /// costs money", us-east-1. Asserted literally rather than computed, because the
    /// whole value of pinning a rate table is that it can be diffed against the page
    /// it came from — and against the Python client's own copy of these literals in
    /// `test_cost.py`.
    ///
    /// `storage_gb_month` is the one derived value: the Pricing API quotes snapshot
    /// storage per GB-hour, and $0.0001111111 x 730 is $0.0811111030. It read 0.08
    /// until 2026-08-07, which was 1.37% low.
    #[test]
    fn every_rate_byte_matches_the_python_literal() {
        let rates = rates();
        assert_eq!(rates.region(), &Region::UsEast1);
        assert_eq!(rates.vcpu_second(), dec!(0.0000276944));
        assert_eq!(rates.gb_second(), dec!(0.0000036667));
        assert_eq!(rates.storage_gb_month(), dec!(0.0811111030));
        assert_eq!(rates.snapshot_read_gb(), dec!(0.00155));
        assert_eq!(rates.snapshot_write_gb(), dec!(0.0038));
        assert_eq!(rates.minimum_retention(), Duration::from_secs(604_800));
        assert_eq!(rates.retrieved(), CalendarDate::from_ymd(2026, 8, 7));
        assert!(rates.source_url().contains("aws.amazon.com"));
        // Scale as well as value: `0.0038` and `0.00380000` are numerically equal to
        // `Decimal` but not the same transcription, and the point of this test is that
        // it is a transcription check.
        assert_eq!(rates.snapshot_write_gb().to_string(), "0.0038");
        assert_eq!(rates.storage_gb_month().to_string(), "0.0811111030");
    }

    /// The storage rate is the hourly figure times a 730-hour month, asserted through
    /// [`HOURS_PER_MONTH`] rather than against a literal — the defect it replaced was
    /// a second month convention, not a typo.
    #[test]
    fn the_storage_rate_is_the_hourly_figure_times_a_seven_hundred_thirty_hour_month() {
        assert_eq!(SECONDS_PER_MONTH / dec!(3600), HOURS_PER_MONTH);
        assert_eq!(HOURS_PER_MONTH, dec!(730));
        assert_eq!(rates().storage_gb_month(), dec!(0.0001111111) * dec!(730));
        assert_ne!(
            rates().storage_gb_month(),
            dec!(0.08),
            "the old hand-read value was 1.37% low"
        );
    }

    /// Four arithmetic mistakes someone would otherwise make by analogy with Lambda
    /// Functions: one blended GB-second, a per-invoke charge, a rounded-up billing
    /// increment, and a free tier. `None` for the increment is "not published" —
    /// distinct from one second, which would overcharge every sub-second exec.
    #[test]
    fn the_documented_billing_facts_are_data_not_prose() {
        let rates = rates();
        assert!(rates.bills_vcpu_and_memory_separately());
        assert_eq!(rates.per_request(), Decimal::ZERO);
        assert!(!rates.free_tier());
        assert_eq!(rates.minimum_billing_increment(), None);
    }

    // -- staleness (COST-7) ----------------------------------------------------

    /// COST-7. A silently stale price is the same failure class as a silently stale
    /// schema, which this repo already fails CI on.
    #[test]
    fn a_rate_table_past_ninety_days_attaches_a_warning_to_the_report() {
        let rates = rates();
        let just_past = CalendarDate::from_ymd(2026, 11, 6);
        assert_eq!(just_past.days_since(rates.retrieved()), 91);
        assert!(rates.is_stale(just_past));

        let report = run_report(
            SizeClass::Mib2048,
            &RunUsage {
                running: Some(DurationP::Measured(Duration::from_secs(60))),
                launched: false,
                ..RunUsage::default()
            },
            &rates,
            just_past,
            "run",
        )
        .expect("a stale table still prices");
        let warning = report
            .staleness()
            .expect("a report from a stale table carries the warning");
        assert!(warning.contains("91 days ago"), "{warning}");
        assert!(warning.contains("stale after 90"), "{warning}");
        // The URL travels with it: a figure copied out of a terminal loses everything
        // else, so the warning has to say what to re-read.
        assert!(warning.contains(rates.source_url()), "{warning}");
    }

    /// The complement. A warning that fires on a fresh table is a warning everyone
    /// learns to filter, and then the stale case goes unseen too. Ninety days exactly
    /// is fresh — the Python's comparison is `>`, not `>=`.
    #[test]
    fn a_fresh_rate_table_is_silent_and_ninety_days_exactly_is_still_fresh() {
        let rates = rates();
        let ninety = CalendarDate::from_ymd(2026, 11, 5);
        assert_eq!(ninety.days_since(rates.retrieved()), 90);
        assert!(!rates.is_stale(ninety));
        assert_eq!(rates.staleness(ninety), None);

        let report = running_report(
            SizeClass::Mib2048,
            DurationP::Measured(Duration::from_secs(60)),
        );
        assert_eq!(report.staleness(), None);
    }

    // -- dates -----------------------------------------------------------------

    /// The day-number algorithm against three known values, since every age in this
    /// module is a subtraction of two of them. A hand-rolled date is only acceptable
    /// if it is pinned.
    #[test]
    fn the_day_number_agrees_with_the_gregorian_calendar() {
        assert_eq!(CalendarDate::from_ymd(1970, 1, 1).day_number(), 0);
        // Both verified against Python's `datetime.date` in this session.
        assert_eq!(CalendarDate::from_ymd(2026, 8, 7).day_number(), 20_672);
        assert_eq!(CalendarDate::from_ymd(2024, 2, 29).day_number(), 19_782);
        assert_eq!(CalendarDate::from_ymd(1969, 12, 31).day_number(), -1);
        // A leap year's February really has 29 days in the arithmetic, not just in
        // the validator.
        assert_eq!(
            CalendarDate::from_ymd(2024, 3, 1).days_since(CalendarDate::from_ymd(2024, 2, 28)),
            2
        );
        assert_eq!(
            CalendarDate::from_ymd(2023, 3, 1).days_since(CalendarDate::from_ymd(2023, 2, 28)),
            1
        );
    }

    /// The pinned literals really are calendar days. [`CalendarDate::from_ymd`] is
    /// `const` and therefore cannot validate, so this is where that is made good.
    #[test]
    fn the_pinned_dates_are_real_calendar_days() {
        for (year, month, day) in [(2026, 8, 7), (2026, 8, 8), (2026, 11, 5), (2026, 11, 6)] {
            CalendarDate::try_from_ymd(year, month, day)
                .unwrap_or_else(|err| panic!("{year}-{month}-{day} should be a real day: {err}"));
        }
    }

    /// The S2 boundary refuses what the `const` constructor cannot: `2026-02-30`
    /// would otherwise produce March 2nd's day number and an age two days out.
    #[test]
    fn a_date_that_is_not_a_calendar_day_is_refused() {
        for (year, month, day) in [
            (2026, 2, 30),
            (2026, 13, 1),
            (2026, 0, 1),
            (2026, 1, 0),
            (2026, 1, 32),
            (2023, 2, 29),
        ] {
            let err = CalendarDate::try_from_ymd(year, month, day)
                .expect_err("only calendar days date a rate table");
            assert_eq!(err.kind(), ErrorKind::InvalidArg, "{year}-{month}-{day}");
        }
        assert!(
            CalendarDate::try_from_ymd(2024, 2, 29).is_ok(),
            "a leap day"
        );
    }

    /// `Display` is ISO 8601 because that is what the Python's `retrieved.isoformat()`
    /// puts in the report header, and the two clients' output has to be diffable.
    #[test]
    fn a_date_renders_as_iso_eight_six_oh_one() {
        assert_eq!(CalendarDate::from_ymd(2026, 8, 7).to_string(), "2026-08-07");
        assert_eq!(
            CalendarDate::from_ymd(2026, 12, 31).to_string(),
            "2026-12-31"
        );
    }

    // -- measured versus estimated (COST-1, COST-2) -----------------------------

    /// COST-1, the runtime half. The compile-fail half is the doctests on
    /// [`DurationP`] — this is the part a `match` can see: the label is on the value,
    /// so a consumer can refuse to print an estimate as a receipt.
    #[test]
    fn a_duration_carries_how_it_is_known() {
        let timed = DurationP::Measured(Duration::from_secs(60));
        let planned = DurationP::Projected(Duration::from_secs(60));
        assert_eq!(timed.provenance(), Provenance::Measured);
        assert_eq!(planned.provenance(), Provenance::Projected);
        assert!(timed.is_measured());
        assert!(!planned.is_measured());
        // Same span, different claim. The two are not equal, so a set or a comparison
        // cannot silently treat a projection as a timing.
        assert_ne!(timed, planned);
        assert_eq!(timed.duration(), planned.duration());
        assert_eq!(timed.to_string(), "60s (measured)");
        assert_eq!(planned.to_string(), "60s (projected)");
    }

    /// An inverted clock would silently render as a credit on the report. The Python
    /// checked this in `__post_init__`; here the check is at the f64 boundary, because
    /// past it a negative duration is not representable.
    #[test]
    fn a_negative_or_non_finite_duration_is_refused_at_the_float_boundary() {
        for seconds in [-1.0, -0.001, f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            let err = DurationP::measured_secs_f64(seconds)
                .expect_err("only a finite non-negative float is a duration");
            assert_eq!(err.kind(), ErrorKind::InvalidArg, "{seconds}");
            assert!(
                err.to_string().contains("credit on the report"),
                "must name why: {err}"
            );
        }
        assert_eq!(
            DurationP::measured_secs_f64(60.5)
                .expect("a plain float is a duration")
                .seconds(),
            dec!(60.5)
        );
    }

    /// COST-2's runtime half: the label is in `Display`, because a figure copied out of
    /// a terminal loses its type name.
    #[test]
    fn every_dollar_figure_renders_as_an_estimate() {
        assert_eq!(
            EstimatedUsd::new(dec!(1.23)).to_string(),
            "~$1.23 (estimated)"
        );
        let report = running_report(SizeClass::Mib2048, DurationP::Measured(MONTH));
        let text = report.render();
        assert!(text.contains("estimates derived from published"), "{text}");
        assert!(text.contains("only Cost Explorer knows the bill"), "{text}");
        // The retrieval date travels too, so a pasted figure can be dated.
        assert!(text.contains("2026-08-07"), "{text}");
    }

    /// The asymmetry is the point: we can time a phase exactly and still only infer its
    /// price, so there is no `ActualUsd` for a measured duration to produce.
    #[test]
    fn a_measured_second_still_yields_an_estimated_dollar() {
        let report = running_report(
            SizeClass::Mib2048,
            DurationP::Measured(Duration::from_secs(3600)),
        );
        assert!(report.fully_measured());
        assert!(
            report
                .items()
                .iter()
                .all(|item| item.amount.estimate().is_some())
        );
        assert!(report.total().to_string().contains("estimated"));
    }

    /// The distinction survives into the report rather than stopping at the duration,
    /// which is what lets a consumer refuse to print an estimate as a receipt.
    #[test]
    fn an_estimate_is_never_fully_measured_and_a_timed_run_is() {
        let projected = estimate_run(
            SizeClass::Mib2048,
            &PlanUsage {
                running_seconds: 3600.0,
                ..PlanUsage::default()
            },
            &rates(),
            fresh_day(),
            "estimate",
        )
        .expect("a plan prices");
        assert!(!projected.fully_measured());
        assert!(
            running_report(
                SizeClass::Mib2048,
                DurationP::Measured(Duration::from_secs(3600))
            )
            .fully_measured()
        );
    }

    /// Display precision: sub-cent figures need six places and monthly ones need two,
    /// and one fixed precision cannot show both without lying or shouting. Half-even at
    /// the boundary, matching Python's `quantize` default, so the two clients render
    /// the same figure identically.
    #[test]
    fn a_sub_cent_figure_and_a_monthly_one_each_render_readably() {
        assert_eq!(
            EstimatedUsd::new(dec!(0.0000276944)).amount_string(),
            "0.000028"
        );
        assert_eq!(EstimatedUsd::new(dec!(92.0512345)).amount_string(), "92.05");
        // Exactly one dollar crosses to two places: `>= 1`, as the Python has it.
        assert_eq!(EstimatedUsd::new(Decimal::ONE).amount_string(), "1.00");
        assert_eq!(
            EstimatedUsd::new(dec!(0.999999)).amount_string(),
            "0.999999"
        );
        // Half-even, not half-up: 0.125 rounds down to 0.12 and 0.135 rounds up to
        // 0.14. `Decimal::rescale` would answer 0.13 for the first.
        assert_eq!(EstimatedUsd::new(dec!(1.125)).amount_string(), "1.12");
        assert_eq!(EstimatedUsd::new(dec!(1.135)).amount_string(), "1.14");
        // Trailing zeros are kept: they say the figure was rounded to the cent rather
        // than happening to land there.
        assert_eq!(EstimatedUsd::new(dec!(2)).amount_string(), "2.00");
    }

    // -- unknown is not zero (COST-3, COST-4) ----------------------------------

    /// COST-3. AWS does not publish whether the server-side build is billed as
    /// compute. The build starts a real MicroVM, so reporting $0.00 would understate
    /// the run in the direction that flatters us.
    #[test]
    fn the_image_build_is_unpriced_rather_than_free() {
        let report = run_report(
            SizeClass::Mib2048,
            &RunUsage {
                image_gb: Some(2.0),
                image_build: Some(DurationP::Measured(Duration::from_secs(300))),
                ..RunUsage::launched()
            },
            &rates(),
            fresh_day(),
            "run",
        )
        .expect("a build prices as unpriced");
        let build: Vec<&LineItem> = report.by_phase(CostPhase::ImageBuild).collect();
        assert_eq!(build.len(), 1);
        let reason = build[0]
            .amount
            .unpriced_reason()
            .expect("the build has no published rate");
        assert!(reason.contains("does not publish"), "{reason}");
        assert_eq!(
            build[0].line, None,
            "no billing line exists to attribute it to"
        );
        // The distinction this requirement is about: not zero dollars.
        assert_eq!(build[0].amount.estimate(), None);
        assert_ne!(
            build[0].amount,
            Amount::Estimated(EstimatedUsd::ZERO),
            "unpriced is a claim about the documentation, zero is a claim about the bill"
        );
        assert_eq!(build[0].amount.kind(), "unpriced");
    }

    /// COST-4. A plain sum cannot express "everything priceable, plus a build whose
    /// price AWS withholds", and the natural way to force it to is to drop the line.
    /// The `AtLeast` variant is what makes dropping it unrepresentable.
    #[test]
    fn an_unpriced_line_makes_the_total_a_lower_bound_that_names_it() {
        let report = run_report(
            SizeClass::Mib2048,
            &RunUsage {
                image_gb: Some(2.0),
                image_build: Some(DurationP::Measured(Duration::from_secs(300))),
                ..RunUsage::launched()
            },
            &rates(),
            fresh_day(),
            "run",
        )
        .expect("a build prices");
        assert!(!report.is_complete());
        let total = report.total();
        assert!(total.is_lower_bound());
        assert!(matches!(total, Total::AtLeast { .. }));
        // The reasons are named, not merely counted (COST-4's "naming its unpriced
        // line items").
        assert_eq!(total.unpriced_reasons().len(), 1);
        assert!(total.unpriced_reasons()[0].contains("does not publish"));
        let rendered = total.to_string();
        assert!(rendered.starts_with("at least "), "{rendered}");
        assert!(rendered.contains("1 unpriced"), "{rendered}");

        // The floor is the sum of the priced lines and nothing else: the unpriced line
        // is neither summed as zero nor allowed to poison the figure.
        let expected: EstimatedUsd = report
            .priced()
            .map(|item| item.amount.estimate().expect("priced"))
            .sum();
        assert_eq!(total.floor(), expected);
    }

    /// The rendered lower bound is the oracle's string, character for character.
    ///
    /// The Python is the contract here and the two divergences it catches are the ones a
    /// re-derivation would not: the parenthetical names **phases** where the Rust used to
    /// join reasons — a hundred-and-ninety-character sentence about what AWS does not
    /// publish, in the field a reader scans for "which phase" — and the separator is
    /// `", "` rather than `"; "`.
    ///
    /// The expected string is copied from the oracle rather than assembled here. What its
    /// `run_report` printed for a 2048 MiB size with 3600 s running, a 300 s measured image
    /// build, and a 2 GB image:
    ///
    /// ```text
    /// at least ~$0.166533 (estimated), plus 1 unpriced (image-build)
    /// ```
    ///
    /// A transcript rather than a command: that client was deleted once this one had driven
    /// the live suite green, and git history is where the code behind the figure lives.
    ///
    /// **Falsification** — restore `unpriced.join("; ")` over the reasons in
    /// [`Total`]'s `Display` and this is red on the whole parenthetical while every
    /// figure, count, and `isLowerBound` still agrees. Verified.
    #[test]
    fn the_rendered_lower_bound_is_the_oracles_string_verbatim() {
        let report = run_report(
            SizeClass::Mib2048,
            &RunUsage {
                running: Some(DurationP::Measured(Duration::from_secs(3600))),
                image_build: Some(DurationP::Measured(Duration::from_secs(300))),
                image_gb: Some(2.0),
                ..RunUsage::launched()
            },
            &rates(),
            fresh_day(),
            "run",
        )
        .expect("a build prices");
        assert_eq!(
            report.total().to_string(),
            "at least ~$0.166533 (estimated), plus 1 unpriced (image-build)"
        );

        // Two unpriced lines in the same phase name it once, and two phases read sorted
        // rather than in report order — `sorted({...})` in the oracle is a set.
        let amounts = [
            (CostPhase::Running, Amount::unpriced("second")),
            (CostPhase::ImageBuild, Amount::unpriced("first")),
            (CostPhase::ImageBuild, Amount::unpriced("also first")),
        ];
        let total = Total::of(amounts.iter().map(|(phase, amount)| (*phase, amount)));
        assert_eq!(total.unpriced_phase_names(), ["image-build", "running"]);
        assert_eq!(
            total.to_string(),
            "at least ~$0.000000 (estimated), plus 3 unpriced (image-build, running)",
            "three lines, two names — the count is lines and the parenthetical is phases"
        );
        // The reasons are still reachable, in report order, for the consumer that wants
        // the paragraph the total has no room for.
        assert_eq!(total.unpriced_reasons(), ["second", "first", "also first"]);
    }

    /// **Every phase round-trips through its own spelling**, and nothing else parses.
    ///
    /// The forward direction is the one that matters for the bindings: `by_phase("running")`
    /// has to find the same variant the report attributed the line to, so `from_str` must be
    /// the exact inverse of `as_str` for all seven. The reverse — an unknown spelling — is
    /// refused *with the whole list*, because the failure this replaces was a Python caller
    /// typing `"suspending"` and getting an empty result set back rather than an error.
    ///
    /// `ALL` is asserted to be complete by exhaustion rather than by a length check: the
    /// `match` below has no wildcard arm, so adding a variant to [`CostPhase`] fails to
    /// compile here until it is added to `ALL` too. A `len() == 7` assertion would only have
    /// caught the same mistake after someone changed the number.
    ///
    /// **Falsification** — drop `CostPhase::Suspend` from `ALL` and the exhaustive match
    /// leaves it unreachable, so the round-trip assertion goes red on `"suspend"`; return
    /// `Ok(CostPhase::Running)` for an unknown string and the refusal assertion goes red.
    /// Verified; see the report at the end of this task.
    #[test]
    fn every_phase_parses_back_from_its_display_and_nothing_else_does() {
        for phase in CostPhase::ALL {
            // Both spellings, because `Display` pads and `as_str` does not — a `from_str`
            // written against the padded form would fail on every column-formatted string a
            // caller copied out of a report.
            assert_eq!(
                phase.as_str().parse::<CostPhase>().expect("a known phase"),
                phase,
                "{} did not round-trip through as_str",
                phase.as_str()
            );
            assert_eq!(
                phase
                    .to_string()
                    .parse::<CostPhase>()
                    .expect("a known phase"),
                phase,
                "{phase} did not round-trip through Display"
            );
            // And the exhaustive match: a variant added to the enum but not to `ALL` fails
            // to compile at this arm rather than silently going unparseable.
            match phase {
                CostPhase::ImageBuild
                | CostPhase::ImageStorage
                | CostPhase::Launch
                | CostPhase::Running
                | CostPhase::Suspended
                | CostPhase::Suspend
                | CostPhase::Resume => {}
            }
        }
        assert_eq!(
            CostPhase::ALL.len(),
            7,
            "the seven phases the billing model has"
        );

        // The near-misses a caller actually types: a lifecycle state that is not a cost
        // phase, the underscore spelling, and the empty string.
        for wrong in ["suspending", "image_build", "RUNNING", "", "total"] {
            let error = wrong
                .parse::<CostPhase>()
                .expect_err("only the seven spellings parse");
            assert_eq!(error.kind(), ErrorKind::InvalidArg);
            let message = error.to_string();
            for phase in CostPhase::ALL {
                assert!(
                    message.contains(phase.as_str()),
                    "the refusal has to offer every phase so a typo is self-correcting, \
                     {phase} missing from: {message}"
                );
            }
        }
    }

    /// Every rendered line's amount starts at the same column.
    ///
    /// The plain-text report is a table, and the phase column is what aligns it. `{:<14}`
    /// in [`LineItem`]'s `Display` is not enough on its own: a `Display` impl written as
    /// `f.write_str(...)` ignores the formatter's width outright, so the format string
    /// reads as padded while the output is ragged — `image-build` and `launch` differ by
    /// six characters and the dollar figures walk. The oracle's own render puts the
    /// amount at column 44 on all seven lines of this same report, checked with
    /// `line.index('~$')` on `cost.py`'s `CostReport.render()`.
    ///
    /// The assertion is that the columns *agree* rather than that they equal 44, because
    /// the widths belong to `LineItem` and a deliberate change there should not have to
    /// come here — but a phase that stops padding is not a deliberate change.
    ///
    /// **Falsification** — put `f.write_str(self.as_str())` back in [`CostPhase`]'s
    /// `Display` and this is red with two distinct offsets. Verified.
    #[test]
    fn every_rendered_line_puts_its_amount_in_the_same_column() {
        let report = run_report(
            SizeClass::Mib2048,
            &RunUsage {
                running: Some(DurationP::Measured(Duration::from_secs(3600))),
                image_build: Some(DurationP::Measured(Duration::from_secs(300))),
                image_gb: Some(2.0),
                suspend_resume_cycles: 1,
                ..RunUsage::launched()
            },
            &rates(),
            fresh_day(),
            "run",
        )
        .expect("a report");
        let text = report.render();
        // The phases whose names differ most: `image-storage` is thirteen characters and
        // `launch` is six, so an unpadded phase column shows up as a seven-column shift.
        let offsets: Vec<(usize, &str)> = text
            .lines()
            .filter(|line| line.starts_with("  "))
            .map(|line| {
                let amount = line
                    .find("~$")
                    .or_else(|| line.find("unpriced"))
                    .unwrap_or_else(|| panic!("every line has an amount: {line}"));
                (amount, line)
            })
            .collect();
        assert!(offsets.len() >= 7, "{text}");
        let (first, first_line) = offsets[0];
        for (offset, line) in &offsets {
            assert_eq!(
                *offset, first,
                "the amount column has to agree across lines:\n{first_line}\n{line}"
            );
        }
        // And the phase is genuinely padded rather than the widths happening to cancel.
        assert_eq!(
            format!("{:<14}|", CostPhase::Launch),
            "launch        |",
            "a phase in a width-bearing format slot has to pad"
        );
    }

    /// A lower bound that will not say what it is missing is not a value [`Total`]
    /// holds: an empty reason list routes to [`Total::Exact`].
    #[test]
    fn a_total_over_only_priced_lines_is_exact_and_names_nothing() {
        let amounts = [Amount::estimated(dec!(1)), Amount::estimated(dec!(2))];
        let total = Total::of(amounts.iter().map(|amount| (CostPhase::Running, amount)));
        assert_eq!(total, Total::Exact(EstimatedUsd::new(dec!(3))));
        assert!(!total.is_lower_bound());
        assert!(total.unpriced_reasons().is_empty());
        assert_eq!(total.to_string(), "~$3.00 (estimated)");
        // The degenerate case: no lines at all is exactly zero, not a lower bound.
        assert_eq!(
            Total::of(std::iter::empty()),
            Total::Exact(EstimatedUsd::ZERO)
        );
    }

    /// The complement of the build test: a run against an existing image pays no build,
    /// so incompleteness is a property of the phases present rather than a permanent
    /// disclaimer nobody reads.
    #[test]
    fn a_report_with_no_image_is_complete() {
        let report = running_report(
            SizeClass::Mib2048,
            DurationP::Measured(Duration::from_secs(60)),
        );
        assert!(report.is_complete());
        assert!(!report.total().is_lower_bound());
        assert!(matches!(report.total(), Total::Exact(_)));
    }

    // -- per-phase attribution (COST-5, COST-8) ---------------------------------

    /// Two entries rather than one blended GB-second, because that is how the pricing
    /// page prices them and a blended figure cannot be reconciled against a Cost
    /// Explorer breakdown that keeps them apart.
    #[test]
    fn running_bills_vcpu_and_memory_as_two_separate_lines() {
        let report = running_report(
            SizeClass::Mib2048,
            DurationP::Measured(Duration::from_secs(3600)),
        );
        let lines: Vec<Option<BillingLine>> = report
            .by_phase(CostPhase::Running)
            .map(|item| item.line)
            .collect();
        assert_eq!(lines, [Some(BillingLine::Vcpu), Some(BillingLine::Memory)]);
    }

    /// COST-5. `sizing` locks that billing follows the baseline; this locks that cost
    /// obeys it. The 2 GB class reports 8 GB in the guest, so reading the peak would
    /// overstate the memory line exactly 4x — and the assertion below is against the
    /// baseline figure spelled out, so a peak-reading implementation is off by that 4x
    /// rather than merely different.
    #[test]
    fn running_bills_the_baseline_and_never_the_peak() {
        let size = SizeClass::Mib2048;
        let report = running_report(size, DurationP::Measured(Duration::from_secs(3600)));
        let rates = rates();

        let memory: Vec<&LineItem> = report
            .by_phase(CostPhase::Running)
            .filter(|item| item.line == Some(BillingLine::Memory))
            .collect();
        assert_eq!(
            memory[0].amount.estimate().expect("priced").amount(),
            dec!(2) * dec!(3600) * rates.gb_second()
        );
        let vcpu: Vec<&LineItem> = report
            .by_phase(CostPhase::Running)
            .filter(|item| item.line == Some(BillingLine::Vcpu))
            .collect();
        assert_eq!(
            vcpu[0].amount.estimate().expect("priced").amount(),
            dec!(1) * dec!(3600) * rates.vcpu_second()
        );

        // Named explicitly, so the failure message says which figure was read: the
        // peak is 8 GB / 4 vCPU for this class.
        assert_eq!(memory[0].quantity, dec!(2) * dec!(3600));
        assert_ne!(
            memory[0].quantity,
            gb_decimal(size.peak_gb()).expect("a documented peak") * dec!(3600),
            "the memory line must read baseline_gb, not peak_gb"
        );
        assert_ne!(
            vcpu[0].quantity,
            gb_decimal(size.peak_vcpu()).expect("a documented peak") * dec!(3600),
            "the vcpu line must read baseline_vcpu, not peak_vcpu"
        );
    }

    /// COST-5 across the whole table, since one class cannot distinguish a baseline
    /// read from a peak read that happens to agree. No shipped class's baseline equals
    /// another's peak in a way that would let a peak-reading implementation pass.
    #[test]
    fn every_size_class_bills_its_own_baseline() {
        let rates = rates();
        for size in SizeClass::ALL {
            let report = running_report(size, DurationP::Measured(Duration::from_secs(1)));
            let memory: Vec<&LineItem> = report
                .by_phase(CostPhase::Running)
                .filter(|item| item.line == Some(BillingLine::Memory))
                .collect();
            let expected = gb_decimal(size.baseline_gb()).expect("a documented baseline");
            assert_eq!(memory[0].quantity, expected, "{size:?}");
            assert_eq!(
                memory[0].amount.estimate().expect("priced").amount(),
                expected * rates.gb_second(),
                "{size:?}"
            );
        }
    }

    /// `docs/PLATFORM.md`: "roughly $100 a month to leave the same VM running at
    /// baseline". An order-of-magnitude anchor on the compute rates, independent of the
    /// exact rate literals above — the two would have to be wrong together.
    #[test]
    fn a_month_of_running_costs_about_a_hundred_dollars() {
        let report = running_report(SizeClass::Mib2048, DurationP::Projected(MONTH));
        let total = report.total().floor().amount();
        assert!(
            dec!(80) < total && total < dec!(120),
            "a month at baseline should be roughly $100, got {total}"
        );
    }

    /// A suspended guest is frozen rather than stopped, so there is no compute line at
    /// all — not a compute line multiplied by zero, which would reappear the moment
    /// someone changed how a duration is derived.
    #[test]
    fn a_suspended_vm_pays_storage_and_no_compute() {
        let report = run_report(
            SizeClass::Mib2048,
            &RunUsage {
                suspended: Some(DurationP::Measured(MONTH)),
                launched: false,
                ..RunUsage::default()
            },
            &rates(),
            fresh_day(),
            "suspended",
        )
        .expect("a suspended hold prices");
        let lines: Vec<Option<BillingLine>> = report
            .by_phase(CostPhase::Suspended)
            .map(|item| item.line)
            .collect();
        assert_eq!(lines, [Some(BillingLine::SnapshotStorage)]);
        assert!(
            !report
                .items()
                .iter()
                .any(|item| item.line == Some(BillingLine::Vcpu)
                    || item.line == Some(BillingLine::Memory)),
            "a frozen VM has no compute line at all"
        );
        // "about $0.16 a month" for a suspended 2 GB VM.
        let total = report.total().floor().amount();
        assert!(dec!(0.10) < total && total < dec!(0.25), "{total}");
    }

    /// COST-8. The line item that dominates a create-and-destroy suite.
    /// `docs/PLATFORM.md`: a 2 GB image deleted sixty seconds after creation still bills
    /// about a week, "roughly four cents". Not applying the minimum understates the
    /// floor by four orders of magnitude and makes the compute look like the cost
    /// driver.
    #[test]
    fn image_storage_bills_the_one_week_minimum_for_a_sixty_second_image() {
        let rates = rates();
        let report = run_report(
            SizeClass::Mib2048,
            &RunUsage {
                image_gb: Some(2.0),
                image_build: Some(DurationP::Measured(Duration::from_secs(300))),
                image_retained: Some(DurationP::Measured(Duration::from_secs(60))),
                launched: false,
                ..RunUsage::default()
            },
            &rates,
            fresh_day(),
            "build",
        )
        .expect("a held image prices");
        let week = dec!(604800);
        assert_eq!(
            priced(&report, CostPhase::ImageStorage),
            dec!(2) * week / SECONDS_PER_MONTH * rates.storage_gb_month()
        );
        let amount = priced(&report, CostPhase::ImageStorage);
        assert!(
            dec!(0.03) < amount && amount < dec!(0.05),
            "roughly four cents, got {amount}"
        );
        // The floor must be visible, not just applied: a reader checking the quantity
        // column against the rate table needs to know why it says a week.
        let storage: Vec<&LineItem> = report.by_phase(CostPhase::ImageStorage).collect();
        assert!(
            storage[0].note.contains("minimum retention"),
            "{}",
            storage[0].note
        );
        assert!(storage[0].note.contains("604800s"), "{}", storage[0].note);
        // And the duration on the line is still the *measured* sixty seconds — the
        // floor changes what is billed, not what was observed.
        assert_eq!(
            storage[0].duration,
            Some(DurationP::Measured(Duration::from_secs(60)))
        );
    }

    /// The note's day count is the rate row's own, not a division written beside the
    /// message.
    ///
    /// The two halves are one requirement: `minimum_retention_days` reports what
    /// `minimum_retention` holds, and the note quotes *that* rather than a second
    /// convention. Asserted against a table whose floor is deliberately **not** a week,
    /// which is the only input that separates the field from the literal — a hand-rolled
    /// `as_secs() / 86_400` at the call site passes the seven-day case and then keeps
    /// saying "7-day" here.
    ///
    /// **Falsification** — put `rates.minimum_retention().as_secs() / 86_400` back in
    /// `storage_line` and this stays green; hard-code either `7` or the week's seconds and
    /// it goes red naming the wrong day count. Verified: replacing the accessor's body
    /// with `7` fails the first assertion, and writing `"; billed 7-day minimum retention
    /// ({floor}s) instead"` fails the note assertion with `7-day` against a 14-day row.
    #[test]
    fn the_retention_note_reads_its_day_count_off_the_rate_row() {
        let week = rates();
        assert_eq!(week.minimum_retention_days(), 7);
        assert_eq!(
            week.minimum_retention(),
            Duration::from_secs(week.minimum_retention_days() * 24 * 60 * 60),
            "the days accessor and the duration must describe the same window"
        );

        // A fortnight, which no rate table ships — the point is that nothing but the
        // field decides what the note says.
        let mut fortnight = week.clone();
        fortnight.minimum_retention = Duration::from_secs(14 * 24 * 60 * 60);
        assert_eq!(fortnight.minimum_retention_days(), 14);

        let item = storage_line(
            CostPhase::ImageStorage,
            dec!(2),
            DurationP::Measured(Duration::from_secs(60)),
            &fortnight,
        )
        .expect("a floored hold prices");
        assert!(
            item.note.contains("14-day minimum retention"),
            "the note must quote the row's own window: {}",
            item.note
        );
        assert!(
            !item.note.contains("7-day"),
            "a second day-count convention survived in the message: {}",
            item.note
        );
    }

    /// The floor is a floor, not a flat fee. A month-long hold that billed one week
    /// would understate a warm pool's storage by more than 4x.
    #[test]
    fn a_long_hold_bills_actual_time_rather_than_the_minimum() {
        let rates = rates();
        let report = run_report(
            SizeClass::Mib2048,
            &RunUsage {
                image_gb: Some(2.0),
                image_retained: Some(DurationP::Measured(MONTH)),
                launched: false,
                ..RunUsage::default()
            },
            &rates,
            fresh_day(),
            "build",
        )
        .expect("a long hold prices");
        assert_eq!(
            priced(&report, CostPhase::ImageStorage),
            dec!(2) * rates.storage_gb_month(),
        );
        let storage: Vec<&LineItem> = report.by_phase(CostPhase::ImageStorage).collect();
        assert!(
            !storage[0].note.contains("minimum retention"),
            "the note must not claim a floor that was not applied: {}",
            storage[0].note
        );
    }

    /// The floor applies to a suspend snapshot too, because the minimum is on the rate
    /// row rather than on the image phase. `docs/PLATFORM.md` demonstrates it only for
    /// images, which is what the note distinguishes.
    #[test]
    fn a_briefly_held_suspend_snapshot_bills_the_same_floor() {
        let report = run_report(
            SizeClass::Mib2048,
            &RunUsage {
                suspended: Some(DurationP::Measured(Duration::from_secs(30))),
                launched: false,
                ..RunUsage::default()
            },
            &rates(),
            fresh_day(),
            "suspended",
        )
        .expect("a brief suspension prices");
        let items: Vec<&LineItem> = report.by_phase(CostPhase::Suspended).collect();
        assert!(
            items[0].note.contains("minimum retention"),
            "{}",
            items[0].note
        );
        assert_eq!(
            priced(&report, CostPhase::Suspended),
            dec!(2) * dec!(604800) / SECONDS_PER_MONTH * rates().storage_gb_month(),
        );
    }

    /// Asymmetric rates — the write is ~2.5x the read — so a cycle costed with one rate
    /// twice is wrong in whichever direction it picked.
    #[test]
    fn a_suspend_resume_cycle_bills_a_write_and_a_read() {
        let rates = rates();
        let report = run_report(
            SizeClass::Mib2048,
            &RunUsage {
                suspend_resume_cycles: 1,
                launched: false,
                ..RunUsage::default()
            },
            &rates,
            fresh_day(),
            "cycle",
        )
        .expect("a cycle prices");
        assert_eq!(
            priced(&report, CostPhase::Suspend),
            dec!(2) * rates.snapshot_write_gb()
        );
        assert_eq!(
            priced(&report, CostPhase::Resume),
            dec!(2) * rates.snapshot_read_gb()
        );
        assert_ne!(
            rates.snapshot_write_gb(),
            rates.snapshot_read_gb(),
            "the two rates differ, which is why the two lines must not share one"
        );
        // "about $0.011 for a 2 GB VM" per cycle.
        let total = report.total().floor().amount();
        assert!(dec!(0.010) < total && total < dec!(0.012), "{total}");
    }

    /// The churn argument only works if churn actually accumulates.
    #[test]
    fn cycles_scale_the_transition_cost() {
        let cycles_cost = |count: u32| {
            run_report(
                SizeClass::Mib2048,
                &RunUsage {
                    suspend_resume_cycles: count,
                    launched: false,
                    ..RunUsage::default()
                },
                &rates(),
                fresh_day(),
                "cycle",
            )
            .expect("a cycle prices")
            .total()
            .floor()
            .amount()
        };
        assert_eq!(cycles_cost(10), cycles_cost(1) * dec!(10));
        // Zero cycles is zero lines, not a zero-priced line: nothing happened.
        assert_eq!(cycles_cost(0), Decimal::ZERO);
    }

    /// A smaller baseline is a real cost lever, which is what makes the CLI's
    /// adequate-over-cheap default a deliberate trade rather than an oversight. 512 MiB
    /// to 8192 MiB is 16x on both vCPU and memory.
    #[test]
    fn a_smaller_baseline_is_a_real_cost_lever() {
        let small = running_report(SizeClass::Mib512, DurationP::Projected(MONTH))
            .total()
            .floor()
            .amount();
        let large = running_report(SizeClass::Mib8192, DurationP::Projected(MONTH))
            .total()
            .floor()
            .amount();
        assert_eq!(large, small * dec!(16));
    }

    // -- estimate mode (COST-10) ------------------------------------------------

    /// COST-10. An estimate is spent before the money is, so nothing in it can claim to
    /// have been timed.
    #[test]
    fn an_estimate_labels_every_duration_projected() {
        let report = estimate_run(
            SizeClass::Mib2048,
            &PlanUsage {
                running_seconds: 3600.0,
                suspended_seconds: 600.0,
                image_gb: Some(2.0),
                ..PlanUsage::launched()
            },
            &rates(),
            fresh_day(),
            "estimate",
        )
        .expect("a plan prices");
        let durations: Vec<DurationP> = report
            .items()
            .iter()
            .filter_map(|item| item.duration)
            .collect();
        assert!(
            !durations.is_empty(),
            "the estimate must attribute durations, not just totals"
        );
        for duration in durations {
            assert_eq!(
                duration.provenance(),
                Provenance::Projected,
                "every duration in a plan is projected"
            );
        }
        assert!(!report.fully_measured());
    }

    /// Same rates, same phases: only the labels differ. If the two paths could disagree
    /// numerically, the label would be hiding a second implementation.
    #[test]
    fn an_estimate_and_a_measured_run_agree_on_the_arithmetic() {
        let projected = estimate_run(
            SizeClass::Mib2048,
            &PlanUsage {
                running_seconds: 3600.0,
                ..PlanUsage::default()
            },
            &rates(),
            fresh_day(),
            "estimate",
        )
        .expect("a plan prices");
        let measured = running_report(
            SizeClass::Mib2048,
            DurationP::Measured(Duration::from_secs(3600)),
        );
        assert_eq!(projected.total(), measured.total());
        // The line items agree on everything except the label, which is the claim.
        let strip = |report: &CostReport| -> Vec<(CostPhase, Option<BillingLine>, Decimal)> {
            report
                .items()
                .iter()
                .map(|item| (item.phase, item.line, item.quantity))
                .collect()
        };
        assert_eq!(strip(&projected), strip(&measured));
        assert!(!projected.fully_measured() && measured.fully_measured());
    }

    /// A plan's fields are seconds, so there is no field a `Measured` duration could be
    /// written into — and a zero-length phase is an absent phase rather than a line
    /// claiming a measurement nobody took.
    #[test]
    fn a_plan_with_no_phases_has_no_durations_at_all() {
        let report = estimate_run(
            SizeClass::Mib2048,
            &PlanUsage::default(),
            &rates(),
            fresh_day(),
            "estimate",
        )
        .expect("an empty plan prices");
        assert!(report.items().is_empty());
        assert_eq!(report.total(), Total::Exact(EstimatedUsd::ZERO));
        // `fully_measured` is false for a report with no durations, not vacuously true
        // — an empty report must not read as a receipt.
        assert!(!report.fully_measured());
    }

    /// An image on a plan still drags the unpriced build line in, because an image that
    /// has storage cost has to be built and the build's price is unknown. So a
    /// create-and-destroy estimate is never complete either.
    #[test]
    fn an_estimate_with_an_image_is_a_lower_bound_too() {
        let report = estimate_run(
            SizeClass::Mib2048,
            &PlanUsage {
                image_gb: Some(2.0),
                ..PlanUsage::launched()
            },
            &rates(),
            fresh_day(),
            "estimate",
        )
        .expect("a plan with an image prices");
        assert!(!report.is_complete());
        assert!(report.total().is_lower_bound());
        // The build line has no duration: a plan has not built anything, so there is
        // nothing to have timed, and inventing a projected build time would be a
        // number with no source.
        let build: Vec<&LineItem> = report.by_phase(CostPhase::ImageBuild).collect();
        assert_eq!(build[0].duration, None);
        assert_eq!(build[0].unit, "seconds (untimed)");
    }

    /// A cost figure for a size the platform would refuse never gets produced — it would
    /// look like an answer. The refusal is `sizing`'s (TRAP-10) and this is the seam
    /// showing it is reachable from here.
    #[test]
    fn an_off_table_baseline_never_reaches_a_cost_figure() {
        let err =
            SizeClass::from_baseline_mib(1500).expect_err("1500 MiB is not a documented baseline");
        assert_eq!(err.kind(), ErrorKind::InvalidArg);
        assert!(
            err.to_string()
                .contains("not a documented size class baseline")
        );
        // And the accepted case really does select the class the caller named.
        let size = SizeClass::from_baseline_mib(1024).expect("1024 MiB is documented");
        assert_eq!(size.baseline_gb(), 1.0);
    }

    /// A size a caller supplies as a float still has to be a number. The one f64
    /// boundary is fallible for this reason (COST-6).
    #[test]
    fn a_non_finite_or_negative_size_is_refused_at_the_float_boundary() {
        for gb in [-1.0, f64::NAN, f64::INFINITY] {
            let err = gb_decimal(gb).expect_err("only a finite non-negative size prices");
            assert_eq!(err.kind(), ErrorKind::InvalidArg, "{gb}");
        }
        // A magnitude no decimal can hold is refused rather than saturated: a saturated
        // figure is a plausible-looking number, which is the only kind nobody checks.
        assert!(gb_decimal(1e30).is_err());
        // And the boundary is exact where it succeeds — via the decimal string, not the
        // binary value, so 0.1 is 0.1 and not 0.1000000000000000055511151231257827.
        assert_eq!(gb_decimal(0.1).expect("0.1 is representable"), dec!(0.1));
        assert_eq!(gb_decimal(0.25).expect("0.25 is representable"), dec!(0.25));
    }

    // -- the ARM-only rule (COST-9) --------------------------------------------

    /// The catalog as the Pricing API returned it on 2026-08-07 for us-east-1: seven
    /// MicroVM line items, including the two x86 compute rates a MicroVM can never be
    /// billed at. They are here precisely so a test can prove we do not reach for them.
    fn recorded_catalog() -> RateCatalog {
        RateCatalog::new()
            .with_entry(
                "AWS-Lambda-MicroVM-vCPU-Second-ARM",
                "vCPU-Seconds",
                dec!(0.0000276944),
            )
            .with_entry(
                "AWS-Lambda-MicroVM-vCPU-Second",
                "vCPU-Seconds",
                dec!(0.0000326557),
            )
            .with_entry(
                "AWS-Lambda-MicroVM-Memory-GB-Second-ARM",
                "GB-Seconds",
                dec!(0.0000036667),
            )
            .with_entry(
                "AWS-Lambda-MicroVM-Memory-GB-Second",
                "GB-Seconds",
                dec!(0.0000043235),
            )
            .with_entry(
                "AWS-Lambda-MicroVM-Snapshot-Storage-GB-Hour",
                "GB-Hours",
                dec!(0.0001111111),
            )
            .with_entry(
                "AWS-Lambda-MicroVM-Snapshot-Read-GB",
                "GB",
                dec!(0.0015467699),
            )
            .with_entry(
                "AWS-Lambda-MicroVM-Snapshot-Write-GB",
                "GB",
                dec!(0.0037977138),
            )
    }

    /// The same catalog without one group, for the refusal cases.
    fn without(group: &str) -> RateCatalog {
        let mut catalog = RateCatalog::new();
        for line in CatalogLine::ALL {
            for candidate in [line.group()].into_iter().chain(line.x86_group()) {
                if candidate == group {
                    continue;
                }
                let entry = recorded_catalog()
                    .matching(candidate)
                    .first()
                    .map(|entry| (*entry).clone());
                if let Some(entry) = entry {
                    catalog = catalog.with_entry(entry.group, entry.unit, entry.usd);
                }
            }
        }
        catalog
    }

    /// The recorded catalog fills a table, and the pinned rates agree with it within the
    /// rounding of the pinned figures. This is the offline half of `mise run live:rates`
    /// — a hand-edit to [`pinned_rates`] fails here rather than waiting for someone to
    /// have credentials.
    #[test]
    fn the_recorded_catalog_fills_a_table_that_matches_the_pinned_one() {
        let fetched = RateTable::from_catalog(
            Region::UsEast1,
            "aws pricing api",
            CalendarDate::from_ymd(2026, 8, 7),
            &recorded_catalog(),
        )
        .expect("the recorded catalog is complete");
        // The compute rates are exact: the pinned table holds the API's own figures.
        assert_eq!(fetched.vcpu_second(), rates().vcpu_second());
        assert_eq!(fetched.gb_second(), rates().gb_second());
        // Storage went through the GB-hour to GB-month conversion and lands exactly on
        // the pinned literal, which is what makes that literal checkable offline.
        assert_eq!(fetched.storage_gb_month(), rates().storage_gb_month());
        // The snapshot rates are pinned as three-significant-figure roundings, so they
        // agree within the 0.5% drift tolerance rather than exactly.
        for (fetched, pinned) in [
            (fetched.snapshot_read_gb(), rates().snapshot_read_gb()),
            (fetched.snapshot_write_gb(), rates().snapshot_write_gb()),
        ] {
            let relative = (fetched - pinned).abs() / pinned;
            assert!(relative < dec!(0.005), "{fetched} vs {pinned}");
            assert_ne!(fetched, pinned, "the pinned figure really is a rounding");
        }
        // A fetched table is authoritative on rates and still hand-read on rules.
        assert_eq!(fetched.minimum_retention(), MINIMUM_RETENTION);
    }

    /// COST-9, the falsification. The one substitution that would look entirely
    /// healthy: the x86 rate parses, is the same shape, and is 17.9% higher. Every
    /// estimate would inflate and nothing would say so.
    #[test]
    fn a_catalog_with_only_the_x86_compute_line_is_refused_not_substituted() {
        let catalog = without("AWS-Lambda-MicroVM-vCPU-Second-ARM");
        let err = RateTable::from_catalog(
            Region::UsEast1,
            "aws pricing api",
            CalendarDate::from_ymd(2026, 8, 7),
            &catalog,
        )
        .expect_err("a missing ARM line is refused, never substituted");
        assert_eq!(err.kind(), ErrorKind::InvalidArg);
        let message = err.to_string();
        assert!(message.contains("ARM64-only"), "{message}");
        assert!(
            message.contains("0.0000326557"),
            "the error must name the rate it refuses to substitute: {message}"
        );
        assert!(message.contains("18%"), "{message}");
        assert!(message.contains("vcpu_second"), "{message}");
    }

    /// The same for memory, since two compute lines means two chances to substitute.
    #[test]
    fn the_memory_line_refuses_its_x86_sibling_too() {
        let err = RateTable::from_catalog(
            Region::UsEast1,
            "aws pricing api",
            CalendarDate::from_ymd(2026, 8, 7),
            &without("AWS-Lambda-MicroVM-Memory-GB-Second-ARM"),
        )
        .expect_err("a missing ARM memory line is refused");
        let message = err.to_string();
        assert!(message.contains("ARM64-only"), "{message}");
        assert!(message.contains("0.0000043235"), "{message}");
        assert!(message.contains("gb_second"), "{message}");
    }

    /// The x86 probe is a courtesy, not a requirement: with neither line present the
    /// error still has to say which field cannot be filled.
    #[test]
    fn both_compute_rates_missing_still_names_the_field() {
        let mut catalog = without("AWS-Lambda-MicroVM-Memory-GB-Second-ARM");
        catalog = {
            let mut rebuilt = RateCatalog::new();
            for entry in catalog.entries.iter() {
                if entry.group != "AWS-Lambda-MicroVM-Memory-GB-Second" {
                    rebuilt = rebuilt.with_entry(&*entry.group, &*entry.unit, entry.usd);
                }
            }
            rebuilt
        };
        let err = RateTable::from_catalog(
            Region::UsEast1,
            "aws pricing api",
            CalendarDate::from_ymd(2026, 8, 7),
            &catalog,
        )
        .expect_err("neither compute line means no table");
        let message = err.to_string();
        assert!(message.contains("gb_second"), "{message}");
        assert!(message.contains("neither"), "{message}");
    }

    /// Snapshot lines have no architecture variant, so there is nothing to refuse — and
    /// the message must not claim otherwise, or a reader goes looking for an ARM line
    /// that never existed.
    #[test]
    fn a_missing_snapshot_rate_names_no_x86_sibling() {
        let err = RateTable::from_catalog(
            Region::UsEast1,
            "aws pricing api",
            CalendarDate::from_ymd(2026, 8, 7),
            &without("AWS-Lambda-MicroVM-Snapshot-Write-GB"),
        )
        .expect_err("a missing snapshot rate means no table");
        let message = err.to_string();
        assert!(message.contains("snapshot_write_gb"), "{message}");
        assert!(!message.contains("ARM64-only"), "{message}");
    }

    /// An empty catalog is the *region*, not the catalog, and it is a different repair:
    /// check the region versus work out what AWS renamed. Measured, an unpriced region
    /// answers `AccessDeniedException` with a null message, which is why this message
    /// carries the IAM caveat.
    #[test]
    fn an_empty_catalog_reads_as_an_unpriced_region_not_a_renamed_line() {
        let err = RateTable::from_catalog(
            Region::unlisted("eu-central-1"),
            "aws pricing api",
            CalendarDate::from_ymd(2026, 8, 7),
            &RateCatalog::new(),
        )
        .expect_err("an empty catalog means no table");
        let message = err.to_string();
        assert!(message.contains("AccessDeniedException"), "{message}");
        assert!(message.contains("null message"), "{message}");
        assert!(
            message.contains("before auditing a policy"),
            "the caveat that saves someone reading an IAM policy that is fine: {message}"
        );
        assert!(!message.contains("ARM64-only"), "{message}");
    }

    /// A restated unit is refused rather than silently rescaled. It is the only signal
    /// available if AWS restates storage per GB-month: the number moves 730x and every
    /// arithmetic check downstream still passes, because they all read the same table.
    #[test]
    fn a_restated_unit_is_refused_rather_than_silently_rescaled() {
        let catalog = without("AWS-Lambda-MicroVM-Snapshot-Storage-GB-Hour").with_entry(
            "AWS-Lambda-MicroVM-Snapshot-Storage-GB-Hour",
            "GB-Months",
            dec!(0.0001111111),
        );
        let err = RateTable::from_catalog(
            Region::UsEast1,
            "aws pricing api",
            CalendarDate::from_ymd(2026, 8, 7),
            &catalog,
        )
        .expect_err("a restated unit is refused");
        let message = err.to_string();
        assert!(message.contains("quoted per \"GB-Months\""), "{message}");
        assert!(message.contains("still look plausible"), "{message}");
    }

    /// A tiered rate cannot be flattened into one number, and picking the first would
    /// silently choose one.
    #[test]
    fn two_products_for_one_group_is_refused_rather_than_arbitrated() {
        let catalog = recorded_catalog().with_entry(
            "AWS-Lambda-MicroVM-vCPU-Second-ARM",
            "vCPU-Seconds",
            dec!(0.0000999999),
        );
        let err = RateTable::from_catalog(
            Region::UsEast1,
            "aws pricing api",
            CalendarDate::from_ymd(2026, 8, 7),
            &catalog,
        )
        .expect_err("two products for one group is refused");
        assert!(err.to_string().contains("silently select a rate"), "{err}");
    }

    /// A fetched table drops straight into a report, which is the point of returning a
    /// [`RateTable`] rather than a map: every figure is computed against whatever table
    /// it was handed, so a regional table needs no second arithmetic path. Tokyo's
    /// compute is 16.4% over Virginia's.
    #[test]
    fn a_regional_table_needs_no_second_arithmetic_path() {
        let tokyo_catalog = RateCatalog::new()
            .with_entry(
                "AWS-Lambda-MicroVM-vCPU-Second-ARM",
                "vCPU-Seconds",
                dec!(0.0000322421),
            )
            .with_entry(
                "AWS-Lambda-MicroVM-Memory-GB-Second-ARM",
                "GB-Seconds",
                dec!(0.0000042688),
            )
            .with_entry(
                "AWS-Lambda-MicroVM-Snapshot-Storage-GB-Hour",
                "GB-Hours",
                dec!(0.0001333333),
            )
            .with_entry(
                "AWS-Lambda-MicroVM-Snapshot-Read-GB",
                "GB",
                dec!(0.0018548941),
            )
            .with_entry(
                "AWS-Lambda-MicroVM-Snapshot-Write-GB",
                "GB",
                dec!(0.0046556039),
            );
        let tokyo = RateTable::from_catalog(
            Region::ApNortheast1,
            "aws pricing api",
            fresh_day(),
            &tokyo_catalog,
        )
        .expect("Tokyo's catalog is complete");
        assert!(tokyo.vcpu_second() / rates().vcpu_second() > dec!(1.16));
        assert!(tokyo.snapshot_write_gb() / rates().snapshot_write_gb() > dec!(1.22));

        let here = run_report(
            SizeClass::Mib2048,
            &RunUsage {
                running: Some(DurationP::Measured(Duration::from_secs(3600))),
                launched: false,
                ..RunUsage::default()
            },
            &tokyo,
            fresh_day(),
            "run",
        )
        .expect("a Tokyo report prices");
        let there = running_report(
            SizeClass::Mib2048,
            DurationP::Measured(Duration::from_secs(3600)),
        );
        assert!(here.total().floor() > there.total().floor());
        assert!(here.render().contains("ap-northeast-1"));
    }

    /// The catalog line metadata is the Python's `MICROVM_LINES`, field for field, so a
    /// drift report names the same thing in both clients.
    #[test]
    fn the_catalog_lines_match_the_python_rate_line_table() {
        let described: Vec<(&str, &str, &str, Option<&str>)> = CatalogLine::ALL
            .iter()
            .map(|line| (line.field(), line.group(), line.unit(), line.x86_group()))
            .collect();
        assert_eq!(
            described,
            [
                (
                    "vcpu_second",
                    "AWS-Lambda-MicroVM-vCPU-Second-ARM",
                    "vCPU-Seconds",
                    Some("AWS-Lambda-MicroVM-vCPU-Second")
                ),
                (
                    "gb_second",
                    "AWS-Lambda-MicroVM-Memory-GB-Second-ARM",
                    "GB-Seconds",
                    Some("AWS-Lambda-MicroVM-Memory-GB-Second")
                ),
                (
                    "storage_gb_month",
                    "AWS-Lambda-MicroVM-Snapshot-Storage-GB-Hour",
                    "GB-Hours",
                    None
                ),
                (
                    "snapshot_read_gb",
                    "AWS-Lambda-MicroVM-Snapshot-Read-GB",
                    "GB",
                    None
                ),
                (
                    "snapshot_write_gb",
                    "AWS-Lambda-MicroVM-Snapshot-Write-GB",
                    "GB",
                    None
                ),
            ]
        );
        // Only the storage line converts, because only it is quoted per hour.
        assert!(CatalogLine::SnapshotStorageGbHour.is_per_hour());
        assert_eq!(
            CatalogLine::ALL
                .iter()
                .filter(|line| line.is_per_hour())
                .count(),
            1
        );
    }

    // -- the warm-pool comparison ----------------------------------------------

    /// The comparison at the shape every golden figure below was recorded against.
    fn month_comparison() -> ResidencyComparison {
        compare_residency(SizeClass::Mib2048, MONTH, 1, &rates(), fresh_day())
            .expect("a month-long comparison prices")
    }

    /// The whole argument for a warm suspended pool, and the reason the strategy memo
    /// can decline to build the scheduler and still hand over the numbers.
    #[test]
    fn suspended_is_two_orders_of_magnitude_cheaper_than_running() {
        assert!(month_comparison().ratio() > dec!(100));
    }

    /// Without the per-cycle cost the honest conclusion inverts: "suspend constantly"
    /// reads as free when each cycle pays a write plus a read.
    #[test]
    fn the_comparison_includes_the_per_cycle_cost() {
        let comparison = month_comparison();
        let rates = rates();
        assert_eq!(
            comparison.per_cycle().expect("a documented size").amount(),
            dec!(2) * (rates.snapshot_write_gb() + rates.snapshot_read_gb())
        );
        let rendered = comparison.render().expect("a comparison renders");
        assert!(rendered.contains("avoid churn"), "{rendered}");
        assert!(rendered.contains("break-even hold"), "{rendered}");
    }

    /// **The golden.** The break-even hold at 2 GB, pinned against the Python oracle
    /// rather than against a re-derivation: its `compare_residency(size=2048,
    /// hold_seconds=730*3600, cycles=1).break_even_seconds` printed
    /// `1371.2916483478837` when it was run in the session that wrote this test.
    ///
    /// Worth recording that the task packet predicted ≈1357s. The oracle decides, and it
    /// says 1371.29 — a re-derivation would have been the wrong check here, because it
    /// is exactly what would have agreed with a plausible wrong number.
    ///
    /// The two clients agree to 22 significant digits and diverge only past that:
    /// `rust_decimal` saturates at 28 digits of scale where Python's `Decimal` context
    /// is unbounded. So the exact figure is asserted to a tolerance well inside a
    /// second, and the f64 form — which is what `cli.py` emits as `breakEvenSeconds` —
    /// is asserted to agree with the Python's float exactly.
    #[test]
    fn the_break_even_hold_at_two_gigabytes_matches_the_python_oracle() {
        let comparison = month_comparison();
        let exact = comparison.break_even_seconds().expect("a documented size");
        let oracle = dec!(1371.291648347883680961978771);
        assert!(
            (exact - oracle).abs() < dec!(0.000000001),
            "the Rust figure {exact} must match the Python oracle {oracle}"
        );
        // The f64 the CLI emits, byte-identical to the Python's.
        assert_eq!(
            comparison
                .break_even_seconds_f64()
                .expect("a documented size"),
            1371.2916483478837
        );
        // And the other recorded figures from the same oracle run, so a change to any
        // one of them is visible here rather than only in the break-even.
        assert_eq!(
            comparison.per_cycle().expect("a documented size").amount(),
            dec!(0.01070)
        );
        assert_eq!(comparison.ratio().round_dp(4), dec!(532.3380));
    }

    /// The break-even is the number a pool scheduler needs, and this is what makes it
    /// meaningful rather than merely reproducible: just under it, a cycle loses money;
    /// just over, it saves. Asserting the *verdict* the figure predicts, per
    /// `.erpaval/solutions/test-failures/proptest-and-dst-tiers-need-verdict-assertions.md`
    /// — a test that only pinned 1371.29 would pass against a formula that computed a
    /// number with no relation to the crossover.
    #[test]
    fn churn_below_break_even_costs_more_than_leaving_the_vm_running() {
        let break_even = month_comparison()
            .break_even_seconds()
            .expect("a documented size");
        assert!(break_even > Decimal::ZERO);

        let suspended_beats_running = |hold: Duration| {
            let comparison = compare_residency(SizeClass::Mib2048, hold, 1, &rates(), fresh_day())
                .expect("a comparison prices");
            comparison.suspended().total().floor() < comparison.running().total().floor()
        };
        let at = |factor: Decimal| {
            Duration::from_secs_f64(
                (break_even * factor)
                    .to_f64()
                    .expect("the break-even hold fits in an f64"),
            )
        };
        assert!(
            !suspended_beats_running(at(dec!(0.9))),
            "just under the break-even hold, a cycle must lose money"
        );
        assert!(
            suspended_beats_running(at(dec!(1.1))),
            "just over it, a cycle must save"
        );
    }

    /// Image build and storage are identical either way, so including them would shrink
    /// the ratio the comparison exists to show — and drag an unpriced build into a figure
    /// whose whole job is to be comparable.
    #[test]
    fn the_comparison_excludes_the_image_so_the_ratio_is_not_diluted() {
        let comparison = month_comparison();
        for report in [comparison.running(), comparison.suspended()] {
            let phases: Vec<CostPhase> = report.items().iter().map(|item| item.phase).collect();
            assert!(!phases.contains(&CostPhase::ImageBuild), "{phases:?}");
            assert!(!phases.contains(&CostPhase::ImageStorage), "{phases:?}");
            assert!(report.is_complete());
            // No launch either: a launch read would be paid on the running side only and
            // would tilt the ratio it is not part of.
            assert!(!phases.contains(&CostPhase::Launch), "{phases:?}");
        }
    }

    /// The hold on a comparison is always projected: a comparison is a hypothetical
    /// about a hold nobody has taken yet, so neither side may read as a receipt.
    #[test]
    fn a_comparison_is_never_a_measured_report() {
        let comparison = month_comparison();
        assert_eq!(comparison.hold().provenance(), Provenance::Projected);
        assert!(!comparison.running().fully_measured());
        assert!(!comparison.suspended().fully_measured());
    }

    /// The ratio's denominator can never be zero, because snapshot storage always has
    /// the minimum-retention floor (COST-8). The degenerate input — a zero-length hold —
    /// is the case that would divide by zero without it.
    #[test]
    fn a_zero_length_hold_still_has_a_finite_ratio() {
        let comparison =
            compare_residency(SizeClass::Mib2048, Duration::ZERO, 1, &rates(), fresh_day())
                .expect("a zero hold prices");
        assert!(comparison.suspended().total().floor().amount() > Decimal::ZERO);
        // Running for zero seconds costs nothing, so the ratio is zero rather than
        // undefined — and suspending a VM for no time is a pure loss, which is what the
        // break-even figure is about.
        assert_eq!(comparison.ratio(), Decimal::ZERO);
    }

    // -- properties ------------------------------------------------------------

    /// A plausible span, in seconds. Weighted rather than uniform, for the reason
    /// T-W2-2 measured: a uniform draw over the whole domain essentially never lands in
    /// the band where the interesting branch is reachable. Here the band that matters is
    /// the minimum-retention window, since that is where the storage floor applies and
    /// where the break-even formula changes slope.
    fn plausible_seconds() -> impl Strategy<Value = u64> {
        prop_oneof![
            // Inside the retention window, where the storage floor binds.
            4 => 0u64..604_800,
            // Past it, where storage grows with the hold.
            3 => 604_800u64..(730 * 3600 * 12),
            // The neighbourhood of the floor itself, where an off-by-one in the `max`
            // would hide.
            2 => 604_795u64..=604_805,
            // A long tail, so nothing above goes unchecked.
            1 => 0u64..u32::MAX as u64,
        ]
    }

    proptest! {
        /// COST-4, as a property over every combination of priced and unpriced lines: a
        /// total is `AtLeast` exactly when an unpriced line is present, its floor is the
        /// sum of the priced ones and nothing else, and every unpriced reason is named.
        ///
        /// The verdict is computed from the generated input rather than read off the
        /// answer, which is what makes this catch a `Total::of` that dropped the
        /// unpriced line: such an implementation answers `Exact` where `documented`
        /// says `AtLeast`.
        ///
        /// The parenthetical is checked against a reference built the oracle's way —
        /// `", ".join(sorted({phase}))` — so a `Display` that reaches for reasons, report
        /// order, or the wrong separator disagrees on some generated input rather than
        /// only on the one report a hand-written test happens to build.
        #[test]
        fn a_total_is_a_lower_bound_exactly_when_something_is_unpriced(
            figures in prop::collection::vec(
                (
                    prop::sample::select(&[
                        CostPhase::ImageBuild,
                        CostPhase::Running,
                        CostPhase::Suspended,
                    ][..]),
                    prop_oneof![
                        (0u64..1_000_000).prop_map(|cents| Amount::estimated(
                            Decimal::from(cents) / dec!(100)
                        )),
                        "[a-z ]{1,20}".prop_map(Amount::unpriced),
                    ],
                ),
                0..12,
            ),
        ) {
            let expected_floor: Decimal = figures
                .iter()
                .filter_map(|(_, amount)| amount.estimate())
                .map(EstimatedUsd::amount)
                .sum();
            let expected_reasons: Vec<String> = figures
                .iter()
                .filter_map(|(_, amount)| amount.unpriced_reason())
                .map(str::to_string)
                .collect();
            let expected_names: Vec<&str> = {
                let mut names: Vec<&str> = figures
                    .iter()
                    .filter(|(_, amount)| amount.unpriced_reason().is_some())
                    .map(|(phase, _)| phase.as_str())
                    .collect();
                names.sort_unstable();
                names.dedup();
                names
            };

            let total = Total::of(figures.iter().map(|(phase, amount)| (*phase, amount)));
            prop_assert_eq!(total.is_lower_bound(), !expected_reasons.is_empty());
            prop_assert_eq!(total.floor().amount(), expected_floor);
            prop_assert_eq!(total.unpriced_reasons(), expected_reasons.as_slice());
            prop_assert_eq!(total.unpriced_phase_names(), expected_names.as_slice());
            match &total {
                Total::Exact(_) => prop_assert!(expected_reasons.is_empty()),
                Total::AtLeast { unpriced, .. } => prop_assert!(!unpriced.is_empty()),
            }
            // The rendering says which it is, since that is what a reader sees.
            prop_assert_eq!(
                total.to_string().starts_with("at least "),
                total.is_lower_bound()
            );
            if total.is_lower_bound() {
                prop_assert_eq!(
                    total.to_string(),
                    format!(
                        "at least {}, plus {} unpriced ({})",
                        total.floor(),
                        expected_reasons.len(),
                        expected_names.join(", ")
                    )
                );
            }
        }

        /// COST-6, over the whole arithmetic: a report's figures are exactly what the
        /// rate table times the quantities says they are, with no precision lost. The
        /// reference is recomputed in `Decimal` from the same inputs, so a float
        /// intermediate anywhere in the compute path disagrees.
        #[test]
        fn compute_figures_are_exactly_the_decimal_reference(
            seconds in plausible_seconds(),
            class_index in 0usize..5,
        ) {
            let size = SizeClass::ALL[class_index];
            let rates = rates();
            let duration = Duration::from_secs(seconds);
            let report = run_report(
                size,
                &RunUsage {
                    running: Some(DurationP::Measured(duration)),
                    launched: false,
                    ..RunUsage::default()
                },
                &rates,
                fresh_day(),
                "run",
            )?;

            let exact_seconds = Decimal::from(seconds);
            let vcpu = gb_decimal(size.baseline_vcpu())?;
            let memory = gb_decimal(size.baseline_gb())?;
            let expected_vcpu = vcpu * exact_seconds * rates.vcpu_second();
            let expected_memory = memory * exact_seconds * rates.gb_second();

            let mut lines = report.by_phase(CostPhase::Running);
            let vcpu_line = lines.next().expect("a vcpu line");
            let memory_line = lines.next().expect("a memory line");
            prop_assert_eq!(vcpu_line.amount.estimate().expect("priced").amount(), expected_vcpu);
            prop_assert_eq!(memory_line.amount.estimate().expect("priced").amount(), expected_memory);
            // And the total is the sum of exactly those two, not a re-derivation.
            prop_assert_eq!(
                report.total(),
                Total::Exact(EstimatedUsd::new(expected_vcpu + expected_memory))
            );
            // COST-5 as a property: the quantity is the baseline times the seconds, and
            // for every class the peak figure is a different number.
            prop_assert_eq!(memory_line.quantity, memory * exact_seconds);
            if seconds > 0 {
                prop_assert_ne!(
                    memory_line.quantity,
                    gb_decimal(size.peak_gb())? * exact_seconds
                );
            }
        }

        /// COST-8 as a property: storage bills `max(held, one week)` for every hold,
        /// and the note says so exactly when the floor bound. A `max` written the wrong
        /// way round passes at long holds and fails inside the window, which is why the
        /// generator is weighted towards it.
        #[test]
        fn storage_bills_the_greater_of_the_hold_and_the_retention_floor(
            seconds in plausible_seconds(),
        ) {
            let rates = rates();
            let report = run_report(
                SizeClass::Mib2048,
                &RunUsage {
                    suspended: Some(DurationP::Measured(Duration::from_secs(seconds))),
                    launched: false,
                    ..RunUsage::default()
                },
                &rates,
                fresh_day(),
                "suspended",
            )?;
            let floor = seconds_of(rates.minimum_retention());
            let billed = Decimal::from(seconds).max(floor);
            let expected = dec!(2) * billed / SECONDS_PER_MONTH;

            let items: Vec<&LineItem> = report.by_phase(CostPhase::Suspended).collect();
            prop_assert_eq!(items[0].quantity, expected);
            prop_assert_eq!(
                items[0].amount.estimate().expect("priced").amount(),
                expected * rates.storage_gb_month()
            );
            // The verdict on the note, computed from the input: it claims the floor
            // exactly when the floor was what bound.
            prop_assert_eq!(
                items[0].note.contains("minimum retention"),
                Decimal::from(seconds) < floor
            );
            // Never below the floor's own charge, which is the failure this requirement
            // is about — understating a create-and-destroy suite by four orders of
            // magnitude.
            prop_assert!(items[0].quantity >= dec!(2) * floor / SECONDS_PER_MONTH);
        }

        /// COST-10 as a property: whatever a plan contains, every duration in the
        /// resulting report is projected and the report is never fully measured.
        #[test]
        fn every_duration_in_a_plan_is_projected(
            running in 0u64..1_000_000,
            suspended in 0u64..1_000_000,
            retained in prop::option::of(1u64..1_000_000),
            cycles in 0u32..5,
            image in prop::option::of(0.5f64..64.0),
        ) {
            let report = estimate_run(
                SizeClass::Mib2048,
                &PlanUsage {
                    running_seconds: running as f64,
                    suspended_seconds: suspended as f64,
                    image_gb: image,
                    image_retained_seconds: retained.map(|seconds| seconds as f64),
                    suspend_resume_cycles: cycles,
                    snapshot_gb: None,
                    launched: true,
                },
                &rates(),
                fresh_day(),
                "estimate",
            )?;
            for item in report.items() {
                if let Some(duration) = item.duration {
                    prop_assert_eq!(duration.provenance(), Provenance::Projected);
                }
            }
            prop_assert!(!report.fully_measured());
            // An image drags the unpriced build in, so the total's shape is decided by
            // whether one was named — the verdict, computed from the input.
            prop_assert_eq!(report.total().is_lower_bound(), image.is_some());
        }

        /// The measured and projected paths are one arithmetic. If they could disagree
        /// numerically, the label would be hiding a second implementation.
        #[test]
        fn a_plan_and_a_timed_run_of_the_same_shape_price_identically(
            running in 1u64..1_000_000,
            cycles in 0u32..5,
        ) {
            let projected = estimate_run(
                SizeClass::Mib2048,
                &PlanUsage {
                    running_seconds: running as f64,
                    suspend_resume_cycles: cycles,
                    ..PlanUsage::default()
                },
                &rates(),
                fresh_day(),
                "estimate",
            )?;
            let measured = run_report(
                SizeClass::Mib2048,
                &RunUsage {
                    running: Some(DurationP::Measured(Duration::from_secs(running))),
                    suspend_resume_cycles: cycles,
                    launched: false,
                    ..RunUsage::default()
                },
                &rates(),
                fresh_day(),
                "run",
            )?;
            prop_assert_eq!(projected.total(), measured.total());
            prop_assert!(!projected.fully_measured());
        }

        /// The float boundary is exact where it succeeds (COST-6). Every f64 that has a
        /// decimal reading gets that reading, not its binary approximation — which is
        /// what `Decimal::try_from(0.1f64)` would give.
        #[test]
        fn the_float_boundary_reads_the_decimal_spelling_not_the_binary_value(
            gb in 0.0f64..1_000_000.0,
        ) {
            let converted = gb_decimal(gb)?;
            prop_assert_eq!(converted.to_string(), gb.to_string());
            prop_assert!(converted >= Decimal::ZERO);
        }

        /// The staleness verdict is a comparison against ninety days and nothing else,
        /// over a range of retrieval ages. Computed from the input, so an off-by-one at
        /// the boundary fails.
        #[test]
        fn staleness_is_the_ninety_day_verdict_and_carries_the_age(
            age_days in -10i64..400,
        ) {
            let rates = rates();
            let today = CalendarDate::from_day_number(
                rates.retrieved().day_number() + age_days
            );
            prop_assert_eq!(rates.age_days(today), age_days);
            prop_assert_eq!(rates.is_stale(today), age_days > STALE_AFTER_DAYS);
            match rates.staleness(today) {
                Some(warning) => {
                    prop_assert!(age_days > STALE_AFTER_DAYS);
                    let expected = format!("{age_days} days ago");
                    prop_assert!(warning.contains(&expected), "{}", warning);
                }
                None => prop_assert!(age_days <= STALE_AFTER_DAYS),
            }
            // And the report carries whatever the table said.
            let report = run_report(
                SizeClass::Mib2048,
                &RunUsage {
                    running: Some(DurationP::Measured(Duration::from_secs(60))),
                    launched: false,
                    ..RunUsage::default()
                },
                &rates,
                today,
                "run",
            )?;
            prop_assert_eq!(report.staleness().is_some(), age_days > STALE_AFTER_DAYS);
        }

        /// The date round trip, which is what licenses a hand-rolled calendar: every day
        /// number is a date whose day number is itself, across four centuries.
        #[test]
        fn a_day_number_round_trips_through_its_calendar_date(
            day_number in -30_000i64..30_000,
        ) {
            let date = CalendarDate::from_day_number(day_number);
            prop_assert_eq!(date.day_number(), day_number);
            // And the parts are a real calendar day, so the validator agrees with the
            // arithmetic.
            prop_assert!(
                CalendarDate::try_from_ymd(date.year(), date.month(), date.day()).is_ok(),
                "{date} is not a calendar day"
            );
        }
    }
}
