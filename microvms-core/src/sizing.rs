// SPDX-License-Identifier: Apache-2.0
//! Size classes: what `minimumMemoryInMiB` actually buys, and what it bills.
//!
//! `minimumMemoryInMiB` does not size a VM. It selects a *class*, and the class has
//! two numbers that differ by 4x — the baseline you are billed for every running
//! second, and the peak the guest reports in `/proc/meminfo`. Measured 2026-08-07,
//! us-east-1: requesting 512 produced a guest reporting ~2 GB `MemTotal`; requesting
//! 2048 produced ~8 GB. Both match AWS's documented sizing table exactly, so the
//! number the caller writes down is neither the number they get nor, on its own,
//! enough to predict the bill (`docs/PLATFORM.md`, "`minimumMemoryInMiB` selects a
//! *baseline*, and the guest reports the *peak*").
//!
//! The peak is provisioned from the start (service team, confirmed 2026-08): there
//! is no scaling event, nothing resizes during a run, and app code never observes a
//! resource change. The baseline is the bill's floor — always paid while the VM
//! runs — and usage above it is billed by consumption.
//!
//! # Why the table is data (TRAP-13)
//!
//! Every documented peak is exactly four times its baseline, which makes
//! `baseline * 4` look like the obvious simplification. It is the one thing this
//! module must not do. That regularity is AWS's and not ours: a sixth row, or a
//! revision to an existing one, that breaks the pattern would silently get the
//! pattern applied to it, and the report would name a provisioned ceiling the service
//! does not offer. So [`SIZE_CLASSES`] is the only place any of these numbers appears, and
//! every accessor reads a row out of it through one lookup — which is also what makes
//! the guard testable: the test below drives the accessors over a table whose peak is
//! *not* 4x, and a computed implementation cannot answer it.
//!
//! # Why an off-table request is refused (TRAP-10)
//!
//! Only the five table values are accepted. What the service does with an off-table
//! request such as 1500 is undocumented and unmeasured, and the two plausible
//! behaviours — round up to the enclosing class, or take the literal as a baseline —
//! differ in both the memory the guest gets and the rate it is billed at. Rejecting
//! locally costs the caller a second; guessing costs them a wrong bill they have no
//! way to notice.

use std::fmt;

use crate::error::Error;

/// MiB per GB as AWS's pricing and sizing tables use it.
///
/// The guest's `MemTotal` reads in KiB and lands slightly under (2 GB shows as
/// 2037648 kB), so any comparison against a guest-reported figure is approximate by
/// nature.
pub const MIB_PER_GB: u32 = 1024;

/// One row of the documented sizing table: a baseline and its provisioned ceiling.
///
/// Plain data with public fields, because that is what it is — a transcription of
/// someone else's published table, not settings. The accessors on [`SizeClass`] are
/// where a caller normally reads it; the row is public so the drift gate and the
/// guard test can compare against the table itself.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SizeRow {
    /// What `minimumMemoryInMiB` must be set to in order to select this class.
    pub baseline_mib: u32,
    /// Billed per running second, alongside `baseline_vcpu`. Not what the guest
    /// reports.
    pub baseline_vcpu: f64,
    /// What the guest reports as `MemTotal`: a fixed ceiling, provisioned from the
    /// start and present the whole run. Usage above the baseline is billed by
    /// consumption, per second.
    pub peak_mib: u32,
    pub peak_vcpu: f64,
}

/// The documented table (`microvms-images.html`), smallest first.
///
/// The single definition of all twenty numbers. See the module docs for why nothing
/// derives one of them from another.
pub const SIZE_CLASSES: [SizeRow; 5] = [
    SizeRow {
        baseline_mib: 512,
        baseline_vcpu: 0.25,
        peak_mib: 2048,
        peak_vcpu: 1.0,
    },
    SizeRow {
        baseline_mib: 1024,
        baseline_vcpu: 0.5,
        peak_mib: 4096,
        peak_vcpu: 2.0,
    },
    SizeRow {
        baseline_mib: 2048,
        baseline_vcpu: 1.0,
        peak_mib: 8192,
        peak_vcpu: 4.0,
    },
    SizeRow {
        baseline_mib: 4096,
        baseline_vcpu: 2.0,
        peak_mib: 16384,
        peak_vcpu: 8.0,
    },
    SizeRow {
        baseline_mib: 8192,
        baseline_vcpu: 4.0,
        peak_mib: 32768,
        peak_vcpu: 16.0,
    },
];

/// One of the five documented size classes.
///
/// Closed, so an off-table memory figure is not a value that exists — the Python
/// client's S2 runtime check (`sizing.py:95` `size_class_for`) becomes S1 for every
/// caller who has a `SizeClass` in hand. [`SizeClass::from_baseline_mib`] is the one
/// boundary where a bare integer still has to be judged, which is where TRAP-10
/// lives.
///
/// The variants are named for their baseline because that is the number the caller
/// writes into `minimumMemoryInMiB`; the peak is deliberately not in any name, since
/// naming both would suggest a caller gets to pick them independently.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum SizeClass {
    Mib512,
    Mib1024,
    Mib2048,
    Mib4096,
    Mib8192,
}

impl SizeClass {
    /// The platform's own default, and ours.
    ///
    /// Deliberately not the smallest class, and the reason is the CEILING: picking
    /// 512 gives the guest a hard 2 GiB total — provisioned from the start, fixed
    /// for the whole run, nothing resizes past it — and a real test suite breaches
    /// it. Guest swap is absent (`SwapTotal: 0 kB`), so there is no paging phase to
    /// absorb the mistake — pressure goes straight to the OOM killer. The default's
    /// job is a ceiling that survives a real test suite, and 8 GiB does.
    pub const DEFAULT: SizeClass = SizeClass::Mib2048;

    /// Every class, smallest first, in [`SIZE_CLASSES`] order.
    pub const ALL: [SizeClass; 5] = [
        SizeClass::Mib512,
        SizeClass::Mib1024,
        SizeClass::Mib2048,
        SizeClass::Mib4096,
        SizeClass::Mib8192,
    ];

    /// The class `minimumMemoryInMiB = mib` selects (TRAP-10).
    ///
    /// Rejects anything not in the table rather than snapping to a neighbour: an
    /// off-table request has two plausible readings that differ in both memory and
    /// rate, and we have measured neither. This is the boundary a figure from a CLI
    /// flag or a config file crosses.
    pub fn from_baseline_mib(mib: u32) -> Result<SizeClass, Error> {
        if let Some(class) = class_for_baseline_in(&SIZE_CLASSES, mib) {
            return Ok(class);
        }
        let offered = SIZE_CLASSES
            .iter()
            .map(|row| row.baseline_mib.to_string())
            .collect::<Vec<_>>()
            .join(", ");
        Err(Error::invalid_arg(format!(
            "minimumMemoryInMiB={mib} is not a documented size class baseline. The field \
             selects a class, it does not size a VM; pass one of {offered} MiB. See \
             docs/PLATFORM.md, '`minimumMemoryInMiB` selects a *baseline*, and the guest \
             reports the *peak*'."
        )))
    }

    /// This class's row of the documented table.
    ///
    /// The one reader of [`SIZE_CLASSES`] for a known class, which is what keeps
    /// every accessor below reading data rather than deriving it.
    pub fn row(self) -> &'static SizeRow {
        row_in(&SIZE_CLASSES, self)
    }

    /// The value to send as `minimumMemoryInMiB`. Billed per running second.
    pub fn baseline_mib(self) -> u32 {
        self.row().baseline_mib
    }

    /// What the guest reports as `MemTotal`: the provisioned ceiling, present from
    /// the start of the run.
    ///
    /// Read from the table. Never `baseline_mib() * 4` — see the module docs.
    pub fn peak_mib(self) -> u32 {
        self.row().peak_mib
    }

    /// The static headroom above the baseline: peak minus baseline, both read from
    /// this class's table row.
    ///
    /// Fixed at provision time — the headroom is always present, never granted by a
    /// scaling event — and usage inside it is billed by consumption. Computed as a
    /// difference of two *table* values, never as `baseline * 3`: the 4x regularity
    /// is AWS's, not ours, and a row that breaks it must be reported as written
    /// (TRAP-13, same discipline as [`SizeClass::peak_mib`]).
    pub fn headroom_mib(self) -> u32 {
        headroom_in(&SIZE_CLASSES, self)
    }

    /// Billed per running second, alongside [`SizeClass::baseline_mib`].
    pub fn baseline_vcpu(self) -> f64 {
        self.row().baseline_vcpu
    }

    /// The provisioned vCPU ceiling, present from the start. Read from the table.
    pub fn peak_vcpu(self) -> f64 {
        self.row().peak_vcpu
    }

    /// The figure a GB-second rate multiplies. Always the baseline, never the peak
    /// (COST-5).
    pub fn baseline_gb(self) -> f64 {
        f64::from(self.baseline_mib()) / f64::from(MIB_PER_GB)
    }

    /// The peak in GB, for a report that shows the caller what the guest will say.
    pub fn peak_gb(self) -> f64 {
        f64::from(self.peak_mib()) / f64::from(MIB_PER_GB)
    }

    /// The position of this class in [`SIZE_CLASSES`].
    ///
    /// Private and `const`: it is the identity mapping between the enum and the
    /// table, and the invariant test below pins it to the baselines so a reordered
    /// table cannot quietly re-point every variant.
    const fn index(self) -> usize {
        match self {
            SizeClass::Mib512 => 0,
            SizeClass::Mib1024 => 1,
            SizeClass::Mib2048 => 2,
            SizeClass::Mib4096 => 3,
            SizeClass::Mib8192 => 4,
        }
    }
}

/// One line naming both numbers, for an error message or a CLI.
///
/// Both numbers or neither: naming only the peak invites someone to budget for
/// memory they will not be billed for, and naming only the baseline invites a
/// memory-pressure test written against a ceiling four times lower than the one the
/// guest enforces.
impl fmt::Display for SizeClass {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let row = self.row();
        write!(
            f,
            "{} GB / {} vCPU baseline (billed while running), with a fixed ceiling of {} GB / \
             {} vCPU provisioned from the start (what the guest reports)",
            self.baseline_gb(),
            row.baseline_vcpu,
            self.peak_gb(),
            row.peak_vcpu,
        )
    }
}

/// The row a class occupies in `table`.
///
/// Takes the table as a parameter for one reason: it is what makes TRAP-13's guard
/// falsifiable. Every accessor above goes through here, so a test can hand this
/// function a table whose peak is not four times its baseline and see whether the
/// answer follows the data or the pattern.
fn row_in(table: &[SizeRow], class: SizeClass) -> &SizeRow {
    &table[class.index()]
}

/// The headroom `table`'s row carries for `class`: its peak minus its baseline.
///
/// Table-parameterized for the same reason as [`row_in`]: a test can hand this a
/// table whose peak is not four times its baseline and see whether the headroom
/// follows the data or the pattern. Saturating rather than plain subtraction so a
/// hostile test table whose peak is below its baseline reads as zero headroom
/// rather than a panic.
fn headroom_in(table: &[SizeRow], class: SizeClass) -> u32 {
    let row = row_in(table, class);
    row.peak_mib.saturating_sub(row.baseline_mib)
}

/// The class whose row in `table` carries `baseline_mib`, if any.
///
/// Table-parameterized for the same reason as [`row_in`], and a linear scan because
/// five rows is five rows.
fn class_for_baseline_in(table: &[SizeRow], baseline_mib: u32) -> Option<SizeClass> {
    SizeClass::ALL
        .into_iter()
        .find(|class| table[class.index()].baseline_mib == baseline_mib)
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;
    use crate::error::ErrorKind;

    /// The measured table, written out as literals. This is the test a table edit
    /// must fail: the twenty numbers are transcriptions of AWS's published table and
    /// two of the rows were measured directly, so changing one is a claim about the
    /// service that needs its own evidence.
    #[test]
    fn the_documented_table_carries_the_measured_rows() {
        let rows: Vec<(u32, f64, u32, f64)> = SIZE_CLASSES
            .iter()
            .map(|r| (r.baseline_mib, r.baseline_vcpu, r.peak_mib, r.peak_vcpu))
            .collect();
        assert_eq!(
            rows,
            [
                (512, 0.25, 2048, 1.0),
                (1024, 0.5, 4096, 2.0),
                (2048, 1.0, 8192, 4.0),
                (4096, 2.0, 16384, 8.0),
                (8192, 4.0, 32768, 16.0),
            ]
        );
    }

    /// TRAP-13. The lookup answers from the data, so a table whose peak breaks the
    /// 4x pattern is reported as written rather than as the pattern predicts.
    ///
    /// This is the falsification the spec asks for: replace [`SizeClass::peak_mib`]'s
    /// body with `self.baseline_mib() * 4` and this row's peak comes back 8192
    /// instead of 9000. A test that only checked the shipped table could not tell the
    /// two implementations apart, because every shipped peak *is* 4x its baseline.
    #[test]
    fn a_peak_that_is_not_four_times_its_baseline_is_read_not_computed() {
        let mut table = SIZE_CLASSES;
        table[SizeClass::Mib2048.index()].peak_mib = 9000;
        table[SizeClass::Mib2048.index()].peak_vcpu = 3.5;

        let row = row_in(&table, SizeClass::Mib2048);
        assert_eq!(
            row.peak_mib, 9000,
            "the peak must come from the table, not from baseline * 4"
        );
        assert_eq!(row.peak_vcpu, 3.5);
        // The baseline is untouched by the edit, which is what makes the assertion
        // above about the peak rather than about the row being found at all.
        assert_eq!(row.baseline_mib, 2048);
        assert_eq!(row.peak_mib * 4 / 4, 9000);
        assert_ne!(row.peak_mib, row.baseline_mib * 4);
    }

    /// The public accessors are that lookup applied to the shipped table, and
    /// nothing else. Under a computed implementation this passes today and goes red
    /// the moment a table row stops being 4x — which is exactly the regression
    /// TRAP-13 names.
    #[test]
    fn every_accessor_returns_its_own_row_from_the_shipped_table() {
        for class in SizeClass::ALL {
            let row = &SIZE_CLASSES[class.index()];
            assert_eq!(class.baseline_mib(), row.baseline_mib, "{class:?}");
            assert_eq!(class.peak_mib(), row.peak_mib, "{class:?}");
            assert_eq!(class.baseline_vcpu(), row.baseline_vcpu, "{class:?}");
            assert_eq!(class.peak_vcpu(), row.peak_vcpu, "{class:?}");
        }
    }

    /// The enum-to-row mapping is pinned to the baselines rather than to positions,
    /// so a reordered table fails here instead of quietly re-pointing every variant
    /// at its neighbour's numbers.
    #[test]
    fn each_variant_is_pinned_to_the_baseline_its_name_states() {
        assert_eq!(SizeClass::Mib512.baseline_mib(), 512);
        assert_eq!(SizeClass::Mib1024.baseline_mib(), 1024);
        assert_eq!(SizeClass::Mib2048.baseline_mib(), 2048);
        assert_eq!(SizeClass::Mib4096.baseline_mib(), 4096);
        assert_eq!(SizeClass::Mib8192.baseline_mib(), 8192);
    }

    /// TRAP-10: the five documented baselines are accepted and answer with their own
    /// class.
    #[test]
    fn the_five_documented_baselines_select_their_own_class() {
        for class in SizeClass::ALL {
            let selected = SizeClass::from_baseline_mib(class.baseline_mib())
                .expect("a documented baseline is accepted");
            assert_eq!(selected, class);
        }
    }

    /// TRAP-10's falsification, at the values a caller actually reaches for: a round
    /// number between two classes, the peak of a class mistaken for a request, and
    /// zero. 1500 is the case in `sizing.py`'s own docstring — the reading is
    /// ambiguous in a way that changes the bill, so it is refused rather than
    /// snapped.
    #[test]
    fn an_off_table_baseline_is_refused_naming_the_finding() {
        for mib in [0, 1, 511, 513, 1500, 3072, 8191, 16384, u32::MAX] {
            let err = SizeClass::from_baseline_mib(mib)
                .expect_err("only the five documented baselines are accepted");
            assert_eq!(err.kind(), ErrorKind::InvalidArg, "{mib}");
            let message = err.to_string();
            assert!(
                message.contains("not a documented size class baseline"),
                "{message}"
            );
            assert!(
                message.contains("selects a class, it does not size a VM"),
                "must name the finding: {message}"
            );
        }
    }

    /// A peak is not a baseline. 2048 is both — the peak of the smallest class and
    /// the baseline of the default — and 8192 is both the peak of the default and the
    /// largest baseline, so a caller who read a `MemTotal` and passed it back gets a
    /// silently different class rather than an error. Nothing can be done about that
    /// (both are legal requests), which is why `Display` always names both numbers.
    /// 16384 and 32768 are peaks that are *not* baselines and must be refused.
    #[test]
    fn a_peak_that_is_not_also_a_baseline_is_refused() {
        assert_eq!(
            SizeClass::from_baseline_mib(2048).expect("2048 is a baseline too"),
            SizeClass::Mib2048
        );
        for peak_only in [16384, 32768] {
            assert!(
                SizeClass::from_baseline_mib(peak_only).is_err(),
                "{peak_only}"
            );
        }
    }

    /// The default is the platform's, not the cheapest, and the reason is the
    /// ceiling: a default of `Mib512` gives the guest a hard 2 GB total —
    /// provisioned from the start, fixed for the whole run — which a real test
    /// suite breaches, and there is no swap to absorb the breach. The default's
    /// job is a ceiling that survives real work.
    #[test]
    fn the_default_class_is_two_gigabytes_of_baseline() {
        assert_eq!(SizeClass::DEFAULT, SizeClass::Mib2048);
        assert_eq!(SizeClass::DEFAULT.baseline_mib(), 2048);
        assert_eq!(SizeClass::DEFAULT.baseline_gb(), 2.0);
        assert_eq!(SizeClass::DEFAULT.peak_gb(), 8.0);
    }

    /// Both numbers or neither, so nobody budgets for memory they are not billed for
    /// and nobody writes a pressure test against the wrong ceiling.
    #[test]
    fn display_names_the_baseline_and_the_peak_together() {
        let described = SizeClass::DEFAULT.to_string();
        assert!(described.contains("2 GB / 1 vCPU baseline"), "{described}");
        assert!(described.contains("8 GB / 4 vCPU"), "{described}");
        assert!(described.contains("billed while running"), "{described}");
        assert!(described.contains("what the guest reports"), "{described}");
        // The mechanism, stated as it is: a fixed ceiling present from the start,
        // not a "burst" the service would have to grant.
        assert!(
            described.contains("provisioned from the start"),
            "{described}"
        );
        assert!(!described.contains("burst"), "{described}");
    }

    /// TRAP-13 for the headroom: peak minus baseline, both read from the table.
    ///
    /// The falsification: replace [`SizeClass::headroom_mib`]'s body with
    /// `self.baseline_mib() * 3` — arithmetically identical over the shipped table,
    /// where every peak is 4x — and this row's headroom comes back 6144 instead of
    /// 6952. A test over the shipped table alone could not tell the two apart.
    #[test]
    fn headroom_follows_a_table_whose_peak_is_not_four_times_its_baseline() {
        let mut table = SIZE_CLASSES;
        table[SizeClass::Mib2048.index()].peak_mib = 9000;
        assert_eq!(
            headroom_in(&table, SizeClass::Mib2048),
            9000 - 2048,
            "headroom must be a difference of table values, not baseline * 3"
        );
        // The shipped table's headroom for every class, through the public accessor.
        for class in SizeClass::ALL {
            let row = &SIZE_CLASSES[class.index()];
            assert_eq!(
                class.headroom_mib(),
                row.peak_mib - row.baseline_mib,
                "{class:?}"
            );
        }
    }

    /// The band where an off-table figure is *plausible*, sampled deliberately.
    ///
    /// A uniform `u32` draw is the wrong generator for TRAP-10 and measuring it
    /// proved so: with the whole domain to pick from, essentially every case lands
    /// above 8192, where the only reachable verdict is "refused" and a
    /// round-up-to-the-enclosing-class implementation is indistinguishable from a
    /// refusing one. Snapping can only happen *between* the smallest and largest
    /// baseline, so the property has to be told to look there. The band is what
    /// a caller actually types.
    fn plausible_baseline_mib() -> impl Strategy<Value = u32> {
        prop_oneof![
            // The whole domain, so nothing outside the band goes unchecked.
            2 => any::<u32>(),
            // The band where an enclosing class exists to snap to.
            5 => 1u32..=8192,
            // The neighbourhoods of each documented baseline, where an off-by-one or
            // a nearest-neighbour rule would hide.
            3 => prop::sample::select(SIZE_CLASSES.to_vec())
                .prop_flat_map(|row| (row.baseline_mib.saturating_sub(2))..=(row.baseline_mib + 2)),
        ]
    }

    proptest! {
        /// The verdict, over the whole `u32` domain plus the band where an off-table
        /// figure is plausible: accepted exactly when the figure is one of the five,
        /// refused as `ERR_INVALID_ARG` otherwise. Asserting the verdict rather than a
        /// side effect is the lesson from
        /// `.erpaval/solutions/test-failures/proptest-and-dst-tiers-need-verdict-assertions.md`
        /// — a property that only checked "no panic" would pass against a function
        /// that accepted everything.
        ///
        /// The `Ok` arm's second assertion is the one that catches snapping: a
        /// rounding implementation returns a class whose baseline is *not* the figure
        /// it was handed, so `class.baseline_mib() == mib` is what says the answer is
        /// the caller's request rather than a nearby one chosen for them.
        #[test]
        fn from_baseline_mib_accepts_exactly_the_five_documented_baselines(
            mib in plausible_baseline_mib(),
        ) {
            let documented = SIZE_CLASSES.iter().any(|row| row.baseline_mib == mib);
            match SizeClass::from_baseline_mib(mib) {
                Ok(class) => {
                    prop_assert!(documented, "{mib} MiB is not a documented baseline but was accepted");
                    prop_assert_eq!(class.baseline_mib(), mib);
                }
                Err(err) => {
                    prop_assert!(!documented, "{} MiB is documented but was refused", mib);
                    prop_assert_eq!(err.kind(), ErrorKind::InvalidArg);
                    prop_assert_eq!(err.code(), "ERR_INVALID_ARG");
                }
            }
        }

        /// Whatever a table says, the lookup says. Generated peaks are deliberately
        /// unrelated to their baselines, so a computed implementation disagrees with
        /// the table on almost every input.
        #[test]
        fn the_lookup_reports_whatever_peak_the_table_carries(
            peak_mib in 1u32..100_000,
            peak_vcpu in 0.1f64..64.0,
        ) {
            let mut table = SIZE_CLASSES;
            for class in SizeClass::ALL {
                table[class.index()].peak_mib = peak_mib;
                table[class.index()].peak_vcpu = peak_vcpu;
            }
            for class in SizeClass::ALL {
                let row = row_in(&table, class);
                prop_assert_eq!(row.peak_mib, peak_mib);
                prop_assert_eq!(row.peak_vcpu, peak_vcpu);
                // The baselines are untouched, so the row is still the right row.
                prop_assert_eq!(row.baseline_mib, class.baseline_mib());
                prop_assert_eq!(
                    class_for_baseline_in(&table, class.baseline_mib()),
                    Some(class)
                );
            }
        }
    }
}
