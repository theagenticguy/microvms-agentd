// SPDX-License-Identifier: Apache-2.0
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

/// Why a stream stopped, and where a resume would pick up.
///
/// Returned by [`ExecHandle::for_each_event`] and
/// [`ExecHandle::for_each_event_async`] so the three endings are distinguishable
/// *without* a caller having to reason about which event it last saw. That distinction is
/// the whole point of this module — a body that ended without an `exit` event was cut, and
/// the byte sequence alone cannot say so — and asking every caller to re-derive it from a
/// tally is how one of them gets it wrong and reports a truncated stream as a clean pass.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct StreamEnd {
    pub reason: EndReason,
    /// The offset a resume would pass as [`StreamOptions::offset`].
    ///
    /// **Core's cursor, not a count of what the callback saw.** The two rules in this
    /// module's docs (advance only past delivered bytes, and advance past a `gap`) are
    /// obeyed by the state machine below; handing the number back means a caller resuming
    /// does not maintain a second cursor that disagrees with this one exactly when a
    /// reconnect happened.
    pub cursor: u64,
}

/// The three ways a stream ends without an error.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EndReason {
    /// The terminal `exit` event arrived and was delivered. The command is over.
    Exited,
    /// The callback answered [`ControlFlow::Break`]. Nothing is wrong; the caller stopped
    /// reading, and [`StreamEnd::cursor`] is where it stopped.
    Stopped,
    /// The body ended with **no** `exit` event and `reconnect` was off, so the connection
    /// was cut and this client was told not to re-attach. The command's outcome is
    /// unknown — not zero — and a caller reporting success here would pass a CI step on
    /// evidence it never received.
    Cut,
}

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

    /// Drives a stream to its end, handing every event to `on_event`.
    ///
    /// The same state machine [`Self::stream_with`] runs, driven by an ordinary loop
    /// instead of wrapped in a [`Stream`]. Everything about the wire behaviour is
    /// identical — same reconnects, same cursor, same terminal-event rule — and both share
    /// [`Self::advance`], so there is one implementation of the property and two ways to
    /// consume it.
    ///
    /// # Why a callback exists beside the `Stream`
    ///
    /// Because `Stream` is not in `std`. A consumer of [`Self::stream_with`] has to name
    /// the crate that defines the trait in order to call `poll_next`, so a client that
    /// wants to print a line per event acquires a dependency to advance a loop —
    /// `microvms-cli` carried `futures-util` for exactly that and nothing else, and
    /// `tests/thinness.rs` (CLI-2) asserts that crate's dependency set exactly. This
    /// method is that dependency's replacement: `ControlFlow` and `async fn` are both std,
    /// so a caller needs nothing but this crate.
    ///
    /// A **synchronous** callback is the right shape for a caller whose per-event work is
    /// synchronous — the CLI writes a line and returns. A caller whose per-event work is an
    /// `await` (both bindings: a bounded-channel `send`) wants
    /// [`Self::for_each_event_async`], which is the same loop with an awaited callback.
    /// Using this one there would mean `blocking_send` on a runtime worker, which is why
    /// the overload exists.
    ///
    /// # What the return value says that a tally cannot
    ///
    /// [`StreamEnd::reason`] distinguishes an `exit` event from a cut body from a caller
    /// that stopped reading. A caller counting events cannot: a cut stream and a finished
    /// one differ only in the terminal event, which is the argument for SSE framing here in
    /// the first place. [`StreamEnd::cursor`] is the state machine's own offset, so a
    /// caller resuming does not maintain a second one.
    ///
    /// An error ends the drive with `Err` and the events already handed over **stay handed
    /// over**. That asymmetry is deliberate: the bytes a callback already wrote are real
    /// output the caller received, and there is nothing to unwind them with.
    pub async fn for_each_event<F>(
        &self,
        options: StreamOptions,
        mut on_event: F,
    ) -> Result<StreamEnd, Error>
    where
        F: FnMut(ExecEvent) -> std::ops::ControlFlow<()>,
    {
        // Delegated rather than a second copy of the loop. A synchronous callback *is* an
        // async one whose future is already complete, and `std::future::ready` says exactly
        // that — so the ordering, the cursor read, the `Break` arm, and the three endings
        // have one implementation instead of two that agree until one is edited.
        self.for_each_event_async(options, |event| std::future::ready(on_event(event)))
            .await
    }

    /// [`Self::for_each_event`] for a callback that **awaits**.
    ///
    /// The same loop and the same [`Self::advance`] state machine; the only difference is
    /// that the per-event callback answers a future, which is `.await`ed before the next
    /// step is taken. So the events are still delivered strictly in order and still one at a
    /// time — a callback that awaits between events holds the drive rather than racing it,
    /// which is what makes this usable as backpressure.
    ///
    /// # Why the overload exists, and what it closed
    ///
    /// Both bindings consume a stream by pushing into a **capacity-1** `mpsc` channel that a
    /// foreign-language iterator drains (`microvms-py/src/exec.rs`, `microvms-js/src/exec.rs`).
    /// The channel bound is deliberate — the daemon's SSE body is the backpressure signal —
    /// and it means the channel is full whenever the host-language consumer is even slightly
    /// behind, i.e. normally. With [`Self::for_each_event`] the only available send is
    /// `blocking_send`, which would block the very runtime worker the driver is running on;
    /// so both bindings kept the `Stream` path, and `futures-util` with it, for a reason
    /// about backpressure rather than about the trait. `send(..).await` inside an
    /// `AsyncFnMut` yields the worker instead, and both bindings dropped the dependency on
    /// 2026-08-09.
    ///
    /// # `FnMut(ExecEvent) -> Fut` rather than `AsyncFnMut`
    ///
    /// Measured, not stylistic, and the measurement is the reason the signature looks older
    /// than the edition. `AsyncFnMut` is the obvious spelling and it compiles here — but a
    /// caller cannot `tokio::spawn` a drive that uses it. Proving the returned future `Send`
    /// requires naming `F::CallRefFuture<'a>` under a `for<'a>` bound, which is the unstable
    /// `async_fn_traits` feature; without it the spawn fails with *"`Send` would have to be
    /// implemented for the type `&ExecHandle`… but `Send` is actually implemented for
    /// `&'0 ExecHandle`, for some specific lifetime"*. Both bindings spawn this drive onto a
    /// runtime, so that is not a corner they can avoid, and the unstable bound additionally
    /// forces the callback's captures to `'static`.
    ///
    /// So `Fut` is a plain type parameter. The cost is that the callback's future cannot
    /// *borrow* from the closure's captures — it has no lifetime to name them with — so a
    /// caller sending on a channel clones the sender per event rather than holding `&sender`.
    /// For a `tokio::sync::mpsc::Sender` that is one atomic increment, which is the right
    /// price for a signature that can cross a spawn.
    ///
    /// The return value and the error asymmetry are [`Self::for_each_event`]'s, unchanged.
    pub async fn for_each_event_async<F, Fut>(
        &self,
        options: StreamOptions,
        mut on_event: F,
    ) -> Result<StreamEnd, Error>
    where
        F: FnMut(ExecEvent) -> Fut,
        Fut: std::future::Future<Output = std::ops::ControlFlow<()>>,
    {
        let mut state = StreamState::Reconnect {
            cursor: options.offset,
            attempts: 0,
        };
        // Seeded from the caller's own starting offset, so a drive that ends before any
        // event arrives reports where it began rather than zero.
        let mut cursor = options.offset;
        loop {
            let Some((item, next)) = self.advance(state, &options).await else {
                // `advance` yields nothing only when the body ended without an `exit`
                // event and reconnecting was refused — the `exit` case returns below,
                // before the machine is stepped again. So this is the cut.
                return Ok(StreamEnd {
                    reason: EndReason::Cut,
                    cursor,
                });
            };
            // Read off the machine rather than recomputed from the event: the two rules in
            // this module's docs live in `advance`, and a second `cursor.max(end)` here
            // would be a second implementation that agrees until a `gap` arrives.
            if let Some(advanced) = next.cursor() {
                cursor = advanced;
            }
            state = next;

            let event = item?;
            let terminal = matches!(event, ExecEvent::Exit(_));
            // Awaited before the loop advances, which is the whole point: the next attach
            // read does not start until the callback's own work is done, so a slow consumer
            // slows the stream rather than buffering behind it.
            if on_event(event).await.is_break() {
                return Ok(StreamEnd {
                    reason: EndReason::Stopped,
                    cursor,
                });
            }
            if terminal {
                return Ok(StreamEnd {
                    reason: EndReason::Exited,
                    cursor,
                });
            }
        }
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
            // A parse failure here is the undelimited-data ceiling, which is
            // `ErrorKind::Protocol` and therefore not retryable — so `advance` ends the
            // stream with it rather than reconnecting into the same non-SSE body. That is
            // the point: a proxy answering an error page would otherwise be retried
            // `max_reconnects` times, filling the buffer again each time.
            self.frames.extend(self.parser.feed(&chunk)?);
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

impl StreamState {
    /// The cursor this state carries, or `None` for [`StreamState::Done`].
    ///
    /// `Done` has none on purpose: it is reached from three different places and inventing
    /// one there would mean picking a number, where the caller
    /// ([`ExecHandle::for_each_event`]) already holds the last real one. `None` says "this
    /// state advanced nothing" rather than "the cursor is zero".
    fn cursor(&self) -> Option<u64> {
        match self {
            StreamState::Reconnect { cursor, .. } | StreamState::Attached { cursor, .. } => {
                Some(*cursor)
            }
            StreamState::Done => None,
        }
    }
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

    // ── the callback driver (`for_each_event`) ───────────────────────────────
    //
    // Four tests, mirroring the four `stream_with` properties that matter to a caller: the
    // events arrive in order, `Break` stops the drive, a reconnect still joins at the
    // cursor, and a cut without an exit event is *reported as a cut*. They are separate
    // tests rather than a re-run of the loop above on purpose — the driver is what the CLI
    // and (next) the bindings actually call, and "the `Stream` version works" is not a
    // statement about it.

    /// Drives a stream with `for_each_event` and returns what the callback saw.
    async fn drive(
        handle: &ExecHandle,
        options: StreamOptions,
    ) -> (Vec<ExecEvent>, Result<StreamEnd, Error>) {
        let mut seen = Vec::new();
        let end = handle
            .for_each_event(options, |event| {
                seen.push(event);
                std::ops::ControlFlow::Continue(())
            })
            .await;
        (seen, end)
    }

    /// **Events reach the callback in wire order, and the end names the terminal event.**
    ///
    /// Order is the property a callback API can silently break where a `Stream` cannot —
    /// buffering to hand over in a batch would reverse or coalesce, and the reader of a
    /// child's stdout would see the second chunk first. Asserted on the *offsets* rather
    /// than only on the reassembled bytes, because two chunks concatenated the wrong way
    /// round still yield the right total length.
    ///
    /// **Falsification** — push events onto a `Vec` inside `for_each_event` and call the
    /// callback after the loop, and the reason still reads `Exited` while the offsets come
    /// out of order only if the wire order was wrong; swap the two `Reply` chunks and this
    /// is red on `[0, 5]`. Verified: reversing the callback's argument order (delivering
    /// the second frame first) fails the offsets assertion.
    #[tokio::test(start_paused = true)]
    async fn the_driver_hands_every_event_to_the_callback_in_order() {
        let recorder = Recorder::with([Reply::Chunks(
            200,
            vec![output(0, b"AAAAA"), output(5, b"BBBBB"), exit(10)],
        )]);
        let (session, _, _) = session_with(Arc::clone(&recorder));
        let handle = session.exec("ordered");

        let (seen, end) = drive(&handle, StreamOptions::default()).await;
        let end = end.expect("the stream completes");

        let offsets: Vec<u64> = seen
            .iter()
            .filter_map(|event| match event {
                ExecEvent::Output { offset, .. } => Some(*offset),
                _ => None,
            })
            .collect();
        assert_eq!(offsets, [0, 5], "the callback saw the frames out of order");
        assert!(
            matches!(seen.last(), Some(ExecEvent::Exit(_))),
            "the terminal event has to be delivered, not merely observed: {seen:#?}"
        );
        assert_eq!(
            end,
            StreamEnd {
                reason: EndReason::Exited,
                cursor: 10,
            },
            "the end has to name the exit event and the total"
        );
        assert_eq!(recorder.requests().len(), 1);
    }

    /// **`ControlFlow::Break` stops the drive, and says so rather than reporting a cut.**
    ///
    /// Two assertions and the second is the load-bearing one. Stopping has to be
    /// distinguishable from a truncated stream: both end without an exit event, and a
    /// caller that read `Cut` after its own `Break` would report "the command's outcome is
    /// unknown" about a command it chose not to watch. The cursor is where the callback
    /// stopped, so passing it back as `--from-offset` resumes exactly there.
    ///
    /// **Falsification** — return `EndReason::Cut` for the break arm and the reason
    /// assertion is red; drop the `is_break()` check and the drive runs to the exit event,
    /// so `seen.len()` reads 3 instead of 1. Verified: removing the early return makes the
    /// event-count assertion fail with 3.
    #[tokio::test(start_paused = true)]
    async fn control_flow_break_stops_the_stream_and_is_not_reported_as_a_cut() {
        let recorder = Recorder::with([Reply::Chunks(
            200,
            vec![output(0, b"first\n"), output(6, b"second\n"), exit(13)],
        )]);
        let (session, _, _) = session_with(Arc::clone(&recorder));
        let handle = session.exec("halted");

        let mut seen = Vec::new();
        let end = handle
            .for_each_event(StreamOptions::default(), |event| {
                seen.push(event);
                // Stop on the first event, which is what a consumer with a `head -1` does.
                std::ops::ControlFlow::Break(())
            })
            .await
            .expect("stopping is not a failure");

        assert_eq!(
            seen.len(),
            1,
            "the drive kept going after Break, so a caller that stopped reading is still \
             being read to: {seen:#?}"
        );
        assert_eq!(
            end,
            StreamEnd {
                reason: EndReason::Stopped,
                cursor: 6,
            },
            "a caller's own stop must not read as a cut stream, and the cursor is where it \
             stopped"
        );
    }

    /// **A cut mid-drive still reconnects at the cursor, losing and duplicating nothing.**
    ///
    /// The same property as the `Stream` version's first test, asserted through the
    /// driver, because that is the path the CLI takes: if the driver dropped the cursor the
    /// reconnect would ask for byte zero and the caller would see `AAAA` twice, and the
    /// total byte count would look right. So the verdict is the reassembled bytes *and*
    /// the offset the second attach asked for.
    ///
    /// **Falsification** — seed the driver's `cursor` from zero instead of
    /// `options.offset`, or drop the `next.cursor()` read, and the reported end cursor is
    /// wrong while the bytes still join (the machine's own cursor is what drives the
    /// reconnect). Verified: replacing `next.cursor()` with `None` fails the `StreamEnd`
    /// assertion at cursor 0.
    #[tokio::test(start_paused = true)]
    async fn the_driver_reconnects_at_the_cursor_across_a_cut() {
        let recorder = Recorder::with([
            Reply::Chunks(200, vec![output(0, b"AAAA\n"), output(5, b"BBBB\n")]),
            Reply::Chunks(200, vec![output(10, b"CCCC\n"), exit(15)]),
        ]);
        let (session, _, _) = session_with(Arc::clone(&recorder));
        let handle = session.exec("resumed-driver");

        let (seen, end) = drive(&handle, StreamOptions::default()).await;
        let end = end.expect("the reconnect lands");

        let mut bytes = Vec::new();
        for event in &seen {
            if let ExecEvent::Output { data, .. } = event {
                bytes.extend_from_slice(data);
            }
        }
        assert_eq!(
            String::from_utf8_lossy(&bytes),
            "AAAA\nBBBB\nCCCC\n",
            "the two attaches did not reconstruct the output through the driver"
        );
        assert_eq!(end.reason, EndReason::Exited);
        assert_eq!(end.cursor, 15);

        let seen_requests = recorder.requests();
        assert_eq!(seen_requests.len(), 2, "the cut produced no reconnect");
        assert_eq!(
            requested_offset(&seen_requests[1]),
            10,
            "the reconnect asked for the wrong byte, so the seam is wrong"
        );
    }

    /// **A body that ended with no exit event and no reconnect is reported as `Cut`.**
    ///
    /// The one ending a byte count cannot detect, and the reason this returns a typed
    /// reason at all: `reconnect: false` is what a caller doing its own reconnection
    /// passes, and it must be able to tell "the command finished" from "the connection
    /// dropped and you told me not to retry". `cursor` is where its own reconnect starts.
    ///
    /// **Falsification** — report `EndReason::Exited` for the `advance` returned `None`
    /// arm and this is red on the reason; a caller acting on that would report exit code
    /// 0 for a command whose outcome nobody knows. Verified.
    #[tokio::test(start_paused = true)]
    async fn a_cut_with_reconnect_off_ends_the_drive_naming_the_cut() {
        let recorder = Recorder::with([Reply::Chunks(200, vec![output(0, b"partial")])]);
        let (session, _, _) = session_with(Arc::clone(&recorder));
        let handle = session.exec("truncated");

        let (seen, end) = drive(
            &handle,
            StreamOptions {
                reconnect: false,
                ..StreamOptions::default()
            },
        )
        .await;
        let end = end.expect("a cut is not an error when reconnect is off");

        assert_eq!(seen.len(), 1);
        assert!(
            !seen.iter().any(|event| matches!(event, ExecEvent::Exit(_))),
            "there was no terminal event to deliver"
        );
        assert_eq!(
            end,
            StreamEnd {
                reason: EndReason::Cut,
                cursor: 7,
            },
            "a stream with no exit event is a cut, and reporting anything else would let a \
             caller pass a CI step on output it never received"
        );
        assert_eq!(recorder.requests().len(), 1);
    }

    /// A fatal error ends the drive with `Err`, and the events already delivered stay
    /// delivered.
    ///
    /// The asymmetry `for_each_event`'s docs state, asserted: a 404 mid-stream is fatal
    /// (the entry was collected, so reattaching can never succeed) and there is nothing to
    /// unwind the bytes a callback already wrote with.
    #[tokio::test(start_paused = true)]
    async fn a_fatal_status_ends_the_drive_with_an_error() {
        let recorder = Recorder::with([Reply::Body(404, b"{\"error\":\"unknown_exec\"}".to_vec())]);
        let (session, _, _) = session_with(Arc::clone(&recorder));
        let handle = session.exec("collected");

        let (seen, end) = drive(&handle, StreamOptions::default()).await;
        assert!(seen.is_empty(), "nothing was delivered: {seen:#?}");
        let error = end.expect_err("a 404 on the attach is fatal");
        assert_eq!(error.wire_kind(), Some(WireKind::NotFound));
        assert_eq!(
            recorder.requests().len(),
            1,
            "a fatal status was reattached, which can never succeed"
        );
    }

    // ── the async callback driver (`for_each_event_async`) ────────────────────
    //
    // Four tests. Three mirror `for_each_event`'s — order, `Break` stops, a reconnect joins at
    // the cursor — because "the sync driver works" is not a statement about this one even though
    // the sync driver is now written in terms of it: the delegation could be inverted tomorrow,
    // and the bindings call *this* method. The fourth is the property only an awaited callback
    // has, and it is the reason the overload exists at all: a callback that awaits between
    // events loses none of them.

    /// Drives a stream with the async driver and returns what the callback saw.
    async fn drive_async(
        handle: &ExecHandle,
        options: StreamOptions,
    ) -> (Vec<ExecEvent>, Result<StreamEnd, Error>) {
        let mut seen = Vec::new();
        let end = handle
            .for_each_event_async(options, |event| {
                seen.push(event);
                std::future::ready(std::ops::ControlFlow::Continue(()))
            })
            .await;
        (seen, end)
    }

    /// **Events reach an async callback in wire order, and the end names the terminal event.**
    ///
    /// Asserted on the *offsets* rather than only on the reassembled bytes, because two chunks
    /// concatenated the wrong way round still yield the right total length. Order is the
    /// property an awaited callback could break where the sync one cannot: a driver that
    /// launched each callback future without awaiting it, or awaited them out of order, would
    /// deliver a child's second stdout chunk before its first.
    ///
    /// **Falsification** — remove the `.await` on `on_event(event)` and the loop does not
    /// compile; replace it with a driver that collects the futures and joins them at the end
    /// and the offsets come out in completion order rather than wire order. Verified by
    /// reversing the `Reply::Chunks` frame order, which fails this assertion on `[5, 0]`.
    #[tokio::test(start_paused = true)]
    async fn the_async_driver_hands_every_event_to_the_callback_in_order() {
        let recorder = Recorder::with([Reply::Chunks(
            200,
            vec![output(0, b"AAAAA"), output(5, b"BBBBB"), exit(10)],
        )]);
        let (session, _, _) = session_with(Arc::clone(&recorder));
        let handle = session.exec("ordered-async");

        let (seen, end) = drive_async(&handle, StreamOptions::default()).await;
        let end = end.expect("the stream completes");

        let offsets: Vec<u64> = seen
            .iter()
            .filter_map(|event| match event {
                ExecEvent::Output { offset, .. } => Some(*offset),
                _ => None,
            })
            .collect();
        assert_eq!(offsets, [0, 5], "the callback saw the frames out of order");
        assert!(
            matches!(seen.last(), Some(ExecEvent::Exit(_))),
            "the terminal event has to be delivered, not merely observed: {seen:#?}"
        );
        assert_eq!(
            end,
            StreamEnd {
                reason: EndReason::Exited,
                cursor: 10,
            },
            "the end has to name the exit event and the total"
        );
        assert_eq!(recorder.requests().len(), 1);
    }

    /// **`ControlFlow::Break` from an async callback stops the drive and is not a cut.**
    ///
    /// The load-bearing half is the reason: stopping has to be distinguishable from a truncated
    /// stream, because both end without an exit event. A binding whose host-language iterator
    /// was dropped answers `Break` here — a failed channel send — and it must not read as "the
    /// command's outcome is unknown".
    ///
    /// **Falsification** — return `EndReason::Cut` from the break arm and the reason assertion
    /// is red; drop the `is_break()` check and the drive runs to the exit event, so `seen.len()`
    /// reads 3 instead of 1. Verified: deleting the early return fails the event-count assertion
    /// with 3.
    #[tokio::test(start_paused = true)]
    async fn an_async_callback_breaking_stops_the_stream_and_is_not_reported_as_a_cut() {
        let recorder = Recorder::with([Reply::Chunks(
            200,
            vec![output(0, b"first\n"), output(6, b"second\n"), exit(13)],
        )]);
        let (session, _, _) = session_with(Arc::clone(&recorder));
        let handle = session.exec("halted-async");

        let mut seen = Vec::new();
        let end = handle
            .for_each_event_async(StreamOptions::default(), |event| {
                seen.push(event);
                // Stop on the first event, which is what a dropped binding iterator does.
                std::future::ready(std::ops::ControlFlow::Break(()))
            })
            .await
            .expect("stopping is not a failure");

        assert_eq!(
            seen.len(),
            1,
            "the drive kept going after Break, so a consumer that stopped reading is still \
             being read to: {seen:#?}"
        );
        assert_eq!(
            end,
            StreamEnd {
                reason: EndReason::Stopped,
                cursor: 6,
            },
            "a consumer's own stop must not read as a cut stream, and the cursor is where it \
             stopped"
        );
    }

    /// **A cut mid-drive still reconnects at the cursor through the async driver.**
    ///
    /// The same seam property, asserted through the path the bindings take. If the driver lost
    /// the cursor the reconnect would ask for byte zero and the consumer would see `AAAA` twice
    /// while the total byte count still looked plausible — so the verdict is the reassembled
    /// bytes *and* the offset the second attach asked for.
    ///
    /// **Falsification** — seed `cursor` from zero instead of `options.offset`, or drop the
    /// `next.cursor()` read, and the reported end cursor is wrong while the bytes still join.
    /// Verified: replacing `next.cursor()` with `None` fails the end-cursor assertion at 0.
    #[tokio::test(start_paused = true)]
    async fn the_async_driver_reconnects_at_the_cursor_across_a_cut() {
        let recorder = Recorder::with([
            Reply::Chunks(200, vec![output(0, b"AAAA\n"), output(5, b"BBBB\n")]),
            Reply::Chunks(200, vec![output(10, b"CCCC\n"), exit(15)]),
        ]);
        let (session, _, _) = session_with(Arc::clone(&recorder));
        let handle = session.exec("resumed-async");

        let (seen, end) = drive_async(&handle, StreamOptions::default()).await;
        let end = end.expect("the reconnect lands");

        let mut bytes = Vec::new();
        for event in &seen {
            if let ExecEvent::Output { data, .. } = event {
                bytes.extend_from_slice(data);
            }
        }
        assert_eq!(
            String::from_utf8_lossy(&bytes),
            "AAAA\nBBBB\nCCCC\n",
            "the two attaches did not reconstruct the output through the async driver"
        );
        assert_eq!(end.reason, EndReason::Exited);
        assert_eq!(end.cursor, 15);

        let seen_requests = recorder.requests();
        assert_eq!(seen_requests.len(), 2, "the cut produced no reconnect");
        assert_eq!(
            requested_offset(&seen_requests[1]),
            10,
            "the reconnect asked for the wrong byte, so the seam is wrong"
        );
    }

    /// **A callback that awaits between events loses none of them.**
    ///
    /// The one property only this overload has, set up as the exact shape both bindings use: a
    /// **capacity-1** `mpsc` channel the callback `send`s into and a separate consumer drains,
    /// with the consumer deliberately slower than the producer so the channel is full for every
    /// event after the first. That is the configuration the sync driver could not serve — its
    /// only available send is `blocking_send`, which would park the runtime worker the driver
    /// itself is running on — and it is why `microvms-py` and `microvms-js` kept the `Stream`
    /// path until this method existed.
    ///
    /// Five events go in and five come out, in wire order, terminal event included. The
    /// verdict is the sequence and not a count: a driver that dropped an event under
    /// backpressure would still report `Exited` with the right cursor, because the cursor is the
    /// state machine's and not the callback's.
    ///
    /// **Falsification** — replace the awaited `send` with `try_send` and ignore the failure
    /// (the shape of a driver that does not respect backpressure) and this is red with three
    /// of five events delivered. Verified.
    #[tokio::test(start_paused = true)]
    async fn an_async_callback_that_awaits_between_events_loses_none_of_them() {
        let recorder = Recorder::with([Reply::Chunks(
            200,
            vec![
                output(0, b"AA"),
                output(2, b"BB"),
                output(4, b"CC"),
                output(6, b"DD"),
                exit(8),
            ],
        )]);
        let (session, _, _) = session_with(Arc::clone(&recorder));
        let handle = session.exec("backpressured");

        // Capacity 1, the bindings' own bound: the SSE body is the backpressure signal, and
        // buffering here would defeat the cursor the core reconnects at.
        let (sender, mut receiver) = tokio::sync::mpsc::channel::<String>(1);
        let consumer = tokio::spawn(async move {
            let mut drained = Vec::new();
            while let Some(described) = receiver.recv().await {
                // Slower than the producer, so every send after the first waits.
                tokio::time::sleep(Duration::from_millis(50)).await;
                drained.push(described);
            }
            drained
        });

        let end = handle
            .for_each_event_async(StreamOptions::default(), |event| {
                let described = match &event {
                    ExecEvent::Output { offset, data, .. } => {
                        format!("out@{offset}:{}", String::from_utf8_lossy(data))
                    }
                    ExecEvent::Gap { from, to } => format!("gap@{from}..{to}"),
                    ExecEvent::Exit(exit) => format!("exit:{:?}", exit.exit_code),
                };
                // Cloned per event rather than borrowed, which is what the plain-`Fut`
                // signature requires and what both bindings do. One atomic increment.
                let sender = sender.clone();
                async move {
                    match sender.send(described).await {
                        Ok(()) => std::ops::ControlFlow::Continue(()),
                        // A closed receiver is the dropped-iterator case, not this path.
                        Err(_) => std::ops::ControlFlow::Break(()),
                    }
                }
            })
            .await
            .expect("the stream completes even though the consumer lags");
        // Closing the sender is what ends the consumer's `recv` loop.
        drop(sender);

        let drained = consumer.await.expect("the consumer task does not panic");
        assert_eq!(
            drained,
            [
                "out@0:AA",
                "out@2:BB",
                "out@4:CC",
                "out@6:DD",
                "exit:Some(0)",
            ],
            "a lagging capacity-1 consumer lost or reordered events, which is the failure the \
             awaited callback exists to prevent"
        );
        assert_eq!(
            end,
            StreamEnd {
                reason: EndReason::Exited,
                cursor: 8,
            },
            "the ending must still name the exit event and the total"
        );
    }
}
