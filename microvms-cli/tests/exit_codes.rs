//! **CLI-3 and CLI-4 at the process boundary**: the exit code a shell reads, and the single
//! document on stdout.
//!
//! The classification half of the exit catalogue is tested in-crate (`src/guards.rs`), where a
//! failure can be *induced* at the seam. What only a spawned child can answer is whether the
//! process really exits with those numbers, because `ExitCode` hides its value in-process — and
//! whether stdout carries exactly one JSON document with progress interleaved on stderr, which is
//! a claim about real file descriptors.

mod support;

use support::{TempDir, run};

/// **The table, at the process boundary.** Every row an invocation can reach without AWS, each
/// asserting the integer, the code string, and the finding together.
///
/// The rows an invocation cannot reach without an account — `ERR_RETRYABLE`, `ERR_CREDENTIALS`
/// against a 401, `ERR_BUILD_WEDGED`, `ERR_LAUNCH_DIED`, `ERR_WINDOW_CLOSED`, `ERR_PROTOCOL`,
/// `ERR_TIMEOUT` — are covered by `src/guards.rs`'s table, which induces each at the seam. Split
/// that way because the two halves answer different questions: that one asks whether the
/// classification is right, and this one asks whether the number survives into `$?`.
///
/// **Falsification** — return `ExitCode::SUCCESS` from `main`'s failure path and every row here
/// goes red on the integer while the envelope's `exitCode` field still reads correctly. That
/// divergence is exactly what this test exists to catch, and it is invisible to any in-process
/// check.
#[test]
fn every_locally_reachable_row_exits_with_its_own_integer_and_code() {
    let ledgers = TempDir::new("exit-rows");
    // (label, argv, expected integer, expected code, expected finding)
    let rows: [(&str, Vec<&str>, i32, &str, &str); 6] = [
        (
            "success",
            vec!["ls", "--state-dir", ledgers.path(), "--json"],
            0,
            "",
            "",
        ),
        (
            "an unknown command",
            vec!["nope", "--json"],
            2,
            "ERR_INVALID_ARG",
            "",
        ),
        (
            "an off-table size class",
            vec!["cost", "--memory", "1500", "--json"],
            2,
            "ERR_INVALID_ARG",
            "",
        ),
        (
            "a region that carries no MicroVMs",
            vec!["logs", "img", "--region", "eu-central-1", "--json"],
            2,
            "ERR_INVALID_ARG",
            "",
        ),
        (
            "a missing prerequisite",
            vec!["logs", "img", "--json"],
            12,
            "ERR_PRECONDITION",
            "",
        ),
        (
            "a binary that does not exist",
            vec![
                "build",
                "/definitely/not/here/agentd",
                "--build-role-arn",
                "arn:aws:iam::123456789012:role/build",
                "--artifact-uri",
                "s3://bucket/img.zip",
                "--json",
            ],
            12,
            "ERR_PRECONDITION",
            "",
        ),
    ];

    for (label, argv, expected_code, expected_err, finding) in rows {
        let outcome = run(&argv, &[]);
        assert_eq!(
            outcome.exit_code(),
            expected_code,
            "{label}: wrong exit code.\nstdout: {}\nstderr: {}",
            outcome.stdout,
            outcome.stderr
        );
        let envelope = outcome.envelope();
        if expected_code == 0 {
            assert_eq!(envelope["status"], "ok", "{label}");
            continue;
        }
        assert_eq!(envelope["status"], "error", "{label}");
        assert_eq!(envelope["code"], expected_err, "{label}: {envelope}");
        // The envelope's own `exitCode` and the process's must agree, or a consumer reading one
        // and a shell reading the other draw different conclusions from one failure.
        assert_eq!(
            envelope["exitCode"], expected_code,
            "{label}: the envelope and $? disagree"
        );
        assert_eq!(envelope["finding"], finding, "{label}");
        assert!(
            envelope["error"].as_str().is_some_and(|s| !s.is_empty()),
            "{label}: no message"
        );
    }
}

/// The success and failure integers are distinct from clap's own conventions.
///
/// clap exits 2 for a usage error by convention, and this catalogue's `ERR_INVALID_ARG` is
/// deliberately also 2 — so a caller who reads `$?` gets the same number whether the parse failed
/// or a handler rejected an argument. Worth pinning because the alternative was tempting: a
/// distinct code for "clap said no" would have split one remedy across two numbers.
#[test]
fn a_parse_failure_and_an_argument_rejection_share_the_argument_error_code() {
    let parse = run(&["--not-a-flag", "--json"], &[]);
    assert_eq!(parse.exit_code(), 2);
    assert_eq!(parse.envelope()["code"], "ERR_INVALID_ARG");

    let rejected = run(&["cost", "--memory", "999", "--json"], &[]);
    assert_eq!(rejected.exit_code(), 2);
    assert_eq!(rejected.envelope()["code"], "ERR_INVALID_ARG");
}

/// **CLI-4's guard.** A failure with progress enabled leaves exactly one JSON document on stdout.
///
/// Progress is *deliberately* not suppressed here — the invocation is not `--quiet` — so the
/// stderr side has real content and the stdout side has to be clean anyway. Any stray `println!`
/// makes `envelope()` fail with trailing characters.
///
/// # The command choice is constrained by a core defect, and that is recorded rather than hidden
///
/// The natural driver is a command that fails at the credential chain, which would exercise the
/// classified `ERR_CREDENTIALS` path. It cannot be used: `microvms-core`'s `aws-config` is pinned
/// with `default-features = false`, so `ControlPlane::new` **panics** ("a http_client is
/// required") before returning any error at all — see the packet's §7 gap note, which reproduces
/// it against `aws-config` alone. A test written against that path would assert on a panic.
///
/// So the driver is `run` against a nonexistent binary: it writes its progress line and then fails
/// at a precondition, which keeps both halves of the property under test — a real write on stderr
/// beside exactly one document on stdout — without depending on a code path that is currently
/// unreachable. Restore the credential version once core's manifest is fixed.
///
/// **Falsification** — change one `ctx.out.progress(...)` in `commands::lifecycle` to a
/// `println!` and this goes red on the parse. Verified; see the packet's guard proofs.
#[test]
fn a_failure_with_progress_enabled_writes_one_json_document_on_stdout() {
    let ledgers = TempDir::new("one-doc");
    let outcome = run(
        &[
            "run",
            "/definitely/not/here/agentd",
            "--artifact-uri",
            "s3://bucket/img.zip",
            "--build-role-arn",
            "arn:aws:iam::123456789012:role/build",
            "--execution-role-arn",
            "arn:aws:iam::123456789012:role/execution",
            "--state-dir",
            ledgers.path(),
            "--name",
            "img",
            "--json",
        ],
        &[],
    );
    let envelope = outcome.envelope();
    assert_eq!(envelope["status"], "error", "{envelope}");
    assert_eq!(outcome.exit_code(), 12);
    assert_eq!(envelope["code"], "ERR_PRECONDITION");
    // stdout is the envelope and nothing else.
    assert!(
        outcome.stdout.trim_start().starts_with('{'),
        "stdout: {}",
        outcome.stdout
    );
    // And the progress line really was emitted, on the other stream — otherwise this test would
    // pass against a CLI that printed no progress at all, which is not the property under test.
    assert!(
        outcome.stderr.contains("preparing img in us-east-1"),
        "the progress line must exist for this to mean anything.\nstderr: {}",
        outcome.stderr
    );
}

/// A success with progress enabled is also exactly one document.
///
/// The other half: a guard that only checked the failure path would miss a `run` that printed its
/// cost table to stdout, which is the most plausible version of this mistake.
#[test]
fn a_success_with_progress_enabled_writes_one_json_document_on_stdout() {
    let outcome = run(
        &["cost", "--running-sec", "3600", "--compare", "--json"],
        &[],
    );
    assert_eq!(outcome.exit_code(), 0);
    let envelope = outcome.envelope();
    assert_eq!(envelope["status"], "ok");
    assert_eq!(envelope["type"], "microvm.cost");
    assert!(envelope["data"]["report"]["items"].is_array());
}

/// `--quiet` silences progress and does not change the envelope.
///
/// Both halves in one pair of invocations, because a `--quiet` that also suppressed the envelope
/// would pass a test that only asserted the progress line was gone.
///
/// The *warning* half of this rule — that `--quiet` cannot buy silence about a leak or a stale rate
/// table — is asserted in-process at `src/envelope.rs`'s `quiet_silences_progress_but_never_a_leak_warning`
/// and end-to-end by `src/guards.rs`'s interrupt test, which reads the real leak warning off stderr.
/// It is not asserted here for the reason the test above records: the commands that emit a leak
/// warning all reach `ControlPlane::new` first, and that currently panics (packet §7).
#[test]
fn quiet_silences_progress_without_changing_the_envelope() {
    let ledgers = TempDir::new("quiet");
    let argv = |quiet: bool| {
        let mut argv = vec![
            "run",
            "/definitely/not/here/agentd",
            "--artifact-uri",
            "s3://bucket/img.zip",
            "--build-role-arn",
            "arn:aws:iam::123456789012:role/build",
            "--execution-role-arn",
            "arn:aws:iam::123456789012:role/execution",
            "--state-dir",
            ledgers.path(),
            "--name",
            "img",
            "--json",
        ];
        if quiet {
            argv.push("--quiet");
        }
        argv
    };

    let loud = run(&argv(false), &[]);
    assert!(
        loud.stderr.contains("preparing img"),
        "stderr: {}",
        loud.stderr
    );

    let quiet = run(&argv(true), &[]);
    assert!(
        !quiet.stderr.contains("preparing img"),
        "--quiet must silence progress: {}",
        quiet.stderr
    );
    // The envelope and the code are identical: `--quiet` is about stderr and nothing else.
    assert_eq!(quiet.envelope(), loud.envelope());
    assert_eq!(quiet.exit_code(), loud.exit_code());
}

/// `--help` and `--version` are successes that print themselves.
///
/// Not envelopes, deliberately: they are clap's own output and a consumer that asked for help
/// wants help. The exit code is what matters — a help screen that exited 2 would fail a CI step
/// that runs `--help` as a smoke test.
#[test]
fn help_and_version_exit_zero() {
    for argv in [vec!["--help"], vec!["--version"], vec!["run", "--help"]] {
        let outcome = run(&argv, &[]);
        assert_eq!(
            outcome.exit_code(),
            0,
            "{argv:?} must exit 0.\nstderr: {}",
            outcome.stderr
        );
        assert!(!outcome.stdout.is_empty(), "{argv:?} printed nothing");
    }
}

/// A parse failure under `--json` is still an envelope.
///
/// This is what the raw-token scan in `main` exists for: the parse never reaches a handler, so the
/// format has to be read off the argv before it. An agent that asked for JSON and got clap's
/// coloured usage block would have nothing to branch on.
#[test]
fn a_parse_failure_still_honours_the_requested_format() {
    let json = run(&["--json", "not-a-command"], &[]);
    assert_eq!(json.exit_code(), 2);
    let envelope = json.envelope();
    assert_eq!(envelope["code"], "ERR_INVALID_ARG");
    assert!(
        envelope["suggestions"]
            .as_array()
            .expect("an array")
            .iter()
            .any(|hint| hint.as_str().is_some_and(|s| s.contains("manifest"))),
        "{envelope}"
    );

    // And without `--json` the same failure is human text rather than a document.
    let plain = run(&["not-a-command"], &[]);
    assert_eq!(plain.exit_code(), 2);
    assert!(
        plain.stdout.contains("error ERR_INVALID_ARG"),
        "{}",
        plain.stdout
    );
    assert!(
        serde_json::from_str::<serde_json::Value>(&plain.stdout).is_err(),
        "the plain path must not emit a document"
    );
}

/// **The TTY rule.** A piped invocation produces plain deterministic text, twice identically.
///
/// The test process's stdout is always a pipe, so this is the `Format::Plain` path by
/// construction — which is the point: the assertion is that a piped `ls` and a piped `cost` are
/// byte-stable and free of terminal escapes, because a caller downstream of a pipe is parsing.
#[test]
fn a_piped_invocation_produces_plain_deterministic_text() {
    let ledgers = TempDir::new("piped");
    for argv in [
        vec!["ls", "--state-dir", ledgers.path()],
        vec!["cost", "--running-sec", "3600"],
        vec!["cost", "--running-sec", "3600", "--dense"],
    ] {
        let first = run(&argv, &[]);
        let second = run(&argv, &[]);
        assert_eq!(first.exit_code(), 0, "{argv:?}: {}", first.stderr);
        assert_eq!(
            first.stdout, second.stdout,
            "{argv:?} is not byte-stable across two runs"
        );
        // No escape sequences, which is what a ratatui frame into a pipe would look like.
        assert!(
            !first.stdout.contains('\u{1b}'),
            "{argv:?} wrote an escape sequence into a pipe: {:?}",
            first.stdout
        );
        assert!(!first.stdout.is_empty(), "{argv:?} wrote nothing");
    }
}

/// `ls` with nothing outstanding says so, in words, on the plain path.
#[test]
fn a_piped_ls_with_an_empty_ledger_says_nothing_outstanding() {
    let ledgers = TempDir::new("empty-ls");
    let outcome = run(&["ls", "--state-dir", ledgers.path()], &[]);
    assert_eq!(outcome.exit_code(), 0);
    assert_eq!(outcome.stdout.trim(), "nothing outstanding");
}

/// The dense cost path is TSV a shell can cut, and never a dollar figure where a line is unpriced.
///
/// Three fields per line and no total row, through the real binary — the same contract
/// `render::report_dense`'s unit test pins, asserted here across the process boundary because
/// this is the shape a shell pipeline actually receives. The oracle for these exact arguments:
///
/// ```text
/// cd clients/python && uv run --with boto3 python -c "
/// import io, contextlib
/// from microvms_agentd.cli import dispatch
/// buf = io.StringIO()
/// with contextlib.redirect_stdout(buf):
///     dispatch(['cost','--running-sec','3600','--build-sec','600','--image-gb','2','--dense'])
/// print(repr(buf.getvalue()))"
/// # 'image-build\tseconds\tunpriced\n...\nresume\tGB\t0.003100\n'   # seven lines, no total
/// ```
///
/// This test used to require a trailing `lower-bound` row, which is why the divergence
/// survived: the guard asserted the wrong shape confidently. The total is still reachable —
/// `--json`'s `total.render` and the plain (non-dense) rendering both carry it.
#[test]
fn the_dense_cost_path_is_cuttable_and_marks_unpriced_lines() {
    let outcome = run(
        &[
            "cost",
            "--running-sec",
            "3600",
            "--build-sec",
            "600",
            "--image-gb",
            "2",
            "--dense",
        ],
        &[],
    );
    assert_eq!(outcome.exit_code(), 0);
    let build_line = outcome
        .stdout
        .lines()
        .find(|line| line.starts_with("image-build"))
        .unwrap_or_else(|| panic!("no build line in {}", outcome.stdout));
    let fields: Vec<&str> = build_line.split('\t').collect();
    assert_eq!(
        fields,
        ["image-build", "seconds", "unpriced"],
        "phase, unit, amount — and a summable zero must never appear in the third: {build_line}"
    );
    // Every line is an item, so field one is always a phase and the line count is the item
    // count. A `total` row here would be a phase called `total` to anything aggregating.
    let lines: Vec<&str> = outcome.stdout.lines().collect();
    assert_eq!(lines.len(), 7, "{}", outcome.stdout);
    for line in &lines {
        assert_eq!(
            line.split('\t').count(),
            3,
            "{line:?} in {}",
            outcome.stdout
        );
    }
    assert!(
        !outcome.stdout.contains("lower-bound") && !outcome.stdout.contains("total\t"),
        "the total belongs to --json and the plain rendering: {}",
        outcome.stdout
    );
}
