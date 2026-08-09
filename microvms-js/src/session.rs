// SPDX-License-Identifier: Apache-2.0
//! The control API of one running MicroVM.
//!
//! # A launched session lives inside its sandbox
//!
//! Same constraint as the Python side, and it comes from the landed core rather than from
//! either binding: `Sandbox` owns its `Session` by value and hands out only
//! `Option<&Session>`, `Session` is not `Clone`, and there is no accessor for the agent
//! token — so a binding cannot build a second independent session against the same VM.
//! [`Held`] is the consequence: `Session.direct(...)` owns its session, and a launched one
//! reaches into the sandbox through the same `Arc<tokio::sync::Mutex<Sandbox>>`
//! [`crate::sandbox::Sandbox`] holds.
//!
//! The mutex is **tokio's** here and `std`'s on the Python side, and the difference is
//! forced: every method below is `async` and holds the guard across an `await`, which a
//! `std::sync::MutexGuard` cannot do (it is not `Send`). Holding it there is not a
//! compromise — the core's `suspend`/`resume`/`terminate` take `&mut self`, so in Rust a
//! `&Session` cannot be alive across them, and this reproduces exactly that exclusion.
//!
//! # `run` takes an argv, and a bare string is one element
//!
//! `session.run(["ls", "-la"])` and `session.run("ls -la", { shell: true })`. A bare string
//! with `shell` unset becomes a **one-element** argv and is never whitespace-split, which is
//! `session.py`'s own rule: splitting on spaces is how a path with a space in it becomes two
//! arguments nobody meant.

use std::collections::HashMap;
use std::sync::Arc;

use microvms_core::sandbox::Sandbox as CoreSandbox;
use microvms_core::session::Session as CoreSession;
use microvms_core::{Error, ErrorKind};
use napi::bindgen_prelude::Either;
use napi_derive::napi;
use tokio::sync::{Mutex, MutexGuard};

use crate::errors::{AsyncError, js, js_async};
use crate::exec::{ExecHandle, ExecResult, seconds_async};

/// How long to wait for a daemon to report bootstrapped, matching the core's default.
const DEFAULT_READY_TIMEOUT: f64 = 120.0;

/// The default one-shot `runSync` deadline, matching the Python client's 300s.
const DEFAULT_RUN_SYNC_TIMEOUT: f64 = 300.0;

/// The daemon's liveness answer. `bootstrapped` is the useful field.
#[napi(object)]
pub struct Health {
    /// The daemon's own version, distinct from the protocol version.
    pub version: String,
    /// Whether the run hook has landed and the control API is open.
    pub bootstrapped: bool,
    /// Bytes available to an unprivileged writer, or `null` when free space could not be
    /// measured.
    ///
    /// `null` is deliberately distinct from zero: unmeasurable is not full, and a monitor
    /// that conflated them would page on a missing `statvfs`.
    pub available_bytes: Option<i64>,
    /// Bytes that must stay free before a write is refused. Zero means the guard is off.
    pub reserve_bytes: Option<i64>,
    /// Whether a write would be refused right now. Precomputed by the daemon so every
    /// consumer applies the same comparison the write path does.
    pub under_pressure: Option<bool>,
    /// Whether any startup identity repair step failed — a duplicate machine-id or boot_id
    /// still in place from the shared image.
    pub identity_degraded: bool,
    /// False when identity repair was switched off by config. Separate from `degraded` so a
    /// monitor can tell "opted out" from "nothing to do".
    pub identity_repaired: bool,
}

impl Health {
    fn wrap(health: protocol::health::Health) -> Self {
        Self {
            version: health.version.into_owned(),
            bootstrapped: health.bootstrapped,
            available_bytes: health.disk.as_ref().map(|disk| disk.available_bytes as i64),
            reserve_bytes: health.disk.as_ref().map(|disk| disk.reserve_bytes as i64),
            under_pressure: health.disk.as_ref().map(|disk| disk.under_pressure),
            identity_degraded: health.identity_degraded,
            identity_repaired: health.identity_repaired,
        }
    }
}

/// How an exec should be started. Every field optional; the defaults are the daemon's.
#[napi(object)]
pub struct ExecOptions {
    /// A single script string rather than an argv. Requires `shell: true`.
    pub shell: Option<bool>,
    pub cwd: Option<String>,
    pub env: Option<HashMap<String, String>>,
    pub user: Option<u32>,
    pub group: Option<u32>,
    /// The daemon's own kill deadline for the child, distinct from a client-side `wait`.
    pub timeout_sec: Option<f64>,
    /// Whether to open a stdin pipe. Writing without this is a 409.
    pub stdin: Option<bool>,
    /// The idempotency key. Omitted, one is minted; supplied, the daemon returns success for
    /// a known id without spawning a second child — so a caller whose retry must be safe
    /// across its own restart passes a stable one.
    pub exec_id: Option<String>,
    /// The client-side deadline for `runSync` only.
    pub timeout: Option<f64>,
}

impl ExecOptions {
    fn empty() -> Self {
        Self {
            shell: None,
            cwd: None,
            env: None,
            user: None,
            group: None,
            timeout_sec: None,
            stdin: None,
            exec_id: None,
            timeout: None,
        }
    }

    /// The daemon's start request.
    ///
    /// `command` arrives as `Either<String, Vec<String>>`, which is napi's tagged extraction
    /// — a number or an object is rejected by the conversion before this runs. A bare string
    /// becomes a **one-element** argv rather than being whitespace-split; see the module
    /// docs.
    fn into_request(self, command: Either<String, Vec<String>>) -> protocol::exec::StartRequest {
        protocol::exec::StartRequest {
            exec_id: self.exec_id.unwrap_or_else(mint_exec_id),
            command: match command {
                Either::A(single) => vec![single],
                Either::B(argv) => argv,
            },
            shell: self.shell.unwrap_or(false),
            cwd: self.cwd,
            env: self.env.unwrap_or_default(),
            user: self.user,
            group: self.group,
            timeout_sec: self.timeout_sec,
            stdin: self.stdin.unwrap_or(false),
        }
    }
}

/// Where a session lives, which decides how it is reached. See the module docs.
pub(crate) enum Held {
    /// A session this object owns, from [`Session::direct`].
    Owned(CoreSession),
    /// A session inside a sandbox, reached under the sandbox's lock.
    InSandbox(Arc<Mutex<CoreSandbox>>),
}

/// One running MicroVM's control API, with the proxy auth handled for you.
#[napi]
pub struct Session {
    held: Held,
}

/// A live session plus, when there is one, the guard keeping it alive.
///
/// The guard has to be *returned* rather than dropped inside a helper, because the reference
/// borrows from it — which is the whole reason this is an enum rather than a closure-taking
/// helper like the Python side's. A closure would work too, but it cannot be `async` without
/// boxing every call site's future.
enum Live<'a> {
    Owned(&'a CoreSession),
    Guarded(MutexGuard<'a, CoreSandbox>),
}

impl Live<'_> {
    fn session(&self) -> Result<&CoreSession, Error> {
        match self {
            Live::Owned(session) => Ok(session),
            Live::Guarded(guard) => guard.session().ok_or_else(|| {
                Error::new(
                    ErrorKind::Precondition,
                    format!(
                        "this sandbox holds no session: it is {} and terminate() drops the session \
                         because the only remaining use of its cached proxy token would be a \
                         request against a VM that is going away. A new VM needs a new Sandbox.",
                        guard.lifecycle()
                    ),
                )
            }),
        }
    }
}

impl Session {
    /// A session that reaches into `sandbox`.
    pub(crate) fn in_sandbox(sandbox: Arc<Mutex<CoreSandbox>>) -> Self {
        Self {
            held: Held::InSandbox(sandbox),
        }
    }

    /// Takes whatever this object needs in order to have a live session.
    ///
    /// For a sandbox-held session that is the sandbox lock, held until the returned value
    /// drops — so it spans exactly one method call and no more.
    async fn live(&self) -> Live<'_> {
        match &self.held {
            Held::Owned(session) => Live::Owned(session),
            Held::InSandbox(sandbox) => Live::Guarded(sandbox.lock().await),
        }
    }
}

#[napi]
impl Session {
    /// A session against a daemon reached **directly**, with no proxy headers.
    ///
    /// The shape for a local binary, a test server, or a VM reached over a tunnel. There is
    /// deliberately no constructor that takes a proxy token: minting one is the control
    /// plane's job and it happens inside every request (TRAP-9), so a caller handing a token
    /// in would be handing in one that expires.
    #[napi(factory)]
    pub fn direct(endpoint: String, agent_token: String) -> napi::Result<Session, String> {
        Ok(Session {
            held: Held::Owned(CoreSession::direct(endpoint, agent_token).map_err(js)?),
        })
    }

    /// The endpoint this session addresses.
    #[napi]
    pub async fn endpoint(&self) -> Result<String, AsyncError> {
        let live = self.live().await;
        Ok(live.session().map_err(js_async)?.endpoint().to_string())
    }

    /// The port the proxy token is scoped to.
    #[napi]
    pub async fn port(&self) -> Result<u16, AsyncError> {
        let live = self.live().await;
        Ok(live.session().map_err(js_async)?.port())
    }

    /// Unauthenticated liveness.
    #[napi]
    pub async fn health(&self) -> Result<Health, AsyncError> {
        let live = self.live().await;
        let session = live.session().map_err(js_async)?;
        Ok(Health::wrap(session.health().await.map_err(js_async)?))
    }

    /// Polls health until the daemon reports bootstrapped.
    ///
    /// Connection errors on the way are expected rather than exceptional: a VM that has just
    /// reached RUNNING commonly refuses a connection or two before the proxy path is wired
    /// up. A *fatal* error ends the wait at once, because retrying a 401 until the deadline
    /// is the mistake the retryable split exists to prevent.
    #[napi]
    pub async fn wait_until_ready(&self, timeout: Option<f64>) -> Result<Health, AsyncError> {
        let timeout = seconds_async(timeout.unwrap_or(DEFAULT_READY_TIMEOUT))?;
        let live = self.live().await;
        let session = live.session().map_err(js_async)?;
        Ok(Health::wrap(
            session.wait_until_ready(timeout).await.map_err(js_async)?,
        ))
    }

    /// Starts a command and returns its handle. Does not wait.
    #[napi]
    pub async fn run(
        &self,
        command: Either<String, Vec<String>>,
        options: Option<ExecOptions>,
    ) -> Result<ExecHandle, AsyncError> {
        let request = options
            .unwrap_or_else(ExecOptions::empty)
            .into_request(command);
        let live = self.live().await;
        let session = live.session().map_err(js_async)?;
        Ok(ExecHandle::wrap(
            session.run(request).await.map_err(js_async)?,
        ))
    }

    /// A handle for an exec started earlier, possibly by another process.
    ///
    /// The reattach path. Nothing is checked against the daemon here — the handle is an id
    /// plus a transport, and a poll is what discovers whether the exec exists.
    #[napi]
    pub async fn exec(&self, exec_id: String) -> Result<ExecHandle, AsyncError> {
        let live = self.live().await;
        let session = live.session().map_err(js_async)?;
        Ok(ExecHandle::wrap(session.exec(exec_id)))
    }

    /// Start, wait, ack. The one-shot shape, for when output is all you want.
    #[napi]
    pub async fn run_sync(
        &self,
        command: Either<String, Vec<String>>,
        options: Option<ExecOptions>,
    ) -> Result<ExecResult, AsyncError> {
        let options = options.unwrap_or_else(ExecOptions::empty);
        let timeout = seconds_async(options.timeout.unwrap_or(DEFAULT_RUN_SYNC_TIMEOUT))?;
        let request = options.into_request(command);
        let live = self.live().await;
        let session = live.session().map_err(js_async)?;
        Ok(ExecResult::wrap(
            session.run_sync(request, timeout).await.map_err(js_async)?,
        ))
    }

    /// Signals an exec's whole process group. Returns whether anything was signalled.
    #[napi]
    pub async fn kill(&self, exec_id: String) -> Result<bool, AsyncError> {
        let live = self.live().await;
        let session = live.session().map_err(js_async)?;
        session.kill(&exec_id).await.map_err(js_async)
    }

    /// Writes one file, creating parents. `mode` is an **octal string** (`"0755"`), which is
    /// the daemon's shape — a number here would be ambiguous between 0o755 and 755.
    #[napi]
    pub async fn upload_file(
        &self,
        path: String,
        data: napi::bindgen_prelude::Uint8Array,
        mode: Option<String>,
    ) -> Result<(), AsyncError> {
        let live = self.live().await;
        let session = live.session().map_err(js_async)?;
        session
            .upload_file(&path, &data, mode.as_deref())
            .await
            .map_err(js_async)
    }

    /// Reads one file.
    #[napi]
    pub async fn download_file(
        &self,
        path: String,
    ) -> Result<napi::bindgen_prelude::Buffer, AsyncError> {
        let live = self.live().await;
        let session = live.session().map_err(js_async)?;
        Ok(session.download_file(&path).await.map_err(js_async)?.into())
    }

    /// Whether a path exists, distinguishing absence from every other refusal.
    #[napi]
    pub async fn file_exists(&self, path: String) -> Result<bool, AsyncError> {
        let live = self.live().await;
        let session = live.session().map_err(js_async)?;
        session.file_exists(&path).await.map_err(js_async)
    }

    /// Extracts pre-built tar bytes under `remote`.
    ///
    /// Bytes rather than a local path: packing a directory is the caller's, because the
    /// symlink and permission decisions in a pack belong to whoever knows what the tree
    /// means.
    #[napi]
    pub async fn upload_tar(
        &self,
        remote: String,
        archive: napi::bindgen_prelude::Uint8Array,
    ) -> Result<(), AsyncError> {
        let live = self.live().await;
        let session = live.session().map_err(js_async)?;
        session
            .upload_tar(&remote, &archive)
            .await
            .map_err(js_async)
    }

    /// The raw tar bytes of a remote tree.
    #[napi]
    pub async fn download_tar(
        &self,
        remote: String,
    ) -> Result<napi::bindgen_prelude::Buffer, AsyncError> {
        let live = self.live().await;
        let session = live.session().map_err(js_async)?;
        Ok(session
            .download_tar(&remote)
            .await
            .map_err(js_async)?
            .into())
    }

    /// How many proxy tokens this session has minted, or `null` for a direct session.
    ///
    /// Exposed because it is the only observable that distinguishes a client which re-minted
    /// after a resume from one that kept a stale token (STATE-8). The **token itself** is not
    /// exposed and cannot be: the core's `ProxyToken` has no `Display`, no `as_str`, and no
    /// `Deref`, so "treat `authToken` as a string" is as inexpressible here as it is there
    /// (TRAP-7).
    #[napi]
    pub async fn proxy_mint_count(&self) -> Result<Option<i64>, AsyncError> {
        let live = self.live().await;
        let session = live.session().map_err(js_async)?;
        Ok(session.proxy_auth().map(|auth| auth.mint_count() as i64))
    }
}

/// A fresh exec id: `x-` plus 16 hex characters, the Python client's shape.
///
/// Not a crate: the id needs to be distinct rather than unguessable — it is an idempotency
/// key, not a credential, and the daemon rejects an unknown one — so the nanosecond clock
/// mixed with a counter is enough, and a CSPRNG dependency in a binding crate for it would
/// not be.
fn mint_exec_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQUENCE: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since| since.as_nanos() as u64)
        .unwrap_or_default();
    let sequence = SEQUENCE.fetch_add(1, Ordering::Relaxed);
    // The counter folds into the clock's high bits so two ids minted in the same nanosecond
    // still differ.
    format!("x-{:016x}", nanos ^ (sequence << 40))
}

/// The daemon's protocol constants, as a JSON string, for a caller asserting against the
/// wire contract.
#[napi]
pub fn session_constants() -> String {
    format!(
        concat!(
            r#"{{"defaultAgentPort":{},"proxyAuthHeader":"{}","proxyPortHeader":"{}","#,
            r#""maxTokenLifetimeSeconds":{},"defaultRefreshAfterSeconds":{},"#,
            r#""phases":["running","exited","acked"],"streamKinds":["stdout","stderr"]}}"#,
        ),
        microvms_core::session::DEFAULT_AGENT_PORT,
        microvms_core::session::PROXY_AUTH_HEADER,
        microvms_core::session::PROXY_PORT_HEADER,
        microvms_core::session::MAX_TOKEN_LIFETIME.as_secs(),
        microvms_core::session::DEFAULT_REFRESH_AFTER.as_secs(),
    )
}
