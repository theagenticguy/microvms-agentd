// SPDX-License-Identifier: Apache-2.0
//! **CLI-2's static half.** The CLI reaches AWS through `microvms-core` and through nothing else,
//! asserted from the manifest and from the source.
//!
//! # Two checks, and the first is the strong one
//!
//! An **exact dependency set** read out of `cargo metadata`, and a source scan. The set is what
//! matters: a denylist of forbidden crate names is defeated by the one crate nobody thought to
//! write down, and `test_cli.py`'s three-legged static guard grew a leg at a time as each was
//! defeated on purpose. An equality against a written-down set has no such gap — every addition to
//! this crate's manifest is a diff against a test that names the six things allowed and says why.
//!
//! The scan catches what a manifest cannot: a control-plane operation invoked through a crate that
//! *is* allowed. `microvms-core` is allowed and re-exports plenty; a handler that reached past the
//! seam into `ControlPlane::new` would add no dependency at all.
//!
//! # The scan matches code, not prose, and that took three attempts
//!
//! `test_cli.py:269` records writing this check the naive way first, as a substring search — it
//! "went red immediately, on a comment explaining *why* a region check is local". A guard that
//! fires on its own documentation gets deleted, and then the other half is alone. That lesson cost
//! this file two rounds of its own:
//!
//! 1. **Comments.** Stripped, per the Python's lesson.
//! 2. **String literals.** Not stripped at first, and the guard went red on
//!    `commands/lifecycle.rs:526` — an *error message* explaining that `CreateMicrovmImage` names an
//!    artifact which must already be in S3. That is the single most useful sentence in the file and
//!    the guard wanted it deleted, which is precisely the failure mode the Python names. Now
//!    stripped too: a control-plane call is an *identifier* in code, never a word in a message.
//! 3. **Test regions.** Skipped, because `src/guards.rs` scripts a fake control plane and names
//!    `RunMicrovm` on purpose.
//!
//! What survives all three is the thing actually worth forbidding: a `RunMicrovm` that is a token
//! the compiler resolves. A CLI cannot invoke an operation without naming it that way, and it
//! cannot be prevented from *explaining* one.
//!
//! (cli.py line numbers resolve at `git show 'c4d396e^:clients/python/src/microvms_agentd/cli.py'` — the retired oracle.)

use std::path::{Path, PathBuf};

/// One name this crate deliberately does **not** depend on, and the API that replaced it.
///
/// Not on the forbidden list below, because it never was a capability — `futures-util` opens no
/// socket and knows nothing about AWS, so it could not have been the second path to the control
/// plane CLI-2 is about. It is here for a different reason: it was an allowed dependency, with a
/// justification, for exactly as long as core's only stream API was one returning a trait `std`
/// does not define. `ExecHandle::for_each_event` takes a `FnMut(ExecEvent) -> ControlFlow<()>`
/// instead, so this crate consumes a stream with nothing but `microvms-core`.
///
/// Recorded as a named absence rather than left to the equality alone so a future contributor who
/// reaches for `StreamExt` finds the alternative in the failure message instead of a bare "the
/// dependency set changed".
const RETIRED: [(&str, &str); 1] = [(
    "futures-util",
    "the `Stream` trait `ExecHandle::stream_with` returns. Retired: core's \
     `ExecHandle::for_each_event(options, |event| ControlFlow::Continue(()))` drives the same \
     state machine through a std callback, so there is no trait to name. See \
     `commands/attached.rs`'s `stream_exec`",
)];

/// The exact set of direct dependencies this crate is allowed, and the reason for each.
///
/// An allowlist. See the module docs on why a denylist is not good enough. The reason strings are
/// not decorative: this test asserts each is a real sentence, so a new entry cannot be added
/// without someone writing down what it is for.
const ALLOWED: [(&str, &str); 6] = [
    (
        "microvms-core",
        "the product surface: every AWS call, every trap closure, the cost engine, the taxonomy — \
         and the wire types, via its `pub use protocol` re-export, so one door covers everything",
    ),
    (
        "clap",
        "the command tree, which is the manifest's only source and the CLI-5 closed sets",
    ),
    (
        "ratatui",
        "the interactive surface CLI-1 requires, drawn only when stdout is a terminal",
    ),
    (
        "serde",
        "the envelope's derives, and the ledger's camelCase wire shape that `microvm ls` reads",
    ),
    (
        "serde_json",
        "the envelope itself, and the constants object the drift gate reads (TRAP-12)",
    ),
    (
        "tokio",
        "the runtime this crate is entitled to choose, and ctrl_c for CLI-6",
    ),
];

/// Crate names that would mean a second path to AWS or to HTTP.
///
/// Belt beside the braces: the equality below already forbids these, and naming them makes the
/// *intent* legible in a failure message. A reviewer reading a diff that added `reqwest` sees why
/// rather than only that a count changed.
const FORBIDDEN: [&str; 12] = [
    "reqwest",
    "hyper",
    "hyper-util",
    "http",
    "aws-config",
    "aws-sdk-s3",
    "aws-sdk-sts",
    "aws-sigv4",
    "aws-credential-types",
    "aws-smithy-runtime",
    "rusoto_core",
    "ureq",
];

/// This crate's package, out of `cargo metadata`.
fn package() -> cargo_metadata::Package {
    let metadata = cargo_metadata::MetadataCommand::new()
        .manifest_path(manifest_path())
        // The whole workspace, because the dependency-direction test in the sibling file needs the
        // other members and building the graph twice is the slow part.
        .exec()
        .expect("cargo metadata runs");
    metadata
        .packages
        .into_iter()
        .find(|package| package.name.as_str() == "microvms-cli")
        .expect("microvms-cli is a workspace member")
}

fn manifest_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml")
}

/// **The exact dependency set.** Six normal dependencies plus the one dev-dependency, and
/// nothing else — the ALLOWED table above is the count's source of truth, not this sentence.
///
/// The requirement under test, stated as an equality rather than as an absence. `cargo metadata`
/// is the source rather than the TOML text, because it resolves path dependencies and workspace
/// inheritance — an edge added through a renamed key or a `[target.'cfg(...)']` table is still an
/// edge this sees, and a hand-parsed manifest would check the file instead of the build.
///
/// **Falsification** — add `reqwest = "0.13"` to `microvms-cli/Cargo.toml` and this goes red
/// naming it, on both the equality and the forbidden list. Verified; see the packet's guard
/// proofs.
#[test]
fn the_direct_dependency_set_is_exactly_the_allowed_one() {
    let package = package();
    let mut actual: Vec<String> = package
        .dependencies
        .iter()
        .filter(|dependency| {
            matches!(
                dependency.kind,
                cargo_metadata::DependencyKind::Normal
                    | cargo_metadata::DependencyKind::Development
            )
        })
        .map(|dependency| dependency.name.clone())
        .collect();
    actual.sort();
    actual.dedup();

    let mut allowed: Vec<String> = ALLOWED
        .iter()
        .map(|(name, _)| (*name).to_string())
        // The one dev-dependency, which reads this very graph.
        .chain(std::iter::once("cargo_metadata".to_string()))
        .collect();
    allowed.sort();

    assert_eq!(
        actual,
        allowed,
        "the CLI's dependency set changed. Added: {:?}. Removed: {:?}. CLI-2 says this crate \
         reaches AWS through microvms-core and nothing else, so a new dependency needs a line in \
         ALLOWED saying what it is for — a real paragraph, on the terms CLI-2 is about: can this \
         crate open a socket or sign a request with it? If it is an HTTP or AWS crate, it needs a \
         different design instead.",
        actual
            .iter()
            .filter(|name| !allowed.contains(name))
            .collect::<Vec<_>>(),
        allowed
            .iter()
            .filter(|name| !actual.contains(name))
            .collect::<Vec<_>>(),
    );

    // And explicitly none of the ones that would mean a second path to AWS, so a failure message
    // says *why* rather than only that a set changed.
    for forbidden in FORBIDDEN {
        assert!(
            !actual.iter().any(|name| name == forbidden),
            "{forbidden} is a direct dependency of the CLI, which gives it a second path to AWS \
             or to HTTP — the requirement CLI-2 is"
        );
    }

    // And explicitly none of the ones that came out again. A separate loop from FORBIDDEN because
    // the message is different: a retired dependency has a *replacement*, and naming it is what
    // stops the same edge being re-added by someone who reached for `StreamExt` and found the
    // guard rather than the API.
    for (name, replacement) in RETIRED {
        assert!(
            !actual.iter().any(|entry| entry == name),
            "{name} is a direct dependency again. It was allowed once and came out for a reason \
             — {replacement}. If the replacement genuinely does not cover the case, move the \
             entry into ALLOWED with a paragraph saying which case, rather than restoring the \
             old one."
        );
    }

    // Every allowance states its purpose, so a new one cannot be added silently.
    for (name, reason) in ALLOWED {
        assert!(
            reason.len() > 25,
            "{name}'s allowance needs a real reason, not {reason:?}"
        );
    }
}

/// Names that would mean this crate is talking to a service itself.
///
/// The control-plane operation names come from the service model; the constructor names are core's
/// own doors that bypass the seam. Both matter for the same reason: neither adds a dependency, so
/// the manifest check above cannot see them.
const FORBIDDEN_IDENTIFIERS: [&str; 14] = [
    // Control-plane operations. A CLI that named one has grown a second implementation.
    "CreateMicrovmImage",
    "RunMicrovm",
    "GetMicrovm",
    "SuspendMicrovm",
    "ResumeMicrovm",
    "TerminateMicrovm",
    "CreateMicrovmAuthToken",
    "CreateMicrovmShellAuthToken",
    "DeleteMicrovmImage",
    // Core's own constructors, which reach AWS without the seam. Allowed in `seam.rs` and nowhere
    // else — that file *is* the seam.
    "ControlPlane::new",
    "Sandbox::new",
    "SignedTransport",
    "Session::direct",
    "Session::connect",
];

/// The files the scan covers, with their contents cut at the test region and stripped of comments.
///
/// A file whose *first* item is an inner `#![cfg(test)]` is skipped entirely: it does not ship, and
/// `src/guards.rs` is exactly that — it scripts a fake control plane, so it names `RunMicrovm` and
/// `TerminateMicrovm` on purpose. Scanning it would make the guard unsatisfiable for any crate that
/// also has a behavioral guard, which is the wrong trade.
///
/// The skip is deliberately keyed on the **inner** attribute (`#![cfg(test)]`, whole file) rather
/// than the outer one (`#[cfg(test)]`, next item), because those are different claims and only the
/// first means "none of this ships".
fn scannable_sources() -> Vec<(PathBuf, String)> {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files: Vec<PathBuf> = Vec::new();
    collect_rust_files(&root, &mut files);
    assert!(
        files.len() >= 10,
        "the scan found almost nothing: {files:?}"
    );
    let scanned: Vec<(PathBuf, String)> = files
        .into_iter()
        .filter_map(|path| {
            let text = std::fs::read_to_string(&path).expect("a readable source file");
            if is_whole_file_test_module(&text) {
                return None;
            }
            Some((path, production_region(&text)))
        })
        .collect();
    assert!(
        scanned.len() >= 10,
        "the skip rule excluded too much; only {} files are scanned",
        scanned.len()
    );
    scanned
}

/// Whether the `#[cfg(test)]` at `index` opens an inline test **module** rather than gating a
/// single item.
///
/// Only a `mod` opens a region. A gated `fn`, `struct`, or `mod x;` declaration does not, and
/// treating one as a cut point is how this test's own first draft was going to exclude eight
/// hundred lines of handler from the scan — the guard reported it, which is the whole point of the
/// `regions <= 1` assertion existing beside the scan rather than being assumed.
fn opens_a_test_region(lines: &[&str], index: usize, line: &str) -> bool {
    if line.trim_start() != "#[cfg(test)]" || line.starts_with(' ') {
        return false;
    }
    lines
        .get(index + 1)
        .map(|next| next.trim_start())
        .is_some_and(|next| next.starts_with("mod ") && next.ends_with('{'))
}

/// Whether the whole file is test-only, by an inner `#![cfg(test)]` attribute.
fn is_whole_file_test_module(text: &str) -> bool {
    text.lines().any(|line| line.trim() == "#![cfg(test)]")
}

/// Every `.rs` file under `dir`, recursively.
fn collect_rust_files(dir: &Path, into: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.filter_map(Result::ok) {
        let path = entry.path();
        if path.is_dir() {
            collect_rust_files(&path, into);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            into.push(path);
        }
    }
}

/// The part of a source file that ships, with `//` comments removed.
///
/// Two transformations, and both were learned rather than designed. See the module docs: the
/// comment strip is `test_cli.py:269`'s lesson, and the test-region cut exists because
/// `src/guards.rs` legitimately scripts a fake control plane and therefore names `Transport`,
/// `Call`, and `Reply`.
///
/// The cut is at the *first* `#[cfg(test)]` at column zero, and a separate assertion below pins
/// that each file has at most one — otherwise a file with an inline `#[cfg(test)]` helper near the
/// top would have almost all of its production code excluded from the scan, silently.
fn production_region(text: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();
    // The first column-zero `#[cfg(test)]` that introduces an inline test *region* — not one that
    // merely gates a `mod x;` declaration.
    //
    // The distinction is load-bearing and it is the second defect this file's own guard found in
    // it: `main.rs` gates `mod guards;` with `#[cfg(test)]` at line 35, so a naive cut there
    // excluded four hundred lines of dispatcher from the scan and reported a clean pass over the
    // module declarations. A gated `mod x;` does not begin a test region — the gated file is its
    // own file, and is either scanned or skipped on its own merits.
    let cut = lines
        .iter()
        .enumerate()
        .find(|(index, line)| opens_a_test_region(&lines, *index, line))
        .map(|(index, _)| index)
        .unwrap_or(usize::MAX);
    // One pass over the whole text, tracking whether we are inside a string literal — which is the
    // only version of this that works, and it took three wrong ones to establish that.
    //
    // Stripping comments line by line first is wrong, because `"s3://{bucket}/{name}.zip"` at
    // `commands/lifecycle.rs:522` contains a `//`: cutting there truncated the literal, left the
    // opening quote unmatched, and desynchronised the quote state for every line after it — which
    // is what finally reported `CreateMicrovmImage` four lines later inside a message it had already
    // stopped recognising as a message.
    //
    // Stripping literals line by line is wrong too, because a literal continued with a trailing
    // `\` spans lines and several of this crate's error messages do.
    //
    // So: one scanner, both jobs, whole file.
    blank_comments_and_literals(&lines[..cut.min(lines.len())].join("\n"))
}

/// Blanks the contents of every string literal and every `//` comment, in one pass.
///
/// One pass because the two jobs are not separable: `//` inside a literal is not a comment, and a
/// quote inside a comment is not a literal delimiter. Doing either first desynchronises the other,
/// which is what the caller's comment records happening twice.
///
/// # Why literals are blanked at all
///
/// An error message that *explains* `CreateMicrovmImage` — that the artifact must be in S3 before
/// it, and that the service's rejection would arrive after the upload — is the most useful sentence
/// on that code path. A guard that demanded its deletion is a guard someone deletes instead, which
/// is `test_cli.py:269`'s lesson exactly. What CLI-2 forbids is *invoking* an operation, and an
/// invocation is an identifier the compiler resolves. No string literal can be one.
///
/// Deliberately crude about the rest: it does not understand raw strings, escaped quotes, or char
/// literals. That is safe because it only has to be conservative in the right direction —
/// over-blanking loses nothing, since a real identifier is never inside quotes, and under-blanking
/// would at worst leave a false positive this test then *reports* rather than hides. Newlines
/// survive so a failure can still name a line.
fn blank_comments_and_literals(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    let mut in_string = false;
    let mut in_comment = false;
    while let Some(character) = chars.next() {
        match character {
            '\n' => {
                in_comment = false;
                out.push('\n');
            }
            _ if in_comment => out.push(' '),
            '"' => {
                in_string = !in_string;
                out.push('"');
            }
            '/' if !in_string && chars.peek() == Some(&'/') => {
                in_comment = true;
                out.push(' ');
            }
            // A backslash inside a literal consumes what follows, so an escaped quote does not
            // close the string. Without this, `\"` flips the state and everything after it reads
            // as code — the same desynchronisation from the other direction.
            '\\' if in_string => {
                out.push(' ');
                if chars.next().is_some() {
                    out.push(' ');
                }
            }
            other => out.push(if in_string { ' ' } else { other }),
        }
    }
    out
}

/// **The source scan.** No shipping line names a control-plane operation or a core constructor
/// that bypasses the seam.
///
/// `seam.rs` is the one exception and it is the point: that file *is* the door, so it is where
/// `ControlPlane::new` belongs. Every other file reaching for it would be a handler that grew its
/// own path to AWS — which adds no dependency and is therefore invisible to the manifest check.
///
/// **Falsification** — replace the `ctx.seam.control_plane(region)` call in
/// `commands/lifecycle.rs`'s `suspend` with `ControlPlane::new(region)` and this goes red naming
/// the file and the identifier. Verified; see the packet's guard proofs.
#[test]
fn no_shipping_source_line_names_an_operation_or_reaches_past_the_seam() {
    for (path, source) in scannable_sources() {
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        for identifier in FORBIDDEN_IDENTIFIERS {
            // `seam.rs` holds the three constructors on purpose. It does *not* get an exemption
            // for the operation names — a seam that named an operation would be one that had
            // started implementing the protocol.
            let is_the_seam = name == "seam.rs" && identifier.contains("::");
            if is_the_seam {
                continue;
            }
            assert!(
                !source.contains(identifier),
                "{name} names {identifier} in shipping code. Every AWS call belongs to \
                 microvms-core and every construction goes through src/seam.rs; a CLI that names \
                 one has grown a second path to the control plane (CLI-2)."
            );
        }
    }
}

/// Each shipping file opens at most one inline test region, so the scan's cut cannot hide code.
///
/// Without this the scan is quietly defeatable: a second `#[cfg(test)] mod` placed near the top of
/// a file would exclude everything after it, and the guard would report a clean pass over three
/// lines of a four-hundred-line module. The `kept` ratio below is the same worry from the other
/// side — it fails if the one region starts so early that most of the file is outside the scan.
#[test]
fn the_scan_cut_cannot_hide_production_code() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    collect_rust_files(&root, &mut files);
    for path in files {
        let text = std::fs::read_to_string(&path).expect("readable");
        if is_whole_file_test_module(&text) {
            continue;
        }
        let lines: Vec<&str> = text.lines().collect();
        let regions = lines
            .iter()
            .enumerate()
            .filter(|(index, line)| opens_a_test_region(&lines, *index, line))
            .count();
        assert!(
            regions <= 1,
            "{} opens {regions} inline test regions; the thinness scan cuts at the first, so a \
             second would exclude shipping code from the scan without saying so. Put every \
             test-only helper inside the one `mod tests`.",
            path.display(),
        );
        // And the scan really did keep most of the file, so the cut is not silently swallowing it.
        let kept = production_region(&text).lines().count();
        let total = lines.len();
        if regions == 1 && total > 50 {
            assert!(
                kept * 4 > total,
                "{} keeps only {kept} of {total} lines in the scanned region — the cut is in the \
                 wrong place",
                path.display(),
            );
        }
    }
}

/// The one stdout write in this crate lives in `envelope.rs`, plus the two named exceptions.
///
/// CLI-4's structural half: "exactly one envelope on stdout" is only enforceable if there is one
/// place that writes to stdout. The two exceptions are `main.rs` — which prints clap's own help and
/// the bare constants object, both documented at their call sites — and nothing else.
///
/// A guard on the *shape* of the code rather than on its behaviour, which is what makes it worth
/// having beside the behavioural check in `exit_codes.rs`: that one catches a stray write that a
/// test happens to exercise, and this one catches one that no test does.
#[test]
fn only_the_envelope_module_and_mains_two_exceptions_write_to_stdout() {
    for (path, source) in scannable_sources() {
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        if name == "envelope.rs" || name == "main.rs" {
            continue;
        }
        for macro_name in ["println!", "print!"] {
            assert!(
                !source.contains(macro_name),
                "{name} writes to stdout with {macro_name}. Progress goes to stderr through \
                 Output::progress and the envelope is written once by the dispatcher — a stray \
                 stdout write passes an 'is the envelope there' check and breaks the parse \
                 (CLI-4)."
            );
        }
    }

    // And `main.rs` has exactly the two documented ones, so the exemption cannot grow.
    let main =
        std::fs::read_to_string(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/main.rs"))
            .expect("readable");
    let writes = production_region(&main).matches("print").count();
    assert!(
        writes <= 3,
        "main.rs has grown extra stdout writes ({writes} `print` occurrences); the only two are \
         clap's own help output and `constants --emit-json`, both documented at their call sites"
    );
}
