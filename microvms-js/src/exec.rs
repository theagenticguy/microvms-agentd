// SPDX-License-Identifier: Apache-2.0
//! One exec, and the stream as a JS async iterator.
//!
//! # Async maps straight through, and that is the whole difference from the Python side
//!
//! Node is async-native, so there is no `block_on` bridge here and no GIL to release. An
//! exported `#[napi] pub async fn` runs on napi's managed tokio runtime and returns a
//! `Promise`; an `Err` rejects it with a JS `Error` whose `cause.message` is the core's
//! `ERR_*` code (see [`crate::errors`] for why the code is on the cause and not on `.code`
//! for an async rejection). That is why this file is shorter than its Python twin despite
//! covering the same surface: the core's async signatures *are* the JS signatures.
//!
//! # `&self` and not `&mut self`
//!
//! napi refuses `&mut self` in an async method without an `unsafe` marker, for a real
//! reason: the JS engine cannot track Rust mutability across an await, and Node also owns
//! `self`. Every method here takes `&self` and the core's `ExecHandle` needs nothing more —
//! it is an `Arc<Transport>` plus an id, and a poll is read-only server-side.
//!
//! # The stream is a real async generator
//!
//! `#[napi(async_iterator)]` plus an `AsyncGenerator` impl gives JS `for await (const event
//! of handle.stream())`. The trait's `next` must answer a `Send + 'static` future, so the
//! stream's consumer runs as a spawned task feeding a bounded channel and `next` awaits
//! `recv` — the same shape as the Python iterator, for the same reason: a `Stream` borrowing
//! the handle cannot be held across a return into the host language.
//!
//! Capacity 1 on the channel is deliberate. The daemon's SSE body is the backpressure
//! signal, and buffering a fast producer here would defeat the byte-offset cursor the core
//! reconnects at.
//!
//! # This file can drop `futures-util`, and the API to do it with already exists
//!
//! The spawned task below is a `StreamExt::next` loop whose whole body is "send, and return
//! if the receiver is gone". `microvms_core::session::ExecHandle::for_each_event` is exactly
//! that shape without the trait — `FnMut(ExecEvent) -> ControlFlow<()>`, where `Break` is
//! this loop's early `return`:
//!
//! ```ignore
//! let end = handle
//!     .for_each_event(options, |event| match sender.blocking_send(Ok(event)) {
//!         Ok(()) => ControlFlow::Continue(()),
//!         // The JS iterator was dropped, which is what `for await (…) break` does.
//!         Err(_) => ControlFlow::Break(()),
//!     })
//!     .await;
//! ```
//!
//! `microvms-cli` made that move and dropped the dependency (`tests/thinness.rs`'s `RETIRED`
//! records it). This crate has not, and the reason is the `blocking_send` above: the callback
//! is synchronous, so a full-channel wait would block the runtime thread the driver is
//! running on rather than yielding it — and capacity 1 means the channel is full whenever the
//! JS consumer is even slightly behind, which is the normal case. Closing that needs either
//! an async-callback overload in core (a `FnMut` returning a future, which is a different
//! signature and a different set of borrow problems) or an unbounded channel, and the second
//! is the thing the capacity-1 paragraph above refuses. So the dependency stays here on
//! purpose, and it stays for a reason about *backpressure* rather than about the trait.
//!
//! [`crate::cost`]'s `by_phase` took the same shape of fix on the smaller scale: core grew
//! `CostPhase::from_str` and both bindings' local copies came out. The pattern is the same
//! one — when two bindings and a CLI each hand-roll a thing, the thing belongs in core.

use std::sync::Arc;

use futures_util::StreamExt;
use microvms_core::session::{
    ExecEvent, ExecHandle as CoreHandle, ExecResult as CoreResult, StreamOptions,
};
use napi::bindgen_prelude::AsyncGenerator;
use napi_derive::napi;
use tokio::sync::Mutex;

use crate::errors::{AsyncError, js, js_async, to_napi};

/// The default wait for `wait`/`waitAndAck`, matching the Python client's 300s.
const DEFAULT_WAIT: f64 = 300.0;

/// An exec's phase and, once it has one, its outcome.
///
/// `#[napi(object)]` is right for this one: it is a pure result with no closure to protect
/// — there is no `ExecResult` a caller can construct wrongly, because no function takes one
/// — so a plain JS object with named fields is the friendlier shape and gives up nothing.
#[napi(object)]
pub struct ExecResult {
    pub exec_id: String,
    /// `"running"`, `"exited"`, or `"acked"`.
    pub phase: String,
    /// `null` when the child died to a signal rather than exiting.
    pub exit_code: Option<i32>,
    /// The signal that killed the child, when one did.
    pub signal: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    /// Set when either stream hit the output cap and was cut. A flag rather than a sentinel
    /// inside the bytes, which would be indistinguishable from output containing it.
    pub truncated: bool,
    /// Set when the post-exit linger deadline expired with the pipes still open: some
    /// grandchild is alive and may write more that nobody will see.
    pub writers_may_be_alive: bool,
    /// Whether the exec has finished, whichever way.
    pub done: bool,
    /// Whether the command exited zero. False for a signal death and for a still-running
    /// exec, since neither is a success.
    pub ok: bool,
}

impl ExecResult {
    pub(crate) fn wrap(result: CoreResult) -> Self {
        Self {
            phase: phase_str(result.phase).to_string(),
            exit_code: result.exit_code(),
            signal: result.outcome.as_ref().and_then(|outcome| outcome.signal),
            stdout: result.stdout().to_string(),
            stderr: result.stderr().to_string(),
            truncated: result
                .outcome
                .as_ref()
                .is_some_and(|outcome| outcome.truncated),
            writers_may_be_alive: result
                .outcome
                .as_ref()
                .is_some_and(|outcome| outcome.writers_may_be_alive),
            done: result.done(),
            ok: result.succeeded(),
            exec_id: result.exec_id,
        }
    }
}

/// What a stdin write accomplished.
#[napi(object)]
pub struct StdinAck {
    pub exec_id: String,
    pub written: u32,
    pub eof: bool,
}

/// One event off an exec's output stream.
///
/// One object with a `kind` discriminant rather than three classes, because JS has no
/// `instanceof` over a union a caller can `switch` on cleanly and the TypeScript idiom is a
/// tagged union. `kind` is `"output"`, `"gap"`, or `"exit"`, and the fields for the other
/// two shapes are `null` — which keeps [`Self::exit_code`] distinguishable from an output
/// chunk that happens to be last. The absence of an `exit` event is what tells a cut
/// connection from a finished command; the byte sequences are otherwise identical.
#[napi(object)]
pub struct StreamEvent {
    /// `"output"`, `"gap"`, or `"exit"`.
    pub kind: String,
    /// `"stdout"` or `"stderr"`, for an output event. Both share one offset space, so a
    /// caller holds one cursor rather than two that can disagree about ordering.
    pub stream: Option<String>,
    /// Where an output chunk starts, or where a gap starts.
    pub offset: Option<i64>,
    /// One past an output chunk's last byte, or a gap's exclusive end — either way, where a
    /// cursor resumes. `null` on an exit event, whose offset is a *total* rather than a
    /// position, so resuming from it would ask the daemon to replay from the end.
    pub end: Option<i64>,
    /// The bytes, for an output event.
    pub data: Option<napi::bindgen_prelude::Buffer>,
    /// The bytes as text, replacing anything undecodable. `data` is the lossless form.
    pub text: Option<String>,
    /// Total bytes published, on an exit event.
    pub total_offset: Option<i64>,
    pub exit_code: Option<i32>,
    pub signal: Option<i32>,
    pub truncated: Option<bool>,
    pub writers_may_be_alive: Option<bool>,
}

impl StreamEvent {
    fn empty(kind: &str) -> Self {
        Self {
            kind: kind.to_string(),
            stream: None,
            offset: None,
            end: None,
            data: None,
            text: None,
            total_offset: None,
            exit_code: None,
            signal: None,
            truncated: None,
            writers_may_be_alive: None,
        }
    }

    fn wrap(event: ExecEvent) -> Self {
        match event {
            ExecEvent::Output {
                stream,
                offset,
                data,
            } => {
                let end = offset + data.len() as u64;
                let text = String::from_utf8_lossy(&data).into_owned();
                Self {
                    stream: Some(stream_str(stream).to_string()),
                    offset: Some(offset as i64),
                    end: Some(end as i64),
                    text: Some(text),
                    data: Some(data.into()),
                    ..Self::empty("output")
                }
            }
            ExecEvent::Gap { from, to } => Self {
                offset: Some(from as i64),
                end: Some(to as i64),
                ..Self::empty("gap")
            },
            ExecEvent::Exit(exit) => Self {
                total_offset: Some(exit.offset as i64),
                exit_code: exit.exit_code,
                signal: exit.signal,
                truncated: Some(exit.truncated),
                writers_may_be_alive: Some(exit.writers_may_be_alive),
                ..Self::empty("exit")
            },
        }
    }
}

/// A JS async iterator over an exec's output.
///
/// See the module docs for why this is a task and a bounded channel. The receiver is behind
/// a tokio `Mutex` because `AsyncGenerator::next` must answer a `Send + 'static` future, so
/// the guard is taken *inside* that future rather than borrowed from `&mut self`.
#[napi(async_iterator)]
pub struct ExecStream {
    receiver: Arc<Mutex<tokio::sync::mpsc::Receiver<Result<ExecEvent, microvms_core::Error>>>>,
}

impl ExecStream {
    fn new(handle: Arc<CoreHandle>, options: StreamOptions) -> Self {
        // Capacity 1: the SSE body is the backpressure signal.
        let (sender, receiver) = tokio::sync::mpsc::channel(1);
        // Spawned on napi's managed runtime, which is the runtime every `#[napi] async fn`
        // in this crate already runs on — so there is no second runtime to keep in step.
        napi::tokio::spawn(async move {
            let mut stream = std::pin::pin!(handle.stream_with(options));
            while let Some(event) = stream.next().await {
                // A send failure means the JS iterator was dropped or `break`-ed out of.
                // Ending the task here is what stops a `break` leaving a task reading a
                // body nobody reads.
                if sender.send(event).await.is_err() {
                    return;
                }
            }
        });
        Self {
            receiver: Arc::new(Mutex::new(receiver)),
        }
    }
}

impl AsyncGenerator for ExecStream {
    type Yield = StreamEvent;
    type Next = ();
    type Return = ();

    fn next(
        &mut self,
        _value: Option<Self::Next>,
    ) -> impl std::future::Future<Output = napi::Result<Option<Self::Yield>>> + Send + 'static {
        let receiver = Arc::clone(&self.receiver);
        async move {
            let mut guard = receiver.lock().await;
            match guard.recv().await {
                Some(Ok(event)) => Ok(Some(StreamEvent::wrap(event))),
                // A stream error rejects the iteration rather than ending it silently — an
                // `OutputGap` with `errorOnGap` set is the case that matters, and a silent
                // end there would read as complete output.
                Some(Err(error)) => Err(napi::Error::new(
                    napi::Status::GenericFailure,
                    to_napi(&error).reason,
                )),
                None => Ok(None),
            }
        }
    }
}

/// How a stream should behave.
#[napi(object)]
pub struct StreamOptionsInput {
    /// The byte to start at. Non-zero resumes a stream a previous process was reading.
    pub offset: Option<i64>,
    /// Whether to reconnect after a cut. `false` ends the stream at the cut instead, which
    /// is what a caller doing its own reconnection wants.
    pub reconnect: Option<bool>,
    /// How many reconnects before giving up. A bound rather than forever, because a stream
    /// that drops every time is a condition a caller needs reported.
    pub max_reconnects: Option<u32>,
    /// Turns a gap into a rejection instead of an event. What a caller that must have
    /// complete output wants.
    pub error_on_gap: Option<bool>,
    /// How long the body may be silent before the connection is treated as dead.
    pub idle_timeout: Option<f64>,
}

/// One exec, addressed by its caller-minted id.
///
/// The id is the idempotency key, so a handle survives a process restart: rebuild it through
/// `Session.exec(execId)` and every method still addresses the same server-side exec.
#[napi]
pub struct ExecHandle {
    /// `Arc` because [`ExecStream`]'s task needs an owned handle and the core's `ExecHandle`
    /// is neither `Clone` nor constructible outside its own crate.
    inner: Arc<CoreHandle>,
}

impl ExecHandle {
    pub(crate) fn wrap(inner: CoreHandle) -> Self {
        Self {
            inner: Arc::new(inner),
        }
    }
}

#[napi]
impl ExecHandle {
    #[napi(getter)]
    pub fn exec_id(&self) -> String {
        self.inner.exec_id().to_string()
    }

    /// Reads current status and output. Read-only server-side; safe to spin on.
    #[napi]
    pub async fn poll(&self) -> Result<ExecResult, AsyncError> {
        Ok(ExecResult::wrap(self.inner.poll().await.map_err(js_async)?))
    }

    /// Polls until the exec is done, or rejects with `ERR_TIMEOUT`.
    ///
    /// A timeout has not touched the exec — polling is read-only and output lives until it
    /// is acked — so a caller that gives up can come back and poll again.
    #[napi]
    pub async fn wait(&self, timeout: Option<f64>) -> Result<ExecResult, AsyncError> {
        let timeout = seconds_async(timeout.unwrap_or(DEFAULT_WAIT))?;
        Ok(ExecResult::wrap(
            self.inner.wait(timeout).await.map_err(js_async)?,
        ))
    }

    /// An async iterator over output as it arrives, reconnecting at the last good offset.
    ///
    /// `for await (const event of handle.stream())`.
    #[napi]
    pub fn stream(&self, options: Option<StreamOptionsInput>) -> napi::Result<ExecStream, String> {
        let defaults = StreamOptions::default();
        let options = options.unwrap_or(StreamOptionsInput {
            offset: None,
            reconnect: None,
            max_reconnects: None,
            error_on_gap: None,
            idle_timeout: None,
        });
        let resolved = StreamOptions {
            offset: options.offset.unwrap_or(0).max(0) as u64,
            reconnect: options.reconnect.unwrap_or(defaults.reconnect),
            max_reconnects: options.max_reconnects.unwrap_or(defaults.max_reconnects),
            error_on_gap: options.error_on_gap.unwrap_or(defaults.error_on_gap),
            idle_timeout: match options.idle_timeout {
                Some(idle) => seconds(idle)?,
                None => defaults.idle_timeout,
            },
        };
        Ok(ExecStream::new(Arc::clone(&self.inner), resolved))
    }

    /// Writes to the child's stdin. Requires the exec to have been started with
    /// `stdin: true`, or the daemon answers 409.
    ///
    /// `eof` in the same call is the common case for feeding a prompt: two round trips would
    /// leave a window where the child has the bytes but not the EOF that says the input is
    /// complete.
    #[napi]
    pub async fn write_stdin(
        &self,
        data: napi::bindgen_prelude::Uint8Array,
        eof: Option<bool>,
    ) -> Result<StdinAck, AsyncError> {
        let ack = self
            .inner
            .write_stdin(&data, eof.unwrap_or(false))
            .await
            .map_err(js_async)?;
        Ok(StdinAck {
            exec_id: ack.exec_id,
            written: ack.written as u32,
            eof: ack.eof,
        })
    }

    /// Sends EOF. Nothing else closes stdin: the daemon's copy of the pipe outlives the
    /// child's wait, so a child blocked reading stdin hangs until its timeout otherwise.
    #[napi]
    pub async fn close_stdin(&self) -> Result<StdinAck, AsyncError> {
        let ack = self.inner.close_stdin().await.map_err(js_async)?;
        Ok(StdinAck {
            exec_id: ack.exec_id,
            written: ack.written as u32,
            eof: ack.eof,
        })
    }

    /// Releases the buffered output and starts the TTL clock.
    #[napi]
    pub async fn ack(&self) -> Result<ExecResult, AsyncError> {
        Ok(ExecResult::wrap(self.inner.ack().await.map_err(js_async)?))
    }

    /// Signals the whole process group. `false` means nothing was signalled because the
    /// child had already been reaped — which is the outcome a kill wanted.
    #[napi]
    pub async fn kill(&self) -> Result<bool, AsyncError> {
        self.inner.kill().await.map_err(js_async)
    }

    /// Wait, then ack, returning the result that carries the output.
    ///
    /// Which result comes back matters: the ack response carries the released output and a
    /// poll issued after the ack reports `acked` with none, so returning the wrong one is a
    /// silent empty-output bug. The core sequences it.
    #[napi]
    pub async fn wait_and_ack(&self, timeout: Option<f64>) -> Result<ExecResult, AsyncError> {
        let timeout = seconds_async(timeout.unwrap_or(DEFAULT_WAIT))?;
        Ok(ExecResult::wrap(
            self.inner.wait_and_ack(timeout).await.map_err(js_async)?,
        ))
    }
}

/// A `Duration` from a caller's number of seconds, for a synchronous function.
///
/// The core's `duration_of_secs_f64` is what refuses a negative or non-finite figure — this
/// is a call, not a check, which is the BIND-2 rule: the refusal and its message stay in one
/// place. It matters more here than in Python, because JS has `NaN` and `Infinity` as
/// ordinary values a caller reaches by accident.
pub(crate) fn seconds(value: f64) -> napi::Result<std::time::Duration, String> {
    microvms_core::cost::duration_of_secs_f64(value).map_err(js)
}

/// [`seconds`] for an async function, whose error type must be the local newtype.
pub(crate) fn seconds_async(value: f64) -> Result<std::time::Duration, AsyncError> {
    microvms_core::cost::duration_of_secs_f64(value).map_err(js_async)
}

fn phase_str(phase: protocol::exec::Phase) -> &'static str {
    match phase {
        protocol::exec::Phase::Running => "running",
        protocol::exec::Phase::Exited => "exited",
        protocol::exec::Phase::Acked => "acked",
    }
}

fn stream_str(kind: protocol::exec::StreamKind) -> &'static str {
    match kind {
        protocol::exec::StreamKind::Stdout => "stdout",
        protocol::exec::StreamKind::Stderr => "stderr",
    }
}
