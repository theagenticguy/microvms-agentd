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
//! * **TRAP-11** — there is **no** `mint_shell_auth_token` method and no `SHELL_INGRESS`
//!   intent. Both halves are closed by absence, which is why the test for them counts
//!   calls a full lifecycle made rather than asserting a refusal.
//!
//! # TRAP-11 is the one that needs saying out loud
//!
//! `CreateMicrovmShellAuthToken` is in the service model and this client does not
//! implement it. That is not an omission to be filled in later: it gates `ctr task exec`
//! through a console terminal, scoped to debugging and recommended disabled in
//! production, and it is not a programmatic exec path despite the name. The absence *is*
//! the closure — a method here would be a method a caller could reach.
//!
//! # Nothing in this module reads the service model at runtime
//!
//! Every constraint is a constant in [`crate::constants`], checked by the build gate
//! against the pinned model (TRAP-12). That matters because botocore's
//! `VALIDATED_METADATA_ATTRS` is `{required, min, document, union}` — `max`, `pattern`,
//! and `enum` violations go to the wire — so every guard in this module is load-bearing
//! rather than belt-and-braces, and "the SDK validates the model already" was never true.

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
    #[must_use]
    pub fn with_port(mut self, port: u16) -> Self {
        self.port = port;
        self
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
    /// The execution role. Optional in the model; every real launch needs one.
    pub execution_role_arn: Option<String>,
    /// The connectors to request, as intents (TRAP-4). Ingress is required for a session
    /// to work at all; omitting egress is how you get no outbound network.
    pub connectors: Vec<ConnectorIntent>,
    /// The already-validated payload carrying the agent token.
    pub run_hook_payload: RunHookPayload,
    /// `maximumDurationInSeconds`, checked against 1..=28800 when the request is sent.
    pub max_duration_sec: u32,
    /// `idlePolicy.maxIdleDurationSeconds`. The model's `min: 60` is one botocore does
    /// enforce, so there is deliberately no local guard for it.
    pub max_idle_sec: u32,
    /// `idlePolicy.suspendedDurationSeconds`.
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
pub fn hooks_block(port: u16, run: RunHookTimeout, build: BuildHookTimeout) -> ops::Hooks {
    const ENABLED: &str = "ENABLED";
    ops::Hooks {
        port,
        microvm_hooks: ops::MicrovmHooks {
            run: ENABLED.to_string(),
            run_timeout_in_seconds: run.as_secs(),
            resume: ENABLED.to_string(),
            resume_timeout_in_seconds: run.as_secs(),
            suspend: ENABLED.to_string(),
            suspend_timeout_in_seconds: run.as_secs(),
            terminate: ENABLED.to_string(),
            terminate_timeout_in_seconds: run.as_secs(),
        },
        microvm_image_hooks: ops::MicrovmImageHooks {
            ready: ENABLED.to_string(),
            ready_timeout_in_seconds: build.as_secs(),
            validate: ENABLED.to_string(),
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
            &hooks.microvm_hooks.run,
            &hooks.microvm_hooks.resume,
            &hooks.microvm_hooks.suspend,
            &hooks.microvm_hooks.terminate,
            &hooks.microvm_image_hooks.ready,
            &hooks.microvm_image_hooks.validate,
        ] {
            assert_eq!(state, "ENABLED", "all six hooks are served");
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
        assert_eq!(plane.with_port(8080).port(), 8080);
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
            // A *label*, folded in beside a fresh nonce. The only token-adjacent field,
            // and it cannot become the token.
            token_scope: _,
        } = CreateImageRequest::new("img", Vec::new(), "s3://b/k", "arn:role");

        let RunMicrovmRequest {
            image_identifier: _,
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
}
