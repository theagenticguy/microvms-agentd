// SPDX-License-Identifier: Apache-2.0
//! The one JSON object per invocation, and the stream discipline that keeps it alone there.
//!
//! # Exactly one envelope on stdout (CLI-4)
//!
//! Progress goes to stderr, always. That is not a style preference: the guard for this
//! requirement induces a failure *with progress enabled* and parses stdout as a single JSON
//! document, so one `println!` of a status line turns it red. Holding both streams in one
//! [`Output`] object means every write in this crate goes through a method that already
//! knows which stream it is for — there is no `print!` anywhere else in the crate, and
//! `tests/thinness.rs` asserts that.
//!
//! # `--quiet` cannot buy silence about a leak
//!
//! [`Output::progress`] is suppressed by `--quiet` and [`Output::warn`] is not. Exactly two
//! things reach `warn`: a stale rate table and a resource that leaked. A leak nobody is told
//! about is the failure `--quiet` must not be able to purchase, and a stale rate is a figure
//! the reader would otherwise copy into a budget.
//!
//! # The failure envelope's fields, and why each is unconditional
//!
//! `finding` is always present and empty when no measured finding applies. A key that
//! appears conditionally is a key every consumer has to guard, and the consumer that forgets
//! reads `undefined` as "no finding" for a failure that had one. Same for `suggestions` and
//! `data`: empty array, empty object, never absent.
//!
//! `data.kind` is the one addition over `cli.py`'s envelope, and it exists because the exit
//! code is deliberately coarser than the daemon's status discipline — `ERR_PROTOCOL` covers
//! five [`microvms_core::WireKind`]s. `conformance/run_rs.py` asserts at the oracle's
//! granularity (`Conflict` versus `NotFound`), so the fine kind travels in `data` where a
//! shell need not look at it and a driver can.
//!
//! # `exec --stream` is the one named exception, and it is a different discriminant
//!
//! One invocation in this binary writes more than one object to stdout: a streamed exec emits
//! **NDJSON** — one JSON object per event, then the envelope last. That is not a relaxation of
//! CLI-4 but a second, narrower contract, and three things keep the two distinguishable.
//!
//! First, the *discriminant differs*: the final envelope's `type` is `microvm.exec.stream`, never
//! `microvm.exec`. A consumer branching on `type` — which is the first field it reads — learns
//! which parse applies from information it already has.
//!
//! Second, the manifest publishes it. `microvm manifest` names the streaming response type beside
//! the ordinary one and states the exception in its `conventions` list, so a consumer discovers
//! the shape rather than encountering it.
//!
//! Third, the envelope is written **compact** once a stream has started ([`Output::stream_line`]
//! sets that), because "the last line is the envelope" is only true if the envelope is one line.
//! A pretty-printed document at the end of an NDJSON stream would be seven broken records.
//!
//! Why the events cannot go on stderr instead, which would preserve the simpler rule: they are
//! the command's **output**, not progress about it. Sending a workload's stdout to the caller's
//! stderr would make `microvm exec --stream build.sh > log` write an empty log, and buffering the
//! events to keep stdout a single document would remove the only reason to stream at all.

use std::io::{IsTerminal, Write};

use serde_json::{Map, Value, json};

use crate::exit::CliError;

/// The envelope's own version.
///
/// Bumped when a field's *meaning* changes, not when a command is added — an agent that
/// pinned to `"1"` must keep parsing. Adding a command changes the manifest, not this.
pub const API_VERSION: &str = "1";

/// How output is rendered. Four renderers over one result type.
///
/// [`Format::Tui`] is not selectable by a flag: it is what
/// [`Output::for_flags`] resolves to when stdout is a terminal and no other format was
/// asked for. A flag would let a piped invocation request it, and a ratatui frame written
/// into a pipe is escape codes where a caller expected text.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Format {
    /// The typed envelope. `--json`.
    Json,
    /// TSV-ish, token-lean. `--dense`.
    Dense,
    /// Deterministic human text. What a pipe gets.
    Plain,
    /// A ratatui surface. Only when stdout is a terminal (CLI-1).
    Tui,
}

impl Format {
    /// Whether this format writes the JSON envelope to stdout.
    pub fn is_json(self) -> bool {
        self == Format::Json
    }
}

/// Where each stream goes, and the format stdout is rendered in.
///
/// Generic over the two writers so a test drives the real code with buffers rather than a
/// second rendering path. The alternative — a `#[cfg(test)]` branch that captures output —
/// tests a code path the shipped binary does not take.
pub struct Output<O: Write, E: Write> {
    format: Format,
    quiet: bool,
    stdout: O,
    stderr: E,
    /// Set by [`Output::emit`], read by the drop check below.
    emitted: bool,
    /// Set by [`Output::stream_line`]: an NDJSON stream is in progress on stdout.
    ///
    /// Two effects, and both are what keeps the streaming exception readable. It relaxes the
    /// `debug_assert` in [`Output::emit`] — a stream's envelope is legitimately not the first
    /// thing written — and it forces that envelope **compact**, because "the last line is the
    /// envelope" is only a true sentence when the envelope occupies one line.
    streaming: bool,
}

impl<O: Write, E: Write> Output<O, E> {
    /// An output over explicit streams and an already-resolved format.
    pub fn new(format: Format, quiet: bool, stdout: O, stderr: E) -> Self {
        Self {
            format,
            quiet,
            stdout,
            stderr,
            emitted: false,
            streaming: false,
        }
    }

    pub fn format(&self) -> Format {
        self.format
    }

    /// Whether an interactive surface should be drawn.
    ///
    /// A single question with a single answer, so a command cannot check `--json` and forget
    /// the TTY or the other way round.
    pub fn tui(&self) -> bool {
        self.format == Format::Tui
    }

    pub fn dense(&self) -> bool {
        self.format == Format::Dense
    }

    /// A human-facing progress line. Never stdout, whatever the format.
    pub fn progress(&mut self, message: &str) {
        if !self.quiet {
            let _ = writeln!(self.stderr, "{message}");
            let _ = self.stderr.flush();
        }
    }

    /// A warning the operator sees even under `--quiet`. See the module docs.
    pub fn warn(&mut self, message: &str) {
        let _ = writeln!(self.stderr, "warning: {message}");
        let _ = self.stderr.flush();
    }

    /// The single write to stdout per invocation.
    ///
    /// Takes both renderings and picks one, rather than letting a caller decide which to
    /// write: a caller that decides is a caller that can decide to write both.
    pub fn emit(&mut self, envelope: &Value, text: &str) {
        debug_assert!(
            !self.emitted,
            "a second envelope reached stdout in one invocation, which breaks CLI-4"
        );
        self.emitted = true;
        // A stream's envelope is the last line of an NDJSON document, so it has to be one line.
        // Pretty-printing it would turn the terminating record into seven broken ones, and the
        // property `conformance/run_rs.py` asserts — "every line before the last parses as an
        // event, the last parses as the envelope" — would be false for a correct stream.
        if self.streaming && self.format.is_json() {
            let _ = writeln!(self.stdout, "{envelope}");
            let _ = self.stdout.flush();
            return;
        }
        match self.format {
            Format::Json => {
                // Compact under `--dense`'s sibling flag combination is handled by
                // `--dense --json`: dense JSON is `to_string`, otherwise pretty. An agent
                // paying per token asked for the first.
                let rendered =
                    serde_json::to_string_pretty(envelope).unwrap_or_else(|_| envelope.to_string());
                let _ = writeln!(self.stdout, "{rendered}");
            }
            _ => {
                // A dense *failure* still carries the code in `text` — `main.rs::report`
                // renders it with `render_error_dense`, so field one is the code.
                let _ = writeln!(self.stdout, "{text}");
            }
        }
        let _ = self.stdout.flush();
    }

    /// Emits the compact JSON form. Used when `--dense --json` are both given.
    pub fn emit_compact(&mut self, envelope: &Value, text: &str) {
        if self.format.is_json() {
            debug_assert!(!self.emitted, "a second envelope reached stdout");
            self.emitted = true;
            let _ = writeln!(self.stdout, "{envelope}");
            let _ = self.stdout.flush();
            return;
        }
        self.emit(envelope, text);
    }

    /// One NDJSON record of a streamed exec's output. See the module docs.
    ///
    /// Writes the compact document plus a newline and flushes, so a consumer reading line by line
    /// sees each event as it happens rather than when a buffer fills — which is the whole point of
    /// streaming, and is the difference between this and building a string.
    ///
    /// Deliberately **not** routed through [`Output::emit`]: emit's contract is one write per
    /// invocation, and a method that could be called repeatedly through it would make that
    /// contract unenforceable for every other command. Separate method, separate flag, and
    /// `emitted` stays false — the envelope written afterwards is still emit's single write.
    ///
    /// A non-JSON format writes nothing here: the plain and dense paths render a streamed exec
    /// exactly as a waited one, because a human reading a terminal wants the output and not a
    /// transcript of the event framing. Only `--json` (and a pipe asking for it) gets NDJSON.
    pub fn stream_line(&mut self, event: &Value) {
        if !self.format.is_json() {
            return;
        }
        self.streaming = true;
        let _ = writeln!(self.stdout, "{event}");
        let _ = self.stdout.flush();
    }

    /// Raw bytes of a streamed exec's output, for the human formats.
    ///
    /// Written to stdout because that is what the bytes *are* — the workload's own stdout — so
    /// `microvm exec --stream ./build > log` has to fill `log`. Bytes rather than a string:
    /// a child's output is not guaranteed UTF-8, and a lossy conversion in the one path whose
    /// job is faithful delivery would corrupt exactly the case that needs it (a tarball on
    /// stdout, a binary diff).
    ///
    /// No flag is set: this stream carries no JSON, so nothing about the envelope changes.
    pub fn stream_bytes(&mut self, bytes: &[u8]) {
        if self.format.is_json() {
            return;
        }
        let _ = self.stdout.write_all(bytes);
        let _ = self.stdout.flush();
    }

    /// Whether an NDJSON stream has been started on stdout.
    pub fn streaming(&self) -> bool {
        self.streaming
    }

    /// Whether anything has been written to stdout yet.
    ///
    /// Read by [`crate::dispatch`] on the failure path: a command that already emitted a
    /// success envelope and then failed must not print a second object — that is the
    /// `AlreadyReported` case, and this is how it is enforced rather than remembered.
    pub fn already_emitted(&self) -> bool {
        self.emitted
    }

    /// Consumes the output and hands back both writers.
    ///
    /// The only way out, because the fields are private — which they are on purpose: a caller
    /// that could reach `stdout` while the object was still live could write to it, and then the
    /// one-envelope rule would be a rule this crate's own code could break. `cfg(test)` because
    /// the shipped binary writes to the real streams and never reads them back.
    #[cfg(test)]
    pub fn into_streams(self) -> (O, E) {
        (self.stdout, self.stderr)
    }
}

impl Output<std::io::Stdout, std::io::Stderr> {
    /// Resolves the format from the flags **and the terminal**, then binds the real streams.
    ///
    /// This is the one behaviour with no Python counterpart: `cli.py` has no `isatty`
    /// anywhere and is purely flag-driven. The rule here is a total function of two inputs:
    ///
    /// * `--json` wins over everything. An agent that asked for JSON gets JSON.
    /// * `--dense` next, since a consumer paying per token asked for the lean text.
    /// * Otherwise: [`Format::Tui`] when stdout is a terminal, [`Format::Plain`] when it is
    ///   not.
    ///
    /// `std::io::IsTerminal` rather than a crate: it is std since 1.70, it is implemented
    /// for `Stdout`, and it answers `false` on a detection failure — which is the correct
    /// direction to fail, since plain text into a terminal is readable and escape codes
    /// into a pipe are not.
    pub fn for_flags(json: bool, dense: bool, quiet: bool) -> Self {
        let format = resolve_format(json, dense, std::io::stdout().is_terminal());
        Self::new(format, quiet, std::io::stdout(), std::io::stderr())
    }
}

/// The format-resolution rule, as a pure function of its three inputs.
///
/// Separated from [`Output::for_flags`] precisely so it is testable without a terminal: the
/// interesting cases are "piped" and "interactive", and a test process's stdout is always
/// the former.
pub fn resolve_format(json: bool, dense: bool, is_terminal: bool) -> Format {
    match (json, dense, is_terminal) {
        (true, _, _) => Format::Json,
        (false, true, _) => Format::Dense,
        (false, false, true) => Format::Tui,
        (false, false, false) => Format::Plain,
    }
}

/// A success envelope.
///
/// `type` is the discriminant an agent branches on first, which is why it is a field rather
/// than something inferred from `data`'s shape.
pub fn ok(kind: &str, data: Map<String, Value>) -> Value {
    json!({
        "status": "ok",
        "apiVersion": API_VERSION,
        "type": kind,
        "data": Value::Object(data),
    })
}

/// A failure envelope. Every field unconditional; see the module docs.
pub fn error(failure: &CliError) -> Value {
    let mut data = failure.data.clone();
    // The fine-grained daemon status, for the consumer the exit code is too coarse for.
    // Inserted rather than replacing whatever `data` already holds, so a teardown's leaked
    // identifiers and the kind coexist on one failure.
    if let Some(wire) = failure.wire_kind {
        data.insert("kind".to_string(), json!(wire.as_str()));
    }
    json!({
        "status": "error",
        "apiVersion": API_VERSION,
        "error": failure.message,
        "code": failure.code(),
        "exitCode": failure.exit.as_u8(),
        "finding": failure.finding(),
        "suggestions": failure.suggestions,
        "data": Value::Object(data),
    })
}

/// The human rendering of a failure.
///
/// Leads with the code because that is what the reader will paste into a search, and puts
/// the finding on its own line because `docs/PLATFORM.md` is where the answer is.
pub fn render_error(failure: &CliError) -> String {
    let mut lines = vec![format!("error {}: {}", failure.code(), failure.message)];
    if !failure.finding().is_empty() {
        lines.push(format!("  see docs/PLATFORM.md, '{}'", failure.finding()));
    }
    lines.extend(
        failure
            .suggestions
            .iter()
            .map(|hint| format!("  hint: {hint}")),
    );
    // Sorted, so two runs of the same failure produce byte-identical text — a diffable
    // failure is worth more than one whose field order follows a hash map.
    let mut keys: Vec<&String> = failure.data.keys().collect();
    keys.sort();
    for key in keys {
        let value = &failure.data[key];
        let rendered = match value {
            Value::String(text) => text.clone(),
            Value::Array(items) => items
                .iter()
                .map(|item| match item {
                    Value::String(text) => text.clone(),
                    other => other.to_string(),
                })
                .collect::<Vec<_>>()
                .join(", "),
            other => other.to_string(),
        };
        lines.push(format!("  {key}: {rendered}"));
    }
    lines.join("\n")
}

/// The dense rendering of a failure: the code, then the message, tab-separated.
pub fn render_error_dense(failure: &CliError) -> String {
    format!("{}\t{}", failure.code(), failure.message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exit::{Exit, classify};
    use microvms_core::{Error, ErrorKind, WireKind};

    fn buffers() -> Output<Vec<u8>, Vec<u8>> {
        Output::new(Format::Json, false, Vec::new(), Vec::new())
    }

    /// **CLI-4's shape.** A failure envelope carries every field, and `finding` is present
    /// and empty rather than absent when no finding applies.
    ///
    /// The empty-string case is the one worth a test: a conditional key is a key every
    /// consumer has to guard, and the one that forgets reads a missing `finding` the same
    /// way it reads an empty one — right by luck until the first failure that has a finding.
    #[test]
    fn a_failure_envelope_carries_every_field_with_finding_present_but_empty() {
        let envelope = error(&classify(&Error::new(ErrorKind::Platform, "boom")));
        assert_eq!(envelope["status"], "error");
        assert_eq!(envelope["apiVersion"], "1");
        assert_eq!(envelope["error"], "boom");
        assert_eq!(envelope["code"], "ERR_PLATFORM");
        assert_eq!(envelope["exitCode"], 9);
        assert_eq!(envelope["finding"], "", "present and empty, never absent");
        assert!(envelope["suggestions"].is_array());
        assert!(envelope["data"].is_object());
        assert!(
            envelope.get("finding").is_some(),
            "the key itself must exist"
        );
    }

    /// The finding travels for the rows that have one.
    #[test]
    fn a_platform_trap_failure_names_its_finding_in_the_envelope() {
        let envelope = error(&classify(&Error::new(
            ErrorKind::WindowClosed,
            "past the 300s window",
        )));
        assert_eq!(envelope["code"], "ERR_WINDOW_CLOSED");
        assert_eq!(envelope["exitCode"], 8);
        assert_eq!(envelope["finding"], "`idlePolicy`");
    }

    /// `data.kind` carries the daemon's own status name, which is what the conformance
    /// oracle asserts on.
    ///
    /// Two `ERR_PROTOCOL` failures that differ only in `data.kind` is the case this exists
    /// for: without it the oracle cannot tell a 400 from a 404, and that distinction is the
    /// one the daemon's whole status discipline exists to preserve.
    #[test]
    fn two_protocol_failures_are_distinguishable_only_through_data_kind() {
        let conflict = error(&classify(&Error::wire(WireKind::Conflict, "409")));
        let missing = error(&classify(&Error::wire(WireKind::NotFound, "404")));
        assert_eq!(conflict["code"], missing["code"]);
        assert_eq!(conflict["exitCode"], missing["exitCode"]);
        assert_eq!(conflict["data"]["kind"], "Conflict");
        assert_eq!(missing["data"]["kind"], "NotFound");
    }

    /// A local reject has no `data.kind`, because nothing reached the daemon.
    #[test]
    fn a_local_reject_reports_no_wire_kind() {
        let envelope = error(&classify(&Error::invalid_arg("off-table size class")));
        assert_eq!(envelope["data"].as_object().expect("an object").len(), 0);
    }

    /// A failure's partial results survive into `data` beside the kind (CLI-6's envelope
    /// half).
    #[test]
    fn leaked_identifiers_and_the_wire_kind_coexist_in_data() {
        let failure = classify(&Error::wire(WireKind::Conflict, "409"))
            .with_data("leaked", json!(["mvm-1", "arn:image"]));
        let envelope = error(&failure);
        assert_eq!(envelope["data"]["kind"], "Conflict");
        assert_eq!(envelope["data"]["leaked"], json!(["mvm-1", "arn:image"]));
    }

    /// Progress and warnings go to stderr; only the envelope reaches stdout.
    #[test]
    fn progress_and_warnings_never_reach_stdout() {
        let mut out = buffers();
        out.progress("building image");
        out.warn("the rate table is 91 days old");
        out.emit(&ok("microvm.build", Map::new()), "text");

        let stdout = String::from_utf8(std::mem::take(&mut out.stdout)).expect("utf8");
        let stderr = String::from_utf8(std::mem::take(&mut out.stderr)).expect("utf8");
        // The whole of stdout parses as one document, which is the assertion CLI-4 is.
        let parsed: Value = serde_json::from_str(&stdout).expect("one JSON document");
        assert_eq!(parsed["status"], "ok");
        assert!(stderr.contains("building image"), "{stderr}");
        assert!(stderr.contains("warning: the rate table"), "{stderr}");
        assert!(!stdout.contains("building"), "{stdout}");
    }

    /// `--quiet` silences progress and never a warning.
    ///
    /// Both halves, so the branch cannot be vacuously true: a `warn` that also checked
    /// `quiet` would pass a test that only asserted the progress line was gone.
    #[test]
    fn quiet_silences_progress_but_never_a_leak_warning() {
        let mut out = Output::new(Format::Json, true, Vec::new(), Vec::new());
        out.progress("tearing down");
        out.warn("could not delete mvm-1 — it is still billing");
        let stderr = String::from_utf8(std::mem::take(&mut out.stderr)).expect("utf8");
        assert!(!stderr.contains("tearing down"), "{stderr}");
        assert!(stderr.contains("still billing"), "{stderr}");
    }

    /// **The TTY rule (new versus the Python), as a total function.**
    ///
    /// Eight cases, which is all of them. The two that matter are the last pair: identical
    /// flags, different terminal, different format — and a piped invocation never reaches
    /// the TUI, because escape codes into a pipe are what a caller cannot parse.
    #[test]
    fn the_format_rule_covers_every_flag_and_terminal_combination() {
        for is_terminal in [true, false] {
            assert_eq!(
                resolve_format(true, false, is_terminal),
                Format::Json,
                "--json wins whatever the terminal is"
            );
            assert_eq!(
                resolve_format(true, true, is_terminal),
                Format::Json,
                "--json wins over --dense"
            );
            assert_eq!(
                resolve_format(false, true, is_terminal),
                Format::Dense,
                "--dense wins over the terminal"
            );
        }
        assert_eq!(resolve_format(false, false, true), Format::Tui);
        assert_eq!(resolve_format(false, false, false), Format::Plain);
    }

    /// A human failure rendering names the code, the finding, and every payload key.
    ///
    /// Deterministic key order, so two runs of one failure diff to nothing.
    #[test]
    fn the_human_failure_rendering_is_deterministic_and_names_the_finding() {
        let failure = classify(&Error::new(ErrorKind::Interrupted, "interrupted"))
            .with_data("leaked", json!(["mvm-1", "arn:image"]))
            .with_data("microvmId", json!("mvm-1"))
            .suggest("record the identifiers above");
        let rendered = render_error(&failure);
        assert!(
            rendered.starts_with("error ERR_INTERRUPTED: interrupted"),
            "{rendered}"
        );
        assert!(
            rendered.contains("The build log group survives Terraform"),
            "{rendered}"
        );
        assert!(
            rendered.contains("hint: record the identifiers"),
            "{rendered}"
        );
        assert!(rendered.contains("leaked: mvm-1, arn:image"), "{rendered}");
        assert_eq!(
            rendered,
            render_error(&failure),
            "byte-identical on a re-render"
        );
        // Sorted keys: `leaked` before `microvmId`.
        let leaked_at = rendered.find("leaked:").expect("present");
        let id_at = rendered.find("microvmId:").expect("present");
        assert!(leaked_at < id_at, "{rendered}");
    }

    /// A success envelope's discriminant is `type`, and `data` is whatever the command put
    /// there.
    #[test]
    fn a_success_envelope_leads_with_its_type_discriminant() {
        let mut data = Map::new();
        data.insert("microvmId".to_string(), json!("mvm-1"));
        let envelope = ok("microvm.state", data);
        assert_eq!(envelope["status"], "ok");
        assert_eq!(envelope["type"], "microvm.state");
        assert_eq!(envelope["data"]["microvmId"], "mvm-1");
        assert_eq!(envelope["apiVersion"], API_VERSION);
    }

    /// `--dense --json` emits the compact document rather than the pretty one.
    #[test]
    fn dense_json_is_compact_and_still_one_document() {
        let mut out = Output::new(Format::Json, false, Vec::new(), Vec::new());
        out.emit_compact(&ok("microvm.runs", Map::new()), "text");
        let stdout = String::from_utf8(std::mem::take(&mut out.stdout)).expect("utf8");
        assert!(
            !stdout.contains("\n  "),
            "compact has no indentation: {stdout}"
        );
        serde_json::from_str::<Value>(&stdout).expect("one JSON document");
    }

    /// A dense failure carries the code in field one.
    #[test]
    fn a_dense_failure_puts_the_code_first() {
        let failure = classify(&Error::new(ErrorKind::Timeout, "deadline elapsed"));
        assert_eq!(
            render_error_dense(&failure),
            "ERR_TIMEOUT\tdeadline elapsed"
        );
        assert_eq!(failure.exit, Exit::Timeout);
    }
}
