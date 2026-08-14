// SPDX-License-Identifier: Apache-2.0
//! Exec wire types: the bodies, queries, and SSE event payloads of `/v1/exec/*`.
//!
//! Every type here derives both halves of serde. The daemon needs only one half of
//! each and a client needs the other, and a type that carries one half is a type the
//! other side has to hand-write — which is the drift this crate exists to prevent.
//! The published `docs/schema.json` is generated from these same attributes under
//! both contracts, so a shape change is a schema diff rather than a surprise.

use std::borrow::Cow;
use std::collections::HashMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Where an exec sits in its lifecycle. Mirrors `ExecPhase` in the model crate.
///
/// `JsonSchema` rides along with `Serialize` on every type from here down that
/// crosses the wire. schemars reads the same `#[serde(...)]` attributes serde
/// does, so the published schema describes what the daemon actually emits — the
/// `rename_all` below is the reason this matters rather than a formality.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    /// Child spawned, still running (or its pipes still held by a grandchild).
    Running,
    /// Child exited and output is buffered and readable.
    Exited,
    /// Caller acked; output has been released and the entry awaits collection.
    Acked,
}

impl Phase {
    /// Every phase, in lifecycle order.
    ///
    /// Public because a client that publishes the closed set — both bindings do, in
    /// their `session_constants` — needs the list from the type rather than a spelled-out
    /// copy that goes stale the first time a phase is added. The round-trip test below
    /// holds `ALL` complete by exhaustive match.
    pub const ALL: [Phase; 3] = [Phase::Running, Phase::Exited, Phase::Acked];

    /// The wire spelling — the exact string serde writes under `rename_all` above.
    ///
    /// Here rather than in each client because two bindings each grew their own
    /// three-arm match over this enum; a variant renamed on the wire must change
    /// exactly one table, and the test below is what keeps this one equal to serde's.
    pub const fn as_str(self) -> &'static str {
        match self {
            Phase::Running => "running",
            Phase::Exited => "exited",
            Phase::Acked => "acked",
        }
    }
}

/// Captured output and exit status of a finished exec.
#[derive(Clone, Debug, Default, Deserialize, JsonSchema, Serialize)]
pub struct Outcome {
    /// Exit code, or `None` when the child died to a signal.
    pub exit_code: Option<i32>,
    /// Signal number that killed the child, when one did.
    pub signal: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    /// Set when either stream hit `max_output_bytes` and was cut. An explicit
    /// flag rather than a sentinel string in the output: a marker inside the
    /// bytes is indistinguishable from output that happens to contain it.
    pub truncated: bool,
    /// Set when the post-exit linger deadline expired with the pipes still open,
    /// meaning some grandchild is alive and may write more that nobody will see.
    /// Reported rather than hidden, because a harness that sees empty output from
    /// a command it knows produced some needs to be able to tell why.
    pub writers_may_be_alive: bool,
}

/// Which pipe a streamed chunk came from. Both share one offset space, so a
/// client holds one cursor rather than two that can disagree about ordering.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StreamKind {
    Stdout,
    Stderr,
}

impl StreamKind {
    /// Both kinds, in the order the daemon documents them.
    ///
    /// Same reason as [`Phase::ALL`]: the bindings publish this closed set, and a
    /// list they spell themselves is a list the enum can outgrow.
    pub const ALL: [StreamKind; 2] = [StreamKind::Stdout, StreamKind::Stderr];

    /// The wire spelling — the exact string serde writes under `rename_all` above.
    pub const fn as_str(self) -> &'static str {
        match self {
            StreamKind::Stdout => "stdout",
            StreamKind::Stderr => "stderr",
        }
    }
}

/// A start request. `command` is either an argv array or, with `shell: true`, a
/// single script string.
#[derive(Debug, Deserialize, JsonSchema, Serialize)]
pub struct StartRequest {
    /// Caller-minted idempotency key. Harbor retries, and a retry must not
    /// produce a second child.
    pub exec_id: String,
    /// argv when `shell` is false, or the script when it is true.
    pub command: Vec<String>,
    #[serde(default)]
    pub shell: bool,
    /// Omitted means inherit the daemon's working directory. See the module docs.
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub env: HashMap<String, String>,
    /// Numeric uid to demote to. Optional; omitted means run as the daemon's own
    /// user.
    #[serde(default)]
    pub user: Option<u32>,
    #[serde(default)]
    pub group: Option<u32>,
    /// Wall-clock budget. Validated before the child spawns — the predecessor
    /// raised on a bad value inside the waiter thread, by which point the child
    /// was already running and became an orphan.
    #[serde(default)]
    pub timeout_sec: Option<f64>,
    /// Whether to give the child a writable stdin pipe. Defaults to false, which
    /// keeps `Stdio::null()`.
    ///
    /// Opt-in rather than always-on, and not only for tidiness: a child holding an
    /// open stdin pipe nobody will ever write to is a child that blocks forever
    /// the first time it reads. `/bin/sh` reading a script from stdin, `git`
    /// deciding it can prompt, any tool that probes for input — all of them behave
    /// differently against a pipe than against `/dev/null`. Every existing caller
    /// gets today's behavior by not setting this.
    #[serde(default)]
    pub stdin: bool,
}

/// `POST /v1/exec/{id}/stdin` body.
///
/// Both fields optional and both meaningful together: a final chunk plus EOF in
/// one request is the common case for feeding a prompt, and forcing two round
/// trips would leave a window where the child has the bytes but not the EOF that
/// tells it the input is complete.
#[derive(Debug, Default, Deserialize, JsonSchema, Serialize)]
pub struct StdinRequest {
    /// Base64 so arbitrary bytes survive JSON. A JSON string cannot carry
    /// non-UTF-8, and stdin is bytes.
    #[serde(default)]
    pub data_b64: Option<String>,
    /// `"eof"` closes the pipe after any `data_b64` is written. Named rather than
    /// a bare boolean so the field has somewhere to grow.
    #[serde(default)]
    pub signal: Option<String>,
}

/// `POST /v1/exec/{id}/stdin` response.
///
/// `pub` rather than private, like every other type in this module the schema
/// route publishes: the generator names them by type, so a response shape that
/// stays private is a shape a consumer cannot be told about.
#[derive(Debug, Deserialize, JsonSchema, Serialize)]
pub struct StdinResponse {
    pub exec_id: String,
    pub written: usize,
    pub eof: bool,
}

/// `GET /v1/exec/{id}/stream` query.
#[derive(Debug, Default, Deserialize, JsonSchema, Serialize)]
pub struct StreamQuery {
    /// Byte offset to resume from. Absent means 0, i.e. everything still in the
    /// replay window.
    #[serde(default)]
    pub offset: Option<u64>,
}

/// One `output` SSE event.
#[derive(Debug, Deserialize, JsonSchema, Serialize)]
pub struct OutputEvent {
    pub offset: u64,
    pub stream: StreamKind,
    pub output: String,
}

/// One `gap` SSE event: the byte range a lagging or late subscriber lost.
#[derive(Debug, Deserialize, JsonSchema, Serialize)]
pub struct GapEvent {
    pub from: u64,
    pub to: u64,
}

/// The terminal `exit` SSE event. Emitted before the stream ends, so a client
/// that sees the body close without one knows the connection failed rather than
/// the command finishing.
#[derive(Debug, Deserialize, JsonSchema, Serialize)]
pub struct ExitEvent {
    pub exit_code: Option<i32>,
    pub signal: Option<i32>,
    pub truncated: bool,
    pub writers_may_be_alive: bool,
    /// Total bytes published, so a client can assert it saw all of them.
    pub offset: u64,
}

#[derive(Debug, Deserialize, JsonSchema, Serialize)]
pub struct StartResponse {
    pub exec_id: String,
    pub phase: Phase,
}

/// `POST /v1/exec/{id}/kill` response.
///
/// A named type rather than the `serde_json::json!` literal this used to be: an
/// ad-hoc `Value` has no schema to derive, so the one route whose body a client
/// most needs to branch on — `killed` distinguishes "signalled" from "the group
/// was already gone", and both are 200 — would have been the one route the
/// published document could not describe.
#[derive(Debug, Deserialize, JsonSchema, Serialize)]
pub struct KillResponse {
    pub exec_id: String,
    /// Whether a signal was actually delivered. `false` with a 200 means the
    /// process group had already exited, which is the outcome a kill wanted.
    pub killed: bool,
}

#[derive(Debug, Deserialize, JsonSchema, Serialize)]
pub struct PollResponse {
    pub exec_id: String,
    pub phase: Phase,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(flatten)]
    pub result: Option<Outcome>,
}

/// The JSON error body every failing control route returns.
///
/// `error` is a stable machine-readable slug and `detail` is prose for a human
/// reading a log. A client branches on `error` and the status code, never on
/// `detail` — which is why the slug is one of the constants below, chosen at each
/// call site, rather than a formatted string. `Cow` so the daemon keeps naming a
/// `&'static str` while a client deserializes into an owned one.
#[derive(Debug, Deserialize, JsonSchema, Serialize)]
pub struct ErrorBody {
    pub error: Cow<'static, str>,
    pub detail: String,
}

/// The `event:` names on `GET /v1/exec/{id}/stream`.
///
/// Named rather than spelled at each `Event::default().event(..)` call: the daemon
/// writes these and a client matches on them, and a typo on either side is a
/// stream that silently carries events nobody dispatches.
pub const EVENT_OUTPUT: &str = "output";
pub const EVENT_GAP: &str = "gap";
pub const EVENT_EXIT: &str = "exit";

// The `ErrorBody::error` slugs. Every failing control route returns one of these,
// they are the part of the surface a client branches on, and they are `&'static
// str` for the same reason the status code is a number: the pairing of the two is
// the contract, and neither is derivable from the other.

/// The body is not a valid request for this route. Always 400, never 404.
pub const ERROR_MALFORMED_REQUEST: &str = "malformed_request";
/// No exec is registered under this id, or its entry was collected after an ack.
pub const ERROR_UNKNOWN_EXEC: &str = "unknown_exec";
/// The child could not be spawned. 500, deliberately not 404.
pub const ERROR_SPAWN_FAILED: &str = "spawn_failed";
/// The exec has not exited, so acking would drop output still being written.
pub const ERROR_STILL_RUNNING: &str = "still_running";
/// An earlier ack already released the output.
pub const ERROR_ALREADY_ACKED: &str = "already_acked";
/// The exec was started without `stdin: true`. Fixable at start time, hence 409.
pub const ERROR_STDIN_NOT_REQUESTED: &str = "stdin_not_requested";
/// stdin was closed by an earlier eof, or the child stopped reading. 410: a retry
/// will never succeed.
pub const ERROR_STDIN_CLOSED: &str = "stdin_closed";
/// The child did not read within the configured window. Retryable, and some bytes
/// may already have been written.
pub const ERROR_STDIN_WRITE_TIMEOUT: &str = "stdin_write_timeout";
/// The decoded write exceeds the configured per-write cap.
pub const ERROR_STDIN_WRITE_TOO_LARGE: &str = "stdin_write_too_large";
/// The write to the pipe failed for a reason other than a broken pipe.
pub const ERROR_STDIN_WRITE_FAILED: &str = "stdin_write_failed";

#[cfg(test)]
mod tests {
    use super::*;

    /// The `rename_all` is the reason these types are shared rather than mirrored:
    /// a client that spelled the variants from the Rust identifiers would send
    /// `Running` where the daemon emits `running`.
    #[test]
    fn phase_and_stream_kind_round_trip_through_their_serde_names() {
        assert_eq!(
            serde_json::to_string(&Phase::Running).expect("serializes"),
            "\"running\""
        );
        assert_eq!(
            serde_json::from_str::<Phase>("\"acked\"").expect("deserializes"),
            Phase::Acked
        );
        assert_eq!(
            serde_json::to_string(&StreamKind::Stderr).expect("serializes"),
            "\"stderr\""
        );
    }

    /// `as_str` is a second spelling of what serde already writes, so the two must be
    /// equal for every variant or the bindings publish names the wire never carries.
    ///
    /// `ALL` is held complete by exhaustion rather than by a length check: the matches
    /// below have no wildcard arm, so a variant added to either enum fails to compile
    /// here until it is added to its `ALL` too.
    #[test]
    fn as_str_agrees_with_serde_for_every_phase_and_stream_kind() {
        for phase in Phase::ALL {
            assert_eq!(
                serde_json::to_string(&phase).expect("serializes"),
                format!("\"{}\"", phase.as_str()),
                "{phase:?} spells differently through as_str and serde"
            );
            match phase {
                Phase::Running | Phase::Exited | Phase::Acked => {}
            }
        }
        for kind in StreamKind::ALL {
            assert_eq!(
                serde_json::to_string(&kind).expect("serializes"),
                format!("\"{}\"", kind.as_str()),
                "{kind:?} spells differently through as_str and serde"
            );
            match kind {
                StreamKind::Stdout | StreamKind::Stderr => {}
            }
        }
    }

    /// Both halves of serde on one type is the whole point of the crate: what the
    /// daemon writes is what a client reads, checked rather than assumed.
    #[test]
    fn a_response_the_daemon_writes_deserializes_on_the_client_side() {
        let written = serde_json::to_string(&StartResponse {
            exec_id: "e1".into(),
            phase: Phase::Running,
        })
        .expect("serializes");
        let read: StartResponse = serde_json::from_str(&written).expect("deserializes");
        assert_eq!(read.exec_id, "e1");
        assert_eq!(read.phase, Phase::Running);
    }

    /// A request the client writes is one the daemon's extractor accepts, including
    /// every serde default — the fields a caller omits are the ones most likely to
    /// disagree across a hand-written mirror.
    #[test]
    fn a_start_request_omitting_every_defaulted_field_deserializes() {
        let request: StartRequest =
            serde_json::from_str(r#"{"exec_id":"e1","command":["true"]}"#).expect("deserializes");
        assert!(!request.shell);
        assert!(request.cwd.is_none());
        assert!(request.env.is_empty());
        assert!(request.timeout_sec.is_none());
        assert!(!request.stdin);
    }

    /// The flatten-plus-skip on `PollResponse.result` is the one shape whose two
    /// serde contracts genuinely differ — absent on the way out while running, and
    /// the same absence has to read back as `None` on the way in. Both directions,
    /// because a client that read a running exec's missing outcome as an error would
    /// fail on every poll before the first one that mattered.
    #[test]
    fn a_poll_response_round_trips_with_and_without_an_outcome() {
        let running = serde_json::to_string(&PollResponse {
            exec_id: "e1".into(),
            phase: Phase::Running,
            result: None,
        })
        .expect("serializes");
        assert_eq!(running, r#"{"exec_id":"e1","phase":"running"}"#);
        let read: PollResponse = serde_json::from_str(&running).expect("deserializes");
        assert!(read.result.is_none());

        let exited = serde_json::to_string(&PollResponse {
            exec_id: "e1".into(),
            phase: Phase::Exited,
            result: Some(Outcome {
                exit_code: Some(0),
                stdout: "hi".into(),
                ..Outcome::default()
            }),
        })
        .expect("serializes");
        let read: PollResponse = serde_json::from_str(&exited).expect("deserializes");
        let outcome = read.result.expect("the outcome is flattened into the body");
        assert_eq!(outcome.exit_code, Some(0));
        assert_eq!(outcome.stdout, "hi");
    }

    /// `error` is a `Cow`, so the daemon's borrowed slug and a client's owned string
    /// are the same type on the wire.
    #[test]
    fn an_error_body_carries_a_borrowed_slug_and_reads_back_owned() {
        let written = serde_json::to_string(&ErrorBody {
            error: Cow::Borrowed(ERROR_UNKNOWN_EXEC),
            detail: "e1".into(),
        })
        .expect("serializes");
        assert_eq!(written, r#"{"error":"unknown_exec","detail":"e1"}"#);
        let read: ErrorBody = serde_json::from_str(&written).expect("deserializes");
        assert_eq!(read.error, ERROR_UNKNOWN_EXEC);
    }
}
