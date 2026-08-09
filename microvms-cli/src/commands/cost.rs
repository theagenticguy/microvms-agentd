// SPDX-License-Identifier: Apache-2.0
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

    // A negative duration is refused here, before any arithmetic, because that is what the
    // oracle does: `cost.py:118-122` raises from `Duration.__post_init__` and the CLI reports
    // ERR_INVALID_ARG / exit 2. Checked up front rather than left to the constructor, because
    // the constructor is only reached for a phase the report keeps — and a phase filtered out
    // for being non-positive would take its own refusal with it, which is how `--build-sec -5`
    // came to exit 0 with a report of a run that did not happen.
    for (flag, seconds) in [
        ("--running-sec", args.running_sec),
        ("--suspended-sec", args.suspended_sec),
        ("--build-sec", args.build_sec),
    ] {
        if seconds < 0.0 {
            // The message is the oracle's verbatim, `{seconds:?}` rather than `{seconds}`
            // because Rust's `Display` prints `-5` for `-5.0` where Python prints `-5.0`.
            // Which flag it was goes in a *suggestion*, which is the CLI-shaped half —
            // the library says what went wrong and the CLI says which flag addresses it —
            // rather than into the message an agent may be matching on.
            return Err(
                crate::exit::classify(&microvms_core::Error::invalid_arg(format!(
                    "a duration cannot be negative: {seconds:?}s"
                )))
                .suggest(format!("{flag} was {seconds:?}")),
            );
        }
    }

    // A launch happened if there is running time or an image, matching `cli.py:2168`'s
    // `bool(running_sec or image_gb)`. Both halves are falsy at zero there, so `--image-gb 0`
    // does *not* launch — an `is_some()` here would have made a zero-sized image claim a
    // snapshot read the plan never pays for. A launch reads a snapshot, so claiming one that
    // did not happen adds a transfer line.
    let launched = args.running_sec > 0.0 || args.image_gb.is_some_and(|gb| gb != 0.0);

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
            // `cli.py:646`'s default. Not "plan": the label is the report's own name for
            // itself and the two clients' reports have to be substitutable.
            "estimate",
        )?
    } else {
        // Zero means "this phase did not happen", not "a phase that cost nothing": a
        // zero-length line on a report claims a measurement nobody took. Exactly zero,
        // matching the oracle's `if running_sec else None` — negative is already refused
        // above, so this is not the place a sign error is caught.
        let measured = |seconds: f64| -> Result<Option<DurationP>, microvms_core::Error> {
            if seconds == 0.0 {
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
        let (result, stderr) = try_cost(args);
        (result.expect("the arithmetic holds"), stderr)
    }

    /// The same drive, keeping the failure. The refusal path is a contract too.
    fn try_cost(args: CostArgs) -> (Result<Rendered, CliError>, String) {
        let mut out = Output::new(Format::Json, false, Vec::new(), Vec::new());
        let env = |_: &str| None;
        let seam = NoAws;
        let result = {
            let mut ctx = Ctx {
                seam: &seam,
                out: &mut out,
                infra: Infra::default(),
                env: &env,
            };
            cost(&mut ctx, &args)
        };
        let stderr = String::from_utf8(out_stderr(out)).expect("utf8");
        (result, stderr)
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
        // `estimate`, which is `cli.py:646`'s default label, not "plan". The label is the
        // report's own name for itself and it is the field a consumer reads to tell a plan
        // from a receipt, so the two clients cannot disagree on the word.
        assert_eq!(estimate.data["report"]["label"], "estimate");
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

    /// A negative duration is refused with ERR_INVALID_ARG rather than dropped.
    ///
    /// The old `seconds <= 0.0` filter treated `-5` the same as `0`: the phase vanished and
    /// the CLI printed a clean report of a run that could not have happened, exit 0. That is
    /// the worst available answer — an inverted clock is a bug in the caller's timing and a
    /// report is the last place it should be laundered into silence. What the Python oracle
    /// printed for `cost --json --running-sec=-5`, and the exit code it returned:
    ///
    /// ```text
    /// {"status": "error", "error": "a duration cannot be negative: -5.0s",
    ///  "code": "ERR_INVALID_ARG", "exitCode": 2, ...}
    /// 2
    /// ```
    ///
    /// A transcript rather than a command: that client was deleted once this one had driven
    /// the live suite green, so this is the record of what it answered.
    ///
    /// All three flags and both paths, because `--estimate` and the measured path reached
    /// the check through different constructors and the oracle refused on all six.
    ///
    /// **Falsification** — restore `if seconds <= 0.0 { return Ok(None) }` and drop the
    /// up-front loop: every case here goes red on `expect_err`, while the report the CLI
    /// then prints still balances. Verified.
    #[test]
    fn a_negative_duration_is_refused_rather_than_silently_dropped() {
        for estimate in [false, true] {
            for flag in ["running", "suspended", "build"] {
                let mut bad = args(crate::cli::MemoryMib::Mib2048);
                bad.estimate = estimate;
                match flag {
                    "running" => bad.running_sec = -5.0,
                    "suspended" => bad.suspended_sec = -5.0,
                    _ => bad.build_sec = -5.0,
                }
                let (result, _) = try_cost(bad);
                let error = result.expect_err(&format!("--{flag}-sec -5 (estimate={estimate})"));
                assert_eq!(error.exit, crate::exit::Exit::InvalidArg);
                assert_eq!(error.code(), "ERR_INVALID_ARG");
                assert_eq!(i32::from(error.exit as u8), 2);
                // The oracle's message verbatim, `-5.0s` and not `-5s`.
                assert_eq!(error.message, "a duration cannot be negative: -5.0s");
                // And which flag it was, so the remedy is actionable.
                assert!(
                    error
                        .suggestions
                        .iter()
                        .any(|line| line.contains(&format!("--{flag}-sec"))),
                    "{:?}",
                    error.suggestions
                );
            }
        }

        // Exactly zero is *not* an error: it means "this phase did not happen" and the
        // phase is omitted, which is the oracle's `if running_sec else None`. Without this
        // half the fix above could have refused zero too and every default invocation would
        // exit 2.
        let mut zeroes = args(crate::cli::MemoryMib::Mib2048);
        zeroes.cycles = 0;
        let (rendered, _) = run_cost(zeroes);
        assert!(
            rendered.data["report"]["items"]
                .as_array()
                .expect("an array")
                .is_empty(),
            "zero omits the phase rather than refusing it: {}",
            rendered.data["report"]["items"]
        );
    }

    /// A zero-sized image does not launch, because `0` is falsy in the oracle.
    ///
    /// `cli.py:2168` is `launched=bool(running_sec or image_gb)`, so `--image-gb 0` leaves
    /// `launched` false. An `is_some()` test here read `Some(0.0)` as truthy and added a
    /// `launch` line — a snapshot read charged against an image with no bytes in it.
    /// Measured against the oracle, which prints
    /// `[('image-build', '0'), ('image-storage', '0.00')]` for
    /// `cost --json --image-gb=0 --cycles=0` and adds `launch` only at `--image-gb=2`.
    ///
    /// **Falsification** — put `args.image_gb.is_some()` back and the first assertion is red
    /// on a `launch` phase the oracle does not emit. Verified.
    #[test]
    fn a_zero_sized_image_does_not_launch() {
        let phases = |args: CostArgs| -> Vec<String> {
            let (rendered, _) = run_cost(args);
            rendered.data["report"]["items"]
                .as_array()
                .expect("an array")
                .iter()
                .map(|item| item["phase"].as_str().unwrap_or_default().to_string())
                .collect()
        };

        let mut zero_image = args(crate::cli::MemoryMib::Mib2048);
        zero_image.image_gb = Some(0.0);
        zero_image.cycles = 0;
        assert_eq!(
            phases(zero_image),
            ["image-build", "image-storage"],
            "a zero-sized image builds and stores nothing, and never launches"
        );

        // A real image does launch, so the check above is about the zero rather than about
        // the flag being ignored.
        let mut real_image = args(crate::cli::MemoryMib::Mib2048);
        real_image.image_gb = Some(2.0);
        real_image.cycles = 0;
        assert_eq!(
            phases(real_image),
            ["image-build", "image-storage", "launch"]
        );
    }
}
