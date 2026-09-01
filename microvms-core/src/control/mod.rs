// SPDX-License-Identifier: Apache-2.0
//! The control-plane client: SigV4-signed rest-json, and every trap closed before the
//! request leaves the process.
//!
//! # The shape of the thing
//!
//! [`ControlPlane`] is the whole surface. It holds a [`transport::Transport`] (signed and
//! real, or a contract recorder in a test), a [`Region`], and a [`Clock`] — and every
//! operation is a method on it. There is no builder, no config struct, and no partially
//! constructed state: [`ControlPlane::new`] resolves credentials and either yields a
//! usable client or says why not.
//!
//! # What is closed here, and how strongly
//!
//! * **TRAP-1** — [`token`]: no `clientToken` parameter exists on any request type. S1.
//! * **TRAP-2** — [`image`]: a build stuck in `CREATING` past the stall grace with every
//!   build `PENDING` is rejected naming the replay signature. S2.
//! * **TRAP-3** — [`CreateImageRequest::repair_guest_identity`] is a `bool`, and the
//!   request injects `["ALL"]` itself. There is no capability list to put
//!   `CAP_SYS_ADMIN` in. S1.
//! * **TRAP-4** — [`connector::ConnectorIntent`]: connectors are derived ARNs from a
//!   closed intent enum. S1.
//! * **TRAP-5** — [`RunHookPayload`]: 4096 bytes inclusive, checked before any call. S2,
//!   and the type means an unchecked string cannot reach the field.
//! * **TRAP-8** — [`ControlPlane::wait_for_running`]: a terminal state before RUNNING is
//!   rejected with the state *and* `stateReason` attached. S2.
//! * **TRAP-11, revised again** — `mint_shell_auth_token` exists now, and the guard
//!   changed ground with it: the closure is no longer the method's absence but the exec
//!   path's separation from it, which is why the test still counts the calls a full
//!   lifecycle made and asserts zero shell operations among them. The connector half is
//!   unchanged: `SHELL_INGRESS` is a [`connector::ConnectorIntent`] variant, and the
//!   lifecycle test asserts a launch carries exactly the connectors its caller asked
//!   for. See [`connector`].
//!
//! # TRAP-11 is the one that needs saying out loud
//!
//! `CreateMicrovmShellAuthToken` is implemented — issue #69 builds `microvm shell` on
//! it, and [`ControlPlane::mint_shell_auth_token`] is its one door. The original reason
//! for staying away entirely — that the shell gates a console-only debugging flow, not
//! a programmatic exec path — did not survive measurement: `docs/PLATFORM.md`
//! (2026-08-15) found a real PTY over a WebSocket, programmatically drivable. What
//! holds is narrower: **one interactive session is not programmatic exec** — no exec
//! ids, no idempotency, no separated stdout/stderr, no exit codes — so the exec path
//! never requests the shell connector and never mints a shell token, and the shell is
//! its own surface ([`crate::session::shell`]) rather than a method the exec path can
//! wander into.
//!
//! # Nothing in this module reads the service model at runtime
//!
//! Every constraint is a constant in [`crate::constants`], checked by the build gate
//! against the pinned model (TRAP-12). That matters because botocore's
//! `VALIDATED_METADATA_ATTRS` is `{required, min, document, union}` — `max`, `pattern`,
//! and `enum` violations go to the wire — so every guard in this module is load-bearing
//! rather than belt-and-braces, and "the SDK validates the model already" was never true.
//!
//! # And "botocore validates `min`" was never true *of this client*
//!
//! The sentence above is about `max`, `pattern`, and `enum`, and for a while it left `min`
//! implicitly exempt: `min` **is** in `VALIDATED_METADATA_ATTRS`, so a `min` violation really is
//! refused before the wire — by botocore. This client does not use botocore. It signs with
//! `aws-sigv4` and sends with `reqwest`, and `validate.py` is nowhere in the dependency graph.
//! That reasoning came from the deleted Python client, where it held, and it did not survive the
//! port; issue #24 measured the consequence, `maxIdleDurationSeconds: 59` on the wire.
//!
//! So the rule is simpler than it was: **every constraint the model states on a member this
//! client sends is enforced by this module or by nothing.** The guards below are the whole of it:
//!
//! * [`require_valid_image_name`] — `ImageName` (1..=64, `[a-zA-Z0-9-_]+`).
//! * [`require_duration_in_range`] — `maximumDurationInSeconds` (1..=28800).
//! * [`require_valid_version`] — `Version` (1..=2048, `[^\s]+`).
//! * [`require_non_blank`] — `NonBlankString`, the model's most-reused shape.
//! * [`require_valid_identifier`] — `MicrovmIdentifier`/`MicrovmImageIdentifier` (1..=256).
//! * [`require_valid_role_arn`] — `RoleArn` (20..=2048, plus the twelve-digit account).
//! * [`require_valid_port`] — `PortNumber`/`HooksPortInteger` (`min: 1`; 0 is not a port).
//! * [`require_idle_duration`] — `maxIdleDurationSeconds` (`min: 60`).
//! * [`require_valid_tags`] — `TagKey`/`TagValue`, two ceilings and two minima.
//!
//! Three constraints are closed by a **type** rather than by a function, which is the stronger
//! form: [`RunHookPayload`] cannot hold over 4096 bytes, [`ops::VersionStatus`] and
//! [`ops::HookState`] cannot spell a value the enum does not have, and
//! [`connector::ConnectorIntent`] cannot name a connector the platform does not publish. A
//! constraint the type system enforces cannot be forgotten at a call site.

pub mod artifact;
pub mod connector;
pub mod image;
pub mod microvm;
pub mod ops;
pub mod token;
pub mod transport;

use std::sync::Arc;
use std::time::Duration;

pub use artifact::{BaseImage, artifact_content_hash, build_artifact, default_dockerfile};
pub use connector::ConnectorIntent;
pub use image::{Image, WaitOpts};
pub use microvm::{Microvm, ProxyToken, RunHookPayload};

use crate::error::{Error, ErrorKind};
use crate::hooks::{BuildHookTimeout, RunHookTimeout};
use crate::region::Region;
use crate::sizing::SizeClass;

/// The default agent port, matching the daemon's own default.
pub const DEFAULT_AGENT_PORT: u16 = 9000;

/// Monotonic time and sleeping, injectable so a wait can be tested without waiting.
///
/// # Why a trait rather than `tokio::time::pause`
///
/// Because the waits here are driven by *elapsed* comparisons against a deadline and a
/// stall grace, and a test has to be able to say "now 300 seconds have passed" between two
/// polls of a fake control plane. A paused tokio clock can do that, but only for code that
/// sleeps on tokio — and it makes every test in the file share one global clock state.
/// A [`Clock`] parameter makes the dependency visible in the signature.
///
/// Monotonic rather than wall-clock, and this is the same reasoning the Python client's
/// injectable `time.monotonic` carries: the suspended window is a *duration*, and a wall
/// clock that steps backward would reopen a closed one.
pub trait Clock: Send + Sync {
    /// Monotonic elapsed time since this clock started. Never decreases.
    fn elapsed(&self) -> Duration;

    /// Sleeps for `duration`.
    fn sleep(
        &self,
        duration: Duration,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + '_>>;
}

/// The real clock: `Instant` and `tokio::time::sleep`.
#[derive(Debug)]
pub struct SystemClock {
    started: std::time::Instant,
}

impl SystemClock {
    pub fn new() -> Self {
        Self {
            started: std::time::Instant::now(),
        }
    }
}

impl Default for SystemClock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock for SystemClock {
    fn elapsed(&self) -> Duration {
        self.started.elapsed()
    }

    fn sleep(
        &self,
        duration: Duration,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + '_>> {
        Box::pin(tokio::time::sleep(duration))
    }
}

/// The control-plane client.
///
/// The transport and clock sit behind `Arc`, so a caller holding one across tasks does not
/// need a second credential chain.
pub struct ControlPlane {
    transport: Arc<dyn transport::Transport>,
    region: Region,
    clock: Arc<dyn Clock>,
    /// The agent port, which the hooks block and the proxy token both need.
    port: u16,
}

impl std::fmt::Debug for ControlPlane {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ControlPlane")
            .field("region", &self.region)
            .field("port", &self.port)
            .finish_non_exhaustive()
    }
}

impl ControlPlane {
    /// Resolves credentials for `region` and returns a usable client.
    ///
    /// The region is a [`Region`], so an unsupported one is either a compile error or a
    /// visible [`Region::unlisted`] at the call site — TRAP-6 is closed by the type before
    /// this function is reached, which is why there is no region check here.
    pub async fn new(region: Region) -> Result<Self, Error> {
        let transport = transport::SignedTransport::new(region.clone()).await?;
        Ok(Self {
            transport: Arc::new(transport),
            region,
            clock: Arc::new(SystemClock::new()),
            port: DEFAULT_AGENT_PORT,
        })
    }

    /// A client over a caller-supplied transport and clock.
    ///
    /// The seam every test uses, and the reason it is public rather than `cfg(test)`: the
    /// state-machine lane (T-W3-6) drives a `ControlPlane` and needs the same seam to test
    /// its own lifecycle without AWS. A caller who passes a real transport here gets
    /// exactly what [`ControlPlane::new`] builds.
    pub fn with_transport(
        transport: Arc<dyn transport::Transport>,
        region: Region,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            transport,
            region,
            clock,
            port: DEFAULT_AGENT_PORT,
        }
    }

    /// Sets the agent port the hooks block and the proxy token name.
    ///
    /// # Why this is fallible where every other builder method is not
    ///
    /// Because `port` is the one value on this type that the model bounds, and 0 is a value a
    /// caller reaches for on purpose. `PortNumber` and `HooksPortInteger` are both `min: 1`;
    /// `u16` closes the ceiling and nothing closed the floor, so before this returned a `Result`
    /// the port set here landed in two places on the wire — `hooks.port` on every
    /// `CreateMicrovmImage` built through the plane, and `allowedPorts: [{"port": 0}]` on every
    /// token it minted (issue #24).
    ///
    /// Both of those are guarded at the call itself as well ([`require_valid_port`] in
    /// [`ControlPlane::create_image`] and in [`ControlPlane::mint_auth_token_for`]), so the
    /// constraint would be closed without this. It is fallible anyway because the two refusals
    /// arrive a long way from the mistake: the create one after an artifact upload, and the mint
    /// one at the moment a session needs a credential. Refusing here means `--port 0` fails
    /// before a control plane is even usable, which is the difference between "that port is not
    /// legal" and "your launch failed".
    ///
    /// Zero rather than the ceiling is what this actually catches, and it is not a typo case: 0
    /// is what "let the kernel choose" means to a listener, so it is the value a caller passes
    /// when they have a socket they have not bound yet.
    pub fn with_port(mut self, port: u16) -> Result<Self, Error> {
        require_valid_port("port", port)?;
        self.port = port;
        Ok(self)
    }

    /// The region every ARN in a request is derived for.
    pub fn region(&self) -> &Region {
        &self.region
    }

    /// The agent port.
    pub fn port(&self) -> u16 {
        self.port
    }

    pub(crate) fn transport(&self) -> &dyn transport::Transport {
        self.transport.as_ref()
    }

    pub(crate) fn clock(&self) -> &dyn Clock {
        self.clock.as_ref()
    }
}

/// Everything `CreateMicrovmImage` needs, with the traps closed in the type.
///
/// # What is deliberately not here
///
/// A `client_token` field. See [`token`] for the fifteen-hour wedge that absence prevents;
/// [`CreateImageRequest::token_scope`] is a **label** folded in beside a fresh nonce and
/// cannot become the token.
///
/// A `capabilities` list. See `repair_guest_identity`.
///
/// An `architecture` field. The model's enum has exactly one value, so the only thing a
/// field could express is a rejected request — and the rejection arrives *after* the
/// artifact upload, reported as a constraint on a field the caller thought was a choice.
/// `ARM_64` is injected. A caller who built an x86 binary finds out from `doctor`'s ELF
/// check, not from a build cycle.
#[derive(Clone, Debug)]
pub struct CreateImageRequest {
    /// The image name. Validated against `ImageName` (1..=64, `[a-zA-Z0-9-_]+`) before the
    /// artifact is uploaded, because AWS's rejection arrives *after* the upload.
    pub name: String,
    /// The base image, pairing the `baseImageArn` with the Dockerfile `FROM`.
    pub base_image: BaseImage,
    /// `baseImageVersion`, or `None` to take whatever the service currently defaults to.
    ///
    /// # Why pinning is worth a field
    ///
    /// Without it a build floats. The managed base's version list is not static — `al2023-1`
    /// carried one version in June and two by July (`"0"` and `"1"`, measured 2026-08-16) — so
    /// two builds of identical inputs weeks apart can sit on different bases, and neither
    /// recorded which. That is a reproducibility hole with no local symptom: the build
    /// succeeds either way, and the difference appears in the guest.
    ///
    /// `None` is the default and stays the default, because pinning without knowing the legal
    /// values is how a caller pins to a version that has been withdrawn.
    /// [`ControlPlane::managed_base_versions`] is what answers that, and the version strings
    /// are **bare integers** for a managed base where a custom image's are `"1.0"` — the two
    /// are not comparable, so a value from anywhere else does not belong here.
    ///
    /// Checked against the `Version` shape before the call, because the create happens after
    /// the artifact upload.
    pub base_image_version: Option<String>,
    /// The Dockerfile, or `None` for the derived default.
    ///
    /// A caller-supplied one is checked against `base_image`'s `docker_ref`: the two
    /// disagreeing builds against a base none of the measured platform behaviour applies
    /// to.
    pub dockerfile: Option<String>,
    /// The daemon binary's bytes, zipped into the artifact.
    pub binary: Vec<u8>,
    /// Where the artifact is uploaded to. This client does not upload — S3 is not in the
    /// crate's dependency set — so the caller puts the bytes there and passes the URI.
    pub code_artifact_uri: String,
    /// The build role, which must grant logs on `/aws/lambda-microvms/*`.
    pub build_role_arn: String,
    /// The size class. Selects a documented baseline; it does not size the VM (TRAP-10).
    pub size: SizeClass,
    /// Whether to repair guest identity (TRAP-3).
    ///
    /// # Why a bool and not a capability list
    ///
    /// Measured 2026-08-06: without the capability, a guest running as root still gets
    /// `EPERM` from `sethostname` and from a bind mount over
    /// `/proc/sys/kernel/random/boot_id`, because the MicroVM drops `CAP_SYS_ADMIN` by
    /// default. Writing `/etc/machine-id` needs no capability and succeeds either way,
    /// which is what makes the gap easy to miss — identity repair looks like it works
    /// until you check the two steps that need the kernel's permission rather than the
    /// filesystem's.
    ///
    /// `Capability`'s enum is exactly `["ALL"]`, so there is no way to ask for
    /// `CAP_SYS_ADMIN` alone. A list parameter would let a caller write
    /// `["CAP_SYS_ADMIN"]` — the request AWS rejects *after* the artifact upload — so the
    /// intent is a boolean and the request injects `["ALL"]` itself.
    pub repair_guest_identity: bool,
    /// Whether the daemon should inherit the image's `WORKDIR`.
    ///
    /// Rejected when nothing declares one: most public ARM64 bases leave it empty, so the
    /// inheritance silently resolves to `/` and the symptom appears in the guest a build
    /// cycle later.
    pub inherit_workdir: bool,
    /// The run-family hook timeout: `run`, `resume`, `suspend`, `terminate`. Caps at 60s,
    /// and the type is what keeps a build-sized value out of it.
    pub run_hook_timeout: RunHookTimeout,
    /// The build-family hook timeout: `ready`, `validate`. Caps at 3600s.
    pub build_hook_timeout: BuildHookTimeout,
    /// Tags, or none.
    pub tags: std::collections::BTreeMap<String, String>,
    /// `logging.cloudWatch.logGroup`, or `None` for the service default — a
    /// service-created group under `/aws/lambda-microvms/<image-name>` with random stream
    /// names.
    ///
    /// Checked against `CloudWatchLoggingLogGroupString` (1..=512, `[a-zA-Z0-9_\-/.#]+`)
    /// before the artifact upload. The build role must be able to write to whatever this
    /// names: the conformance account's role grants logs only on
    /// `/aws/lambda-microvms/*`, so a group outside that prefix builds with no logs at
    /// all — the same failure mode as the wrong-prefix policy (docs/PLATFORM.md).
    pub log_group: Option<String>,
    /// `logging.cloudWatch.logStream` — a stream-name **prefix**, never the exact name.
    ///
    /// # The client always appends a per-build discriminator
    ///
    /// The wire member is an EXACT stream name (prefixes unsupported — measured 2026-08,
    /// docs/PLATFORM.md 'An image build is three VMs and three log streams'), and one
    /// build emits three streams. A fixed configured name collapses all three of every
    /// build into one stream, so [`ControlPlane::create_image`] sends
    /// `<this>/<16 hex>` with fresh CSPRNG per create attempt — the same mechanism as
    /// the `clientToken` nonce (TRAP-1) — and the resolved name comes back on
    /// [`image::Image::log_stream`] so the caller can find their logs.
    ///
    /// Capped at [`crate::constants::MAX_USER_LOG_STREAM_LEN`] (495) so the resolved name
    /// fits the shape's 512, and refused when it carries `:` or `*` (the pattern is
    /// `[^:*]*`). Requires [`Self::log_group`]: a stream inside a group the service
    /// chose randomly is a stream nobody can predict the location of.
    pub log_stream: Option<String>,
    /// A **label** folded into the create token for CloudTrail readability, defaulting to
    /// the image name. Not the token, and cannot become it (TRAP-1).
    pub token_scope: Option<String>,
}

impl CreateImageRequest {
    /// A request with the measured defaults: the al2023 base, the default size class, the
    /// derived Dockerfile, identity repair off, and 30-second hook timeouts.
    ///
    /// The hook timeouts are `expect`ed rather than propagated because 30 is legal for both
    /// families by construction — it is below the run family's 60s ceiling, which is the
    /// lower of the two — and a `Result` here would put a `?` on every call site for a
    /// value this function chose itself.
    pub fn new(
        name: impl Into<String>,
        binary: Vec<u8>,
        code_artifact_uri: impl Into<String>,
        build_role_arn: impl Into<String>,
    ) -> Self {
        Self {
            name: name.into(),
            base_image: BaseImage::al2023(),
            base_image_version: None,
            dockerfile: None,
            binary,
            code_artifact_uri: code_artifact_uri.into(),
            build_role_arn: build_role_arn.into(),
            size: SizeClass::DEFAULT,
            repair_guest_identity: false,
            inherit_workdir: false,
            run_hook_timeout: RunHookTimeout::try_new(30).expect("30s is under the 60s ceiling"),
            build_hook_timeout: BuildHookTimeout::try_new(30)
                .expect("30s is under the 3600s ceiling"),
            tags: std::collections::BTreeMap::new(),
            log_group: None,
            log_stream: None,
            token_scope: None,
        }
    }
}

/// Everything `RunMicrovm` needs.
///
/// `run_hook_payload` is a [`RunHookPayload`], which cannot hold an over-ceiling value —
/// so TRAP-5 is closed before this struct exists rather than checked when it is used.
#[derive(Clone, Debug)]
pub struct RunMicrovmRequest {
    /// The image ARN or ID to launch.
    pub image_identifier: String,
    /// `imageVersion`, or `None` for the image's own latest active version.
    ///
    /// # The launch half of blue/green
    ///
    /// `None` is what this client always sent, and it means "whatever
    /// `latestActiveImageVersion` is now" — which is right for the ordinary case and wrong for
    /// the two that matter. A canary wants to launch against exactly the version it is
    /// testing, not against whatever became latest while it was starting. And a rollback wants
    /// to re-pin to the known-good version, which "latest" cannot express at all once a bad
    /// version is the latest one.
    ///
    /// With [`ControlPlane::set_image_version_status`] the three moves compose: build v2,
    /// canary-launch pinned to v2, and on failure set v2 INACTIVE and re-pin to v1. A version
    /// set INACTIVE refuses to launch when pinned here, which is what makes the retire real
    /// rather than advisory.
    ///
    /// Checked against the `Version` shape before the call — see
    /// [`require_valid_version`] on why a rollback is the worst moment for a rejection about
    /// the request.
    pub image_version: Option<String>,
    /// The execution role. Optional in the model; every real launch needs one.
    pub execution_role_arn: Option<String>,
    /// The connectors to request, as intents (TRAP-4). Ingress is required for a session
    /// to work at all; omitting egress is how you get no outbound network.
    pub connectors: Vec<ConnectorIntent>,
    /// The already-validated payload carrying the agent token.
    pub run_hook_payload: RunHookPayload,
    /// `maximumDurationInSeconds`, checked against 1..=28800 when the request is sent.
    pub max_duration_sec: u32,
    /// `idlePolicy.maxIdleDurationSeconds`, checked against the model's `min: 60` before the call.
    ///
    /// This comment used to say the guard was deliberately absent, because `min` is one of the four
    /// keys botocore's `VALIDATED_METADATA_ATTRS` enforces locally. True of botocore, and this
    /// client never touches it — see [`require_idle_duration`]. `59` reached the wire.
    pub max_idle_sec: u32,
    /// `idlePolicy.suspendedDurationSeconds`.
    ///
    /// The model's `min` is **0**, so every `u32` satisfies it and there is no guard — an absence
    /// with a reason rather than an oversight, and the drift gate pins the 0 so a future floor is
    /// noticed. Its ceiling is unstated.
    ///
    /// This value exists **only in the request** — the client is the only party that can
    /// name the window it asked for, which is what STATE-12's refusal rests on.
    pub suspended_sec: u32,
    /// `idlePolicy.autoResumeEnabled`.
    pub auto_resume: bool,
    /// A label for the run token, defaulting to the image identifier (TRAP-1).
    pub token_scope: Option<String>,
}

impl RunMicrovmRequest {
    /// A launch with the measured defaults: ingress only, ten-minute idle and suspended
    /// windows, a one-hour maximum duration, no auto-resume.
    pub fn new(image_identifier: impl Into<String>, run_hook_payload: RunHookPayload) -> Self {
        Self {
            image_identifier: image_identifier.into(),
            image_version: None,
            execution_role_arn: None,
            connectors: vec![ConnectorIntent::AllIngress],
            run_hook_payload,
            max_duration_sec: 3_600,
            max_idle_sec: 600,
            suspended_sec: 600,
            auto_resume: false,
            token_scope: None,
        }
    }

    /// Adds the egress connector, which is what gives the VM outbound network.
    #[must_use]
    pub fn with_egress(mut self) -> Self {
        if !self.connectors.contains(&ConnectorIntent::Egress) {
            self.connectors.push(ConnectorIntent::Egress);
        }
        self
    }

    /// Requests a shell-capable launch: the ingress set becomes the measured pair,
    /// `[HTTP_INGRESS, SHELL_INGRESS]`.
    ///
    /// **Replaces** `ALL_INGRESS` rather than adding beside it, because the platform
    /// forbids the combination and says so only at token-mint time — the VM launches,
    /// reaches RUNNING, and bills before the failure appears (`docs/PLATFORM.md`,
    /// 2026-08-15). `[HTTP_INGRESS, SHELL_INGRESS]` is the set measured to both launch
    /// and mint a shell token, and `HTTP_INGRESS` stays in the pair so the daemon
    /// endpoint keeps working. Egress is untouched: shell access and outbound network
    /// are separate questions.
    #[must_use]
    pub fn with_shell(mut self) -> Self {
        self.connectors
            .retain(|intent| *intent != ConnectorIntent::AllIngress);
        for wanted in [ConnectorIntent::HttpIngress, ConnectorIntent::ShellIngress] {
            if !self.connectors.contains(&wanted) {
                self.connectors.push(wanted);
            }
        }
        self
    }

    /// Pins the launch to one `imageVersion`. See the field for why that matters.
    #[must_use]
    pub fn with_image_version(mut self, version: impl Into<String>) -> Self {
        self.image_version = Some(version.into());
        self
    }
}

/// Rejects a `maximumDurationInSeconds` outside the service range.
///
/// 28800 is eight hours and the hard ceiling on any single VM's life. A longer session
/// needs a second VM, not a larger number — which is what the message says, because
/// "outside the accepted range" alone invites someone to look for the flag that raises it.
pub fn require_duration_in_range(seconds: u32) -> Result<u32, Error> {
    if (1..=crate::constants::MAX_DURATION_SEC).contains(&seconds) {
        return Ok(seconds);
    }
    Err(Error::invalid_arg(format!(
        "maximumDurationInSeconds={seconds} is outside the accepted range 1..{} (service model \
         {}) — {} seconds is eight hours, the hard ceiling on any one VM's life. A longer \
         session needs a second VM, not a larger number.",
        crate::constants::MAX_DURATION_SEC,
        crate::constants::MODEL_API_VERSION,
        crate::constants::MAX_DURATION_SEC,
    )))
}

/// Rejects a `Version`/`NonBlankString` value the service would reject.
///
/// # Why this one is checked locally when so many are not
///
/// The model's `Version` shape is `min: 1, max: 2048, pattern: [^\s]+`, and issue #24 lists
/// `NonBlankString` as its most-reused unguarded shape. Two of the three constraints are ones
/// botocore would not catch even if this client used it (`max` and `pattern` are outside
/// `VALIDATED_METADATA_ATTRS`), and this client uses `aws-sigv4` plus `reqwest` and never
/// touches botocore's validator at all.
///
/// What makes it worth a guard rather than a comment is *where* the two callers sit.
/// `CreateMicrovmImage.baseImageVersion` is sent **after** the artifact upload, so the
/// service's rejection costs the caller the upload — the same argument
/// [`require_valid_image_name`] makes. And `RunMicrovm.imageVersion` is the pinned-launch
/// half of a rollback: a blank there is a `ValidationException` at the moment someone is
/// trying to re-pin away from a bad version, which is the worst possible time for the failure
/// to be about the request rather than about the version.
///
/// The pattern is `[^\s]+` — **no whitespace anywhere**, not merely "not blank". A version
/// copied out of a terminal with a trailing newline satisfies "non-empty" and fails the
/// pattern, so the message names the character rather than saying the value is invalid.
pub fn require_valid_version(member: &str, version: &str) -> Result<(), Error> {
    let model = crate::constants::MODEL_API_VERSION;
    if version.is_empty() {
        return Err(Error::invalid_arg(format!(
            "{member} is empty, but the Version shape requires at least 1 character (service \
             model {model}). Omit the member entirely to let the service choose — an absent \
             version and a blank one are different requests, and only the first is legal."
        )));
    }
    if version.len() > crate::constants::MAX_VERSION_LEN {
        return Err(Error::invalid_arg(format!(
            "{member} is {} characters, over the Version ceiling of {} (service model {model}).",
            version.len(),
            crate::constants::MAX_VERSION_LEN,
        )));
    }
    if let Some(found) = version.chars().find(char::is_ascii_whitespace) {
        return Err(Error::invalid_arg(format!(
            "{member} {version:?} contains whitespace ({found:?}), which the Version pattern \
             {:?} forbids anywhere in the value (service model {model}). A version pasted from \
             a terminal carries a trailing newline and looks fine; this is that.",
            crate::constants::VERSION_PATTERN,
        )));
    }
    Ok(())
}

/// Rejects a `NonBlankString` value the service would reject.
///
/// # The model's most-reused shape, and the three members this client sends
///
/// `NonBlankString` is `min: 1, max: 2048, pattern: [^\s]+` and 45 members name it. Most are
/// responses; the three this client puts on the wire are `CodeArtifact.uri`,
/// `CreateMicrovmImage.baseImageArn`, and `ListMicrovmImages.nameFilter`, and issue #24 named
/// all three as reachable with no guard.
///
/// Each of the three fails in its own expensive way. `codeArtifact.uri` and `baseImageArn` ride
/// on the create call, which happens **after** the artifact upload — the ordering
/// [`ControlPlane::create_image`] is arranged around, so a rejection about either costs the
/// caller the upload. `nameFilter` is worse in a quieter way: it goes in the **query string**,
/// so a blank one is a `nameFilter=` that either 400s or silently filters differently from what
/// was meant, and [`ControlPlane::find_image_by_name`] is the resolver a launch depends on.
///
/// # `[^\s]+` forbids whitespace *anywhere*
///
/// Not merely "not blank". An S3 URI pasted from a console with a trailing newline satisfies
/// "non-empty" and fails the pattern, which is why the message names the character it found
/// rather than saying the value is invalid. The same reading [`require_valid_version`] documents
/// for `Version`, which is the identical constraint triple on a different shape.
///
/// A separate function from [`require_valid_version`] even though the two shapes agree today,
/// for the reason [`crate::constants::MAX_NON_BLANK_LEN`] gives: they are two shapes, AWS can
/// move one, and one guard serving both would silently stop being about whichever moved.
pub fn require_non_blank(member: &str, value: &str) -> Result<(), Error> {
    let model = crate::constants::MODEL_API_VERSION;
    if value.is_empty() {
        return Err(Error::invalid_arg(format!(
            "{member} is empty, but the NonBlankString shape requires at least 1 character \
             (service model {model}). The shape's own documentation reads 'a string which is not \
             empty or blank (only whitespace)'."
        )));
    }
    if value.len() > crate::constants::MAX_NON_BLANK_LEN {
        return Err(Error::invalid_arg(format!(
            "{member} is {} characters, over the NonBlankString ceiling of {} (service model \
             {model}).",
            value.len(),
            crate::constants::MAX_NON_BLANK_LEN,
        )));
    }
    if let Some(found) = value.chars().find(char::is_ascii_whitespace) {
        return Err(Error::invalid_arg(format!(
            "{member} {value:?} contains whitespace ({found:?}), which the NonBlankString \
             pattern {:?} forbids anywhere in the value (service model {model}). A URI pasted \
             from a console carries a trailing newline and looks fine; this is that.",
            crate::constants::NON_BLANK_PATTERN,
        )));
    }
    Ok(())
}

/// Rejects a `MicrovmIdentifier`/`MicrovmImageIdentifier` the service would reject.
///
/// # Twelve members across every implemented operation
///
/// Both shapes are `min: 1, max: 256`, and between them they bound the identifier on
/// `GetMicrovm`, `SuspendMicrovm`, `ResumeMicrovm`, `TerminateMicrovm`,
/// `CreateMicrovmAuthToken`, `GetMicrovmImage`, `DeleteMicrovmImage`,
/// `DeleteMicrovmImageVersion`, `GetMicrovmImageVersion`, `UpdateMicrovmImageVersion`,
/// `GetMicrovmImageBuild`, the three listings, and `RunMicrovm.imageIdentifier`. Issue #24
/// counted six implemented operations for each shape; every one of them is guarded here.
///
/// # An empty identifier is the case that pays for this
///
/// Ten of those members are **URI parameters**, and an empty one does not produce a validation
/// error about a blank field: it collapses the path. `GET /2025-09-09/microvms/` addresses the
/// *listing*, not a VM, so an empty id sent to `get_microvm` asks a different question and can
/// get a 200 back for it — and `DELETE` on a collapsed path is worse. That failure mode is
/// invisible in the service's answer, which is the strongest argument for a local refusal
/// anywhere in this module.
///
/// # The model contradicts itself, and this is the side that can be guarded
///
/// `MicrovmImageArn` permits 2048 and is what the service *answers* with, so the model allows a
/// legal response value that is an illegal request value — see
/// [`crate::constants::MAX_IDENTIFIER_LEN`] for the whole account. The message says so, because
/// a caller who hit it by echoing an ARN back is looking at a service inconsistency rather than
/// at their own mistake, and telling them "shorten it" would be useless advice about a value
/// they did not choose.
pub fn require_valid_identifier(member: &str, identifier: &str) -> Result<(), Error> {
    let model = crate::constants::MODEL_API_VERSION;
    if identifier.is_empty() {
        return Err(Error::invalid_arg(format!(
            "{member} is empty, but the identifier shapes require at least 1 character (service \
             model {model}). An empty identifier is not refused as a blank field — most of these \
             are URI parameters, so it collapses the path onto the collection: \
             `/microvms/` is the listing rather than one VM, and a request that addresses the \
             wrong resource can succeed."
        )));
    }
    if identifier.len() > crate::constants::MAX_IDENTIFIER_LEN {
        return Err(Error::invalid_arg(format!(
            "{member} is {} characters, over the {} the MicrovmIdentifier and \
             MicrovmImageIdentifier shapes allow (service model {model}). Note the model \
             disagrees with itself here: MicrovmImageArn permits {}, and that is the shape the \
             service answers with on GetMicrovm's imageArn — so if this value came back from a \
             response rather than being chosen here, it is a service-side inconsistency worth \
             reporting rather than a length to shorten.",
            identifier.len(),
            crate::constants::MAX_IDENTIFIER_LEN,
            crate::constants::MAX_IMAGE_ARN_LEN,
        )));
    }
    Ok(())
}

/// Rejects a `RoleArn` the service would reject, before the artifact upload.
///
/// # Why the build role in particular
///
/// `CreateMicrovmImage.buildRoleArn` is sent on the create call, and the create call happens
/// **after** the caller has uploaded the artifact. So the service's rejection of a malformed
/// role ARN arrives having already cost the upload, which is the exact ordering
/// [`ControlPlane::create_image`] exists to protect against and the reason issue #24 singled
/// this member out. `RunMicrovm.executionRoleArn` is cheaper to get wrong but wrong in a
/// confusing place: it is optional in the model, so a malformed one is a `ValidationException`
/// on a member a caller may not know is being filled from their infra config.
///
/// # Three constraints, three messages
///
/// `min: 20`, `max: 2048`, and the pattern. They get separate messages because the pattern's
/// wording ("this does not look like an IAM role ARN") is unhelpful for a value that is merely
/// 3000 characters, and the length wording is unhelpful for a role *name* — which is the most
/// common mistake and is short.
///
/// See [`crate::constants::is_valid_role_arn`] for what the structural check decides. It cannot
/// tell you the role exists or grants the right log permissions; those are the failures that
/// actually happen most and none of them is knowable without IAM.
pub fn require_valid_role_arn(member: &str, arn: &str) -> Result<(), Error> {
    let model = crate::constants::MODEL_API_VERSION;
    if arn.len() < crate::constants::MIN_ROLE_ARN_LEN {
        return Err(Error::invalid_arg(format!(
            "{member} {arn:?} is {} characters, under the RoleArn minimum of {} (service model \
             {model}). A value this short is almost always a role *name* where the ARN was \
             wanted — `arn:aws:iam::<12-digit-account>:role/{arn}` is the shape.",
            arn.len(),
            crate::constants::MIN_ROLE_ARN_LEN,
        )));
    }
    if arn.len() > crate::constants::MAX_ROLE_ARN_LEN {
        return Err(Error::invalid_arg(format!(
            "{member} is {} characters, over the RoleArn ceiling of {} (service model {model}).",
            arn.len(),
            crate::constants::MAX_ROLE_ARN_LEN,
        )));
    }
    if !crate::constants::is_valid_role_arn(arn) {
        return Err(Error::invalid_arg(format!(
            "{member} {arn:?} does not match the RoleArn pattern {:?} (service model {model}). \
             It wants `arn:aws:iam::` then **exactly twelve digits** of account id then `:role/` \
             and a name — so the three things this catches are a role name passed as an ARN, an \
             ARN for some other service, and an account id with a digit dropped. Rejected here \
             rather than by AWS because for buildRoleArn the create call happens *after* the \
             artifact upload, so the service's answer costs you the upload first. Whether the \
             role exists and grants logs on {}/* is not checkable locally and is still the \
             service's answer to give.",
            crate::constants::ROLE_ARN_PATTERN,
            image::BUILD_LOG_GROUP_PREFIX,
        )));
    }
    Ok(())
}

/// Rejects a `logging.cloudWatch.logGroup` the service would reject, before the artifact
/// upload.
///
/// The shape is `CloudWatchLoggingLogGroupString`: 1..=512, pattern `[a-zA-Z0-9_\-/.#]+`.
/// The character the pattern excludes that a caller writes first is the colon — a group
/// pasted as an ARN (`arn:aws:logs:...`) fails here with the pattern named, rather than as
/// a `ValidationException` after the upload.
pub fn require_valid_log_group(group: &str) -> Result<(), Error> {
    let model = crate::constants::MODEL_API_VERSION;
    if group.is_empty() {
        return Err(Error::invalid_arg(format!(
            "logging.cloudWatch.logGroup is empty, but the CloudWatchLoggingLogGroupString \
             shape requires at least 1 character (service model {model}). Omit the setting \
             entirely to take the service default — a service-created group under \
             {}/<image-name>.",
            image::BUILD_LOG_GROUP_PREFIX,
        )));
    }
    if group.len() > crate::constants::MAX_LOG_GROUP_LEN {
        return Err(Error::invalid_arg(format!(
            "logging.cloudWatch.logGroup is {} characters, over the \
             CloudWatchLoggingLogGroupString ceiling of {} (service model {model}).",
            group.len(),
            crate::constants::MAX_LOG_GROUP_LEN,
        )));
    }
    if !crate::constants::is_valid_log_group(group) {
        return Err(Error::invalid_arg(format!(
            "logging.cloudWatch.logGroup {group:?} does not match the \
             CloudWatchLoggingLogGroupString pattern {:?} (service model {model}) — letters, \
             digits, and `_ - / . #` only. A colon usually means an ARN was pasted where the \
             group *name* was wanted. Rejected here rather than by AWS because the create call \
             happens after the artifact upload, so the service's answer costs you the upload \
             first.",
            crate::constants::LOG_GROUP_PATTERN,
        )));
    }
    Ok(())
}

/// Rejects a caller-supplied `logging.cloudWatch.logStream` prefix the resolved name could
/// not legally carry, before the artifact upload.
///
/// The shape is `CloudWatchLoggingLogStreamString`: 1..=512, pattern `[^:*]*` — anything
/// but `:` and `*`. The caller's value is capped at
/// [`crate::constants::MAX_USER_LOG_STREAM_LEN`] (495) rather than 512, because this
/// client always appends `/<16 hex>` of per-build discriminator before the value reaches
/// the wire — see [`CreateImageRequest::log_stream`] for why a verbatim stream name is
/// never sent — and 495 + 17 is exactly the shape's ceiling.
pub fn require_valid_log_stream(stream: &str) -> Result<(), Error> {
    let model = crate::constants::MODEL_API_VERSION;
    if stream.is_empty() {
        return Err(Error::invalid_arg(format!(
            "logging.cloudWatch.logStream is empty, but the CloudWatchLoggingLogStreamString \
             shape requires at least 1 character (service model {model}). Omit the setting to \
             let the service name the streams inside the configured group."
        )));
    }
    if stream.len() > crate::constants::MAX_USER_LOG_STREAM_LEN {
        return Err(Error::invalid_arg(format!(
            "logging.cloudWatch.logStream is {} characters, over this client's ceiling of {}. \
             The shape's own ceiling is {} (service model {model}), and the client always \
             appends a 17-character per-build discriminator (`/` + 16 hex) — one image build \
             is three VMs writing three streams, and the member is an exact stream name, so a \
             fixed configured name would collapse every build's streams into one. The \
             discriminator is what keeps them tellable apart, and it needs its room.",
            stream.len(),
            crate::constants::MAX_USER_LOG_STREAM_LEN,
            crate::constants::MAX_LOG_STREAM_LEN,
        )));
    }
    if let Some(found) = stream.chars().find(|c| matches!(c, ':' | '*')) {
        return Err(Error::invalid_arg(format!(
            "logging.cloudWatch.logStream {stream:?} contains {found:?}, which the \
             CloudWatchLoggingLogStreamString pattern {:?} forbids anywhere in the value \
             (service model {model}). A `*` usually means a prefix or glob was intended — the \
             member is an exact stream name, prefixes are unsupported, and this client's own \
             per-build suffix is how a configured name behaves like one.",
            crate::constants::LOG_STREAM_PATTERN,
        )));
    }
    Ok(())
}

/// Rejects a port the `PortNumber` or `HooksPortInteger` shapes would reject. **Minimum 1.**
///
/// # Port 0 is representable and means something else
///
/// Zero is what "let the kernel pick a port" means to a listener, so it is the value a caller
/// reaches for when they do not have a port yet — and issue #24 measured that
/// [`ControlPlane::with_port`]`(0)` was representable and produced `allowedPorts: [{"port": 0}]`
/// on the wire, plus a `hooks.port` of 0 on every image built through that plane. Neither is a
/// port the platform can forward to.
///
/// # There is no ceiling check here, and that is the type's doing rather than an omission
///
/// `PortNumber.max` and `HooksPortInteger.max` are both 65535, which is `u16::MAX` — so a `max`
/// branch against the constant would be `port > u16::MAX`, a comparison that is false for every
/// input. Clippy's `absurd_extreme_comparisons` says so, and it is right for the reason
/// [`ControlPlane::create_image`] gives about the absent `hooks.port` check: a branch no input can
/// reach reads as protection, appears in a coverage report as protection, and no test can make it
/// fire.
///
/// The ceiling is still *pinned* — [`crate::constants::MAX_PORT`] is in the drift gate, and
/// `the_port_floor_is_one_and_the_ceiling_is_what_a_u16_holds` asserts the equality with
/// `u16::MAX` that makes this reasoning valid. If AWS ever lowers the ceiling below 65535 the gate
/// goes red, that test goes red, and a real branch belongs here.
pub fn require_valid_port(member: &str, port: u16) -> Result<(), Error> {
    if port >= crate::constants::MIN_PORT {
        return Ok(());
    }
    Err(Error::invalid_arg(format!(
        "{member} is {port}, under the PortNumber minimum of {} (service model {}). Port 0 means \
         'let the kernel choose' to a listener and is not a port the platform can forward to — a \
         proxy token minted for it authorizes nothing, and an image whose hooks block names it has \
         no reachable hook endpoint. Name the port the daemon actually listens on \
         ({DEFAULT_AGENT_PORT} by default).",
        crate::constants::MIN_PORT,
        crate::constants::MODEL_API_VERSION,
    )))
}

/// Rejects an `idlePolicy.maxIdleDurationSeconds` under the model's `min: 60`.
///
/// # The exemption this replaces was inherited from a client that no longer exists
///
/// [`RunMicrovmRequest::max_idle_sec`] and `constants.rs` both used to say there was
/// deliberately no guard here, because `min` is one of the four keys in botocore's
/// `VALIDATED_METADATA_ATTRS` and botocore therefore refuses it locally with a clear message.
/// The premise is true and the conclusion does not apply: this client never touches botocore.
/// `ControlPlane` signs with `aws-sigv4` and sends with `reqwest`, so `validate.py` is not on
/// the path, and issue #24 measured `max_idle_sec: 59` reaching the wire.
///
/// It is the one place in this module where the *reason* for having no guard was wrong rather
/// than the number, which is worth saying because the same reasoning would exempt every `min` in
/// the model — `Version`'s, `NonBlankString`'s, the identifiers', `allowedPorts`'s. All of them
/// are now guarded, and none of them by botocore.
///
/// No maximum, because the model states none. The bound that actually ends a VM's life is
/// `maximumDurationInSeconds` ([`require_duration_in_range`]).
pub fn require_idle_duration(seconds: u32) -> Result<(), Error> {
    if seconds >= crate::constants::MIN_IDLE_DURATION_SEC {
        return Ok(());
    }
    Err(Error::invalid_arg(format!(
        "idlePolicy.maxIdleDurationSeconds={seconds} is under the minimum of {} (service model \
         {}). This used to be documented as a constraint botocore enforced locally, which was \
         true of the deleted Python client and never of this one — it signs with aws-sigv4 and \
         sends with reqwest, so nothing validates a request before the wire except this crate.",
        crate::constants::MIN_IDLE_DURATION_SEC,
        crate::constants::MODEL_API_VERSION,
    )))
}

/// Rejects a tag map the service would reject, naming the key and the half that failed.
///
/// # Sent since tags existed, checked by nothing
///
/// [`CreateImageRequest::tags`] goes straight onto `CreateMicrovmImage.tags` and was never
/// validated (issue #24). Same ordering argument as everything else on that call: the rejection
/// lands after the artifact upload.
///
/// # The key and the value have different rules, so the message says which
///
/// `TagKey` is `min: 1, max: 128`; `TagValue` is `min: 0, max: 256`. So an empty **value** is
/// legal and an empty **key** is not, and the ceilings differ by 2x. They share one pattern.
/// Every message here names the offending key, because a caller applying a dozen tags from a
/// config file needs to know which one — a message that only said "a tag is invalid" would send
/// them to read all twelve.
///
/// The key is quoted in the message even when the key itself is the problem, which is the
/// deliberate choice: a key with a stray newline or a zero-width character in it is
/// indistinguishable from a good one until it is printed with `{:?}`.
pub fn require_valid_tags(tags: &std::collections::BTreeMap<String, String>) -> Result<(), Error> {
    let model = crate::constants::MODEL_API_VERSION;
    for (key, value) in tags {
        if key.is_empty() {
            return Err(Error::invalid_arg(format!(
                "a tag key is empty, but TagKey requires at least 1 character (service model \
                 {model}). Note TagValue's minimum is 0, so an empty tag *value* is legal and an \
                 empty key is not."
            )));
        }
        if key.len() > crate::constants::MAX_TAG_KEY_LEN {
            return Err(Error::invalid_arg(format!(
                "the tag key {key:?} is {} characters, over the TagKey ceiling of {} (service \
                 model {model}). TagValue allows {}, so the two halves are not \
                 interchangeable.",
                key.len(),
                crate::constants::MAX_TAG_KEY_LEN,
                crate::constants::MAX_TAG_VALUE_LEN,
            )));
        }
        if !crate::constants::is_valid_tag_component(key) {
            return Err(Error::invalid_arg(format!(
                "the tag key {key:?} has a character outside the TagKey pattern {:?} (service \
                 model {model}). Letters, digits, separators, and `_ . : / = + - @` — spaces and \
                 non-Latin scripts are fine, commas and `#` and `%` are not, and neither is a \
                 newline (it is a control character, not a separator).",
                crate::constants::TAG_COMPONENT_PATTERN,
            )));
        }
        if value.len() > crate::constants::MAX_TAG_VALUE_LEN {
            return Err(Error::invalid_arg(format!(
                "the value of tag {key:?} is {} characters, over the TagValue ceiling of {} \
                 (service model {model}).",
                value.len(),
                crate::constants::MAX_TAG_VALUE_LEN,
            )));
        }
        if !crate::constants::is_valid_tag_component(value) {
            return Err(Error::invalid_arg(format!(
                "the value of tag {key:?} has a character outside the TagValue pattern {:?} \
                 (service model {model}). It is the same character set the key takes; the value \
                 differs only in allowing an empty string.",
                crate::constants::TAG_COMPONENT_PATTERN,
            )));
        }
    }
    Ok(())
}

/// Rejects an image name the service would reject, before the artifact upload.
///
/// The three cases get their own messages because the pattern message ("no dots, no
/// slashes") is actively misleading for a 70-character name that contains neither.
pub fn require_valid_image_name(name: &str) -> Result<(), Error> {
    let model = crate::constants::MODEL_API_VERSION;
    if name.is_empty() {
        return Err(Error::invalid_arg(format!(
            "the image name is empty, but ImageName requires at least 1 character (service \
             model {model})."
        )));
    }
    if name.len() > crate::constants::MAX_IMAGE_NAME_LEN {
        return Err(Error::invalid_arg(format!(
            "the image name is {} characters, over the ImageName ceiling of {} (service model \
             {model}). Rejected here rather than by AWS, because the create call happens \
             *after* the artifact upload — so the service's answer costs you the upload first.",
            name.len(),
            crate::constants::MAX_IMAGE_NAME_LEN,
        )));
    }
    if !crate::constants::is_valid_image_name(name) {
        return Err(Error::invalid_arg(format!(
            "the image name {name:?} does not match the ImageName pattern {:?} (service model \
             {model}). Letters, digits, hyphen, and underscore only — no dots and no slashes, \
             which are the two separators a namespaced name reaches for first.",
            crate::constants::IMAGE_NAME_PATTERN,
        )));
    }
    Ok(())
}

/// The `Hooks` block with all six hooks enabled and both timeout families set.
///
/// All six because `ready` and `validate` are image-*build* hooks: the build calls them to
/// decide whether the snapshot it just produced is usable, before any instance exists and
/// therefore before any token has been delivered. Gating them on bootstrap state fails the
/// *build* rather than the run, which is a confusing place to discover the mistake.
///
/// The two timeouts are separate types, so the 60x gap cannot be crossed by passing one
/// number twice — see [`crate::hooks`].
/// `ENABLED` is [`ops::HookState::Enabled`] rather than a `&str` local, which is issue #24's
/// point about this function: the literal appeared six times with no constant naming either
/// value, so a typo in one of the six was a `ValidationException` on a create call made after
/// the artifact upload. It is now a compile error.
pub fn hooks_block(port: u16, run: RunHookTimeout, build: BuildHookTimeout) -> ops::Hooks {
    const ENABLED: ops::HookState = ops::HookState::Enabled;
    ops::Hooks {
        port,
        microvm_hooks: ops::MicrovmHooks {
            run: ENABLED,
            run_timeout_in_seconds: run.as_secs(),
            resume: ENABLED,
            resume_timeout_in_seconds: run.as_secs(),
            suspend: ENABLED,
            suspend_timeout_in_seconds: run.as_secs(),
            terminate: ENABLED,
            terminate_timeout_in_seconds: run.as_secs(),
        },
        microvm_image_hooks: ops::MicrovmImageHooks {
            ready: ENABLED,
            ready_timeout_in_seconds: build.as_secs(),
            validate: ENABLED,
            validate_timeout_in_seconds: build.as_secs(),
        },
    }
}

/// A deadline elapsed on the client side. The remote resource is untouched.
pub(crate) fn timed_out(what: &str, waited: Duration) -> Error {
    Error::new(
        ErrorKind::Timeout,
        format!(
            "{what} within {:.0}s. This is a client-side deadline: nothing was cancelled, so \
             the resource is in whatever state the service last reported.",
            waited.as_secs_f64()
        ),
    )
}

#[cfg(test)]
pub(crate) mod fake;

#[cfg(test)]
mod tests {
    use super::*;

    /// The three image-name rejections each get their own message, because the pattern
    /// wording is misleading for a name that is merely long.
    #[test]
    fn each_image_name_rejection_explains_its_own_case() {
        let empty = require_valid_image_name("").expect_err("min is 1");
        assert!(
            empty.to_string().contains("at least 1 character"),
            "{empty}"
        );

        let long = require_valid_image_name(&"a".repeat(65)).expect_err("max is 64");
        let message = long.to_string();
        assert!(message.contains("65 characters"), "{message}");
        assert!(
            message.contains("after* the artifact upload"),
            "the reason to check locally is the cost: {message}"
        );
        assert!(
            !message.contains("no dots"),
            "the pattern wording is misleading for a merely-long name: {message}"
        );

        let dotted = require_valid_image_name("my.image").expect_err("dots are not in the pattern");
        let message = dotted.to_string();
        assert!(message.contains("no dots and no slashes"), "{message}");
        assert!(message.contains("[a-zA-Z0-9-_]+"), "{message}");
    }

    /// The boundaries are inclusive on both sides, and every rejection is an `InvalidArg`
    /// so the CLI reports it as refused-locally rather than as a platform failure.
    #[test]
    fn an_image_name_at_the_ceiling_is_accepted_and_one_past_it_is_not() {
        require_valid_image_name(&"a".repeat(64)).expect("64 is the ceiling");
        require_valid_image_name("a").expect("1 is the minimum");
        for bad in ["", &"a".repeat(65), "my.image", "team/image", "a b"] {
            let error = require_valid_image_name(bad).expect_err("rejected");
            assert_eq!(error.kind(), ErrorKind::InvalidArg, "{bad:?}");
        }
    }

    /// The duration range, at both boundaries. 28801 is the value the botocore gap was
    /// confirmed with, and the message has to say a second VM rather than a bigger number.
    #[test]
    fn the_duration_range_is_inclusive_and_names_the_eight_hour_ceiling() {
        assert_eq!(require_duration_in_range(1).expect("1 fits"), 1);
        assert_eq!(
            require_duration_in_range(28_800).expect("the ceiling fits"),
            28_800
        );

        for bad in [0, 28_801, u32::MAX] {
            let error = require_duration_in_range(bad).expect_err("outside 1..28800");
            assert_eq!(error.kind(), ErrorKind::InvalidArg, "{bad}");
            let message = error.to_string();
            assert!(message.contains("eight hours"), "{message}");
            assert!(
                message.contains("a second VM, not a larger number"),
                "{message}"
            );
        }
    }

    /// The hooks block enables all six hooks, and the two families carry their own
    /// timeouts — which is the whole reason they are two types.
    #[test]
    fn the_hooks_block_enables_all_six_with_the_two_families_kept_apart() {
        let hooks = hooks_block(
            9000,
            RunHookTimeout::try_new(45).expect("45s is a legal run timeout"),
            BuildHookTimeout::try_new(1800).expect("1800s is a legal build timeout"),
        );

        assert_eq!(hooks.port, 9000);
        for state in [
            hooks.microvm_hooks.run,
            hooks.microvm_hooks.resume,
            hooks.microvm_hooks.suspend,
            hooks.microvm_hooks.terminate,
            hooks.microvm_image_hooks.ready,
            hooks.microvm_image_hooks.validate,
        ] {
            assert_eq!(state, ops::HookState::Enabled, "all six hooks are served");
            // The wire spelling too, against a literal: the variant name and the string the
            // service reads are two different things, and `HookState::Enabled` rendering as
            // anything but `ENABLED` is a rejected create on six fields at once.
            assert_eq!(state.as_str(), "ENABLED");
        }

        assert_eq!(hooks.microvm_hooks.run_timeout_in_seconds, 45);
        assert_eq!(hooks.microvm_hooks.terminate_timeout_in_seconds, 45);
        assert_eq!(hooks.microvm_image_hooks.ready_timeout_in_seconds, 1800);
        assert_eq!(
            hooks.microvm_image_hooks.validate_timeout_in_seconds, 1800,
            "1800 is legal for the build family and illegal for the run one, which is why \
             one shared number cannot serve both"
        );
    }

    /// The default request carries the measured defaults, and identity repair is **off** —
    /// a capability granted by default is a capability nobody asked for.
    #[test]
    fn the_default_create_request_repairs_no_identity_and_takes_the_default_size() {
        let request = CreateImageRequest::new("img", vec![1, 2, 3], "s3://b/k", "arn:role");
        assert!(!request.repair_guest_identity);
        assert!(!request.inherit_workdir);
        assert_eq!(request.size, SizeClass::DEFAULT);
        assert_eq!(request.base_image, BaseImage::al2023());
        assert_eq!(request.token_scope, None);
        assert_eq!(request.dockerfile, None);
    }

    /// The default launch requests ingress and **not** egress: omitting egress is how you
    /// get a VM with no outbound network, which is the right default for a daemon that
    /// needs none.
    #[test]
    fn the_default_launch_requests_ingress_only() {
        let payload = RunHookPayload::for_agent_token("token").expect("a token fits");
        let request = RunMicrovmRequest::new("arn:image", payload);
        assert_eq!(request.connectors, vec![ConnectorIntent::AllIngress]);

        let with_egress = request.with_egress();
        assert_eq!(
            with_egress.connectors,
            vec![ConnectorIntent::AllIngress, ConnectorIntent::Egress]
        );
    }

    /// Asking for egress twice does not send it twice. `NetworkConnectorList` caps at 10
    /// and a duplicate is a wasted slot at best.
    #[test]
    fn asking_for_egress_twice_adds_it_once() {
        let payload = RunHookPayload::for_agent_token("token").expect("a token fits");
        let request = RunMicrovmRequest::new("arn:image", payload)
            .with_egress()
            .with_egress();
        assert_eq!(request.connectors.len(), 2);
    }

    /// `with_shell` **replaces** `ALL_INGRESS` with the measured pair rather than adding
    /// beside it: the platform accepts `ALL_INGRESS` + finer ingress at launch and
    /// refuses it only when a shell token is minted, after the VM has run and billed —
    /// and `run_microvm` refuses the combination client-side, so a `with_shell` that
    /// merely appended would build a request this client's own validation rejects.
    ///
    /// **Falsification** — watched fail 2026-08-31: dropping the `retain` from
    /// `with_shell` fails the first assertion with `ALL_INGRESS` still in the set.
    #[test]
    fn with_shell_replaces_all_ingress_with_the_measured_pair() {
        let payload = RunHookPayload::for_agent_token("token").expect("a token fits");
        let request = RunMicrovmRequest::new("arn:image", payload).with_shell();
        assert_eq!(
            request.connectors,
            vec![ConnectorIntent::HttpIngress, ConnectorIntent::ShellIngress]
        );

        // Egress is a separate question and survives in either order.
        let payload = RunHookPayload::for_agent_token("token").expect("a token fits");
        let both = RunMicrovmRequest::new("arn:image", payload)
            .with_egress()
            .with_shell()
            .with_shell();
        assert_eq!(
            both.connectors,
            vec![
                ConnectorIntent::Egress,
                ConnectorIntent::HttpIngress,
                ConnectorIntent::ShellIngress
            ],
            "egress survives, and asking twice adds nothing"
        );
    }

    /// A client-side deadline is an `ErrorKind::Timeout` and says the resource was not
    /// touched — because the caller's next question is whether to clean something up.
    #[test]
    fn a_client_side_deadline_says_the_resource_is_untouched() {
        let error = timed_out("the image did not become usable", Duration::from_secs(300));
        assert_eq!(error.kind(), ErrorKind::Timeout);
        let message = error.to_string();
        assert!(message.contains("300s"), "{message}");
        assert!(message.contains("nothing was cancelled"), "{message}");
    }

    /// The default port matches the daemon's, and a caller can change it.
    #[test]
    fn the_agent_port_defaults_to_nine_thousand_and_is_overridable() {
        let plane = ControlPlane::with_transport(
            Arc::new(fake::FakeControlPlane::new()),
            Region::UsEast1,
            Arc::new(fake::TestClock::new()),
        );
        assert_eq!(plane.port(), DEFAULT_AGENT_PORT);
        assert_eq!(plane.port(), 9000);
        assert_eq!(
            plane.with_port(8080).expect("8080 is a legal port").port(),
            8080
        );
    }

    /// **TRAP-1, the compile surface.** No public request type has a field that could carry
    /// a caller-supplied token.
    ///
    /// Asserted by destructuring rather than by grep: a struct pattern must name every
    /// field, so a `client_token` field added later fails to compile this test. That is the
    /// closest a test can get to asserting an absence.
    #[test]
    fn no_request_type_carries_a_caller_supplied_client_token() {
        let CreateImageRequest {
            name: _,
            base_image: _,
            base_image_version: _,
            dockerfile: _,
            binary: _,
            code_artifact_uri: _,
            build_role_arn: _,
            size: _,
            repair_guest_identity: _,
            inherit_workdir: _,
            run_hook_timeout: _,
            build_hook_timeout: _,
            tags: _,
            // The logging pair. The stream is a *prefix* the create call suffixes with a
            // fresh nonce — the caller cannot name the exact wire stream, which is the
            // same closure shape as the token's.
            log_group: _,
            log_stream: _,
            // A *label*, folded in beside a fresh nonce. The only token-adjacent field,
            // and it cannot become the token.
            token_scope: _,
        } = CreateImageRequest::new("img", Vec::new(), "s3://b/k", "arn:role");

        let RunMicrovmRequest {
            image_identifier: _,
            image_version: _,
            execution_role_arn: _,
            connectors: _,
            run_hook_payload: _,
            max_duration_sec: _,
            max_idle_sec: _,
            suspended_sec: _,
            auto_resume: _,
            token_scope: _,
        } = RunMicrovmRequest::new(
            "arn:image",
            RunHookPayload::for_agent_token("t").expect("fits"),
        );
    }

    /// **TRAP-1's last mile.** The wire types' `client_token` fields are `pub(crate)`, so a
    /// caller outside this crate cannot build a request body with a token of their own.
    ///
    /// Worth its own test because the hole was real and I opened it: `ops` is a public module
    /// and the wire structs are public, so `pub client_token` on them meant an external
    /// caller could bypass `CreateImageRequest` entirely, hand-build a
    /// `CreateMicrovmImageWire` with a content digest, and serialize it — the wedge with one
    /// extra step, past a request type that has no such field.
    ///
    /// This test can only observe the *inside* of the boundary, since a compile failure
    /// outside the crate is not something an inline test can assert. What it pins is that the
    /// field is reachable from here and that the value in it came from the minter — so a
    /// later widening back to `pub` is at least a visible diff against a test that documents
    /// why it is narrow. The external half is enforced by the compiler.
    #[test]
    fn the_wire_types_token_field_is_crate_private_and_minted() {
        let wire = ops::RunMicrovmWire {
            image_identifier: "arn:image".to_string(),
            image_version: None,
            execution_role_arn: None,
            ingress_network_connectors: Vec::new(),
            egress_network_connectors: None,
            idle_policy: ops::IdlePolicy {
                max_idle_duration_seconds: 600,
                suspended_duration_seconds: 600,
                auto_resume_enabled: false,
            },
            maximum_duration_in_seconds: 3_600,
            run_hook_payload: String::new(),
            // Reachable from inside the crate, and only ever populated this way.
            client_token: token::run_token("arn:image"),
        };
        assert!(wire.client_token.starts_with("run-"));
        assert!(wire.client_token.len() <= crate::constants::MAX_CLIENT_TOKEN_LEN);
        assert_ne!(
            wire.client_token,
            token::run_token("arn:image"),
            "two mints of the same scope differ, which is the property the narrowing protects"
        );
    }

    // ── issue #24's guards, at the message level ─────────────────────────────
    //
    // Each of these asserts the *refusal and its wording*; the zero-call proofs live beside
    // the operations they protect, in `image.rs` and `microvm.rs`, because only there can a
    // test observe that no control-plane call happened.

    /// The three `NonBlankString` rejections each explain their own case, and the pattern one
    /// names the character it found.
    ///
    /// The whitespace message has to name the character, because the whole class of failure this
    /// catches is a value that *looks* right: a URI or a filter pasted out of a console carries a
    /// trailing newline, satisfies "non-empty", and fails the pattern.
    ///
    /// **Guard proof.** Delete the `is_empty` branch and the blank case returns `Ok`; delete the
    /// whitespace branch and the `"s3://b/k\n"` case does.
    #[test]
    fn each_non_blank_rejection_explains_its_own_case() {
        let blank = require_non_blank("codeArtifact.uri", "").expect_err("min is 1");
        assert_eq!(blank.kind(), ErrorKind::InvalidArg);
        assert!(
            blank.to_string().contains("at least 1 character"),
            "{blank}"
        );
        assert!(
            blank.to_string().contains("NonBlankString"),
            "the shape has to be named: {blank}"
        );

        let long = require_non_blank("baseImageArn", &"a".repeat(2049)).expect_err("max is 2048");
        let message = long.to_string();
        assert!(message.contains("2049 characters"), "{message}");
        assert!(message.contains("ceiling of 2048"), "{message}");

        let newline = require_non_blank("codeArtifact.uri", "s3://bucket/key.zip\n")
            .expect_err("whitespace anywhere is refused");
        let message = newline.to_string();
        assert!(
            message.contains(r"'\n'"),
            "the message must name the character it found: {message}"
        );
        assert!(message.contains("trailing newline"), "{message}");

        // Whitespace in the middle, not only at the end: `[^\s]+` forbids it anywhere.
        let inner =
            require_non_blank("nameFilter", "coding agents").expect_err("an inner space too");
        assert!(
            inner.to_string().contains("anywhere in the value"),
            "{inner}"
        );

        // And the legal values pass, so the guard is not refusing everything.
        require_non_blank("codeArtifact.uri", "s3://bucket/agentd.zip").expect("a real URI");
        require_non_blank(
            "baseImageArn",
            "arn:aws:lambda:us-east-1:aws:microvm-image:al2023-1",
        )
        .expect("a real base ARN");
        require_non_blank("nameFilter", "coding-agents").expect("a real filter");
        require_non_blank("imageVersion", &"9".repeat(2048)).expect("2048 is the ceiling");
    }

    /// The identifier guard's two cases, and the over-long one names the model's own
    /// contradiction rather than telling the caller to shorten a value they may not have chosen.
    ///
    /// The empty message is the one that carries the reason this guard exists at all: ten of the
    /// twelve members are URI parameters, so an empty one collapses the path onto the collection
    /// and addresses the wrong resource — which the service can answer 200 to.
    ///
    /// **Guard proof.** Delete the `is_empty` branch and the blank case returns `Ok`; drop the
    /// `MicrovmImageArn` clause and the assertion on `2048` goes red.
    #[test]
    fn the_identifier_guard_names_the_collapsed_path_and_the_models_contradiction() {
        let blank = require_valid_identifier("microvmIdentifier", "").expect_err("min is 1");
        assert_eq!(blank.kind(), ErrorKind::InvalidArg);
        let message = blank.to_string();
        assert!(message.contains("at least 1 character"), "{message}");
        assert!(
            message.contains("collapses the path"),
            "the reason an empty URI parameter is dangerous has to be in the message: {message}"
        );

        let long =
            require_valid_identifier("imageIdentifier", &"a".repeat(257)).expect_err("max is 256");
        let message = long.to_string();
        assert!(message.contains("257 characters"), "{message}");
        assert!(message.contains("256"), "{message}");
        assert!(
            message.contains("MicrovmImageArn permits 2048"),
            "the model's own contradiction has to be named, or the advice is 'shorten a value \
             the service gave you': {message}"
        );

        require_valid_identifier(
            "imageIdentifier",
            "arn:aws:lambda:us-east-1:1:microvm-image:i",
        )
        .expect("a real ARN");
        require_valid_identifier("microvmIdentifier", "mvm-abc123").expect("a real id");
        require_valid_identifier("imageIdentifier", &"a".repeat(256)).expect("256 is the ceiling");
    }

    /// The role-ARN guard's three cases, and the short one guesses the actual mistake.
    ///
    /// A value under 20 characters is almost always a role *name*, so the message renders the ARN
    /// the caller probably meant rather than restating the bound. That is the difference between
    /// a diagnostic and a validation error.
    ///
    /// **Guard proof.** Delete the `is_valid_role_arn` branch and the eleven-digit case returns
    /// `Ok` — which is the case nothing else catches.
    #[test]
    fn the_role_arn_guard_guesses_the_mistake_a_short_value_is() {
        let name = require_valid_role_arn("buildRoleArn", "build-role").expect_err("min is 20");
        assert_eq!(name.kind(), ErrorKind::InvalidArg);
        let message = name.to_string();
        assert!(message.contains("role *name*"), "{message}");
        assert!(
            message.contains("arn:aws:iam::<12-digit-account>:role/build-role"),
            "the message renders the ARN they probably meant: {message}"
        );

        let long = require_valid_role_arn(
            "executionRoleArn",
            &format!("arn:aws:iam::123456789012:role/{}", "a".repeat(2048)),
        )
        .expect_err("max is 2048");
        assert!(long.to_string().contains("ceiling of 2048"), "{long}");

        // Eleven digits, which is the case no eyeball and no other check catches.
        let eleven = require_valid_role_arn("buildRoleArn", "arn:aws:iam::12345678901:role/build")
            .expect_err("the account id is eleven digits");
        let message = eleven.to_string();
        assert!(
            message.contains("exactly twelve digits"),
            "the digit count is the whole value of this check: {message}"
        );
        assert!(
            message.contains("after* the artifact upload"),
            "the reason to check locally is the cost: {message}"
        );
        assert!(
            message.contains("/aws/lambda-microvms/*"),
            "and the message says what is still the service's answer to give: {message}"
        );

        require_valid_role_arn(
            "buildRoleArn",
            "arn:aws:iam::392583147479:role/bonk-sandbox-microvm-build",
        )
        .expect("the conformance account's real build role");
    }

    /// The port guard refuses 0 and says what 0 means, because 0 is not a typo.
    ///
    /// The ceiling branch cannot fire from a `u16` call site — `MAX_PORT` is `u16::MAX` — so what
    /// is asserted here is the floor and the two legal boundaries.
    ///
    /// **Guard proof.** Change the comparison to `port < 0` (or delete the branch) and the zero
    /// case returns `Ok`.
    #[test]
    fn the_port_guard_refuses_zero_and_says_what_zero_means() {
        let zero = require_valid_port("allowedPorts[].port", 0).expect_err("min is 1");
        assert_eq!(zero.kind(), ErrorKind::InvalidArg);
        let message = zero.to_string();
        assert!(
            message.contains("let the kernel choose"),
            "0 is a value a caller passes on purpose, so the message has to say why it is not a \
             port: {message}"
        );
        assert!(
            message.contains("authorizes nothing"),
            "the proxy-token consequence: {message}"
        );
        assert!(message.contains("9000"), "the default is named: {message}");

        require_valid_port("port", 1).expect("1 is the minimum");
        require_valid_port("port", 9000).expect("the agent port");
        require_valid_port("port", 65_535).expect("65535 is the ceiling");
    }

    /// The idle-duration guard refuses 59 and says the botocore reasoning did not transfer.
    ///
    /// The message names the wrong premise on purpose. The exemption was not a wrong number, it
    /// was a correct fact about a client that no longer exists — and a reader who deletes this
    /// guard will do it by reasoning that `min` is validated locally, which is exactly the
    /// sentence the message answers.
    ///
    /// **Guard proof.** Delete the call in `run_microvm` and the zero-call test in `microvm.rs`
    /// goes red; invert the comparison here and 600 is refused while 59 passes.
    #[test]
    fn the_idle_guard_refuses_fifty_nine_and_names_the_premise_that_did_not_transfer() {
        let under = require_idle_duration(59).expect_err("min is 60");
        assert_eq!(under.kind(), ErrorKind::InvalidArg);
        let message = under.to_string();
        assert!(message.contains("59"), "{message}");
        assert!(message.contains("minimum of 60"), "{message}");
        assert!(
            message.contains("botocore"),
            "the message has to name the premise, because the premise is what was wrong: \
             {message}"
        );
        assert!(
            message.contains("aws-sigv4"),
            "and say what this client actually does instead: {message}"
        );

        require_idle_duration(60).expect("60 is the minimum");
        require_idle_duration(600).expect("the default");
        // No maximum in the model, and the client adds none.
        require_idle_duration(u32::MAX).expect("the model states no ceiling");
    }

    /// Every tag rejection names the offending key, and the key and the value keep their
    /// different rules.
    ///
    /// The two asymmetries are the content of this test: an empty value is legal and an empty key
    /// is not, and the ceilings are 128 against 256. A guard that used one rule for both would
    /// pass a 200-character key or refuse a 200-character value, and neither is what the service
    /// does.
    ///
    /// **Guard proof.** Use `MAX_TAG_KEY_LEN` for the value branch and the 256-character-value
    /// case fails; drop the `key` from any message and the assertion naming it goes red.
    #[test]
    fn every_tag_rejection_names_the_key_and_the_two_halves_keep_their_own_rules() {
        use std::collections::BTreeMap;

        let one = |key: &str, value: &str| {
            let mut tags = BTreeMap::new();
            tags.insert(key.to_string(), value.to_string());
            tags
        };

        // An empty **value** is legal. TagValue's min is 0.
        require_valid_tags(&one("owner", "")).expect("an empty tag value is legal");
        // An empty **key** is not. TagKey's min is 1.
        let blank = require_valid_tags(&one("", "conformance")).expect_err("TagKey min is 1");
        assert_eq!(blank.kind(), ErrorKind::InvalidArg);
        assert!(
            blank.to_string().contains("TagValue's minimum is 0"),
            "the asymmetry is the thing to say: {blank}"
        );

        // The two ceilings differ by 2x, and a 256-character *value* is legal.
        require_valid_tags(&one("owner", &"v".repeat(256))).expect("TagValue max is 256");
        let long_value = require_valid_tags(&one("owner", &"v".repeat(257)))
            .expect_err("257 is one past TagValue");
        assert!(long_value.to_string().contains("\"owner\""), "{long_value}");
        assert!(
            long_value.to_string().contains("257 characters"),
            "{long_value}"
        );

        require_valid_tags(&one(&"k".repeat(128), "v")).expect("TagKey max is 128");
        let long_key =
            require_valid_tags(&one(&"k".repeat(129), "v")).expect_err("129 is one past TagKey");
        let message = long_key.to_string();
        assert!(message.contains("129 characters"), "{message}");
        assert!(
            message.contains("TagValue allows 256"),
            "the message says the two halves are not interchangeable: {message}"
        );

        // The pattern, both halves, with the key named either way.
        let bad_key = require_valid_tags(&one("cost,centre", "x")).expect_err("a comma");
        let message = bad_key.to_string();
        assert!(message.contains("\"cost,centre\""), "{message}");
        assert!(
            message.contains("commas and `#` and `%` are not"),
            "{message}"
        );

        let bad_value = require_valid_tags(&one("owner", "50%")).expect_err("a percent");
        let message = bad_value.to_string();
        assert!(
            message.contains("the value of tag \"owner\""),
            "the message says which half as well as which key: {message}"
        );

        // A newline in a key: legal-looking, and the reason keys are printed with `{:?}`.
        let newline = require_valid_tags(&one("owner\n", "x")).expect_err("a control character");
        assert!(
            newline.to_string().contains(r#""owner\n""#),
            "a key with a stray newline is indistinguishable from a good one until it is quoted: \
             {newline}"
        );

        // And a realistic tag set passes, including a space and a non-Latin key.
        let mut real = BTreeMap::new();
        real.insert("owner".to_string(), "conformance".to_string());
        real.insert("cost centre".to_string(), "team/agents".to_string());
        real.insert("所有者".to_string(), "らいす".to_string());
        real.insert("empty".to_string(), String::new());
        require_valid_tags(&real).expect("a realistic tag set");

        // An empty map is not a tag problem, and `create_image` omits the member entirely.
        require_valid_tags(&BTreeMap::new()).expect("no tags is not an invalid tag");
    }

    /// **Issue #24's `HookState` half.** The two values are named by a type, so neither can be
    /// misspelled, and the block this client sends is six `Enabled`s.
    ///
    /// The wire spellings are asserted against literals rather than derived from the variant
    /// names, because the variant name and the string the service reads are two different things
    /// — the same reason `ops::VersionStatus` does it that way.
    ///
    /// **Guard proof.** Change `HookState::Enabled`'s rename to `"ENABLE"` and this fails on the
    /// serialization assertion; add a third variant and the `HOOK_STATES` comparison fails.
    #[test]
    fn the_hook_state_enum_is_the_models_two_values_and_nothing_else() {
        assert_eq!(ops::HookState::Disabled.as_str(), "DISABLED");
        assert_eq!(ops::HookState::Enabled.as_str(), "ENABLED");
        assert_eq!(ops::HookState::Enabled.to_string(), "ENABLED");

        // Through serde, which is what actually reaches the wire — `as_str` could be right while
        // the `rename` is wrong.
        assert_eq!(
            serde_json::to_value(ops::HookState::Enabled).expect("serialises"),
            serde_json::json!("ENABLED")
        );
        assert_eq!(
            serde_json::to_value(ops::HookState::Disabled).expect("serialises"),
            serde_json::json!("DISABLED")
        );

        // The typed spelling and the drift gate's array agree, or one of them is checking
        // something the other does not send.
        let spelled = [ops::HookState::Disabled, ops::HookState::Enabled];
        let mut wire: Vec<&str> = spelled.iter().map(|state| state.as_str()).collect();
        wire.sort_unstable();
        let mut pinned = crate::constants::HOOK_STATES.to_vec();
        pinned.sort_unstable();
        assert_eq!(wire, pinned);

        // And the misspellings a `String` field allowed are now parse failures on the way in,
        // which is the closest an inline test gets to asserting they are compile errors on the
        // way out.
        for typo in ["\"ENABLE\"", "\"Enabled\"", "\"ACTIVE\"", "\"enabled\""] {
            serde_json::from_str::<ops::HookState>(typo)
                .expect_err(&format!("{typo} is not a HookState"));
        }
    }
}
