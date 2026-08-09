// SPDX-License-Identifier: Apache-2.0
//! One error type, two granularities.
//!
//! # Why two
//!
//! The Python client made this split by accident and it turned out to be
//! load-bearing. `errors.py` has a class per status the daemon deliberately
//! chooses — `Conflict`, `NotFound`, `ProtocolError` — and `cli.py`'s exit table
//! collapses all three onto one code, `ERR_PROTOCOL`, because a shell branching on
//! `$?` cannot usefully tell them apart and a caller who needs to has a message.
//!
//! Both are contracts, to different consumers, and neither can stand in for the
//! other. The conformance suite asserts on the exception *class*
//! (`conformance/run.py`'s `raises()` takes `Unauthorized`, `Conflict`,
//! `NotFound`, `ProtocolError`), so a Rust client that only knew the coarse code
//! would make the oracle unable to tell a 400 from a 404 — and that distinction is
//! the one the daemon's whole status discipline exists to preserve. So
//! [`Error::kind`] answers the exit-code question and [`Error::wire_kind`] answers
//! the which-status-did-the-daemon-choose question, and the CLI carries the second
//! into its JSON envelope as `data.kind` so the oracle keeps its granularity.
//!
//! # What a caller matches on
//!
//! [`ErrorKind`], nearly always. [`WireKind`] exists for the two consumers that
//! need the finer view: the conformance driver, and a caller retrying a specific
//! daemon condition. Nobody should parse a message.

use std::fmt;

/// A failure, classified once at the point it is raised.
///
/// Carries the coarse [`ErrorKind`] every consumer branches on, the optional
/// [`WireKind`] naming the daemon status that produced it, a message that names
/// the `docs/PLATFORM.md` finding when a trap closure raised it, and the
/// underlying error when one exists.
///
/// Deliberately a struct with a private body rather than an enum: the variants a
/// caller wants to match on are the *kinds*, and an enum over every raise site
/// would make every new failure a breaking change for a binding that matched
/// exhaustively.
#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub struct Error {
    kind: ErrorKind,
    wire_kind: Option<WireKind>,
    message: String,
    #[source]
    source: Option<Box<dyn std::error::Error + Send + Sync + 'static>>,
}

impl Error {
    /// A failure of `kind`, with a message that should name its finding.
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            wire_kind: None,
            message: message.into(),
            source: None,
        }
    }

    /// A local reject: the request was refused before any AWS call.
    ///
    /// The shorthand exists because every trap closure in this crate ends in one,
    /// and `ERR_INVALID_ARG` is the code that tells a caller the fix is theirs and
    /// costs nothing.
    pub fn invalid_arg(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::InvalidArg, message)
    }

    /// A failure the daemon reported by choosing a status.
    ///
    /// The [`ErrorKind`] is derived from the [`WireKind`] rather than passed in,
    /// because the two must not be able to disagree: `Unauthorized` is
    /// `ERR_CREDENTIALS` and never `ERR_PROTOCOL`, and a call site free to pick
    /// both is a call site that can get that wrong once.
    pub fn wire(wire_kind: WireKind, message: impl Into<String>) -> Self {
        Self {
            kind: wire_kind.error_kind(),
            wire_kind: Some(wire_kind),
            message: message.into(),
            source: None,
        }
    }

    /// Attaches the underlying error, reachable through [`std::error::Error::source`].
    #[must_use]
    pub fn with_source(mut self, source: impl std::error::Error + Send + Sync + 'static) -> Self {
        self.source = Some(Box::new(source));
        self
    }

    /// The coarse class, one per row of the exit-code table.
    pub fn kind(&self) -> ErrorKind {
        self.kind
    }

    /// The daemon status this came from, when it came from one.
    ///
    /// `None` for every local reject and every control-plane failure — those never
    /// reached the in-VM daemon, so there is no status to report.
    pub fn wire_kind(&self) -> Option<WireKind> {
        self.wire_kind
    }

    /// The machine-readable `ERR_*` code, as `cli.py`'s `Code` spells it.
    pub fn code(&self) -> &'static str {
        self.kind.code()
    }

    /// Whether retrying the identical request could plausibly succeed.
    ///
    /// Derived from the kind rather than stored, so it cannot drift from it: the
    /// five retryable daemon conditions all map to [`ErrorKind::Retryable`], which
    /// is what makes this one comparison instead of a second table.
    pub fn retryable(&self) -> bool {
        matches!(self.kind, ErrorKind::Retryable)
    }
}

/// The coarse failure classes, one per non-zero row of the CLI's exit table.
///
/// Exactly the thirteen codes the deleted Python client's `cli.py` defined.
/// The integer exit code lives in the CLI, not here: a library that owned process
/// exit codes would be a library with an opinion about being a process.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ErrorKind {
    /// No handler claimed this — a bug in this crate, not the platform.
    Unexpected,
    /// Refused locally, before any AWS call. Every trap closure lands here.
    InvalidArg,
    /// Transient. Run the identical request again.
    Retryable,
    /// An identity is wrong or absent; waiting will not fix it.
    Credentials,
    /// The daemon rejected the request on its merits.
    Protocol,
    /// The image build was never scheduled — the `clientToken` replay signature.
    BuildWedged,
    /// The MicroVM reached a terminal state before RUNNING; read `stateReason`.
    LaunchDied,
    /// The launch-time suspended window passed, so there is nothing to resume.
    WindowClosed,
    /// A control-plane failure with no more specific class.
    Platform,
    /// A client-side deadline elapsed. The VM and the exec are untouched.
    Timeout,
    /// Interrupted after launch; teardown ran and any leak is named in the payload.
    Interrupted,
    /// A prerequisite is missing.
    Precondition,
    /// The sandbox worked and the command in it exited non-zero.
    ///
    /// Its own class because it is the one failure that means nothing is wrong with
    /// the platform, the credentials, or this client — a CI caller needs to tell
    /// "your tests failed" from "we never got a VM", and one shared class cannot say
    /// both.
    ExecFailed,
}

impl ErrorKind {
    /// Every kind, in exit-code order.
    ///
    /// Public so a binding can enumerate the catalog and a test can assert the
    /// mapping is total rather than sampling it.
    pub const ALL: [ErrorKind; 13] = [
        ErrorKind::Unexpected,
        ErrorKind::InvalidArg,
        ErrorKind::Retryable,
        ErrorKind::Credentials,
        ErrorKind::Protocol,
        ErrorKind::BuildWedged,
        ErrorKind::LaunchDied,
        ErrorKind::WindowClosed,
        ErrorKind::Platform,
        ErrorKind::Timeout,
        ErrorKind::Interrupted,
        ErrorKind::Precondition,
        ErrorKind::ExecFailed,
    ];

    /// The `ERR_*` string, byte-identical to `cli.py`'s `Code` member.
    ///
    /// A string beside the eventual integer because the two are read by different
    /// consumers: a shell branches on `$?`, an agent parsing `--json` branches on
    /// `code` and should never have to keep an integer table.
    pub fn code(self) -> &'static str {
        match self {
            ErrorKind::Unexpected => "ERR_UNEXPECTED",
            ErrorKind::InvalidArg => "ERR_INVALID_ARG",
            ErrorKind::Retryable => "ERR_RETRYABLE",
            ErrorKind::Credentials => "ERR_CREDENTIALS",
            ErrorKind::Protocol => "ERR_PROTOCOL",
            ErrorKind::BuildWedged => "ERR_BUILD_WEDGED",
            ErrorKind::LaunchDied => "ERR_LAUNCH_DIED",
            ErrorKind::WindowClosed => "ERR_WINDOW_CLOSED",
            ErrorKind::Platform => "ERR_PLATFORM",
            ErrorKind::Timeout => "ERR_TIMEOUT",
            ErrorKind::Interrupted => "ERR_INTERRUPTED",
            ErrorKind::Precondition => "ERR_PRECONDITION",
            ErrorKind::ExecFailed => "ERR_EXEC_FAILED",
        }
    }
}

impl fmt::Display for ErrorKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.code())
    }
}

/// The fine-grained taxonomy: one variant per condition the daemon or the
/// transport distinguishes.
///
/// These are the `errors.py` exception *classes*, which is what the conformance
/// suite asserts on. Several collapse onto one [`ErrorKind`] and that is the
/// point — the collapse happens at the exit code, not at the raise site.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum WireKind {
    /// 401: the presented bearer token is not the installed one. Fatal.
    Unauthorized,
    /// 400: malformed body, missing query key, bad mode, refused tar member.
    ///
    /// Never 404. A 404 here reads as a missing file, which is exactly how one
    /// defect hid for a review round.
    ProtocolError,
    /// 404: a genuinely absent exec id, file, or directory.
    NotFound,
    /// 409: well-formed, but the target is in the wrong state.
    Conflict,
    /// 410: stdin already saw EOF, or the child stopped reading.
    ///
    /// Distinct from [`WireKind::Conflict`], which is "you did not ask for stdin"
    /// and is fixed at start time. This one is a lifecycle fact.
    StdinClosed,
    /// 413: over a configured cap (body, tar members or bytes, stdin write).
    TooLarge,
    /// 408: the child did not drain its stdin pipe within the write timeout.
    ///
    /// Retryable, and the daemon deliberately keeps its stdin handle open so a
    /// retry can succeed. Some bytes may already have landed; reconciling that is
    /// the caller's problem, which is why it is a distinct variant.
    RequestTimeout,
    /// 503: the run hook has not landed, so the control API is closed.
    ///
    /// Not 404 and not a dropped connection. Retry: the platform is about to
    /// deliver the token.
    NotBootstrapped,
    /// 5xx other than 503: spawn failure, io failure, a panicking task.
    ServerError,
    /// The request never produced a status: connection refused, reset, timeout.
    ///
    /// Retryable because it says nothing about the daemon's state. A VM that has
    /// just reached RUNNING commonly refuses a connection or two before the proxy
    /// path is wired up.
    Transport,
    /// `CreateMicrovmAuthToken` failed.
    ///
    /// Retryable, and load-bearing rather than optimistic: proxy tokens expire at
    /// 60 minutes, so a long run *will* mint mid-flight, and a throttle at that
    /// moment must not kill a trial that is otherwise healthy.
    AuthTokenMint,
    /// A client-side wait or stream deadline elapsed. The exec is untouched.
    ExecTimeout,
    /// Output bytes are gone for good — the replay ring evicted them, or this
    /// subscriber lagged the live channel.
    OutputGap,
}

impl WireKind {
    /// Every variant, so a test can assert the mapping is total.
    pub const ALL: [WireKind; 13] = [
        WireKind::Unauthorized,
        WireKind::ProtocolError,
        WireKind::NotFound,
        WireKind::Conflict,
        WireKind::StdinClosed,
        WireKind::TooLarge,
        WireKind::RequestTimeout,
        WireKind::NotBootstrapped,
        WireKind::ServerError,
        WireKind::Transport,
        WireKind::AuthTokenMint,
        WireKind::ExecTimeout,
        WireKind::OutputGap,
    ];

    /// The stable name the CLI puts in `data.kind`, spelled as the Python class.
    ///
    /// The conformance oracle compares against these strings, so they are the
    /// exception class names from `errors.py` and not a re-spelling of them.
    pub fn as_str(self) -> &'static str {
        match self {
            WireKind::Unauthorized => "Unauthorized",
            WireKind::ProtocolError => "ProtocolError",
            WireKind::NotFound => "NotFound",
            WireKind::Conflict => "Conflict",
            WireKind::StdinClosed => "StdinClosed",
            WireKind::TooLarge => "TooLarge",
            WireKind::RequestTimeout => "RequestTimeout",
            WireKind::NotBootstrapped => "NotBootstrapped",
            WireKind::ServerError => "ServerError",
            WireKind::Transport => "Transport",
            WireKind::AuthTokenMint => "AuthTokenMint",
            WireKind::ExecTimeout => "ExecTimeout",
            WireKind::OutputGap => "OutputGap",
        }
    }

    /// The status that produces this variant, for the nine that come from one.
    ///
    /// `None` for the four that have no status: two never got a response
    /// ([`WireKind::Transport`], [`WireKind::AuthTokenMint`]) and two are client-side
    /// facts ([`WireKind::ExecTimeout`], [`WireKind::OutputGap`]).
    pub fn status(self) -> Option<u16> {
        match self {
            WireKind::ProtocolError => Some(400),
            WireKind::Unauthorized => Some(401),
            WireKind::NotFound => Some(404),
            WireKind::RequestTimeout => Some(408),
            WireKind::Conflict => Some(409),
            WireKind::StdinClosed => Some(410),
            WireKind::TooLarge => Some(413),
            WireKind::ServerError => Some(500),
            WireKind::NotBootstrapped => Some(503),
            WireKind::Transport
            | WireKind::AuthTokenMint
            | WireKind::ExecTimeout
            | WireKind::OutputGap => None,
        }
    }

    /// The variant a response status means, or `None` for a status the daemon does
    /// not use to mean anything.
    ///
    /// The table is explicit and there is deliberately **no generic 4xx fallback**.
    /// The daemon's whole point is that 400 and 404 mean different things, and a
    /// fallback that mapped "some 4xx" to one variant would reintroduce the
    /// phantom-missing-file defect that hid in the Python client for a review
    /// round. 5xx does fall back — to [`WireKind::ServerError`], which is what
    /// `errors.py` does — because every 5xx means the same thing to a caller:
    /// the daemon broke, try again.
    pub fn from_status(status: u16) -> Option<WireKind> {
        match status {
            400 => Some(WireKind::ProtocolError),
            401 => Some(WireKind::Unauthorized),
            404 => Some(WireKind::NotFound),
            408 => Some(WireKind::RequestTimeout),
            409 => Some(WireKind::Conflict),
            410 => Some(WireKind::StdinClosed),
            413 => Some(WireKind::TooLarge),
            503 => Some(WireKind::NotBootstrapped),
            s if s >= 500 => Some(WireKind::ServerError),
            _ => None,
        }
    }

    /// The exit-code class this collapses onto.
    ///
    /// The ordering `cli.py`'s `classify` documents as "the contract" is a `match`
    /// here rather than a sequence of `isinstance` tests, which removes the way it
    /// could be got wrong: [`WireKind::Unauthorized`] is an HTTP error whose remedy
    /// is a credential rather than a wait, and in Python it had to be *checked
    /// before* the generic retryable test to avoid being swallowed by it. A match on
    /// a closed enum has no order to get wrong.
    pub fn error_kind(self) -> ErrorKind {
        match self {
            // A credential, not a wait. Retrying a 401 forever is one of the two
            // mistakes the Python ordering existed to prevent.
            WireKind::Unauthorized => ErrorKind::Credentials,
            // The five retryable conditions from `errors.py`. Failing a launch that
            // was 200 ms from ready is the other mistake.
            WireKind::Transport
            | WireKind::AuthTokenMint
            | WireKind::NotBootstrapped
            | WireKind::RequestTimeout
            | WireKind::ServerError => ErrorKind::Retryable,
            // The daemon rejected the request on its merits. Four classes, one exit
            // code: a shell cannot act differently on them, and a caller that can
            // reads `data.kind`.
            WireKind::ProtocolError
            | WireKind::NotFound
            | WireKind::Conflict
            | WireKind::StdinClosed
            | WireKind::TooLarge => ErrorKind::Protocol,
            // A client-side deadline. `cli.py` reaches ERR_PLATFORM for this by
            // accident of `isinstance` ordering — `ExecTimeout` is an `AgentdError`
            // and is tested before the `TimeoutError` branch — but ERR_TIMEOUT's own
            // row reads "a client-side deadline elapsed; the VM and the exec are
            // untouched", which is `ExecTimeout`'s docstring. Corrected here.
            WireKind::ExecTimeout => ErrorKind::Timeout,
            // No better row exists: a gap is not the caller's argument, not the
            // daemon refusing the request, and not retryable — the bytes are gone.
            // `cli.py` reaches ERR_PLATFORM for it too, and this one is right.
            WireKind::OutputGap => ErrorKind::Platform,
        }
    }

    /// Whether retrying the identical request could plausibly succeed.
    ///
    /// The `errors.py` contract, restated as an independent table so the test below
    /// can compare it against [`Error::retryable`] rather than trusting the two
    /// agree. Test-only and deliberately not public: a second public answer to
    /// "is this retryable" is a second thing to keep in step, and the production
    /// answer must stay derived from [`WireKind::error_kind`] so a new variant
    /// cannot be retryable in one place and fatal in the other.
    #[cfg(test)]
    fn retryable(self) -> bool {
        matches!(
            self,
            WireKind::Transport
                | WireKind::AuthTokenMint
                | WireKind::NotBootstrapped
                | WireKind::RequestTimeout
                | WireKind::ServerError
        )
    }
}

impl fmt::Display for WireKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The thirteen codes, spelled as `cli.py` spells them. Written out rather
    /// than generated, because a generated list would agree with a typo.
    #[test]
    fn every_kind_carries_its_python_err_code() {
        let codes: Vec<&str> = ErrorKind::ALL.iter().map(|k| k.code()).collect();
        assert_eq!(
            codes,
            [
                "ERR_UNEXPECTED",
                "ERR_INVALID_ARG",
                "ERR_RETRYABLE",
                "ERR_CREDENTIALS",
                "ERR_PROTOCOL",
                "ERR_BUILD_WEDGED",
                "ERR_LAUNCH_DIED",
                "ERR_WINDOW_CLOSED",
                "ERR_PLATFORM",
                "ERR_TIMEOUT",
                "ERR_INTERRUPTED",
                "ERR_PRECONDITION",
                "ERR_EXEC_FAILED",
            ]
        );
    }

    /// Thirteen distinct codes for thirteen kinds. Two kinds sharing a code would
    /// make the CLI's failure envelope ambiguous, which is the one thing the string
    /// beside the integer exists to prevent.
    #[test]
    fn no_two_kinds_share_a_code() {
        let mut seen: Vec<&str> = ErrorKind::ALL.iter().map(|k| k.code()).collect();
        seen.sort_unstable();
        let before = seen.len();
        seen.dedup();
        assert_eq!(seen.len(), before, "duplicate ERR_* code");
    }

    /// `retryable()` reads the kind, and the kind is derived from the wire kind, so
    /// the only way the two can disagree is a wrong row in `error_kind`. Compared
    /// against the `errors.py` table restated independently in `WireKind::retryable`.
    #[test]
    fn retryable_agrees_with_the_python_exception_contract() {
        for wire in WireKind::ALL {
            let err = Error::wire(wire, "detail");
            assert_eq!(
                err.retryable(),
                wire.retryable(),
                "{wire} disagrees with errors.py on retryability"
            );
        }
    }

    /// Exactly the five `errors.py` marks retryable, named so a sixth added by
    /// mistake fails here rather than in a retry loop that never terminates.
    #[test]
    fn exactly_five_wire_kinds_are_retryable() {
        let retryable: Vec<&str> = WireKind::ALL
            .iter()
            .filter(|w| w.retryable())
            .map(|w| w.as_str())
            .collect();
        assert_eq!(
            retryable,
            [
                "RequestTimeout",
                "NotBootstrapped",
                "ServerError",
                "Transport",
                "AuthTokenMint"
            ]
        );
    }

    /// A 401 is a credential and never a protocol error, whatever order the
    /// classification is written in. This is the `isinstance`-ordering trap from
    /// `cli.py` made unreachable.
    #[test]
    fn unauthorized_is_a_credential_failure_not_a_protocol_one() {
        let err = Error::wire(WireKind::Unauthorized, "401");
        assert_eq!(err.kind(), ErrorKind::Credentials);
        assert_eq!(err.code(), "ERR_CREDENTIALS");
        assert!(!err.retryable());
    }

    /// The load-bearing distinction: 400 and 404 are different variants, and no
    /// generic 4xx fallback can produce either. 402 is a 4xx the daemon never
    /// chooses, and it must map to nothing at all rather than to the nearest
    /// neighbour.
    #[test]
    fn no_generic_four_hundred_fallback_can_produce_a_protocol_error() {
        assert_eq!(WireKind::from_status(400), Some(WireKind::ProtocolError));
        assert_eq!(WireKind::from_status(404), Some(WireKind::NotFound));
        for unmapped in [402, 403, 405, 418, 429, 451] {
            assert_eq!(
                WireKind::from_status(unmapped),
                None,
                "{unmapped} must not resolve to a daemon-chosen variant"
            );
        }
    }

    /// 5xx *does* fall back, because every 5xx means the same thing to a caller.
    /// 503 is the exception and keeps its own variant: "come back in a moment" is
    /// not "the daemon broke".
    #[test]
    fn five_hundreds_fall_back_to_server_error_except_the_bootstrap_one() {
        assert_eq!(WireKind::from_status(500), Some(WireKind::ServerError));
        assert_eq!(WireKind::from_status(502), Some(WireKind::ServerError));
        assert_eq!(WireKind::from_status(599), Some(WireKind::ServerError));
        assert_eq!(WireKind::from_status(503), Some(WireKind::NotBootstrapped));
    }

    /// Every status in the table round-trips: the status a variant reports is the
    /// status that produces it. Two tables that must agree are one table that will
    /// not, and this is the cheap way to keep them one.
    #[test]
    fn status_and_from_status_are_inverses_where_both_are_defined() {
        for wire in WireKind::ALL {
            if let Some(status) = wire.status() {
                assert_eq!(
                    WireKind::from_status(status),
                    Some(wire),
                    "{wire} reports {status} but {status} resolves elsewhere"
                );
            }
        }
    }

    /// A local reject has no wire kind: nothing reached the daemon, so there is no
    /// status to report and the CLI's `data.kind` must be absent rather than
    /// invented.
    #[test]
    fn a_local_reject_carries_no_wire_kind() {
        let err = Error::invalid_arg("refused before any AWS call");
        assert_eq!(err.kind(), ErrorKind::InvalidArg);
        assert_eq!(err.wire_kind(), None);
        assert_eq!(err.to_string(), "refused before any AWS call");
    }

    /// The underlying error stays reachable through `source()`, so a caller can
    /// print a chain rather than a message that already threw the cause away.
    #[test]
    fn an_attached_source_survives_into_the_error_trait() {
        let io = std::io::Error::other("connection reset");
        let err = Error::new(ErrorKind::Retryable, "endpoint request failed").with_source(io);
        let source = std::error::Error::source(&err).expect("source is reachable");
        assert_eq!(source.to_string(), "connection reset");
    }
}
