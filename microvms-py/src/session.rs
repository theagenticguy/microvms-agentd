// SPDX-License-Identifier: Apache-2.0
//! The control API of one running MicroVM.
//!
//! # A session holds no state worth keeping
//!
//! Every exec record, every file, and the bootstrap token live in the VM. So a session
//! rebuilt from an endpoint and an agent token reattaches to everything a previous
//! process was doing, and `Session.exec(exec_id)` addresses the same server-side exec.
//! That is what [`PySession::direct`] is for: it is a supported shape rather than a
//! test-only hatch, and it is the path a caller inside the VM or on a tunnel takes.
//!
//! # `run` takes an argv, and a bare string is one element
//!
//! `session.run(["ls", "-la"])` and `session.run("ls -la", shell=True)` are the two
//! spellings. A bare string with `shell=False` becomes a **one-element** argv and is
//! never whitespace-split, which is `session.py`'s own rule: splitting on spaces is how a
//! path with a space in it becomes two arguments nobody meant.
//!
//! # The exec id is the idempotency key
//!
//! Omitted, one is minted (`x-<16 hex>`, the Python's shape). Supplied, the daemon
//! returns success for a known id without spawning a second child — so a caller whose
//! retry must be safe across its own restart passes a stable one.
//!
//! # A launched session lives inside its sandbox, and the lock is the borrow checker
//!
//! [`microvms_core::sandbox::Sandbox`] owns its `Session` by value and hands out only
//! `Option<&Session>`; there is no accessor for the agent token, so a binding cannot build
//! a second, independent session against the same VM. [`Held`] is the consequence: a
//! session obtained from a sandbox borrows it under the sandbox's lock, and one built by
//! [`PySession::direct`] owns itself.
//!
//! Holding that lock across a session call is not a compromise — it is the core's own
//! discipline at runtime. `Sandbox::suspend`/`resume`/`terminate` take `&mut self`, so in
//! Rust you *cannot* terminate a sandbox while a `&Session` from it is alive. The lock
//! reproduces exactly that exclusion, including its cost: a `wait(timeout=300)` holds the
//! sandbox for up to five minutes, which is the same five minutes the borrow checker would
//! have held it for.

use std::sync::Arc;

use microvms_core::sandbox::Sandbox;
use microvms_core::session::Session;
use microvms_core::{Error, ErrorKind};
use pyo3::prelude::*;
use pyo3::types::{PyBytes, PyDict};

use crate::errors::PyCoreResult;
use crate::exec::{PyExecHandle, PyExecResult, seconds};
use crate::runtime;

/// How long to wait for a daemon to report bootstrapped, matching the core's default.
const DEFAULT_READY_TIMEOUT: f64 = 120.0;

/// The default one-shot `run_sync` deadline, matching the Python client's 300s.
const DEFAULT_RUN_SYNC_TIMEOUT: f64 = 300.0;

/// The daemon's liveness answer. `bootstrapped` is the useful field.
#[pyclass(frozen, name = "Health", module = "microvms")]
pub struct PyHealth {
    version: String,
    bootstrapped: bool,
    available_bytes: Option<u64>,
    reserve_bytes: Option<u64>,
    under_pressure: Option<bool>,
    identity_degraded: bool,
    identity_repaired: bool,
}

impl PyHealth {
    fn wrap(health: protocol::health::Health) -> Self {
        Self {
            version: health.version.into_owned(),
            bootstrapped: health.bootstrapped,
            available_bytes: health.disk.as_ref().map(|disk| disk.available_bytes),
            reserve_bytes: health.disk.as_ref().map(|disk| disk.reserve_bytes),
            under_pressure: health.disk.as_ref().map(|disk| disk.under_pressure),
            identity_degraded: health.identity_degraded,
            identity_repaired: health.identity_repaired,
        }
    }
}

#[pymethods]
impl PyHealth {
    /// The daemon's own version, distinct from the protocol version.
    #[getter]
    fn version(&self) -> &str {
        &self.version
    }

    /// Whether the run hook has landed and the control API is open.
    #[getter]
    fn bootstrapped(&self) -> bool {
        self.bootstrapped
    }

    /// Bytes available to an unprivileged writer, or `None` when free space could not be
    /// measured.
    ///
    /// `None` is deliberately distinct from zero: unmeasurable is not full, and a monitor
    /// that conflated them would page on a missing `statvfs`.
    #[getter]
    fn available_bytes(&self) -> Option<u64> {
        self.available_bytes
    }

    /// Bytes that must stay free before a write is refused. Zero means the guard is off.
    #[getter]
    fn reserve_bytes(&self) -> Option<u64> {
        self.reserve_bytes
    }

    /// Whether a write would be refused right now. Precomputed by the daemon so every
    /// consumer applies the same comparison the write path does.
    #[getter]
    fn under_pressure(&self) -> Option<bool> {
        self.under_pressure
    }

    /// Whether any startup identity repair step failed — a duplicate machine-id or
    /// boot_id still in place from the shared image.
    #[getter]
    fn identity_degraded(&self) -> bool {
        self.identity_degraded
    }

    /// False when identity repair was switched off by config. Separate from `degraded` so
    /// a monitor can tell "opted out" from "nothing to do".
    #[getter]
    fn identity_repaired(&self) -> bool {
        self.identity_repaired
    }

    fn __repr__(&self) -> String {
        format!(
            "Health(version={:?}, bootstrapped={}, identity_degraded={})",
            self.version, self.bootstrapped, self.identity_degraded
        )
    }
}

/// Where a session lives, which decides how it is reached.
///
/// Two variants because there are two real cases and they cannot be unified without
/// giving something up. See the module docs.
pub(crate) enum Held {
    /// A session this object owns, from [`PySession::direct`].
    Owned(Session),
    /// A session inside a sandbox, reached under the sandbox's lock.
    ///
    /// The sandbox is the same `Arc<Mutex<Sandbox>>` [`crate::sandbox::PySandbox`] holds,
    /// so a `terminate()` on the sandbox and a `run()` on the session cannot interleave —
    /// which is the runtime spelling of the core's `&mut self`.
    InSandbox(Arc<std::sync::Mutex<Sandbox>>),
}

/// One running MicroVM's control API, with the proxy auth handled for you.
#[pyclass(frozen, name = "Session", module = "microvms")]
pub struct PySession {
    held: Held,
}

impl PySession {
    /// A session that reaches into `sandbox`.
    pub(crate) fn in_sandbox(sandbox: Arc<std::sync::Mutex<Sandbox>>) -> Self {
        Self {
            held: Held::InSandbox(sandbox),
        }
    }

    /// Runs `body` against the live session, whichever way this object holds one.
    ///
    /// The closure shape is what keeps the lock scope honest: it is held for exactly one
    /// call and released before returning into Python, so nothing can hold it across a
    /// Python callback and deadlock against the GIL.
    ///
    /// A sandbox that has been terminated has no session, and the error says which of the
    /// two states it is in rather than "no session" — the core sets that up by clearing
    /// the session in `terminate`.
    fn with<T>(&self, body: impl FnOnce(&Session) -> Result<T, Error>) -> Result<T, Error> {
        match &self.held {
            Held::Owned(session) => body(session),
            Held::InSandbox(sandbox) => {
                let guard = sandbox
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let session = guard.session().ok_or_else(|| {
                    Error::new(
                        ErrorKind::Precondition,
                        format!(
                            "this sandbox holds no session: it is {} and terminate() drops the \
                             session because the only remaining use of its cached proxy token \
                             would be a request against a VM that is going away. A new VM needs \
                             a new Sandbox.",
                            guard.lifecycle()
                        ),
                    )
                })?;
                body(session)
            }
        }
    }

    /// [`Self::with`], with the GIL released for the whole call.
    ///
    /// Every async method below goes through here, which is what makes "release the GIL,
    /// take the sandbox lock, run, drop both" one shape rather than a sequence each method
    /// could get wrong. The `Ungil` bound on `py.detach` is what stops a `Bound` reference
    /// being captured, so the release is a compile-time guarantee rather than a convention.
    ///
    /// `body` is **synchronous** and calls [`runtime::block_on_detached`] itself, which is
    /// not a style choice: a closure answering a future that borrows its `&Session`
    /// argument needs a higher-ranked bound plus a boxed future at every call site, and the
    /// boxing would be there only to satisfy the signature. Blocking inside the closure
    /// keeps the borrow entirely local and costs one visible call per method.
    fn detached<T>(
        &self,
        py: Python<'_>,
        body: impl FnOnce(&Session) -> Result<T, Error> + Send,
    ) -> Result<T, Error>
    where
        T: Send,
    {
        py.detach(|| self.with(body))
    }
}

#[pymethods]
impl PySession {
    /// A session against a daemon reached **directly**, with no proxy headers.
    ///
    /// The shape for a local binary, a test server, or a VM reached over a tunnel. There
    /// is deliberately no constructor that takes a proxy token: minting one is the
    /// control plane's job and it happens inside every request (TRAP-9), so a caller
    /// handing a token in would be handing in one that expires.
    #[staticmethod]
    fn direct(endpoint: &str, agent_token: &str) -> PyCoreResult<PySession> {
        Ok(PySession {
            held: Held::Owned(Session::direct(endpoint, agent_token)?),
        })
    }

    /// The endpoint this session addresses.
    ///
    /// A `String` rather than a `&str` because a sandbox-held session reads it under the
    /// lock, and a reference would outlive the guard.
    #[getter]
    fn endpoint(&self) -> PyCoreResult<String> {
        Ok(self.with(|session| Ok(session.endpoint().to_string()))?)
    }

    /// The port the proxy token is scoped to.
    #[getter]
    fn port(&self) -> PyCoreResult<u16> {
        Ok(self.with(|session| Ok(session.port()))?)
    }

    /// Unauthenticated liveness.
    fn health(&self, py: Python<'_>) -> PyCoreResult<PyHealth> {
        Ok(PyHealth::wrap(self.detached(py, |session| {
            runtime::block_on_detached(session.health())
        })?))
    }

    /// Polls health until the daemon reports bootstrapped.
    ///
    /// Connection errors on the way are expected rather than exceptional: a VM that has
    /// just reached RUNNING commonly refuses a connection or two before the proxy path is
    /// wired up. A *fatal* error ends the wait at once, because retrying a 401 until the
    /// deadline is the mistake the retryable split exists to prevent.
    #[pyo3(signature = (timeout=DEFAULT_READY_TIMEOUT))]
    fn wait_until_ready(&self, py: Python<'_>, timeout: f64) -> PyCoreResult<PyHealth> {
        let timeout = seconds(timeout)?;
        Ok(PyHealth::wrap(self.detached(py, |session| {
            runtime::block_on_detached(session.wait_until_ready(timeout))
        })?))
    }

    /// Starts a command and returns its handle. Does not wait.
    ///
    /// `command` is a list, or a string that becomes a one-element argv — never
    /// whitespace-split. `shell=True` wants a single script string.
    #[pyo3(signature = (
        command,
        *,
        shell=false,
        cwd=None,
        env=None,
        user=None,
        group=None,
        timeout_sec=None,
        stdin=false,
        exec_id=None,
    ))]
    #[allow(
        clippy::too_many_arguments,
        reason = "one keyword-only parameter per \
         protocol::exec::StartRequest field, which is what keeps a caller from having to \
         build a dict whose keys are unchecked"
    )]
    fn run(
        &self,
        py: Python<'_>,
        command: Command,
        shell: bool,
        cwd: Option<String>,
        env: Option<std::collections::HashMap<String, String>>,
        user: Option<u32>,
        group: Option<u32>,
        timeout_sec: Option<f64>,
        stdin: bool,
        exec_id: Option<String>,
    ) -> PyCoreResult<PyExecHandle> {
        let request = protocol::exec::StartRequest {
            exec_id: exec_id.unwrap_or_else(mint_exec_id),
            command: command.into_argv(),
            shell,
            cwd,
            env: env.unwrap_or_default(),
            user,
            group,
            timeout_sec,
            stdin,
        };
        // The request is moved into the closure, so it is built before the detach rather
        // than inside it — a `Command` extraction needs the GIL and the closure does not
        // have it.
        Ok(PyExecHandle::wrap(self.detached(py, move |session| {
            runtime::block_on_detached(session.run(request))
        })?))
    }

    /// A handle for an exec started earlier, possibly by another process.
    ///
    /// The reattach path. Nothing is checked against the daemon here — the handle is an
    /// id plus a transport, and a poll is what discovers whether the exec exists.
    fn exec(&self, exec_id: &str) -> PyCoreResult<PyExecHandle> {
        Ok(PyExecHandle::wrap(
            self.with(|session| Ok(session.exec(exec_id)))?,
        ))
    }

    /// Start, wait, ack. The one-shot shape, for when output is all you want.
    #[pyo3(signature = (
        command,
        *,
        timeout=DEFAULT_RUN_SYNC_TIMEOUT,
        shell=false,
        cwd=None,
        env=None,
        user=None,
        group=None,
        timeout_sec=None,
        stdin=false,
        exec_id=None,
    ))]
    #[allow(
        clippy::too_many_arguments,
        reason = "the run() signature plus the wait \
         deadline, deliberately"
    )]
    fn run_sync(
        &self,
        py: Python<'_>,
        command: Command,
        timeout: f64,
        shell: bool,
        cwd: Option<String>,
        env: Option<std::collections::HashMap<String, String>>,
        user: Option<u32>,
        group: Option<u32>,
        timeout_sec: Option<f64>,
        stdin: bool,
        exec_id: Option<String>,
    ) -> PyCoreResult<PyExecResult> {
        let request = protocol::exec::StartRequest {
            exec_id: exec_id.unwrap_or_else(mint_exec_id),
            command: command.into_argv(),
            shell,
            cwd,
            env: env.unwrap_or_default(),
            user,
            group,
            timeout_sec,
            stdin,
        };
        let timeout = seconds(timeout)?;
        Ok(PyExecResult::wrap(self.detached(py, move |session| {
            runtime::block_on_detached(session.run_sync(request, timeout))
        })?))
    }

    /// Signals an exec's whole process group. Returns whether anything was signalled.
    fn kill(&self, py: Python<'_>, exec_id: &str) -> PyCoreResult<bool> {
        Ok(self.detached(py, |session| {
            runtime::block_on_detached(session.kill(exec_id))
        })?)
    }

    /// Writes one file, creating parents. `mode` is an **octal string** (`"0755"`), which
    /// is the daemon's shape — an integer here would be ambiguous between 0o755 and 755.
    #[pyo3(signature = (path, data, *, mode=None))]
    fn upload_file(
        &self,
        py: Python<'_>,
        path: &str,
        data: &[u8],
        mode: Option<&str>,
    ) -> PyCoreResult<()> {
        self.detached(py, |session| {
            runtime::block_on_detached(session.upload_file(path, data, mode))
        })?;
        Ok(())
    }

    /// Reads one file.
    fn download_file<'py>(&self, py: Python<'py>, path: &str) -> PyCoreResult<Bound<'py, PyBytes>> {
        let bytes = self.detached(py, |session| {
            runtime::block_on_detached(session.download_file(path))
        })?;
        Ok(PyBytes::new(py, &bytes))
    }

    /// Whether a path exists, distinguishing absence from every other refusal.
    fn file_exists(&self, py: Python<'_>, path: &str) -> PyCoreResult<bool> {
        Ok(self.detached(py, |session| {
            runtime::block_on_detached(session.file_exists(path))
        })?)
    }

    /// Extracts pre-built tar bytes under `remote`.
    ///
    /// Bytes rather than a local path: packing a directory is the caller's, because the
    /// symlink and permission decisions in a pack belong to whoever knows what the tree
    /// means.
    fn upload_tar(&self, py: Python<'_>, remote: &str, archive: &[u8]) -> PyCoreResult<()> {
        self.detached(py, |session| {
            runtime::block_on_detached(session.upload_tar(remote, archive))
        })?;
        Ok(())
    }

    /// The raw tar bytes of a remote tree.
    fn download_tar<'py>(
        &self,
        py: Python<'py>,
        remote: &str,
    ) -> PyCoreResult<Bound<'py, PyBytes>> {
        let bytes = self.detached(py, |session| {
            runtime::block_on_detached(session.download_tar(remote))
        })?;
        Ok(PyBytes::new(py, &bytes))
    }

    /// How many proxy tokens this session has minted, or `None` for a direct session.
    ///
    /// Exposed because it is the only observable that distinguishes a client which
    /// re-minted after a resume from one that kept a stale token (STATE-8), and a harness
    /// asserting on that behaviour needs to be able to read it. The **token itself** is
    /// not exposed and cannot be: the core's `ProxyToken` has no `Display`, no `as_str`,
    /// and no `Deref`, so "treat `authToken` as a string" is as inexpressible here as it
    /// is there (TRAP-7).
    #[getter]
    fn proxy_mint_count(&self) -> PyCoreResult<Option<u64>> {
        Ok(self.with(|session| Ok(session.proxy_auth().map(|auth| auth.mint_count())))?)
    }

    fn __repr__(&self) -> String {
        // A `Debug` that could fail is a `Debug` nobody can read at the moment it matters
        // most — a terminated sandbox — so the unreachable case renders as a state rather
        // than raising out of `repr()`.
        match self.with(|session| Ok((session.endpoint().to_string(), session.port()))) {
            Ok((endpoint, port)) => format!("Session(endpoint={endpoint:?}, port={port})"),
            Err(_) => "Session(<no live session: the sandbox was terminated>)".to_string(),
        }
    }
}

/// A command as either an argv or a single string.
///
/// The `FromPyObject` derive on an untagged enum is what makes `run("ls")` and
/// `run(["ls", "-la"])` both work while `run(3)` is a `TypeError` from PyO3 rather than a
/// check written here. Order matters: `Argv` first, because a Python `str` is a sequence
/// and a `Vec<String>` extraction from `"ls"` would otherwise succeed as `["l", "s"]`.
#[derive(FromPyObject)]
pub enum Command {
    Argv(Vec<String>),
    One(String),
}

impl Command {
    /// The argv the daemon receives.
    ///
    /// A bare string becomes a **one-element** argv rather than being whitespace-split,
    /// matching `session.py`: splitting on spaces turns a path with a space in it into
    /// two arguments nobody meant. `shell=True` is how a caller asks for a script.
    fn into_argv(self) -> Vec<String> {
        match self {
            Command::Argv(argv) => argv,
            Command::One(single) => vec![single],
        }
    }
}

/// A fresh exec id: `x-` plus 16 hex characters, the Python client's shape.
///
/// Not a crate: the id needs to be distinct rather than unguessable — it is an
/// idempotency key, not a credential, and the daemon rejects an unknown one — so the
/// nanosecond clock mixed with a counter is enough, and adding a CSPRNG dependency to a
/// binding crate for it would not be.
fn mint_exec_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQUENCE: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since| since.as_nanos() as u64)
        .unwrap_or_default();
    let sequence = SEQUENCE.fetch_add(1, Ordering::Relaxed);
    // The high bits of the counter fold into the clock so two ids minted in the same
    // nanosecond still differ.
    format!("x-{:016x}", nanos ^ (sequence << 40))
}

/// The daemon's protocol constants, for a caller asserting against the wire contract.
#[pyfunction]
pub(crate) fn session_constants<'py>(py: Python<'py>) -> PyResult<Bound<'py, PyDict>> {
    let dict = PyDict::new(py);
    dict.set_item(
        "defaultAgentPort",
        microvms_core::session::DEFAULT_AGENT_PORT,
    )?;
    dict.set_item("proxyAuthHeader", microvms_core::session::PROXY_AUTH_HEADER)?;
    dict.set_item("proxyPortHeader", microvms_core::session::PROXY_PORT_HEADER)?;
    dict.set_item(
        "maxTokenLifetimeSeconds",
        microvms_core::session::MAX_TOKEN_LIFETIME.as_secs(),
    )?;
    dict.set_item(
        "defaultRefreshAfterSeconds",
        microvms_core::session::DEFAULT_REFRESH_AFTER.as_secs(),
    )?;
    dict.set_item("phases", ["running", "exited", "acked"])?;
    dict.set_item("streamKinds", ["stdout", "stderr"])?;
    Ok(dict)
}

/// Registers the session surface on the module.
pub(crate) fn register(module: &Bound<'_, PyModule>) -> PyResult<()> {
    module.add_class::<PySession>()?;
    module.add_class::<PyHealth>()?;
    module.add_function(wrap_pyfunction!(session_constants, module)?)?;
    Ok(())
}
