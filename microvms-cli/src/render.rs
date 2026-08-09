//! The four renderers over one result type: JSON, dense, plain, and (in [`crate::tui`]) a
//! ratatui frame.
//!
//! # Rendering is separate from command logic, and the reason is arithmetic honesty
//!
//! The cost surface is the part where a renderer can lie. `Amount::Unpriced` must not
//! serialize as `0.0` — a consumer summing the column would produce an invoice that flatters
//! us — so [`line_to_json`] emits **no `usd` key at all** for an unpriced line rather than a
//! null, because a null gets summed as zero by anything permissive. That is the one
//! arithmetic this file refuses to enable, and it is the same decision
//! `microvms-core/src/cost.rs:624` makes by returning `Option` from `Amount::estimate`
//! instead of defaulting to zero.
//!
//! Every **dollar** figure crosses into JSON as a string, never a number. `Decimal` to `f64`
//! to `serde_json::Number` is a lossy step for a figure whose exactness is the whole point of
//! the type, and `cli.py:718` writes `str(...)` for the same reason.
//!
//! Every **seconds** figure crosses as a JSON number: `breakEvenSeconds`, a line item's
//! `duration.seconds` (`cli.py:743`), and a comparison's `holdSeconds` (`cli.py:2189`). Each
//! goes through an accessor core named `_f64`, so the lossy step is a visible call rather than
//! an implicit coercion. The split is not a compromise between two half-rules — it is which
//! consumer is being protected. A caller summing a dollar column must be stopped from doing
//! float arithmetic on money; a caller comparing a duration against a timeout must not have
//! to discover that one of the two clients quotes seconds in quotes.

use microvms_core::cost::{Amount, CostReport, LineItem, ResidencyComparison};
use serde_json::{Map, Value, json};

/// A cost report as JSON, keeping every label the report carries.
///
/// Keys are `cli.py:688`'s `report_to_dict` verbatim, because the conformance oracle compares
/// them and the two clients' `--json` output has to be substitutable.
pub fn report_to_json(report: &CostReport) -> Value {
    let size = report.size();
    let total = report.total();
    json!({
        "label": report.label(),
        "size": {
            "baselineMib": size.baseline_mib(),
            "baselineVcpu": size.baseline_vcpu(),
            "peakMib": size.peak_mib(),
            "peakVcpu": size.peak_vcpu(),
            // The Python's `describe()`; core's is `Display`, which names both numbers or
            // neither for the reason its docs give — naming only the peak invites budgeting
            // for memory nobody is billed for.
            "describe": size.to_string(),
        },
        "rates": {
            "region": report.rates().region().as_str(),
            "retrieved": report.rates().retrieved().to_string(),
            "sourceUrl": report.rates().source_url(),
        },
        // Not "cost": these are estimates derived from published rates, and the field name is
        // the only place that distinction survives a copy-paste.
        "estimated": true,
        "fullyMeasured": report.fully_measured(),
        "complete": report.is_complete(),
        "staleness": report.staleness(),
        "items": report.items().iter().map(line_to_json).collect::<Vec<_>>(),
        "total": {
            // The floor, as a string. `AtLeast`'s floor and `Exact`'s figure are the same
            // field on purpose: a consumer that ignores `isLowerBound` gets a number that is
            // never an over-statement.
            "priced": total.floor().amount().to_string(),
            "isLowerBound": total.is_lower_bound(),
            "render": total.to_string(),
        },
    })
}

/// One line item as JSON. See the module docs on the missing `usd` key.
pub fn line_to_json(item: &LineItem) -> Value {
    let amount = match &item.amount {
        Amount::Estimated(usd) => json!({
            "kind": "estimated-usd",
            "usd": usd.amount().to_string(),
        }),
        Amount::Unpriced { reason } => json!({
            "kind": "unpriced",
            "reason": reason,
        }),
    };
    json!({
        "phase": item.phase.as_str(),
        "line": item.line.map(|line| line.as_str()),
        "quantity": item.quantity.to_string(),
        "unit": item.unit,
        "amount": amount,
        "duration": item.duration.map(|duration| json!({
            // A JSON **number**, because `cli.py:743` emits `item.duration.seconds`, which is
            // a float. Seconds are not dollars: the exactness argument that keeps every money
            // figure a string does apply to a retention span derived from a rate table's
            // month, but parity wins here, and it wins for the reason `breakEvenSeconds` was
            // already a number — a consumer doing arithmetic on a duration must not have to
            // branch on which client produced the envelope. The accessor is named `_f64` so
            // the lossy step is visible, which is core's own convention for exactly this.
            "seconds": duration.seconds_f64(),
            "provenance": duration.provenance().as_str(),
        })),
        "note": item.note,
    })
}

/// A residency comparison as JSON.
pub fn comparison_to_json(comparison: &ResidencyComparison) -> Result<Value, microvms_core::Error> {
    Ok(json!({
        // A number, matching `cli.py:2189`'s `residency.hold.seconds`, through the same
        // `_f64` accessor as the line items' seconds and for the same reason.
        "holdSeconds": comparison.hold().seconds_f64(),
        "cycles": comparison.cycles(),
        "running": report_to_json(comparison.running()),
        "suspended": report_to_json(comparison.suspended()),
        "ratio": comparison.ratio().to_string(),
        "perCycleUsd": comparison.per_cycle()?.amount().to_string(),
        // The one f64 in the envelope, and core's accessor is named `_f64` precisely so this
        // is a visible lossy step rather than a silent one.
        "breakEvenSeconds": comparison.break_even_seconds_f64()?,
        "render": comparison.render()?,
    }))
}

/// The dense rendering of a cost report: `phase\tunit\tamount`, one line per item.
///
/// Three fields and no total row, which is `cli.py:2203-2207` exactly. Both divergences the
/// shape used to carry were the plausible kind. The extra `quantity` field put the amount in
/// field four, so `cut -f3` — the column the module used to name — read the *unit* out of one
/// client and the amount out of the other. And the appended `total` row is a line whose first
/// field is not a phase, so `awk` over the phase column of a dense report sees a phase called
/// `total`; the total is in the JSON envelope and in the plain rendering, which is where a
/// consumer that wants it should read it, rather than in the stream a script is summing.
///
/// An unpriced line reads the literal `unpriced` in the amount column rather than a blank or
/// a zero, so `cut -f3 | paste -sd+ | bc` on this output fails loudly instead of producing a
/// total that flatters us.
pub fn report_dense(report: &CostReport) -> String {
    report
        .items()
        .iter()
        .map(|item| {
            let amount = match &item.amount {
                Amount::Estimated(usd) => usd.amount().to_string(),
                Amount::Unpriced { .. } => "unpriced".to_string(),
            };
            format!("{}\t{}\t{}", item.phase.as_str(), item.unit, amount)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

// ── run ─────────────────────────────────────────────────────────────────────

/// Everything `run` learned, so the handler only formats.
///
/// A struct rather than a tuple because the fields are read by name in four renderers, and a
/// positional shape is how the fourth one prints the exit code where the duration belongs.
#[derive(Clone, Debug, Default)]
pub struct RunOutcome {
    pub image_identifier: Option<String>,
    pub image_name: Option<String>,
    pub microvm_id: Option<String>,
    pub endpoint: Option<String>,
    pub agent_token: Option<String>,
    pub exec_exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub truncated: bool,
    pub build_seconds: f64,
    pub running_seconds: f64,
    pub kept: bool,
    pub leaked: Vec<String>,
    pub cost: Option<Value>,
}

impl RunOutcome {
    /// The success envelope's `data`.
    pub fn to_data(&self) -> Map<String, Value> {
        let mut data = Map::new();
        data.insert("imageIdentifier".into(), json!(self.image_identifier));
        data.insert("imageName".into(), json!(self.image_name));
        data.insert("microvmId".into(), json!(self.microvm_id));
        data.insert("endpoint".into(), json!(self.endpoint));
        // The agent token is in the payload deliberately: `run --keep` is followed by
        // `microvm exec --agent-token`, and a caller who cannot read it cannot use the VM
        // they are now paying for. It is never in a progress line, never in a `Debug`, and
        // core keeps it out of both too.
        data.insert("agentToken".into(), json!(self.agent_token));
        data.insert("execExitCode".into(), json!(self.exec_exit_code));
        data.insert("stdout".into(), json!(self.stdout));
        data.insert("stderr".into(), json!(self.stderr));
        data.insert("truncated".into(), json!(self.truncated));
        data.insert("buildSeconds".into(), json!(self.build_seconds));
        data.insert("runningSeconds".into(), json!(self.running_seconds));
        data.insert("kept".into(), json!(self.kept));
        data.insert("leaked".into(), json!(self.leaked));
        data.insert("cost".into(), self.cost.clone().unwrap_or(Value::Null));
        data
    }

    /// The human view. Output first, because output is what the caller asked for.
    pub fn render(&self, dense: bool) -> String {
        if dense {
            // TSV with the exit code first, so a shell reads field one without parsing.
            return [
                format!(
                    "exit\t{}",
                    self.exec_exit_code
                        .map(|code| code.to_string())
                        .unwrap_or_default()
                ),
                format!("microvm\t{}", self.microvm_id.clone().unwrap_or_default()),
                format!(
                    "image\t{}",
                    self.image_identifier.clone().unwrap_or_default()
                ),
                format!("running_sec\t{:.1}", self.running_seconds),
                format!("leaked\t{}", self.leaked.join(",")),
            ]
            .join("\n");
        }
        let mut lines: Vec<String> = Vec::new();
        if !self.stdout.is_empty() {
            lines.push(self.stdout.trim_end_matches('\n').to_string());
        }
        if !self.stderr.is_empty() {
            lines.push(self.stderr.trim_end_matches('\n').to_string());
        }
        if let Some(code) = self.exec_exit_code {
            lines.push(format!("exit code: {code}"));
        }
        if self.truncated {
            lines.push("note: output was truncated at the daemon's cap".to_string());
        }
        if self.kept {
            lines.push(format!(
                "kept: microvm {}, image {}",
                self.microvm_id.clone().unwrap_or_default(),
                self.image_identifier.clone().unwrap_or_default(),
            ));
            if let (Some(id), Some(endpoint), Some(token)) =
                (&self.microvm_id, &self.endpoint, &self.agent_token)
            {
                lines.push(format!(
                    "  exec against it: microvm exec '<cmd>' --endpoint {endpoint} \
                     --agent-token {token} --microvm-id {id}"
                ));
                lines.push(format!("  release it: microvm terminate {id}"));
            }
        }
        for identifier in &self.leaked {
            lines.push(format!("LEAKED (still billing): {identifier}"));
        }
        if let Some(cost) = &self.cost
            && let Some(render) = cost["total"]["render"].as_str()
        {
            lines.push(format!("cost: {render}"));
        }
        if lines.is_empty() {
            return "done".to_string();
        }
        lines.join("\n")
    }
}

// ── doctor ──────────────────────────────────────────────────────────────────

/// One prerequisite, its verdict, and what to do about it.
///
/// `ok: false` with `fatal: false` is a warning — a region we have not seen listed, a
/// Terraform stack that may live elsewhere. The distinction matters because the exit code is
/// derived from the fatal ones only, and a CLI that failed `doctor` over an advisory would
/// train people to ignore it.
#[derive(Clone, Debug)]
pub struct Check {
    pub name: &'static str,
    pub ok: bool,
    pub detail: String,
    pub fatal: bool,
    pub remedy: String,
}

impl Check {
    pub fn pass(name: &'static str, detail: impl Into<String>) -> Self {
        Self {
            name,
            ok: true,
            detail: detail.into(),
            fatal: true,
            remedy: String::new(),
        }
    }

    pub fn fail(name: &'static str, detail: impl Into<String>, remedy: impl Into<String>) -> Self {
        Self {
            name,
            ok: false,
            detail: detail.into(),
            fatal: true,
            remedy: remedy.into(),
        }
    }

    /// A non-fatal finding: reported, but does not decide the exit code.
    #[must_use]
    pub fn advisory(mut self) -> Self {
        self.fatal = false;
        self
    }

    pub fn to_json(&self) -> Value {
        json!({
            "name": self.name,
            "ok": self.ok,
            "detail": self.detail,
            "fatal": self.fatal,
            "remedy": self.remedy,
        })
    }
}

/// Whether every fatal check passed.
pub fn healthy(checks: &[Check]) -> bool {
    checks.iter().filter(|check| check.fatal).all(|c| c.ok)
}

/// The human rendering of a doctor run.
pub fn render_doctor(checks: &[Check], dense: bool) -> String {
    if dense {
        return checks
            .iter()
            .map(|check| {
                format!(
                    "{}\t{}\t{}",
                    check.name,
                    if check.ok { "ok" } else { "fail" },
                    check.detail
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
    }
    let mut lines: Vec<String> = Vec::new();
    for check in checks {
        // Three marks rather than two: an advisory rendered as FAIL is how a caller learns to
        // ignore the whole command.
        let mark = if check.ok {
            "PASS"
        } else if check.fatal {
            "FAIL"
        } else {
            "WARN"
        };
        lines.push(format!("{mark}  {}: {}", check.name, check.detail));
        if !check.ok && !check.remedy.is_empty() {
            lines.push(format!("      -> {}", check.remedy));
        }
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use microvms_core::SizeClass;
    use microvms_core::cost::{
        CalendarDate, DurationP, PlanUsage, RunUsage, estimate_run, pinned_rates, run_report,
    };

    /// The pinned rate table's own retrieval date, so no test is stale-dependent.
    fn fresh_day() -> CalendarDate {
        pinned_rates().retrieved()
    }

    fn a_report() -> CostReport {
        run_report(
            SizeClass::Mib2048,
            &RunUsage {
                running: Some(DurationP::Measured(std::time::Duration::from_secs(3600))),
                image_gb: Some(2.0),
                image_build: Some(DurationP::Measured(std::time::Duration::from_secs(600))),
                ..RunUsage::launched()
            },
            &pinned_rates(),
            fresh_day(),
            "run img",
        )
        .expect("a report")
    }

    /// **The unpriced line has no `usd` key at all.**
    ///
    /// Not a null, which anything permissive sums as zero — and the build is the one phase
    /// AWS does not publish a rate for, so this is the field that stops a create-and-destroy
    /// report from reading as compute-only.
    ///
    /// **Falsification** — emit `"usd": null` for the unpriced arm and this test is red on
    /// the key's presence while every total still looks right. Verified; see the packet's
    /// guard proofs.
    #[test]
    fn an_unpriced_line_omits_the_usd_key_rather_than_reporting_zero() {
        let json = report_to_json(&a_report());
        let items = json["items"].as_array().expect("items");
        let build = items
            .iter()
            .find(|item| item["phase"] == "image-build")
            .expect("the build line is always present when an image is");
        assert_eq!(build["amount"]["kind"], "unpriced");
        assert!(
            build["amount"].get("usd").is_none(),
            "a null would be summed as zero: {build}"
        );
        assert!(
            build["amount"]["reason"]
                .as_str()
                .expect("a reason")
                .contains("does not publish"),
            "{build}"
        );
        // And the total says it is a floor rather than a figure.
        assert_eq!(json["total"]["isLowerBound"], true);
        assert!(
            json["total"]["render"]
                .as_str()
                .expect("a render")
                .contains("at least"),
            "{}",
            json["total"]["render"]
        );
        assert_eq!(json["complete"], false);
    }

    /// Every dollar figure is a string, and every priced line has one. Every *seconds*
    /// figure is a number.
    ///
    /// A JSON number for a dollar figure is a `Decimal` that went through an f64, which is the
    /// exactness the type exists to hold. Asserted over every item so a single unconverted
    /// field fails.
    ///
    /// The seconds half is the same assertion pointed the other way, and it is pinned here
    /// rather than left implicit because the two rules look contradictory side by side and the
    /// natural "tidy-up" is to make them agree. The Python oracle's `cli.py:743` emitted
    /// `duration.seconds` as a float, so a string here is a field a consumer has to branch on
    /// by client — which is exactly the substitutability `breakEvenSeconds` was already a
    /// number to preserve.
    ///
    /// The oracle's recorded output for `cost --json --running-sec=3600`, read off
    /// `data.report.items[0].duration.seconds`:
    ///
    /// ```text
    /// 3600.0
    /// ```
    ///
    /// A transcript rather than a command, because that client was deleted once this one had
    /// driven the live suite green. The figure is what it printed; git history is where the
    /// code that printed it is.
    ///
    /// **Falsification** — put `.to_string()` back on either seconds field and the matching
    /// arm here is red while every dollar assertion stays green, which is what says the two
    /// halves are independent. Verified.
    #[test]
    fn every_dollar_is_a_string_and_every_seconds_figure_is_a_number() {
        let json = report_to_json(&a_report());
        let mut durations = 0;
        for item in json["items"].as_array().expect("items") {
            if let Some(usd) = item["amount"].get("usd") {
                assert!(usd.is_string(), "{item}");
                usd.as_str()
                    .expect("a string")
                    .parse::<f64>()
                    .expect("still parses as a figure");
            }
            assert!(item["quantity"].is_string(), "{item}");
            if !item["duration"].is_null() {
                durations += 1;
                assert!(
                    item["duration"]["seconds"].is_number(),
                    "a number, matching cli.py:743: {item}"
                );
            }
        }
        assert!(
            durations > 0,
            "vacuous unless a duration was present: {json}"
        );
        assert!(json["total"]["priced"].is_string());

        // The exact values, so this is parity rather than merely a type check. Checked against
        // the oracle on this same report shape — `600` for the timed build and `604800.0` for
        // the retention floor — read through `as_f64` because the oracle's build figure is a
        // JSON int where its retention figure is a float, and both are numbers.
        let seconds: Vec<f64> = json["items"]
            .as_array()
            .expect("items")
            .iter()
            .filter(|item| !item["duration"].is_null())
            .map(|item| item["duration"]["seconds"].as_f64().expect("a number"))
            .collect();
        assert!(seconds.contains(&600.0), "{seconds:?}");
        assert!(seconds.contains(&604_800.0), "{seconds:?}");

        // And `holdSeconds` on the comparison, which is the second field `cli.py` (`:2189`)
        // emits as a number and the one a scheduler compares against a timeout.
        let comparison = comparison_to_json(
            &microvms_core::cost::compare_residency(
                SizeClass::Mib2048,
                std::time::Duration::from_secs(3600),
                1,
                &pinned_rates(),
                fresh_day(),
            )
            .expect("a comparison"),
        )
        .expect("renders");
        assert_eq!(comparison["holdSeconds"].as_f64(), Some(3600.0));
        assert!(comparison["breakEvenSeconds"].is_number());
        // The money beside it stays a string, which is the whole point of the split.
        assert!(comparison["perCycleUsd"].is_string());
    }

    /// A measured run labels the phases a clock timed measured, and the one nobody timed
    /// projected.
    ///
    /// The mixed case is the interesting one and my first draft got it wrong by asserting every
    /// duration on a measured report is measured. It is not: `image-storage`'s duration defaults
    /// to the documented one-week minimum retention, and core marks it
    /// `Projected` because — in its own words — "nobody timed that week either". So a
    /// create-and-destroy report is honestly a mixture, and `fullyMeasured` is `false` for it.
    ///
    /// Worth pinning precisely because the wrong version reads better: "a measured report is
    /// measured" is the assertion someone would write, and it would force the retention default
    /// to lie about a week nobody observed.
    #[test]
    fn a_measured_report_labels_the_timed_phases_measured_and_the_retention_projected() {
        let measured = report_to_json(&a_report());
        let labelled: Vec<(&str, &str)> = measured["items"]
            .as_array()
            .expect("items")
            .iter()
            .filter(|item| !item["duration"].is_null())
            .map(|item| {
                (
                    item["phase"].as_str().unwrap_or_default(),
                    item["duration"]["provenance"].as_str().unwrap_or_default(),
                )
            })
            .collect();
        assert!(
            labelled.contains(&("running", "measured")),
            "a clock timed the run: {labelled:?}"
        );
        assert!(
            labelled.contains(&("image-build", "measured")),
            "a clock timed the build: {labelled:?}"
        );
        assert!(
            labelled.contains(&("image-storage", "projected")),
            "nobody timed the one-week retention floor: {labelled:?}"
        );
        assert_eq!(
            measured["fullyMeasured"], false,
            "a report carrying a projected retention is not fully measured"
        );

        let estimate = estimate_run(
            SizeClass::Mib2048,
            &PlanUsage {
                running_seconds: 3600.0,
                ..PlanUsage::launched()
            },
            &pinned_rates(),
            fresh_day(),
            "plan",
        )
        .expect("an estimate");
        let json = report_to_json(&estimate);
        assert_eq!(json["fullyMeasured"], false);
        // *Every* duration, with no exception, which is the difference from the measured report
        // above: an estimate has nothing a clock touched.
        for item in json["items"].as_array().expect("items") {
            if !item["duration"].is_null() {
                assert_eq!(item["duration"]["provenance"], "projected", "{item}");
            }
        }

        // And a report whose only phase *was* timed is fully measured, so the flag is a real
        // comparison rather than something that is always false.
        let timed_only = run_report(
            SizeClass::Mib2048,
            &RunUsage {
                running: Some(DurationP::Measured(std::time::Duration::from_secs(60))),
                ..RunUsage::launched()
            },
            &pinned_rates(),
            fresh_day(),
            "run",
        )
        .expect("a report");
        assert_eq!(report_to_json(&timed_only)["fullyMeasured"], true);
        // And `estimated: true` is on both, because dollars are always estimates.
        assert_eq!(measured["estimated"], true);
        assert_eq!(json["estimated"], true);
    }

    /// The report's key set is `cli.py`'s `report_to_dict` key for key.
    ///
    /// Pinned because the conformance oracle reads these names: a rename here is a driver
    /// that reports every cost check as missing rather than as wrong.
    #[test]
    fn the_report_json_carries_the_pythons_key_set() {
        let json = report_to_json(&a_report());
        let mut keys: Vec<&String> = json.as_object().expect("object").keys().collect();
        keys.sort();
        assert_eq!(
            keys,
            [
                "complete",
                "estimated",
                "fullyMeasured",
                "items",
                "label",
                "rates",
                "size",
                "staleness",
                "total",
            ]
        );
        let mut size_keys: Vec<&String> =
            json["size"].as_object().expect("object").keys().collect();
        size_keys.sort();
        assert_eq!(
            size_keys,
            [
                "baselineMib",
                "baselineVcpu",
                "describe",
                "peakMib",
                "peakVcpu"
            ]
        );
        let mut item_keys: Vec<&String> = json["items"][0]
            .as_object()
            .expect("object")
            .keys()
            .collect();
        item_keys.sort();
        assert_eq!(
            item_keys,
            [
                "amount", "duration", "line", "note", "phase", "quantity", "unit"
            ]
        );
    }

    /// The dense cost rendering is the oracle's three fields, with no total row.
    ///
    /// The Python oracle's `cli.py:2203-2207` emitted `phase\tunit\tamount` and stopped. Both
    /// divergences this pins are silent under a type check and loud under a pipe: a fourth
    /// `quantity` field moved the amount to field four, so the same `cut -f3` read the *unit*
    /// from one client and the amount from the other, and the appended `total` row put a line
    /// in the stream whose first field is not a phase — `awk '$1=="running"'` is fine, `wc -l`
    /// and any per-phase aggregation are not.
    ///
    /// The field names are checked positionally against what the oracle printed for
    /// `cost --dense --running-sec=3600 --build-sec=300 --image-gb=2`:
    ///
    /// ```text
    /// image-build\tseconds\tunpriced
    /// image-storage\tGB-months\t0.0373...
    /// ...
    /// ```
    ///
    /// A transcript rather than a command, for the reason the seconds-vs-dollars test above
    /// gives: that client is git history now.
    ///
    /// **Falsification** — add the quantity field back, or push the total row back on, and this
    /// is red on the field count or the line count respectively. Verified for both.
    #[test]
    fn the_dense_cost_rendering_is_three_fields_and_no_total() {
        let dense = report_dense(&a_report());
        let rows: Vec<Vec<&str>> = dense
            .lines()
            .map(|line| line.split('\t').collect())
            .collect();
        assert!(!rows.is_empty(), "{dense}");
        for row in &rows {
            assert_eq!(
                row.len(),
                3,
                "phase, unit, amount and nothing else: {row:?}"
            );
        }
        // No total row: the last line is a phase like every other, so the line count is the
        // item count.
        assert_eq!(rows.len(), a_report().items().len(), "{dense}");
        assert!(
            !dense.contains("lower-bound") && !dense.contains("\nexact"),
            "the total belongs to the JSON envelope and the plain render, not this stream: \
             {dense}"
        );

        // Field two is the unit and field three is the amount, in the oracle's positions.
        let build = rows
            .iter()
            .find(|row| row[0] == "image-build")
            .expect("the build line");
        assert_eq!(build[1], "seconds");
        // And the unpriced line writes the word rather than a number, so
        // `cut -f3 | paste -sd+ | bc` fails loudly instead of totalling to something that
        // flatters us.
        assert_eq!(build[2], "unpriced");

        let storage = rows
            .iter()
            .find(|row| row[0] == "image-storage")
            .expect("the storage line");
        assert_eq!(storage[1], "GB-months");
        assert!(
            storage[2].parse::<f64>().is_ok(),
            "a priced line's field three is the figure: {storage:?}"
        );
    }

    /// A comparison carries its own counter-argument: the per-cycle cost and the break-even
    /// hold.
    ///
    /// Without them "100x cheaper suspended" reads as "suspend constantly", and a pool that
    /// churns every few seconds spends more on transitions than it saves on residency.
    #[test]
    fn a_comparison_carries_the_per_cycle_cost_and_the_break_even_hold() {
        let comparison = microvms_core::cost::compare_residency(
            SizeClass::Mib2048,
            std::time::Duration::from_secs(3600),
            1,
            &pinned_rates(),
            fresh_day(),
        )
        .expect("a comparison");
        let json = comparison_to_json(&comparison).expect("renders");
        assert!(json["perCycleUsd"].is_string());
        assert!(
            json["ratio"].is_string(),
            "the ratio is not money but is decimal"
        );
        let break_even = json["breakEvenSeconds"].as_f64().expect("a number");
        assert!(break_even > 0.0, "{break_even}");
        assert!(
            json["render"]
                .as_str()
                .expect("a render")
                .contains("avoid churn"),
            "the conclusion the numbers support has to travel with them"
        );
    }

    /// A staleness warning survives into the envelope.
    ///
    /// The warning has to reach whatever renders it: a library caller with a log filter and a
    /// CLI that only wrote stderr would each lose it on their own, which is why core carries
    /// it on the report.
    #[test]
    fn a_stale_rate_table_puts_its_warning_in_the_envelope() {
        let rates = pinned_rates();
        let retrieved = rates.retrieved();
        // A day past the ninety-day window, computed from the table's own date so this test
        // cannot go stale.
        let stale_day =
            CalendarDate::try_from_ymd(retrieved.year() + 1, retrieved.month(), retrieved.day())
                .expect("a real day one year on");
        let report = run_report(
            SizeClass::Mib2048,
            &RunUsage {
                running: Some(DurationP::Measured(std::time::Duration::from_secs(60))),
                ..RunUsage::launched()
            },
            &rates,
            stale_day,
            "run",
        )
        .expect("a report");
        let json = report_to_json(&report);
        assert!(json["staleness"].is_string(), "{json}");
        assert!(
            json["staleness"].as_str().expect("text").contains("days"),
            "{json}"
        );
    }

    /// A run outcome's dense rendering puts the exit code in field one.
    #[test]
    fn the_dense_run_rendering_leads_with_the_exit_code() {
        let outcome = RunOutcome {
            microvm_id: Some("mvm-1".into()),
            image_identifier: Some("arn:image".into()),
            exec_exit_code: Some(7),
            running_seconds: 12.34,
            leaked: vec!["arn:image".into()],
            ..RunOutcome::default()
        };
        let dense = outcome.render(true);
        assert!(dense.starts_with("exit\t7"), "{dense}");
        assert!(dense.contains("leaked\tarn:image"), "{dense}");
        // Field one is readable with `cut -f2`, which is the whole point.
        let first = dense.lines().next().expect("a line");
        assert_eq!(first.split('\t').nth(1), Some("7"));
    }

    /// A leaked identifier is loud in the human rendering.
    #[test]
    fn a_leak_is_named_in_the_human_rendering() {
        let outcome = RunOutcome {
            leaked: vec!["mvm-1".into()],
            ..RunOutcome::default()
        };
        assert!(
            outcome
                .render(false)
                .contains("LEAKED (still billing): mvm-1"),
            "{}",
            outcome.render(false)
        );
    }

    /// `--keep` prints the three identifiers plus the command that uses them and the one
    /// that releases them.
    ///
    /// The caller has just taken responsibility for a billing resource, so the remedy has to
    /// be copy-pasteable rather than described.
    #[test]
    fn keep_prints_the_exec_and_terminate_commands() {
        let outcome = RunOutcome {
            microvm_id: Some("mvm-1".into()),
            image_identifier: Some("arn:image".into()),
            endpoint: Some("https://mvm-1.example".into()),
            agent_token: Some("deadbeef".into()),
            kept: true,
            ..RunOutcome::default()
        };
        let rendered = outcome.render(false);
        assert!(rendered.contains("kept: microvm mvm-1"), "{rendered}");
        assert!(rendered.contains("microvm exec"), "{rendered}");
        assert!(rendered.contains("--agent-token deadbeef"), "{rendered}");
        assert!(rendered.contains("microvm terminate mvm-1"), "{rendered}");
    }

    /// An outcome with nothing to say says `done` rather than printing an empty line.
    #[test]
    fn a_launch_with_no_exec_renders_done() {
        assert_eq!(RunOutcome::default().render(false), "done");
    }

    /// An advisory renders WARN and does not decide the exit code.
    ///
    /// Both halves: a CLI that failed `doctor` over an unlisted region would block a caller
    /// who is right and we are stale, and one that rendered the advisory as FAIL would teach
    /// people to ignore the command.
    #[test]
    fn an_advisory_check_renders_warn_and_leaves_the_run_healthy() {
        let checks = vec![
            Check::pass("credentials", "account 123456789012"),
            Check::fail("region", "not in the list", "known: us-east-1, ...").advisory(),
        ];
        assert!(healthy(&checks), "an advisory must not fail the run");
        let rendered = render_doctor(&checks, false);
        assert!(rendered.contains("WARN  region"), "{rendered}");
        assert!(!rendered.contains("FAIL"), "{rendered}");
        assert!(rendered.contains("-> known: us-east-1"), "{rendered}");

        // And a fatal one does fail it, so the branch is not vacuously true.
        let fatal = vec![Check::fail("daemon-binary", "not aarch64", "rebuild")];
        assert!(!healthy(&fatal));
        assert!(render_doctor(&fatal, false).contains("FAIL  daemon-binary"));
    }
}
