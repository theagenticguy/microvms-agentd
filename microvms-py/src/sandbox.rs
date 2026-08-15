// SPDX-License-Identifier: Apache-2.0
//! One MicroVM's whole life.
//!
//! # The sandbox is one object, and that is why it wraps cleanly
//!
//! `microvms-core`'s `Sandbox` is runtime-checked rather than typestate, and T-W3-6 chose
//! that *for this file*: a `Sandbox<Running>` returning a `Suspended` handle is stronger in
//! Rust, but a type whose identity changes on every transition cannot be one `#[pyclass]`,
//! so it would be re-erased into a runtime check here and the check would exist twice —
//! with the binding's copy being the one every Python caller hits. What survives the choice
//! is the part that costs nothing: the state check happens **before** the wire call, so a
//! suspend from SUSPENDED is refused with zero control-plane calls rather than answered by
//! AWS.
//!
//! # Every guard below belongs to the core
//!
//! There is no state check in this file. `run` twice, `suspend` from PENDING, `resume` past
//! the window, `resume` after `terminate` — every one of those is refused by the core's own
//! transition, with the core's own message naming the STATE requirement and the
//! `docs/PLATFORM.md` finding. A copy here would be the copy nothing else tests (BIND-2).
//!
//! # There is no context manager that tears down
//!
//! `__enter__`/`__exit__` are here and `__exit__` calls `terminate()`, matching
//! `sandbox.py`. That is a deliberate difference from the Rust core, which has **no** `Drop`
//! that tears down — `Drop` cannot await, so a teardown there would deadlock inside a
//! runtime or race the process exit. Python's `with` is not `Drop`: `__exit__` runs
//! synchronously on the calling thread, which is exactly where a blocking teardown belongs,
//! so the context manager is available here and is not a loosening.
//!
//! # `terminate` returns a report and never raises
//!
//! It runs where a caller's `finally` would, and an exception raised there replaces the real
//! failure with a teardown failure. `TeardownReport.undeleted` names what was left behind,
//! including the build log group, which this client **cannot** delete — CloudWatch is not in
//! the core's dependency set — so asking names it rather than removing it.

use std::sync::{Arc, Mutex, PoisonError};

use microvms_core::SizeClass;
use microvms_core::control::{BaseImage, CreateImageRequest};
use microvms_core::sandbox::{RunRequest, Sandbox, TeardownOpts, TeardownReport};
use pyo3::prelude::*;
use pyo3::types::{PyBytes, PyDict};

use crate::cost::PySizeClass;
use crate::errors::PyCoreResult;
use crate::exec::seconds;
use crate::hooks::{PyBuildHookTimeout, PyRunHookTimeout};
use crate::region::PyRegion;
use crate::runtime;
use crate::session::PySession;

/// A built image, and the log group the service created alongside it.
#[pyclass(frozen, name = "Image", module = "microvms")]
pub struct PyImage {
    identifier: String,
    name: String,
    version: String,
    state: String,
    size: SizeClass,
    build_log_group: String,
}

#[pymethods]
impl PyImage {
    /// The image ARN, which is what `imageIdentifier` takes.
    #[getter]
    fn identifier(&self) -> &str {
        &self.identifier
    }

    #[getter]
    fn name(&self) -> &str {
        &self.name
    }

    #[getter]
    fn version(&self) -> &str {
        &self.version
    }

    #[getter]
    fn state(&self) -> &str {
        &self.state
    }

    /// The class the requested baseline selected.
    ///
    /// Carried on the image because billing follows the baseline requested at *create*
    /// time, and by the time anyone asks what a run cost the request is gone.
    #[getter]
    fn size(&self) -> PySizeClass {
        PySizeClass { inner: self.size }
    }

    /// `/aws/lambda-microvms/<image-name>`.
    ///
    /// The service creates this itself, so no Terraform stack owns it and
    /// `terraform destroy` leaves it behind — "the stack destroyed cleanly" is not "the
    /// account is clean". Six accumulated before anyone noticed.
    #[getter]
    fn build_log_group(&self) -> &str {
        &self.build_log_group
    }

    fn __repr__(&self) -> String {
        format!(
            "Image(identifier={:?}, state={:?})",
            self.identifier, self.state
        )
    }
}

/// What a teardown did, and what it left behind.
///
/// Returned rather than raised — see the module docs.
#[pyclass(frozen, name = "TeardownReport", module = "microvms")]
pub struct PyTeardownReport {
    inner: TeardownReport,
}

#[pymethods]
impl PyTeardownReport {
    /// Identifiers of everything a caller asked to have deleted that still exists.
    ///
    /// Identifiers rather than a boolean, because a leak nobody can name is a leak nobody
    /// can clean up. Two things land here: a delete that was attempted and failed, and the
    /// build **log group**, which this client cannot delete at all.
    #[getter]
    fn undeleted(&self) -> Vec<String> {
        self.inner.undeleted.clone()
    }

    /// Whether the terminate call was accepted.
    #[getter]
    fn terminate_accepted(&self) -> bool {
        self.inner.terminate_accepted
    }

    /// Whether the image was deleted, or `None` when deletion was not asked for.
    #[getter]
    fn image_deleted(&self) -> Option<bool> {
        self.inner.image_deleted
    }

    /// The lifecycle state the sandbox ended in.
    ///
    /// Commonly `"TERMINATING"` rather than `"TERMINATED"`: the default teardown does not
    /// wait, so claiming TERMINATED would claim an observation nobody made. Pass
    /// `wait_for_terminated=True` to observe it.
    #[getter]
    fn lifecycle(&self) -> Option<String> {
        self.inner
            .lifecycle
            .map(|lifecycle| lifecycle.as_str().to_string())
    }

    /// Every failure the teardown swallowed, in the order it hit them.
    ///
    /// Kept because a teardown that never raises is a teardown whose failures are
    /// invisible otherwise, and the first one is usually the cause of the rest.
    #[getter]
    fn failures(&self) -> Vec<String> {
        self.inner.failures.clone()
    }

    /// Whether anything a caller asked for was left behind.
    #[getter]
    fn leaked(&self) -> bool {
        self.inner.leaked()
    }

    /// The report as a dict, for a JSON envelope.
    fn to_dict<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyDict>> {
        let dict = PyDict::new(py);
        dict.set_item("terminateAccepted", self.inner.terminate_accepted)?;
        dict.set_item("imageDeleted", self.inner.image_deleted)?;
        dict.set_item("lifecycle", self.lifecycle())?;
        dict.set_item("undeleted", self.inner.undeleted.clone())?;
        dict.set_item("failures", self.inner.failures.clone())?;
        dict.set_item("leaked", self.inner.leaked())?;
        Ok(dict)
    }

    fn __repr__(&self) -> String {
        format!(
            "TeardownReport(terminate_accepted={}, leaked={}, undeleted={:?})",
            self.inner.terminate_accepted,
            self.inner.leaked(),
            self.inner.undeleted
        )
    }
}

/// The platform's managed base image, paired with the Dockerfile `FROM` it goes with.
///
/// One class rather than two strings, because the two **must** agree and used to be able
/// to disagree: the Python client's default named the managed base for `baseImageArn` while
/// its Dockerfile hardcoded an unrelated registry literal in its `FROM`, so changing either
/// left the other pointing somewhere else.
#[pyclass(frozen, from_py_object, name = "BaseImage", module = "microvms")]
#[derive(Clone)]
pub struct PyBaseImage {
    inner: BaseImage,
}

#[pymethods]
impl PyBaseImage {
    /// The managed base every `docs/PLATFORM.md` measurement from 2026-08-06 onward used.
    #[staticmethod]
    fn al2023() -> PyBaseImage {
        PyBaseImage {
            inner: BaseImage::al2023(),
        }
    }

    /// A base a caller built themselves.
    ///
    /// `working_dir` is what `docker inspect` reports for `WorkingDir`, and empty means the
    /// image declares none. It is a parameter because a caller with a purpose-built image
    /// is the only one who can say what theirs declares — this client cannot read it
    /// without pulling the manifest. Getting it wrong is what
    /// `inherit_workdir` refuses on.
    #[new]
    #[pyo3(signature = (name, docker_ref, working_dir=""))]
    fn new(name: &str, docker_ref: &str, working_dir: &str) -> PyBaseImage {
        PyBaseImage {
            inner: BaseImage {
                name: name.to_string(),
                docker_ref: docker_ref.to_string(),
                working_dir: working_dir.to_string(),
            },
        }
    }

    /// Goes into `baseImageArn` — the platform's managed base, not a registry ref.
    #[getter]
    fn name(&self) -> &str {
        &self.inner.name
    }

    /// Goes into the Dockerfile `FROM` — the registry ref measured alongside `name`.
    #[getter]
    fn docker_ref(&self) -> &str {
        &self.inner.docker_ref
    }

    #[getter]
    fn working_dir(&self) -> &str {
        &self.inner.working_dir
    }

    fn __repr__(&self) -> String {
        format!(
            "BaseImage(name={:?}, docker_ref={:?})",
            self.inner.name, self.inner.docker_ref
        )
    }
}

/// One MicroVM's whole life.
///
/// The five transitions are `build_image`, `run`, `suspend`, `resume`, and `terminate`, and
/// every state guard lives in the core — see the module docs.
#[pyclass(frozen, name = "Sandbox", module = "microvms")]
pub struct PySandbox {
    /// Shared with every [`crate::session::PySession`] this sandbox hands out, so a
    /// `terminate()` and a session call cannot interleave. `frozen` on the pyclass plus the
    /// `Mutex` is what gives `&mut Sandbox` from a `&self` method — which the core's
    /// transitions require.
    inner: Arc<Mutex<Sandbox>>,
}

impl PySandbox {
    /// Runs `body` against the sandbox with the GIL released.
    ///
    /// One shape for every transition: release the GIL, take the lock, run, drop both
    /// before returning into Python. Nothing here holds the lock across a Python callback,
    /// so it cannot deadlock against the GIL.
    ///
    /// `body` is **synchronous** and calls [`runtime::block_on_detached`] itself. A closure
    /// answering a future that borrows its `&mut Sandbox` argument needs a higher-ranked
    /// bound plus a boxed future at every call site, and the boxing would exist only to
    /// satisfy the signature; blocking inside keeps the borrow local.
    fn detached<T>(&self, py: Python<'_>, body: impl FnOnce(&mut Sandbox) -> T + Send) -> T
    where
        T: Send,
    {
        py.detach(|| {
            let mut guard = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
            body(&mut guard)
        })
    }

    /// Reads something off the sandbox under the lock.
    fn read<T>(&self, body: impl FnOnce(&Sandbox) -> T) -> T {
        let guard = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        body(&guard)
    }
}

#[pymethods]
impl PySandbox {
    /// Resolves credentials for `region` and returns a sandbox with nothing launched.
    ///
    /// `region` is a [`PyRegion`] and not a string, which is TRAP-6 at this boundary: the
    /// five supported regions are named constructors and everything else goes through
    /// `Region.parse` (refused) or `Region.unlisted` (opted into, at the call site).
    #[new]
    fn new(py: Python<'_>, region: PyRegion) -> PyCoreResult<PySandbox> {
        let sandbox = runtime::block_on(py, Sandbox::new(region.inner))?;
        Ok(PySandbox {
            inner: Arc::new(Mutex::new(sandbox)),
        })
    }

    /// The lifecycle state: `"PENDING"`, `"RUNNING"`, `"SUSPENDING"`, `"SUSPENDED"`,
    /// `"TERMINATING"`, or `"TERMINATED"`.
    ///
    /// Spelled as the service spells it, because a reader compares it against a
    /// `GetMicrovm` response and `Suspended` beside `SUSPENDED` reads like two facts.
    #[getter]
    fn lifecycle(&self) -> String {
        self.read(|sandbox| sandbox.lifecycle().as_str().to_string())
    }

    /// Whether the agent token has been installed (STATE-2).
    ///
    /// Set by the platform reporting RUNNING, not by the launch call: the run hook is what
    /// delivers the token, and a launch that died during startup delivered nothing.
    #[getter]
    fn token_installed(&self) -> bool {
        self.read(Sandbox::token_installed)
    }

    /// Whether an image is recorded as existing (STATE-1).
    #[getter]
    fn image_exists(&self) -> bool {
        self.read(Sandbox::image_exists)
    }

    /// Whether this VM was ever terminated (STATE-11).
    #[getter]
    fn was_terminated(&self) -> bool {
        self.read(Sandbox::was_terminated)
    }

    /// How many times the token has been installed. Never above one (STATE-3).
    #[getter]
    fn bootstrap_count(&self) -> u32 {
        self.read(Sandbox::bootstrap_count)
    }

    /// The VM id, once launched.
    #[getter]
    fn microvm_id(&self) -> Option<String> {
        self.read(|sandbox| sandbox.microvm().map(|vm| vm.id.clone()))
    }

    /// The proxy endpoint, once launched.
    #[getter]
    fn endpoint(&self) -> Option<String> {
        self.read(|sandbox| sandbox.microvm().map(|vm| vm.endpoint.clone()))
    }

    /// Why the VM is in its current state, when the service said.
    ///
    /// The absence is information: TRAP-8's message distinguishes "no stateReason" from an
    /// empty one.
    #[getter]
    fn state_reason(&self) -> Option<String> {
        self.read(|sandbox| sandbox.microvm().and_then(|vm| vm.state_reason.clone()))
    }

    /// The image, once built.
    #[getter]
    fn image(&self) -> Option<PyImage> {
        self.read(|sandbox| {
            sandbox.image().map(|image| PyImage {
                identifier: image.identifier.clone(),
                name: image.name.clone(),
                version: image.version.clone(),
                state: image.state.clone(),
                size: image.size,
                build_log_group: image.build_log_group(),
            })
        })
    }

    /// The suspended window this sandbox asked for at launch, in seconds.
    ///
    /// `None` before a launch, and for a sandbox that did not send the launch — this client
    /// is the only party that can name the number, because `suspendedDurationSeconds`
    /// exists only in the `RunMicrovm` request and `GetMicrovm` does not return it.
    #[getter]
    fn suspended_window_seconds(&self) -> Option<f64> {
        self.read(|sandbox| {
            sandbox
                .suspended_window()
                .map(|window| window.as_secs_f64())
        })
    }

    /// The session, once launched.
    ///
    /// A new wrapper each call, all reaching the same session under the same lock. There is
    /// no cached `Py<PySession>`: caching one would mean a session object that outlives the
    /// VM it addresses, and the `Held::InSandbox` indirection exists precisely so a
    /// post-terminate call reports the lifecycle rather than a dangling handle.
    #[getter]
    fn session(&self) -> Option<PySession> {
        let has_session = self.read(|sandbox| sandbox.session().is_some());
        has_session.then(|| PySession::in_sandbox(Arc::clone(&self.inner)))
    }

    /// Builds an image and waits for it to become usable.
    ///
    /// Every local guard runs **before** the call, which matters because the create happens
    /// after the caller's artifact upload: a rejection AWS raises costs the upload first.
    ///
    /// # What is deliberately not a parameter
    ///
    /// A `client_token`. There is no such field on the core's request type and none here:
    /// a digest-derived token replays the original create and wedges an image in `CREATING`
    /// for fifteen hours with no error at all (TRAP-1). `token_scope` is a CloudTrail
    /// **label** folded in beside a fresh nonce and cannot become the token.
    ///
    /// A `capabilities` list. `repair_guest_identity` is a bool and the request injects
    /// `["ALL"]` itself, so `["CAP_SYS_ADMIN"]` — the request AWS rejects after the upload
    /// — is not something a caller can write (TRAP-3).
    ///
    /// An `architecture`. The model's enum has exactly one value, so the only thing a field
    /// could express is a rejected request.
    #[pyo3(signature = (
        *,
        name,
        binary,
        code_artifact_uri,
        build_role_arn,
        size=None,
        base_image=None,
        dockerfile=None,
        repair_guest_identity=false,
        inherit_workdir=false,
        run_hook_timeout=None,
        build_hook_timeout=None,
        tags=None,
        token_scope=None,
    ))]
    #[allow(
        clippy::too_many_arguments,
        reason = "one keyword-only parameter per \
         CreateImageRequest field; the two hook timeouts are separate typed parameters on \
         purpose, because that is what makes transposing them impossible"
    )]
    fn build_image(
        &self,
        py: Python<'_>,
        name: &str,
        binary: Vec<u8>,
        code_artifact_uri: &str,
        build_role_arn: &str,
        size: Option<PySizeClass>,
        base_image: Option<PyBaseImage>,
        dockerfile: Option<String>,
        repair_guest_identity: bool,
        inherit_workdir: bool,
        run_hook_timeout: Option<PyRunHookTimeout>,
        build_hook_timeout: Option<PyBuildHookTimeout>,
        tags: Option<std::collections::BTreeMap<String, String>>,
        token_scope: Option<String>,
    ) -> PyCoreResult<PyImage> {
        let mut request = CreateImageRequest::new(name, binary, code_artifact_uri, build_role_arn);
        if let Some(size) = size {
            request.size = size.inner;
        }
        if let Some(base) = base_image {
            request.base_image = base.inner;
        }
        request.dockerfile = dockerfile;
        request.repair_guest_identity = repair_guest_identity;
        request.inherit_workdir = inherit_workdir;
        if let Some(timeout) = run_hook_timeout {
            request.run_hook_timeout = timeout.inner;
        }
        if let Some(timeout) = build_hook_timeout {
            request.build_hook_timeout = timeout.inner;
        }
        if let Some(tags) = tags {
            request.tags = tags;
        }
        request.token_scope = token_scope;

        let built = self.detached(py, move |sandbox| {
            runtime::block_on_detached(sandbox.build_image(request)).map(|image| PyImage {
                identifier: image.identifier.clone(),
                name: image.name.clone(),
                version: image.version.clone(),
                state: image.state.clone(),
                size: image.size,
                build_log_group: image.build_log_group(),
            })
        })?;
        Ok(built)
    }

    /// The artifact bytes to upload to `code_artifact_uri`.
    ///
    /// The upload is the caller's: S3 is not in the core's dependency set. Same parameters
    /// as [`Self::build_image`] so the bytes a caller puts in the bucket are the bytes the
    /// build will receive.
    #[pyo3(signature = (
        *,
        name,
        binary,
        code_artifact_uri,
        build_role_arn,
        base_image=None,
        dockerfile=None,
        inherit_workdir=false,
    ))]
    #[allow(
        clippy::too_many_arguments,
        reason = "the CreateImageRequest fields the \
         artifact actually depends on; fewer would mean the bytes a caller uploads could \
         differ from the bytes the build receives"
    )]
    fn build_artifact<'py>(
        &self,
        py: Python<'py>,
        name: &str,
        binary: Vec<u8>,
        code_artifact_uri: &str,
        build_role_arn: &str,
        base_image: Option<PyBaseImage>,
        dockerfile: Option<String>,
        inherit_workdir: bool,
    ) -> PyCoreResult<Bound<'py, PyBytes>> {
        let mut request = CreateImageRequest::new(name, binary, code_artifact_uri, build_role_arn);
        if let Some(base) = base_image {
            request.base_image = base.inner;
        }
        request.dockerfile = dockerfile;
        request.inherit_workdir = inherit_workdir;
        let bytes = self.read(|sandbox| sandbox.build_artifact_for(&request));
        Ok(PyBytes::new(py, &bytes.map_err(crate::errors::CoreError)?))
    }

    /// Launches a MicroVM, waits for RUNNING, and returns its session.
    ///
    /// # What the core refuses here, and this file does not
    ///
    /// A second `run` on one sandbox, with **zero** control-plane calls: the agent token is
    /// installed at most once per VM lifetime (STATE-3), and a second VM needs a second
    /// `Sandbox`. A run with no image at all, before any call. Neither check is in this
    /// file.
    ///
    /// `agent_token` is optional because the common case is a per-VM secret nobody needs to
    /// see; a caller who has one already — a harness minting its own, or a retry that must
    /// reuse the first attempt's — passes it. It rides in `runHookPayload`, which is what
    /// keeps it out of the shared image snapshot.
    #[pyo3(signature = (
        *,
        image_identifier=None,
        execution_role_arn=None,
        agent_token=None,
        launch_env=None,
        egress=false,
        max_idle_sec=None,
        suspended_sec=None,
        auto_resume=false,
        max_duration_sec=None,
        ready_timeout=None,
        token_scope=None,
    ))]
    #[allow(
        clippy::too_many_arguments,
        reason = "one keyword-only parameter per \
         RunRequest field"
    )]
    fn run(
        &self,
        py: Python<'_>,
        image_identifier: Option<String>,
        execution_role_arn: Option<String>,
        agent_token: Option<String>,
        // `launch_env` is the base environment for every exec in the launched VM,
        // delivered in the same `runHookPayload` as the token and applied *under* each
        // exec's own `env`. It shares the token's 4096-byte payload budget, checked
        // locally before the launch. Not a doc comment: a doc comment on a function
        // parameter is a compile error.
        launch_env: Option<std::collections::HashMap<String, String>>,
        egress: bool,
        max_idle_sec: Option<u32>,
        suspended_sec: Option<u32>,
        auto_resume: bool,
        max_duration_sec: Option<u32>,
        ready_timeout: Option<f64>,
        token_scope: Option<String>,
    ) -> PyCoreResult<PySession> {
        // Every unset window falls back to the core's own default rather than to a number
        // written here: ten-minute idle and suspended windows, a one-hour ceiling, and the
        // five-minute ready wait are measured figures, and a second copy of them in a
        // binding is a second thing to keep in step (the JS binding defers the same way).
        let defaults = RunRequest::new();
        let request = RunRequest {
            image_identifier,
            execution_role_arn,
            agent_token,
            launch_env: launch_env.unwrap_or(defaults.launch_env),
            egress,
            max_idle_sec: max_idle_sec.unwrap_or(defaults.max_idle_sec),
            suspended_sec: suspended_sec.unwrap_or(defaults.suspended_sec),
            auto_resume,
            max_duration_sec: max_duration_sec.unwrap_or(defaults.max_duration_sec),
            ready_timeout: match ready_timeout {
                Some(timeout) => seconds(timeout)?,
                None => defaults.ready_timeout,
            },
            token_scope,
        };
        // `run` answers `&mut Session`, which cannot cross back into Python — so the
        // return value is discarded and the session is reached through the sandbox. That
        // is not a workaround: it is what makes a post-terminate session call report the
        // lifecycle instead of addressing a VM that is gone.
        self.detached(py, move |sandbox| {
            runtime::block_on_detached(sandbox.run(request)).map(|_| ())
        })?;
        Ok(PySession::in_sandbox(Arc::clone(&self.inner)))
    }

    /// Freezes the VM and waits for the platform to report it.
    ///
    /// A freeze and restore rather than a stop and start: the guest keeps its memory, so
    /// the token, the filesystem, and every exec record survive. The one thing that does
    /// not is the guest's view of time — it observes the whole suspension as a single jump,
    /// so any timeout, lease, or TLS session a running command holds expires at once on
    /// resume.
    ///
    /// A suspend from anything but RUNNING is refused by the core with zero control-plane
    /// calls (STATE-5). Returns the state reached, which may be `"TERMINATED"`: a VM that
    /// dies while suspending is a state to report rather than an exception out of the
    /// middle of a teardown.
    fn suspend(&self, py: Python<'_>) -> PyCoreResult<String> {
        self.detached(py, |sandbox| {
            runtime::block_on_detached(sandbox.suspend())?;
            Ok(sandbox.lifecycle().as_str().to_string())
        })
        .map_err(crate::errors::CoreError)
    }

    /// Thaws the VM and returns a usable session.
    ///
    /// # What the core refuses, before any wire call
    ///
    /// A resume after `terminate` (STATE-11) — a terminated VM never returns to RUNNING,
    /// and even a call the service accepted would hand back a different machine. A resume
    /// from anything but SUSPENDED (STATE-7). And a resume past the launch-time suspended
    /// window (STATE-12), which is the one worth knowing about: the `idlePolicy` terminates
    /// a suspended VM once that window passes, so there is nothing left to resume, and
    /// calling would cost the full poll timeout to learn something worse.
    ///
    /// Nothing is re-delivered: no run-hook payload, no token, no bootstrap. The in-memory
    /// token survived the freeze, and re-delivering it would hit the daemon's one-shot
    /// bootstrap and be refused — a 409 that reads like a broken VM.
    fn resume(&self, py: Python<'_>) -> PyCoreResult<PySession> {
        self.detached(py, |sandbox| {
            runtime::block_on_detached(sandbox.resume()).map(|_| ())
        })?;
        Ok(PySession::in_sandbox(Arc::clone(&self.inner)))
    }

    /// Tears down, best-effort, **never raising**.
    ///
    /// Order: VM, then image, then the log group last, because the service can recreate a
    /// group deleted before its image.
    ///
    /// Both deletions are opt-in, because both destroy something a caller may still want:
    /// the image is reusable across runs, and the log group is where a failed build's only
    /// evidence lives. `delete_log_group=True` **names** the group in
    /// `report.undeleted` rather than deleting it — CloudWatch is not in the core's
    /// dependency set, and reporting a leak beats reporting a clean teardown over one.
    ///
    /// `wait_for_terminated=False` by default: the caller is on the way out, and a teardown
    /// that blocked five minutes on a state nobody reads is five minutes of a CI job. The
    /// report then honestly ends in `"TERMINATING"`.
    #[pyo3(signature = (
        *,
        delete_image=false,
        delete_log_group=false,
        delete_attempts=None,
        delete_backoff=None,
        wait_for_terminated=false,
    ))]
    fn terminate(
        &self,
        py: Python<'_>,
        delete_image: bool,
        delete_log_group: bool,
        delete_attempts: Option<u32>,
        delete_backoff: Option<f64>,
        wait_for_terminated: bool,
    ) -> PyCoreResult<PyTeardownReport> {
        // The two retry knobs default to the core's own figures rather than to numbers
        // written here: twenty attempts fifteen seconds apart is the difference between a
        // clean account and a billed leak, and restating them would put a second copy of
        // that measurement in a binding.
        let defaults = TeardownOpts::default();
        let mut opts = TeardownOpts {
            delete_image,
            delete_log_group,
            delete_attempts: delete_attempts.unwrap_or(defaults.delete_attempts),
            delete_backoff: match delete_backoff {
                Some(backoff) => seconds(backoff)?,
                None => defaults.delete_backoff,
            },
            wait_for_terminated: defaults.wait_for_terminated,
        };
        if wait_for_terminated {
            opts = opts.waiting_for_terminated();
        }
        // `terminate` answers a report rather than a `Result`, so the `Ok` here is this
        // wrapper's and never the core's — a teardown cannot raise, which is the whole
        // point of the report.
        let report = self.detached(py, move |sandbox| {
            runtime::block_on_detached(sandbox.terminate(opts))
        });
        Ok(PyTeardownReport { inner: report })
    }

    /// `with sandbox as s:` — returns the sandbox itself.
    fn __enter__(slf: PyRef<'_, Self>) -> PyRef<'_, Self> {
        slf
    }

    /// Tears down on the way out, whatever happened inside the block.
    ///
    /// Returns `False`, so an exception raised inside the block propagates: the teardown is
    /// what has to happen, not what has to be reported. Any leak is on the report the
    /// caller can also get by calling `terminate()` themselves.
    #[pyo3(signature = (exc_type=None, exc_value=None, traceback=None))]
    fn __exit__(
        &self,
        py: Python<'_>,
        exc_type: Option<Py<PyAny>>,
        exc_value: Option<Py<PyAny>>,
        traceback: Option<Py<PyAny>>,
    ) -> bool {
        let _ = (exc_type, exc_value, traceback);
        let opts = TeardownOpts::default();
        // The report is discarded here on purpose. `__exit__` runs where a `finally`
        // would, and there is nowhere to return a value to; a caller who needs the report
        // calls `terminate()` explicitly, which is the documented path.
        let _ = self.detached(py, move |sandbox| {
            runtime::block_on_detached(sandbox.terminate(opts))
        });
        false
    }

    fn __repr__(&self) -> String {
        self.read(|sandbox| {
            format!(
                "Sandbox(lifecycle={:?}, microvm_id={:?}, bootstrap_count={})",
                sandbox.lifecycle().as_str(),
                sandbox.microvm().map(|vm| vm.id.as_str()),
                sandbox.bootstrap_count(),
            )
        })
    }
}

/// Registers the sandbox surface on the module.
pub(crate) fn register(module: &Bound<'_, PyModule>) -> PyResult<()> {
    module.add_class::<PySandbox>()?;
    module.add_class::<PyImage>()?;
    module.add_class::<PyBaseImage>()?;
    module.add_class::<PyTeardownReport>()?;
    Ok(())
}
