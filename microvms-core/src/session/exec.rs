//! The handle a caller holds for one running exec.
//!
//! The whole reason this module exists is [`ExecHandle::stream`]. Everything else here
//! is a thin wrapper over one route.
//!
//! # The cursor is the contract
//!
//! A reconnect passes `?offset=<bytes consumed>`, and the daemon replays from there.
//! Without it a dropped connection means either losing everything after the drop or
//! replaying from zero, and neither is distinguishable from correct behaviour with
//! nothing to check against — E2B's cursorless `connect(pid)` and their issue #1352 are
//! exactly that.
//!
//! Two rules the cursor arithmetic has to obey, and both are properties of *what the
//! caller has seen* rather than of what arrived:
//!
//! * Advance only past bytes actually handed to the caller, so a reconnect never
//!   re-delivers and never skips.
//! * Advance past a `gap` too. The daemon has already moved its own cursor past evicted
//!   bytes; if ours did not follow, a reconnect would ask for them again and be told
//!   about the same gap forever.
//!
//! # The reconnect condition is a typed event, not a byte count
//!
//! A stream that ended *without* an `exit` event was cut. A stream that ended *with* one
//! is over. Those two are the identical byte sequence on a raw stream, which is the
//! whole argument for SSE framing here and why this is not a byte stream. The
//! reconnect decision reads that distinction and nothing else.

use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
use futures_util::Stream;

use super::sse::{ExecEvent, SseParser, decode};
use super::{HttpRequest, Transport};
use crate::error::{Error, ErrorKind, WireKind};

/// Backoff between reconnect attempts. Short, because the offset makes a reconnect
/// cheap — the daemon replays from the cursor, so nothing already delivered is refetched.
const RECONNECT_BACKOFF: [Duration; 5] = [
    Duration::from_millis(250),
    Duration::from_millis(500),
    Duration::from_secs(1),
    Duration::from_secs(2),
    Duration::from_secs(4),
];

/// How long an attached stream may be silent before it is treated as dead.
///
/// Four times the daemon's fifteen-second SSE keepalive, so three missed keepalives are
/// tolerated before a reconnect. Tighter than that turns a slow proxy into a reconnect
/// loop.
pub const DEFAULT_STREAM_IDLE_TIMEOUT: Duration = Duration::from_secs(60);

/// How often to re-poll while waiting for an exec to finish.
const POLL_INTERVAL: Duration = Duration::from_secs(1);

/// An exec's phase and, once it has one, its outcome.
///
/// A thin wrapper over the daemon's [`protocol::exec::PollResponse`] rather than a
/// re-modelling of it, so the two cannot disagree. What it adds is [`ExecResult::done`],
/// which is the question every caller actually asks.
#[derive(Debug)]
pub struct ExecResult {
    pub exec_id: String,
    pub phase: protocol::exec::Phase,
    /// `None` while running. Present once the child has exited.
    pub outcome: Option<protocol::exec::Outcome>,
}

impl From<protocol::exec::PollResponse> for ExecResult {
    fn from(response: protocol::exec::PollResponse) -> Self {
        Self {
            exec_id: response.exec_id,
            phase: response.phase,
            outcome: response.result,
        }
    }
}

impl ExecResult {
    /// Whether the exec has finished, whichever way.
    pub fn done(&self) -> bool {
        matches!(
            self.phase,
            protocol::exec::Phase::Exited | protocol::exec::Phase::Acked
        )
    }

    /// The exit code, when the child exited with one rather than dying to a signal.
    pub fn exit_code(&self) -> Option<i32> {
        self.outcome.as_ref().and_then(|outcome| outcome.exit_code)
    }

    /// Whether the command succeeded. `false` for a signal death and for a still-running
    /// exec, since neither is a success.
    pub fn succeeded(&self) -> bool {
        self.exit_code() == Some(0)
    }

    pub fn stdout(&self) -> &str {
        self.outcome
            .as_ref()
            .map_or("", |outcome| outcome.stdout.as_str())
    }

    pub fn stderr(&self) -> &str {
        self.outcome
            .as_ref()
            .map_or("", |outcome| outcome.stderr.as_str())
    }
}

/// What a stdin write accomplished.
pub type StdinAck = protocol::exec::StdinResponse;

/// How a stream should behave. See [`ExecHandle::stream_with`].
#[derive(Clone, Debug)]
pub struct StreamOptions {
    /// The byte to start at. Non-zero resumes a stream a previous process was reading.
    pub offset: u64,
    /// Whether to reconnect after a cut. `false` ends the stream at the cut instead,
    /// which is what a caller doing its own reconnection wants.
    pub reconnect: bool,
    /// How many reconnects before giving up. A bound rather than forever, because a
    /// stream that drops every time is a condition a caller needs reported.
    pub max_reconnects: u32,
    /// Turns a `gap` into an error instead of an event. What a caller that must have
    /// complete output wants.
    pub error_on_gap: bool,
    /// How long the body may be silent before the connection is treated as dead.
    pub idle_timeout: Duration,
}

impl Default for StreamOptions {
    fn default() -> Self {
        Self {
            offset: 0,
            reconnect: true,
            max_reconnects: 20,
            error_on_gap: false,
            idle_timeout: DEFAULT_STREAM_IDLE_TIMEOUT,
        }
    }
}

/// One exec, addressed by its caller-minted id.
///
/// The id is the idempotency key, so a handle survives a process restart: rebuild it
/// with the same id and every method still addresses the same server-side exec.
pub struct ExecHandle {
    transport: Arc<Transport>,
    exec_id: String,
}

impl ExecHandle {
    pub(crate) fn new(transport: Arc<Transport>, exec_id: String) -> Self {
        Self { transport, exec_id }
    }

    pub fn exec_id(&self) -> &str {
        &self.exec_id
    }

    /// Reads current status and output. Read-only server-side; safe to spin on.
    pub async fn poll(&self) -> Result<ExecResult, Error> {
        let response: protocol::exec::PollResponse = self
            .transport
            .send_json(HttpRequest::new(
                "GET",
                format!("/v1/exec/{}", self.exec_id),
            ))
            .await?;
        Ok(response.into())
    }

    /// Polls until the exec is done, or fails with a client-side timeout.
    ///
    /// A timeout here has not touched the exec — polling is read-only and the output
    /// lives until it is acked — so a caller that gives up can come back and poll again,
    /// and the message says so.
    ///
    /// A retryable error mid-wait is swallowed rather than raised: a VM under load drops
    /// a connection occasionally, and the whole point of a read-only poll is that
    /// repeating it costs nothing. A fatal one ends the wait.
    pub async fn wait(&self, timeout: Duration) -> Result<ExecResult, Error> {
        let deadline = tokio::time::Instant::now() + timeout;
        let mut last_phase: Option<protocol::exec::Phase> = None;
        loop {
            match self.poll().await {
                Ok(result) => {
                    if result.done() {
                        return Ok(result);
                    }
                    last_phase = Some(result.phase);
                }
                Err(err) if err.retryable() => {}
                Err(err) => return Err(err),
            }
            let now = tokio::time::Instant::now();
            if now >= deadline {
                let phase = match last_phase {
                    Some(protocol::exec::Phase::Running) => "running",
                    Some(protocol::exec::Phase::Exited) => "exited",
                    Some(protocol::exec::Phase::Acked) => "acked",
                    None => "unknown",
                };
                return Err(Error::wire(
                    WireKind::ExecTimeout,
                    format!(
                        "exec {} was still {phase} after {}s; the record and its output \
                         are untouched and can be re-polled",
                        self.exec_id,
                        timeout.as_secs()
                    ),
                ));
            }
            tokio::time::sleep(POLL_INTERVAL.min(deadline - now)).await;
        }
    }

    /// Yields output as it arrives, reconnecting at the last good offset.
    pub fn stream(&self) -> impl Stream<Item = Result<ExecEvent, Error>> + '_ {
        self.stream_with(StreamOptions::default())
    }

    /// [`Self::stream`] with the knobs. See [`StreamOptions`].
    ///
    /// Written as a generator over an explicit state machine rather than as a hand-rolled
    /// `Stream` impl: the reconnect logic is a loop with an `await` in the middle of it,
    /// and expressing that as a `poll_next` would mean storing the in-flight attach as a
    /// pinned field — which is where a self-referential-future bug lives.
    pub fn stream_with(
        &self,
        options: StreamOptions,
    ) -> impl Stream<Item = Result<ExecEvent, Error>> + '_ {
        futures_util::stream::unfold(
            StreamState::Reconnect {
                cursor: options.offset,
                attempts: 0,
            },
            move |state| {
                let options = options.clone();
                async move { self.advance(state, &options).await }
            },
        )
    }

    /// One step of the stream state machine.
    ///
    /// Returns `None` to end the stream, or the next item plus the state after it.
    async fn advance(
        &self,
        state: StreamState,
        options: &StreamOptions,
    ) -> Option<(Result<ExecEvent, Error>, StreamState)> {
        let mut state = state;
        loop {
            match state {
                StreamState::Done => return None,
                StreamState::Reconnect { cursor, attempts } => {
                    if attempts > 0 {
                        if !options.reconnect {
                            return None;
                        }
                        if attempts > options.max_reconnects {
                            return Some((
                                Err(Error::new(
                                    ErrorKind::Retryable,
                                    format!(
                                        "the stream of {} dropped {attempts} times without \
                                         an exit event; last good offset {cursor}",
                                        self.exec_id
                                    ),
                                )),
                                StreamState::Done,
                            ));
                        }
                        let backoff = RECONNECT_BACKOFF
                            [((attempts - 1) as usize).min(RECONNECT_BACKOFF.len() - 1)];
                        tokio::time::sleep(backoff).await;
                    }
                    match self.attach(cursor, options.idle_timeout).await {
                        Ok(attach) => {
                            state = StreamState::Attached {
                                attach: Box::new(attach),
                                cursor,
                                attempts,
                            };
                        }
                        // A cut connection or a failed mint. Neither says anything
                        // about the exec, which is still running server-side, so both
                        // are reconnected through rather than surfaced.
                        Err(err) if err.retryable() && options.reconnect => {
                            state = StreamState::Reconnect {
                                cursor,
                                attempts: attempts + 1,
                            };
                        }
                        // Anything fatal — a 404 on a collected entry above all, where
                        // reconnecting can never succeed — ends the stream with the
                        // error rather than looping on it.
                        Err(err) => return Some((Err(err), StreamState::Done)),
                    }
                }
                StreamState::Attached {
                    mut attach,
                    mut cursor,
                    attempts,
                } => match attach.next_event().await {
                    Ok(Some(ExecEvent::Output {
                        stream,
                        offset,
                        data,
                    })) => {
                        // Advance only past bytes actually handed over.
                        let end = offset + data.len() as u64;
                        cursor = cursor.max(end);
                        return Some((
                            Ok(ExecEvent::Output {
                                stream,
                                offset,
                                data,
                            }),
                            StreamState::Attached {
                                attach,
                                cursor,
                                attempts,
                            },
                        ));
                    }
                    Ok(Some(ExecEvent::Gap { from, to })) => {
                        // Follow the daemon past the evicted range, or a reconnect asks
                        // for it again and is told about the same gap forever.
                        cursor = cursor.max(to);
                        let item = if options.error_on_gap {
                            Err(Error::wire(
                                WireKind::OutputGap,
                                format!("output bytes [{from}, {to}) are unrecoverable"),
                            ))
                        } else {
                            Ok(ExecEvent::Gap { from, to })
                        };
                        let next = if options.error_on_gap {
                            StreamState::Done
                        } else {
                            StreamState::Attached {
                                attach,
                                cursor,
                                attempts,
                            }
                        };
                        return Some((item, next));
                    }
                    // The terminal event. The stream is over, and this is the only thing
                    // that says so.
                    Ok(Some(ExecEvent::Exit(exit))) => {
                        return Some((Ok(ExecEvent::Exit(exit)), StreamState::Done));
                    }
                    // The body ended with no exit event, so the connection was cut.
                    Ok(None) => {
                        if !options.reconnect {
                            return None;
                        }
                        state = StreamState::Reconnect {
                            cursor,
                            attempts: attempts + 1,
                        };
                    }
                    Err(err) if err.retryable() && options.reconnect => {
                        state = StreamState::Reconnect {
                            cursor,
                            attempts: attempts + 1,
                        };
                    }
                    Err(err) => return Some((Err(err), StreamState::Done)),
                },
            }
        }
    }

    /// Opens one attach at `offset`.
    async fn attach(&self, offset: u64, idle_timeout: Duration) -> Result<Attach, Error> {
        let path = format!("/v1/exec/{}/stream?offset={offset}", self.exec_id);
        let mut request = HttpRequest::new("GET", path.clone());
        request
            .headers
            .push(("accept".into(), "text/event-stream".into()));
        // Headers are built here rather than in `Transport::request`, because the
        // streaming path does not go through it — and the mint has to happen anyway,
        // which is what makes a mid-stream reconnect re-mint an expired token.
        request.headers.extend(self.transport.headers(None).await?);
        request.timeout = None;

        let (head, chunks) = self
            .transport
            .backend
            .open_stream(request, idle_timeout)
            .await?;
        // The status is checked before any body byte, so a 404 on an unknown exec id
        // surfaces as NotFound rather than as an empty stream.
        head.error_for_status("GET", &path)?;
        Ok(Attach {
            chunks,
            parser: SseParser::new(),
            frames: std::collections::VecDeque::new(),
        })
    }

    /// Writes to the child's stdin.
    ///
    /// Requires the exec to have been started with `stdin: true`, or the daemon answers
    /// 409. `eof` in the same call is the common case for feeding a prompt: two round
    /// trips would leave a window where the child has the bytes but not the EOF that
    /// tells it the input is complete.
    pub async fn write_stdin(&self, data: &[u8], eof: bool) -> Result<StdinAck, Error> {
        let body = protocol::exec::StdinRequest {
            data_b64: (!data.is_empty())
                .then(|| base64::engine::general_purpose::STANDARD.encode(data)),
            signal: eof.then(|| "eof".to_string()),
        };
        let mut request = HttpRequest::new("POST", format!("/v1/exec/{}/stdin", self.exec_id));
        request
            .headers
            .push(("content-type".into(), "application/json".into()));
        request.body = serde_json::to_vec(&body).map_err(|err| {
            Error::invalid_arg(format!("the stdin body will not serialize: {err}"))
        })?;
        self.transport.send_json(request).await
    }

    /// Sends EOF.
    ///
    /// Nothing else closes stdin: the daemon's copy of the pipe outlives `Child::wait()`,
    /// so a child blocked reading stdin hangs until its timeout unless someone calls
    /// this.
    pub async fn close_stdin(&self) -> Result<StdinAck, Error> {
        self.write_stdin(&[], true).await
    }

    /// Releases the buffered output and starts the TTL clock.
    ///
    /// 409 means either the exec has not exited — output is still being written — or an
    /// earlier ack already took it. Both are real states, not the same state, and the
    /// daemon's detail string distinguishes them.
    pub async fn ack(&self) -> Result<ExecResult, Error> {
        let response: protocol::exec::PollResponse = self
            .transport
            .send_json(HttpRequest::new(
                "POST",
                format!("/v1/exec/{}/ack", self.exec_id),
            ))
            .await?;
        Ok(response.into())
    }

    /// Signals the whole process group, not just the direct child.
    ///
    /// Returns whether anything was signalled. `false` means no pgid was ever captured,
    /// i.e. the child had already been reaped — which is the outcome a kill wanted, and
    /// is why it is a 200 rather than an error.
    pub async fn kill(&self) -> Result<bool, Error> {
        let response: protocol::exec::KillResponse = self
            .transport
            .send_json(HttpRequest::new(
                "POST",
                format!("/v1/exec/{}/kill", self.exec_id),
            ))
            .await?;
        Ok(response.killed)
    }

    /// Waits, then acks, returning the result that carries the output.
    ///
    /// Which result is returned matters: the ack response carries the released output,
    /// and a poll issued after the ack reports `acked` with no output at all. Returning
    /// the wrong one is a silent empty-output bug, so the sequencing lives here rather
    /// than at every call site.
    pub async fn wait_and_ack(&self, timeout: Duration) -> Result<ExecResult, Error> {
        let done = self.wait(timeout).await?;
        if done.phase == protocol::exec::Phase::Acked {
            return Ok(done);
        }
        self.ack().await
    }
}

impl std::fmt::Debug for ExecHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExecHandle")
            .field("exec_id", &self.exec_id)
            .finish()
    }
}

/// One live attach: a chunk source plus the parser holding its partial frame.
struct Attach {
    chunks: Box<dyn super::ChunkSource>,
    parser: SseParser,
    frames: std::collections::VecDeque<super::Frame>,
}

impl Attach {
    /// The next event this attach can produce, or `None` when its body ends.
    ///
    /// A frame that decodes to nothing is skipped rather than returned, so an unknown
    /// event name does not surface as a spurious end-of-stream.
    async fn next_event(&mut self) -> Result<Option<ExecEvent>, Error> {
        loop {
            while let Some(frame) = self.frames.pop_front() {
                if let Some(event) = decode(&frame)? {
                    return Ok(Some(event));
                }
            }
            let Some(chunk) = self.chunks.next_chunk().await? else {
                return Ok(None);
            };
            self.frames.extend(self.parser.feed(&chunk));
        }
    }
}

/// Where the stream is. Boxed attach because the enum otherwise carries a trait object
/// inline and every state pays its size.
enum StreamState {
    /// Needs an attach. `attempts` is zero for the first one, which is why the backoff
    /// and the max-reconnect check are both skipped there.
    Reconnect {
        cursor: u64,
        attempts: u32,
    },
    Attached {
        attach: Box<Attach>,
        cursor: u64,
        attempts: u32,
    },
    Done,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::testing::{Recorder, Reply, session_with};
    use futures_util::StreamExt as _;

    /// One SSE `output` frame on the wire.
    fn output(offset: u64, bytes: &[u8]) -> Vec<u8> {
        let encoded = base64::engine::general_purpose::STANDARD.encode(bytes);
        format!(
            "event: output\ndata: {{\"offset\":{offset},\"stream\":\"stdout\",\
             \"output\":\"{encoded}\"}}\n\n"
        )
        .into_bytes()
    }

    fn exit(total: u64) -> Vec<u8> {
        format!(
            "event: exit\ndata: {{\"exit_code\":0,\"signal\":null,\"truncated\":false,\
             \"writers_may_be_alive\":false,\"offset\":{total}}}\n\n"
        )
        .into_bytes()
    }

    fn gap(from: u64, to: u64) -> Vec<u8> {
        format!("event: gap\ndata: {{\"from\":{from},\"to\":{to}}}\n\n").into_bytes()
    }

    /// Collects a stream's bytes and its events.
    async fn collect(
        handle: &ExecHandle,
        options: StreamOptions,
    ) -> (Vec<u8>, Vec<Result<ExecEvent, Error>>) {
        let mut bytes = Vec::new();
        let mut events = Vec::new();
        let mut stream = std::pin::pin!(handle.stream_with(options));
        while let Some(item) = stream.next().await {
            if let Ok(ExecEvent::Output { data, .. }) = &item {
                bytes.extend_from_slice(data);
            }
            events.push(item);
        }
        (bytes, events)
    }

    /// The offset a stream request asked for.
    fn requested_offset(request: &HttpRequest) -> u64 {
        request
            .path
            .split("offset=")
            .nth(1)
            .expect("a stream request always carries an offset")
            .parse()
            .expect("the offset is a number")
    }

    /// The reconnect property, asserted on the reassembled byte sequence.
    ///
    /// The verdict is the bytes, not the absence of an error: a client that reconnected
    /// at zero would deliver every byte too, and only the seam shows the difference. So
    /// this checks the join *and* the offset the second attach asked for.
    #[tokio::test(start_paused = true)]
    async fn a_cut_mid_stream_reconnects_at_the_cursor_losing_and_duplicating_nothing() {
        let recorder = Recorder::with([
            // First attach: two output frames, then the body ends with no exit event.
            Reply::Chunks(200, vec![output(0, b"AAAA\n"), output(5, b"BBBB\n")]),
            // Second attach: the daemon replays from the requested offset.
            Reply::Chunks(200, vec![output(10, b"CCCC\n"), exit(15)]),
        ]);
        let (session, _, _) = session_with(Arc::clone(&recorder));
        let handle = session.exec("resume");

        let (bytes, events) = collect(&handle, StreamOptions::default()).await;

        assert_eq!(
            String::from_utf8_lossy(&bytes),
            "AAAA\nBBBB\nCCCC\n",
            "the two attaches did not reconstruct the output, so the resume \
             duplicated or lost bytes at the seam"
        );
        assert!(
            events.iter().all(Result::is_ok),
            "a cut surfaced as an error rather than a reconnect: {events:#?}"
        );
        assert!(
            matches!(events.last(), Some(Ok(ExecEvent::Exit(_)))),
            "the stream did not end on the terminal event: {events:#?}"
        );

        let seen = recorder.requests();
        assert_eq!(
            seen.len(),
            2,
            "the cut did not produce exactly one reconnect"
        );
        assert_eq!(requested_offset(&seen[0]), 0);
        assert_eq!(
            requested_offset(&seen[1]),
            10,
            "the reconnect asked for the wrong byte, so the seam is wrong"
        );
    }

    /// A stream that ends *with* an exit event does not reconnect.
    ///
    /// The mirror of the test above, and the reason the transport is framed: the two
    /// cases are the same byte count and differ only in the terminal event.
    #[tokio::test(start_paused = true)]
    async fn a_stream_that_ended_with_an_exit_event_does_not_reconnect() {
        let recorder = Recorder::with([Reply::Chunks(200, vec![output(0, b"hi\n"), exit(3)])]);
        let (session, _, _) = session_with(Arc::clone(&recorder));
        let handle = session.exec("finished");

        let (bytes, _) = collect(&handle, StreamOptions::default()).await;
        assert_eq!(bytes, b"hi\n");
        assert_eq!(
            recorder.requests().len(),
            1,
            "a finished stream was reattached, which would replay a completed exec \
             forever"
        );
    }

    /// A gap advances the cursor past the evicted range.
    ///
    /// Without this the reconnect asks for bytes the daemon has already dropped and is
    /// told about the same gap forever — a livelock that looks like a slow stream.
    #[tokio::test(start_paused = true)]
    async fn a_gap_advances_the_cursor_so_a_reconnect_does_not_ask_for_evicted_bytes() {
        let recorder = Recorder::with([
            Reply::Chunks(200, vec![output(0, b"AA"), gap(2, 900)]),
            Reply::Chunks(200, vec![output(900, b"ZZ"), exit(902)]),
        ]);
        let (session, _, _) = session_with(Arc::clone(&recorder));
        let handle = session.exec("lagged");

        let (bytes, events) = collect(&handle, StreamOptions::default()).await;
        assert_eq!(bytes, b"AAZZ");
        assert!(
            events
                .iter()
                .any(|event| matches!(event, Ok(ExecEvent::Gap { from: 2, to: 900 }))),
            "the gap was swallowed, so a truncated log reads as a complete one: \
             {events:#?}"
        );
        assert_eq!(
            requested_offset(&recorder.requests()[1]),
            900,
            "the reconnect asked for bytes the daemon had already evicted"
        );
    }

    /// `error_on_gap` turns a gap into a typed error and ends the stream.
    #[tokio::test(start_paused = true)]
    async fn error_on_gap_surfaces_the_lost_range_as_an_error() {
        let recorder = Recorder::with([Reply::Chunks(200, vec![output(0, b"AA"), gap(2, 900)])]);
        let (session, _, _) = session_with(recorder);
        let handle = session.exec("lagged");

        let (_, events) = collect(
            &handle,
            StreamOptions {
                error_on_gap: true,
                ..StreamOptions::default()
            },
        )
        .await;
        let err = events
            .last()
            .expect("an event")
            .as_ref()
            .expect_err("the gap is an error");
        assert_eq!(err.wire_kind(), Some(WireKind::OutputGap));
        assert!(err.to_string().contains("[2, 900)"), "{err}");
    }

    /// A transport failure opening the attach is reconnected through, and the mint that
    /// the reconnect performs is what carries a fresh token mid-stream (TRAP-9).
    #[tokio::test(start_paused = true)]
    async fn a_failed_attach_is_retried_and_the_reconnect_re_mints_an_expired_token() {
        let recorder = Recorder::with([
            Reply::Cut("connection reset"),
            Reply::Chunks(200, vec![output(0, b"ok\n"), exit(3)]),
        ]);
        let (session, auth, clock) = session_with(Arc::clone(&recorder));
        let handle = session.exec("retried");

        // Age the cached token past the refresh window before the stream starts, so the
        // attach has to mint. The reconnect then crosses the window again.
        clock.advance(super::super::DEFAULT_REFRESH_AFTER + Duration::from_secs(1));

        let (bytes, events) = collect(&handle, StreamOptions::default()).await;
        assert_eq!(bytes, b"ok\n");
        assert!(events.iter().all(Result::is_ok), "{events:#?}");
        assert_eq!(
            auth.mint_count(),
            1,
            "the attach did not mint inside the request path"
        );
        assert_eq!(recorder.requests().len(), 2);
    }

    /// A 404 is fatal even with reconnect on: the entry was collected, so reattaching
    /// can never succeed.
    #[tokio::test(start_paused = true)]
    async fn a_collected_exec_ends_the_stream_rather_than_reconnecting_forever() {
        let recorder = Recorder::with([Reply::Body(404, b"{\"error\":\"unknown_exec\"}".to_vec())]);
        let (session, _, _) = session_with(Arc::clone(&recorder));
        let handle = session.exec("gone");

        let (_, events) = collect(&handle, StreamOptions::default()).await;
        assert_eq!(events.len(), 1);
        let err = events[0].as_ref().expect_err("404 is an error");
        assert_eq!(err.wire_kind(), Some(WireKind::NotFound));
        assert_eq!(
            recorder.requests().len(),
            1,
            "a fatal status was reattached, which can never succeed"
        );
    }

    /// The reconnect budget is bounded, and running out is reported with the last good
    /// offset rather than as a silent end.
    #[tokio::test(start_paused = true)]
    async fn a_stream_that_never_completes_gives_up_naming_its_last_good_offset() {
        let mut replies = vec![Reply::Chunks(200, vec![output(0, b"AA")])];
        for _ in 0..3 {
            replies.push(Reply::Chunks(200, vec![]));
        }
        let recorder = Recorder::with(replies);
        let (session, _, _) = session_with(Arc::clone(&recorder));
        let handle = session.exec("flaky");

        let (bytes, events) = collect(
            &handle,
            StreamOptions {
                max_reconnects: 2,
                ..StreamOptions::default()
            },
        )
        .await;
        assert_eq!(bytes, b"AA");
        let err = events
            .last()
            .expect("an event")
            .as_ref()
            .expect_err("running out of reconnects is an error");
        assert!(err.retryable(), "the exec is still alive server-side");
        assert!(
            err.to_string().contains("last good offset 2"),
            "the give-up must name where to resume: {err}"
        );
        assert_eq!(recorder.requests().len(), 3, "the budget was not respected");
    }

    /// `reconnect: false` ends the stream at the cut, for a caller doing its own.
    #[tokio::test(start_paused = true)]
    async fn reconnect_off_ends_the_stream_at_the_cut() {
        let recorder = Recorder::with([Reply::Chunks(200, vec![output(0, b"AA")])]);
        let (session, _, _) = session_with(Arc::clone(&recorder));
        let handle = session.exec("once");

        let (bytes, events) = collect(
            &handle,
            StreamOptions {
                reconnect: false,
                ..StreamOptions::default()
            },
        )
        .await;
        assert_eq!(bytes, b"AA");
        assert!(events.iter().all(Result::is_ok));
        assert_eq!(recorder.requests().len(), 1);
    }

    /// A non-zero starting offset is what a second process resuming a stream passes.
    #[tokio::test(start_paused = true)]
    async fn a_stream_can_start_at_an_offset_a_previous_process_left_off_at() {
        let recorder = Recorder::with([Reply::Chunks(200, vec![output(64, b"tail"), exit(68)])]);
        let (session, _, _) = session_with(Arc::clone(&recorder));
        let handle = session.exec("resumed");

        let (bytes, _) = collect(
            &handle,
            StreamOptions {
                offset: 64,
                ..StreamOptions::default()
            },
        )
        .await;
        assert_eq!(bytes, b"tail");
        assert_eq!(requested_offset(&recorder.last()), 64);
    }

    /// `wait_and_ack` returns the ack's result, which is the one carrying the output.
    ///
    /// Returning the poll's instead is a silent empty-output bug: a poll after the ack
    /// reports `acked` with nothing in it.
    #[tokio::test(start_paused = true)]
    async fn wait_and_ack_returns_the_ack_result_that_carries_the_output() {
        let recorder = Recorder::with([
            Reply::ok(serde_json::json!({"exec_id":"e1","phase":"running"})),
            Reply::ok(serde_json::json!({"exec_id":"e1","phase":"exited"})),
            Reply::ok(serde_json::json!({
                "exec_id":"e1","phase":"exited","exit_code":0,"signal":null,
                "stdout":"the output","stderr":"","truncated":false,
                "writers_may_be_alive":false
            })),
        ]);
        let (session, _, _) = session_with(Arc::clone(&recorder));
        let handle = session.exec("e1");

        let result = handle
            .wait_and_ack(Duration::from_secs(30))
            .await
            .expect("waits then acks");
        assert_eq!(result.stdout(), "the output");
        assert!(result.succeeded());

        let seen = recorder.requests();
        assert_eq!(seen.len(), 3);
        assert_eq!(seen[2].path, "/v1/exec/e1/ack");
        assert_eq!(seen[2].method, "POST");
    }

    /// An already-acked exec is not acked twice: the second ack is a 409, and the phase
    /// is what tells the difference before the request goes out.
    #[tokio::test(start_paused = true)]
    async fn wait_and_ack_does_not_ack_an_already_acked_exec() {
        let recorder = Recorder::with([Reply::ok(serde_json::json!({
            "exec_id":"e1","phase":"acked","exit_code":0,"signal":null,
            "stdout":"","stderr":"","truncated":false,"writers_may_be_alive":false
        }))]);
        let (session, _, _) = session_with(Arc::clone(&recorder));

        session
            .exec("e1")
            .wait_and_ack(Duration::from_secs(30))
            .await
            .expect("an acked exec is done");
        assert_eq!(
            recorder.requests().len(),
            1,
            "a second ack went out, which the daemon answers 409"
        );
    }

    /// A retryable failure mid-wait is swallowed, because polling is read-only and
    /// repeating it costs nothing.
    #[tokio::test(start_paused = true)]
    async fn a_dropped_connection_mid_wait_is_retried_rather_than_raised() {
        let recorder = Recorder::with([
            Reply::Cut("connection reset"),
            Reply::ok(serde_json::json!({
                "exec_id":"e1","phase":"exited","exit_code":3,"signal":null,
                "stdout":"","stderr":"boom","truncated":false,"writers_may_be_alive":false
            })),
        ]);
        let (session, _, _) = session_with(recorder);

        let result = session
            .exec("e1")
            .wait(Duration::from_secs(30))
            .await
            .expect("the retry lands");
        assert_eq!(result.exit_code(), Some(3));
        assert!(!result.succeeded());
        assert_eq!(result.stderr(), "boom");
    }

    /// A wait that times out says the exec is untouched, because it is.
    #[tokio::test(start_paused = true)]
    async fn a_wait_timeout_says_the_record_can_be_re_polled() {
        let replies: Vec<Reply> = (0..10)
            .map(|_| Reply::ok(serde_json::json!({"exec_id":"e1","phase":"running"})))
            .collect();
        let recorder = Recorder::with(replies);
        let (session, _, _) = session_with(recorder);

        let err = session
            .exec("e1")
            .wait(Duration::from_secs(3))
            .await
            .expect_err("the exec never finishes");
        assert_eq!(err.wire_kind(), Some(WireKind::ExecTimeout));
        assert_eq!(err.kind(), ErrorKind::Timeout);
        assert!(err.to_string().contains("still running"), "{err}");
        assert!(err.to_string().contains("re-polled"), "{err}");
    }

    /// A stdin write base64s its data and names the eof signal, and an empty write with
    /// eof is a valid close.
    #[tokio::test]
    async fn a_stdin_write_carries_base64_and_the_eof_signal() {
        let recorder = Recorder::with([
            Reply::ok(serde_json::json!({"exec_id":"e1","written":3,"eof":true})),
            Reply::ok(serde_json::json!({"exec_id":"e1","written":0,"eof":true})),
        ]);
        let (session, _, _) = session_with(Arc::clone(&recorder));
        let handle = session.exec("e1");

        let ack = handle.write_stdin(b"go\n", true).await.expect("writes");
        assert_eq!(ack.written, 3);
        assert!(ack.eof);
        let body: serde_json::Value =
            serde_json::from_slice(&recorder.last().body).expect("json body");
        assert_eq!(body["data_b64"], "Z28K");
        assert_eq!(body["signal"], "eof");

        handle.close_stdin().await.expect("closes");
        let body: serde_json::Value =
            serde_json::from_slice(&recorder.last().body).expect("json body");
        assert!(
            body.get("data_b64").is_none_or(serde_json::Value::is_null),
            "a bare close must not send an empty data field: {body}"
        );
        assert_eq!(body["signal"], "eof");
    }

    /// `kill` reports whether anything was signalled, and `false` with a 200 is a real
    /// answer rather than a failure.
    #[tokio::test]
    async fn kill_reports_false_for_a_group_that_had_already_exited() {
        let recorder = Recorder::with([
            Reply::ok(serde_json::json!({"exec_id":"e1","killed":true})),
            Reply::ok(serde_json::json!({"exec_id":"e1","killed":false})),
        ]);
        let (session, _, _) = session_with(Arc::clone(&recorder));

        assert!(session.kill("e1").await.expect("kills"));
        assert!(!session.kill("e1").await.expect("already gone"));
        assert_eq!(recorder.last().path, "/v1/exec/e1/kill");
    }

    /// A running poll has no outcome, and that absence is not an error — it is the shape
    /// of every poll before the last one.
    #[tokio::test]
    async fn a_running_poll_carries_no_outcome_and_is_not_an_error() {
        let recorder = Recorder::with([Reply::ok(
            serde_json::json!({"exec_id":"e1","phase":"running"}),
        )]);
        let (session, _, _) = session_with(recorder);

        let result = session.exec("e1").poll().await.expect("polls");
        assert!(!result.done());
        assert!(result.outcome.is_none());
        assert_eq!(result.stdout(), "");
        assert_eq!(result.exit_code(), None);
        assert!(!result.succeeded());
    }
}
