// SPDX-License-Identifier: Apache-2.0
//! One exec, and the stream as a Python iterator.
//!
//! # The stream is the one shape that needed real work
//!
//! Everything else on this surface is "call an async method, block on it". A stream is
//! not: driving one means holding a future across a Python `__next__` that has to return
//! between items, and a borrow of the handle cannot outlive the call.
//!
//! The shape that works is a task and a channel. [`ExecStream::new`] spawns the stream's
//! driver onto the shared runtime with an owned [`microvms_core::session::ExecHandle`]
//! and a bounded `mpsc` sender, and `__next__` blocks on `recv`. The bound is 1, which is
//! deliberate: the daemon's SSE body is the backpressure signal, and an unbounded channel
//! would buffer a fast producer's whole output in the binding while the Python consumer
//! fell behind — which is the failure the core's byte-offset cursor exists to make
//! unnecessary.
//!
//! Dropping the iterator drops the receiver, the next `send` fails, and the drive ends on
//! `ControlFlow::Break`. That is what `for event in handle.stream(): break` has to do, and
//! it is why the task owns everything it touches rather than borrowing from the handle.
//!
//! # Events are classes, not tuples
//!
//! `ExecEvent` in the core is an enum with three shapes. A Python caller gets three
//! classes plus a `kind` tag, so `isinstance` and a `kind` check both work, and the
//! `Exit` event stays distinguishable from an output chunk that happens to be last —
//! which the core's docs call out as the difference between a finished command and a cut
//! connection.
//!
//! # The stream is driven by core's async callback driver
//!
//! [`ExecStream::new`]'s task calls `microvms_core::session::ExecHandle::for_each_event_async`
//! and `.await`s its capacity-1 `send` inside the callback, where `ControlFlow::Break` is the
//! dropped-iterator case. The `Stream` path — `stream_with` plus `StreamExt::next` — was
//! retired here on 2026-08-09, and `futures-util` came out of this crate's manifest with it.
//! The sync driver could not have served this: its only available send is `blocking_send`,
//! which would park the runtime worker the driver runs on, and with capacity 1 that is every
//! event the Python consumer has not drained yet.
//!
//! [`crate::cost`]'s `by_phase` took the same shape of fix at a smaller scale: core grew
//! `CostPhase::from_str` and both bindings' hand-rolled phase tables came out.

use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use microvms_core::session::{ExecEvent, ExecHandle, ExecResult, StreamOptions};
use pyo3::prelude::*;
use pyo3::types::PyBytes;

use crate::errors::{PyCoreResult, to_py_err};
use crate::runtime;

/// The default wait for `wait`/`wait_and_ack`, matching the Python client's 300s.
const DEFAULT_WAIT: f64 = 300.0;

/// An exec's phase and, once it has one, its outcome.
///
/// `stdout` and `stderr` are `str` because the daemon's `Outcome` carries them as strings
/// — the protocol crate's shape, not a choice made here. `exit_code` and `signal` are
/// `None` rather than sentinel integers, which is the same distinction `models.py` makes:
/// a signal death has no exit code, and zero is not "no signal".
#[pyclass(frozen, name = "ExecResult", module = "microvms")]
pub struct PyExecResult {
    exec_id: String,
    phase: &'static str,
    exit_code: Option<i32>,
    signal: Option<i32>,
    stdout: String,
    stderr: String,
    truncated: bool,
    writers_may_be_alive: bool,
    done: bool,
    succeeded: bool,
}

impl PyExecResult {
    pub(crate) fn wrap(result: ExecResult) -> Self {
        Self {
            phase: phase_str(result.phase),
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
            succeeded: result.succeeded(),
            exec_id: result.exec_id,
        }
    }
}

#[pymethods]
impl PyExecResult {
    #[getter]
    fn exec_id(&self) -> &str {
        &self.exec_id
    }

    /// `"running"`, `"exited"`, or `"acked"`.
    #[getter]
    fn phase(&self) -> &'static str {
        self.phase
    }

    /// `None` when the child died to a signal rather than exiting.
    #[getter]
    fn exit_code(&self) -> Option<i32> {
        self.exit_code
    }

    /// The signal that killed the child, when one did.
    #[getter]
    fn signal(&self) -> Option<i32> {
        self.signal
    }

    #[getter]
    fn stdout(&self) -> &str {
        &self.stdout
    }

    #[getter]
    fn stderr(&self) -> &str {
        &self.stderr
    }

    /// Set when either stream hit the output cap and was cut. A flag rather than a
    /// sentinel inside the bytes, which would be indistinguishable from output that
    /// happens to contain it.
    #[getter]
    fn truncated(&self) -> bool {
        self.truncated
    }

    /// Set when the post-exit linger deadline expired with the pipes still open: some
    /// grandchild is alive and may write more that nobody will see.
    #[getter]
    fn writers_may_be_alive(&self) -> bool {
        self.writers_may_be_alive
    }

    /// Whether the exec has finished, whichever way.
    #[getter]
    fn done(&self) -> bool {
        self.done
    }

    /// Whether the command exited zero. False for a signal death and for a still-running
    /// exec, since neither is a success.
    #[getter]
    fn ok(&self) -> bool {
        self.succeeded
    }

    fn __repr__(&self) -> String {
        format!(
            "ExecResult(exec_id={:?}, phase={:?}, exit_code={:?})",
            self.exec_id, self.phase, self.exit_code
        )
    }
}

/// What a stdin write accomplished.
#[pyclass(frozen, name = "StdinAck", module = "microvms")]
pub struct PyStdinAck {
    exec_id: String,
    written: usize,
    eof: bool,
}

#[pymethods]
impl PyStdinAck {
    #[getter]
    fn exec_id(&self) -> &str {
        &self.exec_id
    }

    #[getter]
    fn written(&self) -> usize {
        self.written
    }

    #[getter]
    fn eof(&self) -> bool {
        self.eof
    }

    fn __repr__(&self) -> String {
        format!(
            "StdinAck(exec_id={:?}, written={}, eof={})",
            self.exec_id, self.written, self.eof
        )
    }
}

/// Output bytes, with the offset they start at.
///
/// `data` is `bytes` and not `str`: exec output is arbitrary bytes, and a decode here
/// would be a lossy step the caller cannot see. `end` is where a cursor resumes.
#[pyclass(frozen, name = "OutputChunk", module = "microvms")]
pub struct PyOutputChunk {
    stream: &'static str,
    offset: u64,
    data: Vec<u8>,
}

#[pymethods]
impl PyOutputChunk {
    /// `"output"` — the tag beside `isinstance`, so a caller can branch either way.
    #[getter]
    fn kind(&self) -> &'static str {
        "output"
    }

    /// `"stdout"` or `"stderr"`. Both share one offset space, so a caller holds one
    /// cursor rather than two that can disagree about ordering.
    #[getter]
    fn stream(&self) -> &'static str {
        self.stream
    }

    #[getter]
    fn offset(&self) -> u64 {
        self.offset
    }

    #[getter]
    fn data<'py>(&self, py: Python<'py>) -> Bound<'py, PyBytes> {
        PyBytes::new(py, &self.data)
    }

    /// One past this chunk's last byte: `offset + len(data)`.
    #[getter]
    fn end(&self) -> u64 {
        self.offset + self.data.len() as u64
    }

    /// The bytes as text, replacing anything undecodable.
    ///
    /// A method rather than a getter on `data`, so the lossy step is a call a reader sees
    /// — the same reasoning as the core's `break_even_seconds_f64`.
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.data).into_owned()
    }

    fn __repr__(&self) -> String {
        format!(
            "OutputChunk(stream={:?}, offset={}, len={})",
            self.stream,
            self.offset,
            self.data.len()
        )
    }
}

/// A byte range that is gone for good — the replay ring evicted it, or this subscriber
/// lagged the live channel.
///
/// A typed event rather than a log line, because the alternative is reading a truncated
/// log as a complete one. `start` is inclusive and `end` exclusive, so `end` is where a
/// cursor resumes.
#[pyclass(frozen, name = "Gap", module = "microvms")]
pub struct PyGap {
    start: u64,
    end: u64,
}

#[pymethods]
impl PyGap {
    #[getter]
    fn kind(&self) -> &'static str {
        "gap"
    }

    #[getter]
    fn start(&self) -> u64 {
        self.start
    }

    #[getter]
    fn end(&self) -> u64 {
        self.end
    }

    #[getter]
    fn size(&self) -> u64 {
        self.end.saturating_sub(self.start)
    }

    fn __repr__(&self) -> String {
        format!("Gap(start={}, end={})", self.start, self.end)
    }
}

/// The terminal event. Its **absence** is what distinguishes a cut connection from a
/// finished command — the byte sequences are otherwise identical.
#[pyclass(frozen, name = "Exit", module = "microvms")]
pub struct PyExit {
    exit_code: Option<i32>,
    signal: Option<i32>,
    truncated: bool,
    writers_may_be_alive: bool,
    offset: u64,
}

#[pymethods]
impl PyExit {
    #[getter]
    fn kind(&self) -> &'static str {
        "exit"
    }

    #[getter]
    fn exit_code(&self) -> Option<i32> {
        self.exit_code
    }

    #[getter]
    fn signal(&self) -> Option<i32> {
        self.signal
    }

    #[getter]
    fn truncated(&self) -> bool {
        self.truncated
    }

    #[getter]
    fn writers_may_be_alive(&self) -> bool {
        self.writers_may_be_alive
    }

    /// Total bytes published. A total rather than a position to resume at, which is why
    /// the core's `ExecEvent::end()` answers `None` here.
    #[getter]
    fn offset(&self) -> u64 {
        self.offset
    }

    fn __repr__(&self) -> String {
        format!(
            "Exit(exit_code={:?}, signal={:?}, offset={})",
            self.exit_code, self.signal, self.offset
        )
    }
}

/// One core event as the Python object for its shape.
fn event_to_py(py: Python<'_>, event: ExecEvent) -> PyResult<Py<PyAny>> {
    match event {
        ExecEvent::Output {
            stream,
            offset,
            data,
        } => Ok(Py::new(
            py,
            PyOutputChunk {
                stream: stream_str(stream),
                offset,
                data,
            },
        )?
        .into_any()),
        ExecEvent::Gap { from, to } => Ok(Py::new(
            py,
            PyGap {
                start: from,
                end: to,
            },
        )?
        .into_any()),
        ExecEvent::Exit(exit) => Ok(Py::new(
            py,
            PyExit {
                exit_code: exit.exit_code,
                signal: exit.signal,
                truncated: exit.truncated,
                writers_may_be_alive: exit.writers_may_be_alive,
                offset: exit.offset,
            },
        )?
        .into_any()),
    }
}

/// A Python iterator over an exec's output.
///
/// See the module docs for why this is a task and a bounded channel rather than a stored
/// future. `receiver` is behind a `Mutex` because `#[pyclass]` methods take `&self` when
/// the class is shared, and `recv` needs `&mut`; the lock is held only across one `recv`
/// and never across a Python callback, so it cannot deadlock against the GIL.
#[pyclass(name = "ExecStream", module = "microvms")]
pub struct ExecStream {
    receiver: Mutex<tokio::sync::mpsc::Receiver<Result<ExecEvent, microvms_core::Error>>>,
}

impl ExecStream {
    /// Spawns the stream's consumer and returns the iterator that drains it.
    ///
    /// `handle` arrives as an `Arc` rather than by value because
    /// [`microvms_core::session::ExecHandle`] is neither `Clone` nor constructible outside
    /// its own crate — see the note in the packet — so the only way for the task to own
    /// something it can call `stream_with` on is to share the one handle. The borrow the
    /// stream takes lives inside the `async move` block, which owns the `Arc`.
    fn new(handle: Arc<ExecHandle>, options: StreamOptions) -> Self {
        // Capacity 1: the SSE body is the backpressure signal, and buffering a fast
        // producer here would defeat the cursor the core reconnects at.
        let (sender, receiver) = tokio::sync::mpsc::channel(1);
        runtime::handle().spawn(async move {
            let end = handle
                .for_each_event_async(options, |event| {
                    // Cloned per event rather than borrowed: core's callback future is a plain
                    // type parameter, which cannot name a borrow of this closure's captures —
                    // see `for_each_event_async`'s docs for why that signature and not
                    // `AsyncFnMut`. One atomic increment per event.
                    let sender = sender.clone();
                    async move {
                        // `.await`ed, not `blocking_send`ed. With capacity 1 the channel is
                        // full whenever the Python consumer is even slightly behind, and
                        // blocking here would park the runtime worker this driver runs on —
                        // which is the whole reason core grew the async overload.
                        match sender.send(Ok(event)).await {
                            Ok(()) => std::ops::ControlFlow::Continue(()),
                            // The Python iterator was dropped. `Break` ends the drive, which
                            // is what makes `break` out of a `for` loop stop the stream rather
                            // than leave a task reading a body nobody reads.
                            Err(_) => std::ops::ControlFlow::Break(()),
                        }
                    }
                })
                .await;
            // A stream error is delivered as an item so `__next__` raises it. The events
            // already sent stay sent: the bytes a caller received are real output, and the
            // asymmetry is the driver's own documented one.
            if let Err(error) = end {
                let _ = sender.send(Err(error)).await;
            }
        });
        Self {
            receiver: Mutex::new(receiver),
        }
    }
}

#[pymethods]
impl ExecStream {
    fn __iter__(slf: PyRef<'_, Self>) -> PyRef<'_, Self> {
        slf
    }

    /// The next event, or `StopIteration` when the stream ends.
    ///
    /// Blocks with the GIL released, so another Python thread can run while this one
    /// waits on the daemon.
    fn __next__(&self, py: Python<'_>) -> PyResult<Option<Py<PyAny>>> {
        let received = py.detach(|| {
            let mut receiver = self
                .receiver
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            runtime::block_on_detached(receiver.recv())
        });
        match received {
            Some(Ok(event)) => Ok(Some(event_to_py(py, event)?)),
            Some(Err(error)) => Err(to_py_err(py, &error)),
            None => Ok(None),
        }
    }
}

/// One exec, addressed by its caller-minted id.
///
/// The id is the idempotency key, so a handle survives a process restart: rebuild it
/// through `Session.exec(exec_id)` and every method still addresses the same server-side
/// exec.
#[pyclass(frozen, name = "ExecHandle", module = "microvms")]
pub struct PyExecHandle {
    /// `Arc` because [`ExecStream`] needs an owned handle for its task and cloning the
    /// core handle is a clone of an `Arc<Transport>` plus a `String`.
    inner: Arc<ExecHandle>,
}

impl PyExecHandle {
    pub(crate) fn wrap(inner: ExecHandle) -> Self {
        Self {
            inner: Arc::new(inner),
        }
    }
}

#[pymethods]
impl PyExecHandle {
    #[getter]
    fn exec_id(&self) -> &str {
        self.inner.exec_id()
    }

    /// Reads current status and output. Read-only server-side; safe to spin on.
    fn poll(&self, py: Python<'_>) -> PyCoreResult<PyExecResult> {
        Ok(PyExecResult::wrap(runtime::block_on(
            py,
            self.inner.poll(),
        )?))
    }

    /// Polls until the exec is done, or raises `TimeoutError`.
    ///
    /// A timeout has not touched the exec — polling is read-only and output lives until
    /// it is acked — so a caller that gives up can come back and poll again.
    #[pyo3(signature = (timeout=DEFAULT_WAIT))]
    fn wait(&self, py: Python<'_>, timeout: f64) -> PyCoreResult<PyExecResult> {
        let timeout = seconds(timeout)?;
        Ok(PyExecResult::wrap(runtime::block_on(
            py,
            self.inner.wait(timeout),
        )?))
    }

    /// An iterator over output as it arrives, reconnecting at the last good offset.
    ///
    /// `error_on_gap=True` turns an evicted byte range into an exception instead of a
    /// `Gap` event, which is what a caller that must have complete output wants.
    /// `reconnect=False` ends the iterator at a cut instead, for a caller doing its own
    /// reconnection.
    #[pyo3(signature = (
        *,
        offset=0,
        reconnect=true,
        max_reconnects=20,
        error_on_gap=false,
        idle_timeout=60.0,
    ))]
    fn stream(
        &self,
        offset: u64,
        reconnect: bool,
        max_reconnects: u32,
        error_on_gap: bool,
        idle_timeout: f64,
    ) -> PyCoreResult<ExecStream> {
        let options = StreamOptions {
            offset,
            reconnect,
            max_reconnects,
            error_on_gap,
            idle_timeout: seconds(idle_timeout)?,
        };
        Ok(ExecStream::new(Arc::clone(&self.inner), options))
    }

    /// Writes to the child's stdin. Requires the exec to have been started with
    /// `stdin=True`, or the daemon answers 409.
    ///
    /// `eof` in the same call is the common case for feeding a prompt: two round trips
    /// would leave a window where the child has the bytes but not the EOF that says the
    /// input is complete.
    #[pyo3(signature = (data, *, eof=false))]
    fn write_stdin(&self, py: Python<'_>, data: &[u8], eof: bool) -> PyCoreResult<PyStdinAck> {
        let ack = runtime::block_on(py, self.inner.write_stdin(data, eof))?;
        Ok(PyStdinAck {
            exec_id: ack.exec_id,
            written: ack.written,
            eof: ack.eof,
        })
    }

    /// Sends EOF. Nothing else closes stdin: the daemon's copy of the pipe outlives the
    /// child's wait, so a child blocked reading stdin hangs until its timeout otherwise.
    fn close_stdin(&self, py: Python<'_>) -> PyCoreResult<PyStdinAck> {
        let ack = runtime::block_on(py, self.inner.close_stdin())?;
        Ok(PyStdinAck {
            exec_id: ack.exec_id,
            written: ack.written,
            eof: ack.eof,
        })
    }

    /// Releases the buffered output and starts the TTL clock.
    fn ack(&self, py: Python<'_>) -> PyCoreResult<PyExecResult> {
        Ok(PyExecResult::wrap(runtime::block_on(py, self.inner.ack())?))
    }

    /// Signals the whole process group. `False` means nothing was signalled because the
    /// child had already been reaped — which is the outcome a kill wanted.
    fn kill(&self, py: Python<'_>) -> PyCoreResult<bool> {
        Ok(runtime::block_on(py, self.inner.kill())?)
    }

    /// Wait, then ack, returning the result that carries the output.
    ///
    /// Which result comes back matters: the ack response carries the released output and
    /// a poll issued after the ack reports `acked` with none, so returning the wrong one
    /// is a silent empty-output bug. The core sequences it.
    #[pyo3(signature = (timeout=DEFAULT_WAIT))]
    fn wait_and_ack(&self, py: Python<'_>, timeout: f64) -> PyCoreResult<PyExecResult> {
        let timeout = seconds(timeout)?;
        Ok(PyExecResult::wrap(runtime::block_on(
            py,
            self.inner.wait_and_ack(timeout),
        )?))
    }

    fn __repr__(&self) -> String {
        format!("ExecHandle(exec_id={:?})", self.exec_id())
    }
}

/// A [`Duration`] from a caller's float seconds.
///
/// The core's `duration_of_secs_f64` is what refuses a negative or non-finite figure —
/// this is a call, not a check, which is the BIND-2 rule: the refusal and its message
/// stay in one place.
pub(crate) fn seconds(value: f64) -> Result<Duration, microvms_core::Error> {
    microvms_core::cost::duration_of_secs_f64(value)
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

/// Registers the exec surface on the module.
pub(crate) fn register(module: &Bound<'_, PyModule>) -> PyResult<()> {
    module.add_class::<PyExecHandle>()?;
    module.add_class::<PyExecResult>()?;
    module.add_class::<PyStdinAck>()?;
    module.add_class::<PyOutputChunk>()?;
    module.add_class::<PyGap>()?;
    module.add_class::<PyExit>()?;
    module.add_class::<ExecStream>()?;
    Ok(())
}
