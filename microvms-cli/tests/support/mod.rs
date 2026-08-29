// SPDX-License-Identifier: Apache-2.0
//! Spawning the real binary, which is the only way an integration test can reach this crate.
//!
//! # Why a spawned child rather than a function call
//!
//! Two reasons, and the first is not a choice. This crate has no lib target — that absence is
//! ARCH-5's witness — so there is nothing for an integration test to link against.
//!
//! The second is that these particular assertions are *about the process*. `ExitCode`
//! deliberately hides its raw value: it has no `Eq`, no `Hash`, and no getter, so a test that
//! called `main()` could not compare the answer to a number (research-cli.yaml source [4] says so
//! explicitly). The only place an exit code is observable is `ExitStatus::code()` on a child. And
//! "exactly one JSON document on stdout" is a claim about a real file descriptor with progress
//! interleaved on the other one, which an in-process test with captured buffers cannot make.

use std::path::PathBuf;
use std::process::{Command, Output};

/// The `microvm` binary cargo built for this test run.
///
/// `CARGO_BIN_EXE_<name>` is set by cargo for every binary in the package under test, so the path
/// is exact rather than searched for — no `target/debug` guessing, and no risk of running a stale
/// binary from a previous profile.
pub fn binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_microvm"))
}

/// What one invocation produced.
pub struct Run {
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

impl Run {
    /// The stdout parsed as exactly one JSON document.
    ///
    /// The parse itself is the assertion CLI-4 is: any stray write — a `println!` of a progress
    /// line, a second envelope — makes this fail with trailing characters rather than producing a
    /// value.
    pub fn envelope(&self) -> serde_json::Value {
        serde_json::from_str(&self.stdout).unwrap_or_else(|error| {
            panic!(
                "stdout was not exactly one JSON document ({error}).\n--- stdout ---\n{}\n--- \
                 stderr ---\n{}",
                self.stdout, self.stderr
            )
        })
    }

    /// The exit code, which must exist — a signal death is a failure of the test, not a result.
    pub fn exit_code(&self) -> i32 {
        self.code
            .unwrap_or_else(|| panic!("the child died to a signal.\nstderr: {}", self.stderr))
    }
}

/// Runs `microvm` with `args` and a **cleared** environment.
///
/// Cleared rather than inherited, and this is the difference between a deterministic test and one
/// that passes on a laptop with `AWS_PROFILE` set and fails in CI. `AWS_REGION`,
/// `MICROVM_BUCKET`, and the three role variables all change what these commands do; the ones a
/// test wants are passed in `env` explicitly.
///
/// `HOME` is set to a nonexistent path by default so a stray ledger write cannot land in the real
/// `~/.microvm/runs` — an integration test that littered a developer's home directory would be
/// its own small incident.
pub fn run(args: &[&str], env: &[(&str, &str)]) -> Run {
    let mut command = Command::new(binary());
    command.args(args);
    command.env_clear();
    command.env("HOME", "/nonexistent-microvm-test-home");
    // A terminal-independent answer: the child's stdout is a pipe either way, so
    // `IsTerminal::is_terminal` is false and the plain path is what runs. That is what makes the
    // "piped invocation produces deterministic text" assertions mean something.
    for (key, value) in env {
        command.env(key, value);
    }
    let Output {
        status,
        stdout,
        stderr,
    } = command
        .output()
        .expect("the microvm binary built by cargo is runnable");
    Run {
        code: status.code(),
        stdout: String::from_utf8_lossy(&stdout).to_string(),
        stderr: String::from_utf8_lossy(&stderr).to_string(),
    }
}

/// A temporary directory that cleans itself up.
///
/// To be replaced by the `tempfile` dev-dependency in the dependency sweep, with the
/// crate's other copies of this pattern.
pub struct TempDir(pub PathBuf, #[allow(dead_code)] tempfile::TempDir);

impl TempDir {
    pub fn new(label: &str) -> Self {
        let dir = tempfile::Builder::new()
            .prefix(&format!("microvm-it-{label}-"))
            .tempdir()
            .expect("a temp dir");
        Self(dir.path().to_path_buf(), dir)
    }

    pub fn path(&self) -> &str {
        self.0.to_str().expect("utf8 temp path")
    }
}
