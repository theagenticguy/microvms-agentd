// SPDX-License-Identifier: Apache-2.0
//! The five commands that address a VM this invocation did not launch.
//!
//! `exec`, `health`, `ack`, `stdin`, `cp`. Grouped by the door rather than by shape: every one of
//! them goes through [`crate::seam::CoreSeam::attach_session`], which is the only door that mints a
//! proxy token for a MicroVM nobody here started — so TRAP-9's refresh-inside-the-request-path
//! lives on this file's code path and on no other. `lifecycle.rs` keeps the commands that create or
//! destroy, because those go through `Sandbox` and `ControlPlane` and none of the STATE-* proofs
//! apply here.
//!
//! # A session holds no state, which is what makes these commands possible at all
//!
//! Every exec record, every file, and the bootstrap token live *in the VM*
//! (`microvms-core/src/session/mod.rs:9`). So `microvm ack x-1` a week later, from a different
//! machine, addresses the same server-side exec as the process that started it — reattaching is
//! just naming the thing. That is a property of the protocol rather than of any type here, and it
//! is why `--exec-id` is worth having: the id *is* the handle.
//!
//! # What none of these commands do
//!
//! **No archive is inspected.** `cp --tar` hands core's `upload_tar` the bytes it was given, and
//! `microvms-core/src/session/files.rs:92` says why: the daemon enforces the member rules, and "a
//! second check on this side would be a second thing to keep in step with them, which is how the
//! two come to disagree". The four hostile-archive conformance checks assert the *daemon's*
//! refusal surfacing as `data.kind`, so a pre-validation here would test this file's copy of the
//! guard instead of the one that runs in production.
//!
//! **No stdin is opened unasked.** `exec` sends `stdin: false` unless `--stdin` was given, because
//! a child holding a pipe nobody will write to blocks forever the first time it reads.
//!
//! **No exec is acked by a stream or a poll.** Both are read-only views onto a server-side object,
//! so a streamed exec stays pollable and a polled one stays readable. `ack` is the only thing that
//! releases output, and it is a separate command precisely so that is a decision rather than a
//! side effect.

use std::ops::ControlFlow;
use std::time::Duration;

use microvms_core::session::{EndReason, ExecEvent, Session, StreamOptions};
use microvms_core::{Error, ErrorKind};
use serde_json::{Map, Value, json};

use crate::cli::{AckArgs, AttachFlags, CpArgs, ExecArgs, HealthArgs, RegionFlags, StdinArgs};
use crate::commands::{Ctx, Rendered, STREAM_RESPONSE, response_type};
use crate::exit::{CliError, Exit};
use crate::history::{Event, History};
use crate::seam::{Attach, state_dir};

/// The prefix that means "this side of the copy is in the VM".
///
/// `vm:` rather than a `--to-vm`/`--from-vm` pair of flags, and rather than `host:path` in the
/// `scp` style. Two reasons. It puts the direction *in the argument it applies to*, so
/// `cp vm:/etc/hosts ./hosts` reads as its own documentation and cannot be transposed the way two
/// positional paths plus a separate direction flag can. And it keeps a bare local path meaning a
/// local path — `scp`'s `host:path` grammar makes `cp C:/x /y` ambiguous on Windows, which is a
/// platform this CLI's CI actually runs on.
const VM_PREFIX: &str = "vm:";

/// How many bytes of stdin go out per write.
///
/// Under the daemon's own `max_stdin_write_bytes`, which defaults to 1 MiB
/// (`agentd/src/config.rs:96`) and answers 413 above it. Chunking here rather than sending one
/// body and reporting the 413 is the difference between `microvm exec --stdin < big.json` working
/// and the caller having to know a limit this CLI could have respected for them. A quarter of the
/// default leaves room for a deployment that lowered it.
const STDIN_CHUNK_BYTES: usize = 256 * 1024;

/// A session against the VM the flags name, and the id it resolved to.
///
/// The one helper every command in this file starts with, so the resolve-then-attach pair is
/// written once. Not merely tidiness: `resolve_region` has to happen *before* the attach, because
/// the region is what the proxy-token mint's ARN is derived for, and a command that attached first
/// and resolved later would mint against the wrong region and read the refusal as a bad token.
///
/// # `--name` is the triple, read back from the registry
///
/// A name resolves through the local registry (`ledger::Names`) with zero AWS calls: the record
/// `run --keep --vm-name` wrote carries the endpoint, the agent token, the MicroVM id, *and the
/// launch region* — so the caller types one word where they pasted four values. The record's
/// region is used only when no `--region` flag was given: a flag is the caller overriding the
/// record, which is the same precedence every other flag-versus-recorded-value pair here has.
/// A name this state directory never registered fails with `ERR_PRECONDITION` naming the
/// registry it looked in, because the record may simply live in another machine's state dir.
///
/// The returned id is the one the session addresses, whichever spelling named it — the history
/// append needs it, and reading it off the resolution here is what keeps a `--name` exec's
/// history under the same id as a triple exec's.
async fn attach<O: std::io::Write, E: std::io::Write>(
    ctx: &Ctx<'_, O, E>,
    region: &RegionFlags,
    flags: &AttachFlags,
) -> Result<(Session, String), CliError> {
    let (attach, region) = resolve_attach(ctx, region, flags)?;
    let microvm_id = attach.microvm_id.clone();
    let session = ctx.seam.attach_session(region, attach).await?;
    Ok((session, microvm_id))
}

/// The triple and region an attach will use, from either spelling. Zero AWS calls.
///
/// Split from [`attach`] so the resolution is testable without a seam: everything here is
/// local — clap guarantees exactly one spelling is present, and the registry is a file read.
fn resolve_attach<O: std::io::Write, E: std::io::Write>(
    ctx: &Ctx<'_, O, E>,
    region: &RegionFlags,
    flags: &AttachFlags,
) -> Result<(Attach, microvms_core::Region), CliError> {
    if let Some(name) = &flags.name {
        let root = state_dir(flags.state_dir.clone(), ctx.env);
        let names = crate::ledger::Names::new(&root);
        let Some(record) = names.lookup(name) else {
            return Err(CliError::new(
                Exit::Precondition,
                format!(
                    "no VM named {name:?} in {}. Names are local: `run --keep --vm-name {name}` \
                     registers one here, and a name registered on another machine lives in that \
                     machine's state directory.",
                    root.join("names").display(),
                ),
            )
            .suggest("`microvm ls` shows this state directory's outstanding runs")
            .suggest("pass the --endpoint/--agent-token/--microvm-id triple directly"));
        };
        // The flag wins over the record, so `exec --name x --region us-west-2` means what it
        // says; the record's region is the default that makes the flag unnecessary.
        let resolved_region = if region.region.is_some() || region.unlisted_region.is_some() {
            region.resolve(ctx.env)?
        } else {
            microvms_core::Region::unlisted(&record.region)
        };
        return Ok((
            Attach {
                endpoint: record.endpoint,
                agent_token: record.agent_token,
                microvm_id: record.microvm_id,
                port: flags.port,
            },
            resolved_region,
        ));
    }
    let expect = |value: &Option<String>, flag: &str| -> Result<String, CliError> {
        value.clone().ok_or_else(|| {
            // Unreachable through the parser — `required_unless_present = "name"` — but the
            // struct is constructible in code, and a message beats a panic if it ever is.
            CliError::new(
                Exit::InvalidArg,
                format!("--{flag} is required unless --name is given"),
            )
        })
    };
    Ok((
        Attach {
            endpoint: expect(&flags.endpoint, "endpoint")?,
            agent_token: expect(&flags.agent_token, "agent-token")?,
            microvm_id: expect(&flags.microvm_id, "microvm-id")?,
            port: flags.port,
        },
        region.resolve(ctx.env)?,
    ))
}

// ── exec ────────────────────────────────────────────────────────────────────

/// Runs one command in a MicroVM that is already running, or reads an exec that is already there.
///
/// Four shapes over one subcommand, and they are one subcommand because they are one question
/// asked at different points in an exec's life: start and wait (`exec CMD`), start and watch
/// (`--stream`), start and feed (`--stdin`), or read an existing one (`--poll`). Splitting them
/// into four commands would mean four copies of the identifier triple and four places for the
/// timeout's meaning to drift.
pub async fn exec<O: std::io::Write, E: std::io::Write>(
    ctx: &mut Ctx<'_, O, E>,
    args: &ExecArgs,
) -> Result<Rendered, CliError> {
    let (session, microvm_id) = attach(ctx, &args.region, &args.attach).await?;

    // `--poll` first, because it is the one shape that starts nothing. Clap's `conflicts_with_all`
    // has already refused it beside every writing flag, so this branch cannot be reached with a
    // command to run.
    if let Some(exec_id) = &args.poll {
        return poll_existing(ctx, &session, exec_id).await;
    }

    let command = args
        .command
        .as_deref()
        // Unreachable: `required_unless_present = "poll"` on the positional, and `--poll` returned
        // above. `expect` rather than a constructed error, because a message for a state clap
        // guarantees cannot happen would be a message nobody can ever read to check it is right.
        .expect("clap requires COMMAND unless --poll was given");

    // Read before the child exists, deliberately. A start that succeeded followed by a stdin read
    // that failed would leave a child holding an open pipe that nothing will ever close — which is
    // the exact hang `stdin: false` is the default to prevent. Failing before the spawn costs a
    // caller nothing.
    let feed = if args.stdin {
        Some(read_local_stdin()?)
    } else {
        None
    };

    let request =
        crate::commands::lifecycle::start_request(crate::commands::lifecycle::StartSpec {
            command,
            cwd: args.cwd.clone(),
            exec_id: args.exec_id.clone(),
            stdin: args.stdin,
            // Collected here rather than in the parser, because clap's `Vec<(String, String)>`
            // is the repeatable flag's natural shape and the wire's `HashMap` is not: a map
            // built in the parser would silently deduplicate before anyone chose to. Later
            // flags win on a repeated KEY, which is the shell convention (`FOO=a FOO=b cmd`
            // runs with `b`).
            env: args.env.iter().cloned().collect(),
            user: args.user,
            group: args.group,
        });
    let exec_id = request.exec_id.clone();
    ctx.out.progress(&format!("exec {exec_id}: {command}"));

    let handle = session.run(request).await?;
    // The id the *daemon* confirmed, which is what every later call has to address. Core already
    // prefers it over the requested one (`session/mod.rs:347`); read it back here so the envelope
    // publishes the same string.
    let exec_id = handle.exec_id().to_string();

    // The VM's history, keyed by the id the attach resolved — the same id whichever spelling
    // (`--microvm-id` or `--name`) the caller used, so one VM's record is one file. Every
    // append below carries the daemon's own report — the confirmed exec id and the outcome
    // fields — and swallows its failures, so an unwritable state dir costs the record and
    // never the exec.
    let history = History::for_vm(
        &state_dir(args.attach.state_dir.clone(), ctx.env),
        &microvm_id,
    );

    if let Some(bytes) = feed {
        write_and_close(ctx, &handle, &bytes).await?;
    }

    let timeout = Duration::from_secs_f64(args.timeout.max(0.0));
    if args.stream {
        return stream_exec(
            ctx,
            &handle,
            &exec_id,
            args.from_offset.unwrap_or(0),
            &history,
        )
        .await;
    }
    if args.detach {
        // Started and nothing else: no wait, and above all no ack. The ack is the irreversible
        // step — it releases the output and a second one is a 409 — so a caller who wants to poll
        // later must be able to get here without one having happened.
        //
        // Reported through the same `render_exec` as every other exec shape, with a synthesized
        // `running` phase rather than a poll. A poll here would be a round trip whose answer is
        // already known (the daemon just returned `phase: running` from the start), and it would
        // introduce a window where a fast command had already exited and this reported `exited`
        // with no output — which reads as a command that produced none.
        let started = microvms_core::session::ExecResult {
            exec_id: exec_id.clone(),
            phase: microvms_core::protocol::exec::Phase::Running,
            outcome: None,
        };
        // A null exit code, honestly: the outcome is not known yet, and a record claiming
        // one would be a record this process never observed.
        history.append(Event::Exec {
            exec_id: exec_id.clone(),
            exit_code: None,
            truncated: false,
            writers_may_be_alive: None,
        });
        return Ok(render_exec(&exec_id, &started));
    }
    let result = handle.wait_and_ack(timeout).await?;
    history.append(Event::Exec {
        exec_id: exec_id.clone(),
        exit_code: result.exit_code(),
        truncated: result
            .outcome
            .as_ref()
            .is_some_and(|outcome| outcome.truncated),
        writers_may_be_alive: result
            .outcome
            .as_ref()
            .map(|outcome| outcome.writers_may_be_alive),
    });
    Ok(render_exec(&exec_id, &result))
}

/// `exec --poll <id>`: an exec's status and output, without touching it.
///
/// A **success** for a running exec, and that is the design decision worth stating: polling is
/// read-only server-side and repeating it costs nothing, so "not finished yet" is an answer rather
/// than a failure. The envelope says `phase: running` with a null `exitCode`, which is honest —
/// and a shell loop `until microvm exec --poll x-1 --json | jq -e .data.exitCode` works because
/// the exit code is 0 throughout.
///
/// An unknown id is a different thing entirely: the daemon answers 404 and that arrives as
/// `ERR_PROTOCOL` with `data.kind: NotFound`, because a caller polling an id that does not exist
/// has a bug rather than a wait.
async fn poll_existing<O: std::io::Write, E: std::io::Write>(
    ctx: &mut Ctx<'_, O, E>,
    session: &Session,
    exec_id: &str,
) -> Result<Rendered, CliError> {
    ctx.out.progress(&format!("polling {exec_id}"));
    let result = session.exec(exec_id).poll().await?;
    let rendered = render_exec(exec_id, &result);
    // A poll of a *running* exec exits 0 whatever the eventual outcome, because there is no
    // outcome yet. `render_exec`'s non-zero reporting keys on an exit code, and a running exec has
    // none, so the branch below only ever fires for one that finished — which is what a caller
    // polling a finished failure wants to see in `$?`.
    Ok(rendered)
}

/// `exec --stream`: NDJSON events on stdout, the envelope last.
///
/// # The cursor arithmetic is core's, and this function must not have its own
///
/// Every offset here comes from core rather than being counted locally.
/// `microvms-core/src/session/exec.rs:16` states the two rules — advance only past bytes actually
/// handed over, and advance past a `gap` too — and core's state machine already obeys them across
/// reconnects. A second cursor maintained in this file would be a second implementation of the
/// property under test, and the two would disagree exactly when a reconnect happened, which is the
/// case nobody tests by hand. So `nextOffset` is read off [`StreamEnd::cursor`], not tallied here.
///
/// # The loop is core's callback driver, not a `Stream`
///
/// [`ExecHandle::for_each_event`] rather than `stream_with` plus `StreamExt::next`, and the reason
/// is CLI-2. `Stream` is not in `std` and core does not re-export the trait, so advancing a stream
/// here meant naming `futures-util` — a seventh direct dependency in a manifest whose exact
/// contents `tests/thinness.rs` asserts, carried to call one method. The driver takes a
/// `FnMut(ExecEvent) -> ControlFlow<()>` and both of those are `std`, so this crate needs nothing
/// but `microvms-core` to consume a stream. The wire behaviour is identical: same state machine,
/// same reconnects, same cursor.
///
/// [`ControlFlow::Continue`] on every event: this command streams to completion, and the `Break`
/// arm exists for a consumer that stops early (a binding whose iterator was dropped). CLI-6's
/// interrupt is a separate mechanism — a `select!` in `lifecycle.rs` — and not this.
///
/// What this function owns is the *reporting*: an event per line, a summary in the envelope, and
/// `nextOffset` so an interrupted consumer can pass it back as `--from-offset`.
async fn stream_exec<O: std::io::Write, E: std::io::Write>(
    ctx: &mut Ctx<'_, O, E>,
    handle: &microvms_core::session::ExecHandle,
    exec_id: &str,
    offset: u64,
    history: &History,
) -> Result<Rendered, CliError> {
    let options = StreamOptions {
        offset,
        ..StreamOptions::default()
    };
    let mut events = 0u64;
    let mut bytes = 0u64;
    let mut gaps = 0u64;
    let mut exit: Option<microvms_core::protocol::exec::ExitEvent> = None;

    // A mid-stream failure is raised rather than summarised, and the events already written stay
    // written. That asymmetry is correct: the bytes on stdout are real output the caller received,
    // and rewriting history to pretend the stream never started would discard them. `main`'s
    // failure path knows stdout has been used — `already_emitted` and `streaming` both say so —
    // and puts the failure on stderr rather than appending a second document to an NDJSON stream.
    let end = handle
        .for_each_event(options, |event| {
            events += 1;
            match &event {
                ExecEvent::Output { data, .. } => bytes += data.len() as u64,
                ExecEvent::Gap { .. } => gaps += 1,
                ExecEvent::Exit(_) => {}
            }
            ctx.out.stream_line(&event_to_json(&event));
            if let ExecEvent::Output { data, .. } = &event {
                // The human path gets the raw bytes on stdout, because that is what they are.
                ctx.out.stream_bytes(data);
            }
            if let ExecEvent::Exit(terminal) = event {
                exit = Some(terminal);
            }
            ControlFlow::Continue(())
        })
        .await?;
    // Core's cursor, whatever the ending. A `Cut` reports where to resume, which is the case the
    // `--from-offset` message below is for; an `Exited` reports the total.
    let next_offset = end.cursor;
    debug_assert!(
        end.reason != EndReason::Stopped,
        "this callback never breaks, so a Stopped ending would mean core reported one that did"
    );

    // The terminal event's own fields, or nulls for a cut stream — a record claiming exit 0
    // for a stream that ended without its exit event would be the same lie the envelope
    // refuses below.
    history.append(Event::Exec {
        exec_id: exec_id.to_string(),
        exit_code: exit.as_ref().and_then(|event| event.exit_code),
        truncated: exit.as_ref().is_some_and(|event| event.truncated),
        writers_may_be_alive: exit.as_ref().map(|event| event.writers_may_be_alive),
    });

    let mut data = Map::new();
    data.insert("execId".into(), json!(exec_id));
    data.insert("events".into(), json!(events));
    data.insert("bytes".into(), json!(bytes));
    data.insert("nextOffset".into(), json!(next_offset));
    data.insert("gaps".into(), json!(gaps));
    // Both read off the terminal event and both null when there was none, which is the case that
    // matters: a stream that ended without an `exit` event was **cut**, and core's own docs say
    // that is the only thing distinguishing a cut from a finished command. Reporting 0 here would
    // turn a truncated stream into a passing build.
    data.insert(
        "exitCode".into(),
        json!(exit.as_ref().and_then(|event| event.exit_code)),
    );
    data.insert(
        "truncated".into(),
        json!(exit.as_ref().map(|event| event.truncated)),
    );

    let code = exit.as_ref().and_then(|event| event.exit_code);
    let text = match code {
        Some(code) => format!("exit code: {code} ({events} events, {bytes} bytes)"),
        None => format!(
            "the stream of {exec_id} ended without an exit event after {bytes} bytes, so the \
             command's outcome is unknown — re-attach with --from-offset {next_offset}"
        ),
    };
    let dense = format!(
        "exit\t{}\t{events}\t{bytes}\t{next_offset}",
        code.map(|c| c.to_string()).unwrap_or_default()
    );
    let (kind, _) = STREAM_RESPONSE;
    let rendered = Rendered::ok(kind, data, text, dense);
    if code != Some(0) {
        // Covers both a non-zero exit and a stream with no terminal event at all. The second is
        // the one worth noting: a cut stream reporting success would make a CI step pass on
        // evidence it never received.
        return Ok(rendered.reporting(Exit::ExecFailed));
    }
    Ok(rendered)
}

/// One stream event as an NDJSON record.
///
/// # Output arrives as text plus a byte count, not as base64, and that is a deliberate limit
///
/// A child's stdout is bytes and JSON strings are UTF-8, so something has to give. The choices
/// were base64 (a `base64` edge is fine to add if a caller ever needs the exact bytes; the
/// concern), an array of integers (lossless and unreadable — `[99,104,117,110,107]` for
/// `chunk`), or lossy text beside the true length. The third is chosen, and `lossy` is set when
/// the conversion actually replaced anything — so a consumer is never silently handed altered
/// bytes: it is told, and `bytes` versus the text's own length is the corroborating evidence.
///
/// The faithful path for a consumer that needs exact bytes is the non-JSON one:
/// [`crate::envelope::Output::stream_bytes`] writes the child's output to stdout untouched, which
/// is what `microvm exec --stream ./tarball.sh > out.tar` relies on.
fn event_to_json(event: &ExecEvent) -> Value {
    match event {
        ExecEvent::Output {
            stream,
            offset,
            data,
        } => {
            let text = String::from_utf8_lossy(data);
            json!({
                "event": "output",
                "stream": match stream {
                    microvms_core::protocol::exec::StreamKind::Stdout => "stdout",
                    microvms_core::protocol::exec::StreamKind::Stderr => "stderr",
                },
                "offset": offset,
                "bytes": data.len(),
                "text": text,
                // `as_bytes() != data` rather than a `from_utf8` probe, so the flag reports what
                // the *reader will see* rather than a separate opinion about the same bytes.
                "lossy": text.as_bytes() != data.as_slice(),
            })
        }
        // `to` and not `to - from` as a length: `to` is where a cursor resumes, which is the only
        // thing a consumer can act on. A length would have to be added to `from` to be useful and
        // is one arithmetic step away from an off-by-one.
        ExecEvent::Gap { from, to } => json!({
            "event": "gap",
            "from": from,
            "to": to,
        }),
        ExecEvent::Exit(terminal) => json!({
            "event": "exit",
            "exitCode": terminal.exit_code,
            "signal": terminal.signal,
            "truncated": terminal.truncated,
            "writersMayBeAlive": terminal.writers_may_be_alive,
            "offset": terminal.offset,
        }),
    }
}

/// An exec result as the `microvm.exec` envelope, shared by `exec`, `--poll`, and `ack`.
///
/// One function because the three are the same fact read at three moments, and three renderings of
/// one shape is three places for `exitCode` to mean something slightly different.
///
/// `phase` is in the payload and is what makes this honest across all three callers: an acked
/// exec's poll reports no output at all (the daemon released it, `agentd/src/exec.rs:429`), and a
/// consumer reading empty `stdout` needs to be able to tell "produced nothing" from "already
/// collected by someone".
fn render_exec(exec_id: &str, result: &microvms_core::session::ExecResult) -> Rendered {
    let mut data = Map::new();
    data.insert("execId".into(), json!(exec_id));
    data.insert("phase".into(), json!(phase_name(result.phase)));
    data.insert("exitCode".into(), json!(result.exit_code()));
    data.insert("stdout".into(), json!(result.stdout()));
    data.insert("stderr".into(), json!(result.stderr()));
    let truncated = result
        .outcome
        .as_ref()
        .is_some_and(|outcome| outcome.truncated);
    data.insert("truncated".into(), json!(truncated));

    let code = result.exit_code();
    let dense = format!(
        "exit\t{}\n{}",
        code.map(|c| c.to_string()).unwrap_or_default(),
        result.stdout()
    );
    let mut lines: Vec<String> = Vec::new();
    for part in [result.stdout(), result.stderr()] {
        if !part.is_empty() {
            lines.push(part.trim_end_matches('\n').to_string());
        }
    }
    lines.push(match (result.done(), code) {
        (_, Some(code)) => format!("exit code: {code}"),
        // Still running, which for a poll is the normal answer and not a failure.
        (false, None) => format!("exec {exec_id} is {}", phase_name(result.phase)),
        // Finished with no exit code: a signal death. Reporting it as one — 0, or 128+n — is how a
        // CI caller reads a killed process as a pass.
        (true, None) => format!("exec {exec_id} died to a signal rather than exiting"),
    });

    let (kind, _) = response_type("exec");
    let rendered = Rendered::ok(kind, data, lines.join("\n"), dense);
    // Keyed on a *present* non-zero code, so a running exec's absent one is not a failure. A
    // signal death is: the exec finished and did not succeed, and `succeeded()` is false for it.
    if result.done() && code != Some(0) {
        return rendered.reporting(Exit::ExecFailed);
    }
    rendered
}

/// The wire spelling of a phase, which is what the envelope publishes.
///
/// Matched rather than `Debug`-formatted: `Debug` would emit `Running` and the protocol's own
/// `rename_all = "snake_case"` makes it `running` on the wire. A consumer comparing against the
/// daemon's own JSON must not have to know which of the two this CLI happened to print.
fn phase_name(phase: microvms_core::protocol::exec::Phase) -> &'static str {
    match phase {
        microvms_core::protocol::exec::Phase::Running => "running",
        microvms_core::protocol::exec::Phase::Exited => "exited",
        microvms_core::protocol::exec::Phase::Acked => "acked",
    }
}

// ── health ──────────────────────────────────────────────────────────────────

/// Asks a running VM's daemon whether it is up, and what its identity repair achieved.
///
/// # Why this command exists when a successful `exec` already implies a live daemon
///
/// Three facts no other command reports. `identityDegraded` is true when a startup repair step
/// failed — measured 2026-08-06, without `additionalOsCapabilities: ["ALL"]` the hostname and
/// boot_id steps fail with EPERM even though the daemon is root — which means the VM is serving
/// with a duplicate machine-id from the shared image. `identityRepaired` distinguishes "repair ran
/// and found nothing" from "repair was switched off". And `diskUnderPressure` is the curve that
/// otherwise becomes visible as `useradd: No space left on device` with every writer in the
/// sandbox already broken.
///
/// It is also the only *unauthenticated* route, which makes it the probe that answers "is the
/// daemon alive" without needing the answer to "is my token right" first.
///
/// # A degraded identity is a warning and not a non-zero exit
///
/// Reported on stderr where `--quiet` cannot suppress it, in the same class as a leak, and the
/// envelope carries the flag. But the exit code stays 0, because the daemon's own contract is that
/// a degraded identity "is never a reason for the daemon to refuse to serve"
/// (`protocol/src/health.rs`) — and a CLI that failed here would tell a caller their VM is broken
/// when what is true is that an operator may want to drain it. Draining is a decision; this
/// command's job is to make the decision possible.
pub async fn health<O: std::io::Write, E: std::io::Write>(
    ctx: &mut Ctx<'_, O, E>,
    args: &HealthArgs,
) -> Result<Rendered, CliError> {
    let (session, microvm_id) = attach(ctx, &args.region, &args.attach).await?;
    ctx.out.progress(&format!("health of {microvm_id}"));
    let health = session.health().await?;

    let mut data = Map::new();
    data.insert("version".into(), json!(health.version));
    data.insert("bootstrapped".into(), json!(health.bootstrapped));
    data.insert("identityDegraded".into(), json!(health.identity_degraded));
    data.insert("identityRepaired".into(), json!(health.identity_repaired));
    // Null rather than zero when free space could not be measured, and the distinction is the
    // daemon's own: "unmeasurable is not full, and a monitor that conflated them would page on a
    // missing statvfs".
    data.insert(
        "diskAvailableBytes".into(),
        json!(health.disk.as_ref().map(|disk| disk.available_bytes)),
    );
    data.insert(
        "diskUnderPressure".into(),
        json!(health.disk.as_ref().map(|disk| disk.under_pressure)),
    );
    // The pair an orchestrator polls on a loop. `busy` is what makes such a loop
    // informed rather than unconditional, and the poll itself is the inbound traffic the
    // platform's idle policy measures — which is the only kind that counts, because the
    // endpoint proxy terminates outside the guest.
    data.insert("busy".into(), json!(health.busy));
    data.insert("execs".into(), json!(health.execs));
    // The daemon's hook observations, respelled camelCase like every other key here.
    // These are daemon-reported facts in the exit code's trust class, with one caveat
    // the history module documents: an in-guest caller can forge ADDITIONAL entries by
    // posting the unauthenticated hook paths, never remove or alter real ones.
    data.insert(
        "hooks".into(),
        json!(
            health
                .hooks
                .iter()
                .map(|observation| json!({
                    "hook": observation.hook,
                    "firedAt": observation.fired_at,
                }))
                .collect::<Vec<_>>()
        ),
    );
    data.insert("hooksDropped".into(), json!(health.hooks_dropped));

    // The observations also land in the VM's local history, deduplicated, so "did my
    // validate hook even run?" is answerable after the VM is gone. The id is always in
    // hand on this command — clap requires --microvm-id unless --name resolved one —
    // and the append swallows its failures like every history write.
    crate::history::append_unseen_hooks(
        &state_dir(args.attach.state_dir.clone(), ctx.env),
        &microvm_id,
        &health.hooks,
    );

    if health.identity_degraded {
        ctx.out.warn(
            "identityDegraded: a startup identity repair step failed, so this VM is serving with a \
             value from the shared image still in place — a duplicate machine-id or boot_id. \
             Measured 2026-08-06: without `additionalOsCapabilities: [\"ALL\"]` the hostname and \
             boot_id steps fail with EPERM even as root, which is what --repair-identity asks for. \
             The daemon serves anyway by design; drain the VM if the duplicate matters.",
        );
    }
    if health.disk.as_ref().is_some_and(|disk| disk.under_pressure) {
        ctx.out.warn(
            "diskUnderPressure: a write would be refused right now. Every other writer in the \
             sandbox will start failing in ways that name themselves rather than the disk.",
        );
    }

    let mut lines = vec![
        format!("daemon {} on {microvm_id}", health.version),
        format!("bootstrapped: {}", health.bootstrapped),
        format!(
            "identity: {}",
            match (health.identity_repaired, health.identity_degraded) {
                (false, _) => "repair switched off",
                (true, true) => "DEGRADED — a repair step failed",
                (true, false) => "repaired, every step succeeded",
            }
        ),
        match &health.disk {
            Some(disk) => format!(
                "disk: {} bytes available, reserve {}{}",
                disk.available_bytes,
                disk.reserve_bytes,
                if disk.under_pressure {
                    " — UNDER PRESSURE"
                } else {
                    ""
                }
            ),
            None => "disk: not measurable on this daemon".to_string(),
        },
        // Both numbers, because they answer different questions and the second is the one
        // that stops a caller terminating a VM whose output nobody has read.
        format!(
            "activity: {}, {} exec(s) registered",
            if health.busy {
                "BUSY — at least one exec is still running"
            } else {
                "idle — nothing is running"
            },
            health.execs,
        ),
    ];
    // One line per hook the daemon observed, `hook=<name> firedAt=<secs>`, the same
    // spelling the history renderer produces so the two reports read as one record.
    // An old daemon reports none and prints none — a "hooks: (not reported)" line
    // would claim knowledge of an absence this client cannot distinguish from an
    // empty log.
    for observation in &health.hooks {
        lines.push(format!(
            "hook={} firedAt={}",
            observation.hook, observation.fired_at
        ));
    }
    if health.hooks_dropped > 0 {
        lines.push(format!(
            "hooks dropped: {} — the daemon's log capped; the entries above are the \
             earliest invocations, which are the platform's own",
            health.hooks_dropped
        ));
    }
    let dense = format!(
        "{}\t{}\t{}\t{}\t{}\t{}\t{}",
        health.version,
        health.bootstrapped,
        health.identity_degraded,
        health.identity_repaired,
        health.busy,
        health.execs,
        health
            .hooks
            .iter()
            .map(|observation| format!("{}@{}", observation.hook, observation.fired_at))
            .collect::<Vec<_>>()
            .join(","),
    );
    let (kind, _) = response_type("health");
    let rendered = Rendered::ok(kind, data, lines.join("\n"), dense);
    if !health.bootstrapped {
        // Reachable only from inside the VM or through a tunnel — the platform forwards no
        // external traffic until the run hook returns 200 — and a real answer when it happens: the
        // daemon is up and the token is not installed, which is a different condition from a dead
        // VM and needs a different remedy.
        return Ok(rendered.reporting(Exit::Platform));
    }
    Ok(rendered)
}

// ── ack ─────────────────────────────────────────────────────────────────────

/// Releases a finished exec's buffered output and starts its collection clock.
///
/// # Why a command rather than something `exec` always does
///
/// `exec` does ack, through `wait_and_ack`, and that is right for the one-shot shape. This exists
/// for the detached one: a caller that started an exec with `--exec-id`, went away, and came back
/// — possibly as a different process on a different machine, since the exec record lives in the
/// VM. For that caller the ack *is* the handover, and it has to be issuable on its own.
///
/// # The two 409s are different facts and the daemon's detail string is what separates them
///
/// A 409 means either the exec has not exited (output is still being written) or an earlier ack
/// already took it. Both arrive here as `ERR_PROTOCOL` with `data.kind: Conflict`, because a shell
/// branching on `$?` cannot act differently on them — but the message carries the daemon's own
/// `still_running` or `already_acked` detail, which is the field a human or a driver reads. The
/// second is what the conformance suite's double-ack check asserts, and it is a 409 rather than a
/// 200-with-empty-output for a stated reason: an empty body would read as "the command produced no
/// output" (`agentd/src/exec.rs:854`).
pub async fn ack<O: std::io::Write, E: std::io::Write>(
    ctx: &mut Ctx<'_, O, E>,
    args: &AckArgs,
) -> Result<Rendered, CliError> {
    let (session, _) = attach(ctx, &args.region, &args.attach).await?;
    ctx.out.progress(&format!("acking {}", args.exec_id));
    // The ack response carries the released output; a poll issued after it reports `acked` with
    // none. Returning this one rather than re-polling is the whole reason core sequences it that
    // way, and getting it backwards is a silent empty-output bug.
    let result = session.exec(&args.exec_id).ack().await?;
    let mut rendered = render_exec(&args.exec_id, &result);
    // An ack's own success is the release, not the workload's verdict. Overriding the exec
    // rendering's non-zero report matters for a script shaped `microvm ack x-1 && collect`: the
    // workload's exit code is in `data.exitCode` where it belongs, and `$?` here answers "was the
    // output released", which is the question this command was asked.
    rendered.already_reported = None;
    rendered.kind = response_type("ack").0;
    Ok(rendered)
}

// ── stdin ───────────────────────────────────────────────────────────────────

/// Writes to a running exec's stdin, and optionally closes it.
///
/// # The refusal is the interesting case, and it is a 409
///
/// An exec started without `stdin: true` has `/dev/null` on its stdin, and the daemon answers
/// **409** with `stdin_not_requested` — "the request is well-formed, it is the exec that cannot
/// accept it, and the fix is at start time rather than in this body" (`agentd/src/exec.rs:700`).
/// That check runs before the pipe lookup, so it is the status a caller hits by forgetting
/// `--stdin` rather than by writing too late.
///
/// The **410** on the same route is a different fact: the pipe is gone, because an earlier `--eof`
/// closed it or the child exited. Both collapse onto `ERR_PROTOCOL`, and `data.kind` — `Conflict`
/// versus `StdinClosed` — is what tells a driver which mistake it made. The conformance suite
/// asserts the first, because "a command that did not ask for stdin must not have one" is the
/// opt-in property, and a suite that accepted either status would pass against a daemon that had
/// stopped distinguishing them.
pub async fn stdin<O: std::io::Write, E: std::io::Write>(
    ctx: &mut Ctx<'_, O, E>,
    args: &StdinArgs,
) -> Result<Rendered, CliError> {
    let (session, _) = attach(ctx, &args.region, &args.attach).await?;
    let bytes = match args.data.as_deref() {
        // `-` reads this process's stdin, which is how a pipe feeds a detached exec. Read before
        // the write so a read failure is not reported as a write failure.
        Some("-") => read_local_stdin()?,
        Some(literal) => literal.as_bytes().to_vec(),
        None => Vec::new(),
    };
    if bytes.is_empty() && !args.eof {
        // Neither data nor EOF is a request with no effect, and the daemon would answer 200 to it
        // — which is worse than a refusal, because the caller would conclude their bytes arrived.
        return Err(CliError::new(
            Exit::InvalidArg,
            "nothing to do: `microvm stdin` needs --data, or --eof, or both. A write of zero bytes \
             without an EOF is accepted by the daemon and changes nothing, so a caller who meant \
             to send something would read the success as delivery.",
        )
        .suggest("--data - reads this process's stdin; --eof closes the pipe"));
    }

    ctx.out.progress(&format!(
        "writing {} bytes to {}{}",
        bytes.len(),
        args.exec_id,
        if args.eof { " and closing it" } else { "" }
    ));
    let handle = session.exec(&args.exec_id);
    let ack = write_stdin_chunked(&handle, &bytes, args.eof).await?;

    let mut data = Map::new();
    data.insert("execId".into(), json!(ack.exec_id));
    data.insert("written".into(), json!(ack.written));
    data.insert("eof".into(), json!(ack.eof));
    let (kind, _) = response_type("stdin");
    Ok(Rendered::ok(
        kind,
        data,
        format!(
            "wrote {} bytes to {}, eof={}",
            ack.written, ack.exec_id, ack.eof
        ),
        format!("{}\t{}\t{}", ack.exec_id, ack.written, ack.eof),
    ))
}

/// Feeds `bytes` to a child's stdin and closes it, for `exec --stdin`.
///
/// Split from the `stdin` command so the two share the chunking rather than the argument parsing:
/// `exec --stdin` always closes, because local stdin ending *is* the EOF, and a flag to keep it
/// open would be a flag for a pipe that is already at end of file.
async fn write_and_close<O: std::io::Write, E: std::io::Write>(
    ctx: &mut Ctx<'_, O, E>,
    handle: &microvms_core::session::ExecHandle,
    bytes: &[u8],
) -> Result<(), CliError> {
    ctx.out.progress(&format!(
        "feeding {} bytes of stdin and closing it",
        bytes.len()
    ));
    write_stdin_chunked(handle, bytes, true).await?;
    Ok(())
}

/// Writes `bytes` in chunks under the daemon's per-write cap, with `eof` on the last one.
///
/// `eof` rides the final write rather than going out as a separate request, because two round
/// trips leave a window where the child has the bytes but not the EOF that says the input is
/// complete — and a `cat` in that window is indistinguishable from a hung one. Core's
/// `write_stdin` takes both for exactly this reason.
///
/// The returned ack is the **last** one, and `written` is therefore the last chunk's count rather
/// than the total. That is the daemon's own field and this function does not invent a sum over it:
/// a total this side computed would agree with the daemon on the happy path and disagree silently
/// on a partial write, which is the case a caller needs the real number for.
async fn write_stdin_chunked(
    handle: &microvms_core::session::ExecHandle,
    bytes: &[u8],
    eof: bool,
) -> Result<microvms_core::session::exec::StdinAck, Error> {
    if bytes.is_empty() {
        return handle.write_stdin(&[], eof).await;
    }
    let mut last = None;
    let mut chunks = bytes.chunks(STDIN_CHUNK_BYTES).peekable();
    while let Some(chunk) = chunks.next() {
        let is_last = chunks.peek().is_none();
        last = Some(handle.write_stdin(chunk, eof && is_last).await?);
    }
    Ok(last.expect("a non-empty slice yields at least one chunk"))
}

/// Every byte of this process's stdin.
///
/// Bytes rather than a string: stdin is bytes, the protocol base64-encodes them, and a
/// UTF-8 validation here would refuse a caller piping a tarball into a child that wanted one.
///
/// A blocking read on the async task, deliberately. This binary has nothing else to do while it
/// waits — no server to keep answering, no other command in flight — so `spawn_blocking` would buy
/// a thread hop and no concurrency. What it would cost is a second place the read can fail.
fn read_local_stdin() -> Result<Vec<u8>, CliError> {
    use std::io::Read as _;
    let mut bytes = Vec::new();
    std::io::stdin().read_to_end(&mut bytes).map_err(|error| {
        CliError::new(Exit::Precondition, format!("could not read stdin: {error}")).suggest(
            "--stdin expects something on this process's stdin, e.g. `... --stdin < input`",
        )
    })?;
    Ok(bytes)
}

// ── cp ──────────────────────────────────────────────────────────────────────

/// Which way a copy goes, resolved from the two positionals before anything is opened.
///
/// An enum rather than two booleans, so "both sides are in the VM" and "neither is" are states the
/// type cannot hold — they are the two mistakes a caller actually makes, and each has a message.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Direction {
    /// Local file to VM.
    Upload,
    /// VM to local file.
    Download,
}

impl Direction {
    fn as_str(self) -> &'static str {
        match self {
            Direction::Upload => "upload",
            Direction::Download => "download",
        }
    }
}

/// Copies a file or a tar archive between here and a running MicroVM.
///
/// # `--tar` is asymmetric: a directory in the VM, an archive file here
///
/// The `vm:` side is a **directory** and the daemon does the work. `GET /v1/fs/tar` refuses
/// anything but a directory (`agentd/src/fs.rs:786` answers 400 "use /v1/fs/file" for a file) and
/// packs it with `pack_tree`; `PUT /v1/fs/tar` extracts into one. The daemon carries the `tar`
/// crate precisely so that nothing else has to — including the guest, whose base image may have no
/// `tar` binary at all.
///
/// The local side is an archive *file*, and that half is a genuine limitation. Neither this crate
/// nor `microvms-core` can pack or unpack: `session/files.rs:112` declines to add the `tar` crate
/// because Rust's standard library has no equivalent of Python tarfile's `data` filter, and "an
/// extraction that looked safe and was not is worse than none". Adding one *here* would be worse
/// still — the daemon's confined extractor is currently the only extractor in the system, and a
/// second one in the client is a second set of member rules to keep in step.
///
/// So: `microvm cp vm:/workspace out.tar --tar` archives a tree, `tar xf out.tar` unpacks it
/// locally, and `microvm cp out.tar vm:/restored --tar` puts it back. Members are stored relative
/// to the packed directory (`append_dir_all(".", root)`), so they extract **flattened** under the
/// destination — which is what makes a downloaded archive re-uploadable, the round trip
/// `fs.rs:226` names as the one a harness performs constantly.
///
/// # No archive is inspected on the way through, and the hostile-archive checks depend on that
///
/// The four conformance checks — parent traversal, absolute link target, symlink redirect,
/// character device — assert that the *daemon* refuses each, surfacing as `ERR_PROTOCOL` with
/// `data.kind: ProtocolError`. A pre-flight check in this function would make those checks pass on
/// this file's copy of the rules while the real extractor went untested, which is the exact
/// substitution the plan's CLI-2 constraint names.
pub async fn cp<O: std::io::Write, E: std::io::Write>(
    ctx: &mut Ctx<'_, O, E>,
    args: &CpArgs,
) -> Result<Rendered, CliError> {
    let (direction, local, remote) = resolve_paths(&args.src, &args.dst)?;
    let (session, _) = attach(ctx, &args.region, &args.attach).await?;

    let bytes = match direction {
        Direction::Upload => {
            let payload = std::fs::read(&local).map_err(|error| {
                CliError::new(
                    Exit::Precondition,
                    format!("could not read {local}: {error}"),
                )
                .suggest(if args.tar {
                    "--tar expects a .tar file; pack a directory first with `tar cf out.tar dir`"
                } else {
                    "the local side of an upload is a file that must already exist"
                })
            })?;
            let count = payload.len();
            ctx.out
                .progress(&format!("uploading {count} bytes to vm:{remote}"));
            if args.tar {
                session.upload_tar(&remote, &payload).await?;
            } else {
                session
                    .upload_file(&remote, &payload, args.mode.as_deref())
                    .await?;
            }
            count
        }
        Direction::Download => {
            ctx.out.progress(&format!("downloading vm:{remote}"));
            let payload = if args.tar {
                session.download_tar(&remote).await?
            } else {
                session.download_file(&remote).await?
            };
            let count = payload.len();
            // Parent directories created, because the remote side's parents are created too
            // (core's `upload_file` says so) and an asymmetry there is a surprise in exactly one
            // direction. A write failure after a successful download is still a failure: the bytes
            // are gone from this process either way, and reporting success would claim a file
            // exists that does not.
            if let Some(parent) = std::path::Path::new(&local).parent()
                && !parent.as_os_str().is_empty()
            {
                std::fs::create_dir_all(parent).map_err(|error| {
                    CliError::new(
                        Exit::Precondition,
                        format!("could not create {}: {error}", parent.display()),
                    )
                })?;
            }
            std::fs::write(&local, &payload).map_err(|error| {
                CliError::new(
                    Exit::Precondition,
                    format!(
                        "downloaded {count} bytes from vm:{remote} but could not write {local}: \
                         {error}. The bytes are gone from this process; run the copy again."
                    ),
                )
            })?;
            count
        }
    };

    let mut data = Map::new();
    data.insert("direction".into(), json!(direction.as_str()));
    data.insert("bytes".into(), json!(bytes));
    data.insert("local".into(), json!(local));
    data.insert("remote".into(), json!(remote));
    data.insert("tar".into(), json!(args.tar));
    let (kind, _) = response_type("cp");
    let arrow = match direction {
        Direction::Upload => format!("{local} -> vm:{remote}"),
        Direction::Download => format!("vm:{remote} -> {local}"),
    };
    Ok(Rendered::ok(
        kind,
        data,
        format!(
            "{arrow} ({bytes} bytes{})",
            if args.tar { ", tar" } else { "" }
        ),
        format!("{}\t{bytes}\t{local}\t{remote}", direction.as_str()),
    ))
}

/// Resolves the two positionals into a direction, a local path, and a remote path.
///
/// Both failures are named rather than guessed at, and neither is recoverable by picking a
/// default: two local paths is a caller who forgot which side the VM is on (and `cp` without a VM
/// is `cp`), and two `vm:` paths is a VM-to-VM copy no route implements — the daemon has no
/// endpoint that reads one VM and writes another, and doing it through this process would need a
/// third argument for the second VM's identifiers.
fn resolve_paths(src: &str, dst: &str) -> Result<(Direction, String, String), CliError> {
    let src_remote = src.strip_prefix(VM_PREFIX);
    let dst_remote = dst.strip_prefix(VM_PREFIX);
    match (src_remote, dst_remote) {
        (None, Some(remote)) => Ok((Direction::Upload, src.to_string(), remote.to_string())),
        (Some(remote), None) => Ok((Direction::Download, dst.to_string(), remote.to_string())),
        (None, None) => Err(CliError::new(
            Exit::InvalidArg,
            format!(
                "neither {src:?} nor {dst:?} names a path in the VM, so this is a local copy. One \
                 side must be prefixed `vm:` — `microvm cp ./f vm:/tmp/f` writes, `microvm cp \
                 vm:/tmp/f ./f` reads."
            ),
        )
        .suggest("`cp` is for crossing the boundary; use your shell's own cp locally")),
        (Some(_), Some(_)) => Err(CliError::new(
            Exit::InvalidArg,
            format!(
                "both {src:?} and {dst:?} are in the VM. There is no VM-to-VM route: the daemon \
                 has no endpoint that reads one and writes another, and routing the bytes through \
                 this process would need a second set of --endpoint/--agent-token/--microvm-id for \
                 the other VM."
            ),
        )
        .suggest("download to a local file, then upload it to the other VM")),
    }
}

// ── sync ─────────────────────────────────────────────────────────────────────

/// A [`crate::sync::SyncError`] as the `ERR_SYNC` row — the same mapping `run <DIR>` uses,
/// spelled here because `lifecycle.rs`'s copy is private to it and the message is the row's
/// contract, not either command's.
fn sync_failure(error: crate::sync::SyncError) -> CliError {
    CliError::new(Exit::Sync, error.to_string())
        .suggest("the failure is on this machine's filesystem; the platform was not involved")
}

/// Re-classifies the daemon's disk-pressure refusal; everything else converts as usual.
///
/// The daemon answers 507 when a write would take the target filesystem under its
/// configured reserve, and its own rationale (`agentd/src/fs.rs`) says why the default
/// classification is wrong for it: a 507 arrives as a 5xx, 5xx maps to `ERR_RETRYABLE`,
/// and retrying an identical upload against a full disk is "correct for a defect and
/// actively harmful for a full disk". So the one status whose remedy is *free space, then
/// retry* is surfaced as `ERR_PLATFORM` — the row for a platform-side condition — with the
/// daemon's own byte counts kept in the message and the remedy attached. No new exit code:
/// the vocabulary already has the right row, the default mapping just cannot know this
/// body means "not until space is freed".
fn classify_upload(error: microvms_core::Error) -> CliError {
    let pressure = error.wire_kind() == Some(microvms_core::WireKind::ServerError)
        && error.to_string().contains("-> 507");
    if !pressure {
        return error.into();
    }
    CliError::new(
        Exit::Platform,
        format!(
            "the VM's disk is under pressure: {error}. diskUnderPressure means a write \
             would be refused right now; the sync was not applied."
        ),
    )
    .suggest("free space in the VM: `microvm exec --name <vm> -- rm -rf /workspace/<big-dir>`")
    .suggest("`microvm health` reports diskAvailableBytes and diskUnderPressure")
    .with_data("diskUnderPressure", json!(true))
}

/// What one sync pass did, for the envelope and the watch loop's running totals.
struct SyncPass {
    uploaded_bytes: usize,
    uploaded_members: usize,
    deleted: usize,
    /// No manifest was found (or `--full` ignored it), so the whole tree travelled.
    full: bool,
    /// The manifest already matched the tree; nothing travelled at all.
    unchanged: bool,
    /// The tree as this pass left it in the guest — the next pass's baseline.
    manifest: crate::sync::Manifest,
}

/// The guest's manifest, or `None` when no sync has written one (or it does not parse).
///
/// A manifest that fails to parse is treated as absent rather than as an error: it means a
/// different version (or a workload) wrote that path, and the safe reading of "I cannot
/// tell what is over there" is the same as "nothing is over there" — a full upload, which
/// re-establishes a manifest this build understands.
async fn remote_manifest<O: std::io::Write, E: std::io::Write>(
    ctx: &mut Ctx<'_, O, E>,
    session: &Session,
) -> Result<Option<crate::sync::Manifest>, CliError> {
    match session.download_file(crate::sync::MANIFEST_PATH).await {
        Ok(bytes) => match serde_json::from_slice::<crate::sync::Manifest>(&bytes) {
            Ok(manifest) => Ok(Some(manifest)),
            Err(error) => {
                ctx.out.progress(&format!(
                    "the guest manifest does not parse ({error}); syncing the whole tree"
                ));
                Ok(None)
            }
        },
        Err(error) if error.wire_kind() == Some(microvms_core::WireKind::NotFound) => Ok(None),
        Err(error) => Err(error.into()),
    }
}

/// Whether a deletion path from the guest manifest is safe to hand to an in-guest `rm`.
///
/// The manifest is read *from the VM*, and the VM is where untrusted work runs — the same
/// trust asymmetry [`crate::sync::extract_artifacts`] documents for the returned archive.
/// A workload that rewrites the manifest to claim `../../etc/passwd` or `/root/.ssh` was
/// synced would otherwise get this CLI to order that deletion on its behalf. Only a
/// relative path with no `..` and no empty component qualifies; anything else is skipped
/// like a hostile archive member, not executed.
fn deletable(path: &str) -> bool {
    !path.is_empty()
        && !path.starts_with('/')
        && !path.contains('\\')
        && path
            .split('/')
            .all(|component| !component.is_empty() && component != "..")
}

/// One incremental pass: hash, diff, upload what changed, delete what vanished, and
/// rewrite the guest manifest. `remote` is the baseline; `None` means upload everything.
async fn sync_pass<O: std::io::Write, E: std::io::Write>(
    ctx: &mut Ctx<'_, O, E>,
    session: &Session,
    dir: &std::path::Path,
    remote: Option<crate::sync::Manifest>,
    delete_timeout: f64,
) -> Result<SyncPass, CliError> {
    let local = crate::sync::manifest(dir).map_err(sync_failure)?;
    let full = remote.is_none();
    let baseline = remote.unwrap_or_default();
    let delta = crate::sync::diff(&local, &baseline);

    if delta.is_empty() && !full {
        return Ok(SyncPass {
            uploaded_bytes: 0,
            uploaded_members: 0,
            deleted: 0,
            full: false,
            unchanged: true,
            manifest: local,
        });
    }

    let mut uploaded_bytes = 0usize;
    let mut uploaded_members = 0usize;
    if !delta.upload.is_empty() {
        let packed = crate::sync::pack_paths(dir, &delta.upload).map_err(sync_failure)?;
        ctx.out.progress(&format!(
            "uploading {} member(s) ({} bytes) to vm:{}",
            packed.members,
            packed.archive.len(),
            crate::sync::REMOTE_WORKDIR,
        ));
        session
            .upload_tar(crate::sync::REMOTE_WORKDIR, &packed.archive)
            .await
            .map_err(classify_upload)?;
        uploaded_bytes = packed.archive.len();
        uploaded_members = packed.members;
    }

    let mut deleted = 0usize;
    let doomed: Vec<&String> = delta.delete.iter().filter(|path| deletable(path)).collect();
    if doomed.len() < delta.delete.len() {
        ctx.out.progress(&format!(
            "skipping {} manifest deletion path(s) that are not plain relative paths — \
             the manifest is the VM's word, and a path that points outside /workspace is \
             refused, not executed",
            delta.delete.len() - doomed.len(),
        ));
    }
    if !doomed.is_empty() {
        ctx.out.progress(&format!(
            "removing {} deleted path(s) in the VM",
            doomed.len()
        ));
        let mut command: Vec<String> = vec!["rm".into(), "-rf".into(), "--".into()];
        command.extend(doomed.iter().map(|path| (*path).clone()));
        let exec_id = format!(
            "microvm-sync-rm-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|epoch| epoch.as_nanos())
                .unwrap_or_default(),
        );
        let result = session
            .run_sync(
                microvms_core::protocol::exec::StartRequest {
                    exec_id,
                    command,
                    shell: false,
                    cwd: Some(crate::sync::REMOTE_WORKDIR.into()),
                    env: Default::default(),
                    user: None,
                    group: None,
                    timeout_sec: Some(delete_timeout),
                    stdin: false,
                },
                Duration::from_secs_f64(delete_timeout.max(1.0) + 30.0),
            )
            .await?;
        match result.outcome {
            Some(outcome) if outcome.exit_code == Some(0) => deleted = doomed.len(),
            outcome => {
                return Err(CliError::new(
                    Exit::ExecFailed,
                    format!(
                        "the in-guest removal of {} deleted path(s) failed: {}. The uploaded \
                         members landed; the guest manifest was left as it was, so the next \
                         sync will order these deletions again.",
                        doomed.len(),
                        outcome
                            .map(|outcome| {
                                let detail = outcome.stderr.trim();
                                if detail.is_empty() {
                                    format!("exit {:?}", outcome.exit_code)
                                } else {
                                    detail.to_string()
                                }
                            })
                            .unwrap_or_else(|| "still running at the deadline".into()),
                    ),
                ));
            }
        }
    }

    let body = serde_json::to_vec(&local).map_err(|error| {
        CliError::new(
            Exit::Unexpected,
            format!("the manifest will not serialize: {error}"),
        )
    })?;
    session
        .upload_file(crate::sync::MANIFEST_PATH, &body, None)
        .await
        .map_err(classify_upload)?;

    Ok(SyncPass {
        uploaded_bytes,
        uploaded_members,
        deleted,
        full,
        unchanged: false,
        manifest: local,
    })
}

/// Whether a watch event under `root` can affect what a sync would upload.
///
/// A filter, not a gate: an event this lets through only triggers a re-hash, and the hash
/// compare decides whether bytes move. So a false positive costs hashing time and a false
/// negative would cost correctness — which is why the list here is exactly the pack's own
/// skip list plus the manifest name, and nothing cleverer.
///
/// `pub(crate)` for the guard in `crate::guards`: the filter's misses are invisible in an
/// integration run (a filtered event just means no pass), so its table is pinned directly.
pub(crate) fn watch_relevant(root: &std::path::Path, path: &std::path::Path) -> bool {
    let Ok(relative) = path.strip_prefix(root) else {
        // Outside the tree — a watcher handed us something we never asked about.
        return false;
    };
    relative.components().all(|component| {
        let name = component.as_os_str();
        name != crate::sync::MANIFEST_NAME
            && ![".git", "target", "node_modules", ".venv"]
                .iter()
                .any(|skipped| name == *skipped)
    })
}

/// The watched root in both spellings the OS may use for it.
///
/// `notify` reports paths as the kernel spells them, not as the caller typed them. On macOS
/// that is the resolved path: a tree under `/var/folders/…` comes back as
/// `/private/var/folders/…`, and a filter that strips only the caller's spelling drops
/// every event as "outside the tree" — the watch looks alive and never re-syncs. Both
/// spellings are accepted. The canonical one is computed once; if the root cannot be
/// resolved, or resolves to itself, there is only the one spelling.
pub(crate) struct WatchRoot {
    given: std::path::PathBuf,
    canonical: Option<std::path::PathBuf>,
}

impl WatchRoot {
    pub(crate) fn new(dir: &std::path::Path) -> Self {
        let canonical = std::fs::canonicalize(dir)
            .ok()
            .filter(|resolved| resolved != dir);
        Self {
            given: dir.to_path_buf(),
            canonical,
        }
    }

    pub(crate) fn relevant(&self, path: &std::path::Path) -> bool {
        watch_relevant(&self.given, path)
            || self
                .canonical
                .as_deref()
                .is_some_and(|root| watch_relevant(root, path))
    }
}

/// Starts the `notify` watcher over `dir` and returns it with the channel it feeds.
///
/// The watcher is returned rather than leaked so the caller's binding keeps it alive for
/// exactly the loop it wakes; dropping it stops the stream.
fn start_watcher(
    dir: &std::path::Path,
) -> Result<
    (
        notify::RecommendedWatcher,
        tokio::sync::mpsc::UnboundedReceiver<Vec<std::path::PathBuf>>,
    ),
    CliError,
> {
    use notify::Watcher as _;
    let (sender, events) = tokio::sync::mpsc::unbounded_channel::<Vec<std::path::PathBuf>>();
    let mut watcher =
        notify::recommended_watcher(move |event: Result<notify::Event, notify::Error>| {
            if let Ok(event) = event {
                let _ = sender.send(event.paths);
            }
        })
        .map_err(|error| {
            CliError::new(
                Exit::Precondition,
                format!("could not start the filesystem watcher: {error}"),
            )
        })?;
    watcher
        .watch(dir, notify::RecursiveMode::Recursive)
        .map_err(|error| {
            CliError::new(
                Exit::Precondition,
                format!("could not watch {}: {error}", dir.display()),
            )
        })?;
    Ok((watcher, events))
}

/// Syncs a project directory into a running VM's workspace, incrementally.
///
/// The incremental half of issue #71. The batch half (`run <DIR>`, issue #72) is
/// launch-coupled: the tree travels once, at launch, whole. This command addresses a VM
/// that is already running — the attach door, like every command in this file — and makes
/// the *n*th transfer proportional to the edit rather than to the tree: hash the local
/// tree, read the manifest the previous sync left in the guest, upload only the members
/// whose identity changed, remove in the guest only what vanished locally.
///
/// # The second sync of an unchanged tree transfers no archive
///
/// That is the acceptance line on the issue, and it falls out of the design rather than
/// being a special case: an unchanged tree diffs to an empty delta, an empty delta packs
/// nothing, uploads nothing, deletes nothing, and leaves the guest manifest untouched.
/// The pass costs one manifest read and the local hashing.
///
/// # `--watch` trusts hashes, not events
///
/// The watcher (the `notify` crate) exists to *wake* the loop, never to decide what
/// changed: every batch of events collapses, after a short debounce, into the same
/// hash-diff-upload pass a bare `sync` runs. A missed event costs staleness until the
/// next event, a spurious one costs a re-hash that uploads nothing — neither costs
/// correctness. Ctrl-C ends the loop as a success carrying the totals, the
/// `port-forward` precedent: a watch the user closed did its job.
pub async fn sync<O: std::io::Write, E: std::io::Write>(
    ctx: &mut Ctx<'_, O, E>,
    args: &crate::cli::SyncArgs,
    interrupt: crate::commands::lifecycle::Interrupt<'_>,
) -> Result<Rendered, CliError> {
    if !args.dir.is_dir() {
        return Err(CliError::new(
            Exit::Precondition,
            format!(
                "{} is not a directory. `sync` moves a project tree; a single file is \
                 `microvm cp <file> vm:{}/<file>`.",
                args.dir.display(),
                crate::sync::REMOTE_WORKDIR,
            ),
        ));
    }
    let (session, microvm_id) = attach(ctx, &args.region, &args.attach).await?;

    // Armed before the first pass, not after it: an edit that lands while the initial
    // upload is in flight must wake the loop too. Registering the watcher afterwards
    // leaves a window in which a save is silently lost until the next one. Events the
    // first pass itself provokes are harmless: they wake one re-hash that diffs to an
    // empty delta.
    let watch = if args.watch {
        Some(start_watcher(&args.dir)?)
    } else {
        None
    };

    let baseline = if args.full {
        None
    } else {
        remote_manifest(ctx, &session).await?
    };
    let first = sync_pass(ctx, &session, &args.dir, baseline, args.timeout).await?;

    let mut passes = 1usize;
    let mut totals = SyncPass { ..first };
    if let Some((_watcher, mut events)) = watch {
        let root = WatchRoot::new(&args.dir);
        report_pass(ctx, &totals);
        ctx.out.progress(&format!(
            "watching {} — Ctrl-C stops and reports the totals",
            args.dir.display()
        ));

        let mut interrupt = interrupt;
        loop {
            let batch = tokio::select! {
                batch = events.recv() => batch,
                () = &mut interrupt => break,
            };
            let Some(batch) = batch else { break };
            let mut relevant = batch.iter().any(|path| root.relevant(path));
            // The debounce: one save fans out into create/modify/rename events, and one
            // sync pass should absorb them all. A quarter second of quiet is the line.
            while let Ok(Some(more)) =
                tokio::time::timeout(Duration::from_millis(250), events.recv()).await
            {
                relevant |= more.iter().any(|path| root.relevant(path));
            }
            if !relevant {
                continue;
            }
            let baseline = std::mem::take(&mut totals.manifest);
            let pass = sync_pass(ctx, &session, &args.dir, Some(baseline), args.timeout).await?;
            passes += 1;
            report_pass(ctx, &pass);
            totals.uploaded_bytes += pass.uploaded_bytes;
            totals.uploaded_members += pass.uploaded_members;
            totals.deleted += pass.deleted;
            totals.unchanged = false;
            totals.manifest = pass.manifest;
        }
    }

    let mut data = Map::new();
    data.insert("microvmId".into(), json!(microvm_id));
    data.insert("workdir".into(), json!(crate::sync::REMOTE_WORKDIR));
    data.insert("uploadedBytes".into(), json!(totals.uploaded_bytes));
    data.insert("uploadedMembers".into(), json!(totals.uploaded_members));
    data.insert("deleted".into(), json!(totals.deleted));
    data.insert("full".into(), json!(totals.full));
    data.insert("unchanged".into(), json!(totals.unchanged));
    data.insert("passes".into(), json!(passes));
    data.insert("watched".into(), json!(args.watch));

    let (kind, _) = response_type("sync");
    let text = if args.watch {
        format!(
            "watched {}: {} pass(es), {} member(s) ({} bytes) uploaded, {} deleted",
            args.dir.display(),
            passes,
            totals.uploaded_members,
            totals.uploaded_bytes,
            totals.deleted,
        )
    } else if totals.unchanged {
        format!(
            "{} is already what vm:{} holds — nothing uploaded (0 bytes)",
            args.dir.display(),
            crate::sync::REMOTE_WORKDIR,
        )
    } else {
        format!(
            "synced {} -> vm:{} ({} member(s), {} bytes uploaded, {} deleted{})",
            args.dir.display(),
            crate::sync::REMOTE_WORKDIR,
            totals.uploaded_members,
            totals.uploaded_bytes,
            totals.deleted,
            if totals.full { ", full" } else { "" },
        )
    };
    let dense = format!(
        "sync\t{}\t{}\t{}\t{}\t{}",
        microvm_id, totals.uploaded_bytes, totals.uploaded_members, totals.deleted, passes,
    );
    Ok(Rendered::ok(kind, data, text, dense))
}

/// One progress line per pass, so a `--watch` session reads as a log of what moved.
fn report_pass<O: std::io::Write, E: std::io::Write>(ctx: &mut Ctx<'_, O, E>, pass: &SyncPass) {
    if pass.unchanged {
        ctx.out.progress("unchanged — nothing uploaded (0 bytes)");
    } else {
        ctx.out.progress(&format!(
            "synced {} member(s) ({} bytes), {} deleted{}",
            pass.uploaded_members,
            pass.uploaded_bytes,
            pass.deleted,
            if pass.full { ", full" } else { "" },
        ));
    }
}

// ── port-forward ────────────────────────────────────────────────────────────

/// Serves a guest port on localhost until Ctrl-C, or until `--max-connections` is reached.
///
/// # The one command whose success envelope is written after the work, not during it
///
/// Every other command here does one round trip and reports it. This one holds a listener open
/// for as long as the caller wants, so the envelope is a *summary* — how many connections were
/// served, how many the proxy refused, and how many tokens were minted. The progress stream
/// carries the live detail, which is why it is on stderr: a caller piping `--json` gets one
/// envelope at the end and nothing interleaved into it (CLI-4).
///
/// # Ctrl-C is a success, not an interrupt
///
/// A tunnel the user closed did its job. So the interrupt resolves the serve loop into the
/// normal reporting path rather than aborting the process, and the exit code is 0 — the same
/// reasoning `run --keep` uses for a workload that exited cleanly. A tunnel that exited
/// non-zero because someone pressed the key that stops it would be a CLI teaching people to
/// ignore its exit codes.
///
/// # The mint count is in the envelope because it is the only evidence of a refresh
///
/// A token cached forever and a token refreshed on schedule produce identical successful
/// requests. `proxyTokenMints` is what distinguishes them, which is the same argument
/// [`microvms_core::session::ProxyAuth::mint_count`] makes for being public at all — and on a
/// tunnel held past the platform's sixty-minute ceiling it is the number that says the ceiling
/// was crossed rather than survived by luck.
pub async fn port_forward<O: std::io::Write, E: std::io::Write>(
    ctx: &mut Ctx<'_, O, E>,
    args: &crate::cli::PortForwardArgs,
    interrupt: crate::commands::lifecycle::Interrupt<'_>,
) -> Result<Rendered, CliError> {
    use microvms_core::session::forward;

    // Refused before the attach, so a mistyped pair costs no AWS call. See
    // `crate::cli::parse_port_pair` on why the mint is the thing worth not spending.
    let (local_port, guest_port) = crate::cli::parse_port_pair(&args.ports).map_err(|detail| {
        CliError::new(Exit::InvalidArg, detail)
            .suggest("`microvm port-forward 8080` forwards 8080 to 8080")
            .suggest("`microvm port-forward 3000:8080` serves the guest's 8080 on local 3000")
    })?;

    let bind: std::net::SocketAddr =
        format!("{}:{local_port}", args.bind)
            .parse()
            .map_err(|err| {
                CliError::new(
                Exit::InvalidArg,
                format!(
                    "--bind {:?} with local port {local_port} is not an address this process can \
                     listen on: {err}",
                    args.bind
                ),
            )
            .suggest("`--bind 127.0.0.1` is the default and is what you want for a browser here")
            })?;

    let (session, microvm_id) = attach(ctx, &args.region, &args.attach).await?;

    // A direct session mints nothing, so there is no port-scoped credential to forward with —
    // and a tunnel that silently sent unauthenticated requests would fail at the proxy with a
    // message about the token rather than about the missing minter.
    let Some(auth) = session.proxy_auth().cloned() else {
        return Err(CliError::new(
            Exit::Precondition,
            "this session reaches the daemon directly rather than through the endpoint proxy, so \
             there is no port-scoped token to forward with. Port forwarding exists to cross the \
             proxy; a direct session is already on the other side of it."
                .to_string(),
        ));
    };

    let spec = forward::ForwardSpec::new(bind, guest_port, session.endpoint().to_string());
    let listener = forward::bind(&spec).await?;
    let bound = listener.local_addr().map_err(|err| {
        Error::new(
            ErrorKind::Unexpected,
            format!("the listener has no address: {err}"),
        )
    })?;

    ctx.out.progress(&format!(
        "forwarding localhost:{} -> {microvm_id} port {guest_port} (ctrl-c to stop)",
        bound.port()
    ));

    // One client for the whole tunnel: connection reuse to the endpoint is what keeps a
    // page-load of thirty assets from paying thirty TLS handshakes. Built through core's
    // newtype, because this crate cannot name an HTTP client (CLI-2's thinness guard).
    let client = forward::ForwardClient::new()?;

    let mut served: u32 = 0;
    let mut refused: u32 = 0;
    let mut upgrades: u32 = 0;
    let mut interrupted = false;
    let mut interrupt = interrupt;

    loop {
        if args.max_connections.is_some_and(|max| served >= max) {
            break;
        }

        let accepted = tokio::select! {
            // Biased so a pending interrupt wins over a connection that arrived in the same
            // wakeup: the caller pressed the key, and serving one more request first would look
            // like the key did nothing.
            biased;
            () = &mut interrupt => {
                interrupted = true;
                break;
            }
            accepted = forward::accept(&listener) => accepted,
        };

        let (local, peer) = match accepted {
            Ok(pair) => pair,
            Err(error) => {
                // An accept failure is the listener's problem, not one connection's, so this
                // ends the tunnel rather than looping on a socket that will keep failing.
                ctx.out
                    .warn(&format!("the local listener stopped: {error}"));
                break;
            }
        };

        let mut events = Vec::new();
        let outcome = forward::serve_connection(local, &spec, &auth, &client, |event| {
            events.push(event);
        })
        .await;

        for event in &events {
            match event {
                forward::ForwardEvent::Refused { explanation, .. } => {
                    refused += 1;
                    // A warning rather than a failure: one refused request must not tear down a
                    // tunnel whose other ports or paths are working, and the sentence is the
                    // 403-vs-502 diagnostic the user needs to act.
                    ctx.out.warn(explanation);
                }
                forward::ForwardEvent::Forwarded { upgraded: true, .. } => upgrades += 1,
                _ => {}
            }
        }

        match outcome {
            Ok(()) => served += 1,
            Err(error) => {
                // Per-connection, so a dev server that dropped one request does not end the
                // tunnel. The peer is named because with several tabs open it is the only way to
                // tell which connection failed.
                ctx.out
                    .warn(&format!("the connection from {peer} ended early: {error}"));
                served += 1;
            }
        }
    }

    let mints = auth.mint_count();
    let mut data = Map::new();
    data.insert("microvmId".into(), json!(microvm_id));
    data.insert("localPort".into(), json!(bound.port()));
    data.insert("localAddress".into(), json!(bound.to_string()));
    data.insert("guestPort".into(), json!(guest_port));
    data.insert("connectionsServed".into(), json!(served));
    data.insert("connectionsRefused".into(), json!(refused));
    data.insert("upgrades".into(), json!(upgrades));
    // See the doc comment: the only externally visible evidence that the refresh schedule ran.
    data.insert("proxyTokenMints".into(), json!(mints));
    data.insert("interrupted".into(), json!(interrupted));

    let text = format!(
        "forwarded localhost:{} -> {microvm_id} port {guest_port}\n\
         connections: {served} served, {refused} refused by the proxy, {upgrades} upgraded\n\
         proxy tokens minted: {mints}\n\
         stopped: {}",
        bound.port(),
        if interrupted {
            "ctrl-c"
        } else {
            "connection limit reached"
        }
    );
    let dense = format!(
        "port-forward {}->{guest_port} served={served} refused={refused} upgrades={upgrades} \
         mints={mints}",
        bound.port()
    );

    let (kind, _) = response_type("port-forward");
    Ok(Rendered::ok(kind, data, text, dense))
}

// ── tunnel ──────────────────────────────────────────────────────────────────

/// Tunnels arbitrary TCP to a guest port until Ctrl-C, or until `--max-connections`.
///
/// # Where this differs from `port-forward`, and why both exist
///
/// `port-forward` re-issues HTTP requests; this carries bytes. The split is forced by the
/// platform rather than chosen: the endpoint proxy has no CONNECT method, and an upgrade
/// replayed over its HTTPS path is answered 400 (measured 2026-08-29), so raw TCP has to ride
/// inside WebSocket binary frames to a relay in the guest. A single command could not do both
/// without deciding per connection whether the payload was HTTP, which is a guess about
/// somebody else's protocol.
///
/// # One task per connection, and a spawn is the whole concurrency story
///
/// Each accepted connection gets its own WebSocket and its own task, matching the daemon's
/// no-multiplexing decision. `psql` opens one connection and `ssh` opens one; a client that
/// opens five gets five tunnels, and none of them can stall another.
///
/// # Ctrl-C is a success
///
/// Same reasoning as `port-forward`: a tunnel the user closed did its job, so the interrupt
/// resolves into the reporting path and the exit code is 0.
pub async fn tunnel<O: std::io::Write, E: std::io::Write>(
    ctx: &mut Ctx<'_, O, E>,
    args: &crate::cli::TunnelArgs,
    interrupt: crate::commands::lifecycle::Interrupt<'_>,
) -> Result<Rendered, CliError> {
    use microvms_core::session::{forward, tunnel as core_tunnel};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, Ordering};

    // Refused before the attach, so a mistyped pair costs no AWS call — the same ordering
    // `port-forward` uses, and the same shared parser, so the two cannot disagree about what
    // `15432:5432` means.
    let (local_port, guest_port) = crate::cli::parse_port_pair(&args.ports).map_err(|detail| {
        CliError::new(Exit::InvalidArg, detail)
            .suggest("`microvm tunnel 5432` tunnels the guest's 5432 to local 5432")
            .suggest("`microvm tunnel 15432:5432` moves it aside when something local holds 5432")
    })?;

    let bind: std::net::SocketAddr =
        format!("{}:{local_port}", args.bind)
            .parse()
            .map_err(|err| {
                CliError::new(
                Exit::InvalidArg,
                format!(
                    "--bind {:?} with local port {local_port} is not an address this process can \
                     listen on: {err}",
                    args.bind
                ),
            )
            .suggest("`--bind 127.0.0.1` is the default, and the right choice for a local client")
            })?;

    // The identity material, resolved before the attach so a caller missing it pays no AWS
    // call. Explicit flags win over the registry record for the region flag's reason: the
    // caller who typed them means them, and the record is the default that makes them
    // unnecessary. Fails closed — `--verify-identity` with material nowhere is a refusal,
    // never a silent downgrade to an unverified tunnel.
    let identity = if args.verify_identity {
        let (host_seed, vm_public_key) = match (
            &args.identity_host_seed,
            &args.identity_vm_public_key,
            &args.attach.name,
        ) {
            (Some(seed), Some(pin), _) => (seed.clone(), pin.clone()),
            (None, None, Some(name)) => {
                let root = state_dir(args.attach.state_dir.clone(), ctx.env);
                let record = crate::ledger::Names::new(&root).lookup(name);
                match record
                    .and_then(|record| record.identity_host_seed.zip(record.identity_vm_public_key))
                {
                    Some(pair) => pair,
                    None => {
                        return Err(CliError::new(
                            Exit::Precondition,
                            format!(
                                "the record for {name:?} carries no identity material, so there \
                                 is nothing to verify the VM against. Identity is generated at \
                                 launch and cannot be added to a running VM."
                            ),
                        )
                        .suggest("relaunch with `microvm run --keep --vm-name <NAME> --identity`")
                        .suggest("drop --verify-identity to use an unverified tunnel"));
                    }
                }
            }
            _ => {
                return Err(CliError::new(
                    Exit::InvalidArg,
                    "--verify-identity needs the identity pair: --name <NAME> whose record \
                     carries it, or both --identity-host-seed and --identity-vm-public-key \
                     from `run --identity`'s envelope."
                        .to_string(),
                ));
            }
        };
        Some(microvms_core::identity::TunnelIdentity::from_encoded_parts(
            &host_seed,
            &vm_public_key,
        )?)
    } else {
        None
    };

    let (session, microvm_id) = attach(ctx, &args.region, &args.attach).await?;

    let Some(auth) = session.proxy_auth().cloned() else {
        return Err(CliError::new(
            Exit::Precondition,
            "this session reaches the daemon directly rather than through the endpoint proxy, so \
             there is no port-scoped token to tunnel with. A tunnel exists to cross the proxy; a \
             direct session is already on the other side of it."
                .to_string(),
        ));
    };

    // The listener is `forward`'s, because binding a local port and naming the collision is the
    // same problem for both commands and a second copy would be a second error message.
    let spec = forward::ForwardSpec::new(bind, guest_port, session.endpoint().to_string());
    let listener = forward::bind(&spec).await?;
    let bound = listener.local_addr().map_err(|err| {
        Error::new(
            ErrorKind::Unexpected,
            format!("the listener has no address: {err}"),
        )
    })?;

    ctx.out.progress(&format!(
        "tunnelling localhost:{} -> {microvm_id} tcp/{guest_port} (ctrl-c to stop)",
        bound.port()
    ));

    let endpoint = Arc::new(session.endpoint().to_string());
    let agent_token = Arc::new(session.agent_token().to_string());
    // Shared counters rather than returned values, because each connection lives in its own
    // task and the envelope is written after the loop.
    let served = Arc::new(AtomicU32::new(0));
    let refused = Arc::new(AtomicU32::new(0));
    let mut refusals: Vec<String> = Vec::new();
    let mut interrupted = false;
    let mut interrupt = interrupt;
    let mut tasks = Vec::new();

    loop {
        if args
            .max_connections
            .is_some_and(|max| served.load(Ordering::SeqCst) >= max)
        {
            break;
        }

        let accepted = tokio::select! {
            // Biased, so a pending interrupt wins over a connection that arrived in the same
            // wakeup: the caller pressed the key, and accepting one more first looks like the
            // key did nothing.
            biased;
            () = &mut interrupt => {
                interrupted = true;
                break;
            }
            accepted = forward::accept(&listener) => accepted,
        };

        let (local, peer) = match accepted {
            Ok(pair) => pair,
            Err(error) => {
                ctx.out
                    .warn(&format!("the local listener stopped: {error}"));
                break;
            }
        };

        served.fetch_add(1, Ordering::SeqCst);
        let endpoint = Arc::clone(&endpoint);
        let token = Arc::clone(&agent_token);
        let auth = Arc::clone(&auth);
        let refused = Arc::clone(&refused);
        let identity = identity.clone();
        tasks.push(tokio::spawn(async move {
            let outcome = match &identity {
                None => {
                    core_tunnel::relay_connection(local, &endpoint, guest_port, &token, &auth).await
                }
                Some(identity) => {
                    core_tunnel::relay_connection_verified(
                        local, &endpoint, guest_port, &token, &auth, identity,
                    )
                    .await
                }
            };
            match outcome {
                Ok(core_tunnel::TunnelEnd::Closed) => None,
                Ok(core_tunnel::TunnelEnd::Refused { code, reason }) => {
                    refused.fetch_add(1, Ordering::SeqCst);
                    // The relay's own sentence when it sent one, and core's explanation of the
                    // code otherwise — never a bare number, which tells a caller nothing about
                    // which component refused.
                    Some(if reason.is_empty() {
                        core_tunnel::explain_close(code, guest_port)
                            .unwrap_or_else(|| format!("the tunnel closed with code {code}"))
                    } else {
                        reason
                    })
                }
                Err(error) => {
                    refused.fetch_add(1, Ordering::SeqCst);
                    Some(format!("the tunnel from {peer} failed: {error}"))
                }
            }
        }));
    }

    // Drained rather than abandoned: a task still relaying when the loop ends holds bytes the
    // caller's client is waiting for, and dropping the handle would truncate them.
    for task in tasks {
        if let Ok(Some(detail)) = task.await {
            refusals.push(detail);
        }
    }
    for detail in &refusals {
        ctx.out.warn(detail);
    }

    let served = served.load(Ordering::SeqCst);
    let refused = refused.load(Ordering::SeqCst);
    let mints = auth.mint_count();

    let mut data = Map::new();
    data.insert("microvmId".into(), json!(microvm_id));
    data.insert("localPort".into(), json!(bound.port()));
    data.insert("localAddress".into(), json!(bound.to_string()));
    data.insert("guestPort".into(), json!(guest_port));
    data.insert("connectionsServed".into(), json!(served));
    data.insert("connectionsRefused".into(), json!(refused));
    // The same observable `port-forward` publishes, and for the same reason: a token cached
    // forever and one refreshed on schedule produce identical successful tunnels.
    data.insert("proxyTokenMints".into(), json!(mints));
    data.insert("interrupted".into(), json!(interrupted));

    let text = format!(
        "tunnelled localhost:{} -> {microvm_id} tcp/{guest_port}\n\
         connections: {served} served, {refused} refused\n\
         proxy tokens minted: {mints}\n\
         stopped: {}",
        bound.port(),
        if interrupted {
            "ctrl-c"
        } else {
            "connection limit reached"
        }
    );
    let dense = format!(
        "tunnel {}->{guest_port} served={served} refused={refused} mints={mints}",
        bound.port()
    );

    let (kind, _) = response_type("tunnel");
    Ok(Rendered::ok(kind, data, text, dense))
}

// ── shell ───────────────────────────────────────────────────────────────────

/// How often the shell session re-reads the terminal size, to turn a window drag into a
/// resize control frame.
///
/// A poll rather than SIGWINCH, deliberately: the signal is Unix-only and this binary
/// ships on Windows, the size read is one cheap ioctl, and a poll cannot race the raw
/// stdin reader the way an event stream consuming stdin would. Half a second is far
/// below how long a human takes to finish dragging a window.
const RESIZE_POLL_INTERVAL: Duration = Duration::from_millis(500);

/// Puts the local terminal into raw mode and guarantees it comes back.
///
/// A drop guard rather than paired calls, because every early return between the two —
/// a dropped connection, a `?` on a core error — would otherwise leave the caller's
/// terminal eating its own keystrokes. `enable` failing is not an error: a piped stdin
/// has no terminal to make raw, and the session still works line-buffered.
struct RawModeGuard {
    enabled: bool,
}

impl RawModeGuard {
    fn enable() -> Self {
        Self {
            enabled: ratatui::crossterm::terminal::enable_raw_mode().is_ok(),
        }
    }
}

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        if self.enabled {
            let _ = ratatui::crossterm::terminal::disable_raw_mode();
        }
    }
}

/// The local terminal's size, or `(0, 0)` when there is no terminal to ask.
///
/// `(0, 0)` is what core's `resize_frame` refuses to send, so a piped invocation simply
/// never resizes — the guest keeps its size-unknown default, which is the honest state.
fn local_terminal_size() -> (u16, u16) {
    ratatui::crossterm::terminal::size().unwrap_or((0, 0))
}

/// Opens an interactive PTY in a running MicroVM and pumps raw terminal bytes until the
/// shell exits or this side hangs up.
///
/// # The order of operations is the safety story
///
/// Resolve (zero AWS calls), then mint through the control-plane door, then — only once
/// there is a session to run — raw mode. A refusal at the mint (the common failure: a VM
/// launched without `run --shell` answers with a ValidationException naming
/// `SHELL_INGRESS`) therefore never touches the caller's terminal.
///
/// # No interrupt parameter, deliberately
///
/// In raw mode Ctrl-C is a `0x03` byte on stdin, delivered to the guest shell as SIGINT
/// — which is the whole point of a PTY. Leaving is `exit` or Ctrl-D, and closing stdin
/// ends the session cleanly.
///
/// # This command's exit code says nothing about commands run inside the shell
///
/// The protocol has no exit-status channel (docs/PLATFORM.md, 2026-08-15): the shell's
/// exit is a WebSocket close, and a caller who needs a command's status asks the shell
/// (`echo $?`). Reporting one here would be inventing data.
pub async fn shell<O: std::io::Write, E: std::io::Write>(
    ctx: &mut Ctx<'_, O, E>,
    args: &crate::cli::ShellArgs,
) -> Result<Rendered, CliError> {
    use microvms_core::session::shell as core_shell;

    // Which VM, resolved locally — the same name-or-explicit precedence `attach` has,
    // minus the agent token the shell never sends.
    let (endpoint, microvm_id, region) = if let Some(name) = &args.name {
        let root = state_dir(args.state_dir.clone(), ctx.env);
        let names = crate::ledger::Names::new(&root);
        let Some(record) = names.lookup(name) else {
            return Err(CliError::new(
                Exit::Precondition,
                format!(
                    "no VM named {name:?} in {}. Names are local: `run --keep --vm-name \
                     {name}` registers one here, and a name registered on another machine \
                     lives in that machine's state directory.",
                    root.join("names").display(),
                ),
            )
            .suggest("`microvm ls` shows this state directory's outstanding runs")
            .suggest("pass --endpoint and --microvm-id directly"));
        };
        let region = if args.region.region.is_some() || args.region.unlisted_region.is_some() {
            args.region.resolve(ctx.env)?
        } else {
            microvms_core::Region::unlisted(&record.region)
        };
        (record.endpoint, record.microvm_id, region)
    } else {
        let expect = |value: &Option<String>, flag: &str| -> Result<String, CliError> {
            value.clone().ok_or_else(|| {
                // Unreachable through the parser (`required_unless_present = "name"`),
                // but the struct is constructible in code and a message beats a panic.
                CliError::new(
                    Exit::InvalidArg,
                    format!("--{flag} is required unless --name is given"),
                )
            })
        };
        (
            expect(&args.endpoint, "endpoint")?,
            expect(&args.microvm_id, "microvm-id")?,
            args.region.resolve(ctx.env)?,
        )
    };

    // The mint, through the control-plane door. This is where a VM launched without
    // `run --shell` fails, with the service naming the missing connector — before raw
    // mode, before any terminal state is touched.
    let plane = ctx.seam.control_plane(region).await?;
    let token = plane.mint_shell_auth_token(&microvm_id).await?;

    ctx.out.progress(&format!(
        "shell to {microvm_id} — leave with `exit` or Ctrl-D; Ctrl-C goes to the guest"
    ));

    // The size feed: current value now (sent as the session's first control frame —
    // the guest believes the terminal is 0x0 until told otherwise), then a poll that
    // publishes only actual changes, so the watch channel wakes the relay only when a
    // resize frame is genuinely due.
    let (size_tx, size_rx) = tokio::sync::watch::channel(local_terminal_size());
    let poller = tokio::spawn(async move {
        loop {
            tokio::time::sleep(RESIZE_POLL_INTERVAL).await;
            let size = local_terminal_size();
            size_tx.send_if_modified(|current| {
                if *current == size {
                    false
                } else {
                    *current = size;
                    true
                }
            });
        }
    });

    // Raw mode last, so every failure above leaves the terminal untouched; the guard
    // restores it on every path out, early `?` returns included.
    let raw = RawModeGuard::enable();
    let outcome = core_shell::run_shell(
        tokio::io::stdin(),
        tokio::io::stdout(),
        &endpoint,
        token.value(),
        size_rx,
    )
    .await;
    drop(raw);
    poller.abort();
    let outcome = outcome?;

    let (end, close_reason) = match outcome.end {
        core_shell::ShellEnd::Exited { reason } => ("shell-exited", reason),
        core_shell::ShellEnd::LocalClosed => ("local-closed", String::new()),
        core_shell::ShellEnd::Disconnected { code, reason } => {
            let said = if reason.is_empty() {
                String::new()
            } else {
                format!(", reason {reason:?}")
            };
            return Err(CliError::new(
                Exit::Platform,
                format!(
                    "the shell session ended without the shell exiting: close code \
                     {code}{said}. 1006 with no reason is what every endpoint-proxy \
                     failure collapses to — a dropped connection, or a VM that suspended \
                     or terminated mid-session."
                ),
            )
            .suggest("`microvm health` against the same VM says whether it is still up")
            .suggest("a suspended VM resumes with `microvm resume`; then open a new shell"));
        }
    };

    let mut data = Map::new();
    data.insert("microvmId".into(), json!(microvm_id));
    data.insert("sessionId".into(), json!(outcome.session_id));
    data.insert("end".into(), json!(end));
    data.insert("closeReason".into(), json!(close_reason));

    let text = format!("shell to {microvm_id} ended: {end}");
    let dense = format!("shell {microvm_id} end={end}");
    let (kind, _) = response_type("shell");
    Ok(Rendered::ok(kind, data, text, dense))
}

/// Re-exported so [`ErrorKind`] is nameable in this module's documentation.
#[allow(unused_imports, reason = "named in the documentation above")]
use ErrorKind as _DocsOnly;

#[cfg(test)]
mod tests {
    use super::*;

    /// **The direction grammar, including both ways it can be wrong.**
    ///
    /// The two failures are the point. A `cp` that guessed — treating two local paths as an upload
    /// to a path that happens to look local — would write to the VM at a path the caller never
    /// meant, which is a silent wrong answer rather than a refusal.
    #[test]
    fn one_side_must_be_in_the_vm_and_exactly_one() {
        let (direction, local, remote) =
            resolve_paths("./out.tar", "vm:/tmp/dst").expect("an upload");
        assert_eq!(direction, Direction::Upload);
        assert_eq!(local, "./out.tar");
        assert_eq!(remote, "/tmp/dst");

        let (direction, local, remote) =
            resolve_paths("vm:/tmp/src", "./in.tar").expect("a download");
        assert_eq!(direction, Direction::Download);
        assert_eq!(local, "./in.tar");
        assert_eq!(remote, "/tmp/src");

        let neither = resolve_paths("./a", "./b").expect_err("a local copy is not this command");
        assert_eq!(neither.exit, Exit::InvalidArg);
        assert!(neither.message.contains("vm:"), "{}", neither.message);

        let both = resolve_paths("vm:/a", "vm:/b").expect_err("there is no VM-to-VM route");
        assert_eq!(both.exit, Exit::InvalidArg);
        assert!(
            both.message.contains("VM-to-VM"),
            "the message must say why rather than only that it is refused: {}",
            both.message
        );
    }

    /// A Windows drive letter is a local path, not a `vm:` prefix.
    ///
    /// The reason the prefix is `vm:` rather than `scp`'s `host:path`: `C:/x` contains a colon, and
    /// a grammar that split on the first one would read the drive letter as a remote host. This CI
    /// runs on Windows.
    #[test]
    fn a_windows_drive_letter_is_a_local_path() {
        let (direction, local, _) =
            resolve_paths("C:/work/out.tar", "vm:/tmp/dst").expect("an upload");
        assert_eq!(direction, Direction::Upload);
        assert_eq!(local, "C:/work/out.tar");
    }

    /// An output event reports the byte count and the text, and flags a lossy conversion.
    ///
    /// Both directions, because a `lossy` flag that was always true or always false would be
    /// worse than none: a consumer would either distrust every record or trust a corrupted one.
    #[test]
    fn an_output_event_reports_bytes_and_says_when_the_text_is_lossy() {
        let clean = event_to_json(&ExecEvent::Output {
            stream: microvms_core::protocol::exec::StreamKind::Stdout,
            offset: 7,
            data: b"chunk-1\n".to_vec(),
        });
        assert_eq!(clean["event"], "output");
        assert_eq!(clean["stream"], "stdout");
        assert_eq!(clean["offset"], 7);
        assert_eq!(clean["bytes"], 8);
        assert_eq!(clean["text"], "chunk-1\n");
        assert_eq!(clean["lossy"], false);

        // 0xff is not valid UTF-8, so the text is a replacement character and the flag says so.
        // `bytes` is still the true length, which is the corroborating evidence.
        let binary = event_to_json(&ExecEvent::Output {
            stream: microvms_core::protocol::exec::StreamKind::Stderr,
            offset: 0,
            data: vec![0xff, 0xfe],
        });
        assert_eq!(binary["stream"], "stderr");
        assert_eq!(binary["bytes"], 2);
        assert_eq!(
            binary["lossy"], true,
            "a consumer must be told the text is not the bytes: {binary}"
        );
    }

    /// A gap reports `to` as the resume point, and an exit carries every terminal field.
    ///
    /// `to` rather than a length, for the reason in the function's docs: a length is one addition
    /// away from an off-by-one in the only arithmetic a resuming consumer does.
    #[test]
    fn a_gap_names_its_resume_point_and_an_exit_carries_the_terminal_fields() {
        let gap = event_to_json(&ExecEvent::Gap { from: 10, to: 4096 });
        assert_eq!(gap["event"], "gap");
        assert_eq!(gap["from"], 10);
        assert_eq!(gap["to"], 4096);

        let exit = event_to_json(&ExecEvent::Exit(microvms_core::protocol::exec::ExitEvent {
            exit_code: Some(4),
            signal: None,
            truncated: true,
            writers_may_be_alive: true,
            offset: 8192,
        }));
        assert_eq!(exit["event"], "exit");
        assert_eq!(exit["exitCode"], 4);
        assert_eq!(exit["signal"], Value::Null);
        assert_eq!(exit["truncated"], true);
        assert_eq!(exit["writersMayBeAlive"], true);
        assert_eq!(exit["offset"], 8192);
    }

    /// Every NDJSON record is one line, which is what makes "the last line is the envelope" true.
    ///
    /// A record containing a raw newline would split into two lines, and `run_rs.py`'s purity
    /// check — every line before the last parses as an event — would fail on a *correct* stream.
    /// `serde_json` escapes newlines inside strings, and this is the assertion that it does.
    #[test]
    fn an_event_record_is_always_exactly_one_line() {
        let multiline = event_to_json(&ExecEvent::Output {
            stream: microvms_core::protocol::exec::StreamKind::Stdout,
            offset: 0,
            data: b"first\nsecond\nthird\n".to_vec(),
        });
        let line = multiline.to_string();
        assert!(
            !line.contains('\n'),
            "a record with an embedded newline is two records: {line}"
        );
        // And it round-trips, so the escaping is not merely absent-newline but correct.
        let back: Value = serde_json::from_str(&line).expect("one document");
        assert_eq!(back["text"], "first\nsecond\nthird\n");
    }

    /// A running exec renders as a success with a null exit code.
    ///
    /// The design decision `--poll` exists on: polling is read-only, so "not finished" is an answer
    /// and not a failure. A CLI that exited non-zero here would make the natural shell loop
    /// (`until microvm exec --poll ...`) exit on its first iteration.
    #[test]
    fn a_running_exec_polls_as_a_success_with_no_exit_code() {
        let running = microvms_core::session::ExecResult {
            exec_id: "x-1".into(),
            phase: microvms_core::protocol::exec::Phase::Running,
            outcome: None,
        };
        let rendered = render_exec("x-1", &running);
        assert_eq!(rendered.already_reported, None, "polling is not a failure");
        assert_eq!(rendered.data["phase"], "running");
        assert_eq!(rendered.data["exitCode"], Value::Null);
        assert!(rendered.text.contains("is running"), "{}", rendered.text);
    }

    /// A finished exec that failed reports its code and earns `ERR_EXEC_FAILED`.
    #[test]
    fn a_finished_failing_exec_earns_the_exec_failed_row() {
        let failed = microvms_core::session::ExecResult {
            exec_id: "x-2".into(),
            phase: microvms_core::protocol::exec::Phase::Exited,
            outcome: Some(microvms_core::protocol::exec::Outcome {
                exit_code: Some(4),
                stdout: "partial\n".into(),
                truncated: true,
                ..microvms_core::protocol::exec::Outcome::default()
            }),
        };
        let rendered = render_exec("x-2", &failed);
        assert_eq!(rendered.already_reported, Some(Exit::ExecFailed));
        assert_eq!(rendered.data["exitCode"], 4);
        assert_eq!(rendered.data["truncated"], true);
        assert_eq!(rendered.data["phase"], "exited");
    }

    /// A signal death is not reported as an exit code, in either the payload or the text.
    ///
    /// The failure this prevents is specific: a CI caller reading `exitCode: 0` for a process the
    /// OOM killer took, or `128 + n` for one that never chose a code at all.
    #[test]
    fn a_signal_death_is_not_rendered_as_an_exit_code() {
        let killed = microvms_core::session::ExecResult {
            exec_id: "x-3".into(),
            phase: microvms_core::protocol::exec::Phase::Exited,
            outcome: Some(microvms_core::protocol::exec::Outcome {
                exit_code: None,
                signal: Some(9),
                ..microvms_core::protocol::exec::Outcome::default()
            }),
        };
        let rendered = render_exec("x-3", &killed);
        assert_eq!(rendered.data["exitCode"], Value::Null);
        assert_eq!(
            rendered.already_reported,
            Some(Exit::ExecFailed),
            "a killed process is not a success"
        );
        assert!(rendered.text.contains("signal"), "{}", rendered.text);
    }

    /// The phase spellings are the wire's, not `Debug`'s.
    ///
    /// A consumer comparing this envelope's `phase` against the daemon's own JSON must not have to
    /// know which of `Running` and `running` this CLI happened to print.
    #[test]
    fn the_phase_names_are_the_wire_spellings() {
        assert_eq!(
            phase_name(microvms_core::protocol::exec::Phase::Running),
            "running"
        );
        assert_eq!(
            phase_name(microvms_core::protocol::exec::Phase::Exited),
            "exited"
        );
        assert_eq!(
            phase_name(microvms_core::protocol::exec::Phase::Acked),
            "acked"
        );
    }

    /// The stdin chunk size is under the daemon's default per-write cap.
    ///
    /// Pinned because the consequence of getting it wrong is a 413 on a body this CLI chose the
    /// size of — a failure the caller cannot fix and did not cause.
    ///
    /// A `const` block, so this is a **compile** error rather than a test failure. Clippy asked
    /// for it (`assertions_on_constants`) and clippy is right here: both sides are compile-time
    /// constants, so a runtime assertion is a check that could in principle not be run, and a
    /// bound on a constant is exactly the thing a compiler should refuse to build. The upper
    /// bound is the daemon's cap; the lower is so a megabyte of input is a handful of round trips
    /// rather than thousands — a chunk of a few kilobytes would make `--stdin < big.json`
    /// pathologically slow over a proxy.
    #[test]
    fn the_stdin_chunk_stays_under_the_daemons_write_cap() {
        const {
            assert!(
                STDIN_CHUNK_BYTES < 1024 * 1024,
                "the daemon's max_stdin_write_bytes defaults to 1 MiB and answers 413 above it"
            );
            assert!(
                STDIN_CHUNK_BYTES >= 64 * 1024,
                "too small a chunk makes a large stdin pathologically slow over the proxy"
            );
        }
    }
}
