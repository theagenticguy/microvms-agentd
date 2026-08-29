// SPDX-License-Identifier: Apache-2.0
//! The whole surface, generated from the clap tree and the exit table.
//!
//! # Never hand-maintained, and that is structural rather than a promise
//!
//! Every command, every parameter, and every parameter's domain is read out of
//! [`clap::Command`] by introspection; the exit rows come from [`crate::exit::EXIT_TABLE`],
//! which the runtime reads too. So a flag added to a handler appears here without anyone
//! remembering to add it, and a flag removed disappears. The manifest's entire value to an
//! agent is not that it is accurate but that it **cannot be wrong**, and generation is the only
//! version of that.
//!
//! The one thing that is a table — [`crate::commands::RESPONSE_TYPES`] — is cross-checked
//! against the clap tree by `tests/manifest.rs`, so a command added without a row fails rather
//! than shipping undescribed. That check is what keeps the table from being the artifact the
//! generation rule forbids.
//!
//! # `choices` is the CLI-5 witness
//!
//! Closed set or null, per parameter. An option whose library counterpart is S1 — the size
//! class, the region — must report a closed set, because the CLI is the layer where an S1 guard
//! is most easily downgraded to a convenience string flag. A reviewer, or a test, can read this
//! field and see whether that happened.

use clap::CommandFactory;
use serde_json::{Value, json};

use crate::cli::Cli;
use crate::commands::response_type;
use crate::envelope::API_VERSION;
use crate::exit::EXIT_TABLE;

/// The manifest.
pub fn build() -> Value {
    let command = Cli::command();
    let commands: Vec<Value> = command
        .get_subcommands()
        .map(|sub| {
            let name = sub.get_name();
            let (kind, keys) = response_type(name);
            // `exec --stream` answers with a different discriminant and a different stdout shape,
            // so the manifest publishes both rather than describing one and leaving the other to
            // be discovered. Generated from the same command tree — the flag's presence is what
            // decides, so a `--stream` removed from `exec` takes this with it.
            let streaming = sub
                .get_arguments()
                .any(|arg| arg.get_long() == Some("stream"));
            let alternate = streaming.then(|| {
                let (stream_kind, stream_keys) = crate::commands::STREAM_RESPONSE;
                json!({
                    "when": "--stream",
                    "responseType": stream_kind,
                    "responseKeys": stream_keys,
                    // The one place the exception is stated as a machine-readable fact rather
                    // than as prose in a conventions string. A consumer that reads this knows to
                    // parse stdout line by line before it ever sees the first line.
                    "stdout": "ndjson — one event object per line, then this envelope as the \
                               final line. The documented exception to the one-envelope rule: \
                               stream chunks are the command's output, not progress, so they \
                               cannot go on stderr.",
                })
            });
            json!({
                "name": name,
                // The first line of the doc comment, which clap keeps as `about`. The rest is
                // in `long_about` and is prose for a human rather than a summary for an agent.
                "summary": sub.get_about().map(|about| about.to_string()).unwrap_or_default(),
                "parameters": sub.get_arguments().map(parameter).collect::<Vec<_>>(),
                "supportsJson": true,
                "responseType": kind,
                "responseKeys": keys,
                // Null for every command but `exec`, and present-and-null rather than absent for
                // the same reason the failure envelope's `finding` is: a key that appears
                // conditionally is a key every consumer has to guard.
                "alternateResponse": alternate,
            })
        })
        .collect();

    json!({
        "apiVersion": API_VERSION,
        "cli": "microvm",
        "version": env!("CARGO_PKG_VERSION"),
        "commands": commands,
        "exitCodes": EXIT_TABLE.iter().map(|row| json!({
            "exit": row.exit.as_u8(),
            "code": row.code,
            "meaning": row.meaning,
            "finding": row.finding,
        })).collect::<Vec<_>>(),
        "envelope": {
            "discriminator": "status",
            "ok": {
                "status": "ok",
                "apiVersion": "string",
                "type": "string — one of responseType above",
                "data": "object — keys per responseKeys",
            },
            "error": {
                "status": "error",
                "apiVersion": "string",
                "error": "string — human readable, may be reworded between releases",
                "code": "string — stable, branch on this",
                "exitCode": "integer — matches the process exit code",
                "finding": "string — the docs/PLATFORM.md section, or empty",
                "suggestions": "array of string",
                "data": "object — partial results, e.g. leaked identifiers, and `kind` \
                         naming the daemon status when one produced the failure",
            },
        },
        "conventions": [
            "exactly one envelope object on stdout per invocation; progress is on stderr",
            "branch on `code`, never on `error`",
            "dollar figures are estimates derived from published rates, never an invoice",
            "an unpriced line item omits `usd` rather than reporting zero",
            // The fifth, which the Python has no counterpart for because it has no `data.kind`:
            // an agent that needs finer granularity than the exit code reads it there.
            "`data.kind` carries the daemon's own status name when the exit code is coarser \
             than the failure (ERR_PROTOCOL covers five)",
            // The sixth, and the one that qualifies the first. Stated here as well as in `exec`'s
            // `alternateResponse` because these two lists are read by different consumers: an
            // agent choosing a command reads the command entry, and one writing a parser reads
            // the conventions.
            "`exec --stream` is the one exception to the line above: it writes NDJSON — one \
             event object per line — and this envelope as the final line, with the \
             discriminant `microvm.exec.stream` rather than `microvm.exec`. Every other \
             invocation writes exactly one object",
        ],
    })
}

/// One parameter, with its domain.
fn parameter(arg: &clap::Arg) -> Value {
    let choices: Option<Vec<String>> = {
        // A flag is deliberately *not* a two-valued domain, though clap reports one:
        // `SetTrue` carries possible values `["true", "false"]`, and publishing those would put a
        // `choices` array on every boolean in the manifest. That is not merely noise — `choices`
        // is the CLI-5 witness, and a reader scanning for the parameters with a closed set would
        // find nineteen flags among them. `type: "boolean"` already says everything a caller
        // needs.
        let values = arg.get_possible_values();
        if values.is_empty() || is_flag(arg) {
            None
        } else {
            Some(
                values
                    .iter()
                    .map(|value| value.get_name().to_string())
                    .collect(),
            )
        }
    };
    json!({
        // The long flag where there is one, else the positional's id — which is what a caller
        // writes on the command line either way.
        "name": arg.get_long().map(str::to_string).unwrap_or_else(|| arg.get_id().to_string()),
        "type": type_name(arg),
        // Closed set or null. See the module docs: this is the AC-5-5 / CLI-5 field.
        "choices": choices,
        "required": arg.is_required_set(),
        "positional": arg.is_positional(),
        "help": arg.get_help().map(|help| help.to_string()).unwrap_or_default(),
        "default": arg.get_default_values()
            .first()
            .map(|value| value.to_string_lossy().to_string()),
    })
}

/// The parameter's type, as a name an agent can act on.
///
/// Derived from clap's own value hints and arity rather than from the Rust type, which is not
/// available at runtime. Four names, because four is what a caller needs to distinguish: a flag
/// takes nothing, a path is a path, an enumerated value comes from `choices`, and everything
/// else is a string the handler parses.
fn type_name(arg: &clap::Arg) -> &'static str {
    if is_flag(arg) {
        return "boolean";
    }
    if !arg.get_possible_values().is_empty() {
        return "enum";
    }
    match arg.get_value_hint() {
        clap::ValueHint::FilePath | clap::ValueHint::DirPath | clap::ValueHint::AnyPath => "path",
        _ => "string",
    }
}

/// Whether this argument is a flag rather than something that takes a value.
///
/// Read off the *action* rather than off the absence of possible values, because clap gives a
/// `SetTrue` flag the values `["true", "false"]` — so "has no domain" is not the same question.
fn is_flag(arg: &clap::Arg) -> bool {
    matches!(
        arg.get_action(),
        clap::ArgAction::SetTrue | clap::ArgAction::SetFalse
    )
}

/// The human view: the two tables an operator actually reads.
pub fn render(manifest: &Value, dense: bool) -> String {
    let commands = manifest["commands"].as_array().cloned().unwrap_or_default();
    if dense {
        return commands
            .iter()
            .map(|command| {
                format!(
                    "{}\t{}\t{}",
                    command["name"].as_str().unwrap_or_default(),
                    command["responseType"].as_str().unwrap_or_default(),
                    command["parameters"]
                        .as_array()
                        .map(|params| params
                            .iter()
                            .filter_map(|param| param["name"].as_str())
                            .collect::<Vec<_>>()
                            .join(","))
                        .unwrap_or_default(),
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
    }
    let mut lines = vec![
        format!(
            "microvm {} — {} commands",
            manifest["version"].as_str().unwrap_or_default(),
            commands.len()
        ),
        String::new(),
    ];
    for command in &commands {
        lines.push(format!(
            "  {:<10} {}",
            command["name"].as_str().unwrap_or_default(),
            command["summary"].as_str().unwrap_or_default(),
        ));
    }
    lines.push(String::new());
    lines.push("exit codes:".to_string());
    for row in manifest["exitCodes"]
        .as_array()
        .cloned()
        .unwrap_or_default()
    {
        lines.push(format!(
            "  {:<3} {:<20} {}",
            row["exit"].as_u64().unwrap_or_default(),
            row["code"].as_str().unwrap_or("-"),
            row["meaning"].as_str().unwrap_or_default(),
        ));
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The manifest's commands are exactly the clap tree's, in the same order.
    ///
    /// The whole generation claim, and the reason it is an equality rather than a subset check:
    /// a manifest listing a command the parser does not accept is as bad as one omitting a
    /// command it does.
    #[test]
    fn the_manifest_lists_exactly_the_registered_commands() {
        let manifest = build();
        let listed: Vec<String> = manifest["commands"]
            .as_array()
            .expect("an array")
            .iter()
            .map(|command| command["name"].as_str().unwrap_or_default().to_string())
            .collect();
        let registered: Vec<String> = Cli::command()
            .get_subcommands()
            .map(|sub| sub.get_name().to_string())
            .collect();
        assert_eq!(listed, registered);
        assert_eq!(
            listed.len(),
            18,
            "the lifecycle six, the attached five, and the local seven"
        );
    }

    /// **`exec` publishes its streaming shape and no other command claims one.**
    ///
    /// The exception has to be discoverable or it is not an exception, it is a surprise. Asserted
    /// in both directions: a `--stream` added to another command without a response row would
    /// publish a second NDJSON shape nobody documented, and an `exec` that lost the entry would
    /// leave a consumer parsing an NDJSON stream as one document.
    #[test]
    fn only_exec_publishes_an_alternate_streaming_response() {
        let manifest = build();
        let mut found = 0;
        for command in manifest["commands"].as_array().expect("an array") {
            let name = command["name"].as_str().unwrap_or_default();
            let alternate = &command["alternateResponse"];
            if name == "exec" {
                assert_eq!(alternate["when"], "--stream");
                assert_eq!(alternate["responseType"], "microvm.exec.stream");
                assert_ne!(
                    alternate["responseType"], command["responseType"],
                    "the streaming shape must be a *different* discriminant, or a consumer \
                     branching on `type` cannot tell which parse applies"
                );
                assert!(
                    alternate["stdout"]
                        .as_str()
                        .expect("a description")
                        .contains("ndjson"),
                    "{alternate}"
                );
                assert!(
                    !alternate["responseKeys"]
                        .as_array()
                        .expect("an array")
                        .is_empty()
                );
                found += 1;
            } else {
                assert_eq!(
                    *alternate,
                    Value::Null,
                    "{name} publishes an alternate response shape nothing documents"
                );
            }
        }
        assert_eq!(
            found, 1,
            "exec must be in the manifest for this to mean anything"
        );
    }

    /// Every exit row reaches the manifest with its code, meaning, and finding.
    #[test]
    fn the_manifest_carries_all_seventeen_exit_rows() {
        let manifest = build();
        let rows = manifest["exitCodes"].as_array().expect("an array");
        assert_eq!(rows.len(), 17);
        for (row, expected) in rows.iter().zip(EXIT_TABLE.iter()) {
            assert_eq!(row["exit"], expected.exit.as_u8());
            assert_eq!(row["code"], json!(expected.code));
            assert_eq!(row["meaning"], expected.meaning);
            assert_eq!(row["finding"], expected.finding);
        }
        // Row 0 has a null code, which is what says "success has no ERR_* string".
        assert_eq!(rows[0]["code"], Value::Null);
    }

    /// **The CLI-5 witness.** `--memory` and `--region` report closed sets everywhere they
    /// appear, and the sets are the documented ones.
    ///
    /// Read off the manifest rather than off the enum, because the manifest is what an agent
    /// consumes — so this is the assertion that the *published* contract is the guarded one.
    #[test]
    fn every_s1_parameter_publishes_its_closed_set() {
        let manifest = build();
        let mut checked = 0;
        for command in manifest["commands"].as_array().expect("an array") {
            for param in command["parameters"].as_array().expect("an array") {
                let name = param["name"].as_str().unwrap_or_default();
                if name == "memory" {
                    assert_eq!(
                        param["choices"],
                        json!(["512", "1024", "2048", "4096", "8192"]),
                        "{}'s --memory: {param}",
                        command["name"]
                    );
                    assert_eq!(param["type"], "enum");
                    checked += 1;
                }
                if name == "region" {
                    assert_eq!(
                        param["choices"],
                        json!([
                            "us-east-1",
                            "us-east-2",
                            "us-west-2",
                            "eu-west-1",
                            "ap-northeast-1"
                        ]),
                        "{}'s --region: {param}",
                        command["name"]
                    );
                    checked += 1;
                }
            }
        }
        assert!(
            checked >= 3,
            "the S1 options must actually appear: {checked}"
        );
    }

    /// A free-text parameter reports `choices: null` rather than an empty array.
    ///
    /// The distinction is what makes the field readable: `[]` says "a closed set with nothing
    /// in it", which is not a thing, and a consumer that treated it as a domain would refuse
    /// every value.
    #[test]
    fn a_free_text_parameter_reports_a_null_domain() {
        let manifest = build();
        let build_command = manifest["commands"]
            .as_array()
            .expect("an array")
            .iter()
            .find(|command| command["name"] == "build")
            .expect("build is registered")
            .clone();
        let name_param = build_command["parameters"]
            .as_array()
            .expect("an array")
            .iter()
            .find(|param| param["name"] == "name")
            .expect("--name exists")
            .clone();
        assert_eq!(name_param["choices"], Value::Null);
        assert_eq!(name_param["type"], "string");
    }

    /// Every command declares a namespaced response type and non-empty keys.
    ///
    /// A command with an empty `responseType` is one the response table forgot, which would
    /// leave an agent unable to branch on the envelope it receives.
    #[test]
    fn every_command_declares_a_response_type_and_its_keys() {
        for command in build()["commands"].as_array().expect("an array") {
            let name = command["name"].as_str().unwrap_or_default();
            let kind = command["responseType"].as_str().unwrap_or_default();
            assert!(
                kind.starts_with("microvm."),
                "{name} has no namespaced response type: {kind:?}"
            );
            assert!(
                !command["responseKeys"]
                    .as_array()
                    .expect("an array")
                    .is_empty(),
                "{name} declares no response keys"
            );
            assert_eq!(command["supportsJson"], true);
        }
    }

    /// Every command's summary comes from its doc comment.
    ///
    /// The coupling worth pinning: the summary is what an agent reads to choose a command, and a
    /// command whose doc comment is deleted would publish an empty one rather than fail.
    #[test]
    fn every_command_publishes_a_summary_from_its_doc_comment() {
        for command in build()["commands"].as_array().expect("an array") {
            let summary = command["summary"].as_str().unwrap_or_default();
            assert!(
                summary.len() > 20,
                "{}'s summary is not a sentence: {summary:?}",
                command["name"]
            );
        }
    }

    /// **A boolean flag publishes no domain**, even though clap reports one.
    ///
    /// `SetTrue` carries possible values `["true", "false"]`. Publishing them would put a
    /// `choices` array on all nineteen flags, and `choices` is the field a reader scans to find
    /// the parameters with a *closed set* — so nineteen false positives would make the CLI-5
    /// witness unreadable.
    #[test]
    fn a_flag_is_typed_boolean_and_publishes_no_domain() {
        let manifest = build();
        let run = manifest["commands"]
            .as_array()
            .expect("an array")
            .iter()
            .find(|command| command["name"] == "run")
            .expect("run is registered")
            .clone();
        let keep = run["parameters"]
            .as_array()
            .expect("an array")
            .iter()
            .find(|param| param["name"] == "keep")
            .expect("--keep exists")
            .clone();
        assert_eq!(keep["type"], "boolean");
        assert_eq!(keep["choices"], Value::Null);
    }

    /// The conventions include the two honesty rules, the `data.kind` note, and the one exception.
    #[test]
    fn the_conventions_name_the_honesty_rules_and_the_streaming_exception() {
        let conventions: Vec<String> = build()["conventions"]
            .as_array()
            .expect("an array")
            .iter()
            .map(|value| value.as_str().unwrap_or_default().to_string())
            .collect();
        assert_eq!(conventions.len(), 6);
        // The exception has to be *stated*, and it has to name the discriminant a consumer uses to
        // detect it. A convention saying only "exec --stream is different" would leave a parser
        // author guessing at how to tell.
        let exception = conventions
            .iter()
            .find(|line| line.contains("--stream"))
            .expect("the one exception to the one-envelope rule must be written down");
        assert!(
            exception.contains("ndjson") || exception.contains("NDJSON"),
            "{exception}"
        );
        assert!(
            exception.contains("microvm.exec.stream"),
            "the convention must name the discriminant, which is how a consumer detects it: \
             {exception}"
        );
        assert!(
            conventions.iter().any(|line| line.contains("omits `usd`")),
            "{conventions:?}"
        );
        assert!(
            conventions
                .iter()
                .any(|line| line.contains("branch on `code`")),
            "{conventions:?}"
        );
        assert!(
            conventions.iter().any(|line| line.contains("data.kind")),
            "{conventions:?}"
        );
    }

    /// The human rendering names every command and every exit code.
    #[test]
    fn the_human_rendering_names_every_command_and_exit_code() {
        let manifest = build();
        let rendered = render(&manifest, false);
        for name in [
            "run",
            "build",
            "exec",
            "health",
            "ack",
            "stdin",
            "cp",
            "manifest",
            "constants",
        ] {
            assert!(rendered.contains(name), "{name} missing from {rendered}");
        }
        for code in ["ERR_INVALID_ARG", "ERR_EXEC_FAILED", "ERR_INTERRUPTED"] {
            assert!(rendered.contains(code), "{code} missing");
        }
        assert!(rendered.contains("18 commands"), "{rendered}");

        // The dense rendering is one line per command with its parameters.
        let dense = render(&manifest, true);
        assert_eq!(dense.lines().count(), 18);
        assert!(
            dense
                .lines()
                .next()
                .expect("a line")
                .contains("microvm.run"),
            "{dense}"
        );
    }

    /// The manifest round-trips as JSON, so it is emittable.
    ///
    /// Trivial-looking and worth having: a `Value` holding an f64 NaN serializes as `null` and
    /// the failure would appear only in a consumer's parser.
    #[test]
    fn the_manifest_serializes_and_reparses_unchanged() {
        let manifest = build();
        let text = serde_json::to_string(&manifest).expect("serializes");
        let back: Value = serde_json::from_str(&text).expect("reparses");
        assert_eq!(back, manifest);
    }
}
