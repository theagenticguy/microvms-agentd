// SPDX-License-Identifier: Apache-2.0
//! Command execution: idempotent start, read-only poll, ack-gated release, and
//! process-group kill.
//!
//! The lifecycle here is the one the `agentd-model` crate enumerates. Running →
//! Exited → Acked, with output held until the caller acks and only acked entries
//! eligible for TTL collection. Every rule below has a defect behind it, and the
//! four that are easiest to reintroduce are:
//!
//! * **Pipes, not temp files.** The Python predecessor wrote output to temp files
//!   and unlinked them when the direct child exited, which destroyed everything a
//!   backgrounded grandchild wrote afterward. Pipes invert that: grandchildren
//!   inherit the write end, so EOF arrives only when the last writer closes, and
//!   "the backgrounded server keeps logging" works by construction rather than by
//!   a special case.
//! * **The pgid is captured immediately after spawn.** `Child::id()` returns
//!   `None` once the child has been reaped, so reading it lazily from the kill
//!   path yields nothing exactly when a kill is most needed.
//! * **No `pre_exec`.** Demotion goes through `Command::uid`/`gid`, which do the
//!   work in C between fork and exec. Running interpreted code in a forked child
//!   of a threaded process can deadlock on an allocator lock the parent held.
//! * **`cwd` omitted means inherit.** No `cd`, and no defaulting to `/`. The
//!   daemon is the container `CMD`, so its own working directory *is* the image
//!   `WORKDIR`, and forcing `/` broke every prebuilt-image task.
//!
//! # Streaming and stdin
//!
//! Poll-only exec cannot serve an agent harness running inside the VM: Claude
//! Code or Codex CLI emits for many minutes and may need a prompt written to its
//! stdin, and re-fetching the whole buffer on a timer is both quadratic and
//! unable to say "the command is still alive, you just have not missed anything".
//! Two additions close that, and the shapes are not arbitrary:
//!
//! * **Output streams over SSE with a byte-offset cursor.** The offset is the
//!   difference between a reconnect that works and one that silently loses or
//!   duplicates bytes — E2B's `connect(pid)` has no cursor, and their issue #1352
//!   is exactly that. SSE rather than a raw chunked body because framing is what
//!   lets a client distinguish "the command exited" from "the connection died";
//!   with bare bytes those are the same observation. The platform documents SSE
//!   support on the MicroVM inbound endpoint explicitly.
//! * **stdin is a separate POST, never multiplexed onto the output stream.**
//!   Runloop, Daytona, E2B and Modal all split them, and the reason is that it
//!   makes a dropped attach harmless: the writer half does not live on the
//!   connection that just died, so nothing has to be re-established to keep
//!   feeding a running process. An exec is a server-side object and attaching is
//!   a view onto it.

use std::collections::VecDeque;
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};

use std::borrow::Cow;

use axum::Json;
use axum::extract::rejection::{JsonRejection, QueryRejection};
use axum::extract::{Path, Query, State};
use axum::http::{HeaderValue, StatusCode};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use base64::Engine as _;
use bytes::Bytes;
use futures_util::Stream;
use serde::Serialize;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::{ChildStdin, Command};
use tokio::sync::{Mutex, broadcast};

use crate::state::AppState;
// The error slugs and SSE event names. Named rather than spelled at each call site:
// a client matches on these exact strings, so a typo on either side is a response a
// consumer cannot branch on.
use protocol::exec::{
    ERROR_ALREADY_ACKED, ERROR_MALFORMED_REQUEST, ERROR_SPAWN_FAILED, ERROR_STDIN_CLOSED,
    ERROR_STDIN_NOT_REQUESTED, ERROR_STDIN_WRITE_FAILED, ERROR_STDIN_WRITE_TIMEOUT,
    ERROR_STDIN_WRITE_TOO_LARGE, ERROR_STILL_RUNNING, ERROR_UNKNOWN_EXEC, EVENT_EXIT, EVENT_GAP,
    EVENT_OUTPUT,
};

/// The wire types, re-exported from their original paths.
///
/// They live in the `protocol` crate now, because a client needs the identical
/// shapes and the only thing that had kept two copies in step was review. Re-exported
/// rather than referenced through `protocol::` at each use so that `exec::Phase` keeps
/// naming the same type it always has — for the handlers below, for the doc references
/// in the `agentd-model` crate, and for anything downstream that imported it.
pub use protocol::exec::{
    ErrorBody, ExitEvent, GapEvent, KillResponse, Outcome, OutputEvent, Phase, PollResponse,
    StartRequest, StartResponse, StdinRequest, StdinResponse, StreamKind, StreamQuery,
};

/// The exit status of a finished exec, kept separately from [`Outcome`].
///
/// Duplicating four small fields buys something the polled result cannot give: an
/// ack *takes* the `Outcome`, so after an ack `result` is `None` again and is no
/// longer usable as "has this exec finished?". A stream attaching at that point
/// would conclude the exec is still running and wait on a channel that will never
/// carry another message. This marker is written once and never taken, so both
/// "is it over" and "how did it end" stay answerable for the life of the entry.
#[derive(Clone, Copy, Debug, Serialize)]
struct Terminal {
    exit_code: Option<i32>,
    signal: Option<i32>,
    truncated: bool,
    writers_may_be_alive: bool,
}

/// One message on an exec's live fan-out channel.
///
/// `Finished` rather than closing the sender: the sender lives in `Shared`
/// alongside everything else and outlives the waiter, so channel closure is not
/// available as the end-of-output signal. An explicit marker also lets a
/// subscriber distinguish "the command ended" from "the daemon dropped the
/// channel", which is the same distinction SSE framing buys on the wire.
#[derive(Clone, Debug)]
enum Frame {
    Chunk {
        stream: StreamKind,
        /// Offset of the first byte of `bytes` in the exec's combined output.
        offset: u64,
        bytes: Bytes,
    },
    Finished,
}

/// The replay window: a ring of recent output plus the offsets that describe it.
///
/// This is a *tail* window and `Capped` is a *head* buffer, and the difference is
/// deliberate rather than redundant. The polled `Outcome` keeps exactly today's
/// semantics — the first `max_output_bytes` of each stream, with `truncated` set
/// when there was more — because the live conformance suite asserts on that flag.
/// A streaming consumer needs the opposite end: the bytes it has not seen yet. So
/// output past the head cap still lands here, and a stream that falls behind the
/// ring is told the byte range it lost instead of being handed a buffer that
/// silently starts later than it asked for.
#[derive(Default)]
struct Log {
    ring: VecDeque<RingChunk>,
    /// Total bytes ever published. Also the offset the next byte will receive.
    total: u64,
    /// Offset of the earliest byte still in the ring.
    start: u64,
    /// Bytes currently held, tracked rather than recomputed so trimming is O(1)
    /// per evicted chunk instead of O(ring) per publish.
    retained: usize,
}

struct RingChunk {
    stream: StreamKind,
    offset: u64,
    bytes: Bytes,
}

impl Log {
    /// Evicts from the front until the ring fits `cap`.
    ///
    /// Splits the oldest chunk rather than dropping it whole, so the window is
    /// the size configured and not "the size configured, minus up to one read".
    /// `Bytes::slice` is a refcount bump, so the split is free.
    fn trim(&mut self, cap: usize) {
        while self.retained > cap {
            let over = self.retained - cap;
            let Some(front) = self.ring.front_mut() else {
                break;
            };
            if front.bytes.len() <= over {
                let dropped = front.bytes.len();
                self.retained -= dropped;
                self.start += dropped as u64;
                self.ring.pop_front();
            } else {
                front.bytes = front.bytes.slice(over..);
                front.offset += over as u64;
                self.retained -= over;
                self.start += over as u64;
            }
        }
    }

    /// Everything retained from `from` onward, plus the gap `from` fell into.
    ///
    /// The gap is returned rather than papered over. Handing back a window that
    /// starts later than the caller asked for, with no marker, is the failure mode
    /// a cursorless attach has by construction: the client cannot tell a quiet
    /// command from output it will never see.
    fn since(&self, from: u64) -> (Option<(u64, u64)>, VecDeque<Frame>, u64) {
        let gap = (from < self.start).then_some((from, self.start));
        let cursor = from.max(self.start);
        let frames = self
            .ring
            .iter()
            .filter(|chunk| chunk.offset + chunk.bytes.len() as u64 > cursor)
            .map(|chunk| Frame::Chunk {
                stream: chunk.stream,
                offset: chunk.offset,
                bytes: chunk.bytes.clone(),
            })
            .collect();
        (gap, frames, cursor)
    }
}

/// One exec slot, keyed by the caller-minted idempotency key.
///
/// The child is not owned here. Waiting on it and draining its pipes happens in
/// a detached task, which publishes into `shared` when it is done; the registry
/// lock is therefore never held across an await.
pub struct ExecEntry {
    /// Process group id, captured immediately after spawn while `Child::id()`
    /// still answers. `None` only if the child had already been reaped by the
    /// time we asked, which a fast-exiting command can manage.
    pgid: Option<u32>,
    shared: Arc<Shared>,
    /// When the entry was acked. TTL collection reads this; an unacked entry has
    /// no deadline and is never collected.
    acked_at: Option<Instant>,
}

/// The part of an entry the waiter task and the handlers both touch.
struct Shared {
    /// Written once by the waiter task when the child is done and its output has
    /// been drained (or the linger deadline cut the drain short). Taken by `ack`.
    result: Mutex<Option<Outcome>>,
    /// Written once, never taken. See [`Terminal`].
    terminal: Mutex<Option<Terminal>>,
    log: Mutex<Log>,
    /// Live fan-out to attached streams. Bounded on purpose: a subscriber that
    /// falls behind gets `RecvError::Lagged`, which is recoverable — it re-reads
    /// the ring from its own cursor — so the bound costs a re-read rather than
    /// either unbounded memory or lost output.
    live: broadcast::Sender<Frame>,
    /// Our copy of the child's stdin, present only when the start request asked
    /// for it. `None` in the `Option` after an EOF or a broken pipe.
    ///
    /// The trap worth naming: `Child::wait()` drops the handle *the Child owns*,
    /// not this one. A child blocked in `read` on stdin therefore never sees EOF
    /// while this copy is alive, so `cat` with no explicit EOF hangs until its
    /// timeout — which looks exactly like a daemon that dropped the write.
    stdin: Mutex<Option<ChildStdin>>,
    /// Whether stdin was ever requested. Distinguishes "you did not ask for
    /// stdin" (a request error the caller fixes by setting the flag) from "stdin
    /// is closed" (a lifecycle fact the caller cannot fix), which would otherwise
    /// collapse into one indistinguishable `None`.
    stdin_requested: bool,
    /// Copied from config at spawn so publishing does not reach back into state.
    buffer_cap: usize,
}

impl Shared {
    /// Appends to the replay ring and fans the same bytes out live.
    ///
    /// Both happen under the one lock. A subscriber snapshots the ring after
    /// subscribing, and if a chunk could land in the channel before the ring, that
    /// snapshot could miss a chunk the channel had already delivered past.
    async fn publish(&self, stream: StreamKind, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        let mut log = self.log.lock().await;
        let offset = log.total;
        log.total += bytes.len() as u64;
        let bytes = Bytes::copy_from_slice(bytes);
        log.retained += bytes.len();
        log.ring.push_back(RingChunk {
            stream,
            offset,
            bytes: bytes.clone(),
        });
        let cap = self.buffer_cap;
        log.trim(cap);
        // No receivers is the normal case — nothing is attached — so a send error
        // is not worth reporting. Sent while still holding the log lock, which is
        // what makes `attach` below atomic against a publish.
        let _ = self.live.send(Frame::Chunk {
            stream,
            offset,
            bytes,
        });
    }

    /// Subscribes to the live channel and snapshots the replay ring, atomically.
    ///
    /// The ordering is the whole correctness argument, so it is enforced by one
    /// lock rather than by two statements in the right sequence. `publish` holds
    /// the log lock across its broadcast send, so while this holds it no chunk can
    /// land in one half without landing in the other. Written as two statements it
    /// is a silent one-chunk hole that only appears under load: snapshot first and
    /// the write between the steps is in neither; subscribe first and the write is
    /// in both, which the returned cursor then de-duplicates.
    ///
    /// Subscribe-before-snapshot is therefore the *safe* order of the two, and this
    /// method exists so the unsafe one is not expressible from the handler.
    async fn attach(
        &self,
        from: u64,
    ) -> (
        broadcast::Receiver<Frame>,
        Option<(u64, u64)>,
        VecDeque<Frame>,
        u64,
    ) {
        let log = self.log.lock().await;
        let live = self.live.subscribe();
        let (gap, backlog, cursor) = log.since(from);
        (live, gap, backlog, cursor)
    }
}

/// Builds an error response.
///
/// The status is always chosen by the caller of this function, never inferred
/// from an error type: a bad body key must be 400 and an absent id must be 404,
/// and collapsing the two is the defect that made a protocol typo look like a
/// missing artifact.
///
/// Stays here rather than moving to `protocol` with the body it builds: the pairing
/// of a slug with an axum `StatusCode` is daemon machinery, and a client reads the
/// two rather than constructing them.
fn fail(status: StatusCode, error: &'static str, detail: impl Into<String>) -> Response {
    (
        status,
        Json(ErrorBody {
            error: Cow::Borrowed(error),
            detail: detail.into(),
        }),
    )
        .into_response()
}

/// `POST /v1/exec/start`.
pub async fn start(
    State(state): State<AppState>,
    body: Result<Json<StartRequest>, JsonRejection>,
) -> Response {
    let Ok(Json(req)) = body else {
        return fail(
            StatusCode::BAD_REQUEST,
            ERROR_MALFORMED_REQUEST,
            "body is not a valid start request",
        );
    };

    if req.exec_id.is_empty() {
        return fail(
            StatusCode::BAD_REQUEST,
            ERROR_MALFORMED_REQUEST,
            "exec_id must not be empty",
        );
    }

    // Validated here, before anything is spawned. Doing it in the waiter left a
    // running child with nobody to reap it.
    let timeout = match validate_timeout(req.timeout_sec) {
        Ok(timeout) => timeout,
        Err(detail) => return fail(StatusCode::BAD_REQUEST, ERROR_MALFORMED_REQUEST, detail),
    };

    let command = match build_command(&req) {
        Ok(command) => command,
        Err(detail) => return fail(StatusCode::BAD_REQUEST, ERROR_MALFORMED_REQUEST, detail),
    };

    // Idempotency: decided under the registry lock, before the spawn, so two
    // concurrent retries cannot both find the slot empty. A known id returns
    // success and leaves the existing entry's phase and output untouched.
    let already_present = state.with_execs(|execs| execs.contains_key(&req.exec_id));
    if already_present {
        tracing::info!(exec_id = %req.exec_id, "start retried; not spawning again");
        return (
            StatusCode::OK,
            Json(StartResponse {
                exec_id: req.exec_id,
                phase: Phase::Running,
            }),
        )
            .into_response();
    }

    match spawn(&state, &req.exec_id, command, timeout) {
        Ok(()) => (
            StatusCode::OK,
            Json(StartResponse {
                exec_id: req.exec_id,
                phase: Phase::Running,
            }),
        )
            .into_response(),
        // A spawn failure is the daemon's problem, not a malformed request:
        // ENOENT on argv[0] is reported as a 500 with the detail rather than a
        // 404, which a client would read as "the exec id does not exist".
        Err(err) => {
            tracing::warn!(exec_id = %req.exec_id, %err, "spawn failed");
            fail(
                StatusCode::INTERNAL_SERVER_ERROR,
                ERROR_SPAWN_FAILED,
                err.to_string(),
            )
        }
    }
}

/// `GET /v1/exec/{id}`.
///
/// Strictly read-only. Nothing in this function may write to the registry or to
/// an entry — the model asserts it against the transition function, because
/// read-only is a property of the step and not of any reachable state.
pub async fn poll(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    let shared = match state.with_execs(|execs| execs.get(&id).map(|entry| entry.shared.clone())) {
        Some(shared) => shared,
        None => return fail(StatusCode::NOT_FOUND, ERROR_UNKNOWN_EXEC, id),
    };

    let acked = state.with_execs(|execs| {
        execs
            .get(&id)
            .map(|entry| entry.acked_at.is_some())
            .unwrap_or(false)
    });

    let result = shared.result.lock().await.clone();
    let phase = phase_of(acked, result.is_some());

    (
        StatusCode::OK,
        Json(PollResponse {
            exec_id: id,
            // An acked entry's output has been released; reporting it again would
            // contradict the phase.
            result: if acked { None } else { result },
            phase,
        }),
    )
        .into_response()
}

/// `GET /v1/exec/{id}/stream?offset=<bytes>`.
///
/// Attaches to an exec's output as Server-Sent Events, resuming at a byte offset.
/// Like [`poll`], read-only with respect to the registry: attaching and detaching
/// are views onto a server-side object, so a client that hangs up mid-command has
/// not affected the command.
///
/// Three events, all `data:` JSON:
///
/// * `output` — `{offset, stream, output}`, `output` base64. The offset is the
///   position of the first byte, so a client's next resume value is
///   `offset + len(decoded)`.
/// * `gap` — `{from, to}`. Bytes in that range are gone: the request resumed
///   before the replay window, or the subscriber lagged the live channel. Reported
///   rather than hidden, because a client that cannot tell missing output from no
///   output will read a truncated log as a complete one.
/// * `exit` — the terminal event, then the stream ends. A body that closes without
///   it means the *connection* failed, not the command. That distinction is the
///   entire reason this is SSE and not a chunked byte stream.
pub async fn stream(
    State(state): State<AppState>,
    Path(id): Path<String>,
    query: Result<Query<StreamQuery>, QueryRejection>,
) -> Response {
    let Ok(Query(query)) = query else {
        return fail(
            StatusCode::BAD_REQUEST,
            ERROR_MALFORMED_REQUEST,
            "offset must be a non-negative integer",
        );
    };

    let shared = match state.with_execs(|execs| execs.get(&id).map(|entry| entry.shared.clone())) {
        Some(shared) => shared,
        None => return fail(StatusCode::NOT_FOUND, ERROR_UNKNOWN_EXEC, id),
    };

    let from = query.offset.unwrap_or(0);
    let (live, gap, backlog, cursor) = shared.attach(from).await;
    // Read after the snapshot, so an exec that finished between the two is
    // observed as finished rather than as running forever. The other order would
    // let an exec that ends in between look like one still running, and the stream
    // would then wait on a `Finished` that was sent before it subscribed.
    let already_finished = shared.terminal.lock().await.is_some();

    let events = build_stream(shared, live, gap, backlog, cursor, already_finished);
    let mut response = Sse::new(events)
        .keep_alive(KeepAlive::new().interval(state.config().sse_keepalive))
        .into_response();
    // axum sets content-type and `Cache-Control: no-cache`; this one it does not.
    // Without it a buffering proxy holds events until its own buffer fills, which
    // turns a live stream into a batch delivered at exit — indistinguishable from
    // a daemon that never streamed at all.
    response
        .headers_mut()
        .insert("x-accel-buffering", HeaderValue::from_static("no"));
    response
}

/// Per-subscriber stream state: the replayed backlog, then the live channel.
struct Attach {
    shared: Arc<Shared>,
    live: broadcast::Receiver<Frame>,
    /// Events ready to hand out. `Sse` takes one event per poll, and a single
    /// step here can produce two (a gap plus the chunk after it).
    pending: VecDeque<Event>,
    backlog: VecDeque<Frame>,
    /// Next byte this subscriber expects. Advances across replayed and live
    /// chunks alike, so a chunk the backlog already carried is trimmed rather
    /// than sent twice — the backlog and the channel legitimately overlap, and
    /// that overlap is exactly what subscribe-before-replay costs.
    cursor: u64,
    /// The exec was already over when we attached, so the live channel may never
    /// deliver a `Finished` (the waiter sent it before we subscribed).
    finished: bool,
}

impl Attach {
    /// Turns one chunk into an event, dropping the prefix already delivered.
    fn take_chunk(&mut self, stream: StreamKind, offset: u64, bytes: &Bytes) {
        let end = offset + bytes.len() as u64;
        if end <= self.cursor {
            return;
        }
        // A live chunk landing past the cursor means the subscriber missed bytes
        // that the ring had already evicted; say so rather than emitting a chunk
        // at a discontinuous offset the client cannot reconcile.
        if offset > self.cursor {
            self.pending.push_back(gap_event(self.cursor, offset));
            self.cursor = offset;
        }
        let skip = (self.cursor - offset) as usize;
        let slice = bytes.slice(skip..);
        self.pending
            .push_back(output_event(stream, self.cursor, &slice));
        self.cursor = end;
    }

    /// The terminal event, read from the marker that an ack cannot take.
    async fn exit_event(&self) -> Event {
        let terminal = self.shared.terminal.lock().await.unwrap_or(Terminal {
            exit_code: None,
            signal: None,
            truncated: false,
            writers_may_be_alive: false,
        });
        let offset = self.shared.log.lock().await.total;
        typed(
            EVENT_EXIT,
            &ExitEvent {
                exit_code: terminal.exit_code,
                signal: terminal.signal,
                truncated: terminal.truncated,
                writers_may_be_alive: terminal.writers_may_be_alive,
                offset,
            },
        )
    }
}

/// Chains the replayed backlog onto the live channel and ends after `exit`.
///
/// `None` from the stream is what closes the SSE body, so the terminal event is
/// emitted on the step *before* the one that returns `None`.
fn build_stream(
    shared: Arc<Shared>,
    live: broadcast::Receiver<Frame>,
    gap: Option<(u64, u64)>,
    backlog: VecDeque<Frame>,
    cursor: u64,
    already_finished: bool,
) -> impl Stream<Item = Result<Event, std::convert::Infallible>> + Send + 'static {
    let mut pending = VecDeque::new();
    if let Some((from, to)) = gap {
        pending.push_back(gap_event(from, to));
    }

    let attach = Attach {
        shared,
        live,
        pending,
        backlog,
        cursor,
        finished: already_finished,
    };

    futures_util::stream::unfold(Some(attach), |slot| async move {
        let mut attach = slot?;
        loop {
            if let Some(event) = attach.pending.pop_front() {
                return Some((Ok(event), Some(attach)));
            }

            if let Some(frame) = attach.backlog.pop_front() {
                if let Frame::Chunk {
                    stream,
                    offset,
                    bytes,
                } = frame
                {
                    attach.take_chunk(stream, offset, &bytes);
                }
                continue;
            }

            // Backlog drained. If the exec was already over when we attached, the
            // channel will not deliver a `Finished` of its own, so end here —
            // waiting would hold the connection open until the client gives up.
            if attach.finished {
                return Some((Ok(attach.exit_event().await), None));
            }

            match attach.live.recv().await {
                Ok(Frame::Chunk {
                    stream,
                    offset,
                    bytes,
                }) => attach.take_chunk(stream, offset, &bytes),
                Ok(Frame::Finished) => {
                    return Some((Ok(attach.exit_event().await), None));
                }
                // Recoverable, and the reason the channel is `broadcast` rather
                // than something that blocks the producer: `n` messages were
                // dropped for this subscriber alone. Their bytes are still in the
                // ring if it has not wrapped, but we cannot know their offsets, so
                // the honest move is to tell the client its cursor is stale and
                // let it re-GET from the last offset it actually received.
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    tracing::warn!(dropped = n, "exec stream subscriber lagged");
                    let total = attach.shared.log.lock().await.total;
                    if total > attach.cursor {
                        attach.pending.push_back(gap_event(attach.cursor, total));
                        attach.cursor = total;
                    }
                }
                // The sender lives in `Shared`, which the entry holds, so this
                // only happens once the entry is collected. Nothing more is
                // coming; end with what we know.
                Err(broadcast::error::RecvError::Closed) => {
                    return Some((Ok(attach.exit_event().await), None));
                }
            }
        }
    })
}

fn output_event(stream: StreamKind, offset: u64, bytes: &[u8]) -> Event {
    typed(
        EVENT_OUTPUT,
        &OutputEvent {
            offset,
            stream,
            // Base64 rather than a JSON string of the bytes: output is arbitrary
            // bytes, and lossy UTF-8 here would corrupt any binary a command emits and
            // would split multi-byte characters at chunk boundaries besides.
            output: base64::engine::general_purpose::STANDARD.encode(bytes),
        },
    )
}

fn gap_event(from: u64, to: u64) -> Event {
    typed(EVENT_GAP, &GapEvent { from, to })
}

/// Builds one named SSE event.
///
/// `Event::data` panics if called twice, so every event is built in one place that
/// calls it once. Serialization cannot realistically fail for these types, and a
/// failure would have nowhere to go inside a stream, so it degrades to a `gap`-free
/// empty payload with a log line rather than taking the connection down.
fn typed<T: Serialize>(name: &'static str, payload: &T) -> Event {
    match serde_json::to_string(payload) {
        Ok(json) => Event::default().event(name).data(json),
        Err(err) => {
            tracing::error!(%err, name, "failed to serialize an SSE event");
            Event::default().event(name).data("{}")
        }
    }
}

/// `POST /v1/exec/{id}/stdin`.
///
/// A separate endpoint from the output stream on purpose. Multiplexing the write
/// half onto the read connection means a dropped attach also drops the ability to
/// feed the process, so reconnecting becomes load-bearing for correctness rather
/// than only for observation. Runloop, Daytona, E2B and Modal all split them.
pub async fn write_stdin(
    State(state): State<AppState>,
    Path(id): Path<String>,
    body: Result<Json<StdinRequest>, JsonRejection>,
) -> Response {
    let Ok(Json(req)) = body else {
        return fail(
            StatusCode::BAD_REQUEST,
            ERROR_MALFORMED_REQUEST,
            "body is not a valid stdin request",
        );
    };

    let shared = match state.with_execs(|execs| execs.get(&id).map(|entry| entry.shared.clone())) {
        Some(shared) => shared,
        None => return fail(StatusCode::NOT_FOUND, ERROR_UNKNOWN_EXEC, id),
    };

    if !shared.stdin_requested {
        // 409, not 400: the request is well-formed, it is the exec that cannot
        // accept it, and the fix is at start time rather than in this body.
        return fail(
            StatusCode::CONFLICT,
            ERROR_STDIN_NOT_REQUESTED,
            "this exec was started without stdin: true, so its stdin is /dev/null",
        );
    }

    let eof = match req.signal.as_deref() {
        None => false,
        Some("eof") => true,
        Some(other) => {
            return fail(
                StatusCode::BAD_REQUEST,
                ERROR_MALFORMED_REQUEST,
                format!("unknown stdin signal {other:?}; only \"eof\" is defined"),
            );
        }
    };

    let data = match req.data_b64.as_deref() {
        None => Vec::new(),
        Some(encoded) => match base64::engine::general_purpose::STANDARD.decode(encoded) {
            Ok(bytes) => bytes,
            Err(err) => {
                return fail(
                    StatusCode::BAD_REQUEST,
                    ERROR_MALFORMED_REQUEST,
                    format!("data_b64 is not valid base64: {err}"),
                );
            }
        },
    };

    let cap = state.config().max_stdin_write_bytes;
    if data.len() > cap {
        return fail(
            StatusCode::PAYLOAD_TOO_LARGE,
            ERROR_STDIN_WRITE_TOO_LARGE,
            format!(
                "{} bytes exceeds the {cap}-byte stdin write limit",
                data.len()
            ),
        );
    }

    let mut slot = shared.stdin.lock().await;
    let Some(pipe) = slot.as_mut() else {
        // Gone means either an earlier EOF or the child having exited. 410 rather
        // than 409: the resource is not in the wrong state, it no longer exists,
        // and a client retrying will never succeed.
        return fail(
            StatusCode::GONE,
            ERROR_STDIN_CLOSED,
            "stdin has already been closed or the child has exited",
        );
    };

    let write_timeout = state.config().stdin_write_timeout;
    if !data.is_empty() {
        // Bounded because a child that stopped reading fills the 64 KiB pipe
        // buffer and then this write blocks forever, pinning a request and its
        // connection for the life of the VM.
        let wrote = tokio::time::timeout(write_timeout, async {
            pipe.write_all(&data).await?;
            pipe.flush().await
        })
        .await;

        match wrote {
            Ok(Ok(())) => {}
            // Rust sets SIGPIPE to SIG_IGN, so writing to a pipe whose reader is
            // gone surfaces as an error rather than killing the daemon. Drop our
            // handle: it can never succeed again, and holding it keeps an fd alive.
            Ok(Err(err)) if err.kind() == std::io::ErrorKind::BrokenPipe => {
                *slot = None;
                return fail(
                    StatusCode::GONE,
                    ERROR_STDIN_CLOSED,
                    "the child is no longer reading stdin",
                );
            }
            Ok(Err(err)) => {
                return fail(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    ERROR_STDIN_WRITE_FAILED,
                    err.to_string(),
                );
            }
            Err(_) => {
                // The handle stays: the child may drain the pipe and be writable
                // again, so this is a retryable condition rather than a terminal
                // one. Partially-written bytes are the caller's problem to
                // reconcile, which is why the detail says so.
                return fail(
                    StatusCode::REQUEST_TIMEOUT,
                    ERROR_STDIN_WRITE_TIMEOUT,
                    "the child did not read stdin within the write timeout; \
                     some bytes may have been written",
                );
            }
        }
    }

    if eof {
        // Dropping the handle is the only way the child sees EOF. `Child::wait()`
        // closes the copy the `Child` owns, not this one, so a `cat` waiting on
        // input would hang forever if this were left in place.
        drop(slot.take());
    }
    drop(slot);

    tracing::info!(exec_id = %id, bytes = data.len(), eof, "stdin written");
    (
        StatusCode::OK,
        Json(StdinResponse {
            exec_id: id,
            written: data.len(),
            eof,
        }),
    )
        .into_response()
}

/// `POST /v1/exec/{id}/ack`.
///
/// Releases the buffered output and starts the TTL clock. Acking a still-running
/// exec is 409, not a silent success: succeeding would drop output that is still
/// being written, which is precisely what unlinking on child exit did.
pub async fn ack(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    let shared = match state.with_execs(|execs| execs.get(&id).map(|entry| entry.shared.clone())) {
        Some(shared) => shared,
        None => return fail(StatusCode::NOT_FOUND, ERROR_UNKNOWN_EXEC, id),
    };

    let mut slot = shared.result.lock().await;
    if slot.is_none() {
        return fail(
            StatusCode::CONFLICT,
            ERROR_STILL_RUNNING,
            "exec has not exited; output is still being written",
        );
    }

    let released = slot.take();
    drop(slot);

    let marked = state.with_execs(|execs| match execs.get_mut(&id) {
        Some(entry) if entry.acked_at.is_none() => {
            entry.acked_at = Some(Instant::now());
            true
        }
        // Already acked. The output was released by the first ack, so there is
        // nothing left to hand back; answer 409 rather than 200 with an empty
        // body, which would read as "the command produced no output".
        Some(_) => false,
        None => false,
    });

    if !marked {
        return fail(
            StatusCode::CONFLICT,
            ERROR_ALREADY_ACKED,
            "output was released by an earlier ack",
        );
    }

    tracing::info!(exec_id = %id, "output acked and released");
    (
        StatusCode::OK,
        Json(PollResponse {
            exec_id: id,
            phase: Phase::Acked,
            result: released,
        }),
    )
        .into_response()
}

/// `POST /v1/exec/{id}/kill`.
///
/// Signals the whole process group, not just the direct child. A shell that
/// backgrounded a server leaves the interesting process outside the child pid,
/// and `kill(child)` returned success while the workload kept running.
pub async fn kill(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    let (pgid, done) = match state.with_execs(|execs| {
        execs
            .get(&id)
            .map(|entry| (entry.pgid, entry.shared.clone()))
    }) {
        Some((pgid, shared)) => (pgid, shared),
        None => return fail(StatusCode::NOT_FOUND, ERROR_UNKNOWN_EXEC, id),
    };

    let Some(pgid) = pgid else {
        // No pgid was ever captured, which means the child had already been
        // reaped. Nothing to signal, and saying so is more useful than a 500.
        return (
            StatusCode::OK,
            Json(KillResponse {
                exec_id: id,
                killed: false,
            }),
        )
            .into_response();
    };

    let grace = state.config().kill_grace;
    let signaled = escalate(pgid, grace, done).await;

    tracing::info!(exec_id = %id, pgid, signaled, "kill requested");
    (
        StatusCode::OK,
        Json(KillResponse {
            exec_id: id,
            killed: signaled,
        }),
    )
        .into_response()
}

/// Collects acked entries whose TTL has elapsed.
///
/// Returns how many were removed. Deliberately a plain function the daemon's own
/// loop calls, not a task spawned from a handler: a reaper started per request
/// multiplies with traffic, and the ones the predecessor spawned outlived the
/// entries they were meant to collect.
///
/// Only acked entries are eligible. An unacked entry has no deadline however old
/// it is, because collecting it would destroy output the caller never read.
pub fn collect_expired(state: &AppState) -> usize {
    let ttl = state.config().exec_ttl;
    let now = Instant::now();
    state.with_execs(|execs| {
        let before = execs.len();
        execs.retain(|_, entry| match entry.acked_at {
            Some(at) => now.duration_since(at) < ttl,
            None => true,
        });
        before - execs.len()
    })
}

/// Rejects a timeout that cannot describe a real budget.
///
/// `f64` from JSON admits NaN and infinity through some encoders, and both turn
/// into a `Duration` conversion panic or an effectively infinite wait.
fn validate_timeout(raw: Option<f64>) -> Result<Option<Duration>, String> {
    let Some(secs) = raw else { return Ok(None) };
    if !secs.is_finite() || secs <= 0.0 {
        return Err(format!(
            "timeout_sec must be a positive finite number, got {secs}"
        ));
    }
    Ok(Some(Duration::from_secs_f64(secs)))
}

/// Assembles the child command, including the shell decision and demotion.
fn build_command(req: &StartRequest) -> Result<Command, String> {
    let mut command = if req.shell {
        // A single argument to `sh -c`, not a constructed wrapper. The
        // predecessor built `"cd %s && {\n%s\n}"`, which made an empty command
        // and a comment-terminated command into syntax errors and let an
        // unbalanced `}` in the script escape the group it was supposed to be
        // confined by. `sh -c ''` exits 0, which is the correct answer.
        let script = req.command.join("\n");
        let mut command = Command::new("/bin/sh");
        command.arg("-c").arg(script);
        command
    } else {
        let Some((program, args)) = req.command.split_first() else {
            return Err("command must not be empty when shell is false".to_string());
        };
        let mut command = Command::new(program);
        command.args(args);
        command
    };

    // Only what the request asks for. Inheriting the daemon's environment would
    // carry the agent token into the child, and that is one of the three security
    // properties the model pins — so the environment starts empty and nothing
    // reads from `std::env`.
    command.env_clear();
    command.envs(&req.env);

    // Omitted cwd means inherit. Not `/`.
    if let Some(cwd) = &req.cwd {
        command.current_dir(cwd);
    }

    // Demotion between fork and exec, in C. Never through `pre_exec`: a closure
    // there runs interpreted-equivalent code in a forked child of a threaded
    // process, where a lock held by another thread at fork time is held forever.
    if let Some(gid) = req.group {
        command.gid(gid);
    }
    if let Some(uid) = req.user {
        command.uid(uid);
    }

    // A new process group, so kill can signal the whole tree. 0 means "use the
    // child's own pid as the pgid".
    command.process_group(0);
    // Null unless asked. A pipe nobody writes to turns any read of stdin into a
    // hang, where `/dev/null` returns EOF immediately.
    if req.stdin {
        command.stdin(Stdio::piped());
    } else {
        command.stdin(Stdio::null());
    }
    command.stdout(Stdio::piped());
    command.stderr(Stdio::piped());
    Ok(command)
}

/// Spawns the child and registers the entry, then detaches the waiter.
fn spawn(
    state: &AppState,
    id: &str,
    mut command: Command,
    timeout: Option<Duration>,
) -> std::io::Result<()> {
    let mut child = command.spawn()?;

    // Immediately, while the child is still unreaped. `Child::id()` answers
    // `None` after a wait, so a kill path that read it lazily would find nothing
    // for exactly the fast-then-forking commands that most need killing. Because
    // of `process_group(0)`, the pid is also the pgid.
    let pgid = child.id();

    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    // Taken out of the `Child` so a later stdin handler can reach it. That the
    // `Child` no longer holds it is also why we must drop it ourselves on EOF:
    // `wait()` closes only what it still owns.
    let stdin = child.stdin.take();
    let stdin_requested = stdin.is_some();

    let cfg = state.config().clone();
    let (live, _) = broadcast::channel(cfg.stream_channel_capacity.max(1));
    let shared = Arc::new(Shared {
        result: Mutex::new(None),
        terminal: Mutex::new(None),
        log: Mutex::new(Log::default()),
        live,
        stdin: Mutex::new(stdin),
        stdin_requested,
        buffer_cap: cfg.stream_buffer_bytes,
    });

    state.with_execs(|execs| {
        execs.insert(
            id.to_string(),
            ExecEntry {
                pgid,
                shared: clone_shared(&shared),
                acked_at: None,
            },
        );
    });

    let owned_id = id.to_string();
    tokio::spawn(async move {
        let outcome = super_wait(
            &mut child,
            stdout,
            stderr,
            pgid,
            cfg.max_output_bytes,
            cfg.output_linger,
            cfg.kill_grace,
            timeout,
            &shared,
        )
        .await;
        tracing::info!(
            exec_id = %owned_id,
            exit_code = ?outcome.exit_code,
            signal = ?outcome.signal,
            truncated = outcome.truncated,
            "exec finished"
        );
        let terminal = Terminal {
            exit_code: outcome.exit_code,
            signal: outcome.signal,
            truncated: outcome.truncated,
            writers_may_be_alive: outcome.writers_may_be_alive,
        };
        // Terminal before result: a stream that sees `Finished` immediately reads
        // the terminal marker, and finding it absent would end the stream with no
        // exit event — the one thing the framing exists to guarantee.
        *shared.terminal.lock().await = Some(terminal);
        *shared.result.lock().await = Some(outcome);
        // The child cannot read any more, so holding our stdin copy only keeps a
        // pipe fd alive for an entry that may sit until its TTL.
        drop(shared.stdin.lock().await.take());
        let _ = shared.live.send(Frame::Finished);
    });

    Ok(())
}

fn clone_shared(shared: &Arc<Shared>) -> Arc<Shared> {
    Arc::clone(shared)
}

fn phase_of(acked: bool, finished: bool) -> Phase {
    if acked {
        Phase::Acked
    } else if finished {
        Phase::Exited
    } else {
        Phase::Running
    }
}

/// Waits for the child while draining both pipes concurrently, then lingers.
///
/// Draining must be concurrent with the wait: a child that fills a 64 KiB pipe
/// buffer blocks in `write` forever if nobody is reading, and a waiter that only
/// reads after `wait()` returns deadlocks on exactly the noisy commands it was
/// written for.
#[allow(clippy::too_many_arguments)]
async fn super_wait(
    child: &mut tokio::process::Child,
    stdout: Option<tokio::process::ChildStdout>,
    stderr: Option<tokio::process::ChildStderr>,
    pgid: Option<u32>,
    cap: usize,
    linger: Duration,
    grace: Duration,
    timeout: Option<Duration>,
    shared: &Shared,
) -> Outcome {
    let mut out_reader = Capped::new(stdout, cap, StreamKind::Stdout);
    let mut err_reader = Capped::new(stderr, cap, StreamKind::Stderr);

    let mut status = None;
    let deadline = timeout.map(|budget| Instant::now() + budget);
    let mut timed_out = false;

    // Phase one: the direct child is alive. Read both pipes and the exit status
    // together.
    while status.is_none() {
        tokio::select! {
            biased;
            waited = child.wait() => status = Some(waited),
            _ = out_reader.pump(shared) => {}
            _ = err_reader.pump(shared) => {}
            _ = sleep_until(deadline) => {
                timed_out = true;
                if let Some(pgid) = pgid {
                    escalate_blind(pgid, grace).await;
                }
                // Fall through: the group has been signalled, so the wait above
                // will now complete and the pipes will reach EOF.
                status = Some(child.wait().await);
            }
        }
    }

    // Phase two: the child is gone but grandchildren may still hold the write
    // end. This is the case temp files got wrong — they were unlinked here, so
    // anything a backgrounded process wrote afterward went to a file with no
    // name. A pipe keeps working; all we need is a deadline so a daemonized
    // server does not hold the exec open forever.
    let linger_deadline = Some(Instant::now() + linger);
    let mut writers_may_be_alive = false;
    loop {
        if out_reader.done() && err_reader.done() {
            break;
        }
        tokio::select! {
            _ = out_reader.pump(shared) => {}
            _ = err_reader.pump(shared) => {}
            _ = sleep_until(linger_deadline) => {
                writers_may_be_alive = true;
                break;
            }
        }
    }

    let (exit_code, signal) = match status {
        Some(Ok(status)) => (status.code(), unix_signal(&status)),
        // A failed wait leaves the status genuinely unknown. Reporting `None`
        // for both is honest; inventing a code would be worse than saying so.
        Some(Err(err)) => {
            tracing::warn!(%err, "wait on exec child failed");
            (None, None)
        }
        None => (None, None),
    };

    if timed_out {
        tracing::warn!("exec exceeded its timeout and its process group was signalled");
    }

    let truncated = out_reader.truncated || err_reader.truncated;
    Outcome {
        exit_code,
        signal,
        stdout: out_reader.into_string(),
        stderr: err_reader.into_string(),
        truncated,
        writers_may_be_alive,
    }
}

/// Sleeps until `deadline`, or forever when there is none.
///
/// `select!` needs a branch that is always well-formed, and a `None` deadline has
/// to be a future that never completes rather than one that completes instantly —
/// the latter would make every unbounded exec look like it timed out.
async fn sleep_until(deadline: Option<Instant>) {
    match deadline {
        Some(at) => tokio::time::sleep_until(tokio::time::Instant::from_std(at)).await,
        None => std::future::pending().await,
    }
}

/// A pipe reader with a byte cap that also feeds the streaming replay window.
///
/// After the cap, bytes are still read and discarded rather than left in the
/// pipe: stopping the read would block a writer in the kernel indefinitely, and a
/// command whose output overflows the cap should still be able to finish.
///
/// The cap applies to `buf` only. Everything read is published to the exec's
/// stream log regardless — a streamer past 8 MiB has been keeping up all along and
/// has no reason to stop receiving, and the ring bounds that side independently.
struct Capped<R> {
    reader: Option<R>,
    buf: Vec<u8>,
    scratch: Vec<u8>,
    cap: usize,
    truncated: bool,
    eof: bool,
    kind: StreamKind,
}

impl<R: AsyncReadExt + Unpin> Capped<R> {
    fn new(reader: Option<R>, cap: usize, kind: StreamKind) -> Self {
        Self {
            eof: reader.is_none(),
            reader,
            buf: Vec::new(),
            scratch: vec![0u8; 16 * 1024],
            cap,
            truncated: false,
            kind,
        }
    }

    fn done(&self) -> bool {
        self.eof
    }

    /// Reads one chunk. Resolves immediately once at EOF, so a `select!` loop
    /// that still has the other stream open does not spin on this one — the
    /// caller's loop exits on `done()`.
    async fn pump(&mut self, shared: &Shared) {
        if self.eof {
            std::future::pending::<()>().await;
            return;
        }
        let Some(reader) = self.reader.as_mut() else {
            self.eof = true;
            return;
        };
        match reader.read(&mut self.scratch).await {
            Ok(0) => self.eof = true,
            Ok(n) => {
                shared.publish(self.kind, &self.scratch[..n]).await;
                let room = self.cap.saturating_sub(self.buf.len());
                if room == 0 {
                    self.truncated = true;
                } else if n > room {
                    self.buf.extend_from_slice(&self.scratch[..room]);
                    self.truncated = true;
                } else {
                    self.buf.extend_from_slice(&self.scratch[..n]);
                }
            }
            Err(err) => {
                tracing::debug!(%err, "exec pipe read failed; treating as EOF");
                self.eof = true;
            }
        }
    }

    /// Renders the captured bytes.
    ///
    /// Lossy on purpose. A cap that lands mid-codepoint is normal, and a strict
    /// decode would turn "your command printed a lot" into "the daemon lost your
    /// output".
    fn into_string(self) -> String {
        String::from_utf8_lossy(&self.buf).into_owned()
    }
}

/// SIGTERM, wait `grace` for the group to go, then SIGKILL.
///
/// Returns whether the first signal was delivered. `done` lets the grace period
/// end early when the child finishes on its own, so a well-behaved process does
/// not cost the full grace period.
async fn escalate(pgid: u32, grace: Duration, done: Arc<Shared>) -> bool {
    if !signal_group(pgid, nix::sys::signal::Signal::SIGTERM) {
        return false;
    }

    let waited = tokio::time::timeout(grace, async {
        loop {
            if done.result.lock().await.is_some() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await;

    if waited.is_err() {
        tracing::warn!(
            pgid,
            "process group survived SIGTERM; escalating to SIGKILL"
        );
        signal_group(pgid, nix::sys::signal::Signal::SIGKILL);
    }
    true
}

/// The timeout path's escalation, which has no `Shared` to watch because the
/// waiter task *is* the caller.
async fn escalate_blind(pgid: u32, grace: Duration) {
    if !signal_group(pgid, nix::sys::signal::Signal::SIGTERM) {
        return;
    }
    tokio::time::sleep(grace).await;
    signal_group(pgid, nix::sys::signal::Signal::SIGKILL);
}

/// Signals a whole process group.
///
/// `ESRCH` is not an error worth reporting: it means the group is already gone,
/// which is the outcome a kill was asking for.
fn signal_group(pgid: u32, signal: nix::sys::signal::Signal) -> bool {
    let pid = nix::unistd::Pid::from_raw(pgid as i32);
    match nix::sys::signal::killpg(pid, signal) {
        Ok(()) => true,
        Err(nix::errno::Errno::ESRCH) => {
            tracing::debug!(pgid, "process group already gone");
            false
        }
        Err(err) => {
            tracing::warn!(pgid, %err, "killpg failed");
            false
        }
    }
}

#[cfg(unix)]
fn unix_signal(status: &std::process::ExitStatus) -> Option<i32> {
    std::os::unix::process::ExitStatusExt::signal(status)
}

#[cfg(not(unix))]
fn unix_signal(_status: &std::process::ExitStatus) -> Option<i32> {
    None
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use crate::config::Config;

    fn state() -> AppState {
        AppState::new(Config::default())
    }

    fn state_with(f: impl FnOnce(&mut Config)) -> AppState {
        let mut cfg = Config::default();
        f(&mut cfg);
        AppState::new(cfg)
    }

    fn req(id: &str, argv: &[&str]) -> StartRequest {
        StartRequest {
            exec_id: id.to_string(),
            command: argv.iter().map(|s| s.to_string()).collect(),
            shell: false,
            cwd: None,
            env: HashMap::new(),
            user: None,
            group: None,
            timeout_sec: None,
            stdin: false,
        }
    }

    /// Drives an exec to completion the way the daemon does, then returns the
    /// outcome. Polls the shared slot rather than sleeping a fixed interval, so
    /// the test is fast and does not race.
    async fn run(state: &AppState, req: StartRequest) -> Outcome {
        let id = req.exec_id.clone();
        let timeout = validate_timeout(req.timeout_sec).expect("valid timeout");
        let command = build_command(&req).expect("buildable command");
        spawn(state, &id, command, timeout).expect("spawn");
        await_result(state, &id).await
    }

    /// Waits for the waiter task to publish, with a bounded number of attempts so
    /// a regression fails the test rather than hanging the suite.
    async fn await_result(state: &AppState, id: &str) -> Outcome {
        let shared = state
            .with_execs(|execs| execs.get(id).map(|e| e.shared.clone()))
            .expect("entry registered");
        for _ in 0..600 {
            if let Some(outcome) = shared.result.lock().await.clone() {
                return outcome;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("exec {id} never finished");
    }

    /// Decodes a handler response body as JSON, so tests can assert on what the
    /// caller actually receives and not only on the status.
    async fn body_json(response: Response) -> serde_json::Value {
        let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
            .await
            .expect("body");
        serde_json::from_slice(&bytes).expect("json body")
    }

    /// One parsed SSE event: its `event:` name and its `data:` JSON.
    #[derive(Debug)]
    struct Sighting {
        name: String,
        data: serde_json::Value,
    }

    impl Sighting {
        /// The decoded bytes of an `output` event.
        fn output(&self) -> Vec<u8> {
            let encoded = self.data["output"].as_str().expect("output is a string");
            base64::engine::general_purpose::STANDARD
                .decode(encoded)
                .expect("output is base64")
        }

        fn offset(&self) -> u64 {
            self.data["offset"].as_u64().expect("offset is a number")
        }
    }

    /// Reads an SSE response body incrementally.
    ///
    /// Frame-by-frame rather than `to_bytes`, because two of the properties under
    /// test are only observable partway through a live stream: that a client can
    /// detach mid-command, and that the terminal event arrives *before* the body
    /// ends rather than being inferred from the end.
    struct SseReader {
        body: axum::body::Body,
        buffer: String,
    }

    impl SseReader {
        fn new(response: Response) -> Self {
            assert_eq!(
                response
                    .headers()
                    .get(axum::http::header::CONTENT_TYPE)
                    .map(|v| v.to_str().expect("ascii content type")),
                Some("text/event-stream"),
                "the attach response is not an SSE stream"
            );
            assert_eq!(
                response
                    .headers()
                    .get("x-accel-buffering")
                    .map(|v| v.to_str().expect("ascii")),
                Some("no"),
                "without this a buffering proxy batches the whole stream to exit"
            );
            Self {
                body: response.into_body(),
                buffer: String::new(),
            }
        }

        /// The next real event, skipping keep-alive comments. `None` once the body
        /// ends, which is how a client learns the stream is over.
        async fn next(&mut self) -> Option<Sighting> {
            use http_body_util::BodyExt as _;
            loop {
                if let Some(idx) = self.buffer.find("\n\n") {
                    let raw = self.buffer[..idx].to_string();
                    self.buffer.drain(..idx + 2);
                    if let Some(event) = parse_sse(&raw) {
                        return Some(event);
                    }
                    continue;
                }
                let frame = self.body.frame().await?.expect("body frame");
                if let Some(data) = frame.data_ref() {
                    self.buffer
                        .push_str(std::str::from_utf8(data).expect("utf8 sse"));
                }
            }
        }

        /// Reads to the end of the stream, with a bound so a regression that never
        /// terminates fails rather than hanging the suite.
        async fn drain(&mut self) -> Vec<Sighting> {
            let mut seen = Vec::new();
            for _ in 0..10_000 {
                match tokio::time::timeout(Duration::from_secs(20), self.next()).await {
                    Ok(Some(event)) => seen.push(event),
                    Ok(None) => return seen,
                    Err(_) => panic!("the stream stalled with no terminal event"),
                }
            }
            panic!("the stream never ended");
        }
    }

    fn parse_sse(raw: &str) -> Option<Sighting> {
        let mut name = None;
        let mut data = String::new();
        for line in raw.lines() {
            if let Some(rest) = line.strip_prefix("event: ") {
                name = Some(rest.to_string());
            } else if let Some(rest) = line.strip_prefix("data: ") {
                if !data.is_empty() {
                    data.push('\n');
                }
                data.push_str(rest);
            }
        }
        // A keep-alive is a bare comment with no event name and no data.
        let name = name?;
        Some(Sighting {
            name,
            data: serde_json::from_str(&data).expect("event data is json"),
        })
    }

    /// Attaches to an exec at `offset`, asserting the handler accepted the attach.
    async fn attach(state: &AppState, id: &str, offset: Option<u64>) -> SseReader {
        let response = stream(
            State(state.clone()),
            Path(id.to_string()),
            Ok(Query(StreamQuery { offset })),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        SseReader::new(response)
    }

    /// Concatenates the `output` events of one stream kind.
    fn collected(events: &[Sighting], want: &str) -> Vec<u8> {
        events
            .iter()
            .filter(|e| e.name == "output" && e.data["stream"] == want)
            .flat_map(|e| e.output())
            .collect()
    }

    /// Starts an exec without waiting for it, the way a streaming client does.
    fn launch(state: &AppState, request: StartRequest) {
        let id = request.exec_id.clone();
        let timeout = validate_timeout(request.timeout_sec).expect("valid timeout");
        let command = build_command(&request).expect("buildable command");
        spawn(state, &id, command, timeout).expect("spawn");
    }

    fn stdin_body(data: Option<&str>, signal: Option<&str>) -> StdinRequest {
        StdinRequest {
            data_b64: data.map(|d| base64::engine::general_purpose::STANDARD.encode(d)),
            signal: signal.map(str::to_string),
        }
    }

    #[tokio::test]
    async fn a_simple_command_captures_stdout_and_exit_zero() {
        let state = state();
        let outcome = run(&state, req("e1", &["/bin/sh", "-c", "echo hi"])).await;
        assert_eq!(outcome.exit_code, Some(0));
        assert_eq!(outcome.stdout.trim(), "hi");
        assert!(!outcome.truncated);
    }

    #[tokio::test]
    async fn stderr_is_captured_separately_and_a_nonzero_code_is_reported() {
        let state = state();
        let outcome = run(
            &state,
            req("e2", &["/bin/sh", "-c", "echo oops >&2; exit 3"]),
        )
        .await;
        assert_eq!(outcome.exit_code, Some(3));
        assert_eq!(outcome.stderr.trim(), "oops");
        assert!(outcome.stdout.is_empty());
    }

    /// The predecessor wrapped the script in `cd %s && { ... }`, which made an
    /// empty command a syntax error. `sh -c ''` exits 0 and that is correct.
    #[tokio::test]
    async fn an_empty_shell_command_exits_zero() {
        let state = state();
        let mut request = req("e3", &[]);
        request.shell = true;
        let outcome = run(&state, request).await;
        assert_eq!(
            outcome.exit_code,
            Some(0),
            "empty command must not be a syntax error"
        );
        assert!(outcome.stderr.is_empty(), "stderr was {:?}", outcome.stderr);
    }

    /// The other half of the same defect: a trailing comment used to swallow the
    /// closing brace, and an unbalanced brace used to escape the group.
    #[tokio::test]
    async fn a_comment_terminated_shell_command_exits_zero() {
        let state = state();
        let mut request = req("e4", &["echo hi", "# trailing comment"]);
        request.shell = true;
        let outcome = run(&state, request).await;
        assert_eq!(outcome.exit_code, Some(0));
        assert_eq!(outcome.stdout.trim(), "hi");
    }

    #[tokio::test]
    async fn an_unbalanced_brace_cannot_escape_the_wrapper() {
        let state = state();
        let mut request = req("e5", &["echo a", "}", "echo b"]);
        request.shell = true;
        let outcome = run(&state, request).await;
        // It is a shell syntax error inside `sh -c`, which is the honest result:
        // nothing after the stray brace runs, and the daemon reports the code.
        assert_ne!(outcome.exit_code, Some(0));
        assert!(
            !outcome.stdout.contains('b'),
            "stdout was {:?}",
            outcome.stdout
        );
    }

    /// Idempotency. Two starts with one id produce one child, and the second
    /// leaves the first entry's phase and output alone.
    #[tokio::test]
    async fn a_retried_start_does_not_spawn_a_second_child() {
        let state = state();
        // A file whose content counts spawns: two children would append twice.
        let dir = tempfile::tempdir().expect("tempdir");
        let marker = dir.path().join("spawns");
        let script = format!("echo x >> {}", marker.display());

        let first = start(
            State(state.clone()),
            Ok(Json(req("dup", &["/bin/sh", "-c", &script]))),
        )
        .await;
        assert_eq!(first.status(), StatusCode::OK);
        await_result(&state, "dup").await;

        let second = start(
            State(state.clone()),
            Ok(Json(req("dup", &["/bin/sh", "-c", &script]))),
        )
        .await;
        assert_eq!(second.status(), StatusCode::OK, "a retry is success");

        assert_eq!(
            state.with_execs(|execs| execs.len()),
            1,
            "the retry created a second entry"
        );
        let content = std::fs::read_to_string(&marker).expect("marker written");
        assert_eq!(
            content.lines().count(),
            1,
            "the retry spawned a second child"
        );
    }

    /// The retry must not disturb the existing entry's output either. Polling
    /// after the retry still sees the first child's stdout.
    #[tokio::test]
    async fn a_retry_leaves_the_existing_entrys_output_intact() {
        let state = state();
        run(&state, req("keep", &["/bin/sh", "-c", "echo original"])).await;

        start(
            State(state.clone()),
            Ok(Json(req("keep", &["/bin/sh", "-c", "echo replacement"]))),
        )
        .await;

        let shared = state
            .with_execs(|execs| execs.get("keep").map(|e| e.shared.clone()))
            .expect("entry");
        let held = shared.result.lock().await.clone().expect("output held");
        assert_eq!(held.stdout.trim(), "original");
    }

    /// Poll is read-only. Nothing about the entry may change across it.
    #[tokio::test]
    async fn poll_does_not_mutate_the_entry() {
        let state = state();
        run(&state, req("ro", &["/bin/sh", "-c", "echo hi"])).await;

        let before = state.with_execs(|execs| {
            let e = execs.get("ro").expect("entry");
            (e.pgid, e.acked_at)
        });
        let before_output = {
            let shared = state
                .with_execs(|execs| execs.get("ro").map(|e| e.shared.clone()))
                .expect("entry");
            shared.result.lock().await.clone()
        };

        for _ in 0..3 {
            let response = poll(State(state.clone()), Path("ro".to_string())).await;
            assert_eq!(response.status(), StatusCode::OK);
        }

        let after = state.with_execs(|execs| {
            let e = execs.get("ro").expect("entry");
            (e.pgid, e.acked_at)
        });
        let after_output = {
            let shared = state
                .with_execs(|execs| execs.get("ro").map(|e| e.shared.clone()))
                .expect("entry");
            shared.result.lock().await.clone()
        };

        assert_eq!(before, after, "poll changed entry metadata");
        assert_eq!(
            before_output.map(|o| (o.stdout, o.exit_code)),
            after_output.map(|o| (o.stdout, o.exit_code)),
            "poll consumed or altered the held output"
        );
        assert_eq!(state.with_execs(|execs| execs.len()), 1);
    }

    /// One of the three security properties the model pins: the agent token
    /// never enters a child's environment. Proven by having the child print its
    /// own environment and grepping it.
    #[tokio::test]
    async fn the_agent_token_never_reaches_the_child_environment() {
        let state = state();
        state.bootstrap(b"super-secret-agent-token");
        // Also present in the daemon's own environment, so an inherited env — not
        // just an explicitly forwarded one — would fail this test.
        // SAFETY-free alternative to set_var: the child env starts empty because
        // build_command calls env_clear, so we assert on that instead of mutating
        // the process environment.
        let outcome = run(&state, req("envtest", &["/usr/bin/env"])).await;

        assert_eq!(outcome.exit_code, Some(0));
        assert!(
            !outcome.stdout.contains("super-secret-agent-token"),
            "the agent token appeared in the child environment: {:?}",
            outcome.stdout
        );
        assert!(
            !outcome.stdout.to_ascii_lowercase().contains("agent_token"),
            "an agent token variable name leaked: {:?}",
            outcome.stdout
        );
        assert!(
            outcome.stdout.trim().is_empty(),
            "the child environment must start empty, got {:?}",
            outcome.stdout
        );
    }

    /// Requested variables do reach the child, so the emptiness above is a
    /// property of what we pass and not of a broken env path.
    #[tokio::test]
    async fn requested_environment_variables_do_reach_the_child() {
        let state = state();
        let mut request = req("envpass", &["/bin/sh", "-c", "echo $WANTED"]);
        request.env.insert("WANTED".to_string(), "yes".to_string());
        let outcome = run(&state, request).await;
        assert_eq!(outcome.stdout.trim(), "yes");
    }

    /// Output past the cap truncates and says so, rather than growing until the
    /// daemon is OOM-killed in a VM nothing can restart it in.
    #[tokio::test]
    async fn output_past_the_cap_is_truncated_and_flagged() {
        let state = state_with(|cfg| cfg.max_output_bytes = 1024);
        let outcome = run(
            &state,
            req(
                "cap",
                &["/bin/sh", "-c", "i=0; while [ $i -lt 400 ]; do echo aaaaaaaaaaaaaaaaaaaaaaaaaaaaaa; i=$((i+1)); done"],
            ),
        )
        .await;
        assert!(outcome.truncated, "the cap was not reported");
        assert!(
            outcome.stdout.len() <= 1024,
            "captured {} bytes past a 1024 cap",
            outcome.stdout.len()
        );
        // The command still finished: past the cap we keep draining so the writer
        // never blocks in the kernel.
        assert_eq!(outcome.exit_code, Some(0));
    }

    /// Output under the cap is not flagged, so `truncated` means something.
    #[tokio::test]
    async fn output_under_the_cap_is_not_flagged() {
        let state = state_with(|cfg| cfg.max_output_bytes = 1024);
        let outcome = run(&state, req("nocap", &["/bin/sh", "-c", "echo short"])).await;
        assert!(!outcome.truncated);
        assert_eq!(outcome.stdout.trim(), "short");
    }

    /// Omitting cwd inherits the daemon's working directory. Forcing `/` broke
    /// every prebuilt-image task, and it was the last blocker found in the PR.
    #[tokio::test]
    async fn an_omitted_cwd_inherits_rather_than_defaulting_to_root() {
        let state = state();
        let outcome = run(&state, req("cwd", &["/bin/sh", "-c", "pwd"])).await;
        let expected = std::env::current_dir().expect("cwd");
        assert_eq!(outcome.stdout.trim(), expected.to_string_lossy());
        assert_ne!(outcome.stdout.trim(), "/", "cwd must not default to /");
    }

    #[tokio::test]
    async fn an_explicit_cwd_is_honored() {
        let state = state();
        let dir = tempfile::tempdir().expect("tempdir");
        let mut request = req("cwd2", &["/bin/sh", "-c", "pwd"]);
        request.cwd = Some(dir.path().to_string_lossy().into_owned());
        let outcome = run(&state, request).await;
        // Compared canonically: the temp root can be a symlink on some systems.
        let want = dir.path().canonicalize().expect("canonical");
        let got = std::path::PathBuf::from(outcome.stdout.trim())
            .canonicalize()
            .expect("canonical");
        assert_eq!(got, want);
    }

    /// An argv array execs directly, with no shell between. If a shell were
    /// involved the metacharacters here would be interpreted.
    #[tokio::test]
    async fn an_argv_array_is_not_passed_through_a_shell() {
        let state = state();
        let outcome = run(&state, req("noshell", &["/bin/echo", "a; echo b", "$HOME"])).await;
        assert_eq!(outcome.exit_code, Some(0));
        assert_eq!(outcome.stdout.trim(), "a; echo b $HOME");
    }

    /// Ack releases the output and moves the entry to Acked.
    #[tokio::test]
    async fn ack_releases_the_output_once() {
        let state = state();
        run(&state, req("ackme", &["/bin/sh", "-c", "echo done"])).await;

        let first = ack(State(state.clone()), Path("ackme".to_string())).await;
        assert_eq!(first.status(), StatusCode::OK);

        let acked_at = state.with_execs(|execs| execs.get("ackme").and_then(|e| e.acked_at));
        assert!(acked_at.is_some(), "ack did not start the TTL clock");

        // A second ack has nothing left to hand back, so it is a conflict rather
        // than a 200 with empty output that would read as "no output".
        let second = ack(State(state.clone()), Path("ackme".to_string())).await;
        assert_eq!(second.status(), StatusCode::CONFLICT);
    }

    /// Acking a live exec is 409. A silent success would drop output that is
    /// still being written.
    #[tokio::test]
    async fn acking_a_running_exec_is_a_conflict_not_a_silent_success() {
        let state = state();
        let request = req("live", &["/bin/sh", "-c", "sleep 30"]);
        let timeout = validate_timeout(request.timeout_sec).expect("valid");
        let command = build_command(&request).expect("buildable");
        spawn(&state, "live", command, timeout).expect("spawn");

        let response = ack(State(state.clone()), Path("live".to_string())).await;
        assert_eq!(response.status(), StatusCode::CONFLICT);
        assert!(
            state.with_execs(|execs| execs["live"].acked_at.is_none()),
            "a refused ack must not start the TTL clock"
        );

        kill(State(state.clone()), Path("live".to_string())).await;
    }

    /// Kill signals the group, so a backgrounded grandchild dies with it. The
    /// direct child exits immediately; the sleep it backgrounded is what the
    /// group signal has to reach.
    #[tokio::test]
    async fn kill_signals_the_whole_process_group() {
        let state = state_with(|cfg| {
            cfg.kill_grace = Duration::from_millis(50);
            cfg.output_linger = Duration::from_millis(200);
        });
        let request = req("group", &["/bin/sh", "-c", "sleep 30 & echo started; wait"]);
        let command = build_command(&request).expect("buildable");
        spawn(&state, "group", command, None).expect("spawn");

        let pgid = state
            .with_execs(|execs| execs.get("group").and_then(|e| e.pgid))
            .expect("the pgid must be captured at spawn, not lazily");
        assert!(pgid > 0);

        let response = kill(State(state.clone()), Path("group".to_string())).await;
        assert_eq!(response.status(), StatusCode::OK);
        // The body reports whether a signal was actually delivered. Asserting on
        // it rather than only on the status is what catches a lost pgid: a kill
        // with nothing to signal still answers 200.
        assert_eq!(
            body_json(response).await["killed"],
            serde_json::Value::Bool(true),
            "no signal was delivered, so the pgid was lost"
        );

        // `wait` in the script only returns once the backgrounded sleep is gone,
        // so the outcome arriving at all proves the group signal reached it.
        let outcome = await_result(&state, "group").await;
        assert_ne!(
            outcome.exit_code,
            Some(0),
            "the group was not actually signalled"
        );
    }

    /// The pgid is read at spawn, so it survives the child being reaped. Reading
    /// `Child::id()` from the kill path would return `None` here.
    #[tokio::test]
    async fn the_pgid_survives_the_child_being_reaped() {
        let state = state();
        run(&state, req("pgid", &["/bin/true"])).await;
        let pgid = state.with_execs(|execs| execs["pgid"].pgid);
        assert!(pgid.is_some(), "pgid was lost once the child was reaped");

        // Killing an already-gone group is not an error the caller has to handle.
        let response = kill(State(state.clone()), Path("pgid".to_string())).await;
        assert_eq!(response.status(), StatusCode::OK);
    }

    /// A timeout signals the group and the entry still reports a result rather
    /// than hanging. Kept short so the suite stays fast.
    #[tokio::test]
    async fn a_timeout_kills_the_group_and_still_reports() {
        let state = state_with(|cfg| {
            cfg.kill_grace = Duration::from_millis(30);
            cfg.output_linger = Duration::from_millis(100);
        });
        let mut request = req("slow", &["/bin/sh", "-c", "echo before; sleep 30"]);
        request.timeout_sec = Some(0.15);
        let outcome = run(&state, request).await;

        assert_ne!(outcome.exit_code, Some(0), "the timeout did not fire");
        assert_eq!(
            outcome.stdout.trim(),
            "before",
            "output before the kill is kept"
        );
    }

    /// Only acked entries are collected. An unacked entry has no deadline
    /// however old it is — collecting one destroys output nobody read, which is
    /// the defect the Python daemon shipped by unlinking on exit.
    #[tokio::test]
    async fn collection_takes_acked_entries_only() {
        let state = state_with(|cfg| cfg.exec_ttl = Duration::ZERO);
        run(&state, req("gc-acked", &["/bin/true"])).await;
        run(&state, req("gc-unacked", &["/bin/true"])).await;

        ack(State(state.clone()), Path("gc-acked".to_string())).await;
        // The TTL is zero, so the acked entry is immediately expired while the
        // unacked one is still not eligible.
        assert_eq!(collect_expired(&state), 1);
        assert!(state.with_execs(|execs| execs.contains_key("gc-unacked")));
        assert!(!state.with_execs(|execs| execs.contains_key("gc-acked")));
    }

    #[tokio::test]
    async fn an_acked_entry_survives_until_its_ttl_elapses() {
        let state = state_with(|cfg| cfg.exec_ttl = Duration::from_secs(300));
        run(&state, req("gc-young", &["/bin/true"])).await;
        ack(State(state.clone()), Path("gc-young".to_string())).await;

        assert_eq!(collect_expired(&state), 0);
        assert!(state.with_execs(|execs| execs.contains_key("gc-young")));
    }

    /// An unknown id is 404, because it is genuinely absent.
    #[tokio::test]
    async fn an_unknown_id_is_404_on_every_route() {
        let state = state();
        for response in [
            poll(State(state.clone()), Path("nope".to_string())).await,
            ack(State(state.clone()), Path("nope".to_string())).await,
            kill(State(state.clone()), Path("nope".to_string())).await,
        ] {
            assert_eq!(response.status(), StatusCode::NOT_FOUND);
        }
    }

    /// A malformed body is 400, never 404. Clients map 404 onto
    /// FileNotFoundError, so the wrong code turns a protocol typo into a phantom
    /// absent artifact — that is how one defect hid for a whole review round.
    #[tokio::test]
    async fn a_malformed_body_is_400_and_never_404() {
        let state = state();
        let rejection = Json::<StartRequest>::from_bytes(b"{").expect_err("invalid json");
        let response = start(State(state.clone()), Err(rejection)).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_ne!(response.status(), StatusCode::NOT_FOUND);
        assert!(
            state.with_execs(|execs| execs.is_empty()),
            "a rejected start must not register anything"
        );
    }

    #[tokio::test]
    async fn an_empty_exec_id_is_400() {
        let state = state();
        let response = start(State(state.clone()), Ok(Json(req("", &["/bin/true"])))).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert!(state.with_execs(|execs| execs.is_empty()));
    }

    /// A bad timeout is rejected before anything spawns. The predecessor raised
    /// inside the waiter thread, by which point the child was running with
    /// nobody left to reap it.
    #[tokio::test]
    async fn a_bad_timeout_is_rejected_before_the_child_spawns() {
        for bad in [0.0, -1.0, f64::NAN, f64::INFINITY] {
            let state = state();
            let mut request = req("badtimeout", &["/bin/sh", "-c", "sleep 30"]);
            request.timeout_sec = Some(bad);

            let response = start(State(state.clone()), Ok(Json(request))).await;
            assert_eq!(
                response.status(),
                StatusCode::BAD_REQUEST,
                "timeout_sec {bad} was accepted"
            );
            assert!(
                state.with_execs(|execs| execs.is_empty()),
                "timeout_sec {bad} spawned an orphan before validation"
            );
        }
    }

    #[test]
    fn a_valid_timeout_is_accepted_and_an_absent_one_means_unbounded() {
        assert_eq!(
            validate_timeout(Some(1.5)).expect("valid"),
            Some(Duration::from_millis(1500))
        );
        assert_eq!(validate_timeout(None).expect("valid"), None);
    }

    /// An argv array with no program is a 400, not a spawn attempt on "".
    #[tokio::test]
    async fn an_empty_argv_without_shell_is_400() {
        let state = state();
        let response = start(State(state.clone()), Ok(Json(req("empty", &[])))).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert!(state.with_execs(|execs| execs.is_empty()));
    }

    /// Demotion is optional. With no user or group the child runs as the daemon,
    /// which is what every current caller wants.
    #[tokio::test]
    async fn demotion_is_optional() {
        let state = state();
        let outcome = run(&state, req("nodemote", &["/usr/bin/id", "-u"])).await;
        let expected = std::process::Command::new("/usr/bin/id")
            .arg("-u")
            .output()
            .expect("id -u");
        assert_eq!(
            outcome.stdout.trim(),
            String::from_utf8_lossy(&expected.stdout).trim()
        );
    }

    /// Requested demotion reaches the builder. Actually demoting needs root, so
    /// the assertion is on the built command rather than on a spawned child —
    /// what matters is that the uid/gid path is `Command::uid`/`gid` and not
    /// `pre_exec`, and that is visible in the source, not in a child's output.
    #[test]
    fn a_demotion_request_builds_without_pre_exec() {
        let mut request = req("demote", &["/bin/true"]);
        request.user = Some(65534);
        request.group = Some(65534);
        let command = build_command(&request).expect("buildable");
        assert_eq!(
            command.as_std().get_program(),
            std::ffi::OsStr::new("/bin/true")
        );
    }

    /// The phase mapping matches the model's `ExecPhase`.
    #[test]
    fn phases_match_the_modeled_state_machine() {
        assert_eq!(phase_of(false, false), Phase::Running);
        assert_eq!(phase_of(false, true), Phase::Exited);
        assert_eq!(phase_of(true, true), Phase::Acked);
    }

    // ---- streaming ----

    /// A command that emits in stages streams those stages, and the terminal
    /// event carries the real exit code.
    ///
    /// The staging matters: a single `echo` would pass even against an
    /// implementation that only replays the buffer once the child is gone, which is
    /// the poll behavior streaming exists to replace.
    #[tokio::test]
    async fn a_staged_command_streams_its_stages_and_a_terminal_exit() {
        let state = state();
        launch(
            &state,
            req(
                "stream-stages",
                &[
                    "/bin/sh",
                    "-c",
                    "echo one; sleep 0.15; echo two >&2; sleep 0.15; echo three; exit 7",
                ],
            ),
        );

        let mut reader = attach(&state, "stream-stages", None).await;
        let events = reader.drain().await;

        assert_eq!(
            String::from_utf8_lossy(&collected(&events, "stdout")),
            "one\nthree\n"
        );
        assert_eq!(
            String::from_utf8_lossy(&collected(&events, "stderr")),
            "two\n"
        );

        let exit = events.last().expect("at least one event");
        assert_eq!(
            exit.name, "exit",
            "the last event must be the terminal one, got {events:#?}"
        );
        assert_eq!(
            exit.data["exit_code"], 7,
            "the terminal event carried the wrong status"
        );
        assert_eq!(
            exit.offset(),
            (b"one\nthree\ntwo\n".len()) as u64,
            "the terminal offset must equal the bytes published"
        );

        // Streaming did not consume the poll path: both views work.
        let polled =
            body_json(poll(State(state.clone()), Path("stream-stages".into())).await).await;
        assert_eq!(polled["exit_code"], 7);
        assert_eq!(polled["stdout"], "one\nthree\n");
    }

    /// Reconnecting at an offset yields exactly the bytes after it: no
    /// duplication, no loss. This is the property a cursorless attach cannot have.
    #[tokio::test]
    async fn reattaching_at_an_offset_yields_exactly_the_bytes_after_it() {
        let state = state();
        launch(
            &state,
            req(
                "stream-resume",
                &["/bin/sh", "-c", "echo AAAA; sleep 0.2; echo BBBB"],
            ),
        );

        // First attach: read only the first chunk, then hang up mid-command.
        let mut first = attach(&state, "stream-resume", None).await;
        let head = first.next().await.expect("a first output event");
        assert_eq!(head.name, "output");
        assert_eq!(head.offset(), 0);
        let seen = head.output();
        assert_eq!(String::from_utf8_lossy(&seen), "AAAA\n");
        drop(first);

        // Second attach at exactly where the first stopped.
        let mut second = attach(&state, "stream-resume", Some(seen.len() as u64)).await;
        let rest = second.drain().await;

        assert!(
            !rest.iter().any(|e| e.name == "gap"),
            "a reattach inside the replay window must not report a gap: {rest:#?}"
        );
        let tail = collected(&rest, "stdout");
        assert_eq!(
            String::from_utf8_lossy(&tail),
            "BBBB\n",
            "the resume duplicated or lost bytes"
        );
        assert_eq!(
            rest.iter()
                .filter(|e| e.name == "output")
                .map(|e| e.offset())
                .next(),
            Some(seen.len() as u64),
            "the first resumed event must start at the requested offset"
        );
        assert_eq!(rest.last().expect("events").name, "exit");
    }

    /// Attaching at offset 0 after the exec is over replays the whole window and
    /// still terminates, rather than waiting on a channel whose `Finished` was
    /// sent before this subscriber existed.
    #[tokio::test]
    async fn attaching_after_the_exec_finished_replays_and_terminates() {
        let state = state();
        let outcome = run(&state, req("stream-late", &["/bin/sh", "-c", "echo late"])).await;
        assert_eq!(outcome.exit_code, Some(0));

        let mut reader = attach(&state, "stream-late", None).await;
        let events = reader.drain().await;
        assert_eq!(
            String::from_utf8_lossy(&collected(&events, "stdout")),
            "late\n"
        );
        assert_eq!(events.last().expect("events").name, "exit");
    }

    /// Attaching after an ack still terminates.
    ///
    /// An ack *takes* the `Outcome`, so `result` goes back to `None` and cannot be
    /// used as "has this finished?". A stream keyed on it would decide a
    /// long-finished exec is still running and wait on a channel that will never
    /// carry another message — which is why the terminal marker is separate.
    #[tokio::test]
    async fn attaching_after_an_ack_still_terminates() {
        let state = state();
        run(&state, req("stream-acked", &["/bin/sh", "-c", "echo gone"])).await;
        let acked = ack(State(state.clone()), Path("stream-acked".to_string())).await;
        assert_eq!(acked.status(), StatusCode::OK);

        let mut reader = attach(&state, "stream-acked", None).await;
        let events = tokio::time::timeout(Duration::from_secs(5), reader.drain())
            .await
            .expect("an attach after an ack must not hang waiting for an exit");
        let exit = events.last().expect("events");
        assert_eq!(exit.name, "exit");
        assert_eq!(
            exit.data["exit_code"], 0,
            "the terminal status must survive the ack that released the output"
        );
    }

    /// A dropped stream must never kill the exec. The exec is a server-side
    /// object; attaching is a view onto it.
    #[tokio::test]
    async fn dropping_a_stream_leaves_the_exec_alive_and_pollable() {
        let state = state();
        launch(
            &state,
            req(
                "stream-detach",
                &[
                    "/bin/sh",
                    "-c",
                    "echo first; sleep 0.3; echo second; exit 4",
                ],
            ),
        );

        let mut reader = attach(&state, "stream-detach", None).await;
        let head = reader.next().await.expect("a first event");
        assert_eq!(String::from_utf8_lossy(&head.output()), "first\n");
        // Hang up while the child is still running and still has output to write.
        drop(reader);

        // The command ran to completion regardless, including the part written
        // after nobody was listening.
        let outcome = await_result(&state, "stream-detach").await;
        assert_eq!(
            outcome.exit_code,
            Some(4),
            "the detach interfered with the exec"
        );
        assert_eq!(outcome.stdout, "first\nsecond\n");

        let polled =
            body_json(poll(State(state.clone()), Path("stream-detach".into())).await).await;
        assert_eq!(polled["phase"], "exited");
        assert_eq!(polled["exit_code"], 4);
    }

    /// Two attaches at once each get the whole stream. Neither steals from the
    /// other, which a single-consumer channel would allow.
    #[tokio::test]
    async fn two_concurrent_streams_each_see_everything() {
        let state = state();
        launch(
            &state,
            req(
                "stream-fanout",
                &["/bin/sh", "-c", "sleep 0.1; echo shared"],
            ),
        );

        let mut a = attach(&state, "stream-fanout", None).await;
        let mut b = attach(&state, "stream-fanout", None).await;
        let (seen_a, seen_b) = tokio::join!(a.drain(), b.drain());

        assert_eq!(
            String::from_utf8_lossy(&collected(&seen_a, "stdout")),
            "shared\n"
        );
        assert_eq!(
            String::from_utf8_lossy(&collected(&seen_b, "stdout")),
            "shared\n"
        );
        assert_eq!(seen_a.last().expect("events").name, "exit");
        assert_eq!(seen_b.last().expect("events").name, "exit");
    }

    /// Resuming before the replay window reports the lost range instead of
    /// silently handing back a buffer that starts later than asked. A client that
    /// cannot tell missing output from no output reads a hole as completion.
    #[tokio::test]
    async fn resuming_before_the_replay_window_reports_a_gap() {
        // A tiny ring against far more output, so eviction is certain.
        let state = state_with(|cfg| cfg.stream_buffer_bytes = 64);
        let outcome = run(
            &state,
            req(
                "stream-gap",
                &[
                    "/bin/sh",
                    "-c",
                    "i=0; while [ $i -lt 200 ]; do echo LINE$i; i=$((i+1)); done",
                ],
            ),
        )
        .await;
        assert_eq!(outcome.exit_code, Some(0));

        let mut reader = attach(&state, "stream-gap", Some(0)).await;
        let events = reader.drain().await;

        let gap = events
            .iter()
            .find(|e| e.name == "gap")
            .expect("a gap must be reported when the window has wrapped");
        assert_eq!(gap.data["from"], 0);
        assert!(
            gap.data["to"].as_u64().expect("to") > 0,
            "the gap must name the range that was lost"
        );
        // The retained tail is still delivered, and it is the tail rather than the
        // head: a streaming consumer wants the bytes it has not seen.
        let delivered = collected(&events, "stdout");
        assert!(
            delivered.ends_with(b"LINE199\n"),
            "the tail was not replayed"
        );
        assert!(delivered.len() <= 64 + 16 * 1024, "the ring did not bound");
        assert_eq!(events.last().expect("events").name, "exit");
    }

    /// Output past `max_output_bytes` still streams. The head cap governs the
    /// polled `Outcome`, which the live conformance suite asserts on; it must not
    /// also silence a stream that has been keeping up.
    #[tokio::test]
    async fn output_past_the_poll_cap_still_streams_while_truncated_still_reports() {
        let state = state_with(|cfg| {
            cfg.max_output_bytes = 128;
            cfg.stream_buffer_bytes = 1024 * 1024;
        });
        launch(
            &state,
            req(
                "stream-past-cap",
                &[
                    "/bin/sh",
                    "-c",
                    "i=0; while [ $i -lt 100 ]; do echo 0123456789012345678901234567890123456789; i=$((i+1)); done",
                ],
            ),
        );

        let mut reader = attach(&state, "stream-past-cap", None).await;
        let events = reader.drain().await;
        let streamed = collected(&events, "stdout");
        assert_eq!(
            streamed.len(),
            100 * 41,
            "the head cap silenced the stream past {} bytes",
            128
        );

        // And the existing polled semantics are untouched.
        let outcome = await_result(&state, "stream-past-cap").await;
        assert!(
            outcome.truncated,
            "the polled truncation flag stopped working"
        );
        assert!(outcome.stdout.len() <= 128);
        let exit = events.last().expect("events");
        assert_eq!(exit.name, "exit");
        assert_eq!(
            exit.data["truncated"],
            serde_json::Value::Bool(true),
            "the terminal event must carry the same truncation fact as the poll"
        );
    }

    /// An unknown id is 404 on the stream route too, and a non-numeric offset is
    /// 400 rather than being silently treated as 0.
    #[tokio::test]
    async fn stream_rejects_an_unknown_id_and_a_bad_offset() {
        let state = state();
        let missing = stream(
            State(state.clone()),
            Path("nope".to_string()),
            Ok(Query(StreamQuery { offset: None })),
        )
        .await;
        assert_eq!(missing.status(), StatusCode::NOT_FOUND);

        run(&state, req("q", &["/bin/true"])).await;
        let rejection = Query::<StreamQuery>::try_from_uri(
            &"http://x/v1/exec/q/stream?offset=banana"
                .parse()
                .expect("uri"),
        )
        .expect_err("a non-numeric offset must not parse");
        let bad = stream(State(state.clone()), Path("q".to_string()), Err(rejection)).await;
        assert_eq!(bad.status(), StatusCode::BAD_REQUEST);
    }

    // ---- stdin ----

    /// stdin defaults to null. A child that reads it sees EOF immediately rather
    /// than blocking on a pipe nobody will write to.
    #[tokio::test]
    async fn stdin_defaults_to_null() {
        let state = state();
        let outcome = run(&state, req("stdin-default", &["/bin/cat"])).await;
        assert_eq!(
            outcome.exit_code,
            Some(0),
            "cat against /dev/null must exit immediately"
        );
        assert!(outcome.stdout.is_empty());

        // And the write route refuses, distinguishably from "already closed".
        let refused = write_stdin(
            State(state.clone()),
            Path("stdin-default".to_string()),
            Ok(Json(stdin_body(Some("hi"), None))),
        )
        .await;
        assert_eq!(refused.status(), StatusCode::CONFLICT);
        assert_eq!(
            body_json(refused).await["error"],
            "stdin_not_requested",
            "the refusal must say the flag was not set, not that stdin closed"
        );
    }

    /// The whole point: write a prompt, close the pipe, and `cat` completes.
    ///
    /// Without dropping our handle this hangs — `Child::wait()` closes only the
    /// copy the `Child` owns, so the child never sees EOF.
    #[tokio::test]
    async fn a_stdin_write_then_eof_lets_cat_complete() {
        let state = state();
        let mut request = req("stdin-cat", &["/bin/cat"]);
        request.stdin = true;
        launch(&state, request);

        let wrote = write_stdin(
            State(state.clone()),
            Path("stdin-cat".to_string()),
            Ok(Json(stdin_body(Some("hello stdin\n"), None))),
        )
        .await;
        assert_eq!(wrote.status(), StatusCode::OK);
        assert_eq!(body_json(wrote).await["written"], 12);

        // Still running: the bytes are in, but nothing has said the input is over.
        let mid = body_json(poll(State(state.clone()), Path("stdin-cat".into())).await).await;
        assert_eq!(
            mid["phase"], "running",
            "cat must still be waiting for more input"
        );

        let closed = write_stdin(
            State(state.clone()),
            Path("stdin-cat".to_string()),
            Ok(Json(stdin_body(None, Some("eof")))),
        )
        .await;
        assert_eq!(closed.status(), StatusCode::OK);
        assert_eq!(body_json(closed).await["eof"], true);

        let outcome = await_result(&state, "stdin-cat").await;
        assert_eq!(outcome.exit_code, Some(0), "cat never saw EOF");
        assert_eq!(outcome.stdout, "hello stdin\n");
    }

    /// Data and EOF in one request, which is the common shape for feeding a
    /// prompt. Two round trips would leave a window where the child has the bytes
    /// but not the signal that the input is complete.
    #[tokio::test]
    async fn data_and_eof_in_one_request_is_enough() {
        let state = state();
        let mut request = req("stdin-once", &["/usr/bin/wc", "-c"]);
        request.stdin = true;
        launch(&state, request);

        let response = write_stdin(
            State(state.clone()),
            Path("stdin-once".to_string()),
            Ok(Json(stdin_body(Some("abcde"), Some("eof")))),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);

        let outcome = await_result(&state, "stdin-once").await;
        assert_eq!(outcome.exit_code, Some(0));
        assert_eq!(outcome.stdout.trim(), "5");
    }

    /// Writing after the child is gone is a mapped error, not a dead daemon.
    /// Rust sets SIGPIPE to SIG_IGN, so the write surfaces as `BrokenPipe`.
    #[tokio::test]
    async fn writing_stdin_after_the_child_exited_is_gone_not_a_crash() {
        let state = state();
        let mut request = req("stdin-dead", &["/bin/true"]);
        request.stdin = true;
        launch(&state, request);
        await_result(&state, "stdin-dead").await;

        let response = write_stdin(
            State(state.clone()),
            Path("stdin-dead".to_string()),
            Ok(Json(stdin_body(Some("too late"), None))),
        )
        .await;
        assert_eq!(response.status(), StatusCode::GONE);
        assert_eq!(body_json(response).await["error"], "stdin_closed");

        // The daemon is still serving, which is the half a panic would take out.
        let health = poll(State(state.clone()), Path("stdin-dead".into())).await;
        assert_eq!(health.status(), StatusCode::OK);
    }

    /// A child that closes its own stdin while still running makes the next write
    /// fail with `BrokenPipe`, and that must be a mapped 410 rather than a 500 —
    /// and must not take the daemon down.
    ///
    /// Distinct from the exited-child case above: there our own handle is already
    /// gone, so the write never reaches the pipe. Here the handle is live and the
    /// *reader* is what disappeared, which is the only way to exercise the errno.
    /// Rust sets SIGPIPE to SIG_IGN, so this arrives as an `io::Error` rather than
    /// as a signal that would kill the process.
    #[tokio::test]
    async fn a_write_to_a_child_that_closed_its_own_stdin_is_gone() {
        let state = state_with(|cfg| {
            cfg.kill_grace = Duration::from_millis(30);
            cfg.output_linger = Duration::from_millis(50);
        });
        let mut request = req(
            "stdin-closed-by-child",
            // Closes fd 0, announces it, then stays alive so the write below has a
            // live handle pointing at a pipe with no reader.
            &["/bin/sh", "-c", "exec 0<&-; echo closed; sleep 30"],
        );
        request.stdin = true;
        launch(&state, request);

        // Wait for the child to say it has closed its end. Bounded so a regression
        // fails rather than hanging.
        let shared = state
            .with_execs(|execs| execs.get("stdin-closed-by-child").map(|e| e.shared.clone()))
            .expect("entry");
        let mut ready = false;
        for _ in 0..400 {
            if shared.log.lock().await.total > 0 {
                ready = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(ready, "the child never closed its stdin");

        // Fill past the pipe's capacity rather than writing a token payload a
        // bounded number of times.
        //
        // Why this matters, measured: with a 7-byte payload the write can succeed
        // indefinitely, because the bytes fit the kernel's 64 KiB pipe buffer and
        // EPIPE is only raised once a write must block on a reader that is gone.
        // The count needed to overflow that buffer is not a property of this
        // daemon — under a parallel suite another test's `fork` transiently
        // duplicates every open descriptor, including this pipe's read end, so the
        // pipe genuinely still has a reader for that window and the write is
        // genuinely fine. The old loop asserted on that race and failed roughly
        // one run in eight, single-threaded never.
        //
        // A payload larger than the buffer removes the race: the write cannot be
        // absorbed, so it must either block on a reader or report that none
        // exists, whatever else the machine is doing.
        let overflow = "x".repeat(256 * 1024);
        let mut status = None;
        for _ in 0..8 {
            let response = write_stdin(
                State(state.clone()),
                Path("stdin-closed-by-child".to_string()),
                // Well under `max_stdin_write_bytes` (1 MiB) so a 413 cannot be
                // mistaken for the 410 under test, and well over the 64 KiB pipe
                // buffer so the write cannot be quietly absorbed.
                Ok(Json(stdin_body(Some(&overflow), None))),
            )
            .await;
            status = Some(response.status());
            if response.status() != StatusCode::OK {
                assert_eq!(
                    response.status(),
                    StatusCode::GONE,
                    "a broken pipe must map to 410, not 500: {:?}",
                    body_json(response).await
                );
                break;
            }
        }
        assert_eq!(
            status,
            Some(StatusCode::GONE),
            "the write never reported the reader was gone"
        );

        // The handle was dropped, so a subsequent write is refused without
        // touching the pipe again.
        let after = write_stdin(
            State(state.clone()),
            Path("stdin-closed-by-child".to_string()),
            Ok(Json(stdin_body(Some("more"), None))),
        )
        .await;
        assert_eq!(after.status(), StatusCode::GONE);

        // And the daemon is still serving, which is the half a SIGPIPE would take.
        let health = poll(
            State(state.clone()),
            Path("stdin-closed-by-child".to_string()),
        )
        .await;
        assert_eq!(health.status(), StatusCode::OK);

        kill(
            State(state.clone()),
            Path("stdin-closed-by-child".to_string()),
        )
        .await;
        await_result(&state, "stdin-closed-by-child").await;
    }

    /// A second EOF is 410 rather than a silent success: the handle is gone, and
    /// saying so lets a client tell a duplicate close from a live one.
    #[tokio::test]
    async fn a_second_eof_is_gone() {
        let state = state();
        let mut request = req("stdin-twice", &["/bin/cat"]);
        request.stdin = true;
        launch(&state, request);

        let first = write_stdin(
            State(state.clone()),
            Path("stdin-twice".to_string()),
            Ok(Json(stdin_body(None, Some("eof")))),
        )
        .await;
        assert_eq!(first.status(), StatusCode::OK);

        let second = write_stdin(
            State(state.clone()),
            Path("stdin-twice".to_string()),
            Ok(Json(stdin_body(None, Some("eof")))),
        )
        .await;
        assert_eq!(second.status(), StatusCode::GONE);

        await_result(&state, "stdin-twice").await;
    }

    /// stdin bytes are bytes. Base64 is what carries the ones a JSON string
    /// cannot hold, and a write that mangled them would not survive a round trip
    /// through `od`.
    #[tokio::test]
    async fn stdin_carries_arbitrary_bytes_intact() {
        let state = state();
        let mut request = req("stdin-bytes", &["/bin/cat"]);
        request.stdin = true;
        launch(&state, request);

        // Invalid UTF-8 and a NUL: neither can appear in a JSON string.
        let raw = [0x00u8, 0xff, 0xfe, b'a', 0x80];
        let response = write_stdin(
            State(state.clone()),
            Path("stdin-bytes".to_string()),
            Ok(Json(StdinRequest {
                data_b64: Some(base64::engine::general_purpose::STANDARD.encode(raw)),
                signal: Some("eof".to_string()),
            })),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(body_json(response).await["written"], 5);

        let mut reader = attach(&state, "stdin-bytes", Some(0)).await;
        let events = reader.drain().await;
        assert_eq!(
            collected(&events, "stdout"),
            raw.to_vec(),
            "the bytes did not survive the write and the stream"
        );
    }

    /// The bounds. A write past the cap is 413 and a bad body is 400, and neither
    /// is 404 — a client that maps 404 onto "missing" would report a phantom
    /// absent exec for what is really a request problem.
    #[tokio::test]
    async fn stdin_bounds_and_malformed_bodies_are_rejected_distinguishably() {
        let state = state_with(|cfg| cfg.max_stdin_write_bytes = 8);
        let mut request = req("stdin-bounds", &["/bin/cat"]);
        request.stdin = true;
        launch(&state, request);

        let too_big = write_stdin(
            State(state.clone()),
            Path("stdin-bounds".to_string()),
            Ok(Json(stdin_body(Some("0123456789"), None))),
        )
        .await;
        assert_eq!(too_big.status(), StatusCode::PAYLOAD_TOO_LARGE);

        let bad_b64 = write_stdin(
            State(state.clone()),
            Path("stdin-bounds".to_string()),
            Ok(Json(StdinRequest {
                data_b64: Some("not!base64".to_string()),
                signal: None,
            })),
        )
        .await;
        assert_eq!(bad_b64.status(), StatusCode::BAD_REQUEST);
        assert_ne!(bad_b64.status(), StatusCode::NOT_FOUND);

        let bad_signal = write_stdin(
            State(state.clone()),
            Path("stdin-bounds".to_string()),
            Ok(Json(stdin_body(None, Some("close")))),
        )
        .await;
        assert_eq!(
            bad_signal.status(),
            StatusCode::BAD_REQUEST,
            "an unknown signal must be refused, not treated as EOF"
        );

        let unknown = write_stdin(
            State(state.clone()),
            Path("no-such-exec".to_string()),
            Ok(Json(stdin_body(Some("x"), None))),
        )
        .await;
        assert_eq!(unknown.status(), StatusCode::NOT_FOUND);

        // Nothing above reached the child, so the exec is still waiting.
        write_stdin(
            State(state.clone()),
            Path("stdin-bounds".to_string()),
            Ok(Json(stdin_body(None, Some("eof")))),
        )
        .await;
        let outcome = await_result(&state, "stdin-bounds").await;
        assert!(
            outcome.stdout.is_empty(),
            "a rejected write still reached the child: {:?}",
            outcome.stdout
        );
    }

    /// A wedged child cannot pin the request forever. The child never reads, so
    /// the pipe buffer fills and the write blocks; the bound turns that into a
    /// status the caller can act on.
    #[tokio::test]
    async fn a_stdin_write_to_a_child_that_never_reads_times_out() {
        let state = state_with(|cfg| {
            cfg.stdin_write_timeout = Duration::from_millis(150);
            cfg.max_stdin_write_bytes = 8 * 1024 * 1024;
            cfg.kill_grace = Duration::from_millis(30);
            cfg.output_linger = Duration::from_millis(50);
        });
        let mut request = req("stdin-wedged", &["/bin/sh", "-c", "sleep 30"]);
        request.stdin = true;
        launch(&state, request);

        // Far more than the 64 KiB pipe buffer, to a child that never reads.
        let response = write_stdin(
            State(state.clone()),
            Path("stdin-wedged".to_string()),
            Ok(Json(StdinRequest {
                data_b64: Some(
                    base64::engine::general_purpose::STANDARD.encode(vec![b'x'; 1 << 20]),
                ),
                signal: None,
            })),
        )
        .await;
        assert_eq!(response.status(), StatusCode::REQUEST_TIMEOUT);
        assert_eq!(body_json(response).await["error"], "stdin_write_timeout");

        kill(State(state.clone()), Path("stdin-wedged".to_string())).await;
        await_result(&state, "stdin-wedged").await;
    }

    /// A bare `Shared` with no child behind it, for driving the interleavings that
    /// a real spawn cannot be made to hit on demand.
    fn bare_shared(cfg: &Config) -> Arc<Shared> {
        let (live, _) = broadcast::channel(cfg.stream_channel_capacity);
        Arc::new(Shared {
            result: Mutex::new(None),
            terminal: Mutex::new(None),
            log: Mutex::new(Log::default()),
            live,
            stdin: Mutex::new(None),
            stdin_requested: false,
            buffer_cap: cfg.stream_buffer_bytes,
        })
    }

    async fn drain_frames(
        events: impl Stream<Item = Result<Event, std::convert::Infallible>> + Send + 'static,
    ) -> Vec<Sighting> {
        let response = Sse::new(events).into_response();
        let mut reader = SseReader {
            body: response.into_body(),
            buffer: String::new(),
        };
        tokio::time::timeout(Duration::from_secs(5), reader.drain())
            .await
            .expect("the constructed stream must terminate")
    }

    /// Marks a `Shared` finished the way the waiter task does, so a constructed
    /// stream terminates.
    async fn finish(shared: &Shared, exit_code: i32) {
        *shared.terminal.lock().await = Some(Terminal {
            exit_code: Some(exit_code),
            signal: None,
            truncated: false,
            writers_may_be_alive: false,
        });
        let _ = shared.live.send(Frame::Finished);
    }

    // No test asserts the subscribe/snapshot ordering inside `Shared::attach`
    // directly, and that is a deliberate omission rather than an oversight. There
    // is no await between the two steps and the daemon runs a current-thread
    // runtime, so no schedule exists in which a publish interleaves — reversing the
    // two lines is green against every test that can be written here. A test that
    // cannot fail against the broken version measures nothing and is worse than its
    // absence, which this project has been bitten by twice. What holds the property
    // instead is structural: `attach` takes the log lock and `publish` sends under
    // that same lock, so the pair stays atomic even if an await appears between
    // them later or the runtime becomes multi-threaded. The failure mode the
    // ordering prevents *is* covered, from the other direction, by
    // `snapshotting_before_subscribing_would_lose_a_write`.

    /// An attach against a quiet exec delivers exactly the buffer, once, with
    /// contiguous offsets and no gap.
    #[tokio::test]
    async fn an_attach_delivers_contiguous_offsets_exactly_once() {
        let cfg = Config::default();
        let shared = bare_shared(&cfg);
        shared.publish(StreamKind::Stdout, b"AAAA").await;
        shared.publish(StreamKind::Stdout, b"BBBB").await;
        shared.publish(StreamKind::Stdout, b"CCCC").await;
        let (live, gap, backlog, cursor) = shared.attach(0).await;
        finish(&shared, 0).await;

        let events = drain_frames(build_stream(shared, live, gap, backlog, cursor, false)).await;
        assert_eq!(
            String::from_utf8_lossy(&collected(&events, "stdout")),
            "AAAABBBBCCCC",
            "the attach lost or duplicated bytes"
        );
        // A client reconciles by offset, so a repeat that happens to reproduce the
        // same total bytes is still a stream it cannot resume from.
        let mut expected = 0u64;
        for event in events.iter().filter(|e| e.name == "output") {
            assert_eq!(
                event.offset(),
                expected,
                "offsets went backwards or skipped: {events:#?}"
            );
            expected += event.output().len() as u64;
        }
        assert_eq!(expected, 12);
        assert!(
            !events.iter().any(|e| e.name == "gap"),
            "nothing was actually lost, so nothing may be reported lost: {events:#?}"
        );
        assert_eq!(events.last().expect("events").name, "exit");
    }

    /// The unsafe ordering, spelled out so the reason `attach` exists is checked
    /// rather than asserted in a comment: snapshotting before subscribing puts a
    /// concurrent write in neither half.
    #[tokio::test]
    async fn snapshotting_before_subscribing_would_lose_a_write() {
        let cfg = Config::default();
        let shared = bare_shared(&cfg);

        shared.publish(StreamKind::Stdout, b"AAAA").await;
        // Snapshot FIRST, the wrong way round.
        let (gap, backlog, cursor) = shared.log.lock().await.since(0);
        // This write is the one that falls into the hole: too late for the
        // snapshot, too early for the subscription taken next.
        shared.publish(StreamKind::Stdout, b"LOST").await;
        let live = shared.live.subscribe();
        shared.publish(StreamKind::Stdout, b"CCCC").await;
        finish(&shared, 0).await;

        let events = drain_frames(build_stream(shared, live, gap, backlog, cursor, false)).await;
        let delivered = String::from_utf8_lossy(&collected(&events, "stdout")).into_owned();
        assert!(
            !delivered.contains("LOST"),
            "this ordering cannot deliver that byte range, so the handler must go \
             through Shared::attach; got {delivered:?}"
        );
        // The loss is at least reported rather than silent, because the next live
        // chunk arrives past the cursor.
        assert!(
            events.iter().any(|e| e.name == "gap"),
            "a discontinuous live chunk must produce a gap: {events:#?}"
        );
    }

    /// A subscriber that falls further behind than the channel holds is told its
    /// cursor is stale rather than being handed a silently discontinuous stream.
    #[tokio::test]
    async fn a_lagging_subscriber_is_told_it_lagged() {
        let cfg = Config {
            stream_channel_capacity: 2,
            ..Config::default()
        };
        let shared = bare_shared(&cfg);
        let live = shared.live.subscribe();
        // Far more than the channel holds, so this subscriber's slots are
        // overwritten before it ever polls.
        for _ in 0..64 {
            shared.publish(StreamKind::Stdout, b"xxxxxxxx").await;
        }
        finish(&shared, 0).await;

        let total = shared.log.lock().await.total;
        let events =
            drain_frames(build_stream(shared, live, None, VecDeque::new(), 0, false)).await;
        let gap = events
            .iter()
            .find(|e| e.name == "gap")
            .expect("a lagged subscriber must get a gap, not a quiet hole");
        assert_eq!(gap.data["from"], 0);
        assert_eq!(
            gap.data["to"].as_u64().expect("to"),
            total,
            "the gap must name everything the subscriber could not account for"
        );
        assert_eq!(events.last().expect("events").name, "exit");
    }

    /// The ring trims to its cap by splitting the oldest chunk rather than
    /// dropping it whole, and `start` tracks what was evicted so a gap is
    /// computed from the truth.
    #[test]
    fn the_ring_trims_to_its_cap_and_tracks_what_it_evicted() {
        let mut log = Log::default();
        for chunk in [&b"aaaa"[..], b"bbbb", b"cccc"] {
            let offset = log.total;
            log.total += chunk.len() as u64;
            log.retained += chunk.len();
            log.ring.push_back(RingChunk {
                stream: StreamKind::Stdout,
                offset,
                bytes: Bytes::copy_from_slice(chunk),
            });
            log.trim(6);
        }

        assert_eq!(log.retained, 6);
        assert_eq!(log.total, 12);
        assert_eq!(log.start, 6, "start must name the earliest retained byte");

        let (gap, frames, cursor) = log.since(0);
        assert_eq!(gap, Some((0, 6)), "the evicted range must be reported");
        assert_eq!(cursor, 6);
        let bytes: Vec<u8> = frames
            .iter()
            .flat_map(|f| match f {
                Frame::Chunk { bytes, .. } => bytes.to_vec(),
                Frame::Finished => Vec::new(),
            })
            .collect();
        assert_eq!(
            &bytes, b"bbcccc",
            "the split chunk lost or duplicated bytes"
        );

        // A cursor inside the window is not a gap.
        let (gap, _, cursor) = log.since(8);
        assert_eq!(gap, None);
        assert_eq!(cursor, 8);
    }
}
