// SPDX-License-Identifier: Apache-2.0
//! One handler per command, and the result type all four renderers read.
//!
//! # A handler never writes to stdout
//!
//! Every handler returns a [`Rendered`] and [`crate::dispatch`] does the single write. That
//! is what makes CLI-4 structural rather than remembered: there is no `emit` call in this
//! module, so "exactly one envelope per invocation" is a property of the dispatcher rather
//! than a rule twelve handlers each have to follow.
//!
//! # `AlreadyReported` is a field, not an exception
//!
//! `cli.py:257` needs an `AlreadyReported` exception for the case where a **success**
//! envelope is correct and the exit code must still be non-zero: `run`'s workload exited 4,
//! `suspend` reached TERMINATED instead of SUSPENDED, `doctor` found something broken. Its
//! docstring says why it cannot be a `CliError` — that would print a second envelope and
//! break the one-envelope rule.
//!
//! Here it is [`Rendered::already_reported`], a field on the value the handler returns. The
//! dispatcher emits the envelope and *then* reads the field, so a second envelope is not
//! merely discouraged: there is no code path that could write one.
//!
//! (cli.py line numbers resolve at `git show 'c4d396e^:clients/python/src/microvms_agentd/cli.py'` — the retired oracle.)

pub mod attached;
pub mod cost;
pub mod doctor;
pub mod lifecycle;
pub mod local;

use std::io::Write;

use serde_json::{Map, Value};

use crate::envelope::Output;
use crate::exit::Exit;
use crate::seam::{CoreSeam, Infra};

/// What a handler produces: one envelope's contents, and both text renderings.
///
/// Both renderings rather than a `dense: bool` the handler reads, because a handler that
/// reads the flag is a handler that can render the wrong one — and the dispatcher already
/// knows which format it is in.
#[derive(Debug)]
pub struct Rendered {
    /// The success envelope's `type` discriminant.
    pub kind: &'static str,
    pub data: Map<String, Value>,
    pub text: String,
    pub dense_text: String,
    /// A non-zero exit whose success envelope is nonetheless the right answer.
    ///
    /// See the module docs. `None` for every command that simply worked.
    pub already_reported: Option<Exit>,
}

impl Rendered {
    /// A success with nothing to add to the exit code.
    pub fn ok(kind: &'static str, data: Map<String, Value>, text: String, dense: String) -> Self {
        Self {
            kind,
            data,
            text,
            dense_text: dense,
            already_reported: None,
        }
    }

    /// The same, but the process exits `exit` after the envelope is written.
    #[must_use]
    pub fn reporting(mut self, exit: Exit) -> Self {
        self.already_reported = Some(exit);
        self
    }

    /// The text for `format`.
    pub fn text_for(&self, dense: bool) -> &str {
        if dense { &self.dense_text } else { &self.text }
    }
}

/// Everything a handler needs that is not its own arguments.
///
/// The seam is `&dyn` rather than a generic, so a handler compiles once and the behavioral
/// guard exercises the same machine code the shipped binary runs.
pub struct Ctx<'a, O: Write, E: Write> {
    pub seam: &'a dyn CoreSeam,
    pub out: &'a mut Output<O, E>,
    pub infra: Infra,
    /// The environment, injected so a test never mutates the process's own.
    ///
    /// `std::env::set_var` is `unsafe` in edition 2024 and is shared mutable state besides,
    /// which under a parallel test runner means one test's region leaking into another's.
    pub env: &'a dyn Fn(&str) -> Option<String>,
}

/// The `type` discriminant and `data` keys each command's success envelope carries.
///
/// Declared beside the commands rather than derived from a return type, because the handlers
/// return a `Map` — and cross-checked against the clap tree by `tests/manifest.rs`, so a
/// command added without an entry fails rather than shipping undescribed. That check is the
/// only thing that keeps this table from being the hand-maintained artifact the manifest is
/// forbidden to be.
pub const RESPONSE_TYPES: [(&str, &str, &[&str]); 20] = [
    (
        "run",
        "microvm.run",
        &[
            "imageIdentifier",
            "imageName",
            "microvmId",
            "endpoint",
            "agentToken",
            "execExitCode",
            "stdout",
            "stderr",
            "truncated",
            "buildSeconds",
            "runningSeconds",
            "kept",
            "vmName",
            "leaked",
            "cost",
            // What each config-mergeable knob resolved to, as {value, source} with source
            // one of flag/config/default — and which file supplied the config values
            // (null when none did). Issue #73: a caller who stopped passing flags reads
            // what the run actually used here rather than re-deriving the precedence.
            "resolvedConfig",
            "configPath",
            // `run <DIR>`'s report: {workdir, uploadedBytes, uploadedMembers, artifacts:
            // [{path, bytes}]} — null for a plain run. Issue #72.
            "sync",
        ],
    ),
    (
        "build",
        "microvm.image",
        &[
            "imageIdentifier",
            "imageName",
            "buildLogGroup",
            "size",
            // Always present, `false` for a plain build: `true` means `--reuse` matched
            // an existing image by content-hash name and nothing was built.
            "reused",
        ],
    ),
    (
        "exec",
        "microvm.exec",
        &[
            "execId",
            "exitCode",
            "stdout",
            "stderr",
            "truncated",
            "phase",
        ],
    ),
    (
        "health",
        "microvm.health",
        &[
            "version",
            "bootstrapped",
            "identityDegraded",
            "identityRepaired",
            "diskAvailableBytes",
            "diskUnderPressure",
            "busy",
            "execs",
        ],
    ),
    (
        "ack",
        "microvm.exec",
        &[
            "execId",
            "phase",
            "exitCode",
            "stdout",
            "stderr",
            "truncated",
        ],
    ),
    ("stdin", "microvm.stdin", &["execId", "written", "eof"]),
    (
        "cp",
        "microvm.copy",
        &["direction", "bytes", "local", "remote", "tar"],
    ),
    (
        "tunnel",
        "microvm.tunnel",
        &[
            "microvmId",
            "localPort",
            "localAddress",
            "guestPort",
            "connectionsServed",
            "connectionsRefused",
            "proxyTokenMints",
            "interrupted",
        ],
    ),
    (
        "port-forward",
        "microvm.port-forward",
        &[
            "microvmId",
            "localPort",
            "localAddress",
            "guestPort",
            "connectionsServed",
            "connectionsRefused",
            "upgrades",
            "proxyTokenMints",
            "interrupted",
        ],
    ),
    ("suspend", "microvm.state", &["microvmId", "state"]),
    (
        "resume",
        "microvm.state",
        &["microvmId", "state", "endpoint"],
    ),
    (
        "terminate",
        "microvm.teardown",
        &[
            "microvmId",
            "imageIdentifier",
            "leaked",
            "undeletedLogGroups",
            "state",
        ],
    ),
    ("ls", "microvm.runs", &["runs"]),
    ("history", "microvm.history", &["microvmId", "events"]),
    ("logs", "microvm.logs", &["logGroup", "lines"]),
    ("cost", "microvm.cost", &["report", "comparison"]),
    ("doctor", "microvm.doctor", &["checks", "ok"]),
    (
        "manifest",
        "microvm.manifest",
        &[
            "apiVersion",
            "cli",
            "version",
            "commands",
            "exitCodes",
            "envelope",
            "conventions",
        ],
    ),
    ("constants", "microvm.constants", &["constants"]),
    (
        "dockerfile",
        "microvm.dockerfile",
        &[
            "stanza",
            "baseImageName",
            "baseImageDockerRef",
            "port",
            "workdir",
        ],
    ),
];

/// The discriminant `exec --stream` emits instead of `microvm.exec`, and the keys it carries.
///
/// # Why a second row rather than a flag on the first
///
/// `exec --stream` is the one invocation in this binary that writes more than one JSON object to
/// stdout: one NDJSON line per event, then the envelope last. An agent handed a `microvm.exec`
/// envelope is entitled to assume the whole of stdout was that envelope — that is what CLI-4
/// promises — so the streaming shape has to announce itself with a **different discriminant**
/// rather than the same one with an extra key. A consumer branching on `type` therefore learns
/// which parse to use from the field it already reads first.
///
/// The keys are a *summary* rather than the output: the output was the NDJSON, and repeating it in
/// the envelope would double a stream's memory cost for a consumer that has already seen every
/// byte. `events` and `bytes` are what let a caller assert it read everything, and `nextOffset` is
/// where a resume would continue.
pub const STREAM_RESPONSE: (&str, &[&str]) = (
    "microvm.exec.stream",
    &[
        "execId",
        "events",
        "bytes",
        "nextOffset",
        "exitCode",
        "truncated",
        "gaps",
    ],
);

/// The MicroVM id `identifier` names, through the local registry when it is a bare name.
///
/// The discrimination is total, and the name grammar is what makes it so: an identifier
/// starting with `microvm-` (the real service's prefix) or `mvm-` (the fixtures') is an id shape (a legal name is refused those prefixes
/// at registration), and anything that fails the name grammar — an ARN's `:`, a path's `/`
/// — cannot be in the registry, so it passes through verbatim for the service to answer.
/// Only a legal name is looked up, and a legal name the registry does not hold fails
/// locally with `ERR_PRECONDITION` — the image-resolution precedent: the service's answer
/// to a bare name is a 400 about malformed identifiers, which says nothing about names.
///
/// Zero AWS calls on every path: passthrough is a prefix check and resolution is a file read.
pub fn resolve_vm_identifier<O: Write, E: Write>(
    ctx: &Ctx<'_, O, E>,
    identifier: &str,
    state_dir_flag: Option<std::path::PathBuf>,
) -> Result<String, crate::exit::CliError> {
    if identifier.starts_with("microvm-")
        || identifier.starts_with("mvm-")
        || crate::ledger::validate_name(identifier).is_err()
    {
        return Ok(identifier.to_string());
    }
    let root = crate::seam::state_dir(state_dir_flag, ctx.env);
    match crate::ledger::Names::new(&root).lookup(identifier) {
        Some(record) => Ok(record.microvm_id),
        None => Err(crate::exit::CliError::new(
            Exit::Precondition,
            format!(
                "no VM named {identifier:?} in {}. Names are local: `run --keep --vm-name \
                 {identifier}` registers one here, and a name registered on another machine \
                 lives in that machine's state directory.",
                root.join("names").display(),
            ),
        )
        .suggest("`microvm ls` shows this state directory's outstanding runs")
        .suggest("a MicroVM id (mvm-…) is accepted directly")),
    }
}

/// The `type` and `data` keys for `name`, or empty when the table has no row.
///
/// Empty rather than a panic: the manifest is the command that reports the surface, and a
/// manifest that panicked on an undescribed command would be unable to report the very thing
/// that is wrong. The cross-check test is what makes the empty case unreachable in practice.
pub fn response_type(name: &str) -> (&'static str, &'static [&'static str]) {
    RESPONSE_TYPES
        .iter()
        .find(|(command, _, _)| *command == name)
        .map(|(_, kind, keys)| (*kind, *keys))
        .unwrap_or(("", &[]))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every row names a distinct command, and every `type` is namespaced.
    ///
    /// The namespace matters because `type` is the first thing an agent branches on: a bare
    /// `run` would collide with any other tool's discriminant in a shared log.
    #[test]
    fn every_response_row_is_distinct_and_namespaced() {
        let mut names: Vec<&str> = RESPONSE_TYPES.iter().map(|(name, _, _)| *name).collect();
        let before = names.len();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), before, "a duplicated command row");

        for (name, kind, keys) in RESPONSE_TYPES {
            assert!(
                kind.starts_with("microvm."),
                "{name}'s type must be namespaced: {kind}"
            );
            assert!(!keys.is_empty(), "{name} declares no response keys");
        }
    }

    /// `already_reported` starts absent and is only set deliberately.
    #[test]
    fn a_rendered_result_reports_nothing_until_asked() {
        let plain = Rendered::ok("microvm.runs", Map::new(), "a".into(), "b".into());
        assert_eq!(plain.already_reported, None);
        assert_eq!(plain.text_for(false), "a");
        assert_eq!(plain.text_for(true), "b");
        assert_eq!(
            plain.reporting(Exit::ExecFailed).already_reported,
            Some(Exit::ExecFailed)
        );
    }

    /// An unknown command answers empty rather than panicking.
    #[test]
    fn an_unknown_command_has_no_response_type() {
        assert_eq!(response_type("nope"), ("", &[] as &[&str]));
        assert_eq!(response_type("run").0, "microvm.run");
    }
}
