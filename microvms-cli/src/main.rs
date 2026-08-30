// SPDX-License-Identifier: Apache-2.0
//! `microvm`: a working sandbox in one command, and nothing `microvms-core` does not do.
//!
//! # Two audiences share one library
//!
//! A consumer building a product depends on `microvms-core`; a consumer who wants a VM to run a
//! test suite in *now* runs `microvm run ./agentd --exec pytest`. This binary is the second door
//! onto the first room — it parses, it renders, and it exits with a code. Every AWS call and
//! every trap guard belongs to the library, and that is a **checked** property rather than an
//! intention: `tests/thinness.rs` asserts that no direct dependency is an HTTP or AWS crate,
//! that no source file here names a transport or a control-plane operation, and that
//! every AWS-touching command fails when the library seam is made to refuse. Any one of those
//! alone is defeatable, which is why there are three.
//!
//! # A coding agent is a first-class consumer
//!
//! So the surface is machine-legible by construction. `microvm manifest` emits the whole command
//! tree with its option domains, exit codes, and envelope schema, generated from the parser
//! rather than written down. Every command honours `--json` and emits exactly one envelope object
//! on stdout with nothing else on that stream — progress goes to stderr, always, because a CLI
//! that writes a log line to stdout passes an "is the envelope there" check and breaks the parse.
//!
//! # There is no lib target (ARCH-5)
//!
//! This crate is a binary and exports nothing. A binding cannot need a type from here because
//! there is nothing here to need, and `tests/dependency_direction.rs` reads `cargo metadata` and
//! fails if a lib target ever appears. That is why the modules below are declared in `main.rs`
//! rather than in a `lib.rs` half the world could import.

mod cli;
mod commands;
mod config;
mod envelope;
mod exit;
// The guards that have to reach inside the crate. See the module docs on why these three are
// here and the process-level ones are in `tests/`. `cfg(test)` only.
#[cfg(test)]
mod guards;
mod history;
mod ledger;
mod manifest;
mod provision;
mod render;
mod seam;
mod sync;
mod tui;

use std::process::ExitCode;

use clap::{CommandFactory, FromArgMatches};

use crate::cli::{Cli, Command};
use crate::commands::Ctx;
use crate::envelope::Output;
use crate::exit::{CliError, Exit};
use crate::seam::{AwsSeam, Infra, process_env};

/// Parses, dispatches, and exits with a code from the catalog.
///
/// `ExitCode` rather than `std::process::exit`, because the latter skips destructors — and one of
/// the destructors here is `Sandbox`'s drop warning about a live VM, which is the last line of
/// defence for a caller who is about to be billed for something they forgot.
///
/// The runtime is built by hand rather than through `#[tokio::main]` so the parse happens
/// *before* a runtime exists: a `microvm --help` or a misspelled flag should not spin up a
/// thread pool to be told it is wrong.
fn main() -> ExitCode {
    // `--json` and `--dense` are read off the raw tokens here, before the parse, because a parse
    // failure never reaches a handler — and an agent that asked for JSON must get JSON even when
    // what it gets is an argument error. This is `cli.py:2318`'s trick and it is necessary for
    // the same reason.
    //
    // (cli.py line numbers resolve at `git show 'c4d396e^:clients/python/src/microvms_agentd/cli.py'` — the retired oracle.)
    let tokens: Vec<String> = std::env::args().skip(1).collect();
    let wants = |flag: &str| tokens.iter().any(|token| token == flag);
    let mut out = Output::for_flags(wants("--json"), wants("--dense"), wants("--quiet"));

    // Through `ArgMatches` rather than `Parser::try_parse_from`, because the matches carry
    // the one fact the parsed struct cannot: `value_source`, which is how `run`'s config
    // merge tells `--memory 2048` (typed, wins over the file) from the same value arriving
    // as clap's default (loses to the file). The derive path is the same parser — this is
    // its own two-step spelling, not a second grammar.
    let parsed = match Cli::command()
        .try_get_matches_from(std::env::args())
        .and_then(|matches| {
            let mut parsed = Cli::from_arg_matches(&matches)?;
            if let (Command::Run(args), Some((_, sub))) =
                (&mut parsed.command, matches.subcommand())
            {
                args.explicit = cli::Explicit::from_matches(sub);
            }
            Ok(parsed)
        }) {
        Ok(parsed) => parsed,
        Err(error) => {
            // clap's help and version are successes that print themselves. Everything else is an
            // argument error, and it becomes an envelope rather than clap's own rendering so a
            // `--json` consumer is not handed a coloured help screen it cannot parse.
            use clap::error::ErrorKind as ClapKind;
            if matches!(
                error.kind(),
                ClapKind::DisplayHelp
                    | ClapKind::DisplayVersion
                    | ClapKind::DisplayHelpOnMissingArgumentOrSubcommand
            ) {
                print!("{error}");
                return ExitCode::from(Exit::Ok.as_u8());
            }
            return report(&mut out, &exit::from_parse_error(&error));
        }
    };

    // Re-resolved from the parsed flags, which is the authoritative reading: `--json` may have
    // arrived after the subcommand, and the raw scan above only exists for the failure path that
    // never gets here.
    //
    // `manifest` is the one command that is always JSON. The only consumer that asks for a
    // manifest is one that parses it, so a bare `microvm manifest` is already what it wants —
    // `cli.py:2278` makes the same choice with `json: bool = True`. Folded in here rather than read
    // inside the handler, because the format is what decides which stream and which rendering the
    // dispatcher uses, and a handler that could change it after the fact would be a second answer
    // to one question.
    let wants_json = parsed.json || matches!(&parsed.command, Command::Manifest);
    let format = envelope::resolve_format(
        wants_json,
        parsed.dense,
        std::io::IsTerminal::is_terminal(&std::io::stdout()),
    );
    let mut out = Output::new(format, parsed.quiet, std::io::stdout(), std::io::stderr());

    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            // Reported through the envelope like any other failure, and as `ERR_UNEXPECTED`
            // because a runtime that will not build is a bug here or an exhausted machine, not
            // anything about the platform.
            return report(
                &mut out,
                &CliError::new(
                    Exit::Unexpected,
                    format!("could not start the async runtime: {error}"),
                ),
            );
        }
    };

    runtime.block_on(run(&mut out, parsed))
}

/// Runs one parsed invocation.
///
/// The **only** place a success envelope is written, which is what makes CLI-4 structural: no
/// handler can write a second one because no handler writes the first.
async fn run<O: std::io::Write, E: std::io::Write>(
    out: &mut Output<O, E>,
    parsed: Cli,
) -> ExitCode {
    let seam = AwsSeam;
    let infra = infra_for(&parsed.command);
    let dense = out.dense() || out.format().is_json() && parsed.dense;

    let result = {
        let mut ctx = Ctx {
            seam: &seam,
            out,
            infra,
            env: &process_env,
            fetch: &provision::SubprocessFetch,
        };
        handle(&mut ctx, &parsed.command, commands::lifecycle::on_ctrl_c()).await
    };

    match result {
        Ok(rendered) => {
            let envelope = envelope::ok(rendered.kind, rendered.data.clone());
            let text = rendered.text_for(out.dense()).to_string();
            // `constants --emit-json` is the one write that is deliberately not an envelope; its
            // consumer compares key-for-key against a pinned service model, and an envelope
            // would put every comparison behind `["data"]["constants"]`. The flag's help says so.
            let bare = matches!(&parsed.command, Command::Constants(args) if args.emit_json)
                && !parsed.json;
            if bare {
                println!("{text}");
            } else if dense && out.format().is_json() {
                out.emit_compact(&envelope, &text);
            } else {
                maybe_tui(out, &rendered, &envelope, &text);
            }
            // Read *after* the envelope is written, which is what makes a second one impossible
            // rather than merely discouraged. See `commands/mod.rs` on `AlreadyReported`.
            ExitCode::from(rendered.already_reported.unwrap_or(Exit::Ok).as_u8())
        }
        Err(failure) => report(out, &failure),
    }
}

/// Draws the interactive surface where one exists, else writes the text.
///
/// The fallback is unconditional: a TUI that cannot initialise must not cost the caller their
/// output, so [`tui::draw`] returning `false` lands here in the plain path.
fn maybe_tui<O: std::io::Write, E: std::io::Write>(
    out: &mut Output<O, E>,
    rendered: &commands::Rendered,
    envelope: &serde_json::Value,
    text: &str,
) {
    // Three conditions, one `&&` chain: a terminal, a surface for this result type, and a draw that
    // actually succeeded. Any of them false lands in the text path, which is what makes the TUI an
    // enhancement rather than a dependency.
    if out.tui()
        && let Some(grid) = tui_grid(rendered)
        && tui::draw(&grid)
    {
        // The frame *is* the stdout write for this invocation, so the envelope path is not also
        // taken — two renderings of one result is exactly what CLI-4 forbids.
        return;
    }
    out.emit(envelope, text);
}

/// The grid for a command that has an interactive surface, or `None`.
///
/// Three surfaces, and they are the three where alignment earns its keep: a list of outstanding
/// runs, a cost table, and the exit-code catalogue. `run`'s progress is already a stream of stderr
/// lines and a full-screen version of it would fight with the daemon's own output.
fn tui_grid(rendered: &commands::Rendered) -> Option<tui::Grid> {
    match rendered.kind {
        "microvm.runs" => {
            let runs = rendered.data.get("runs")?.as_array()?;
            let mut grid = tui::Grid::new(
                "outstanding runs",
                vec![
                    "run".into(),
                    "microvm".into(),
                    "image".into(),
                    "leaked".into(),
                ],
            );
            let mut leaked_total = 0usize;
            for entry in runs {
                let leaked: Vec<&str> = entry["leaked"]
                    .as_array()
                    .map(|items| items.iter().filter_map(|item| item.as_str()).collect())
                    .unwrap_or_default();
                grid = grid.with_row(vec![
                    entry["runId"].as_str().unwrap_or("-").to_string(),
                    entry["microvmId"].as_str().unwrap_or("-").to_string(),
                    entry["imageIdentifier"].as_str().unwrap_or("-").to_string(),
                    if leaked.is_empty() {
                        "-".to_string()
                    } else {
                        leaked.join(",")
                    },
                ]);
                if !leaked.is_empty() {
                    leaked_total += 1;
                    grid = grid.alarming();
                }
            }
            Some(grid.with_footer(format!(
                "{} run(s), {leaked_total} with something still billing",
                runs.len()
            )))
        }
        "microvm.cost" => {
            let items = rendered.data.get("report")?.get("items")?.as_array()?;
            let mut grid = tui::Grid::new(
                "cost — estimates from published rates, never an invoice",
                vec![
                    "phase".into(),
                    "quantity".into(),
                    "unit".into(),
                    "amount".into(),
                ],
            );
            for item in items {
                let amount = match item["amount"]["kind"].as_str() {
                    Some("unpriced") => "unpriced".to_string(),
                    // The string form, never a float: the figure's exactness is the point.
                    _ => format!("${}", item["amount"]["usd"].as_str().unwrap_or("?")),
                };
                let unpriced = item["amount"]["kind"] == "unpriced";
                grid = grid.with_row(vec![
                    item["phase"].as_str().unwrap_or_default().to_string(),
                    item["quantity"].as_str().unwrap_or_default().to_string(),
                    item["unit"].as_str().unwrap_or_default().to_string(),
                    amount,
                ]);
                if unpriced {
                    // Not an error, but the one row a reader must not add up with the others.
                    grid = grid.alarming();
                }
            }
            let total = rendered.data.get("report")?.get("total")?;
            Some(grid.with_footer(total["render"].as_str().unwrap_or_default().to_string()))
        }
        "microvm.manifest" => {
            let commands = rendered.data.get("commands")?.as_array()?;
            let mut grid = tui::Grid::new(
                "microvm — commands",
                vec!["command".into(), "type".into(), "summary".into()],
            );
            for command in commands {
                grid = grid.with_row(vec![
                    command["name"].as_str().unwrap_or_default().to_string(),
                    command["responseType"]
                        .as_str()
                        .unwrap_or_default()
                        .to_string(),
                    command["summary"].as_str().unwrap_or_default().to_string(),
                ]);
            }
            Some(
                grid.with_footer(
                    "`microvm manifest --json` emits the whole surface including exit codes"
                        .to_string(),
                ),
            )
        }
        _ => None,
    }
}

/// Writes the failure envelope and returns its exit code.
fn report<O: std::io::Write, E: std::io::Write>(
    out: &mut Output<O, E>,
    failure: &CliError,
) -> ExitCode {
    // A command that already emitted a success envelope must not print a second object. Reaching
    // here with something already written would be a bug, so the failure goes to stderr and the
    // code still travels — the alternative is breaking the parse of a document a consumer is
    // mid-read of.
    if out.already_emitted() {
        out.warn(&envelope::render_error(failure));
        return ExitCode::from(failure.exit.as_u8());
    }
    // A stream that failed part-way through has written NDJSON events to stdout and no envelope.
    // The failure envelope becomes the stream's **last line**, compact, which is what
    // `Output::emit` does once `streaming` is set — and it is the right answer rather than a
    // concession: an NDJSON consumer reading line by line needs a terminating record saying why
    // the events stopped, and a document appearing on stderr instead would be one it never looks
    // at. The already-written events stay written, because they are real output the caller
    // received.
    //
    // On the human paths the same failure goes to stderr instead: `stream_bytes` put the child's
    // raw output on stdout, and appending an error message to it would corrupt the file a caller
    // was redirecting into.
    if out.streaming() && !out.format().is_json() {
        out.warn(&envelope::render_error(failure));
        return ExitCode::from(failure.exit.as_u8());
    }
    let text = if out.dense() {
        envelope::render_error_dense(failure)
    } else {
        envelope::render_error(failure)
    };
    out.emit(&envelope::error(failure), &text);
    ExitCode::from(failure.exit.as_u8())
}

/// Resolves the three account values for the commands that take them.
fn infra_for(command: &Command) -> Infra {
    let (bucket, build, execution) = match command {
        Command::Run(args) => (
            args.infra.bucket.clone(),
            args.infra.build_role_arn.clone(),
            args.infra.execution_role_arn.clone(),
        ),
        Command::Build(args) => (
            args.infra.bucket.clone(),
            args.infra.build_role_arn.clone(),
            args.infra.execution_role_arn.clone(),
        ),
        Command::Doctor(args) => (
            args.infra.bucket.clone(),
            args.infra.build_role_arn.clone(),
            args.infra.execution_role_arn.clone(),
        ),
        _ => (None, None, None),
    };
    Infra::resolve(bucket, build, execution, &process_env)
}

/// Routes to the handler. One arm per command, exhaustive by the compiler.
///
/// Exhaustive matters: a thirteenth command added to [`Command`] fails to compile here rather
/// than silently doing nothing, which is the failure mode a `HashMap<&str, fn>` dispatcher has.
///
/// The interrupt is a parameter rather than built in the `run` arm, so the behavioral guard
/// dispatches through this exact function with [`commands::lifecycle::never`] instead of
/// maintaining an eighteen-arm copy that only differs there. `main` passes
/// [`commands::lifecycle::on_ctrl_c`]; nothing is installed until the future is polled, so the
/// seventeen commands that never poll it pay nothing.
async fn handle<O: std::io::Write, E: std::io::Write>(
    ctx: &mut Ctx<'_, O, E>,
    command: &Command,
    interrupt: commands::lifecycle::Interrupt<'_>,
) -> Result<commands::Rendered, CliError> {
    match command {
        // The only command that races the interrupt, because it is the only one that launches
        // (CLI-6). Injected rather than a global handler, so the guard can pass its own.
        Command::Run(args) => commands::lifecycle::run(ctx, args, interrupt).await,
        Command::Build(args) => commands::lifecycle::build(ctx, args).await,
        // The attached block: five commands, one door. See `commands/attached.rs`.
        Command::Exec(args) => commands::attached::exec(ctx, args).await,
        Command::Health(args) => commands::attached::health(ctx, args).await,
        Command::Ack(args) => commands::attached::ack(ctx, args).await,
        Command::Stdin(args) => commands::attached::stdin(ctx, args).await,
        Command::Cp(args) => commands::attached::cp(ctx, args).await,
        // The second command that races the interrupt, and the only one for which the
        // interrupt is the *expected* ending rather than an abort: a tunnel runs until the
        // caller stops it. See `attached::port_forward` on why that exits 0.
        // Both long-running commands take the interrupt, and for both it is the expected
        // ending rather than an abort.
        Command::Tunnel(args) => commands::attached::tunnel(ctx, args, interrupt).await,
        Command::PortForward(args) => commands::attached::port_forward(ctx, args, interrupt).await,
        Command::Suspend(args) => commands::lifecycle::suspend(ctx, args).await,
        Command::Resume(args) => commands::lifecycle::resume(ctx, args).await,
        Command::Terminate(args) => commands::lifecycle::terminate(ctx, args).await,
        Command::Ls(args) => commands::local::ls(ctx, args),
        Command::History(args) => commands::local::history(ctx, args),
        Command::Logs(args) => commands::local::logs(ctx, args),
        Command::Cost(args) => commands::cost::cost(ctx, args),
        Command::Doctor(args) => commands::doctor::doctor(ctx, args).await,
        Command::Manifest => commands::local::manifest(ctx),
        Command::Constants(_) => commands::local::constants(ctx),
        Command::Dockerfile(args) => commands::local::dockerfile(ctx, args),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::envelope::Format;
    use serde_json::json;

    /// The dense-format resolution used by [`run`], as a readable expression.
    ///
    /// Pinned because `--dense --json` means "compact JSON" while `--dense` alone means TSV, and
    /// conflating them would either pretty-print for a token-paying consumer or hand a shell a
    /// JSON document.
    #[test]
    fn dense_and_json_together_mean_compact_json() {
        assert_eq!(envelope::resolve_format(true, true, false), Format::Json);
        assert_eq!(envelope::resolve_format(false, true, false), Format::Dense);
    }

    /// `ls`'s grid marks every run with a leak as an alarm and counts them.
    ///
    /// The count in the footer is what a skimming operator reads, and a grid that drew the rows
    /// but reported zero would be worse than no footer.
    #[test]
    fn the_runs_grid_alarms_on_every_leaked_row_and_counts_them() {
        let mut data = serde_json::Map::new();
        data.insert(
            "runs".into(),
            json!([
                {"runId": "a", "microvmId": "mvm-1", "imageIdentifier": null, "leaked": []},
                {"runId": "b", "microvmId": "mvm-2", "imageIdentifier": "arn:i", "leaked": ["arn:i"]},
                {"runId": "c", "microvmId": "mvm-3", "imageIdentifier": null, "leaked": ["mvm-3"]},
            ]),
        );
        let rendered = commands::Rendered::ok("microvm.runs", data, String::new(), String::new());
        let grid = tui_grid(&rendered).expect("ls has a surface");
        assert_eq!(grid.rows.len(), 3);
        assert_eq!(grid.alarm_rows, [1, 2]);
        assert!(
            grid.footer
                .as_deref()
                .expect("a footer")
                .contains("2 with something still billing"),
            "{:?}",
            grid.footer
        );
    }

    /// The cost grid never renders an unpriced line as a dollar figure.
    ///
    /// The same honesty rule as the JSON and dense paths, in the surface where it would be
    /// easiest to lose: a table wants every cell in a column to look alike.
    #[test]
    fn the_cost_grid_writes_unpriced_rather_than_a_figure() {
        let mut data = serde_json::Map::new();
        data.insert(
            "report".into(),
            json!({
                "items": [
                    {"phase": "image-build", "quantity": "600", "unit": "second",
                     "amount": {"kind": "unpriced", "reason": "AWS does not publish"}},
                    {"phase": "running", "quantity": "7200", "unit": "GB-second",
                     "amount": {"kind": "estimated-usd", "usd": "0.0264"}},
                ],
                "total": {"render": "at least ~$0.03 (estimated), plus 1 unpriced"},
            }),
        );
        let rendered = commands::Rendered::ok("microvm.cost", data, String::new(), String::new());
        let grid = tui_grid(&rendered).expect("cost has a surface");
        assert_eq!(grid.rows[0][3], "unpriced");
        assert_eq!(grid.rows[1][3], "$0.0264");
        assert_eq!(
            grid.alarm_rows,
            [0],
            "the unpriced row is the one not to add up with the others"
        );
        assert!(
            grid.footer
                .as_deref()
                .expect("a footer")
                .contains("at least"),
            "{:?}",
            grid.footer
        );
    }

    /// A command with no interactive surface returns `None` and falls through to text.
    ///
    /// The default, and it has to be the default: a surface invented for `terminate` would be a
    /// full-screen frame for a command whose whole output is one line.
    #[test]
    fn a_command_without_a_surface_falls_through_to_text() {
        let rendered = commands::Rendered::ok(
            "microvm.teardown",
            serde_json::Map::new(),
            "terminated mvm-1".into(),
            String::new(),
        );
        assert!(tui_grid(&rendered).is_none());
    }

    /// A malformed payload does not panic the grid builder.
    ///
    /// Every field read is an `Option` chain ending in `?`, so a missing key produces `None` and
    /// the text path — rather than an index panic in the one code path that runs only on a real
    /// terminal, where a test would never see it.
    #[test]
    fn a_grid_builder_answers_none_rather_than_panicking_on_a_missing_field() {
        for kind in ["microvm.runs", "microvm.cost", "microvm.manifest"] {
            let rendered =
                commands::Rendered::ok(kind, serde_json::Map::new(), String::new(), String::new());
            assert!(tui_grid(&rendered).is_none(), "{kind}");
        }
    }
}
