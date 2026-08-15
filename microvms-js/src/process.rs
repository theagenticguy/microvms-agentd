// SPDX-License-Identifier: Apache-2.0
//! One exec as two byte streams, a `wait()`, and an idempotent `kill()`.
//!
//! # Shape-compatible with the AI SDK's `SandboxProcess`, and not an adapter
//!
//! [`ExecProcess`] deliberately matches `type SandboxProcess` from `@ai-sdk/harness`:
//! `stdout` and `stderr` as separate `ReadableStream<Uint8Array>`, `wait()` resolving to
//! `{ exitCode }`, an idempotent `kill()`, and an optional `pid`. An external provider wraps
//! one in a few lines and needs no translation layer:
//!
//! ```js
//! const proc = await session.spawn(['bash', '-lc', 'make test'], { cwd: '/work' });
//! return { stdout: proc.stdout, stderr: proc.stderr, wait: () => proc.wait(), kill: () => proc.kill() };
//! ```
//!
//! There is **no dependency on any harness package** here, and that is the point rather than
//! an omission. A dependency would put this crate's release cadence behind theirs and would
//! make their type the definition of ours, which is backwards: this handle's real contract is
//! the daemon's, and the harness shape is one consumer of it. So the name is neutral, the
//! types are this crate's own, and the compatibility is a documented property that a test
//! asserts structurally rather than a trait we implement.
//!
//! # Two streams out of one interleaved channel
//!
//! The daemon publishes stdout and stderr into **one** SSE stream with a `stream`
//! discriminator per output frame, sharing **one** offset space. That is not an
//! inconvenience to be undone; it is what makes the byte cursor work — a single cursor
//! cannot be split into two without inventing an ordering between them that the wire never
//! stated. So the demultiplexing happens here, at the last possible moment: one core drive,
//! one cursor, two channels chosen by the discriminator, two `ReadableStream`s over those.
//!
//! Interleaving order is therefore preserved *within* each stream and is not recoverable
//! *between* them, which is exactly the guarantee `SandboxProcess` makes and exactly what
//! two independent `ReadableStream`s can express. A consumer that needs the interleaving
//! reads [`crate::exec::ExecHandle::stream`] instead, which is still there and still the
//! richer surface.
//!
//! # Reconnect-at-cursor survives, and this is the property no other sandbox backend has
//!
//! The drive is `microvms_core`'s `for_each_event_async` with `StreamOptions` untouched, so a
//! stream cut by a MicroVM suspend/resume reconnects at the byte cursor and the two
//! `ReadableStream`s see a contiguous join rather than an end. That distinction is invisible
//! in a byte stream: a cut and a clean exit are the same absence of further bytes. Closing
//! the streams on a cut — which is what a naive `ReadableStream` bridge does, and what E2B's
//! cursorless `connect(pid)` does — turns a suspend into a silent truncation that a consumer
//! reports as success.
//!
//! Concretely: the streams close only on the terminal `exit` event or on an error, never
//! because one attach's body ended. [`ExecProcess::wait`] resolves from the daemon's exec
//! record rather than from "the stream stopped", so it cannot report an exit the daemon never
//! published.
//!
//! # A gap is surfaced as an error on the affected stream, and that is the whole decision
//!
//! When the daemon has evicted output, the wire says so with a `gap` frame. A
//! `ReadableStream<Uint8Array>` has exactly three things it can do with that: swallow it,
//! carry it out-of-band, or **error the stream**. This handle errors it, by default, and the
//! reasoning is that the other two options are each unsound for this consumer:
//!
//! * **Swallowing** hands the consumer a contiguous-looking byte stream that is missing
//!   bytes. That is the one failure the whole cursor protocol exists to prevent, and it is
//!   undetectable downstream — a truncated build log reads as a passing build.
//! * **An out-of-band event** (a `gapped` callback, or a field on the process) is
//!   *ignorable*, and a signal a consumer can ignore by writing the obvious code is a signal
//!   that will be ignored. The obvious harness-side code is
//!   `for await (const chunk of proc.stdout)`, which never looks at the process object again.
//!   Errors, by contrast, cannot be ignored: `for await` throws, `pipeTo` rejects.
//!
//! So the default is honest-by-construction rather than honest-if-read. The cost is
//! real and is why the option below exists: a caller who genuinely wants the surviving bytes
//! more than the completeness guarantee — a log tail, a progress display — passes
//! `gapPolicy: 'event'`, which delivers a [`ExecProcess::gaps`] record instead and leaves the
//! streams open. That choice is then *at the call site*, which is where the tradeoff is
//! visible, instead of buried in a default nobody chose.
//!
//! The error carries the byte range, so a caller can resume from it: `[from, to)` is
//! precisely what a fresh `stream({ offset: to })` would ask for.
//!
//! # Why the streams are built lazily and taken once
//!
//! `ReadableStream::new` needs an `&Env`, which exists only inside a JS call, so the streams
//! cannot be built in the constructor (which runs inside an `async fn`, off the JS thread).
//! Each getter therefore builds its stream on first read and hands out the same object
//! afterwards, which is also what the harness contract wants: `readonly stdout` is one
//! stream, and two calls returning two independent readers over one channel would split the
//! bytes between them.

use std::sync::{Arc, Mutex, PoisonError};

use microvms_core::session::{ExecEvent, ExecHandle as CoreHandle, StreamOptions};
use napi::bindgen_prelude::{Env, FromNapiValue, JsValue, ObjectRef, ReadableStream, Uint8Array};
use napi_derive::napi;

use crate::errors::{AsyncError, js_async};

/// The default `wait()` deadline, matching `ExecHandle.wait`'s.
const DEFAULT_WAIT: f64 = 300.0;

/// What to do when the daemon reports evicted output.
///
/// A string union rather than an enum class, because napi renders `#[napi(string_enum)]` as a
/// TypeScript union of literals — `'error' | 'event'`, which is what a JS caller writes anyway
/// — and there is no closure here to protect (see `lib.rs` on when a class is required
/// instead).
// `Copy` because the drive's per-event closure needs the policy on every event and a
// two-variant marker has nothing to own. Without it the closure would have to capture by move
// and could not be `FnMut`.
#[derive(Clone, Copy)]
#[napi(string_enum = "lowercase")]
pub enum GapPolicy {
    /// **The default.** A gap errors both streams, with the lost range in the message. See the
    /// module docs for why an error rather than an ignorable event.
    Error,
    /// A gap is recorded on [`ExecProcess::gaps`] and both streams stay open.
    ///
    /// For a caller that wants the surviving bytes more than the completeness guarantee. The
    /// consumer is then responsible for reading `gaps`, and nothing forces it to.
    Event,
}

/// One byte range the daemon could not replay.
///
/// `from` inclusive, `to` exclusive — so `to` is exactly the offset a resume would pass.
#[napi(object)]
pub struct OutputGap {
    /// Which stream lost bytes: `"stdout"` or `"stderr"`.
    ///
    /// Read off the *following* output frame rather than from the gap frame, which carries no
    /// discriminator: the wire's offset space is shared, so a gap is a hole in the combined
    /// stream and the daemon cannot say which side's bytes were in it. `null` when the gap was
    /// the last thing on the stream and nothing followed it to attribute it to.
    pub stream: Option<String>,
    pub from: i64,
    pub to: i64,
}

/// What `wait()` resolves to.
///
/// `exitCode` and nothing else on purpose, matching the harness contract's
/// `wait(): PromiseLike<{ exitCode: number }>`. A caller wanting the signal, the truncation
/// flag, or the buffered output polls [`crate::exec::ExecHandle`] instead, which answers with
/// the whole record.
#[napi(object)]
pub struct ProcessExit {
    /// The child's exit status.
    ///
    /// **`null` when the child died to a signal**, which is where this shape and the harness's
    /// `{ exitCode: number }` genuinely differ, and it is deliberate. A signal death has no
    /// exit code, and the two available lies are `0` (a killed build reported as passing) and
    /// `128 + signo` (a number the daemon never published, indistinguishable from a child that
    /// really exited with it). A wrapper that must produce a number can pick one at its own
    /// boundary; this handle will not pick for it.
    pub exit_code: Option<i32>,
    /// The signal that killed the child, when one did. Not part of the harness shape; here
    /// because it is the only thing that makes a `null` exit code actionable.
    pub signal: Option<i32>,
}

/// One recorded gap, before it becomes an [`OutputGap`].
///
/// A named struct rather than a `(Option<String>, u64, u64)`, because the tuple's first field is
/// the one a reader would guess wrong: it is the stream a *later* frame named, not one the gap
/// frame carried.
struct RecordedGap {
    /// The stream the following output frame belonged to, or `None` when nothing followed.
    attributed_to: Option<String>,
    from: u64,
    to: u64,
}

/// Which of the two channels an output frame belongs to.
fn is_stderr(stream: protocol::exec::StreamKind) -> bool {
    matches!(stream, protocol::exec::StreamKind::Stderr)
}

/// One demultiplexed channel: the receiver a `ReadableStream` will drain, or the stream once
/// it has been built.
enum Channel {
    /// Not yet read from JS.
    Pending(tokio::sync::mpsc::Receiver<napi::Result<Uint8Array>>),
    /// Built. The `ReadableStream` object lives in JS; this holds the reference that keeps
    /// the *same* object coming back out of the getter.
    Built(ObjectRef<false>),
    /// Being replaced. Never observable: only held across the swap inside one getter call.
    Empty,
}

/// A long-running exec as the AI SDK's `SandboxProcess` shape.
///
/// See the module docs. Built by `Session.spawn`, never by a constructor: a process object
/// with no exec behind it is one whose every method fails in a way that looks like a dead VM.
#[napi]
pub struct ExecProcess {
    /// Kept so `kill()` and `wait()` address the exec, and so `execId` can be read back for a
    /// reattach.
    handle: Arc<CoreHandle>,
    stdout: Mutex<Channel>,
    stderr: Mutex<Channel>,
    gaps: Arc<Mutex<Vec<RecordedGap>>>,
}

impl ExecProcess {
    /// Starts the demultiplexing drive and returns the handle over it.
    ///
    /// The drive is spawned here rather than on first stream read, which is the difference
    /// between a process that is running and one that is only *described*: the harness may
    /// `kill()` a spawned process without ever reading a byte, and a drive that had not
    /// started would leave the daemon's SSE body unattached until then.
    pub(crate) fn start(
        handle: Arc<CoreHandle>,
        options: StreamOptions,
        policy: GapPolicy,
    ) -> Self {
        // Capacity 1 on each, for the reason `exec.rs` gives: the daemon's SSE body is the
        // backpressure signal, and buffering here would defeat the byte cursor the core
        // reconnects at. Per-channel rather than shared, because a consumer that reads stdout
        // and ignores stderr must not deadlock — with one shared channel a full stderr would
        // stall stdout, and "the process hangs when you don't read stderr" is the classic
        // subprocess bug this shape exists to avoid. The cost is that a *wholly* unread
        // channel still stalls the drive at one buffered chunk, which is the same bound a
        // single pipe has.
        let (out_tx, out_rx) = tokio::sync::mpsc::channel(1);
        let (err_tx, err_rx) = tokio::sync::mpsc::channel(1);
        let gaps = Arc::new(Mutex::new(Vec::new()));

        let drive_handle = Arc::clone(&handle);
        let drive_gaps = Arc::clone(&gaps);
        // Gaps seen but not yet attributed to a stream. Behind an `Arc<Mutex<..>>` rather than
        // a plain local, because core's callback future is a plain type parameter that cannot
        // name a borrow of the closure's captures — so state that spans events has to be
        // shared by handle. Per-`ExecProcess`, never a static: a global would attribute one
        // exec's gap to another's stream.
        let unattributed: Arc<Mutex<Vec<(u64, u64)>>> = Arc::new(Mutex::new(Vec::new()));
        let drive_unattributed = Arc::clone(&unattributed);
        // `napi::bindgen_prelude::spawn` and **not** `napi::tokio::spawn`: this is called from
        // an `async fn` today, but `exec.rs`'s note records that the latter needs an ambient
        // runtime and aborts the process without one. Same submission path as every other
        // drive in this crate.
        napi::bindgen_prelude::spawn(async move {
            let end = drive_handle
                .for_each_event_async(options, |event| {
                    // Cloned per event rather than borrowed, for the reason above. One atomic
                    // increment each.
                    let out_tx = out_tx.clone();
                    let err_tx = err_tx.clone();
                    let gaps = Arc::clone(&drive_gaps);
                    let unattributed = Arc::clone(&drive_unattributed);
                    let error_on_gap = matches!(policy, GapPolicy::Error);
                    async move {
                        match event {
                            ExecEvent::Output { stream, data, .. } => {
                                // A gap seen earlier is attributed to *this* frame's stream,
                                // which is the closest thing the wire supports: the bytes that
                                // resumed after the hole came out of one side, and that side is
                                // the one whose log now has a hole in it.
                                let held: Vec<(u64, u64)> = unattributed
                                    .lock()
                                    .unwrap_or_else(PoisonError::into_inner)
                                    .drain(..)
                                    .collect();
                                if !held.is_empty() {
                                    let name = stream.as_str().to_string();
                                    let mut recorded =
                                        gaps.lock().unwrap_or_else(PoisonError::into_inner);
                                    for (from, to) in held {
                                        recorded.push(RecordedGap {
                                            attributed_to: Some(name.clone()),
                                            from,
                                            to,
                                        });
                                    }
                                }
                                let target = if is_stderr(stream) { &err_tx } else { &out_tx };
                                // `.await`ed rather than `try_send`: capacity 1 means the
                                // channel is full whenever the JS reader is behind, which is
                                // the normal case, and dropping there would lose output the
                                // cursor believes was delivered.
                                match target.send(Ok(Uint8Array::new(data))).await {
                                    Ok(()) => std::ops::ControlFlow::Continue(()),
                                    // The reader was cancelled or GC'd. Ending the drive is what
                                    // stops a task reading a body nobody reads — and, because
                                    // the other channel shares this drive, it is also why a
                                    // consumer must not cancel one stream and keep reading the
                                    // other.
                                    Err(_) => std::ops::ControlFlow::Break(()),
                                }
                            }
                            ExecEvent::Gap { from, to } => {
                                if error_on_gap {
                                    // **Both** streams, because the wire cannot say which side
                                    // lost the bytes: the offset space is shared, so erroring
                                    // only one would leave the other looking complete when it
                                    // may be the truncated one. The message carries the range,
                                    // so a caller can resume at `to`.
                                    let message = format!(
                                        "output bytes [{from}, {to}) are unrecoverable: the \
                                         daemon evicted them before this client read them. \
                                         Resume from offset {to}, or pass gapPolicy: 'event' \
                                         to keep the surviving bytes instead."
                                    );
                                    let _ = out_tx
                                        .send(Err(napi::Error::new(
                                            napi::Status::GenericFailure,
                                            message.clone(),
                                        )))
                                        .await;
                                    let _ = err_tx
                                        .send(Err(napi::Error::new(
                                            napi::Status::GenericFailure,
                                            message,
                                        )))
                                        .await;
                                    // Break, so nothing more is pushed into streams that have
                                    // already errored.
                                    std::ops::ControlFlow::Break(())
                                } else {
                                    // Held rather than recorded now: the stream it belongs to is
                                    // named by the *next* output frame.
                                    unattributed
                                        .lock()
                                        .unwrap_or_else(PoisonError::into_inner)
                                        .push((from, to));
                                    std::ops::ControlFlow::Continue(())
                                }
                            }
                            // Nothing is sent for the terminal event: closing the channels is
                            // what ends the streams, and dropping the senders when this task
                            // returns is what closes them. `wait()` is where the exit code
                            // comes from, because the daemon's record is the only thing that can
                            // distinguish an exit from a cut.
                            ExecEvent::Exit(_) => std::ops::ControlFlow::Continue(()),
                        }
                    }
                })
                .await;

            // A gap the stream ended on has no following frame to attribute it to, so it is
            // recorded with `stream: null` rather than guessed at.
            let leftover: Vec<(u64, u64)> = unattributed
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .drain(..)
                .collect();
            if !leftover.is_empty() {
                let mut recorded = drive_gaps.lock().unwrap_or_else(PoisonError::into_inner);
                for (from, to) in leftover {
                    recorded.push(RecordedGap {
                        attributed_to: None,
                        from,
                        to,
                    });
                }
            }

            // A drive error reaches the consumer as a stream error, so an exhausted reconnect
            // budget rejects the read rather than ending it — the same rule `exec.rs` states,
            // and for the same reason: a silent end reads as complete output.
            if let Err(error) = end {
                let message = error.to_string();
                let _ = out_tx
                    .send(Err(napi::Error::new(
                        napi::Status::GenericFailure,
                        message.clone(),
                    )))
                    .await;
                let _ = err_tx
                    .send(Err(napi::Error::new(napi::Status::GenericFailure, message)))
                    .await;
            }
        });

        Self {
            handle,
            stdout: Mutex::new(Channel::Pending(out_rx)),
            stderr: Mutex::new(Channel::Pending(err_rx)),
            gaps,
        }
    }

    /// Builds a channel's `ReadableStream` on first read and returns the same object after.
    fn stream_of<'env>(
        env: &'env Env,
        slot: &Mutex<Channel>,
        which: &str,
    ) -> napi::Result<ReadableStream<'env, Uint8Array>> {
        let mut guard = slot.lock().unwrap_or_else(PoisonError::into_inner);
        let reference = match std::mem::replace(&mut *guard, Channel::Empty) {
            Channel::Pending(receiver) => {
                let stream: ReadableStream<'env, Uint8Array> = ReadableStream::new(
                    env,
                    napi::tokio_stream::wrappers::ReceiverStream::new(receiver),
                )?;
                // Referenced so the *same* object comes back next time. Through `Object`
                // because `create_ref` lives there and a `ReadableStream` *is* a JS object —
                // `from_raw` is a view over the same `napi_value`, not a conversion.
                //
                // `LEAK_CHECK` off deliberately: the check exists to catch an `ObjectRef` a
                // method forgot to unref before returning, and this one is owned by the
                // `ExecProcess` for its whole life (see this type's `Drop`).
                napi::bindgen_prelude::Object::from_raw(env.raw(), stream.raw())
                    .create_ref::<false>()?
            }
            Channel::Built(reference) => reference,
            Channel::Empty => {
                // Unreachable through the getters, which hold the lock across the whole swap.
                // Reported rather than `unreachable!`, because a panic here crosses the FFI
                // boundary and takes Node with it (see `exec.rs` on `failed to initiate panic`).
                return Err(napi::Error::new(
                    napi::Status::GenericFailure,
                    format!("the {which} stream slot was left empty by a failed build"),
                ));
            }
        };
        let value = reference.get_value(env)?;
        let raw = value.raw();
        *guard = Channel::Built(reference);
        // SAFETY: `raw` is the `ReadableStream` object this function itself constructed (or
        // re-fetched from the reference to it), so the type is right by construction. napi's
        // own `FromNapiValue` for `ReadableStream` is an unchecked rewrap for exactly this.
        unsafe { ReadableStream::<Uint8Array>::from_napi_value(env.raw(), raw) }
    }
}

impl Drop for ExecProcess {
    /// Releases the two stream references.
    ///
    /// `napi_delete_reference` needs an `Env` and `Drop` has none, so this leans on the
    /// `LEAK_CHECK = false` variant: the reference is a strong one, and dropping it without
    /// unreffing leaves the `ReadableStream` object reachable until the environment tears down.
    /// That is a bounded, per-process leak of one JS object rather than of the stream's buffers
    /// — the `ReceiverStream` behind it is owned by the stream's own finalizer state — and the
    /// alternative is a `Drop` that cannot exist, since napi hands no `Env` to one.
    ///
    /// Recorded rather than silently accepted, because the honest description of this is "an
    /// `ObjectRef` per stream is retained for the life of the addon", and a reader deserves to
    /// know that without reading napi's source.
    fn drop(&mut self) {
        for slot in [&self.stdout, &self.stderr] {
            let mut guard = slot.lock().unwrap_or_else(PoisonError::into_inner);
            if let Channel::Built(_) = &*guard {
                // Left in place: `ObjectRef<false>` drops without complaining, which is what
                // the `false` selects.
            }
            *guard = Channel::Empty;
        }
    }
}

#[napi]
impl ExecProcess {
    /// The exec id, which is the idempotency key.
    ///
    /// Read it back to reattach from another process: `session.exec(id)` addresses the same
    /// server-side exec, and `session.spawn` with the same id does not start a second child.
    #[napi(getter)]
    pub fn exec_id(&self) -> String {
        self.handle.exec_id().to_string()
    }

    /// The process id, if the sandbox exposes one.
    ///
    /// **Always `null`**, and that is a true answer rather than a stub. The daemon signals a
    /// whole process group and addresses an exec by its caller-minted id; it publishes no pid,
    /// because a pid inside a MicroVM is not meaningful outside it and would invite a caller to
    /// signal it directly through a channel that does not exist. The harness contract marks
    /// `pid` optional for exactly this case.
    #[napi(getter)]
    pub fn pid(&self) -> Option<u32> {
        None
    }

    /// The child's standard output as a stream of bytes.
    ///
    /// One stream per process: reading this twice hands back the same object, because two
    /// readers over one channel would split the bytes between them.
    #[napi(getter)]
    pub fn stdout<'env>(&self, env: &'env Env) -> napi::Result<ReadableStream<'env, Uint8Array>> {
        Self::stream_of(env, &self.stdout, "stdout")
    }

    /// The child's standard error as a stream of bytes.
    ///
    /// Independent of [`Self::stdout`]: order is preserved within each and is not recoverable
    /// between them, which is what the wire supports (one shared offset space) and what the
    /// harness shape promises.
    #[napi(getter)]
    pub fn stderr<'env>(&self, env: &'env Env) -> napi::Result<ReadableStream<'env, Uint8Array>> {
        Self::stream_of(env, &self.stderr, "stderr")
    }

    /// Every byte range the daemon could not replay, under `gapPolicy: 'event'`.
    ///
    /// Empty under the default policy, where a gap errors the streams instead — see the module
    /// docs for why the ignorable form is not the default.
    #[napi(getter)]
    pub fn gaps(&self) -> Vec<OutputGap> {
        self.gaps
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .map(|gap| OutputGap {
                stream: gap.attributed_to.clone(),
                from: gap.from as i64,
                to: gap.to as i64,
            })
            .collect()
    }

    /// Resolves when the process exits.
    ///
    /// **From the daemon's exec record, not from the streams ending**, and that is the whole
    /// reason a suspend/resume does not look like a clean exit here: a stream that stopped
    /// carrying bytes is a stream that stopped, which is the identical observation for a cut
    /// connection and a finished command. Only the daemon's record distinguishes them.
    ///
    /// Rejects with `ERR_TIMEOUT` past `timeoutSeconds` (300 by default). A timeout has not
    /// touched the exec — polling is read-only and the output lives until it is acked — so a
    /// caller that gives up can call this again.
    #[napi]
    pub async fn wait(&self, timeout_seconds: Option<f64>) -> Result<ProcessExit, AsyncError> {
        let timeout = crate::exec::seconds_async(timeout_seconds.unwrap_or(DEFAULT_WAIT))?;
        let result = self.handle.wait(timeout).await.map_err(js_async)?;
        Ok(ProcessExit {
            exit_code: result.exit_code(),
            signal: result.outcome.as_ref().and_then(|outcome| outcome.signal),
        })
    }

    /// Terminates the process. Idempotent.
    ///
    /// Signals the whole process group rather than the direct child, so a shell's children go
    /// with it. Idempotent because the daemon answers 200 either way: a group that had already
    /// been reaped reports nothing signalled, which is the outcome a kill wanted — so a second
    /// call is a success and not a 404. That is what lets a harness call this in a `finally`
    /// without guarding it.
    #[napi]
    pub async fn kill(&self) -> Result<(), AsyncError> {
        self.handle.kill().await.map_err(js_async)?;
        Ok(())
    }
}
