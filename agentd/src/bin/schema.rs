//! Regenerates `docs/schema.json` from the daemon's own types.
//!
//! `cargo run -p agentd --bin schema` writes the file; `--check` compares without
//! writing and exits non-zero when the committed copy is stale. CI runs the check,
//! which is the only thing standing between a generated artifact and the failure
//! mode that makes generated docs worse than none: a document that describes a
//! version of the protocol nobody serves any more. A reviewer cannot see that in a
//! diff, because the diff is the absence of a change.
//!
//! The document is generated against `Config::default()` rather than
//! `Config::from_env()`. The committed artifact has to be a function of the source
//! alone, or the check fails on whichever machine happens to have `AGENTD_PORT`
//! exported. `GET /v1/schema` on a live daemon serves the running config's real
//! numbers; this file is the defaults, which is what a reader of the repo wants.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use agentd::{Config, routes, schema};

fn main() -> ExitCode {
    let check_only = std::env::args().any(|arg| arg == "--check");
    let target = artifact_path();

    let document = schema::document(&Config::default(), &routes::surface_docs());
    // Pretty-printed with a trailing newline: this file is committed and reviewed,
    // and a one-line JSON blob makes every change a whole-file diff. The trailing
    // newline is what keeps `git diff` from reporting "\ No newline at end of file"
    // forever.
    let mut rendered = match serde_json::to_string_pretty(&document) {
        Ok(rendered) => rendered,
        Err(err) => {
            eprintln!("cannot serialize the schema document: {err}");
            return ExitCode::FAILURE;
        }
    };
    rendered.push('\n');

    if check_only {
        return check(&target, &rendered);
    }

    match std::fs::write(&target, &rendered) {
        Ok(()) => {
            println!("wrote {} ({} bytes)", target.display(), rendered.len());
            ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!("cannot write {}: {err}", target.display());
            ExitCode::FAILURE
        }
    }
}

/// Compares byte for byte, and says how to fix a mismatch.
///
/// Byte comparison rather than parsed-JSON equality on purpose: the artifact's
/// formatting is part of what is committed, and a reviewer diffing it wants the
/// file to be exactly what the generator produces. A semantic comparison would pass
/// on a file somebody hand-reformatted, and the next regeneration would then land a
/// diff nobody asked for.
fn check(target: &Path, expected: &str) -> ExitCode {
    let found = match std::fs::read_to_string(target) {
        Ok(found) => found,
        Err(err) => {
            eprintln!(
                "cannot read {}: {err}\n\
                 regenerate it with: cargo run -p agentd --bin schema",
                target.display()
            );
            return ExitCode::FAILURE;
        }
    };

    if found == expected {
        println!("{} is up to date", target.display());
        return ExitCode::SUCCESS;
    }

    eprintln!(
        "{} is stale: the daemon's types no longer match the committed schema.\n\
         Regenerate it with: cargo run -p agentd --bin schema\n\
         committed {} bytes, generated {} bytes",
        target.display(),
        found.len(),
        expected.len(),
    );
    ExitCode::FAILURE
}

/// Resolves `docs/schema.json` relative to this crate rather than to the process's
/// working directory, so the binary produces the same file whether it is run from
/// the workspace root, from `agentd/`, or by a test harness.
pub fn artifact_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("the agentd crate has a workspace parent")
        .join("docs/schema.json")
}
