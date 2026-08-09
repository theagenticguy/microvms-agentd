//! The exit-code contract, and the one function that maps a failure onto it.
//!
//! # Append-only, because a consumer branches on these
//!
//! Fourteen rows, 0 through 13, byte-identical to the deleted Python client's
//! `Exit`/`Code`/`EXIT_TABLE` trio (see git history). Split by *what the caller should do next*, which is the
//! only distinction worth a separate integer: `ERR_RETRYABLE` means run it again unchanged,
//! `ERR_CREDENTIALS` means fix an identity and no amount of waiting helps. The three
//! platform codes are separate because each names a different `docs/PLATFORM.md` finding
//! with a different remedy, and collapsing them would send someone to re-read the wrong
//! section.
//!
//! # `classify` is a match, and that is the port's one real upgrade here
//!
//! `cli.py:297`'s `classify` is an ordered chain of `isinstance` tests, and its docstring
//! says "order is the contract": `Unauthorized` had to be tested *before* the generic
//! retryable branch because it is an `HttpError` whose remedy is a credential rather than a
//! wait, and `AgentdError.retryable` had to be tested before the status split. Retrying a
//! 401 forever and failing a launch 200 ms from ready are the two mistakes that ordering
//! existed to prevent.
//!
//! None of that survives into Rust, because the judgement already happened.
//! [`microvms_core::ErrorKind`] is a closed thirteen-variant enum and
//! [`microvms_core::Error::kind`] is exactly the answer the chain computed —
//! `microvms-core/src/error.rs:365` documents the same reduction on its own side. So
//! [`classify`] is a `match` on a closed enum, and a match has no order to get wrong.
//!
//! The second thing that disappears is worse and more interesting.
//! `cli.py:281`'s `_TRAP_SIGNATURES` matched *message substrings* — `"clientToken replay
//! signature"`, `"before RUNNING"`, `"suspendedDurationSeconds"` — because `sandbox.py`
//! raised a bare `RuntimeError` for all three traps and the CLI's contract is that they are
//! different failures. Its own comment calls this "a seam, not a preference", and names the
//! right eventual shape: a distinct type per trap, in the library. That is what landed —
//! `ErrorKind::BuildWedged`, `ErrorKind::LaunchDied`, `ErrorKind::WindowClosed` — so the
//! table is gone and with it the failure mode where a library message reworded for clarity
//! silently collapses two exit codes into one.
//!
//! # The coarse code is not the whole story, which is why the envelope carries both
//!
//! Four [`microvms_core::WireKind`]s collapse onto `ERR_PROTOCOL`, deliberately: a shell
//! branching on `$?` cannot act differently on a 400 than on a 409. A consumer that *can*
//! reads `data.kind`, which [`CliError`] carries from
//! [`microvms_core::Error::wire_kind`] — see [`crate::envelope`].

use std::fmt;

use microvms_core::{Error, ErrorKind, WireKind};

/// One row of the exit-code table.
///
/// `code` is `None` only for row 0: success has no `ERR_*` string because a success
/// envelope has no `code` field to put one in.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ExitRow {
    pub exit: Exit,
    pub code: Option<&'static str>,
    /// What the caller should do next, which is what the integer is *for*.
    pub meaning: &'static str,
    /// The `docs/PLATFORM.md` section that measured this, or `""`.
    ///
    /// In the table rather than only in a message because it is the field that turns a
    /// failure into a lookup: an agent reading `finding: "`idlePolicy`"` can go read the
    /// measurement instead of guessing at a retry policy.
    pub finding: &'static str,
}

/// The exit-code contract. Append-only.
///
/// `#[repr(u8)]` with explicit discriminants so the integer a shell sees is written down
/// beside the name rather than inferred from declaration order — a variant inserted in the
/// middle of a plain enum silently renumbers every one after it, which for this type means
/// silently rewriting the contract.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
#[repr(u8)]
pub enum Exit {
    Ok = 0,
    /// An error no arm claimed — a bug in this CLI, not the platform.
    ///
    /// Distinct from every handled class on purpose: a bug here reported as a platform
    /// failure would send the reader to AWS.
    Unexpected = 1,
    InvalidArg = 2,
    Retryable = 3,
    Credentials = 4,
    Protocol = 5,
    BuildWedged = 6,
    LaunchDied = 7,
    WindowClosed = 8,
    Platform = 9,
    Timeout = 10,
    Interrupted = 11,
    Precondition = 12,
    /// The sandbox worked and the caller's command exited non-zero.
    ///
    /// Its own code because it is the one non-zero exit that means nothing is wrong with the
    /// platform, the credentials, or this CLI — a CI caller needs to tell "your tests
    /// failed" from "we never got a VM", and one shared code cannot say both.
    ExecFailed = 13,
}

impl Exit {
    /// The integer a shell reads from `$?`.
    ///
    /// `as u8` on the discriminant rather than a second match, so the number here and the
    /// number in the enum cannot disagree.
    pub fn as_u8(self) -> u8 {
        self as u8
    }

    /// This exit's row.
    ///
    /// Infallible by construction: the table below is indexed by the discriminant and a
    /// test pins that correspondence, so there is no "unknown exit code" case for a caller
    /// to handle.
    pub fn row(self) -> &'static ExitRow {
        &EXIT_TABLE[self.as_u8() as usize]
    }

    /// The `ERR_*` string, or `None` for success.
    pub fn code(self) -> Option<&'static str> {
        self.row().code
    }

    /// The `docs/PLATFORM.md` finding, or `""`.
    pub fn finding(self) -> &'static str {
        self.row().finding
    }

    /// The row a core failure class earns.
    ///
    /// The whole of the CLI's classification of a library failure, and it is total by
    /// construction: [`ErrorKind`] is closed, so a fourteenth kind added to core is a
    /// non-exhaustive match here rather than a silent fall-through to `ERR_UNEXPECTED`.
    /// That is the property `cli.py`'s `isinstance` chain could not have — its final
    /// `return CliError(Exit.UNEXPECTED, ...)` catches a new library exception class and
    /// reports it as a bug in the CLI.
    pub fn for_kind(kind: ErrorKind) -> Exit {
        match kind {
            ErrorKind::Unexpected => Exit::Unexpected,
            ErrorKind::InvalidArg => Exit::InvalidArg,
            ErrorKind::Retryable => Exit::Retryable,
            ErrorKind::Credentials => Exit::Credentials,
            ErrorKind::Protocol => Exit::Protocol,
            ErrorKind::BuildWedged => Exit::BuildWedged,
            ErrorKind::LaunchDied => Exit::LaunchDied,
            ErrorKind::WindowClosed => Exit::WindowClosed,
            ErrorKind::Platform => Exit::Platform,
            ErrorKind::Timeout => Exit::Timeout,
            ErrorKind::Interrupted => Exit::Interrupted,
            ErrorKind::Precondition => Exit::Precondition,
            ErrorKind::ExecFailed => Exit::ExecFailed,
        }
    }
}

impl fmt::Display for Exit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.as_u8())
    }
}

/// The fourteen rows, in exit-code order, so the rendered table reads like the contract it
/// is.
///
/// Meanings and findings transcribed from `cli.py:149`'s `EXIT_TABLE`, which is what
/// `microvm manifest` publishes and what the conformance oracle compares against. Written
/// out rather than generated from [`ErrorKind::code`], because a generated table would
/// agree with a typo — the same reason `microvms-core/src/error.rs:432` spells its thirteen
/// codes by hand.
pub const EXIT_TABLE: [ExitRow; 14] = [
    ExitRow {
        exit: Exit::Ok,
        code: None,
        meaning: "the command did what it said",
        finding: "",
    },
    ExitRow {
        exit: Exit::Unexpected,
        code: Some("ERR_UNEXPECTED"),
        meaning: "an exception no handler claimed — a bug in this CLI, not the platform",
        finding: "",
    },
    ExitRow {
        exit: Exit::InvalidArg,
        code: Some("ERR_INVALID_ARG"),
        meaning: "the request was refused locally, before any AWS call",
        finding: "",
    },
    ExitRow {
        exit: Exit::Retryable,
        code: Some("ERR_RETRYABLE"),
        meaning: "a transient condition; run the identical command again",
        finding: "Endpoint authentication",
    },
    ExitRow {
        exit: Exit::Credentials,
        code: Some("ERR_CREDENTIALS"),
        meaning: "an identity is wrong or absent; waiting will not fix it",
        finding: "",
    },
    ExitRow {
        exit: Exit::Protocol,
        code: Some("ERR_PROTOCOL"),
        meaning: "the daemon rejected the request on its merits",
        finding: "",
    },
    ExitRow {
        exit: Exit::BuildWedged,
        code: Some("ERR_BUILD_WEDGED"),
        meaning: "the image build was never scheduled — the clientToken replay signature",
        finding: "`clientToken` is a permanent idempotency key",
    },
    ExitRow {
        exit: Exit::LaunchDied,
        code: Some("ERR_LAUNCH_DIED"),
        meaning: "the MicroVM reached a terminal state before RUNNING; read stateReason",
        finding: "`runHookPayload` arrives wrapped, not as the body",
    },
    ExitRow {
        exit: Exit::WindowClosed,
        code: Some("ERR_WINDOW_CLOSED"),
        meaning: "the launch-time suspended window passed, so there is nothing to resume",
        finding: "`idlePolicy`",
    },
    ExitRow {
        exit: Exit::Platform,
        code: Some("ERR_PLATFORM"),
        meaning: "a control-plane failure with no more specific class",
        finding: "",
    },
    ExitRow {
        exit: Exit::Timeout,
        code: Some("ERR_TIMEOUT"),
        meaning: "a client-side deadline elapsed; the VM and the exec are untouched",
        finding: "",
    },
    ExitRow {
        exit: Exit::Interrupted,
        code: Some("ERR_INTERRUPTED"),
        meaning: "interrupted after launch; teardown ran and any leak is named in the payload",
        finding: "The build log group survives Terraform",
    },
    ExitRow {
        exit: Exit::Precondition,
        code: Some("ERR_PRECONDITION"),
        meaning: "a prerequisite is missing — run `microvm doctor`",
        finding: "",
    },
    ExitRow {
        exit: Exit::ExecFailed,
        code: Some("ERR_EXEC_FAILED"),
        meaning: "the sandbox worked and the command in it exited non-zero",
        finding: "",
    },
];

/// A failure already classified into the contract.
///
/// `data` survives into the envelope so a partial result is still machine readable on the
/// failure path — most importantly the identifiers a teardown could not delete (CLI-6),
/// which are worthless to an operator who cannot name them.
#[derive(Clone, Debug)]
pub struct CliError {
    pub exit: Exit,
    pub message: String,
    /// The daemon status that produced this, when one did.
    ///
    /// Carried separately from `exit` and emitted as `data.kind`, because the exit code
    /// collapses `Conflict`/`NotFound`/`ProtocolError`/`StdinClosed`/`TooLarge` onto
    /// `ERR_PROTOCOL` and the conformance oracle asserts at the finer granularity — see
    /// the module docs and `microvms-core/src/error.rs:1`.
    pub wire_kind: Option<WireKind>,
    pub suggestions: Vec<String>,
    pub data: serde_json::Map<String, serde_json::Value>,
}

impl CliError {
    /// A failure of `exit` with `message`, no suggestions and no payload.
    pub fn new(exit: Exit, message: impl Into<String>) -> Self {
        Self {
            exit,
            message: message.into(),
            wire_kind: None,
            suggestions: Vec::new(),
            data: serde_json::Map::new(),
        }
    }

    /// Adds a remedy line. Order is preserved: the first is the one most likely to help.
    #[must_use]
    pub fn suggest(mut self, suggestion: impl Into<String>) -> Self {
        self.suggestions.push(suggestion.into());
        self
    }

    /// Attaches a partial-result field to the failure envelope's `data`.
    #[must_use]
    pub fn with_data(mut self, key: &str, value: serde_json::Value) -> Self {
        self.data.insert(key.to_string(), value);
        self
    }

    /// The `ERR_*` code. Never `None`: a `CliError` of `Exit::Ok` is not constructible
    /// through any path here, and [`classify`] cannot produce one.
    pub fn code(&self) -> &'static str {
        self.exit.code().unwrap_or("ERR_UNEXPECTED")
    }

    /// The `docs/PLATFORM.md` finding for this row, or `""`.
    pub fn finding(&self) -> &'static str {
        self.exit.finding()
    }
}

/// Maps a core failure onto exactly one row, with the remedy that row's reader needs.
///
/// The exit code comes from [`Exit::for_kind`] and nothing else. What is added here is the
/// *suggestion*, which is CLI-shaped rather than library-shaped: the library says what went
/// wrong, and the CLI says which flag or command addresses it.
pub fn classify(error: &Error) -> CliError {
    let exit = Exit::for_kind(error.kind());
    let mut classified = CliError {
        exit,
        message: error.to_string(),
        wire_kind: error.wire_kind(),
        suggestions: Vec::new(),
        data: serde_json::Map::new(),
    };
    // Keyed on the *wire* kind where two conditions share an exit code and differ in
    // remedy. A 401 and an unresolvable credential chain are both `ERR_CREDENTIALS`, and
    // "check your agent token" is unhelpful for the second while "run doctor" is unhelpful
    // for the first.
    let suggestions: &[&str] = match (exit, error.wire_kind()) {
        (Exit::Credentials, Some(WireKind::Unauthorized)) => {
            &["the agent token does not match the one the run hook installed at launch"]
        }
        (Exit::Credentials, _) => &[
            "`microvm doctor` reports which credential the SDK could not resolve",
            "an AccessDeniedException with a null message is the unsupported-region \
             signature, not an IAM problem — check --region first",
        ],
        (Exit::Retryable, Some(WireKind::AuthTokenMint)) => {
            &["minting happens inside the request path, so the identical command may succeed"]
        }
        (Exit::Retryable, _) => &["a transient condition: run the identical command again"],
        (Exit::Timeout, _) => {
            &["polling is read-only, so the exec and its output are untouched and re-pollable"]
        }
        (Exit::InvalidArg, _) => &["`microvm manifest` lists every command and its option domains"],
        (Exit::Precondition, _) => &["`microvm doctor` checks every prerequisite at once"],
        (Exit::BuildWedged, _) => &[
            "the image cannot be deleted from CREATING; record its identifier and build \
             under a fresh --name",
        ],
        (Exit::LaunchDied, _) => {
            &["`microvm logs <image-name>` names the build log group the hook wrote to"]
        }
        (Exit::WindowClosed, _) => {
            &["a longer window is set at launch with --suspended-sec; no call extends this one"]
        }
        _ => &[],
    };
    classified
        .suggestions
        .extend(suggestions.iter().map(|s| (*s).to_string()));
    classified
}

impl From<Error> for CliError {
    fn from(error: Error) -> Self {
        classify(&error)
    }
}

/// A clap parse failure, as an argument error the caller can fix without touching AWS.
///
/// An unknown command, a misspelled option, a missing argument, a value that will not
/// coerce into one of the closed sets in [`crate::cli`]. clap's own message already carries
/// its did-you-mean line, so it is forwarded verbatim rather than restated.
pub fn from_parse_error(error: &clap::Error) -> CliError {
    CliError::new(Exit::InvalidArg, error.render().to_string().trim_end())
        .suggest("`microvm manifest` lists every command and its option domains")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **CLI-3, the table-driven guard over every row.** All fourteen, each asserting the
    /// integer, the `ERR_*` string, and the `docs/PLATFORM.md` finding together.
    ///
    /// All three per row is what makes collapsing impossible. A CLI that mapped every
    /// failure to one code satisfies "it exited non-zero" and fails on the integer; one
    /// that kept the integers and shared a code string fails on the code; one that got both
    /// right and dropped the finding fails on the third — and the finding is the field that
    /// sends a reader to the measurement rather than to a guess.
    ///
    /// **Falsification** — collapse `Exit::BuildWedged` and `Exit::LaunchDied` onto one row
    /// in `EXIT_TABLE` and this is red on both the code and the finding. Verified; see the
    /// packet's guard proofs.
    #[test]
    fn every_row_carries_its_integer_its_code_and_its_finding() {
        let expected: [(u8, Option<&str>, &str); 14] = [
            (0, None, ""),
            (1, Some("ERR_UNEXPECTED"), ""),
            (2, Some("ERR_INVALID_ARG"), ""),
            (3, Some("ERR_RETRYABLE"), "Endpoint authentication"),
            (4, Some("ERR_CREDENTIALS"), ""),
            (5, Some("ERR_PROTOCOL"), ""),
            (
                6,
                Some("ERR_BUILD_WEDGED"),
                "`clientToken` is a permanent idempotency key",
            ),
            (
                7,
                Some("ERR_LAUNCH_DIED"),
                "`runHookPayload` arrives wrapped, not as the body",
            ),
            (8, Some("ERR_WINDOW_CLOSED"), "`idlePolicy`"),
            (9, Some("ERR_PLATFORM"), ""),
            (10, Some("ERR_TIMEOUT"), ""),
            (
                11,
                Some("ERR_INTERRUPTED"),
                "The build log group survives Terraform",
            ),
            (12, Some("ERR_PRECONDITION"), ""),
            (13, Some("ERR_EXEC_FAILED"), ""),
        ];
        assert_eq!(EXIT_TABLE.len(), expected.len());
        for (row, (integer, code, finding)) in EXIT_TABLE.iter().zip(expected) {
            assert_eq!(row.exit.as_u8(), integer, "{row:?}");
            assert_eq!(row.code, code, "{row:?}");
            assert_eq!(row.finding, finding, "{row:?}");
            assert!(
                !row.meaning.is_empty(),
                "every row states a remedy: {row:?}"
            );
        }
    }

    /// The table is indexed by the discriminant, so `Exit::row` is infallible.
    ///
    /// A row out of order would make every lookup return a neighbour's code — a failure
    /// that produces plausible output and is invisible to the test above, which reads the
    /// table directly rather than through `row()`.
    #[test]
    fn the_table_is_indexed_by_the_exit_integer() {
        for (index, row) in EXIT_TABLE.iter().enumerate() {
            assert_eq!(row.exit.as_u8() as usize, index);
            assert_eq!(row.exit.row(), row, "row() must return this row");
        }
    }

    /// Thirteen distinct non-zero codes for thirteen classes.
    ///
    /// Two rows sharing a code makes the failure envelope ambiguous, which is the one thing
    /// the string beside the integer exists to prevent.
    #[test]
    fn no_two_rows_share_a_code_or_an_integer() {
        let mut codes: Vec<&str> = EXIT_TABLE.iter().filter_map(|row| row.code).collect();
        assert_eq!(codes.len(), 13, "only row 0 has no code");
        codes.sort_unstable();
        let before = codes.len();
        codes.dedup();
        assert_eq!(codes.len(), before, "duplicate ERR_* code");

        let mut integers: Vec<u8> = EXIT_TABLE.iter().map(|row| row.exit.as_u8()).collect();
        integers.sort_unstable();
        integers.dedup();
        assert_eq!(integers.len(), 14, "duplicate exit integer");
    }

    /// The CLI's table and core's taxonomy are the same thirteen classes, and the code
    /// strings agree byte for byte.
    ///
    /// The load-bearing cross-check. Two tables that must agree are one table that will
    /// not: core's `ErrorKind::code` is what a binding reports and this table is what a
    /// shell branches on, so a disagreement means one consumer is told `ERR_TIMEOUT` and
    /// the other `ERR_PLATFORM` about the same failure.
    #[test]
    fn the_exit_table_and_cores_error_kinds_are_the_same_thirteen_classes() {
        let from_core: Vec<&str> = ErrorKind::ALL
            .iter()
            .map(|kind| Exit::for_kind(*kind).code().expect("no kind maps to row 0"))
            .collect();
        let from_table: Vec<&str> = EXIT_TABLE.iter().filter_map(|row| row.code).collect();
        assert_eq!(from_core, from_table);

        // And each kind's own code string is the row's, so neither side is merely
        // consistently wrong.
        for kind in ErrorKind::ALL {
            assert_eq!(
                Exit::for_kind(kind).code(),
                Some(kind.code()),
                "{kind} disagrees with its row"
            );
        }
    }

    /// No two core kinds map onto one exit row.
    ///
    /// This is the assertion that fails if a future edit "simplifies" the mapping — say by
    /// routing `ErrorKind::Precondition` to `Exit::InvalidArg` because both mean "the
    /// caller's fault". They do not: one is fixed by editing a flag and the other by
    /// applying a Terraform stack.
    #[test]
    fn the_kind_to_exit_mapping_is_injective() {
        let mut exits: Vec<u8> = ErrorKind::ALL
            .iter()
            .map(|kind| Exit::for_kind(*kind).as_u8())
            .collect();
        exits.sort_unstable();
        let before = exits.len();
        exits.dedup();
        assert_eq!(
            exits.len(),
            before,
            "two kinds collapsed onto one exit code"
        );
    }

    /// Every `WireKind` reaches a row, and the five that collapse onto `ERR_PROTOCOL` are
    /// exactly the five core says collapse.
    ///
    /// The collapse is deliberate and the envelope's `data.kind` is what preserves the
    /// distinction, so what this pins is that the collapse is *the intended one* rather
    /// than an accident that grew.
    #[test]
    fn the_five_protocol_wire_kinds_collapse_and_the_others_do_not() {
        let mut collapsed: Vec<&str> = WireKind::ALL
            .iter()
            .filter(|wire| Exit::for_kind(wire.error_kind()) == Exit::Protocol)
            .map(|wire| wire.as_str())
            .collect();
        collapsed.sort_unstable();
        assert_eq!(
            collapsed,
            [
                "Conflict",
                "NotFound",
                "ProtocolError",
                "StdinClosed",
                "TooLarge"
            ]
        );
        // The one the Python got wrong by `isinstance` ordering, corrected in core.
        assert_eq!(
            Exit::for_kind(WireKind::ExecTimeout.error_kind()),
            Exit::Timeout
        );
        assert_eq!(
            Exit::for_kind(WireKind::Unauthorized.error_kind()),
            Exit::Credentials
        );
    }

    /// A classified failure carries its wire kind through, so the envelope can report the
    /// oracle's granularity.
    #[test]
    fn classify_carries_the_wire_kind_for_a_daemon_failure_and_none_for_a_local_reject() {
        let conflict = classify(&Error::wire(WireKind::Conflict, "409 wrong state"));
        assert_eq!(conflict.exit, Exit::Protocol);
        assert_eq!(conflict.code(), "ERR_PROTOCOL");
        assert_eq!(conflict.wire_kind, Some(WireKind::Conflict));

        let local = classify(&Error::invalid_arg("refused before any AWS call"));
        assert_eq!(local.exit, Exit::InvalidArg);
        assert_eq!(local.wire_kind, None, "nothing reached the daemon");
        assert!(
            local
                .suggestions
                .iter()
                .any(|hint| hint.contains("microvm manifest")),
            "{local:?}"
        );
    }

    /// Two failures sharing `ERR_CREDENTIALS` get different remedies.
    ///
    /// The suggestion is the only part of a failure envelope that is the CLI's own
    /// judgement rather than the library's, and it is worth having only if it is specific:
    /// "check your agent token" against an unresolvable credential chain is worse than
    /// nothing, because it sends the reader to the one thing that is fine.
    #[test]
    fn the_two_credential_failures_do_not_share_a_remedy() {
        let token = classify(&Error::wire(WireKind::Unauthorized, "401"));
        let chain = classify(&Error::new(ErrorKind::Credentials, "no provider resolved"));
        assert_eq!(token.exit, chain.exit);
        assert_ne!(token.suggestions, chain.suggestions);
        assert!(token.suggestions[0].contains("agent token"), "{token:?}");
        assert!(
            chain.suggestions.iter().any(|s| s.contains("doctor")),
            "{chain:?}"
        );
        assert!(
            chain.suggestions.iter().any(|s| s.contains("null message")),
            "the region trap has to be named on the path that reaches it: {chain:?}"
        );
    }

    /// A parse failure is an argument error carrying clap's own message.
    #[test]
    fn a_parse_failure_is_an_argument_error_carrying_claps_message() {
        let error = clap::Error::raw(clap::error::ErrorKind::UnknownArgument, "unexpected --nope");
        let classified = from_parse_error(&error);
        assert_eq!(classified.exit, Exit::InvalidArg);
        assert_eq!(classified.code(), "ERR_INVALID_ARG");
        assert!(classified.message.contains("--nope"), "{classified:?}");
    }
}
