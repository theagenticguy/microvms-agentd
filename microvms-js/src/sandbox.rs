//! One MicroVM's whole life.
//!
//! # The sandbox is one object, and that is why it wraps cleanly
//!
//! `microvms-core`'s `Sandbox` is runtime-checked rather than typestate, and T-W3-6 chose that
//! *for this file and its Python twin*: a `Sandbox<Running>` returning a `Suspended` handle is
//! stronger in Rust, but a type whose identity changes on every transition cannot be one
//! `#[napi]` class, so it would be re-erased into a runtime check here and the check would
//! exist twice — with the binding's copy being the one every JS caller hits. What survives the
//! choice is the part that costs nothing: the state check happens **before** the wire call, so
//! a suspend from SUSPENDED is refused with zero control-plane calls rather than answered by
//! AWS.
//!
//! # Every guard below belongs to the core
//!
//! There is no state check in this file. `run` twice, `suspend` from PENDING, `resume` past
//! the window, `resume` after `terminate` — every one is refused by the core's own transition,
//! with the core's own message naming the STATE requirement and the `docs/PLATFORM.md`
//! finding. A copy here would be the copy nothing else tests (BIND-2).
//!
//! # The lock is the borrow checker, and it is tokio's
//!
//! The core's transitions take `&mut self`, which a `#[napi]` method taking `&self` cannot
//! give — and napi refuses `&mut self` in an async method for a real reason: the JS engine
//! cannot track Rust mutability across an await. So the sandbox sits behind an
//! `Arc<tokio::sync::Mutex<..>>`, tokio's because the guard is held across an `await`.
//! [`crate::session::Session`] shares that same `Arc`, so a `terminate()` and a session call
//! cannot interleave — which is exactly the exclusion `&mut self` gives in Rust.
//!
//! # There is no automatic teardown
//!
//! JS has no `with` and no deterministic destructor. `FinalizationRegistry` runs at the GC's
//! convenience, on no thread in particular, and a teardown fired from one would race the
//! process exit — the same objection the core records for `Drop`. So the rule here is the
//! core's rule: call [`Sandbox::terminate`] explicitly, in a `finally`.
//!
//! # `terminate` resolves with a report and never rejects
//!
//! It runs where a caller's `finally` would, and an error thrown there replaces the real
//! failure with a teardown failure. `TeardownReport.undeleted` names what was left behind,
//! including the build log group, which this client **cannot** delete — CloudWatch is not in
//! the core's dependency set — so asking names it rather than removing it.

use std::collections::BTreeMap;
use std::sync::Arc;

use microvms_core::control::{BaseImage as CoreBaseImage, CreateImageRequest};
use microvms_core::sandbox::{
    RunRequest, Sandbox as CoreSandbox, TeardownOpts, TeardownReport as CoreTeardownReport,
};
use napi_derive::napi;
use tokio::sync::Mutex;

use crate::cost::SizeClass;
use crate::errors::{AsyncError, js_async};
use crate::exec::seconds_async;
use crate::hooks::{BuildHookTimeout, RunHookTimeout};
use crate::region::Region;
use crate::session::Session;

/// A built image, and the log group the service created alongside it.
#[napi(object)]
pub struct Image {
    /// The image ARN, which is what `imageIdentifier` takes.
    pub identifier: String,
    pub name: String,
    pub version: String,
    pub state: String,
    /// The baseline MiB of the class the request selected.
    ///
    /// Carried because billing follows the baseline requested at *create* time, and by the
    /// time anyone asks what a run cost the request is gone.
    pub baseline_mib: u32,
    /// `/aws/lambda-microvms/<image-name>`.
    ///
    /// The service creates this itself, so no Terraform stack owns it and `terraform destroy`
    /// leaves it behind — "the stack destroyed cleanly" is not "the account is clean". Six
    /// accumulated before anyone noticed.
    pub build_log_group: String,
}

impl Image {
    fn wrap(image: &microvms_core::control::Image) -> Self {
        Self {
            identifier: image.identifier.clone(),
            name: image.name.clone(),
            version: image.version.clone(),
            state: image.state.clone(),
            baseline_mib: image.size.baseline_mib(),
            build_log_group: image.build_log_group(),
        }
    }
}

/// What a teardown did, and what it left behind.
#[napi(object)]
pub struct TeardownReport {
    /// Identifiers of everything a caller asked to have deleted that still exists.
    ///
    /// Identifiers rather than a boolean, because a leak nobody can name is a leak nobody can
    /// clean up. Two things land here: a delete that was attempted and failed, and the build
    /// **log group**, which this client cannot delete at all.
    pub undeleted: Vec<String>,
    /// Whether the terminate call was accepted.
    pub terminate_accepted: bool,
    /// Whether the image was deleted, or `null` when deletion was not asked for.
    pub image_deleted: Option<bool>,
    /// The lifecycle state the sandbox ended in.
    ///
    /// Commonly `"TERMINATING"` rather than `"TERMINATED"`: the default teardown does not
    /// wait, so claiming TERMINATED would claim an observation nobody made. Pass
    /// `waitForTerminated: true` to observe it.
    pub lifecycle: Option<String>,
    /// Every failure the teardown swallowed, in the order it hit them.
    ///
    /// Kept because a teardown that never throws is a teardown whose failures are invisible
    /// otherwise, and the first one is usually the cause of the rest.
    pub failures: Vec<String>,
    /// Whether anything a caller asked for was left behind.
    pub leaked: bool,
}

impl TeardownReport {
    fn wrap(report: CoreTeardownReport) -> Self {
        Self {
            leaked: report.leaked(),
            lifecycle: report
                .lifecycle
                .map(|lifecycle| lifecycle.as_str().to_string()),
            undeleted: report.undeleted,
            terminate_accepted: report.terminate_accepted,
            image_deleted: report.image_deleted,
            failures: report.failures,
        }
    }
}

/// The platform's managed base image, paired with the Dockerfile `FROM` it goes with.
///
/// One value rather than two loose strings, because the two **must** agree and used to be able
/// to disagree: the Python client's default named the managed base for `baseImageArn` while
/// its Dockerfile hardcoded an unrelated registry literal in its `FROM`, so changing either
/// left the other pointing somewhere else.
///
/// `#[napi(object)]` is acceptable here where it is not for a guarded type: the pairing is the
/// point and both halves are required fields, so a structurally-valid object is a valid
/// pairing. `workingDir` is what `docker inspect` reports for `WorkingDir`, and empty means
/// the image declares none — a field because a caller with a purpose-built image is the only
/// one who can say what theirs declares.
#[napi(object)]
pub struct BaseImageInput {
    pub name: String,
    pub docker_ref: String,
    pub working_dir: Option<String>,
}

impl BaseImageInput {
    fn into_core(self) -> CoreBaseImage {
        CoreBaseImage {
            name: self.name,
            docker_ref: self.docker_ref,
            working_dir: self.working_dir.unwrap_or_default(),
        }
    }
}

/// The managed base every `docs/PLATFORM.md` measurement from 2026-08-06 onward used.
#[napi]
pub fn default_base_image() -> BaseImageInput {
    let base = CoreBaseImage::al2023();
    BaseImageInput {
        name: base.name,
        docker_ref: base.docker_ref,
        working_dir: Some(base.working_dir),
    }
}

/// Everything `CreateMicrovmImage` needs.
///
/// # What is deliberately not a field
///
/// A `clientToken`. There is no such field on the core's request type and none here: a
/// digest-derived token replays the original create and wedges an image in `CREATING` for
/// fifteen hours with no error at all (TRAP-1). `tokenScope` is a CloudTrail **label** folded
/// in beside a fresh nonce and cannot become the token.
///
/// A `capabilities` list. `repairGuestIdentity` is a boolean and the request injects `["ALL"]`
/// itself, so `["CAP_SYS_ADMIN"]` — the request AWS rejects after the artifact upload — is not
/// something a caller can write (TRAP-3).
///
/// An `architecture`. The model's enum has exactly one value, so the only thing a field could
/// express is a rejected request.
#[napi(object)]
pub struct BuildImageOptions {
    pub name: String,
    /// The daemon binary's bytes, zipped into the artifact.
    pub binary: napi::bindgen_prelude::Uint8Array,
    /// Where the artifact is uploaded to. This client does not upload — S3 is not in the
    /// core's dependency set — so the caller puts the bytes there and passes the URI.
    pub code_artifact_uri: String,
    /// The build role, which must grant logs on `/aws/lambda-microvms/*`.
    pub build_role_arn: String,
    pub base_image: Option<BaseImageInput>,
    /// A caller-supplied Dockerfile, checked against the base image's `FROM`.
    pub dockerfile: Option<String>,
    /// Whether to repair guest identity. A boolean, not a capability list — see above.
    pub repair_guest_identity: Option<bool>,
    /// Whether the daemon should inherit the image's `WORKDIR`. Refused when nothing declares
    /// one, because the inheritance would silently resolve to `/`.
    pub inherit_workdir: Option<bool>,
    pub tags: Option<std::collections::HashMap<String, String>>,
    /// A CloudTrail-readability **label**, defaulting to the image name. Not the token.
    pub token_scope: Option<String>,
}

/// The three guarded values a build takes, as separate parameters rather than fields.
///
/// # Why they are not in [`BuildImageOptions`]
///
/// Measured, not preferred. A `#[napi(object)]` field holding a class instance must be a
/// `ClassInstance<'a, T>`, which carries raw `napi_value`/`napi_env` pointers and is therefore
/// **not `Send`** — and napi's async path requires `Future: Send`. So an options object with a
/// `size: SizeClass` field cannot be a parameter of an `async fn`, which
/// `Sandbox.buildImage` has to be. The compiler said so in as many words:
/// `future created by async block is not Send ... has type BuildImageOptions<'_> which is not
/// Send`.
///
/// A *reference* parameter — `Option<&SizeClass>` — has no such problem, because napi
/// dereferences it before the future is built. So the guarded types move out of the bag and
/// into the signature, which loses the keyword-argument look and keeps every closure:
///
/// * `size` still refuses an off-table baseline, because the only way to have a `SizeClass` is
///   `SizeClass.fromBaselineMib` or `SizeClass.defaultClass` (TRAP-10).
/// * `runHookTimeout` and `buildHookTimeout` are still two distinct classes, so they still
///   cannot be transposed — which was the whole reason they are types (BIND-2).
///
/// A caller writes `sandbox.buildImage(opts, size, runTimeout, buildTimeout)` with the last
/// three optional.
impl BuildImageOptions {
    /// The core request, with the guarded values applied.
    fn into_request(
        self,
        size: Option<&SizeClass>,
        run_hook_timeout: Option<&RunHookTimeout>,
        build_hook_timeout: Option<&BuildHookTimeout>,
    ) -> CreateImageRequest {
        let mut request = CreateImageRequest::new(
            self.name,
            self.binary.to_vec(),
            self.code_artifact_uri,
            self.build_role_arn,
        );
        if let Some(size) = size {
            request.size = size.inner;
        }
        if let Some(base) = self.base_image {
            request.base_image = base.into_core();
        }
        request.dockerfile = self.dockerfile;
        request.repair_guest_identity = self.repair_guest_identity.unwrap_or(false);
        request.inherit_workdir = self.inherit_workdir.unwrap_or(false);
        if let Some(timeout) = run_hook_timeout {
            request.run_hook_timeout = timeout.inner;
        }
        if let Some(timeout) = build_hook_timeout {
            request.build_hook_timeout = timeout.inner;
        }
        if let Some(tags) = self.tags {
            request.tags = tags.into_iter().collect::<BTreeMap<_, _>>();
        }
        request.token_scope = self.token_scope;
        request
    }
}

/// Everything a launch needs.
#[napi(object)]
pub struct RunOptions {
    /// The image to launch, or omitted for the one `buildImage` built.
    pub image_identifier: Option<String>,
    /// The execution role. Optional in the model; every real launch needs one.
    pub execution_role_arn: Option<String>,
    /// The bearer token the daemon will accept, or omitted to mint one.
    ///
    /// Optional because the common case is a per-VM secret nobody needs to see; a caller who
    /// has one already — a harness minting its own, or a retry that must reuse the first
    /// attempt's — passes it. It rides in `runHookPayload`, which is what keeps it out of the
    /// shared image snapshot.
    pub agent_token: Option<String>,
    /// Whether to request the egress connector. Off means no outbound network.
    pub egress: Option<bool>,
    pub max_idle_sec: Option<u32>,
    /// The window a resume is refused past (STATE-12). Exists **only** in the launch request:
    /// `GetMicrovm` does not return it, so this client is the only party that can name it.
    pub suspended_sec: Option<u32>,
    pub auto_resume: Option<bool>,
    pub max_duration_sec: Option<u32>,
    /// How long to wait for RUNNING.
    pub ready_timeout: Option<f64>,
    /// A label for the run token. Never the token.
    pub token_scope: Option<String>,
}

/// What a teardown should delete beyond the VM itself.
///
/// Both deletions are opt-in, because both destroy something a caller may still want: the
/// image is reusable across runs, and the log group is where a failed build's only evidence
/// lives.
#[napi(object)]
pub struct TeardownOptions {
    pub delete_image: Option<bool>,
    /// **Names** the group in `report.undeleted` rather than deleting it — CloudWatch is not
    /// in the core's dependency set, and reporting a leak beats reporting a clean teardown
    /// over one.
    pub delete_log_group: Option<bool>,
    pub delete_attempts: Option<u32>,
    pub delete_backoff: Option<f64>,
    /// `false` by default: the caller is on the way out, and a teardown that blocked five
    /// minutes on a state nobody reads is five minutes of a CI job. The report then honestly
    /// ends in `"TERMINATING"`.
    pub wait_for_terminated: Option<bool>,
}

/// One MicroVM's whole life.
///
/// The five transitions are `buildImage`, `run`, `suspend`, `resume`, and `terminate`, and
/// every state guard lives in the core — see the module docs.
#[napi]
pub struct Sandbox {
    inner: Arc<Mutex<CoreSandbox>>,
}

#[napi]
impl Sandbox {
    /// Resolves credentials for `region` and returns a sandbox with nothing launched.
    ///
    /// A factory rather than a constructor because credential resolution is async, and a
    /// `#[napi(constructor)]` cannot be. `region` is a [`Region`] instance and not a string,
    /// which is TRAP-6 at this boundary.
    #[napi(factory)]
    pub async fn create(region: &Region) -> Result<Sandbox, AsyncError> {
        let sandbox = CoreSandbox::new(region.inner.clone())
            .await
            .map_err(js_async)?;
        Ok(Sandbox {
            inner: Arc::new(Mutex::new(sandbox)),
        })
    }

    /// The lifecycle state: `"PENDING"`, `"RUNNING"`, `"SUSPENDING"`, `"SUSPENDED"`,
    /// `"TERMINATING"`, or `"TERMINATED"`.
    ///
    /// Spelled as the service spells it, because a reader compares it against a `GetMicrovm`
    /// response and `Suspended` beside `SUSPENDED` reads like two facts.
    #[napi]
    pub async fn lifecycle(&self) -> String {
        self.inner.lock().await.lifecycle().as_str().to_string()
    }

    /// Whether the agent token has been installed (STATE-2).
    ///
    /// Set by the platform reporting RUNNING, not by the launch call: the run hook is what
    /// delivers the token, and a launch that died during startup delivered nothing.
    #[napi]
    pub async fn token_installed(&self) -> bool {
        self.inner.lock().await.token_installed()
    }

    /// Whether an image is recorded as existing (STATE-1).
    #[napi]
    pub async fn image_exists(&self) -> bool {
        self.inner.lock().await.image_exists()
    }

    /// Whether this VM was ever terminated (STATE-11).
    #[napi]
    pub async fn was_terminated(&self) -> bool {
        self.inner.lock().await.was_terminated()
    }

    /// How many times the token has been installed. Never above one (STATE-3).
    #[napi]
    pub async fn bootstrap_count(&self) -> u32 {
        self.inner.lock().await.bootstrap_count()
    }

    /// The VM id, once launched.
    #[napi]
    pub async fn microvm_id(&self) -> Option<String> {
        self.inner.lock().await.microvm().map(|vm| vm.id.clone())
    }

    /// The proxy endpoint, once launched.
    #[napi]
    pub async fn endpoint(&self) -> Option<String> {
        self.inner
            .lock()
            .await
            .microvm()
            .map(|vm| vm.endpoint.clone())
    }

    /// Why the VM is in its current state, when the service said.
    ///
    /// The absence is information: TRAP-8's message distinguishes "no stateReason" from an
    /// empty one.
    #[napi]
    pub async fn state_reason(&self) -> Option<String> {
        self.inner
            .lock()
            .await
            .microvm()
            .and_then(|vm| vm.state_reason.clone())
    }

    /// The image, once built.
    #[napi]
    pub async fn image(&self) -> Option<Image> {
        self.inner.lock().await.image().map(Image::wrap)
    }

    /// The suspended window this sandbox asked for at launch, in seconds.
    ///
    /// `null` before a launch, and for a sandbox that did not send the launch — this client is
    /// the only party that can name the number, because `suspendedDurationSeconds` exists only
    /// in the `RunMicrovm` request.
    #[napi]
    pub async fn suspended_window_seconds_async(&self) -> Option<f64> {
        self.inner
            .lock()
            .await
            .suspended_window()
            .map(|window| window.as_secs_f64())
    }

    /// The session, once launched.
    ///
    /// A new wrapper each call, all reaching the same session under the same lock. There is no
    /// cached instance: caching one would mean a session object that outlives the VM it
    /// addresses, and the indirection exists precisely so a post-terminate call reports the
    /// lifecycle rather than a dangling handle.
    #[napi]
    pub async fn session(&self) -> Option<Session> {
        let guard = self.inner.lock().await;
        guard
            .session()
            .is_some()
            .then(|| Session::in_sandbox(Arc::clone(&self.inner)))
    }

    /// Builds an image and waits for it to become usable.
    ///
    /// Every local guard runs **before** the call, which matters because the create happens
    /// after the caller's artifact upload: a rejection AWS raises costs the upload first.
    #[napi]
    pub async fn build_image(
        &self,
        options: BuildImageOptions,
        size: Option<&SizeClass>,
        run_hook_timeout: Option<&RunHookTimeout>,
        build_hook_timeout: Option<&BuildHookTimeout>,
    ) -> Result<Image, AsyncError> {
        let request = options.into_request(size, run_hook_timeout, build_hook_timeout);
        let mut guard = self.inner.lock().await;
        let image = guard.build_image(request).await.map_err(js_async)?;
        Ok(Image::wrap(image))
    }

    /// The artifact bytes to upload to `codeArtifactUri`.
    ///
    /// The upload is the caller's: S3 is not in the core's dependency set. Takes the same
    /// options as [`Self::build_image`] so the bytes a caller puts in the bucket are the bytes
    /// the build will receive.
    #[napi]
    pub async fn build_artifact(
        &self,
        options: BuildImageOptions,
        size: Option<&SizeClass>,
        run_hook_timeout: Option<&RunHookTimeout>,
        build_hook_timeout: Option<&BuildHookTimeout>,
    ) -> Result<napi::bindgen_prelude::Buffer, AsyncError> {
        let request = options.into_request(size, run_hook_timeout, build_hook_timeout);
        let guard = self.inner.lock().await;
        Ok(guard.build_artifact_for(&request).map_err(js_async)?.into())
    }

    /// Launches a MicroVM, waits for RUNNING, and resolves with its session.
    ///
    /// # What the core refuses here, and this file does not
    ///
    /// A second `run` on one sandbox, with **zero** control-plane calls: the agent token is
    /// installed at most once per VM lifetime (STATE-3), and a second VM needs a second
    /// `Sandbox`. A run with no image at all, before any call. Neither check is in this file.
    #[napi]
    pub async fn run(&self, options: Option<RunOptions>) -> Result<Session, AsyncError> {
        let defaults = RunRequest::new();
        let options = options.unwrap_or(RunOptions {
            image_identifier: None,
            execution_role_arn: None,
            agent_token: None,
            egress: None,
            max_idle_sec: None,
            suspended_sec: None,
            auto_resume: None,
            max_duration_sec: None,
            ready_timeout: None,
            token_scope: None,
        });
        // Every unset field falls back to the core's own default rather than to a number
        // written here: ten-minute idle and suspended windows and a one-hour ceiling are
        // measured figures, and a second copy of them in a binding is a second thing to keep
        // in step.
        let request = RunRequest {
            image_identifier: options.image_identifier,
            execution_role_arn: options.execution_role_arn,
            agent_token: options.agent_token,
            egress: options.egress.unwrap_or(defaults.egress),
            max_idle_sec: options.max_idle_sec.unwrap_or(defaults.max_idle_sec),
            suspended_sec: options.suspended_sec.unwrap_or(defaults.suspended_sec),
            auto_resume: options.auto_resume.unwrap_or(defaults.auto_resume),
            max_duration_sec: options
                .max_duration_sec
                .unwrap_or(defaults.max_duration_sec),
            ready_timeout: match options.ready_timeout {
                Some(timeout) => seconds_async(timeout)?,
                None => defaults.ready_timeout,
            },
            token_scope: options.token_scope,
        };
        // The core answers `&mut Session`, which cannot cross into JS — so the return value is
        // discarded and the session is reached through the sandbox. That is not a workaround:
        // it is what makes a post-terminate session call report the lifecycle instead of
        // addressing a VM that is gone.
        {
            let mut guard = self.inner.lock().await;
            guard.run(request).await.map_err(js_async)?;
        }
        Ok(Session::in_sandbox(Arc::clone(&self.inner)))
    }

    /// Freezes the VM and waits for the platform to report it.
    ///
    /// A freeze and restore rather than a stop and start: the guest keeps its memory, so the
    /// token, the filesystem, and every exec record survive. The one thing that does not is
    /// the guest's view of time — it observes the whole suspension as a single jump, so any
    /// timeout, lease, or TLS session a running command holds expires at once on resume.
    ///
    /// A suspend from anything but RUNNING is refused by the core with zero control-plane
    /// calls (STATE-5). Resolves with the state reached, which may be `"TERMINATED"`: a VM
    /// that dies while suspending is a state to report rather than an error thrown out of the
    /// middle of a teardown.
    #[napi]
    pub async fn suspend(&self) -> Result<String, AsyncError> {
        let mut guard = self.inner.lock().await;
        guard.suspend().await.map_err(js_async)?;
        Ok(guard.lifecycle().as_str().to_string())
    }

    /// Thaws the VM and resolves with a usable session.
    ///
    /// # What the core refuses, before any wire call
    ///
    /// A resume after `terminate` (STATE-11) — a terminated VM never returns to RUNNING, and
    /// even a call the service accepted would hand back a different machine. A resume from
    /// anything but SUSPENDED (STATE-7). And a resume past the launch-time suspended window
    /// (STATE-12), which is the one worth knowing about: the `idlePolicy` terminates a
    /// suspended VM once that window passes, so there is nothing left to resume, and calling
    /// would cost the full poll timeout to learn something worse.
    ///
    /// Nothing is re-delivered: no run-hook payload, no token, no bootstrap. The in-memory
    /// token survived the freeze, and re-delivering it would hit the daemon's one-shot
    /// bootstrap and be refused — a 409 that reads like a broken VM.
    #[napi]
    pub async fn resume(&self) -> Result<Session, AsyncError> {
        {
            let mut guard = self.inner.lock().await;
            guard.resume().await.map_err(js_async)?;
        }
        Ok(Session::in_sandbox(Arc::clone(&self.inner)))
    }

    /// Tears down, best-effort, **never rejecting**.
    ///
    /// Order: VM, then image, then the log group last, because the service can recreate a
    /// group deleted before its image.
    #[napi]
    pub async fn terminate(
        &self,
        options: Option<TeardownOptions>,
    ) -> Result<TeardownReport, AsyncError> {
        // The two retry knobs default to the core's own figures rather than to numbers written
        // here: twenty attempts fifteen seconds apart is the difference between a clean
        // account and a billed leak, and restating them would put a second copy of that
        // measurement in a binding.
        let defaults = TeardownOpts::default();
        let options = options.unwrap_or(TeardownOptions {
            delete_image: None,
            delete_log_group: None,
            delete_attempts: None,
            delete_backoff: None,
            wait_for_terminated: None,
        });
        let mut opts = TeardownOpts {
            delete_image: options.delete_image.unwrap_or(false),
            delete_log_group: options.delete_log_group.unwrap_or(false),
            delete_attempts: options.delete_attempts.unwrap_or(defaults.delete_attempts),
            delete_backoff: match options.delete_backoff {
                Some(backoff) => seconds_async(backoff)?,
                None => defaults.delete_backoff,
            },
            wait_for_terminated: defaults.wait_for_terminated,
        };
        if options.wait_for_terminated.unwrap_or(false) {
            opts = opts.waiting_for_terminated();
        }
        let mut guard = self.inner.lock().await;
        // The core answers a report rather than a `Result`, so the `Ok` here is this wrapper's
        // and never the core's — a teardown cannot reject, which is the whole point.
        Ok(TeardownReport::wrap(guard.terminate(opts).await))
    }
}
