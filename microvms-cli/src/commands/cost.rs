//! `cost`: what a run cost, or what a plan will cost. Every figure labelled.
//!
//! Touches no account — the rate table is pinned in `microvms-core` — which is why it is one
//! of the four commands the behavioral thinness guard names as legitimately local.
//!
//! # `--estimate` is a different function, not a flag on one
//!
//! `run_report` takes [`microvms_core::cost::DurationP`]s and `estimate_run` takes plain
//! seconds it marks projected itself. Two entry points rather than one with a `measured: bool`,
//! because a plan whose durations are labelled measured is a report of something that never
//! happened — and core made that inexpressible by giving [`microvms_core::cost::PlanUsage`] no
//! field a `Measured` value could be written into.

use microvms_core::cost::{
    CalendarDate, DurationP, PlanUsage, RunUsage, compare_residency, estimate_run, pinned_rates,
    run_report,
};
use serde_json::{Map, Value};

use crate::cli::CostArgs;
use crate::commands::{Ctx, Rendered, response_type};
use crate::exit::CliError;
use crate::render::{comparison_to_json, report_dense, report_to_json};

/// Computes and renders a cost report, optionally beside the residency comparison.
pub fn cost<O: std::io::Write, E: std::io::Write>(
    ctx: &mut Ctx<'_, O, E>,
    args: &CostArgs,
) -> Result<Rendered, CliError> {
    let size = args.memory.size_class();
    let rates = pinned_rates();
    // A parameter rather than a clock read inside the report, so a report is a pure function of
    // its inputs — which is core's own choice and what lets a test assert staleness without
    // travelling in time.
    let today = CalendarDate::today_utc();
    // A launch happened if there is running time or an image, matching `cli.py:2168`. A launch
    // reads a snapshot, so claiming one that did not happen adds a transfer line.
    let launched = args.running_sec > 0.0 || args.image_gb.is_some();

    let report = if args.estimate {
        estimate_run(
            size,
            &PlanUsage {
                running_seconds: args.running_sec,
                suspended_seconds: args.suspended_sec,
                image_gb: args.image_gb,
                image_retained_seconds: None,
                suspend_resume_cycles: args.cycles,
                snapshot_gb: None,
                launched,
            },
            &rates,
            today,
            "plan",
        )?
    } else {
        // Zero means "this phase did not happen", not "a phase that cost nothing": a
        // zero-length line on a report claims a measurement nobody took.
        let measured = |seconds: f64| -> Result<Option<DurationP>, microvms_core::Error> {
            if seconds <= 0.0 {
                return Ok(None);
            }
            Ok(Some(DurationP::measured_secs_f64(seconds)?))
        };
        run_report(
            size,
            &RunUsage {
                running: measured(args.running_sec)?,
                suspended: measured(args.suspended_sec)?,
                image_build: measured(args.build_sec)?,
                image_gb: args.image_gb,
                image_retained: None,
                suspend_resume_cycles: args.cycles,
                snapshot_gb: None,
                launched,
            },
            &rates,
            today,
            "run",
        )?
    };

    // Not suppressed by `--quiet`: a figure copied into a budget is worse than no figure when
    // the rates behind it are three months old.
    if let Some(warning) = report.staleness() {
        ctx.out.warn(warning);
    }

    let mut comparison_json = Value::Null;
    let mut comparison_text = String::new();
    if args.compare {
        let comparison = compare_residency(
            size,
            std::time::Duration::from_secs_f64(args.hold_sec.max(0.0)),
            args.cycles,
            &rates,
            today,
        )?;
        comparison_text = comparison.render()?;
        comparison_json = comparison_to_json(&comparison)?;
    }

    let mut data = Map::new();
    data.insert("report".into(), report_to_json(&report));
    data.insert("comparison".into(), comparison_json);

    let (kind, _) = response_type("cost");
    let text = [report.render(), comparison_text]
        .into_iter()
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("\n");
    let dense = report_dense(&report);
    Ok(Rendered::ok(kind, data, text, dense))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::envelope::{Format, Output};
    use crate::seam::Infra;
    use microvms_core::SizeClass;

    struct NoAws;

    impl crate::seam::CoreSeam for NoAws {
        fn control_plane(
            &self,
            _region: microvms_core::Region,
        ) -> crate::seam::futures_util_shim::BoxFuture<
            '_,
            Result<microvms_core::control::ControlPlane, microvms_core::Error>,
        > {
            panic!("cost reached the control plane")
        }

        fn open_sandbox(
            &self,
            _region: microvms_core::Region,
            _port: Option<u16>,
        ) -> crate::seam::futures_util_shim::BoxFuture<
            '_,
            Result<microvms_core::sandbox::Sandbox, microvms_core::Error>,
        > {
            panic!("cost opened a sandbox")
        }

        fn attach_session(
            &self,
            _region: microvms_core::Region,
            _attach: crate::seam::Attach,
        ) -> crate::seam::futures_util_shim::BoxFuture<
            '_,
            Result<microvms_core::session::Session, microvms_core::Error>,
        > {
            panic!("cost attached a session")
        }

        fn put_artifact(
            &self,
            _uri: &str,
            _bytes: Vec<u8>,
        ) -> crate::seam::futures_util_shim::BoxFuture<'_, Result<(), microvms_core::Error>>
        {
            panic!("cost uploaded an artifact")
        }
    }

    fn run_cost(args: CostArgs) -> (Rendered, String) {
        let mut out = Output::new(Format::Json, false, Vec::new(), Vec::new());
        let env = |_: &str| None;
        let seam = NoAws;
        let rendered = {
            let mut ctx = Ctx {
                seam: &seam,
                out: &mut out,
                infra: Infra::default(),
                env: &env,
            };
            cost(&mut ctx, &args).expect("the arithmetic holds")
        };
        let stderr = String::from_utf8(out_stderr(out)).expect("utf8");
        (rendered, stderr)
    }

    /// Drains an output's stderr buffer. A helper because `Output`'s fields are private.
    fn out_stderr(out: Output<Vec<u8>, Vec<u8>>) -> Vec<u8> {
        // `Output` owns the writers, so the only way out is to consume it. Written as a
        // function so the two call sites do not each reach for a different trick.
        out.into_streams().1
    }

    fn args(memory: crate::cli::MemoryMib) -> CostArgs {
        CostArgs {
            estimate: false,
            compare: false,
            memory,
            running_sec: 0.0,
            suspended_sec: 0.0,
            build_sec: 0.0,
            image_gb: None,
            cycles: 1,
            hold_sec: 3600.0,
        }
    }

    /// An estimate never prints as a report of something that happened.
    ///
    /// The label is the whole point (COST-10): the same seconds through `--estimate` and
    /// without it produce reports that differ in `fullyMeasured` and in every duration's
    /// provenance, and only one of them is a claim about a clock.
    #[test]
    fn an_estimate_is_never_a_measured_report() {
        let mut plan = args(crate::cli::MemoryMib::Mib2048);
        plan.estimate = true;
        plan.running_sec = 3600.0;
        let (estimate, _) = run_cost(plan);

        let mut timed = args(crate::cli::MemoryMib::Mib2048);
        timed.running_sec = 3600.0;
        let (measured, _) = run_cost(timed);

        assert_eq!(estimate.data["report"]["fullyMeasured"], false);
        assert_eq!(measured.data["report"]["fullyMeasured"], true);
        assert_eq!(estimate.data["report"]["label"], "plan");
        assert_eq!(measured.data["report"]["label"], "run");
        // The dollar figures agree — it is the same arithmetic — which is what makes the label
        // the only difference and therefore the thing that has to be right.
        assert_eq!(
            estimate.data["report"]["total"]["priced"],
            measured.data["report"]["total"]["priced"]
        );
    }

    /// A zero-length *duration* phase produces no line, while the cycle transitions still do.
    ///
    /// The split is not obvious and it is deliberate parity with the oracle, which I checked
    /// rather than inferred: `run_report(size=2048, launched=False, suspend_resume_cycles=1)`
    /// prints exactly `[('suspend', 'GB'), ('resume', 'GB')]`. A duration of zero means "this
    /// phase did not happen" and gets no line, because a line priced at zero claims a
    /// measurement nobody took. A *cycle* count of one is a different statement — it says a
    /// transition happened, and `--cycles` defaults to 1 in both clients — so the snapshot write
    /// and read are real charges with no duration to them.
    ///
    /// My first draft of this test asserted `items.is_empty()` and was simply wrong about the
    /// contract. Recorded rather than quietly fixed, because the failure mode it would have
    /// caused is the interesting one: "correct" it by dropping the cycles default and a caller
    /// comparing residency reads transitions as free, which is the exact inversion
    /// `ResidencyComparison`'s per-cycle figure exists to prevent.
    #[test]
    fn a_zero_length_duration_produces_no_line_while_a_cycle_still_does() {
        let (rendered, _) = run_cost(args(crate::cli::MemoryMib::Mib2048));
        let items = rendered.data["report"]["items"]
            .as_array()
            .expect("an array");
        let phases: Vec<&str> = items
            .iter()
            .map(|item| item["phase"].as_str().unwrap_or_default())
            .collect();
        assert_eq!(
            phases,
            ["suspend", "resume"],
            "the one default cycle's two transitions, and nothing a clock would have timed"
        );
        // No duration on either, which is what says these are transfers rather than phases.
        assert!(
            items.iter().all(|item| item["duration"].is_null()),
            "{items:?}"
        );
        assert_eq!(rendered.data["comparison"], Value::Null);

        // And `--cycles 0` really does produce an empty report, so the lines above are the
        // cycle's rather than something unconditional.
        let mut no_cycles = args(crate::cli::MemoryMib::Mib2048);
        no_cycles.cycles = 0;
        let (empty, _) = run_cost(no_cycles);
        assert!(
            empty.data["report"]["items"]
                .as_array()
                .expect("an array")
                .is_empty(),
            "{}",
            empty.data["report"]["items"]
        );
        assert_eq!(empty.data["report"]["total"]["priced"], "0");
    }

    /// `--compare` carries the per-cycle cost and the break-even hold beside the ratio.
    ///
    /// The ratio alone reads as "suspend constantly", and a pool that churns every few seconds
    /// spends more on transitions than it saves on residency — which is what the break-even
    /// number is for.
    #[test]
    fn compare_carries_its_own_counter_argument() {
        let mut with_compare = args(crate::cli::MemoryMib::Mib2048);
        with_compare.compare = true;
        with_compare.hold_sec = 86_400.0;
        let (rendered, _) = run_cost(with_compare);

        let comparison = &rendered.data["comparison"];
        assert!(comparison["ratio"].is_string(), "{comparison}");
        assert!(comparison["perCycleUsd"].is_string(), "{comparison}");
        assert!(
            comparison["breakEvenSeconds"].as_f64().expect("a number") > 0.0,
            "{comparison}"
        );
        assert!(
            rendered.text.contains("avoid churn"),
            "the conclusion has to travel with the numbers: {}",
            rendered.text
        );
    }

    /// A smaller baseline really is a cheaper report, so `--memory` is not decorative.
    ///
    /// This is what makes the closed set worth enforcing: the flag changes a number, so a
    /// value core would reject reaching the arithmetic would produce a *plausible* wrong
    /// figure rather than an error.
    #[test]
    fn a_smaller_baseline_produces_a_smaller_figure() {
        let mut small = args(crate::cli::MemoryMib::Mib512);
        small.running_sec = 3600.0;
        let mut large = args(crate::cli::MemoryMib::Mib8192);
        large.running_sec = 3600.0;

        let (small, _) = run_cost(small);
        let (large, _) = run_cost(large);
        let figure = |rendered: &Rendered| -> f64 {
            rendered.data["report"]["total"]["priced"]
                .as_str()
                .expect("a string")
                .parse()
                .expect("a figure")
        };
        assert!(
            figure(&small) < figure(&large),
            "{} vs {}",
            figure(&small),
            figure(&large)
        );
        assert_eq!(
            small.data["report"]["size"]["baselineMib"],
            SizeClass::Mib512.baseline_mib()
        );
    }

    /// The compute line bills the baseline, never the peak.
    ///
    /// The 2 GB class reports 8 GB in the guest, so reading the peak would overstate the
    /// memory line by exactly 4x — and it would look entirely reasonable.
    #[test]
    fn the_memory_line_bills_the_baseline_and_not_the_peak() {
        let mut timed = args(crate::cli::MemoryMib::Mib2048);
        timed.running_sec = 3600.0;
        let (rendered, _) = run_cost(timed);
        let memory = rendered.data["report"]["items"]
            .as_array()
            .expect("items")
            .iter()
            .find(|item| item["line"] == "memory")
            .expect("a memory line")
            .clone();
        // 2 GB for 3600 s is 7200 GB-seconds; the peak would be 28800.
        assert_eq!(memory["quantity"], "7200");
        assert_eq!(memory["unit"], "GB-seconds");
    }
}
