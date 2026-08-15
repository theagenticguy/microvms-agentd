// SPDX-License-Identifier: Apache-2.0
//! The `Sandbox` lifecycle: one VM's whole life, with the state machine the symspec
//! model describes made true of the code.
//!
//! Composes [`crate::control`] and [`crate::session`] into the surface the CLI and the
//! bindings use: build, run, suspend, resume, terminate, the launch-time suspended
//! window (STATE-12), and teardown in the order that does not leak.
//!
//! # The lifecycle is a field, and every transition is guarded
//!
//! [`Lifecycle`] is the symspec's `vm_state` verbatim, and [`Sandbox`] carries the other
//! four variables beside it — `token_installed`, `image_exists`, `was_terminated`,
//! `bootstrap_count`. The Z3 proofs over that model (bootstrap at most once, suspend from
//! non-RUNNING unreachable, TERMINATED never returns to RUNNING) are proofs about *this*
//! struct's reachable states, which is only worth something if the transitions here are
//! the only way to move it. They are: every one of those fields is private, and every
//! mutation happens in one of the five methods below.
//!
//! # Runtime-checked rather than typestate, deliberately
//!
//! The packet offered `Sandbox<Running>` returning a `Suspended` handle, which would make
//! STATE-5's wrong call a compile error rather than a local refusal — strictly stronger on
//! the ladder in [`crate`]. It is not what landed, for the reason the packet names as
//! acceptable: T-W3-8 wraps **one** object for PyO3 and napi-rs, and a type whose Rust
//! identity changes on every transition cannot be one `#[pyclass]`. A typestate sandbox
//! would be re-erased into a runtime-checked enum at the binding boundary, so the check
//! would exist twice with the binding's copy being the one most callers actually hit.
//!
//! What is kept from the typestate idea is the part that costs nothing: the check happens
//! **before** the wire call, so a suspend from SUSPENDED is refused with zero
//! control-plane calls rather than answered by AWS. The test asserts the call count, which
//! is the observable that distinguishes the two.
//!
//! # The suspended window is the client's alone (STATE-12)
//!
//! `suspendedDurationSeconds` exists only in the `RunMicrovm` **request**. `GetMicrovm`
//! does not return it, so the client that sent the launch is the only party that can name
//! the window it asked for — and the launch-time `idlePolicy` *terminates* a suspended VM
//! once that window passes, which means "resume later" silently stops working. A resume
//! past the window is refused locally, before `ResumeMicrovm`, because the alternative is
//! calling and reading the failure: the service answers about a terminated id, which is
//! not the same statement as "the window you set at launch closed", and getting there
//! costs the full poll timeout first.
//!
//! # Teardown never raises, and the log group is last
//!
//! [`Sandbox::terminate`] returns a [`TeardownReport`] rather than a `Result`. It runs
//! where a caller's `finally` would, and an error raised there replaces the real failure
//! with a teardown failure — the real one being the one worth reading.
//!
//! The order is VM, then image (retrying), then the log group **last**, because the
//! service can recreate a group deleted before its image. See
//! [`TeardownReport::undeleted`] for what this crate can and cannot delete.
//!
//! # There is no `Drop` that tears down
//!
//! Rust has no context manager and `Drop` cannot await. A `Drop` that blocked on a runtime
//! would deadlock inside one; a `Drop` that spawned would race the process exit. So
//! [`Sandbox`]'s `Drop` only **warns** about a live VM, naming the id, and the rule is
//! that a caller calls [`Sandbox::terminate`] explicitly.

use std::sync::Arc;
use std::time::Duration;

use crate::control::{
    ControlPlane, CreateImageRequest, Image, Microvm, RunHookPayload, RunMicrovmRequest, WaitOpts,
};
use crate::error::{Error, ErrorKind};
use crate::region::Region;
use crate::session::{Session, TokenMinter};

/// The default launch wait: five minutes, matching the Python client's `ready_timeout_sec`.
pub const DEFAULT_READY_TIMEOUT: Duration = Duration::from_secs(300);

/// The default lifecycle wait for suspend, resume, and terminate.
pub const DEFAULT_LIFECYCLE_TIMEOUT: Duration = Duration::from_secs(300);

/// How many times a teardown retries the image delete.
///
/// Twenty, from the Python client: an image in `CREATING` refuses deletion and a VM still
/// terminating holds a reference, so this is the difference between a clean account and a
/// billed leak rather than politeness.
pub const DEFAULT_DELETE_ATTEMPTS: u32 = 20;

/// The gap between image-delete attempts.
pub const DEFAULT_DELETE_BACKOFF: Duration = Duration::from_secs(15);

/// How often a lifecycle wait polls.
const LIFECYCLE_POLL_INTERVAL: Duration = Duration::from_secs(5);

/// The symspec's `vm_state`, verbatim.
///
/// Six states and no others, which is the S1 half of this module: a lifecycle held as a
/// `String` would let `"RUNNING "` and `"Running"` both exist, and every guard below would
/// have to decide which it meant.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Lifecycle {
    /// The initial state, and the state a launch is accepted into (STATE-1).
    Pending,
    /// The run hook answered with a success status and the token is installed (STATE-2).
    Running,
    /// A suspend was accepted and the platform has not yet reported it complete (STATE-4).
    Suspending,
    /// The platform reported suspension complete (STATE-6).
    Suspended,
    /// A terminate was accepted (STATE-9).
    Terminating,
    /// The platform reported termination complete (STATE-10).
    Terminated,
}

impl Lifecycle {
    /// The name the service uses for this state, which is also what an error message says.
    pub fn as_str(self) -> &'static str {
        match self {
            Lifecycle::Pending => "PENDING",
            Lifecycle::Running => "RUNNING",
            Lifecycle::Suspending => "SUSPENDING",
            Lifecycle::Suspended => "SUSPENDED",
            Lifecycle::Terminating => "TERMINATING",
            Lifecycle::Terminated => "TERMINATED",
        }
    }

    /// Whether a VM in this state is still billing, which is what a `Drop` warning is for.
    pub fn is_live(self) -> bool {
        matches!(
            self,
            Lifecycle::Pending | Lifecycle::Running | Lifecycle::Suspending | Lifecycle::Suspended
        )
    }
}

impl std::fmt::Display for Lifecycle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Everything a launch needs, with the defaults the Python client measured.
///
/// `agent_token` is optional because the common case is a per-VM secret nobody needs to
/// see; a caller who has one already — a harness minting its own, or a retry that must
/// reuse the first attempt's — passes it.
#[derive(Clone, Debug)]
pub struct RunRequest {
    /// The image to launch, or `None` for the one [`Sandbox::build_image`] built.
    pub image_identifier: Option<String>,
    /// The execution role. Optional in the model; every real launch needs one.
    pub execution_role_arn: Option<String>,
    /// The bearer token the daemon will accept, or `None` to mint one.
    pub agent_token: Option<String>,
    /// Base environment for every exec in the launched VM, delivered in the same
    /// `runHookPayload` as the token.
    ///
    /// The daemon applies this *under* each request's own `env`, so a per-exec value
    /// wins on a key both set. Empty by default, and an empty map produces byte-for-byte
    /// the payload this client always sent — a caller who never touches this field
    /// cannot be affected by the field existing.
    ///
    /// It shares the token's 4096-byte payload budget, and the check is local:
    /// [`crate::control::RunHookPayload::for_launch`] refuses an over-ceiling payload
    /// before any call, naming the byte count and how much of it the env is. That
    /// matters here more than for the token, because one bearer token has always fit
    /// with room to spare and a map of credentials does not.
    pub launch_env: std::collections::HashMap<String, String>,
    /// Whether to request the egress connector. Off means no outbound network.
    pub egress: bool,
    /// `idlePolicy.maxIdleDurationSeconds`.
    pub max_idle_sec: u32,
    /// `idlePolicy.suspendedDurationSeconds` — the window STATE-12 refuses past.
    pub suspended_sec: u32,
    /// `idlePolicy.autoResumeEnabled`.
    pub auto_resume: bool,
    /// `maximumDurationInSeconds`, checked against 1..=28800 before the call.
    pub max_duration_sec: u32,
    /// How long to wait for RUNNING.
    pub ready_timeout: Duration,
    /// A label for the run token (TRAP-1). Never the token.
    pub token_scope: Option<String>,
}

impl Default for RunRequest {
    fn default() -> Self {
        Self {
            image_identifier: None,
            execution_role_arn: None,
            agent_token: None,
            launch_env: std::collections::HashMap::new(),
            egress: false,
            max_idle_sec: 600,
            suspended_sec: 600,
            auto_resume: false,
            max_duration_sec: 3_600,
            ready_timeout: DEFAULT_READY_TIMEOUT,
            token_scope: None,
        }
    }
}

impl RunRequest {
    /// A launch with the measured defaults: ingress only, ten-minute idle and suspended
    /// windows, a one-hour maximum duration, no auto-resume.
    pub fn new() -> Self {
        Self::default()
    }

    /// Launches `identifier` rather than the built image.
    #[must_use]
    pub fn with_image(mut self, identifier: impl Into<String>) -> Self {
        self.image_identifier = Some(identifier.into());
        self
    }

    /// Requests the egress connector, which is what gives the VM outbound network.
    #[must_use]
    pub fn with_egress(mut self) -> Self {
        self.egress = true;
        self
    }

    /// Sets the suspended window this sandbox will refuse a resume past (STATE-12).
    #[must_use]
    pub fn with_suspended_sec(mut self, seconds: u32) -> Self {
        self.suspended_sec = seconds;
        self
    }

    /// Reuses a caller-supplied agent token rather than minting one.
    #[must_use]
    pub fn with_agent_token(mut self, token: impl Into<String>) -> Self {
        self.agent_token = Some(token.into());
        self
    }

    /// Adds one launch-environment variable, which every exec in the VM starts with.
    ///
    /// One pair per call rather than a whole map, because that is how a caller builds
    /// one — from flags, from a config file, one credential at a time — and a
    /// map-taking setter makes the second call silently discard the first. The field is
    /// public for a caller who really does hold a map.
    #[must_use]
    pub fn with_launch_env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.launch_env.insert(key.into(), value.into());
        self
    }
}

/// What a teardown should delete beyond the VM itself.
///
/// Both deletions are opt-in, because both destroy something a caller may still want: the
/// image is reusable across runs, and the log group is where a failed build's only evidence
/// lives.
#[derive(Clone, Copy, Debug)]
pub struct TeardownOpts {
    /// Whether to delete the image the sandbox built.
    pub delete_image: bool,
    /// Whether the build log group should be deleted.
    ///
    /// This crate **cannot** delete it — CloudWatch is not in the dependency set — so
    /// asking names the group in [`TeardownReport::undeleted`] rather than removing it.
    /// See that field for why naming is the honest answer rather than a silent success.
    pub delete_log_group: bool,
    /// How many times the image delete is retried.
    pub delete_attempts: u32,
    /// The gap between image-delete attempts.
    pub delete_backoff: Duration,
    /// How long to wait for TERMINATED, or `None` to return as soon as the terminate call
    /// is accepted.
    ///
    /// `None` by default, matching the Python client: the caller is on the way out, and a
    /// teardown that blocked five minutes on a state nobody reads is five minutes of a CI
    /// job. A caller that needs STATE-10 *observed* passes
    /// [`TeardownOpts::waiting_for_terminated`].
    pub wait_for_terminated: Option<Duration>,
}

impl Default for TeardownOpts {
    fn default() -> Self {
        Self {
            delete_image: false,
            delete_log_group: false,
            delete_attempts: DEFAULT_DELETE_ATTEMPTS,
            delete_backoff: DEFAULT_DELETE_BACKOFF,
            wait_for_terminated: None,
        }
    }
}

impl TeardownOpts {
    /// Deletes the image as well as the VM.
    #[must_use]
    pub fn deleting_image(mut self) -> Self {
        self.delete_image = true;
        self
    }

    /// Asks for the log group too, which names it rather than deleting it.
    #[must_use]
    pub fn deleting_log_group(mut self) -> Self {
        self.delete_log_group = true;
        self
    }

    /// Waits for TERMINATED before returning (STATE-10).
    #[must_use]
    pub fn waiting_for_terminated(mut self) -> Self {
        self.wait_for_terminated = Some(DEFAULT_LIFECYCLE_TIMEOUT);
        self
    }
}

/// What a teardown did, and what it left behind.
///
/// Returned rather than raised. See the module docs: this runs where a `finally` would.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct TeardownReport {
    /// Identifiers of everything a caller asked to have deleted that still exists.
    ///
    /// The CLI emits these (CLI-6), which is the whole reason they are identifiers rather
    /// than a boolean: a leak nobody can name is a leak nobody can clean up.
    ///
    /// Two things land here. A delete that was attempted and failed — an image whose twenty
    /// attempts all hit a conflict. And the build **log group**, which this crate cannot
    /// delete at all, because CloudWatch is absent from the dependency set T-W2-2 froze and
    /// this lane adds only source files. Naming it is strictly better than the
    /// alternatives: deleting it is unavailable, and silently succeeding would report a
    /// clean teardown over six accumulated log groups — which is how the leak was found in
    /// the first place.
    pub undeleted: Vec<String>,
    /// Whether the terminate call was accepted.
    pub terminate_accepted: bool,
    /// Whether the image was deleted, or `None` when deletion was not asked for.
    pub image_deleted: Option<bool>,
    /// The lifecycle state the sandbox ended in.
    pub lifecycle: Option<Lifecycle>,
    /// Every failure the teardown swallowed, in the order it hit them.
    ///
    /// Kept because a teardown that never raises is a teardown whose failures are invisible
    /// otherwise, and the first one is usually the cause of the rest.
    pub failures: Vec<String>,
}

impl TeardownReport {
    /// Whether anything a caller asked for was left behind.
    pub fn leaked(&self) -> bool {
        !self.undeleted.is_empty()
    }
}

/// Mints proxy tokens for one MicroVM through the control plane.
///
/// The bridge between the two lanes: `ControlPlane::mint_auth_token` answers a
/// `control::ProxyToken`, [`TokenMinter`] wants a `session::ProxyToken`, and the `From`
/// impl at `session/proxy.rs` is the conversion — one `.into()` at the boundary, which is
/// why that impl is `From` rather than a named function.
///
/// The plane sits behind an `Arc` rather than a reference because the minter must outlive
/// the call that built the session: minting happens inside the request path on every later
/// request, which is what makes it happen at all (TRAP-9).
struct ControlPlaneMinter {
    control: Arc<ControlPlane>,
    microvm_id: String,
}

impl TokenMinter for ControlPlaneMinter {
    fn mint(
        &self,
    ) -> futures_util::future::BoxFuture<'_, Result<crate::session::ProxyToken, Error>> {
        Box::pin(async move {
            let minted = self.control.mint_auth_token(&self.microvm_id).await?;
            Ok(minted.into())
        })
    }

    /// Overrides the default, which ignores the ports and delegates.
    ///
    /// This is the minter that has a control plane behind it, so it is the one that can
    /// actually widen a token's scope — and without this override
    /// `Session::connect_headers(8080)` would keep answering a header pair behind a token
    /// scoped to 9000 only, which the proxy refuses with 403 `Access to port denied`. See
    /// [`TokenMinter::mint_for_ports`] for the measurement.
    fn mint_for_ports(
        &self,
        ports: &[u16],
    ) -> futures_util::future::BoxFuture<'_, Result<crate::session::ProxyToken, Error>> {
        let specs: Vec<crate::control::ops::PortSpecification> = ports
            .iter()
            .map(|port| crate::control::ops::PortSpecification::port(*port))
            .collect();
        Box::pin(async move {
            let minted = self
                .control
                .mint_auth_token_for(&self.microvm_id, &specs)
                .await?;
            Ok(minted.into())
        })
    }
}

/// One MicroVM's whole life.
///
/// See the module docs for the state machine, the window, and why teardown is explicit.
pub struct Sandbox {
    control: Arc<ControlPlane>,
    image: Option<Image>,
    microvm: Option<Microvm>,
    session: Option<Session>,

    // ── the symspec's five variables ─────────────────────────────────────────
    lifecycle: Lifecycle,
    token_installed: bool,
    image_exists: bool,
    was_terminated: bool,
    bootstrap_count: u32,

    /// The window from *our own* `RunMicrovm` request. `GetMicrovm` does not return it.
    suspended_window: Option<Duration>,
    /// The clock reading when the suspend call was accepted, or `None` when not suspended.
    suspended_at: Option<Duration>,
    /// Set by [`Sandbox::terminate`], so `Drop` can tell an abandoned VM from a torn-down
    /// one.
    torn_down: bool,
}

impl std::fmt::Debug for Sandbox {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // No agent token: it is a credential, and a `Debug` that printed it would put it in
        // every log line that formats a sandbox.
        f.debug_struct("Sandbox")
            .field("lifecycle", &self.lifecycle)
            .field("microvm", &self.microvm.as_ref().map(|vm| &vm.id))
            .field("image", &self.image.as_ref().map(|image| &image.identifier))
            .field("token_installed", &self.token_installed)
            .field("bootstrap_count", &self.bootstrap_count)
            .field("was_terminated", &self.was_terminated)
            .field("suspended_window", &self.suspended_window)
            .finish_non_exhaustive()
    }
}

impl Sandbox {
    /// Resolves credentials for `region` and returns a sandbox with nothing launched.
    ///
    /// TRAP-6 is closed by [`Region`] before this is reached, which is why there is no
    /// region check here.
    pub async fn new(region: Region) -> Result<Self, Error> {
        Ok(Self::with_control_plane(ControlPlane::new(region).await?))
    }

    /// A sandbox over an already-built [`ControlPlane`].
    ///
    /// The seam every test uses, and public for the same reason `ControlPlane::with_transport`
    /// is: a caller who wants a non-default port, or a fake transport, has already built the
    /// plane, and a sandbox constructible only from a [`Region`] would be a sandbox no test
    /// can drive without AWS.
    ///
    /// The plane's own clock is what times the suspended window, rather than a second one
    /// this type would own. Two clocks in one lifecycle is the trap the daemon's turmoil
    /// tests record: a window measured on one clock while the poll loop sleeps on another
    /// calls a closed window open.
    pub fn with_control_plane(control: ControlPlane) -> Self {
        Self {
            control: Arc::new(control),
            image: None,
            microvm: None,
            session: None,
            lifecycle: Lifecycle::Pending,
            token_installed: false,
            image_exists: false,
            was_terminated: false,
            bootstrap_count: 0,
            suspended_window: None,
            suspended_at: None,
            torn_down: false,
        }
    }

    // ── the symspec's five variables, readable ───────────────────────────────

    /// The lifecycle state, which is the symspec's `vm_state`.
    pub fn lifecycle(&self) -> Lifecycle {
        self.lifecycle
    }

    /// Whether the agent token has been installed (STATE-2).
    pub fn token_installed(&self) -> bool {
        self.token_installed
    }

    /// Whether an image is recorded as existing (STATE-1).
    pub fn image_exists(&self) -> bool {
        self.image_exists
    }

    /// Whether this VM was ever terminated (STATE-11).
    pub fn was_terminated(&self) -> bool {
        self.was_terminated
    }

    /// How many times the token has been installed. Never above one (STATE-3).
    pub fn bootstrap_count(&self) -> u32 {
        self.bootstrap_count
    }

    /// The image, once built.
    pub fn image(&self) -> Option<&Image> {
        self.image.as_ref()
    }

    /// The VM as the service last described it.
    pub fn microvm(&self) -> Option<&Microvm> {
        self.microvm.as_ref()
    }

    /// The session, once launched.
    pub fn session(&self) -> Option<&Session> {
        self.session.as_ref()
    }

    /// The suspended window this sandbox asked for at launch, once it has launched.
    pub fn suspended_window(&self) -> Option<Duration> {
        self.suspended_window
    }

    // ── build ────────────────────────────────────────────────────────────────

    /// Builds an image and waits for it to become usable.
    ///
    /// Every local guard runs inside [`ControlPlane::create_image`], before the call —
    /// which matters because the create happens *after* the caller's artifact upload, so a
    /// rejection AWS raises costs the upload first.
    pub async fn build_image(&mut self, request: CreateImageRequest) -> Result<&Image, Error> {
        let size = request.size;
        let created = self.control.create_image(request).await?;
        let built = self
            .control
            .wait_for_image(&created.identifier, size, WaitOpts::default())
            .await?;
        // Recorded before any launch, because a built image exists whether or not anything
        // is ever run from it — and the teardown has to be able to name it either way.
        self.image_exists = true;
        self.image = Some(built);
        Ok(self.image.as_ref().expect("just assigned"))
    }

    /// The artifact bytes to upload to the request's `code_artifact_uri`.
    ///
    /// The upload is the caller's: S3 is not in this crate's dependency set.
    pub fn build_artifact_for(&self, request: &CreateImageRequest) -> Result<Vec<u8>, Error> {
        self.control.build_artifact_for(request)
    }

    /// The ARN for `identifier`: an ARN passes through with zero calls, a bare name is
    /// resolved through the image listing by exact match.
    ///
    /// A read-only delegation to [`ControlPlane::resolve_image_arn`], on this type so the
    /// CLI's launch path reaches it through the one sandbox it already holds. It touches
    /// none of the five state-machine variables — resolution is a question about the
    /// account, not about this VM's lifecycle.
    pub async fn resolve_image_arn(&self, identifier: &str) -> Result<String, Error> {
        self.control.resolve_image_arn(identifier).await
    }

    /// The image with exactly `name`, or `None`. Read-only; see
    /// [`ControlPlane::find_image_by_name`] for the pagination and exact-match rules.
    pub async fn find_image_by_name(
        &self,
        name: &str,
    ) -> Result<Option<crate::control::ops::MicrovmImageSummaryWire>, Error> {
        self.control.find_image_by_name(name).await
    }

    /// The content hash `build --reuse` keys an image name to. Local; zero calls.
    pub fn artifact_content_hash_for(&self, request: &CreateImageRequest) -> String {
        self.control.artifact_content_hash_for(request)
    }

    // ── run (STATE-1, STATE-2, STATE-3) ──────────────────────────────────────

    /// Launches a MicroVM, waits for RUNNING, and returns its session.
    ///
    /// # The three state requirements this is
    ///
    /// STATE-1: the accepted launch moves the lifecycle to PENDING and records the image as
    /// existing. STATE-2: the platform reporting RUNNING is what marks the token installed
    /// — not the launch call, because the run hook is what delivers it and a launch that
    /// dies during startup delivered nothing. STATE-3: `bootstrap_count` is incremented
    /// exactly here, and a second `run` on the same sandbox is refused, which is what makes
    /// "at most once per VM lifetime" a property of the type rather than of a caller's
    /// discipline.
    ///
    /// The agent token rides in `runHookPayload`, which is what keeps it out of the shared
    /// image snapshot. That is safe because the platform forwards no external traffic until
    /// the run hook returns 200, so a per-VM secret delivered at launch wins the
    /// first-writer race through the endpoint.
    pub async fn run(&mut self, request: RunRequest) -> Result<&mut Session, Error> {
        // STATE-3's local half. A sandbox that has already bootstrapped cannot bootstrap
        // again, and the refusal is here rather than in a comment because `run` twice is
        // the plausible mistake — a retry loop around a launch that timed out.
        if self.bootstrap_count > 0 || self.microvm.is_some() {
            return Err(Error::invalid_arg(format!(
                "this sandbox has already launched a VM ({} bootstrap(s), lifecycle {}), and the \
                 agent token is installed at most once per VM lifetime (STATE-3). A second VM \
                 needs a second Sandbox — reusing this one would either re-deliver a run-hook \
                 payload to a daemon whose one-shot bootstrap refuses it, or silently address \
                 two guests through one handle.",
                self.bootstrap_count, self.lifecycle,
            )));
        }

        let Some(identifier) = request
            .image_identifier
            .clone()
            .or_else(|| self.image.as_ref().map(|image| image.identifier.clone()))
        else {
            return Err(Error::new(
                ErrorKind::Precondition,
                "no image to launch: pass RunRequest::with_image or call build_image first."
                    .to_string(),
            ));
        };

        let agent_token = request.agent_token.clone().unwrap_or_else(mint_agent_token);
        // Checked even though this builds the JSON itself, because neither half is ours:
        // the token may be caller-supplied, so someone passing a signed blob rather than a
        // bearer token is exactly who this catches (TRAP-5), and the launch env is entirely
        // the caller's. This is the pre-flight refusal — over-ceiling fails here with the
        // byte count, before the launch, rather than as a `ValidationException` on a member
        // the caller did not know they were filling.
        let payload = RunHookPayload::for_launch(&agent_token, &request.launch_env)?;

        let mut wire = RunMicrovmRequest::new(&identifier, payload);
        wire.execution_role_arn = request.execution_role_arn.clone();
        wire.max_idle_sec = request.max_idle_sec;
        wire.suspended_sec = request.suspended_sec;
        wire.auto_resume = request.auto_resume;
        wire.max_duration_sec = request.max_duration_sec;
        wire.token_scope = request.token_scope.clone();
        if request.egress {
            wire = wire.with_egress();
        }

        let launched = self.control.run_microvm(wire).await?;

        // STATE-1: the launch was accepted.
        self.lifecycle = Lifecycle::Pending;
        self.image_exists = true;
        // Recorded here rather than after the wait, because the window the idlePolicy
        // enforces was set by *this* request and a launch that then fails still leaves a VM
        // the caller may have to reason about.
        self.suspended_window = Some(Duration::from_secs(u64::from(request.suspended_sec)));
        let id = launched.id.clone();
        self.microvm = Some(launched);

        let running = self
            .control
            .wait_for_running(
                &id,
                WaitOpts {
                    timeout: request.ready_timeout,
                    poll_interval: LIFECYCLE_POLL_INTERVAL,
                    stall_grace: Duration::MAX,
                },
            )
            .await?;

        // STATE-2. The platform reported the run hook succeeded, so the token is in the
        // guest's memory — and this is the one place that counts it (STATE-3).
        self.lifecycle = Lifecycle::Running;
        self.token_installed = true;
        self.bootstrap_count += 1;
        let endpoint = running.endpoint.clone();
        self.microvm = Some(running);

        let minter = Arc::new(ControlPlaneMinter {
            control: Arc::clone(&self.control),
            microvm_id: id,
        });
        self.session = Some(
            Session::builder(endpoint, agent_token)
                .with_minter(minter)
                .with_port(self.control.port())
                .build()?,
        );
        Ok(self.session.as_mut().expect("just assigned"))
    }

    // ── suspend (STATE-4, STATE-5, STATE-6) ──────────────────────────────────

    /// Freezes the VM and waits for the platform to report it.
    ///
    /// A freeze and restore rather than a stop and start: the guest keeps its memory, so the
    /// token, the filesystem, and every exec record survive. The one thing that does not is
    /// the guest's view of time — it observes the whole suspension as a single jump, so any
    /// timeout, lease, or TLS session a running command holds expires at once on resume.
    ///
    /// # STATE-5 is checked before the wire, not after
    ///
    /// A suspend from anything but RUNNING is refused here with **zero** control-plane
    /// calls. That is the observable difference between this and a client that lets AWS
    /// answer, and it is what the test asserts on.
    pub async fn suspend(&mut self) -> Result<(), Error> {
        let id = self.require_microvm("suspend")?;

        // STATE-5.
        if self.lifecycle != Lifecycle::Running {
            return Err(Error::invalid_arg(format!(
                "microvm {id} is {} and a suspend is only issued from RUNNING (STATE-5). Refused \
                 here rather than by the service, because the service's answer about a \
                 non-running id does not say which of the two things went wrong — and a suspend \
                 issued from SUSPENDED is a caller who believes they resumed.",
                self.lifecycle,
            )));
        }

        // STATE-4: accepted, so the lifecycle moves before the wait. Acceptance is the
        // wire call succeeding, so the assignment comes after it: moving to SUSPENDING
        // first would leave a failed call (a throttle, a dead transport) stuck in a state
        // neither suspend nor resume accepts, bricking the handle over one bad request.
        self.control.suspend(&id).await?;
        self.lifecycle = Lifecycle::Suspending;
        // Stamped after the call and before the wait, not after the wait: the idlePolicy's
        // window starts when the platform begins suspending, so timing it from SUSPENDED
        // would under-count the transition and call a closed window open.
        self.suspended_at = Some(self.control.clock().elapsed());

        let settled = self
            .control
            .wait_for_state(
                &id,
                &crate::control::microvm::SUSPEND_WANTED,
                &[],
                self.lifecycle_wait(),
            )
            .await?;

        // STATE-6, and the TERMINATED case beside it. A VM that dies while suspending is a
        // state to report rather than an exception out of the middle of a teardown, so the
        // wait *wants* TERMINATED — and recording it here is what stops a resume from being
        // offered afterwards (STATE-11).
        self.lifecycle = match settled.state.as_str() {
            "SUSPENDED" => Lifecycle::Suspended,
            "TERMINATED" => {
                self.was_terminated = true;
                Lifecycle::Terminated
            }
            other => {
                return Err(Error::new(
                    ErrorKind::Platform,
                    format!(
                        "the suspend wait returned {other}, which is neither SUSPENDED nor \
                         TERMINATED — and those two are what this client asked for."
                    ),
                ));
            }
        };
        self.microvm = Some(settled);
        Ok(())
    }

    // ── resume (STATE-7, STATE-8, STATE-12) ──────────────────────────────────

    /// Thaws the VM and returns a usable session.
    ///
    /// # What is deliberately not re-delivered (STATE-7)
    ///
    /// Nothing. No run-hook payload, no token, no bootstrap. The in-memory token survived
    /// the freeze, and re-delivering it would hit the daemon's one-shot bootstrap and be
    /// refused — a 409 that reads like a broken VM.
    ///
    /// # The window is checked first (STATE-12)
    ///
    /// Before any wire call, because the answer is already known and calling costs the poll
    /// timeout to learn something worse. The falsification is a service reporting
    /// TERMINATED: without this check the resume burns the full deadline and then reports a
    /// state the client could have named at once.
    ///
    /// # The proxy token is dropped (STATE-8)
    ///
    /// [`Session::rebind`] invalidates it. The endpoint URL does not change across
    /// suspend/resume, so the rebind is usually a no-op on the URL — but a token minted
    /// against the pre-suspend instance may no longer validate, and that rejection reads
    /// exactly like a dead daemon.
    pub async fn resume(&mut self) -> Result<&mut Session, Error> {
        let id = self.require_microvm("resume")?;

        // STATE-11's local half: a terminated VM never returns to RUNNING, so the refusal
        // comes before the window check and before any call.
        if self.was_terminated || self.lifecycle == Lifecycle::Terminated {
            return Err(Error::invalid_arg(format!(
                "microvm {id} was terminated, and a terminated VM never returns to RUNNING \
                 (STATE-11). There is nothing to resume: the guest's memory is gone, so even a \
                 call the service accepted would hand back a different machine."
            )));
        }
        if self.lifecycle != Lifecycle::Suspended {
            return Err(Error::invalid_arg(format!(
                "microvm {id} is {} and a resume is only issued from SUSPENDED (STATE-7).",
                self.lifecycle,
            )));
        }

        // STATE-12, first and locally.
        self.require_open_suspended_window(&id)?;

        self.control.resume(&id).await?;
        // `fail_on` is the *dead* states rather than the terminal ones: SUSPENDED is the
        // state this call was made from, so failing on it would fail every resume. A VM the
        // idlePolicy terminated during suspension never reaches RUNNING, and waiting only
        // for RUNNING there burns the full timeout and then reports a timeout message
        // hiding a cause the service had already stated in `stateReason`.
        let running = self
            .control
            .wait_for_state(
                &id,
                &["RUNNING"],
                &crate::constants::DEAD_STATES,
                self.lifecycle_wait(),
            )
            .await?;

        self.lifecycle = Lifecycle::Running;
        // STATE-8, through the endpoint the service just reported rather than the one held:
        // the URL is measured not to change, and reading it from the response is what makes
        // that a fact this code depends on rather than an assumption it encodes.
        let endpoint = running.endpoint.clone();
        self.microvm = Some(running);
        if let Some(session) = self.session.as_mut() {
            session.rebind(endpoint);
        }
        // Cleared on success so the next cycle's window is measured from the next suspend.
        // Leaving it set would accumulate every suspension's elapsed time into one total and
        // reject a resume whose own window is wide open.
        self.suspended_at = None;

        self.session.as_mut().ok_or_else(|| {
            Error::new(
                ErrorKind::Unexpected,
                "the VM resumed but this sandbox holds no session, which run() always builds"
                    .to_string(),
            )
        })
    }

    /// Rejects a resume the launch-time `idlePolicy` has already made impossible (STATE-12).
    ///
    /// The message names the elapsed time, the window, and the `idlePolicy` finding, because
    /// "cannot resume" alone sends a reader looking for the flag that reopens it.
    fn require_open_suspended_window(&self, id: &str) -> Result<(), Error> {
        let (Some(window), Some(since)) = (self.suspended_window, self.suspended_at) else {
            // No window recorded means this sandbox did not send the launch — the attach
            // path — and guessing a default would refuse a resume the service would honour.
            return Ok(());
        };
        let elapsed = self.control.clock().elapsed().saturating_sub(since);
        if elapsed <= window {
            return Ok(());
        }
        Err(Error::new(
            ErrorKind::WindowClosed,
            format!(
                "microvm {id} has been suspended {}s, past the {}s suspendedDurationSeconds \
                 window set at launch — the idlePolicy terminates a suspended VM once that window \
                 passes, so there is nothing left to resume (docs/PLATFORM.md, '`idlePolicy`'). \
                 Refused before ResumeMicrovm because suspendedDurationSeconds exists only in the \
                 RunMicrovm request: GetMicrovm does not return it, so this client is the only \
                 party that can name the number. A longer window has to be set at launch on the \
                 next VM; there is no call that extends this one.",
                elapsed.as_secs(),
                window.as_secs(),
            ),
        ))
    }

    // ── terminate (STATE-9, STATE-10) ────────────────────────────────────────

    /// Tears down, best-effort, never erroring.
    ///
    /// Order: VM, then image, then the log group **last**. The log group is last because the
    /// service can recreate a group deleted before its image — and see
    /// [`TeardownReport::undeleted`] for why this crate names it rather than deleting it.
    pub async fn terminate(&mut self, opts: TeardownOpts) -> TeardownReport {
        let mut report = TeardownReport::default();
        self.torn_down = true;

        // The session first: it holds a cached proxy token whose only remaining use would be
        // a request against a VM that is going away.
        self.session = None;

        // 1. The VM.
        if let Some(id) = self.microvm.as_ref().map(|vm| vm.id.clone()) {
            // STATE-9. Recorded before the call, so a terminate whose call fails still marks
            // the VM as one this client asked to destroy — which is what stops a later
            // resume (STATE-11) rather than leaving the sandbox looking resumable.
            self.lifecycle = Lifecycle::Terminating;
            self.was_terminated = true;

            match self.control.terminate(&id).await {
                Ok(()) => report.terminate_accepted = true,
                Err(error) => {
                    report.failures.push(format!("terminate {id}: {error}"));
                    report.undeleted.push(id.clone());
                }
            }

            if report.terminate_accepted
                && let Some(timeout) = opts.wait_for_terminated
            {
                let wait = WaitOpts {
                    timeout,
                    poll_interval: LIFECYCLE_POLL_INTERVAL,
                    stall_grace: Duration::MAX,
                };
                match self
                    .control
                    .wait_for_state(&id, &["TERMINATED"], &[], wait)
                    .await
                {
                    // STATE-10.
                    Ok(settled) => {
                        self.lifecycle = Lifecycle::Terminated;
                        self.microvm = Some(settled);
                    }
                    // Not a leak: the platform accepted the terminate, so the VM is on its
                    // way out and the lifecycle stays TERMINATING honestly.
                    Err(error) => report
                        .failures
                        .push(format!("waiting for {id} to reach TERMINATED: {error}")),
                }
            }
        }

        // 2. The image, retrying — an image in CREATING refuses deletion and a VM still
        //    terminating holds a reference.
        if opts.delete_image {
            match self.image.as_ref().map(|image| image.identifier.clone()) {
                Some(identifier) => {
                    let deleted = self
                        .control
                        .delete_image(&identifier, opts.delete_attempts, opts.delete_backoff)
                        .await;
                    report.image_deleted = Some(deleted);
                    if deleted {
                        self.image_exists = false;
                    } else {
                        report.failures.push(format!(
                            "the image {identifier} survived {} delete attempts",
                            opts.delete_attempts.max(1)
                        ));
                        report.undeleted.push(identifier);
                    }
                }
                None => report.image_deleted = Some(false),
            }
        }

        // 3. The log group, LAST. Named rather than deleted; see TeardownReport::undeleted.
        if opts.delete_log_group
            && let Some(group) = self.image.as_ref().map(Image::build_log_group)
        {
            report.failures.push(format!(
                "the build log group {group} was not deleted: CloudWatch Logs is not in this \
                 crate's dependency set, so it is reported rather than removed. It is \
                 service-created, which means no Terraform stack owns it and `terraform destroy` \
                 leaves it behind — six accumulated before anyone noticed."
            ));
            report.undeleted.push(group);
        }

        report.lifecycle = Some(self.lifecycle);
        report
    }

    // ── internals ────────────────────────────────────────────────────────────

    /// The VM id, or a precondition error naming what was attempted.
    fn require_microvm(&self, what: &str) -> Result<String, Error> {
        self.microvm
            .as_ref()
            .map(|vm| vm.id.clone())
            .ok_or_else(|| {
                Error::new(
                    ErrorKind::Precondition,
                    format!("nothing to {what}: this sandbox has not launched a VM."),
                )
            })
    }

    /// Five minutes at five-second polls, for suspend, resume, and terminate.
    fn lifecycle_wait(&self) -> WaitOpts {
        WaitOpts {
            timeout: DEFAULT_LIFECYCLE_TIMEOUT,
            poll_interval: LIFECYCLE_POLL_INTERVAL,
            stall_grace: Duration::MAX,
        }
    }
}

/// Warns about a live VM rather than tearing it down.
///
/// See the module docs: `Drop` cannot await, so a teardown here would either deadlock
/// inside a runtime or race the process exit. The warning names the id, because the only
/// useful thing a drop can do is tell whoever reads stderr what to go delete.
///
/// `eprintln!` rather than a log macro because this crate has no logging dependency, and
/// taking one on to warn about a leak would be a dependency for a diagnostic.
impl Drop for Sandbox {
    fn drop(&mut self) {
        if self.torn_down {
            return;
        }
        if let Some(vm) = self.microvm.as_ref()
            && self.lifecycle.is_live()
        {
            eprintln!(
                "warning: the Sandbox for microvm {} was dropped in {} without terminate(). \
                 Nothing was torn down — Drop cannot await, so a teardown here would deadlock \
                 inside a runtime. The VM bills until its maximumDurationInSeconds ceiling: \
                 terminate it with `microvm terminate {}`.",
                vm.id, self.lifecycle, vm.id,
            );
        }
    }
}

/// A fresh per-VM bearer token: 32 bytes of randomness as 64 hex characters.
///
/// # Why `/dev/urandom` and not a crate
///
/// The dependency set T-W2-2 froze carries no CSPRNG and this lane adds only source files,
/// so what remains is the kernel pool directly — which is what `getrandom` reaches on Linux
/// anyway, and the daemon this drives is Linux-only. `control::token` reads it the same way
/// for the same reason; the duplication is deliberate rather than shared, because unifying
/// it would mean editing another lane's module for four lines.
///
/// # Why the fallback is not silent
///
/// A failed read falls back to `RandomState` mixed with the nanosecond clock and a stack
/// address, which is weaker and still per-VM distinct. That is the property that matters
/// here — this token is delivered once, over a channel the platform does not forward until
/// the hook returns 200, to a daemon that installs it at most once — and the alternative,
/// failing a launch for want of 32 bytes, turns an unreadable `/dev/urandom` into an
/// unusable client.
fn mint_agent_token() -> String {
    use std::fmt::Write as _;
    use std::io::Read as _;

    let mut bytes = [0u8; 32];
    let read = std::fs::File::open("/dev/urandom")
        .ok()
        .and_then(|mut file| file.read_exact(&mut bytes).ok())
        .is_some();
    if !read {
        use std::collections::hash_map::RandomState;
        use std::hash::{BuildHasher as _, Hasher as _};
        for chunk in bytes.chunks_mut(8) {
            let mut hasher = RandomState::new().build_hasher();
            hasher.write_usize(chunk.as_ptr() as usize);
            hasher.write_u128(
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|since| since.as_nanos())
                    .unwrap_or_default(),
            );
            let mixed = hasher.finish().to_le_bytes();
            chunk.copy_from_slice(&mixed[..chunk.len()]);
        }
    }

    let mut token = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(token, "{byte:02x}");
    }
    token
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::control::fake::{self as fake, Answer, FakeControlPlane, TestClock};
    use crate::control::transport::Method;

    /// A sandbox over the contract recorder, plus the two handles a test asserts through.
    ///
    /// The recorder is T-W2-4's, deliberately: its answers are literal JSON in the service
    /// model's spelling rather than values serialized from this crate's types, so a member
    /// this lane misreads cannot be misread identically by the fake.
    fn planted() -> (Sandbox, Arc<FakeControlPlane>, Arc<TestClock>) {
        let recorder = Arc::new(FakeControlPlane::new());
        let clock = Arc::new(TestClock::new());
        let plane = ControlPlane::with_transport(
            Arc::clone(&recorder) as Arc<dyn crate::control::transport::Transport>,
            Region::UsEast1,
            Arc::clone(&clock) as Arc<dyn crate::control::Clock>,
        );
        (Sandbox::with_control_plane(plane), recorder, clock)
    }

    /// Queues everything a launch to RUNNING needs.
    fn answer_launch(recorder: &FakeControlPlane) {
        recorder
            .answer(
                "RunMicrovm",
                Answer::ok(fake::microvm_response("PENDING", None)),
            )
            .answer(
                "GetMicrovm",
                Answer::ok(fake::microvm_response("RUNNING", None)),
            )
            .answer(
                "CreateMicrovmAuthToken",
                Answer::ok(fake::auth_token_response("proxy-token")),
            );
    }

    /// A launched sandbox in RUNNING, which is where four of the twelve keys start.
    async fn launched() -> (Sandbox, Arc<FakeControlPlane>, Arc<TestClock>) {
        let (mut sandbox, recorder, clock) = planted();
        answer_launch(&recorder);
        sandbox
            .run(RunRequest::new().with_image("arn:image"))
            .await
            .expect("the launch reaches RUNNING");
        (sandbox, recorder, clock)
    }

    /// Drives a launched sandbox to SUSPENDED, which is where resume starts.
    ///
    /// Both `GetMicrovm` answers are queued **before** the launch rather than one before
    /// each wait, and that is not a style choice. The recorder repeats its last queued
    /// answer, so a queue left at `[RUNNING]` after the launch and then appended to becomes
    /// `[RUNNING, SUSPENDED]` — the suspend wait pops the stale RUNNING, sleeps a poll
    /// interval, and the fake's `sleep` **advances the clock**. That five seconds is
    /// invisible until it lands in the middle of the STATE-12 window arithmetic, which is
    /// exactly where it first showed up. Queueing both up front means every wait matches on
    /// its first poll and no test clock moves except when a test moves it.
    async fn suspended_with_window(
        suspended_sec: u32,
    ) -> (Sandbox, Arc<FakeControlPlane>, Arc<TestClock>) {
        let (mut sandbox, recorder, clock) = planted();
        recorder
            .answer(
                "RunMicrovm",
                Answer::ok(fake::microvm_response("PENDING", None)),
            )
            .answer(
                "GetMicrovm",
                Answer::ok(fake::microvm_response("RUNNING", None)),
            )
            .answer(
                "GetMicrovm",
                Answer::ok(fake::microvm_response("SUSPENDED", None)),
            )
            .answer(
                "CreateMicrovmAuthToken",
                Answer::ok(fake::auth_token_response("proxy-token")),
            )
            .answer("SuspendMicrovm", Answer::ok(fake::empty_response()));

        sandbox
            .run(
                RunRequest::new()
                    .with_image("arn:image")
                    .with_suspended_sec(suspended_sec),
            )
            .await
            .expect("launches");
        sandbox.suspend().await.expect("suspends");
        assert_eq!(
            clock.now(),
            Duration::ZERO,
            "no wait may have slept, or the window arithmetic below is measuring the fake's \
             poll interval as well as the elapsed suspension"
        );
        (sandbox, recorder, clock)
    }

    /// A suspended sandbox at the default ten-minute window.
    async fn suspended() -> (Sandbox, Arc<FakeControlPlane>, Arc<TestClock>) {
        suspended_with_window(600).await
    }

    /// **STATE-1 and STATE-2.** A launch accepted is PENDING with the image recorded as
    /// existing; the platform reporting the run hook succeeded is what marks the token
    /// installed.
    ///
    /// The two halves are asserted with the *same* fake because they are the same call, and
    /// what distinguishes them is which fact is recorded when: `image_exists` is true from
    /// the accepted launch, `token_installed` only from the RUNNING report. A client that
    /// set both at the launch call would pass an end-state assertion and be wrong about a
    /// VM that died during startup — which is the next test.
    #[tokio::test]
    async fn a_launch_records_the_image_and_the_running_report_installs_the_token() {
        let (mut sandbox, recorder, _) = planted();
        assert_eq!(sandbox.lifecycle(), Lifecycle::Pending);
        assert!(
            !sandbox.image_exists(),
            "nothing is recorded before a launch"
        );
        assert!(!sandbox.token_installed());

        answer_launch(&recorder);
        let session = sandbox
            .run(RunRequest::new().with_image("arn:image"))
            .await
            .expect("launches");
        assert_eq!(
            session.endpoint(),
            "https://mvm-abc123.microvm.us-east-1.amazonaws.com"
        );

        assert_eq!(sandbox.lifecycle(), Lifecycle::Running);
        assert!(sandbox.image_exists(), "STATE-1: the image is recorded");
        assert!(sandbox.token_installed(), "STATE-2: the token is installed");
        assert_eq!(sandbox.bootstrap_count(), 1);
        assert_eq!(sandbox.microvm().expect("launched").id, "mvm-abc123");

        // The payload really carried the token, read off the wire member rather than from
        // the request type — a field on a struct proves nothing about what was emitted.
        let body = recorder.first_body("RunMicrovm");
        let payload = body["runHookPayload"].as_str().expect("a string");
        assert!(payload.starts_with(r#"{"agent_token":"#), "{payload}");
        assert_eq!(body["idlePolicy"]["suspendedDurationSeconds"], 600);
    }

    /// A launch env reaches the wire member the daemon parses, and it is the only thing
    /// that changes about the request.
    ///
    /// Read off `runHookPayload` rather than off the request struct: a field on a struct
    /// proves nothing about what was emitted, which is the same argument the test above
    /// makes about the token.
    #[tokio::test]
    async fn a_launch_env_reaches_the_run_hook_payload_on_the_wire() {
        let (mut sandbox, recorder, _) = planted();
        answer_launch(&recorder);
        sandbox
            .run(
                RunRequest::new()
                    .with_image("arn:image")
                    .with_launch_env("ANTHROPIC_BASE_URL", "https://gateway.example")
                    .with_launch_env("PATH", "/usr/local/bin:/usr/bin:/bin"),
            )
            .await
            .expect("launches");

        let body = recorder.first_body("RunMicrovm");
        let payload = body["runHookPayload"].as_str().expect("a string");
        // One parse deeper, because that is where the daemon reads it from too.
        let inner: serde_json::Value =
            serde_json::from_str(payload).expect("the payload is itself JSON");
        assert!(
            inner["agent_token"].as_str().is_some_and(|t| !t.is_empty()),
            "the token still rides alongside: {payload}"
        );
        assert_eq!(
            inner["env"]["ANTHROPIC_BASE_URL"],
            "https://gateway.example"
        );
        assert_eq!(inner["env"]["PATH"], "/usr/local/bin:/usr/bin:/bin");
    }

    /// **The local refusal, with zero control-plane calls.**
    ///
    /// The observable difference between this client and one that lets AWS answer, and the
    /// same shape as STATE-5's suspend refusal: an over-budget launch env fails before the
    /// launch rather than as a `ValidationException` on a member the caller did not know
    /// they were filling. botocore does not enforce the ceiling client-side, so without
    /// this there is no local signal at all.
    ///
    /// **Falsification** — build the payload after `run_microvm` instead of before, and the
    /// call count assertion goes red.
    #[tokio::test]
    async fn an_over_budget_launch_env_is_refused_before_any_control_plane_call() {
        let (mut sandbox, recorder, _) = planted();
        answer_launch(&recorder);

        let error = sandbox
            .run(
                RunRequest::new()
                    .with_image("arn:image")
                    .with_launch_env("AWS_SESSION_TOKEN", "t".repeat(4096)),
            )
            .await
            .expect_err("a credential-scale launch env does not fit the payload");

        assert_eq!(error.kind(), ErrorKind::InvalidArg);
        let message = error.to_string();
        assert!(message.contains("ceiling of 4096"), "{message}");
        assert!(message.contains("launch env contributed"), "{message}");
        assert_eq!(
            recorder.calls().len(),
            0,
            "the refusal has to be local: a launch that reached AWS costs a VM and a bill"
        );
        // And the sandbox is left usable rather than half-launched, so a caller can trim
        // the env and retry through the same handle.
        assert_eq!(sandbox.lifecycle(), Lifecycle::Pending);
        assert_eq!(sandbox.bootstrap_count(), 0);
        assert!(sandbox.microvm().is_none());
    }

    /// A launch whose VM dies during startup leaves the token **not** installed, because
    /// the run hook is what delivers it and a dead VM ran no hook.
    ///
    /// The distinction this pins is the one the test above cannot: both a correct client and
    /// one that installs the token at the launch call reach RUNNING with the token set, and
    /// only this case separates them.
    #[tokio::test]
    async fn a_launch_that_dies_during_startup_installs_no_token() {
        let (mut sandbox, recorder, _) = planted();
        recorder
            .answer(
                "RunMicrovm",
                Answer::ok(fake::microvm_response("PENDING", None)),
            )
            .answer(
                "GetMicrovm",
                Answer::ok(fake::microvm_response(
                    "TERMINATED",
                    Some("run hook returned 500"),
                )),
            );

        let error = sandbox
            .run(RunRequest::new().with_image("arn:image"))
            .await
            .expect_err("a VM that died during startup is not a launch");
        assert_eq!(error.kind(), ErrorKind::LaunchDied);
        assert!(
            error.to_string().contains("run hook returned 500"),
            "{error}"
        );

        assert!(
            !sandbox.token_installed(),
            "the run hook never answered, so nothing was installed"
        );
        assert_eq!(
            sandbox.bootstrap_count(),
            0,
            "a bootstrap counted here would make STATE-3's ceiling unreachable for a retry"
        );
        assert_eq!(sandbox.lifecycle(), Lifecycle::Pending);
        assert!(
            sandbox.image_exists(),
            "STATE-1 still holds: the launch was accepted against a real image"
        );
    }

    /// **STATE-3, the guard proof.** A second `run` on one sandbox is refused, and it is
    /// refused with **zero** further control-plane calls.
    ///
    /// The call count is the assertion that matters. A client that let the second launch
    /// through would create a second VM and silently address two guests through one handle,
    /// and it would also deliver a second run-hook payload to a daemon whose one-shot
    /// bootstrap answers 409 — so "an error came back" is not enough to tell the two apart.
    ///
    /// **Falsification** — delete the `bootstrap_count > 0` branch from `run` and this test
    /// is red on both the count and the call total. Verified; see the packet's guard proofs.
    #[tokio::test]
    async fn a_second_run_on_one_sandbox_is_refused_before_any_call() {
        let (mut sandbox, recorder, _) = launched().await;
        assert_eq!(sandbox.bootstrap_count(), 1);
        let before = recorder.calls().len();

        let error = sandbox
            .run(RunRequest::new().with_image("arn:image"))
            .await
            .expect_err("the token is installed at most once per VM lifetime");
        assert_eq!(error.kind(), ErrorKind::InvalidArg);
        assert!(error.to_string().contains("STATE-3"), "{error}");
        assert!(
            error.to_string().contains("needs a second Sandbox"),
            "the remedy has to be nameable: {error}"
        );

        assert_eq!(
            sandbox.bootstrap_count(),
            1,
            "bootstrap_count <= 1 is the Z3-proved invariant"
        );
        assert_eq!(
            recorder.calls().len(),
            before,
            "the refusal must cost no control-plane call"
        );
        assert_eq!(recorder.call_count("RunMicrovm"), 1);
    }

    /// **STATE-4 and STATE-6.** A suspend from RUNNING moves to SUSPENDING before the wait,
    /// and the platform reporting SUSPENDED is what moves it to SUSPENDED.
    #[tokio::test]
    async fn a_suspend_from_running_passes_through_suspending_to_suspended() {
        let (sandbox, recorder, _) = suspended().await;
        assert_eq!(sandbox.lifecycle(), Lifecycle::Suspended);
        assert!(
            sandbox.token_installed(),
            "a freeze keeps the guest's memory, so the token survives"
        );
        assert_eq!(sandbox.bootstrap_count(), 1, "no re-bootstrap on a freeze");

        let calls = recorder.calls();
        let suspend = calls
            .iter()
            .find(|call| call.operation == "SuspendMicrovm")
            .expect("the suspend went out");
        assert_eq!(suspend.method, Method::Post);
        assert_eq!(suspend.path, "/2025-09-09/microvms/mvm-abc123/suspend");
    }

    /// A suspend whose VM dies while suspending reports TERMINATED rather than raising, and
    /// records it — which is what stops a later resume.
    ///
    /// TERMINATED is *wanted* by the suspend wait for a reason: a VM that dies mid-suspend
    /// is a state to report, not an exception out of the middle of a teardown.
    #[tokio::test]
    async fn a_vm_that_dies_while_suspending_is_recorded_rather_than_raised() {
        let (mut sandbox, recorder, _) = launched().await;
        recorder
            .answer("SuspendMicrovm", Answer::ok(fake::empty_response()))
            .answer(
                "GetMicrovm",
                Answer::ok(fake::microvm_response("TERMINATED", Some("idle policy"))),
            );

        sandbox
            .suspend()
            .await
            .expect("a death mid-suspend is a state, not an error");
        assert_eq!(sandbox.lifecycle(), Lifecycle::Terminated);
        assert!(
            sandbox.was_terminated(),
            "STATE-11's precondition is recorded"
        );
    }

    /// A suspend whose wire call fails leaves the sandbox in RUNNING, and a retry works.
    ///
    /// STATE-4's "accepted" is the wire call succeeding. A client that moved to SUSPENDING
    /// before the call would strand a failed call there — a state neither suspend nor
    /// resume accepts, so one throttled or dropped request would brick the handle.
    /// **Falsification** — move `self.lifecycle = Lifecycle::Suspending` back above the
    /// `control.suspend` call and the first assertion here reads SUSPENDING, and the retry
    /// is refused by the STATE-5 guard.
    #[tokio::test]
    async fn a_suspend_whose_call_fails_stays_running_and_can_be_retried() {
        let (mut sandbox, recorder, _) = planted();
        recorder
            .answer(
                "RunMicrovm",
                Answer::ok(fake::microvm_response("PENDING", None)),
            )
            .answer(
                "GetMicrovm",
                Answer::ok(fake::microvm_response("RUNNING", None)),
            )
            .answer(
                "GetMicrovm",
                Answer::ok(fake::microvm_response("SUSPENDED", None)),
            )
            .answer(
                "CreateMicrovmAuthToken",
                Answer::ok(fake::auth_token_response("proxy-token")),
            )
            // A 409 rather than a 429 or a transport cut, because those two are retried
            // inside `send_with_retry` and this test wants the call to fail once, fast.
            .answer("SuspendMicrovm", Answer::failure(409, "ConflictException"))
            .answer("SuspendMicrovm", Answer::ok(fake::empty_response()));

        sandbox
            .run(RunRequest::new().with_image("arn:image"))
            .await
            .expect("launches");

        sandbox
            .suspend()
            .await
            .expect_err("the control plane refused the suspend");
        assert_eq!(
            sandbox.lifecycle(),
            Lifecycle::Running,
            "a refused suspend must not move the lifecycle: SUSPENDING accepts neither a \
             suspend nor a resume, so recording it here would brick the handle"
        );

        sandbox.suspend().await.expect("the retry suspends");
        assert_eq!(sandbox.lifecycle(), Lifecycle::Suspended);
        assert_eq!(recorder.call_count("SuspendMicrovm"), 2);
    }

    /// **STATE-5, the guard proof.** A suspend from anything but RUNNING is refused with
    /// **zero** control-plane calls.
    ///
    /// Every non-RUNNING state, so a state added to `Lifecycle` later is not silently
    /// exempt. The count is the load-bearing assertion: letting the call go and reading the
    /// service's answer also produces an error, and that error is about a non-running id
    /// rather than about which of two things the caller got wrong.
    ///
    /// **Falsification** — delete the `lifecycle != Running` branch from `suspend` and the
    /// SUSPENDED case emits a `SuspendMicrovm` call, turning the count assertion red.
    /// Verified; see the packet's guard proofs.
    #[tokio::test]
    async fn a_suspend_from_a_non_running_state_reaches_no_control_plane_call() {
        // From SUSPENDED — the caller who believes they resumed.
        let (mut sandbox, recorder, _) = suspended().await;
        let before = recorder.call_count("SuspendMicrovm");

        let error = sandbox
            .suspend()
            .await
            .expect_err("a suspend is only issued from RUNNING");
        assert_eq!(error.kind(), ErrorKind::InvalidArg);
        assert!(error.to_string().contains("STATE-5"), "{error}");
        assert!(error.to_string().contains("SUSPENDED"), "{error}");
        assert_eq!(
            recorder.call_count("SuspendMicrovm"),
            before,
            "the refusal must be local: no second suspend went to the wire"
        );

        // And from PENDING, before anything launched at all.
        let (mut fresh, fresh_recorder, _) = planted();
        let error = fresh.suspend().await.expect_err("nothing to suspend");
        assert_eq!(error.kind(), ErrorKind::Precondition);
        assert_eq!(fresh_recorder.calls().len(), 0);
    }

    /// **STATE-7 and STATE-8.** A resume from SUSPENDED returns to RUNNING, re-delivers
    /// **nothing**, and drops the cached proxy token.
    ///
    /// Three assertions, and each one is a different failure mode. The bootstrap count says
    /// no run-hook payload was re-delivered — which a daemon's one-shot bootstrap would
    /// answer 409 to, reading like a broken VM. The `RunMicrovm` count says no second launch
    /// happened. And `is_cached` says the token was invalidated, which is the only
    /// observable difference on a path where the endpoint URL does not change: a client that
    /// kept the token would produce identical requests until the pre-suspend token stopped
    /// validating, and that rejection reads exactly like a dead daemon.
    #[tokio::test]
    async fn a_resume_reuses_the_token_redelivers_nothing_and_drops_the_proxy_token() {
        let (mut sandbox, recorder, _) = suspended().await;

        // The session mints once before the suspend, so there is a cached token to drop.
        let auth = Arc::clone(
            sandbox
                .session()
                .expect("launched")
                .proxy_auth()
                .expect("the launch wired a minter"),
        );
        auth.headers().await.expect("mints");
        assert!(
            auth.is_cached(),
            "there must be a token for the drop to matter"
        );
        assert_eq!(auth.mint_count(), 1);

        recorder
            .answer("ResumeMicrovm", Answer::ok(fake::empty_response()))
            .answer(
                "GetMicrovm",
                Answer::ok(fake::microvm_response("RUNNING", None)),
            );
        let session = sandbox.resume().await.expect("resumes");
        assert_eq!(
            session.endpoint(),
            "https://mvm-abc123.microvm.us-east-1.amazonaws.com"
        );

        assert_eq!(sandbox.lifecycle(), Lifecycle::Running);
        assert!(
            sandbox.token_installed(),
            "STATE-7: the installed token is reused"
        );
        assert_eq!(
            sandbox.bootstrap_count(),
            1,
            "STATE-7: a resume re-delivers no run-hook payload"
        );
        assert_eq!(
            recorder.call_count("RunMicrovm"),
            1,
            "a resume is not a second launch"
        );
        assert!(
            !auth.is_cached(),
            "STATE-8: the cached proxy token survived the resume"
        );
    }

    /// **STATE-8's guard proof, on the mint count.** The request after a resume mints a
    /// fresh token rather than reusing the pre-suspend one.
    ///
    /// Separate from the test above because `is_cached` is a fact about the cache and this is
    /// a fact about the wire: a client that dropped the cache but kept emitting the old
    /// header would pass the first assertion and fail every request.
    ///
    /// **Falsification** — delete the `proxy.invalidate()` line from `Session::rebind` and
    /// the mint count stays at 1 here. Verified; see the packet's guard proofs.
    #[tokio::test]
    async fn the_request_after_a_resume_mints_a_fresh_proxy_token() {
        let (mut sandbox, recorder, _) = suspended().await;
        let auth = Arc::clone(
            sandbox
                .session()
                .expect("launched")
                .proxy_auth()
                .expect("wired"),
        );
        auth.headers().await.expect("mints");
        assert_eq!(auth.mint_count(), 1);

        recorder
            .answer("ResumeMicrovm", Answer::ok(fake::empty_response()))
            .answer(
                "GetMicrovm",
                Answer::ok(fake::microvm_response("RUNNING", None)),
            );
        sandbox.resume().await.expect("resumes");

        // No clock movement at all, so a re-mint here is caused by the invalidation rather
        // than by the refresh window rolling over.
        auth.headers().await.expect("re-mints");
        assert_eq!(
            auth.mint_count(),
            2,
            "STATE-8: the resume did not force a fresh mint"
        );
    }

    /// **STATE-12, the guard proof.** A resume past the launch-time suspended window is
    /// refused locally, with **zero** resume calls, and the message names the window.
    ///
    /// Four assertions, and the packet asks for all four because each rules out a different
    /// wrong implementation. `ErrorKind::WindowClosed` rather than `Timeout` separates this
    /// from the client that waits. Zero `ResumeMicrovm` calls is what "before any wire call"
    /// means. The elapsed reading rules out an implementation that polled to the deadline
    /// first — the fake's clock advances on every `sleep`, so a poll loop here would show up
    /// as time passed. And the message naming both numbers plus `idlePolicy` is what sends a
    /// reader to the finding rather than looking for the flag that reopens the window.
    ///
    /// **Falsification** — delete the `require_open_suspended_window` call from `resume` and
    /// the fake answers TERMINATED, so this test fails with an `ErrorKind::LaunchDied` after
    /// a `ResumeMicrovm` call, naming neither the window nor the seconds elapsed. Verified;
    /// see the packet's guard proofs.
    #[tokio::test]
    async fn a_resume_past_the_suspended_window_is_refused_before_the_wire() {
        // A short window, so the arithmetic is legible: sixty seconds asked for at launch.
        let (mut sandbox, recorder, clock) = suspended_with_window(60).await;
        assert_eq!(sandbox.suspended_window(), Some(Duration::from_secs(60)));

        // The window closes. This is the whole of what the clock injection buys.
        clock.advance(Duration::from_secs(61));

        // What a service says about a VM the idlePolicy already terminated — which is what a
        // client without the local check would spend its poll timeout discovering.
        recorder
            .answer("ResumeMicrovm", Answer::ok(fake::empty_response()))
            .answer(
                "GetMicrovm",
                Answer::ok(fake::microvm_response(
                    "TERMINATED",
                    Some("suspended window elapsed"),
                )),
            );
        let elapsed_before = clock.now();
        let resumes_before = recorder.call_count("ResumeMicrovm");

        let error = sandbox
            .resume()
            .await
            .expect_err("the window the launch set has closed");

        assert_eq!(error.kind(), ErrorKind::WindowClosed);
        assert_eq!(error.code(), "ERR_WINDOW_CLOSED");
        let message = error.to_string();
        assert!(message.contains("61s"), "the elapsed time: {message}");
        assert!(
            message.contains("60s suspendedDurationSeconds"),
            "the window has to be named: {message}"
        );
        assert!(message.contains("idlePolicy"), "the finding: {message}");
        assert!(
            message.contains("GetMicrovm does not return it"),
            "why the client is the only party that can say this: {message}"
        );
        assert_eq!(
            recorder.call_count("ResumeMicrovm"),
            resumes_before,
            "the refusal must come before ResumeMicrovm"
        );
        assert_eq!(
            clock.now(),
            elapsed_before,
            "the refusal must be immediate rather than polled to the deadline"
        );
    }

    /// A resume **inside** the window goes through, so the guard is a comparison rather than
    /// a blanket refusal of every resume.
    ///
    /// The boundary is inclusive: elapsed exactly equal to the window is still open, matching
    /// the Python's `elapsed <= window`.
    #[tokio::test]
    async fn a_resume_inside_the_window_is_allowed_and_the_boundary_is_inclusive() {
        let (mut sandbox, recorder, clock) = suspended_with_window(60).await;

        // Exactly at the window.
        clock.advance(Duration::from_secs(60));
        recorder
            .answer("ResumeMicrovm", Answer::ok(fake::empty_response()))
            .answer(
                "GetMicrovm",
                Answer::ok(fake::microvm_response("RUNNING", None)),
            );
        sandbox
            .resume()
            .await
            .expect("60s == the 60s window is open");
        assert_eq!(sandbox.lifecycle(), Lifecycle::Running);
        assert_eq!(recorder.call_count("ResumeMicrovm"), 1);
    }

    /// A successful resume clears the stamp, so the next cycle's window is measured from the
    /// next suspend rather than from the first one.
    ///
    /// The failure this rules out is subtle and would only show on a second cycle:
    /// accumulating every suspension's elapsed time into one total refuses a resume whose own
    /// window is wide open. The clock is advanced past the window *between* the two cycles,
    /// which is what makes the accumulating implementation fail here.
    #[tokio::test]
    async fn a_successful_resume_clears_the_stamp_so_a_second_cycle_measures_its_own_window() {
        let (mut sandbox, recorder, clock) = planted();
        // Every `GetMicrovm` answer for both cycles, in the order the waits consume them:
        // the launch's RUNNING, then SUSPENDED / RUNNING twice. Queued up front for the
        // reason `suspended_with_window` documents — a stale answer left at the head makes a
        // wait poll twice, and the fake's `sleep` moves the very clock this test is measuring.
        recorder
            .answer(
                "RunMicrovm",
                Answer::ok(fake::microvm_response("PENDING", None)),
            )
            .answer(
                "CreateMicrovmAuthToken",
                Answer::ok(fake::auth_token_response("proxy-token")),
            )
            .answer("SuspendMicrovm", Answer::ok(fake::empty_response()))
            .answer("ResumeMicrovm", Answer::ok(fake::empty_response()))
            .answer(
                "GetMicrovm",
                Answer::ok(fake::microvm_response("RUNNING", None)),
            )
            .answer(
                "GetMicrovm",
                Answer::ok(fake::microvm_response("SUSPENDED", None)),
            )
            .answer(
                "GetMicrovm",
                Answer::ok(fake::microvm_response("RUNNING", None)),
            )
            .answer(
                "GetMicrovm",
                Answer::ok(fake::microvm_response("SUSPENDED", None)),
            )
            .answer(
                "GetMicrovm",
                Answer::ok(fake::microvm_response("RUNNING", None)),
            );
        sandbox
            .run(
                RunRequest::new()
                    .with_image("arn:image")
                    .with_suspended_sec(60),
            )
            .await
            .expect("launches");

        for cycle in 0..2 {
            sandbox
                .suspend()
                .await
                .unwrap_or_else(|error| panic!("cycle {cycle} suspend: {error}"));
            assert_eq!(sandbox.lifecycle(), Lifecycle::Suspended, "cycle {cycle}");

            // Forty-five seconds each time — inside the sixty-second window on its own, and
            // ninety in total. An implementation that never cleared the stamp measures the
            // total and refuses the second resume, which is the whole point of the loop.
            clock.advance(Duration::from_secs(45));

            sandbox.resume().await.unwrap_or_else(|error| {
                panic!("cycle {cycle} resume, whose own window is wide open: {error}")
            });
            assert_eq!(sandbox.lifecycle(), Lifecycle::Running, "cycle {cycle}");
            assert_eq!(
                sandbox.bootstrap_count(),
                1,
                "cycle {cycle}: no re-bootstrap"
            );
        }

        assert_eq!(
            clock.now(),
            Duration::from_secs(90),
            "ninety seconds of accumulated suspension against a sixty-second window: this is \
             the total an implementation that never cleared the stamp would have compared"
        );
        assert_eq!(
            recorder.call_count("ResumeMicrovm"),
            2,
            "both cycles resumed"
        );
    }

    /// **STATE-11, the resume-after-terminate case the packet names.** A terminated VM never
    /// returns to RUNNING, and the recorder sees **zero** resume wire calls.
    ///
    /// The zero count is the assertion the packet asks for, and it is the right one: a client
    /// that called and read the answer would also fail, with whatever the service says about
    /// a terminated id — which is a different statement from "this VM is gone and even a
    /// successful call would hand you a different machine".
    #[tokio::test]
    async fn a_resume_after_terminate_records_zero_resume_calls() {
        let (mut sandbox, recorder, _) = launched().await;
        recorder.answer("TerminateMicrovm", Answer::ok(fake::empty_response()));
        let report = sandbox.terminate(TeardownOpts::default()).await;
        assert!(report.terminate_accepted);
        assert!(sandbox.was_terminated());

        let error = sandbox
            .resume()
            .await
            .expect_err("a terminated VM never returns to RUNNING");
        assert_eq!(error.kind(), ErrorKind::InvalidArg);
        assert!(error.to_string().contains("STATE-11"), "{error}");
        assert_eq!(
            recorder.call_count("ResumeMicrovm"),
            0,
            "no resume may reach the wire once the VM was terminated"
        );
        assert_ne!(
            sandbox.lifecycle(),
            Lifecycle::Running,
            "the lifecycle must not return to RUNNING"
        );
    }

    /// **STATE-9 and STATE-10.** A terminate accepted is TERMINATING with the VM recorded
    /// terminated; the platform reporting termination complete is what marks TERMINATED.
    ///
    /// The two are separate states rather than one because the default teardown does not
    /// wait — so TERMINATING is a state a report really ends in, and calling it TERMINATED
    /// would claim an observation nobody made.
    #[tokio::test]
    async fn a_terminate_records_terminating_and_the_completion_report_marks_terminated() {
        let (mut sandbox, recorder, _) = launched().await;
        recorder.answer("TerminateMicrovm", Answer::ok(fake::empty_response()));

        let report = sandbox.terminate(TeardownOpts::default()).await;
        assert!(report.terminate_accepted, "STATE-9: the call was accepted");
        assert!(sandbox.was_terminated(), "STATE-9: recorded terminated");
        assert_eq!(
            report.lifecycle,
            Some(Lifecycle::Terminating),
            "the default teardown does not wait, so TERMINATED is not claimed"
        );
        assert!(!report.leaked(), "{report:?}");

        // And with the wait asked for, STATE-10 is observed.
        let (mut sandbox, recorder, _) = launched().await;
        recorder
            .answer("TerminateMicrovm", Answer::ok(fake::empty_response()))
            .answer(
                "GetMicrovm",
                Answer::ok(fake::microvm_response("TERMINATED", None)),
            );
        let report = sandbox
            .terminate(TeardownOpts::default().waiting_for_terminated())
            .await;
        assert_eq!(report.lifecycle, Some(Lifecycle::Terminated));
        assert_eq!(sandbox.lifecycle(), Lifecycle::Terminated);
    }

    /// The teardown order is VM, then image, then the log group **last** — asserted on the
    /// recorder's ledger rather than on the report, because the order is a property of what
    /// was emitted.
    ///
    /// The log group is last because the service can recreate a group deleted before its
    /// image. This crate cannot delete it at all, so the assertion is that it is *named
    /// after* the image work rather than that a delete call went out — see
    /// `TeardownReport::undeleted`.
    #[tokio::test]
    async fn teardown_deletes_the_vm_then_the_image_and_names_the_log_group_last() {
        let (mut sandbox, recorder, _) = planted();
        recorder
            .answer(
                "CreateMicrovmImage",
                Answer::created(fake::create_image_response("agentd-conformance")),
            )
            .answer(
                "GetMicrovmImage",
                Answer::ok(fake::get_image_response("agentd-conformance", "CREATED")),
            );
        sandbox
            .build_image(CreateImageRequest::new(
                "agentd-conformance",
                b"binary".to_vec(),
                "s3://bucket/img.zip",
                "arn:aws:iam::123456789012:role/build",
            ))
            .await
            .expect("builds");
        answer_launch(&recorder);
        sandbox
            .run(RunRequest::new())
            .await
            .expect("launches from the built image");

        recorder
            .answer("TerminateMicrovm", Answer::ok(fake::empty_response()))
            .answer(
                "ListMicrovmImageVersions",
                Answer::ok(fake::list_versions_response("1")),
            )
            .answer(
                "DeleteMicrovmImage",
                Answer::ok(fake::delete_image_response()),
            );

        let report = sandbox
            .terminate(
                TeardownOpts::default()
                    .deleting_image()
                    .deleting_log_group(),
            )
            .await;
        assert!(report.terminate_accepted);
        assert_eq!(report.image_deleted, Some(true));

        // The ledger, filtered to the three destructive operations.
        let order: Vec<&str> = recorder
            .operations()
            .into_iter()
            .filter(|operation| matches!(*operation, "TerminateMicrovm" | "DeleteMicrovmImage"))
            .collect();
        assert_eq!(
            order,
            ["TerminateMicrovm", "DeleteMicrovmImage"],
            "the VM goes before the image: a terminating VM holds a reference to it"
        );

        // The log group is named, and named last.
        assert_eq!(
            report.undeleted,
            ["/aws/lambda-microvms/agentd-conformance"],
            "{report:?}"
        );
        assert!(report.leaked());
        assert!(
            report
                .failures
                .last()
                .expect("a failure")
                .contains("log group"),
            "the log group's failure has to be the last one recorded: {report:?}"
        );
        assert!(
            report
                .failures
                .last()
                .expect("a failure")
                .contains("terraform destroy"),
            "the reason it leaks has to be named: {report:?}"
        );
    }

    /// An image delete that fails every attempt lands its identifier in the report rather
    /// than raising, so the CLI can emit what was left behind (CLI-6).
    ///
    /// The identifier rather than a flag is the whole point: a leak nobody can name is a leak
    /// nobody can clean up.
    #[tokio::test]
    async fn a_failing_image_delete_names_what_it_left_behind() {
        let (mut sandbox, recorder, _) = planted();
        recorder
            .answer(
                "CreateMicrovmImage",
                Answer::created(fake::create_image_response("img")),
            )
            .answer(
                "GetMicrovmImage",
                Answer::ok(fake::get_image_response("img", "CREATED")),
            );
        sandbox
            .build_image(CreateImageRequest::new(
                "img",
                b"binary".to_vec(),
                "s3://bucket/img.zip",
                "arn:aws:iam::123456789012:role/build",
            ))
            .await
            .expect("builds");
        answer_launch(&recorder);
        sandbox.run(RunRequest::new()).await.expect("launches");

        recorder
            .answer("TerminateMicrovm", Answer::ok(fake::empty_response()))
            .answer(
                "ListMicrovmImageVersions",
                Answer::ok(fake::list_versions_response("1")),
            )
            .answer(
                "DeleteMicrovmImage",
                Answer::failure(409, "image is in CREATING"),
            );

        let mut opts = TeardownOpts::default().deleting_image();
        opts.delete_attempts = 3;
        opts.delete_backoff = Duration::from_secs(1);
        let report = sandbox.terminate(opts).await;

        assert_eq!(report.image_deleted, Some(false));
        assert_eq!(
            report.undeleted,
            ["arn:aws:lambda:us-east-1:123456789012:microvm-image:img"],
            "{report:?}"
        );
        assert!(
            sandbox.image_exists(),
            "the image really is still there, so the flag must say so"
        );
        assert_eq!(
            recorder.call_count("DeleteMicrovmImage"),
            3,
            "every attempt ran"
        );
    }

    /// A terminate whose own call fails still records the VM as one this client asked to
    /// destroy, and names it as undeleted.
    ///
    /// Recording it is what stops a later resume from being offered against a VM the caller
    /// already tried to kill (STATE-11) — the alternative leaves the sandbox looking
    /// resumable, which is the worse of the two wrong answers.
    #[tokio::test]
    async fn a_terminate_whose_call_fails_still_records_the_intent_and_the_leak() {
        let (mut sandbox, recorder, _) = launched().await;
        recorder.answer(
            "TerminateMicrovm",
            Answer::failure(500, "InternalServerException"),
        );

        let report = sandbox.terminate(TeardownOpts::default()).await;
        assert!(!report.terminate_accepted);
        assert_eq!(report.undeleted, ["mvm-abc123"], "{report:?}");
        assert!(sandbox.was_terminated());
        assert_eq!(report.lifecycle, Some(Lifecycle::Terminating));
        assert!(
            !report.failures.is_empty(),
            "the swallowed failure is recorded"
        );
    }

    /// A teardown on a sandbox that never launched is a no-op report rather than a panic.
    ///
    /// The path a caller reaches by tearing down in a `finally` after a launch that failed
    /// before any VM existed, which is exactly when a teardown that assumed a VM would turn
    /// the real failure into a panic.
    #[tokio::test]
    async fn a_teardown_before_any_launch_is_an_empty_report() {
        let (mut sandbox, recorder, _) = planted();
        let report = sandbox.terminate(TeardownOpts::default()).await;
        assert!(!report.terminate_accepted);
        assert!(!report.leaked());
        assert_eq!(report.lifecycle, Some(Lifecycle::Pending));
        assert_eq!(recorder.calls().len(), 0);
    }

    /// A run with no image at all is a precondition error before any call, rather than a
    /// launch of an empty identifier.
    #[tokio::test]
    async fn a_run_with_no_image_is_refused_before_any_call() {
        let (mut sandbox, recorder, _) = planted();
        let error = sandbox
            .run(RunRequest::new())
            .await
            .expect_err("there is nothing to launch");
        assert_eq!(error.kind(), ErrorKind::Precondition);
        assert!(error.to_string().contains("build_image first"), "{error}");
        assert_eq!(recorder.calls().len(), 0);
    }

    /// The lifecycle's six states are the symspec's `vm_state` domain, spelled as the service
    /// spells them.
    ///
    /// Asserted rather than assumed because the spelling reaches an error message a reader
    /// compares against a `GetMicrovm` response: a lifecycle rendering `Suspended` beside a
    /// service saying `SUSPENDED` reads like two different facts.
    #[test]
    fn the_lifecycle_states_are_the_state_models_domain_in_the_services_spelling() {
        let all = [
            Lifecycle::Pending,
            Lifecycle::Running,
            Lifecycle::Suspending,
            Lifecycle::Suspended,
            Lifecycle::Terminating,
            Lifecycle::Terminated,
        ];
        assert_eq!(
            all.map(Lifecycle::as_str),
            [
                "PENDING",
                "RUNNING",
                "SUSPENDING",
                "SUSPENDED",
                "TERMINATING",
                "TERMINATED"
            ]
        );
        for state in all {
            assert_eq!(state.to_string(), state.as_str());
        }
        // The four live states are the ones a `Drop` warns about, and TERMINATING is not one
        // of them: the platform accepted the terminate, so nobody needs telling.
        assert_eq!(
            all.iter().filter(|state| state.is_live()).count(),
            4,
            "a state added to the enum defaults to not-live, which would silence the warning"
        );
        assert!(!Lifecycle::Terminating.is_live());
        assert!(!Lifecycle::Terminated.is_live());
    }

    /// An agent token is 64 hex characters and never repeats.
    ///
    /// The distinctness matters because the token is the only thing separating one VM's
    /// control API from another's: two VMs sharing a token means either one's caller can
    /// drive the other's guest.
    #[test]
    fn a_minted_agent_token_is_sixty_four_hex_characters_and_never_repeats() {
        let minted: std::collections::HashSet<String> =
            (0..200).map(|_| mint_agent_token()).collect();
        assert_eq!(minted.len(), 200, "a repeated token is a shared guest");
        for token in &minted {
            assert_eq!(token.len(), 64, "{token}");
            assert!(token.chars().all(|c| c.is_ascii_hexdigit()), "{token}");
            assert_ne!(
                token,
                &"0".repeat(64),
                "an all-zero draw is a read that did nothing"
            );
        }
    }

    /// A sandbox's `Debug` does not print the agent token.
    #[tokio::test]
    async fn a_sandbox_debug_does_not_print_the_agent_token() {
        let (mut sandbox, recorder, _) = planted();
        answer_launch(&recorder);
        sandbox
            .run(
                RunRequest::new()
                    .with_image("arn:image")
                    .with_agent_token("super-secret-agent-token"),
            )
            .await
            .expect("launches");

        let rendered = format!("{sandbox:?}");
        assert!(rendered.contains("mvm-abc123"), "{rendered}");
        assert!(rendered.contains("Running"), "{rendered}");
        assert!(
            !rendered.contains("super-secret"),
            "the agent token reached a Debug string: {rendered}"
        );
    }

    /// A torn-down sandbox does not warn on drop, and an abandoned one is the case the
    /// warning exists for.
    ///
    /// Asserted on the flag rather than on stderr — capturing another thread's stderr from a
    /// test is not something this crate can do without a dependency — so what is pinned is
    /// the condition the warning branches on. Both halves, so the branch cannot be
    /// vacuously true.
    #[tokio::test]
    async fn a_torn_down_sandbox_is_distinguishable_from_an_abandoned_one() {
        let (mut sandbox, recorder, _) = launched().await;
        assert!(
            sandbox.lifecycle().is_live() && !sandbox.torn_down,
            "an abandoned live VM is what the drop warning is for"
        );

        recorder.answer("TerminateMicrovm", Answer::ok(fake::empty_response()));
        sandbox.terminate(TeardownOpts::default()).await;
        assert!(
            sandbox.torn_down,
            "terminate must mark the sandbox torn down"
        );
        assert!(
            !sandbox.lifecycle().is_live(),
            "and the lifecycle must no longer read as live"
        );
    }
}
