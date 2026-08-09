// SPDX-License-Identifier: Apache-2.0
//! One tokio runtime for the process, and the two ways to block on it.
//!
//! # Why sync bindings over an async core
//!
//! `microvms-core` is async throughout, and Python is not. The two ways to bridge that
//! are a real `asyncio` awaitable through `pyo3-async-runtimes`, or a synchronous method
//! that blocks. This ships the second and only the second, which is the choice
//! `research-bindings.yaml` names as the default: a sync surface works in a plain script,
//! in a notebook, and under `pytest`, and a caller who wants concurrency has
//! `asyncio.to_thread`. Offering both would mean two spellings of every method on
//! [`crate::sandbox::Sandbox`] and [`crate::session::Session`], and the second spelling
//! would be the one nobody tested.
//!
//! # The GIL is released first, and that is not an optimization
//!
//! [`block_on`] calls `py.detach` before `Runtime::block_on`. Blocking while attached to
//! the interpreter deadlocks the moment anything inside the future needs the GIL — and
//! things inside this future do: an [`crate::exec::ExecStream`] hands events back through
//! a channel a Python iterator drains. `Python::detach` is 0.29's name for what older
//! guides call `allow_threads`; the `Ungil` bound on the closure is what stops a `Bound`
//! reference being carried across the release, and it is a compile error rather than a
//! rule.
//!
//! # The re-entrancy guard
//!
//! `Runtime::block_on` panics when called from inside a runtime worker thread. That
//! happens for real: a caller running this module's methods from a thread another
//! extension's runtime owns, or from inside a `tokio::task::spawn_blocking`. So
//! [`block_on`] asks `Handle::try_current()` first and takes `block_in_place` on that
//! path, which is the documented way to block on a worker without wedging the scheduler.
//!
//! # The runtime is multi-thread and process-wide
//!
//! One `LazyLock`, not a runtime per call. A current-thread runtime would be cheaper to
//! create and wrong: the core's `ProxyAuth` mints under a `tokio::sync::Mutex` held
//! across an await, and `block_in_place` requires the multi-thread flavour. The
//! `LazyLock` holds only a runtime and calls no Python during initialization, so it is
//! not the `OnceLock`-against-the-GIL deadlock PyO3's FAQ warns about.

use std::sync::LazyLock;

use pyo3::prelude::*;

/// The process's runtime. See the module docs for why one, why multi-thread, and why
/// `LazyLock` is safe here.
static RUNTIME: LazyLock<tokio::runtime::Runtime> = LazyLock::new(|| {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name("microvms-py")
        .build()
        .expect("a multi-thread tokio runtime is buildable on every platform this loads on")
});

/// Runs `future` to completion with the GIL released.
///
/// The `Send + 'static` bounds on the future are what make the detach sound: nothing
/// borrowed from the interpreter can be captured, so there is no `Bound` to touch while
/// the GIL is elsewhere.
pub(crate) fn block_on<F>(py: Python<'_>, future: F) -> F::Output
where
    F: std::future::Future + Send,
    F::Output: Send,
{
    py.detach(|| block_on_detached(future))
}

/// [`block_on`] for a caller that has already released the GIL, or never held it.
///
/// Separate because the stream reader in [`crate::exec`] runs on a thread with no
/// `Python` token at all, and threading a token there only to detach it would be a
/// fiction.
pub(crate) fn block_on_detached<F>(future: F) -> F::Output
where
    F: std::future::Future,
{
    match tokio::runtime::Handle::try_current() {
        // Already on a worker. `block_on` here panics; `block_in_place` moves the
        // current task off the worker so the rest of the scheduler keeps running.
        Ok(_) => tokio::task::block_in_place(|| RUNTIME.block_on(future)),
        Err(_) => RUNTIME.block_on(future),
    }
}

/// A handle for spawning onto the shared runtime.
///
/// The stream iterator needs this: it spawns the consumer of a core `Stream` as a task
/// and reads events off a channel, because a `Stream` cannot be advanced from a
/// `__next__` that has to return between items.
pub(crate) fn handle() -> tokio::runtime::Handle {
    RUNTIME.handle().clone()
}
