// SPDX-License-Identifier: Apache-2.0
//! Image create, the build wait, and the stalled-build probe (TRAP-1, TRAP-2).
//!
//! # Why the wait needs a probe at all
//!
//! `CREATING` covers two situations that are indistinguishable from the outside: a build in
//! progress, and a build the service **never scheduled**. The second is the `clientToken`
//! replay signature — the image was created as a no-op replay of an earlier create, its
//! builds were never queued, it cannot be deleted (`CREATING` forbids it) and its only
//! version cannot be dropped (it is the last one).
//!
//! Waiting through it burns the full 45-minute build timeout and then reports "did not
//! become usable in time", which is a timeout message hiding a cause that was knowable in
//! four minutes. So after [`WaitOpts::stall_grace`] the wait probes the build list: every
//! build still `PENDING` means nothing was ever scheduled, and that is reported as
//! [`ErrorKind::BuildWedged`] naming the replay signature.
//!
//! # The field is `buildState`
//!
//! `MicrovmImageBuildSummary` has no `state` member. Reading `state` returns nothing from
//! every real response, which is exactly how this guard was dead for a review round while
//! its unit test passed — see [`crate::control::ops`]. The deserializer refuses a
//! `state`-spelled summary outright, so that failure mode is now a test rather than a
//! comment.

use std::time::Duration;

use super::transport::{Call, paths, send_with_retry};
use super::{ControlPlane, CreateImageRequest, artifact, hooks_block, ops, timed_out, token};
use crate::error::{Error, ErrorKind};
use crate::sizing::SizeClass;

/// How long an image may sit in `CREATING` before the stall probe runs.
///
/// Four minutes: long enough that a genuinely slow build is not accused, short enough that a
/// wedged one does not burn the full build timeout in silence.
pub const DEFAULT_STALL_GRACE: Duration = Duration::from_secs(240);

/// The default build timeout, 45 minutes.
pub const DEFAULT_BUILD_TIMEOUT: Duration = Duration::from_secs(45 * 60);

/// The default gap between polls.
///
/// Fifteen seconds, matching the Python client's `max(poll_interval, 15.0)` for the image
/// wait: a build takes minutes, so a tighter interval only spends control-plane quota.
pub const DEFAULT_IMAGE_POLL_INTERVAL: Duration = Duration::from_secs(15);

/// The log-group prefix the service writes build logs under.
///
/// Measured 2026-08-05. **Not** `/aws/lambda/microvms/*` — an IAM policy granting that
/// plausible-looking prefix instead produces server-side builds with **no logs at all**,
/// and then every build failure reads `reason=unknown`, which looks like the service
/// failing to populate `stateReason` when it is the caller's own policy discarding the
/// evidence.
pub const BUILD_LOG_GROUP_PREFIX: &str = "/aws/lambda-microvms";

/// A built image, and the log group the service created alongside it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Image {
    /// The image ARN, which is what `imageIdentifier` takes.
    pub identifier: String,
    /// The image name.
    pub name: String,
    /// The version the create call produced.
    pub version: String,
    /// The state the service last reported.
    pub state: String,
    /// The class the requested baseline selected.
    ///
    /// Carried on the image because billing follows the baseline requested at *create*
    /// time, and by the time anyone asks what a run cost the request is gone.
    pub size: SizeClass,
}

impl Image {
    /// `/aws/lambda-microvms/<image-name>`.
    ///
    /// The service creates this itself, so a Terraform stack never owns it and `terraform
    /// destroy` leaves it behind — "the stack destroyed cleanly" is not "the account is
    /// clean". Six of these accumulated before anyone noticed.
    pub fn build_log_group(&self) -> String {
        format!("{BUILD_LOG_GROUP_PREFIX}/{}", self.name)
    }

    /// Whether the state the service reported means "built and usable".
    ///
    /// Accepts the two model-backed spellings and the two tolerated ones. Kept generous
    /// because the service has answered differently across API versions, and a hard
    /// equality check on one spelling is how a working build looks like a stalled one.
    pub fn is_ready(state: &str) -> bool {
        crate::constants::MODEL_IMAGE_READY_STATES.contains(&state)
            || crate::constants::TOLERATED_IMAGE_READY_STATES.contains(&state)
    }

    /// Whether the state means the build failed.
    ///
    /// A substring test rather than a set, because the model's failure spellings are
    /// `CREATE_FAILED`, `UPDATE_FAILED`, and `DELETE_FAILED` and the thing they share is
    /// the word — matching that means a fourth added later is still recognised as a
    /// failure rather than polled until the deadline.
    pub fn is_failed(state: &str) -> bool {
        state.contains("FAILED")
    }
}

/// How long to wait and how often to poll.
#[derive(Clone, Copy, Debug)]
pub struct WaitOpts {
    /// The client-side deadline.
    pub timeout: Duration,
    /// The gap between polls.
    pub poll_interval: Duration,
    /// How long `CREATING` is tolerated before the stall probe runs (TRAP-2).
    ///
    /// Only meaningful for the image wait; the launch wait has no equivalent, because a
    /// launch has no build list to probe.
    pub stall_grace: Duration,
}

impl Default for WaitOpts {
    fn default() -> Self {
        Self {
            timeout: DEFAULT_BUILD_TIMEOUT,
            poll_interval: DEFAULT_IMAGE_POLL_INTERVAL,
            stall_grace: DEFAULT_STALL_GRACE,
        }
    }
}

impl WaitOpts {
    /// Options for the launch wait: five minutes, five-second polls.
    ///
    /// `stall_grace` is set past the timeout rather than to zero, since a zero grace would
    /// read as "probe immediately" — and there is nothing to probe on the launch path.
    pub fn for_launch() -> Self {
        Self {
            timeout: Duration::from_secs(300),
            poll_interval: Duration::from_secs(5),
            stall_grace: Duration::MAX,
        }
    }
}

impl ControlPlane {
    /// Creates an image, closing every local guard **before** the call.
    ///
    /// Order matters and is the same order the Python client uses for the same reason: the
    /// create call happens *after* the artifact upload, so a rejection the service raises
    /// costs the caller the upload first. Everything checkable is checked here — and the
    /// checks are [`Self::preflight`], so an uploading caller can run the same list before
    /// the upload and pay nothing for a request this client itself refuses.
    ///
    /// The artifact is built here too, but **not** uploaded — S3 is not in this crate's
    /// dependency set, so [`CreateImageRequest::code_artifact_uri`] is where the caller
    /// says they have already put it. [`ControlPlane::build_artifact_for`] produces the
    /// bytes to upload.
    pub async fn create_image(&self, request: CreateImageRequest) -> Result<Image, Error> {
        self.preflight(&request)?;

        let wire = ops::CreateMicrovmImageWire {
            name: request.name.clone(),
            base_image_arn: request.base_image.arn(&self.region),
            base_image_version: request.base_image_version.clone(),
            build_role_arn: request.build_role_arn.clone(),
            code_artifact: ops::CodeArtifact {
                uri: request.code_artifact_uri.clone(),
            },
            // The enum's only value. Injected rather than accepted, because a field could
            // only ever express a request AWS rejects — after the upload.
            cpu_configurations: vec![ops::CpuConfiguration {
                architecture: crate::constants::ARCHITECTURES[0].to_string(),
            }],
            resources: vec![ops::Resources {
                minimum_memory_in_mib: request.size.baseline_mib(),
            }],
            hooks: hooks_block(
                self.port,
                request.run_hook_timeout,
                request.build_hook_timeout,
            ),
            // TRAP-3: the boolean intent becomes the one accepted enum value. There is no
            // list a caller could put `CAP_SYS_ADMIN` in.
            additional_os_capabilities: request
                .repair_guest_identity
                .then(|| vec![crate::constants::CAPABILITIES[0].to_string()]),
            tags: (!request.tags.is_empty()).then(|| request.tags.clone()),
            // TRAP-1: minted here, from a label. Never from the caller.
            client_token: token::create_token(
                request.token_scope.as_deref().unwrap_or(&request.name),
            ),
        };

        let call = Call::post_json("CreateMicrovmImage", paths::microvm_images(), &wire)?;
        let reply = send_with_retry(self.transport(), call).await?;
        let created: ops::CreateMicrovmImageResponseWire = reply.json("CreateMicrovmImage")?;

        Ok(Image {
            identifier: created.image_arn,
            name: created.name,
            version: created.image_version,
            state: created.state,
            size: request.size,
        })
    }

    /// Every local guard [`Self::create_image`] runs, callable **before** the artifact upload.
    ///
    /// # One list of guards, reachable from before the upload
    ///
    /// The guards below are pure functions of the request — no call, no credential — and
    /// `create_image` runs them before its own wire call. But `create_image` runs *after*
    /// the caller's artifact upload, so from inside it a locally-refusable request has
    /// already cost the caller the S3 PUT (issue #47). This method is the same list,
    /// extracted so an uploading caller invokes it first and the list cannot drift between
    /// call sites — `create_image` delegates here rather than keeping its own copy.
    ///
    /// A caller who skips it loses nothing but the upload: `create_image` still refuses
    /// before the wire.
    pub fn preflight(&self, request: &CreateImageRequest) -> Result<(), Error> {
        super::require_valid_image_name(&request.name)?;

        if request.inherit_workdir {
            artifact::require_workdir(&request.base_image, request.dockerfile.as_deref())?;
        }
        if let Some(dockerfile) = request.dockerfile.as_deref() {
            artifact::require_matching_from(&request.base_image, dockerfile)?;
            artifact::require_matching_agentd_port(self.port, dockerfile)?;
            artifact::require_keepalive_under_idle_timeout(
                crate::session::exec::DEFAULT_STREAM_IDLE_TIMEOUT,
                dockerfile,
            )?;
            // The artifact carries the daemon unconditionally; a Dockerfile that never runs
            // it builds cleanly and fails as a run-hook timeout naming nothing (issue #46).
            artifact::require_daemon_cmd(dockerfile)?;
        }

        // Pinned only when the caller asked for it, and refused locally when they asked for
        // something the `Version` shape cannot carry — see `require_valid_version`. A blank
        // or whitespace-bearing value here is a `ValidationException` raised *after* the
        // artifact upload, which is the ordering this whole function is arranged to protect
        // against.
        if let Some(version) = request.base_image_version.as_deref() {
            super::require_valid_version("baseImageVersion", version)?;
        }

        // Every remaining member the model constrains, in the order the request lists them.
        // Each one is here for the same reason `require_valid_image_name` above is: the
        // create call happens *after* the caller's artifact upload, so a rejection the
        // service raises has already cost them the upload. Issue #24 listed all four as
        // reachable with no guard.
        //
        // `baseImageArn` is derived from `BaseImage` and the region, so it cannot be blank
        // today — checked anyway, because `BaseImage` is a public type with public fields and
        // a caller can construct one with an empty `name`, which renders an ARN this client
        // built and the service refuses.
        // `hooks.port` is deliberately **not** checked here. `ControlPlane::port` is private and
        // `with_port` is its only setter, and that setter refuses 0 against
        // `HooksPortInteger.min` — so a plane whose port is illegal cannot exist and a check here
        // would be a branch no input can reach. An unfalsifiable guard is worse than none: it
        // reads as protection and no test can make it fire. See
        // `the_hooks_port_that_reaches_the_wire_is_legal_by_construction`.
        super::require_valid_role_arn("buildRoleArn", &request.build_role_arn)?;
        super::require_non_blank("codeArtifact.uri", &request.code_artifact_uri)?;
        super::require_non_blank("baseImageArn", &request.base_image.arn(&self.region))?;
        super::require_valid_tags(&request.tags)
    }

    /// The artifact bytes to upload to [`CreateImageRequest::code_artifact_uri`].
    ///
    /// Separate from [`ControlPlane::create_image`] because the upload is the caller's — and
    /// because the byte-scan guard (AC-2-3) needs to inspect these bytes without making a
    /// control-plane call.
    pub fn build_artifact_for(&self, request: &CreateImageRequest) -> Result<Vec<u8>, Error> {
        let dockerfile = match request.dockerfile.as_deref() {
            Some(dockerfile) => dockerfile.to_string(),
            None => artifact::default_dockerfile(self.port, None, &request.base_image),
        };
        artifact::build_artifact(&request.binary, &dockerfile)
    }

    /// Polls until the image is usable, distinguishing a stalled build from a slow one.
    ///
    /// Returns the image as the service last described it. Raises
    /// [`ErrorKind::BuildWedged`] for the replay signature (TRAP-2),
    /// [`ErrorKind::Platform`] for a reported build failure, and [`ErrorKind::Timeout`]
    /// when the deadline elapses.
    pub async fn wait_for_image(
        &self,
        identifier: &str,
        size: SizeClass,
        opts: WaitOpts,
    ) -> Result<Image, Error> {
        // Before the loop rather than inside it: an empty identifier would collapse the URI
        // onto the collection and poll the *listing* until the deadline, which is a 45-minute
        // timeout about a resource nobody addressed. See `require_valid_identifier`.
        super::require_valid_identifier("imageIdentifier", identifier)?;
        let started = self.clock().elapsed();
        let mut probed = false;

        loop {
            let call = Call::get("GetMicrovmImage", paths::microvm_image(identifier));
            let reply = send_with_retry(self.transport(), call).await?;
            let got: ops::GetMicrovmImageResponseWire = reply.json("GetMicrovmImage")?;

            if Image::is_ready(&got.state) {
                return Ok(Image {
                    identifier: got.image_arn,
                    name: got.name,
                    version: got.latest_active_image_version.unwrap_or_default(),
                    state: got.state,
                    size,
                });
            }
            if Image::is_failed(&got.state) {
                return Err(self.build_failure(&got).await);
            }

            let elapsed = self.clock().elapsed().saturating_sub(started);
            if !probed && elapsed > opts.stall_grace {
                probed = true;
                // Raises for the replay signature; returns for anything else, including a
                // listing that failed.
                self.probe_stalled_build(identifier, elapsed).await?;
            }
            if elapsed >= opts.timeout {
                return Err(timed_out(
                    &format!(
                        "image {identifier} did not become usable (last state {})",
                        got.state
                    ),
                    elapsed,
                ));
            }
            self.clock().sleep(opts.poll_interval).await;
        }
    }

    /// Raises [`ErrorKind::BuildWedged`] when every build is still `PENDING`.
    ///
    /// # Why a listing failure is not swallowed silently
    ///
    /// The Python probe caught every exception and returned, on the reasoning that a
    /// best-effort probe must never break the wait (`sandbox.py:879`). That is right about
    /// the wait and wrong about the diagnosis: a probe that cannot see the build list
    /// cannot say the build is wedged, but it also should not report "everything is fine".
    /// So a listing failure is **mapped to retryable and returned as `Ok`** — the wait
    /// continues, the probe re-arms nothing, and the caller ends up with the honest
    /// timeout rather than a wedge claim made without evidence.
    ///
    /// The distinction that matters is the one the empty-log-group diagnostic makes for
    /// build failures: unknown is not empty, and a claim made on a throttled API call sends
    /// the reader after the wrong cause.
    async fn probe_stalled_build(&self, identifier: &str, elapsed: Duration) -> Result<(), Error> {
        let Some(builds) = self.builds_of_first_version(identifier).await else {
            // Could not see the build list. Say nothing rather than guess.
            return Ok(());
        };

        if builds.is_empty() {
            // No builds listed at all is not the signature: the replay case lists builds
            // and leaves them PENDING. An empty list is a version whose builds have not
            // been enumerated yet.
            return Ok(());
        }
        // `buildState`, not `state`. The deserializer refuses the other spelling, so this
        // read cannot silently produce nothing the way `b.get("state")` did.
        if !builds.iter().all(|build| build.build_state == "PENDING") {
            return Ok(());
        }

        Err(Error::new(
            ErrorKind::BuildWedged,
            format!(
                "build never scheduled after {:.0}s: all {} builds are still PENDING ({}). This \
                 is the clientToken replay signature — a clientToken is a permanent idempotency \
                 key, so a create whose token repeats an earlier one is replayed as a no-op: the \
                 image sits in CREATING with its builds never queued, cannot be deleted (CREATING \
                 forbids it), and its only version cannot be dropped either because it is the last \
                 one. Two images were wedged this way for ~15 hours (docs/PLATFORM.md, \
                 '`clientToken` is a permanent idempotency key'). Waiting will not help.",
                elapsed.as_secs_f64(),
                builds.len(),
                builds
                    .iter()
                    .map(ops::MicrovmImageBuildSummaryWire::describe)
                    .collect::<Vec<_>>()
                    .join(", "),
            ),
        ))
    }

    /// Every build of the image's first version, or `None` when the listing could not be
    /// read.
    ///
    /// Two calls — versions, then builds — because `ListMicrovmImageBuilds` requires an
    /// `imageVersion` in its path.
    ///
    /// # Every page, because the verdict is over *all* builds
    ///
    /// `ListMicrovmImageBuilds` caps `maxResults` at 50, and [`Self::probe_stalled_build`]
    /// asserts **all** builds are `PENDING`. Over a truncated page that quantifier is a
    /// claim about a subset presented as a claim about the set, and it is wrong in both
    /// directions: 50 pending builds on page one with a scheduled build on page two reads
    /// as a wedge on a healthy image, and one page of scheduled builds hides a real wedge.
    /// Since the verdict is a universal, the loop reads to the last page before deciding.
    async fn builds_of_first_version(
        &self,
        identifier: &str,
    ) -> Option<Vec<ops::MicrovmImageBuildSummaryWire>> {
        let version = self.first_version(identifier).await?.image_version;
        self.builds_of_version(identifier, &version).await
    }

    /// Every build of `version`, or `None` when the listing could not be read.
    async fn builds_of_version(
        &self,
        identifier: &str,
        version: &str,
    ) -> Option<Vec<ops::MicrovmImageBuildSummaryWire>> {
        let mut builds = Vec::new();
        let mut next_token: Option<String> = None;
        loop {
            let call = Call::get(
                "ListMicrovmImageBuilds",
                paths::image_builds(identifier, version, next_token.as_deref()),
            );
            let page: ops::ListImageBuildsResponseWire = send_with_retry(self.transport(), call)
                .await
                .ok()?
                .json("ListMicrovmImageBuilds")
                .ok()?;
            builds.extend(page.items);
            match page.next_token {
                Some(token) => next_token = Some(token),
                None => return Some(builds),
            }
        }
    }

    /// The first version listed, or `None` when the listing could not be read or the image
    /// has none.
    ///
    /// Only the first page is fetched, and that is correct rather than a second instance
    /// of the same bug: this reads `items.first()`, so a later page cannot change the
    /// answer. Fetching the rest would be calls whose results are discarded.
    async fn first_version(&self, identifier: &str) -> Option<ops::MicrovmImageVersionSummaryWire> {
        let call = Call::get(
            "ListMicrovmImageVersions",
            paths::image_versions(identifier, None),
        );
        let versions: ops::ListImageVersionsResponseWire = send_with_retry(self.transport(), call)
            .await
            .ok()?
            .json("ListMicrovmImageVersions")
            .ok()?;
        versions.items.into_iter().next()
    }

    /// The message for a build the service reported as failed, with whatever reason the
    /// service already stated.
    ///
    /// # The reasons come first, because the service usually has one
    ///
    /// `GetMicrovmImage` has no `stateReason` member — the shape structurally cannot carry
    /// one — so for a while this message could only name the log group and guess. It does
    /// not have to guess: `MicrovmImageVersionSummary` and `MicrovmImageBuildSummary` both
    /// carry a `stateReason`, and `GetMicrovmImage` names the failed version in
    /// `latestFailedImageVersion`. So two listings the client already knows how to make
    /// turn "the build failed, here is where the logs would be" into "the build failed
    /// because X".
    ///
    /// The lookup is **best effort**: a listing that cannot be read leaves the reasons out
    /// and the message keeps its log-group remedy, for the reason
    /// [`Self::probe_stalled_build`] gives at length — a diagnosis made on a throttled call
    /// sends the reader after the wrong cause. What it must never do is turn a real build
    /// failure into a listing error, so nothing here returns `Err`.
    ///
    /// # The log-group paragraph stays
    ///
    /// It names the required prefix, because a failure that reads as "unknown" is most often
    /// a build role granted the *plausible* prefix rather than the measured one. This client
    /// cannot check the log group — CloudWatch is not in its dependency set — so the prefix
    /// is named whenever no reason was found, rather than unconditionally. That conditional
    /// is what the reasons above buy back: this used to be "a deliberate weakening" of the
    /// Python diagnostic precisely because the client had no reason in hand, and now it does.
    /// The full assessment of why no log *read* happens here is in
    /// `microvms-cli/src/commands/local.rs`'s `logs`, beside the command a caller reaches
    /// for instead.
    async fn build_failure(&self, got: &ops::GetMicrovmImageResponseWire) -> Error {
        let name = &got.name;
        let state = &got.state;
        let reasons = self.failure_reasons(got).await;

        let said = if reasons.is_empty() {
            format!(
                "Neither the failed version nor any of its builds carried a stateReason, so the \
                 service stated no cause. If the build log group {BUILD_LOG_GROUP_PREFIX}/{name} \
                 also contains no events, the cause is most likely the build role's log \
                 permissions rather than a silent service: the role must grant logs on the \
                 {BUILD_LOG_GROUP_PREFIX}/* prefix, and a policy granting /aws/lambda/microvms/* \
                 instead — the plausible spelling, and the wrong one — produces builds with no \
                 logs at all (docs/PLATFORM.md, 'Build logs go to \
                 {BUILD_LOG_GROUP_PREFIX}/<image-name>')."
            )
        } else {
            format!(
                "The service stated the cause: {}. Full build logs are in \
                 {BUILD_LOG_GROUP_PREFIX}/{name}.",
                reasons.join("; "),
            )
        };

        Error::new(
            ErrorKind::Platform,
            format!("the image build for {name:?} failed: {state}. {said}"),
        )
    }

    /// Every `stateReason` the service already stated about this image's failure: the
    /// version's, then each failed build's.
    ///
    /// Empty when nothing could be read or nothing carried a reason, which the caller
    /// reports as the service having stated no cause rather than as an absent lookup.
    async fn failure_reasons(&self, got: &ops::GetMicrovmImageResponseWire) -> Vec<String> {
        // The version the service itself named as the failed one, falling back to the first
        // listed — which is what an image whose only version failed looks like.
        let Some(version) = self.failed_version(got).await else {
            return Vec::new();
        };

        let mut reasons = Vec::new();
        if let Some(reason) = version.state_reason.as_deref() {
            reasons.push(format!(
                "version {} is {} because {reason}",
                version.image_version, version.state
            ));
        }

        // The build-level reasons are the specific ones: a version's reason is often "one
        // or more builds failed", which names the shape of the problem and not the problem.
        if let Some(builds) = self
            .builds_of_version(&got.image_arn, &version.image_version)
            .await
        {
            for build in builds
                .iter()
                .filter(|build| build.state_reason.is_some() && build.build_state != "PENDING")
            {
                reasons.push(format!(
                    "build {}",
                    self.describe_build_deeply(&got.image_arn, &version.image_version, build)
                        .await
                ));
            }
        }
        reasons
    }

    /// One failed build, described as fully as the service will say: the listing's line, or
    /// `GetMicrovmImageBuild`'s richer one when that call answers.
    ///
    /// # What the extra call buys, and why it is worth one per failed build
    ///
    /// The listing already carries the `stateReason`, so this is not about the reason. It is
    /// about `snapshotBuild`, which the listing has no member for and which is the **only**
    /// size any operation in this model reports. On a real failure it comes back **partial** —
    /// measured 2026-08-16 against a `FAILED` build, `codeInstallSizeInBytes` alone with no
    /// memory and no disk snapshot — and that partial shape is itself the diagnosis: the build
    /// installed 1.7 GB of code and then never produced a snapshot, which distinguishes "the
    /// image was too big / the ready hook never answered" from "the Dockerfile failed before
    /// anything was installed". It also names the `chipsetGeneration`, which matters because
    /// one `CreateMicrovmImage` fans out into one build per generation and they can disagree.
    ///
    /// # Best effort, in the same sense the rest of this diagnosis is
    ///
    /// A `GetMicrovmImageBuild` that fails leaves the listing's own line in place rather than
    /// dropping the build from the report or turning a build failure into a lookup error. That
    /// is the rule [`Self::probe_stalled_build`] states at length: a diagnosis made on a
    /// throttled call sends the reader after the wrong cause, and the reason the listing
    /// already gave is not made less true by a second call failing.
    async fn describe_build_deeply(
        &self,
        identifier: &str,
        version: &str,
        listed: &ops::MicrovmImageBuildSummaryWire,
    ) -> String {
        match self
            .get_image_build(identifier, version, &listed.build_id)
            .await
        {
            Ok(deeper) => deeper.describe(),
            Err(_) => listed.describe(),
        }
    }

    /// The version `latestFailedImageVersion` names, or the first version listed when the
    /// service named none.
    async fn failed_version(
        &self,
        got: &ops::GetMicrovmImageResponseWire,
    ) -> Option<ops::MicrovmImageVersionSummaryWire> {
        let Some(wanted) = got.latest_failed_image_version.as_deref() else {
            return self.first_version(&got.image_arn).await;
        };

        let mut next_token: Option<String> = None;
        loop {
            let call = Call::get(
                "ListMicrovmImageVersions",
                paths::image_versions(&got.image_arn, next_token.as_deref()),
            );
            let page: ops::ListImageVersionsResponseWire = send_with_retry(self.transport(), call)
                .await
                .ok()?
                .json("ListMicrovmImageVersions")
                .ok()?;
            if let Some(hit) = page
                .items
                .into_iter()
                .find(|item| item.image_version == wanted)
            {
                return Some(hit);
            }
            next_token = Some(page.next_token?);
        }
    }

    // ── build and version introspection ──────────────────────────────────────

    /// `GetMicrovmImageBuild`: one build, with the snapshot sizes the listing does not carry.
    ///
    /// # What this adds over the listing, and it is not marginal
    ///
    /// `snapshotBuild` — the memory, code, and disk byte counts — is the **only** size any
    /// operation in this model reports for anything the platform builds. `GetMicrovmImage`
    /// carries none, `GetMicrovm` carries none, and both listings carry none, so a storage
    /// estimate had nothing to multiply but the size class's baseline. These three figures are
    /// what the snapshot read, write, and storage line items actually bill on.
    ///
    /// It also carries a `stateReason` at the same level the listing does, which is the level
    /// that is populated on a real failure — the version's is null (docs/PLATFORM.md).
    ///
    /// `build_id` comes off [`ops::MicrovmImageBuildSummaryWire::build_id`], which the listing
    /// already parses; nothing else in the API mints one.
    pub async fn get_image_build(
        &self,
        identifier: &str,
        version: &str,
        build_id: &str,
    ) -> Result<ops::GetImageBuildResponseWire, Error> {
        super::require_valid_identifier("imageIdentifier", identifier)?;
        // Both URI members are `NonBlankString`, not `Version` — `GetMicrovmImageBuild` is the
        // one operation where `imageVersion` names the reused shape rather than the version one,
        // and the two are identical today. Checked as what the model says they are.
        super::require_non_blank("imageVersion", version)?;
        super::require_non_blank("buildId", build_id)?;
        let call = Call::get(
            "GetMicrovmImageBuild",
            paths::image_build(identifier, version, build_id),
        );
        let reply = send_with_retry(self.transport(), call).await?;
        reply.json("GetMicrovmImageBuild")
    }

    /// `GetMicrovmImageVersion`: one version's whole configuration, its state, and its
    /// availability status.
    ///
    /// # The only full config readback there is
    ///
    /// The response echoes the creation request: `baseImageArn` with the `baseImageVersion`
    /// that was actually used, the `buildRoleArn`, the `codeArtifact` URI, the egress
    /// connectors, the CPU configurations, and `resources` — which is where a built image's
    /// size class lives, since `GetMicrovm` reports no memory figure at all. It is how a
    /// caller who no longer holds the request finds out what an image was built with.
    ///
    /// `status` is the member that matters for a retire: `ACTIVE` means `RunMicrovm` will
    /// launch this version, `INACTIVE` means it refuses. See
    /// [`Self::set_image_version_status`].
    ///
    /// One call rather than a filtered `ListMicrovmImageVersions`, and the difference is not
    /// only cost: a listing can page, so "find version X" is a loop that may read the whole
    /// history, while this addresses the version directly and answers
    /// `ResourceNotFoundException` when it is gone.
    pub async fn get_image_version(
        &self,
        identifier: &str,
        version: &str,
    ) -> Result<ops::MicrovmImageVersionSummaryWire, Error> {
        super::require_valid_identifier("imageIdentifier", identifier)?;
        super::require_non_blank("imageVersion", version)?;
        let call = Call::get(
            "GetMicrovmImageVersion",
            paths::image_version(identifier, version),
        );
        let reply = send_with_retry(self.transport(), call).await?;
        reply.json("GetMicrovmImageVersion")
    }

    /// `UpdateMicrovmImageVersion`: set a version `ACTIVE` or `INACTIVE`.
    ///
    /// # The model's only non-destructive retire
    ///
    /// `DeleteMicrovmImageVersion` is the alternative and it is irreversible, cannot be
    /// applied to an image's last version at all, and destroys the readback a post-mortem
    /// needs. `INACTIVE` does none of that: `RunMicrovm` refuses to launch the version,
    /// **already-running VMs keep running**, and [`Self::get_image_version`] still answers
    /// with the whole configuration. So the rollback for a bad build is two calls that both
    /// preserve evidence — set the new version INACTIVE, re-pin the launch to the old one —
    /// rather than a delete.
    ///
    /// # Why the status is typed and the version is checked
    ///
    /// [`ops::VersionStatus`] has two variants, so `"INACTIVATE"` is a compile error rather
    /// than a `ValidationException` on the only member this request has. The version string is
    /// checked against the `Version` shape locally for the reason
    /// [`super::require_valid_version`] gives: this call is made at the moment someone is
    /// rolling back, which is when a failure about the request rather than about the version
    /// is most expensive.
    ///
    /// Returns the readback, which carries the status the service now reports — so a caller
    /// confirms the change rather than assuming the 200 meant it took.
    pub async fn set_image_version_status(
        &self,
        identifier: &str,
        version: &str,
        status: ops::VersionStatus,
    ) -> Result<ops::MicrovmImageVersionSummaryWire, Error> {
        super::require_valid_identifier("imageIdentifier", identifier)?;
        super::require_valid_version("imageVersion", version)?;

        let wire = ops::UpdateImageVersionWire { status };
        let call = Call::patch_json(
            "UpdateMicrovmImageVersion",
            paths::image_version(identifier, version),
            &wire,
        )?;
        let reply = send_with_retry(self.transport(), call).await?;
        let updated: ops::MicrovmImageVersionSummaryWire =
            reply.json("UpdateMicrovmImageVersion")?;

        // The readback is asserted rather than trusted, for the reason
        // `ops::DeleteImageResponseWire` gives about a 2xx that reports a failure state: a
        // request the service accepts and does not apply is indistinguishable from one it
        // applied, and here the consequence is a version a caller believes is retired while
        // `RunMicrovm` still launches it.
        if updated.status != status.as_str() {
            return Err(Error::new(
                ErrorKind::Platform,
                format!(
                    "UpdateMicrovmImageVersion answered 2xx for version {version} of \
                     {identifier} but read back status {} rather than the requested {status}. A \
                     version that is still ACTIVE is one RunMicrovm will still launch, so \
                     treating the 200 as the change having taken would leave a retired version \
                     reachable.",
                    updated.status,
                ),
            ));
        }
        Ok(updated)
    }

    /// `ListManagedMicrovmImageVersions`, read to its last page: the versions of a managed
    /// base image, newest first as the service orders them.
    ///
    /// # What this is for
    ///
    /// [`ops::CreateMicrovmImageWire::base_image_version`] is how a build stops floating on
    /// whatever the service currently defaults to, and this is the only operation that says
    /// what the legal values are. The default has already moved once: `al2023-1` carried one
    /// version in June and two by July (`"0"` and `"1"`, measured 2026-08-16), so every build
    /// made before this existed recorded no base version and is not reproducible.
    ///
    /// # Every page, and an ARN rather than a name
    ///
    /// Every page for the reason the other listings give: a version on page two is still a
    /// version, and a caller pinning "the newest" from a truncated page pins the wrong one.
    ///
    /// The identifier must be the base's **full ARN** — a bare `al2023-1` answers
    /// `ValidationException: Invalid ARN format` (measured 2026-08-16) — so that is checked
    /// here rather than at the wire, because the service's message names the value without
    /// saying which member wanted an ARN or that
    /// [`super::BaseImage::arn`] is what produces one.
    pub async fn managed_base_versions(
        &self,
        base_image_arn: &str,
    ) -> Result<Vec<ops::ManagedMicrovmImageVersionWire>, Error> {
        // The shape bound first, then the ARN precondition. Order matters only for the empty
        // case: `""` is not an ARN either, and "needs the full ARN, not \"\"" is a less useful
        // message than the identifier guard's account of what an empty URI parameter does to the
        // path.
        super::require_valid_identifier("imageIdentifier", base_image_arn)?;
        if !base_image_arn.starts_with("arn:") {
            return Err(Error::new(
                ErrorKind::Precondition,
                format!(
                    "ListManagedMicrovmImageVersions needs the managed base's full ARN, not \
                     {base_image_arn:?}. The service answers `ValidationException: Invalid ARN \
                     format: {base_image_arn}` for a bare name (measured 2026-08-16), which \
                     names the value without saying which member wanted an ARN. \
                     `BaseImage::al2023().arn(region)` builds one — \
                     arn:aws:lambda:{}:aws:microvm-image:{base_image_arn}.",
                    self.region.as_str(),
                ),
            ));
        }

        let mut items = Vec::new();
        let mut next_token: Option<String> = None;
        loop {
            let call = Call::get(
                "ListManagedMicrovmImageVersions",
                paths::managed_image_versions(base_image_arn, next_token.as_deref()),
            );
            let reply = send_with_retry(self.transport(), call).await?;
            let page: ops::ListManagedVersionsResponseWire =
                reply.json("ListManagedMicrovmImageVersions")?;
            items.extend(page.items);
            match page.next_token {
                Some(token) => next_token = Some(token),
                None => return Ok(items),
            }
        }
    }

    /// `ListManagedMicrovmImages`, read to its last page: the base images AWS publishes.
    ///
    /// # Informational only, and that is a limitation rather than a choice
    ///
    /// `ManagedMicrovmImageSummary` carries an ARN and two timestamps and **nothing else** —
    /// no registry reference, no architecture, no working directory. A
    /// [`super::BaseImage`] needs all three of ARN, Dockerfile `FROM`, and WORKDIR knowledge,
    /// because `require_matching_from` compares a caller's Dockerfile against the `FROM` and
    /// `require_workdir` refuses inheritance when the base declares none. Neither guard has an
    /// input here, and the registry ref is not derivable from the ARN — `al2023-1` pairs with
    /// `public.ecr.aws/amazonlinux/amazonlinux:2023-minimal` and nothing in the ARN says so.
    ///
    /// So a discovered base **cannot safely be built from**, and this exists to answer one
    /// question: has AWS published a base this client does not know about? `microvm doctor`
    /// reports it. Measured 2026-08-16, the answer is one item, so hardcoding `al2023-1`
    /// currently misses nothing.
    pub async fn managed_base_images(
        &self,
    ) -> Result<Vec<ops::ManagedMicrovmImageSummaryWire>, Error> {
        let mut items = Vec::new();
        let mut next_token: Option<String> = None;
        loop {
            let call = Call::get(
                "ListManagedMicrovmImages",
                paths::managed_microvm_images(next_token.as_deref()),
            );
            let reply = send_with_retry(self.transport(), call).await?;
            let page: ops::ListManagedImagesResponseWire =
                reply.json("ListManagedMicrovmImages")?;
            items.extend(page.items);
            match page.next_token {
                Some(token) => next_token = Some(token),
                None => return Ok(items),
            }
        }
    }

    /// `ListMicrovmImages`, read to its last page.
    ///
    /// # Why this is public where the private walkers are not
    ///
    /// [`Self::find_image_by_name`] sends `nameFilter` and answers one image, which is what a
    /// resolver wants and is a different question from "what is in this account". The live tier
    /// asks the second: `tests/live_versions.rs` walks the account looking for an image with a
    /// version in a particular state, because *which* image that is is an account fact a test
    /// must not hardcode. `microvm ls` deliberately does **not** use this — it reads the local
    /// ledger, because the resources worth asking about are the ones a killed process never got
    /// to report and no listing can attribute those back to a command.
    pub async fn list_images(&self) -> Result<Vec<ops::MicrovmImageSummaryWire>, Error> {
        let mut items = Vec::new();
        let mut next_token: Option<String> = None;
        loop {
            let call = Call::get(
                "ListMicrovmImages",
                paths::microvm_images_list(None, next_token.as_deref()),
            );
            let reply = send_with_retry(self.transport(), call).await?;
            let page: ops::ListImagesResponseWire = reply.json("ListMicrovmImages")?;
            items.extend(page.items);
            match page.next_token {
                Some(token) => next_token = Some(token),
                None => return Ok(items),
            }
        }
    }

    /// `ListMicrovmImageVersions`, read to its last page.
    ///
    /// A `Result` where the private [`Self::builds_of_version`] answers `Option`, and the
    /// difference is the caller: the private one feeds a *diagnosis*, where a listing failure
    /// must not become a wedge claim, and this one feeds a caller who asked about the versions
    /// and needs to know if the answer is unavailable.
    pub async fn list_image_versions(
        &self,
        identifier: &str,
    ) -> Result<Vec<ops::MicrovmImageVersionSummaryWire>, Error> {
        super::require_valid_identifier("imageIdentifier", identifier)?;
        let mut items = Vec::new();
        let mut next_token: Option<String> = None;
        loop {
            let call = Call::get(
                "ListMicrovmImageVersions",
                paths::image_versions(identifier, next_token.as_deref()),
            );
            let reply = send_with_retry(self.transport(), call).await?;
            let page: ops::ListImageVersionsResponseWire =
                reply.json("ListMicrovmImageVersions")?;
            items.extend(page.items);
            match page.next_token {
                Some(token) => next_token = Some(token),
                None => return Ok(items),
            }
        }
    }

    /// `ListMicrovmImageBuilds` for one version, read to its last page.
    ///
    /// The identifiers [`Self::get_image_build`] takes come from here: nothing else in the API
    /// mints a `buildId`. See [`Self::list_image_versions`] on why this answers a `Result` where
    /// the diagnosis path's walker answers an `Option`.
    pub async fn list_image_builds(
        &self,
        identifier: &str,
        version: &str,
    ) -> Result<Vec<ops::MicrovmImageBuildSummaryWire>, Error> {
        super::require_valid_identifier("imageIdentifier", identifier)?;
        super::require_non_blank("imageVersion", version)?;
        let mut items = Vec::new();
        let mut next_token: Option<String> = None;
        loop {
            let call = Call::get(
                "ListMicrovmImageBuilds",
                paths::image_builds(identifier, version, next_token.as_deref()),
            );
            let reply = send_with_retry(self.transport(), call).await?;
            let page: ops::ListImageBuildsResponseWire = reply.json("ListMicrovmImageBuilds")?;
            items.extend(page.items);
            match page.next_token {
                Some(token) => next_token = Some(token),
                None => return Ok(items),
            }
        }
    }

    /// The image with exactly `name`, or `None` when no page of the listing has one.
    ///
    /// # `nameFilter` narrows, it does not answer
    ///
    /// The model's `nameFilter` is a server-side **substring** filter ("images whose name
    /// contains the specified string"), so `coding-agents` also matches
    /// `coding-agents-old`. The filter is sent — it keeps a large account's listing to one
    /// page in practice — and the exact-match comparison happens here, on the client,
    /// because "contains" and "is" are different questions and only the second one is
    /// safe to launch from.
    ///
    /// # The listing paginates, and every page is read
    ///
    /// An image on page two is still an image. A resolver that read only the first page
    /// would answer "no image named X" for a name that exists — the confident wrong
    /// answer, which then sends the caller to rebuild an image they already paid for.
    pub async fn find_image_by_name(
        &self,
        name: &str,
    ) -> Result<Option<ops::MicrovmImageSummaryWire>, Error> {
        // `nameFilter` is a `NonBlankString` **querystring** member, which is the reason a blank
        // one is worth refusing rather than letting through: it does not fail as a missing field,
        // it goes out as `?nameFilter=` and either 400s or filters differently from what was
        // meant. This function is the resolver a launch depends on, so a silently different
        // filter is a launch against the wrong image.
        super::require_non_blank("nameFilter", name)?;
        let mut next_token: Option<String> = None;
        loop {
            let call = Call::get(
                "ListMicrovmImages",
                paths::microvm_images_list(Some(name), next_token.as_deref()),
            );
            let reply = send_with_retry(self.transport(), call).await?;
            let listed: ops::ListImagesResponseWire = reply.json("ListMicrovmImages")?;

            if let Some(hit) = listed.items.into_iter().find(|item| item.name == name) {
                return Ok(Some(hit));
            }
            match listed.next_token {
                Some(token) => next_token = Some(token),
                None => return Ok(None),
            }
        }
    }

    /// The ARN for `identifier`: an ARN passes through untouched, a bare name is
    /// resolved through the image listing by exact name.
    ///
    /// # Why this exists
    ///
    /// `RunMicrovm.imageIdentifier` takes an ARN, and a bare name sent verbatim is
    /// answered with HTTP 400 "Malformed ARN" — after a credential resolution and a
    /// signed call, and with a message that says nothing about names. Every consumer was
    /// scripting this resolution with the AWS CLI (the coding-agents example did); this
    /// is that script, once, with the pagination handled.
    ///
    /// # An ARN costs zero calls
    ///
    /// The passthrough is checked first, so a caller who already holds the ARN pays
    /// nothing for the convenience existing. The prefix test is `arn:` exactly — the one
    /// spelling every AWS ARN starts with, and a string the `ImageName` pattern
    /// (`[a-zA-Z0-9-_]+`) cannot produce, so no legal image name is mistaken for one.
    pub async fn resolve_image_arn(&self, identifier: &str) -> Result<String, Error> {
        if identifier.starts_with("arn:") {
            return Ok(identifier.to_string());
        }
        match self.find_image_by_name(identifier).await? {
            Some(image) => Ok(image.image_arn),
            None => Err(Error::new(
                ErrorKind::Precondition,
                format!(
                    "no image named {identifier:?} exists in {} (the listing was read to its \
                     last page and resolution requires an exact name match). Build one first — \
                     `microvm build <binary> --name {identifier}` — or pass the image ARN \
                     directly.",
                    self.region.as_str(),
                ),
            )),
        }
    }

    /// The content hash of the artifact `request` would build, for content-addressed
    /// image reuse.
    ///
    /// Derives the Dockerfile exactly as [`ControlPlane::build_artifact_for`] does —
    /// same default, same port — so the name a reuse check computes is the name a build
    /// of the same request would carry. See [`artifact::artifact_content_hash`] for what
    /// is hashed and why it is the inputs rather than the zip.
    pub fn artifact_content_hash_for(&self, request: &CreateImageRequest) -> String {
        let dockerfile = match request.dockerfile.as_deref() {
            Some(dockerfile) => dockerfile.to_string(),
            None => artifact::default_dockerfile(self.port, None, &request.base_image),
        };
        artifact::artifact_content_hash(&request.binary, &dockerfile)
    }

    /// Deletes every version but the first, then the image, retrying.
    ///
    /// # Why it retries
    ///
    /// Not politeness: an image in `CREATING` refuses deletion and a VM still terminating
    /// holds a reference. This loop is the difference between a clean account and a billed
    /// leak.
    ///
    /// # Why not `backon`, unlike `transport::send_with_retry`
    ///
    /// Assessed for the dependency sweep (issue #91) and kept as a loop: the sleeps here
    /// go through the injected [`Clock`] seam so tests drive them without real time, and
    /// `Clock::sleep` returns a future borrowing `&self` — `backon`'s `Sleeper` needs a
    /// future that owns its state, so bridging the seam costs an adapter bigger than the
    /// five lines it would replace. The fixed interval is also the contract: teardown
    /// polls a state that resolves on the service's schedule, not a growing backoff.
    ///
    /// # Why the first version is kept
    ///
    /// The last remaining version cannot be deleted on its own, only together with the
    /// image. Trying produces a `ConflictException` that reads like a permissions problem.
    ///
    /// Returns `true` when the image was deleted, `false` when every attempt failed. It
    /// does not raise, because the caller is a teardown path and the original failure is
    /// the one worth reading.
    ///
    /// # The identifier check is here rather than in [`Self::try_delete_image`]
    ///
    /// Inside the attempt it would be re-decided on every retry, which for the default twenty
    /// attempts is nineteen backoff sleeps over a value that cannot become valid. Before the loop
    /// it costs one comparison and zero calls. The refusal is a `false` rather than an `Err`
    /// because this function's whole contract is that it does not raise into a teardown path —
    /// and `false` from here is honest: no image was deleted.
    pub async fn delete_image(&self, identifier: &str, attempts: u32, backoff: Duration) -> bool {
        if super::require_valid_identifier("imageIdentifier", identifier).is_err() {
            return false;
        }
        for attempt in 0..attempts.max(1) {
            if attempt > 0 {
                self.clock().sleep(backoff).await;
            }
            if self.try_delete_image(identifier).await.is_ok() {
                return true;
            }
        }
        false
    }

    /// One deletion attempt: drop the extra versions, then the image.
    ///
    /// # Every page of versions, or the image cannot be deleted at all
    ///
    /// `ListMicrovmImageVersions` caps `maxResults` at 50. Reading one page and then
    /// deleting the image is not a partial cleanup, it is a **permanent** one: the versions
    /// on page two still exist, so the final `DeleteMicrovmImage` answers
    /// `ConflictException`, [`Self::delete_image`]'s retry loop re-reads the same first
    /// page every attempt, and the call returns `false` forever. The outcome is a billing
    /// image nothing can delete through this client, which is the exact outcome the retry
    /// loop exists to prevent.
    ///
    /// The whole listing is collected before the first delete rather than deleted
    /// page-by-page, because deleting from under a cursor is how a paginated traversal
    /// skips items: the service's page boundaries move as the collection shrinks.
    async fn try_delete_image(&self, identifier: &str) -> Result<(), Error> {
        let mut versions: Vec<String> = Vec::new();
        let mut next_token: Option<String> = None;
        loop {
            let call = Call::get(
                "ListMicrovmImageVersions",
                paths::image_versions(identifier, next_token.as_deref()),
            );
            let page: ops::ListImageVersionsResponseWire = send_with_retry(self.transport(), call)
                .await?
                .json("ListMicrovmImageVersions")?;
            versions.extend(page.items.into_iter().map(|item| item.image_version));
            match page.next_token {
                Some(token) => next_token = Some(token),
                None => break,
            }
        }

        // Skip the first: the last remaining version goes with the image.
        for version in versions.iter().skip(1) {
            let call = Call::delete(
                "DeleteMicrovmImageVersion",
                paths::image_version(identifier, version),
            );
            send_with_retry(self.transport(), call).await?;
        }

        let call = Call::delete("DeleteMicrovmImage", paths::microvm_image(identifier));
        let reply = send_with_retry(self.transport(), call).await?;

        // The readback is parsed rather than discarded, so `DeleteImageResponseWire` is a
        // live shape. See [`ops::DeleteImageResponseWire`] for why the state is checked
        // for a *failure* spelling only: DELETING and DELETED are both the call having
        // worked, and treating DELETING as incomplete would retry a deletion in progress.
        let deleted: ops::DeleteImageResponseWire = reply.json("DeleteMicrovmImage")?;
        if Image::is_failed(&deleted.state) {
            return Err(Error::new(
                ErrorKind::Platform,
                format!(
                    "DeleteMicrovmImage answered 2xx for {} but reported state {}, so the image \
                     still exists and still bills. A DELETE_FAILED readback is the service \
                     accepting the request and refusing the work, which is not something a retry \
                     of the identical request fixes.",
                    deleted.image_identifier, deleted.state,
                ),
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::super::fake::{self as fake, Answer, FakeControlPlane, TestClock};
    use super::*;
    use crate::hooks::{BuildHookTimeout, RunHookTimeout};
    use crate::region::Region;

    /// A plane over the fake, and the two handles a test asserts through.
    fn planted() -> (ControlPlane, Arc<FakeControlPlane>, Arc<TestClock>) {
        let fake = Arc::new(FakeControlPlane::new());
        let clock = Arc::new(TestClock::new());
        let plane = ControlPlane::with_transport(fake.clone(), Region::UsEast1, clock.clone());
        (plane, fake, clock)
    }

    fn a_request() -> CreateImageRequest {
        CreateImageRequest::new(
            "agentd-conformance",
            b"\x7fELF fake daemon".to_vec(),
            "s3://bucket/agentd-conformance.zip",
            "arn:aws:iam::123456789012:role/build",
        )
    }

    /// The create request lands on the model's path and method, and its body carries the
    /// model's member names. Asserted through the recorder's generic JSON view, so a
    /// misspelled member here is visible rather than hidden by a matching struct.
    #[tokio::test]
    async fn create_image_emits_the_models_path_method_and_members() {
        let (plane, fake, _) = planted();
        fake.answer(
            "CreateMicrovmImage",
            Answer::created(fake::create_image_response("agentd-conformance")),
        );

        let image = plane.create_image(a_request()).await.expect("creates");
        assert_eq!(image.name, "agentd-conformance");
        assert_eq!(image.state, "CREATING");
        assert_eq!(image.version, "1");

        let calls = fake.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].method, super::super::transport::Method::Post);
        assert_eq!(calls[0].path, "/2025-09-09/microvm-images");

        let body = fake.first_body("CreateMicrovmImage");
        assert_eq!(body["name"], "agentd-conformance");
        assert_eq!(
            body["baseImageArn"],
            "arn:aws:lambda:us-east-1:aws:microvm-image:al2023-1"
        );
        assert_eq!(
            body["codeArtifact"]["uri"],
            "s3://bucket/agentd-conformance.zip"
        );
        assert_eq!(body["cpuConfigurations"][0]["architecture"], "ARM_64");
        assert_eq!(body["resources"][0]["minimumMemoryInMiB"], 2048);
        assert_eq!(body["hooks"]["port"], 9000);
    }

    /// **TRAP-3.** Identity repair sets `additionalOsCapabilities` to exactly `["ALL"]`,
    /// and the field is **absent** when repair was not requested.
    ///
    /// Read off the wire member, which is the assertion that matters: a bool field on the
    /// request type proves nothing about what got emitted.
    ///
    /// **Falsification** — change the injection to a caller-supplied list and there is no
    /// longer a bool to set, so this test does not compile; change it to emit
    /// `["CAP_SYS_ADMIN"]` and the first assertion fails.
    #[tokio::test]
    async fn identity_repair_sets_the_one_accepted_capability_and_nothing_else() {
        let (plane, fake, _) = planted();
        fake.answer(
            "CreateMicrovmImage",
            Answer::created(fake::create_image_response("img")),
        );
        let mut request = a_request();
        request.repair_guest_identity = true;
        plane.create_image(request).await.expect("creates");

        let body = fake.first_body("CreateMicrovmImage");
        assert_eq!(
            body["additionalOsCapabilities"],
            serde_json::json!(["ALL"]),
            "the Capability enum has exactly one value"
        );

        let (plane, fake, _) = planted();
        fake.answer(
            "CreateMicrovmImage",
            Answer::created(fake::create_image_response("img")),
        );
        plane.create_image(a_request()).await.expect("creates");
        let body = fake.first_body("CreateMicrovmImage");
        assert!(
            body.get("additionalOsCapabilities").is_none(),
            "omitted rather than empty when repair was not asked for: {body}"
        );
    }

    /// **TRAP-1, observed on the wire.** Creating the same image name twice emits two
    /// distinct `clientToken` values, as the fake saw them.
    ///
    /// This is the create-delete-recreate case: same name, same bytes, same scope. A token
    /// derived from any of those would repeat and wedge the image.
    ///
    /// **Falsification** — replace the nonce with a digest of the scope and this fails
    /// with two identical tokens.
    #[tokio::test]
    async fn recreating_the_same_image_name_emits_two_distinct_client_tokens() {
        let (plane, fake, _) = planted();
        fake.answer(
            "CreateMicrovmImage",
            Answer::created(fake::create_image_response("img")),
        );

        plane.create_image(a_request()).await.expect("first create");
        plane.create_image(a_request()).await.expect("recreate");

        let tokens = fake.client_tokens();
        assert_eq!(tokens.len(), 2, "both creates carried a token");
        assert_ne!(
            tokens[0], tokens[1],
            "a repeated clientToken wedges the image in CREATING for ~15 hours"
        );
        for token in &tokens {
            assert!(token.starts_with("create-"), "{token}");
            assert!(
                token.len() <= crate::constants::MAX_CLIENT_TOKEN_LEN,
                "{token}"
            );
        }
    }

    /// The token's label follows `token_scope` when one is given — the CloudTrail
    /// readability affordance — and the token is still per-attempt distinct.
    #[tokio::test]
    async fn a_token_scope_labels_the_token_without_becoming_it() {
        let (plane, fake, _) = planted();
        fake.answer(
            "CreateMicrovmImage",
            Answer::created(fake::create_image_response("img")),
        );
        let mut request = a_request();
        request.token_scope = Some("nightly-run-42".to_string());
        plane.create_image(request.clone()).await.expect("creates");
        plane.create_image(request).await.expect("creates again");

        let tokens = fake.client_tokens();
        assert!(tokens[0].contains("nightly-run-42"), "{}", tokens[0]);
        assert_ne!(tokens[0], tokens[1], "the label is not the token");
    }

    /// A name the service would reject never reaches the wire, so the caller does not pay
    /// for the artifact upload first.
    #[tokio::test]
    async fn an_invalid_image_name_is_refused_before_any_call() {
        let (plane, fake, _) = planted();
        let mut request = a_request();
        request.name = "my.image".to_string();

        let error = plane
            .create_image(request)
            .await
            .expect_err("dots are refused");
        assert_eq!(error.kind(), ErrorKind::InvalidArg);
        assert_eq!(fake.calls().len(), 0, "nothing reached the control plane");
    }

    /// A Dockerfile disagreeing with the base image is refused locally too — same reason,
    /// and the check is only run when a Dockerfile was supplied.
    #[tokio::test]
    async fn a_mismatched_dockerfile_is_refused_before_any_call() {
        let (plane, fake, _) = planted();
        let mut request = a_request();
        request.dockerfile = Some("FROM ubuntu:24.04\n".to_string());

        let error = plane.create_image(request).await.expect_err("refused");
        assert_eq!(error.kind(), ErrorKind::InvalidArg);
        assert!(error.to_string().contains("ubuntu:24.04"), "{error}");
        assert_eq!(fake.calls().len(), 0);
    }

    /// **Issue #46 at the create boundary.** A Dockerfile that never runs the daemon — no
    /// `CMD`, or an `ENTRYPOINT` that swallows it — is refused before any call, like every
    /// other agreement guard.
    ///
    /// **Falsification** — run 2026-08-17. Delete the `require_daemon_cmd` call from
    /// `preflight` and both halves go red with the create having reached the fake.
    #[tokio::test]
    async fn a_dockerfile_that_never_runs_the_daemon_is_refused_before_any_call() {
        for dockerfile in [
            // No CMD: the base's default process runs instead of the daemon.
            "FROM public.ecr.aws/amazonlinux/amazonlinux:2023-minimal\nCOPY agentd /agentd\n",
            // A non-empty ENTRYPOINT: the CMD becomes its arguments.
            "FROM public.ecr.aws/amazonlinux/amazonlinux:2023-minimal\n\
             ENTRYPOINT [\"/bin/sh\", \"-c\"]\nCMD [\"/agentd\"]\n",
        ] {
            let (plane, fake, _) = planted();
            let mut request = a_request();
            request.dockerfile = Some(dockerfile.to_string());

            let error = plane.create_image(request).await.expect_err("refused");
            assert_eq!(error.kind(), ErrorKind::InvalidArg, "{dockerfile}");
            assert!(
                error.to_string().contains("run-hook timeout"),
                "the symptom the reader would otherwise chase has to be named: {error}"
            );
            assert_eq!(fake.calls().len(), 0, "nothing reached the control plane");
        }
    }

    /// **Issue #47's contract, stated at the library boundary.** `preflight` and
    /// `create_image` refuse the same request for the same reason, with zero calls — which
    /// is what lets an uploading caller run the list before paying for the upload.
    ///
    /// Asserted over every guard family `preflight` carries, not just the Dockerfile ones,
    /// because the extraction's risk is a guard left behind in `create_image` where the
    /// pre-upload caller never sees it.
    #[tokio::test]
    async fn preflight_and_create_image_refuse_identically_with_zero_calls() {
        type Breakage = Box<dyn Fn(&mut CreateImageRequest)>;
        let broken: [(&str, Breakage); 5] = [
            ("an invalid name", Box::new(|r| r.name = "my.image".into())),
            (
                "a mismatched FROM",
                Box::new(|r| r.dockerfile = Some("FROM ubuntu:24.04\nCMD [\"/agentd\"]\n".into())),
            ),
            (
                "a Dockerfile with no CMD",
                Box::new(|r| {
                    r.dockerfile =
                        Some("FROM public.ecr.aws/amazonlinux/amazonlinux:2023-minimal\n".into());
                }),
            ),
            (
                "a blank base version",
                Box::new(|r| r.base_image_version = Some("  ".into())),
            ),
            (
                "a blank build role",
                Box::new(|r| r.build_role_arn = String::new()),
            ),
        ];

        for (label, break_it) in broken {
            let (plane, fake, _) = planted();
            let mut request = a_request();
            break_it(&mut request);

            let from_preflight = plane.preflight(&request).expect_err(label).to_string();
            let from_create = plane
                .create_image(request)
                .await
                .expect_err(label)
                .to_string();
            assert_eq!(
                from_preflight, from_create,
                "{label}: one list of guards, not two that can drift"
            );
            assert_eq!(fake.calls().len(), 0, "{label}: zero calls either way");
        }

        // And the request every other test builds passes preflight, so the guard list is
        // refusing the breakage rather than the baseline.
        let (plane, _, _) = planted();
        plane.preflight(&a_request()).expect("the baseline passes");
    }

    /// The hook timeouts reach the wire in their own families, so a build-sized value
    /// cannot land in a run-family field.
    #[tokio::test]
    async fn the_two_hook_families_reach_their_own_wire_members() {
        let (plane, fake, _) = planted();
        fake.answer(
            "CreateMicrovmImage",
            Answer::created(fake::create_image_response("img")),
        );
        let mut request = a_request();
        request.run_hook_timeout = RunHookTimeout::try_new(60).expect("the run ceiling");
        request.build_hook_timeout = BuildHookTimeout::try_new(3600).expect("the build ceiling");
        plane.create_image(request).await.expect("creates");

        let body = fake.first_body("CreateMicrovmImage");
        assert_eq!(body["hooks"]["microvmHooks"]["runTimeoutInSeconds"], 60);
        assert_eq!(
            body["hooks"]["microvmImageHooks"]["readyTimeoutInSeconds"],
            3600
        );
    }

    /// The wait returns as soon as the image is usable, and accepts every ready spelling —
    /// including the two tolerated ones, since a hard check on `CREATED` alone is how a
    /// working build looks like a stalled one.
    #[tokio::test]
    async fn the_wait_returns_on_every_ready_spelling() {
        for state in ["CREATED", "UPDATED", "ACTIVE", "AVAILABLE"] {
            let (plane, fake, _) = planted();
            fake.answer(
                "GetMicrovmImage",
                Answer::ok(fake::get_image_response("img", state)),
            );
            let image = plane
                .wait_for_image("arn:image", SizeClass::DEFAULT, WaitOpts::default())
                .await
                .unwrap_or_else(|error| panic!("{state} is usable: {error}"));
            assert_eq!(image.state, state);
            assert_eq!(fake.call_count("GetMicrovmImage"), 1, "no extra polls");
        }
    }

    /// It polls through `CREATING` and returns when the state changes, without the stall
    /// probe firing — the grace period has not elapsed.
    #[tokio::test]
    async fn the_wait_polls_through_creating_without_probing_inside_the_grace() {
        let (plane, fake, _) = planted();
        fake.answer(
            "GetMicrovmImage",
            Answer::ok(fake::get_image_response("img", "CREATING")),
        )
        .answer(
            "GetMicrovmImage",
            Answer::ok(fake::get_image_response("img", "CREATED")),
        );

        let image = plane
            .wait_for_image("arn:image", SizeClass::DEFAULT, WaitOpts::default())
            .await
            .expect("becomes usable");
        assert_eq!(image.state, "CREATED");
        assert_eq!(fake.call_count("GetMicrovmImage"), 2);
        assert_eq!(
            fake.call_count("ListMicrovmImageBuilds"),
            0,
            "one poll interval is well inside the four-minute grace"
        );
    }

    /// **TRAP-2.** Past the grace, in `CREATING`, with every build `PENDING`: the wait is
    /// rejected naming the client-token replay signature.
    ///
    /// The response bodies are literal model-spelled JSON from the fake — `buildState`, not
    /// `state` — which is what makes this guard falsifiable at all.
    ///
    /// **Falsification** — three of them, and all three were run: (a) delete the
    /// `probe_stalled_build` call from the wait and this test times out instead, (b) read
    /// `state` instead of `buildState` and the deserializer refuses the fake's body, (c)
    /// drop the `all PENDING` condition and the polls-through-CREATING test above goes red.
    #[tokio::test]
    async fn a_build_stuck_creating_with_all_builds_pending_names_the_replay_signature() {
        let (plane, fake, _) = planted();
        fake.answer(
            "GetMicrovmImage",
            Answer::ok(fake::get_image_response("img", "CREATING")),
        )
        .answer(
            "ListMicrovmImageVersions",
            Answer::ok(fake::list_versions_response("1")),
        )
        .answer(
            "ListMicrovmImageBuilds",
            Answer::ok(fake::list_builds_response("PENDING")),
        );

        let error = plane
            .wait_for_image(
                "arn:image",
                SizeClass::DEFAULT,
                WaitOpts {
                    stall_grace: Duration::from_secs(240),
                    poll_interval: Duration::from_secs(120),
                    timeout: Duration::from_secs(2700),
                },
            )
            .await
            .expect_err("a wedged image must not be waited out");

        assert_eq!(error.kind(), ErrorKind::BuildWedged);
        assert_eq!(error.code(), "ERR_BUILD_WEDGED");
        let message = error.to_string();
        assert!(
            message.contains("clientToken replay signature"),
            "the message must name the signature: {message}"
        );
        assert!(message.contains("PENDING"), "{message}");
        assert!(
            message.contains("permanent idempotency key"),
            "the cause has to be nameable: {message}"
        );
        assert!(
            message.contains("Waiting will not help"),
            "the remedy is not patience: {message}"
        );
        assert!(message.contains("docs/PLATFORM.md"), "{message}");
    }

    /// A build that is genuinely in progress past the grace is **not** accused. `IN_PROGRESS`
    /// means the service scheduled it, which is exactly what the replay case never does.
    ///
    /// Three `CREATING` answers rather than two, because the probe only runs on a poll that
    /// *sees* `CREATING` after the grace has elapsed: with two answers the second poll
    /// already returns `CREATED` and the probe never fires, so the test would assert nothing
    /// about the probe's verdict. The `ListMicrovmImageBuilds` count at the bottom is what
    /// keeps that honest.
    #[tokio::test]
    async fn a_slow_but_scheduled_build_is_not_accused_of_being_wedged() {
        let (plane, fake, _) = planted();
        fake.answer(
            "GetMicrovmImage",
            Answer::ok(fake::get_image_response("img", "CREATING")),
        )
        .answer(
            "GetMicrovmImage",
            Answer::ok(fake::get_image_response("img", "CREATING")),
        )
        .answer(
            "GetMicrovmImage",
            Answer::ok(fake::get_image_response("img", "CREATED")),
        )
        .answer(
            "ListMicrovmImageVersions",
            Answer::ok(fake::list_versions_response("1")),
        )
        .answer(
            "ListMicrovmImageBuilds",
            Answer::ok(fake::list_builds_response("IN_PROGRESS")),
        );

        let image = plane
            .wait_for_image(
                "arn:image",
                SizeClass::DEFAULT,
                WaitOpts {
                    stall_grace: Duration::from_secs(240),
                    poll_interval: Duration::from_secs(300),
                    timeout: Duration::from_secs(2700),
                },
            )
            .await
            .expect("a scheduled build is allowed to be slow");
        assert_eq!(image.state, "CREATED");
        assert_eq!(
            fake.call_count("ListMicrovmImageBuilds"),
            1,
            "the probe runs once and does not re-arm"
        );
    }

    /// **Issue #23.** One scheduled build on page **two** of the build listing is enough to
    /// clear a healthy image, so a truncated page cannot produce a false wedge verdict.
    ///
    /// `probe_stalled_build` asserts **all** builds are PENDING. Over one page that
    /// quantifier is a claim about a subset presented as a claim about the set: fifty pending
    /// builds on page one plus one `IN_PROGRESS` build on page two is a healthy image that
    /// the old code would have called wedged, and `ERR_BUILD_WEDGED` is an exit code a caller
    /// is told to trust.
    ///
    /// **Falsification** — run 2026-08-15. Stop the loop after the first page in
    /// `builds_of_version` and this fails with `ErrorKind::BuildWedged`, which is exactly the
    /// false verdict issue #23 describes. Restored.
    #[tokio::test]
    async fn a_scheduled_build_on_page_two_prevents_a_false_wedge_verdict() {
        let (plane, fake, _) = planted();
        fake.answer(
            "GetMicrovmImage",
            Answer::ok(fake::get_image_response("img", "CREATING")),
        )
        .answer(
            "GetMicrovmImage",
            Answer::ok(fake::get_image_response("img", "CREATING")),
        )
        .answer(
            "GetMicrovmImage",
            Answer::ok(fake::get_image_response("img", "CREATED")),
        )
        .answer(
            "ListMicrovmImageVersions",
            Answer::ok(fake::list_versions_response("1")),
        )
        // Page one is entirely PENDING, which is the wedge signature over a partial list.
        .answer(
            "ListMicrovmImageBuilds",
            Answer::ok(fake::list_builds_page(
                &[("build-1", "PENDING"), ("build-2", "PENDING")],
                Some("builds-page-2"),
            )),
        )
        // Page two says the service scheduled something, so nothing is wedged.
        .answer(
            "ListMicrovmImageBuilds",
            Answer::ok(fake::list_builds_page(&[("build-3", "IN_PROGRESS")], None)),
        );

        let image = plane
            .wait_for_image(
                "arn:image",
                SizeClass::DEFAULT,
                WaitOpts {
                    stall_grace: Duration::from_secs(240),
                    poll_interval: Duration::from_secs(300),
                    timeout: Duration::from_secs(2700),
                },
            )
            .await
            .expect("a build scheduled on page two is a scheduled build");
        assert_eq!(image.state, "CREATED");
        assert_eq!(
            fake.call_count("ListMicrovmImageBuilds"),
            2,
            "the probe read both pages before deciding"
        );

        let listings: Vec<String> = fake
            .calls()
            .into_iter()
            .filter(|call| call.operation == "ListMicrovmImageBuilds")
            .map(|call| call.path)
            .collect();
        assert!(
            listings[1].contains("nextToken=builds-page-2"),
            "the second request must carry the first page's token: {}",
            listings[1]
        );
    }

    /// The other direction: a wedge that is only visible on page two is still caught. All
    /// builds PENDING **across every page** is the signature, and the page-two builds are
    /// named in the message.
    ///
    /// Without this, a paginating probe that read every page but formed its verdict from the
    /// last one would pass the test above while missing a real wedge.
    #[tokio::test]
    async fn a_wedge_is_still_named_when_the_builds_span_two_pages() {
        let (plane, fake, _) = planted();
        fake.answer(
            "GetMicrovmImage",
            Answer::ok(fake::get_image_response("img", "CREATING")),
        )
        .answer(
            "ListMicrovmImageVersions",
            Answer::ok(fake::list_versions_response("1")),
        )
        .answer(
            "ListMicrovmImageBuilds",
            Answer::ok(fake::list_builds_page(
                &[("build-1", "PENDING")],
                Some("builds-page-2"),
            )),
        )
        .answer(
            "ListMicrovmImageBuilds",
            Answer::ok(fake::list_builds_page(&[("build-2", "PENDING")], None)),
        );

        let error = plane
            .wait_for_image(
                "arn:image",
                SizeClass::DEFAULT,
                WaitOpts {
                    stall_grace: Duration::from_secs(240),
                    poll_interval: Duration::from_secs(120),
                    timeout: Duration::from_secs(2700),
                },
            )
            .await
            .expect_err("a wedged image must not be waited out");

        assert_eq!(error.kind(), ErrorKind::BuildWedged);
        let message = error.to_string();
        assert!(
            message.contains("all 2 builds"),
            "the count must be over every page, not one: {message}"
        );
        assert!(
            message.contains("build-2"),
            "the page-two build must be in the verdict: {message}"
        );
        assert!(message.contains("build-1"), "{message}");
    }

    /// A build list that cannot be read produces **no** wedge claim. Unknown is not
    /// wedged, and a claim made on a throttled call sends the reader after the wrong cause.
    #[tokio::test]
    async fn an_unreadable_build_list_does_not_produce_a_wedge_claim() {
        let (plane, fake, _) = planted();
        fake.answer(
            "GetMicrovmImage",
            Answer::ok(fake::get_image_response("img", "CREATING")),
        )
        .answer(
            "ListMicrovmImageVersions",
            Answer::failure(429, "Rate exceeded"),
        );

        let error = plane
            .wait_for_image(
                "arn:image",
                SizeClass::DEFAULT,
                WaitOpts {
                    stall_grace: Duration::from_secs(240),
                    poll_interval: Duration::from_secs(300),
                    timeout: Duration::from_secs(600),
                },
            )
            .await
            .expect_err("the wait still ends at its deadline");

        assert_eq!(
            error.kind(),
            ErrorKind::Timeout,
            "an honest timeout, not a wedge claim: {error}"
        );
        assert!(!error.to_string().contains("replay signature"), "{error}");
    }

    /// A reported build failure with **no** reason anywhere names the required log-group
    /// prefix, because a failure that reads as "unknown" is most often the wrong IAM prefix
    /// rather than a silent service.
    ///
    /// The listings answer, and answer with no `stateReason`, which is the case that
    /// distinguishes "the service stated no cause" from "we did not look".
    #[tokio::test]
    async fn a_failed_build_with_no_stated_reason_names_the_required_log_group_prefix() {
        let (plane, fake, _) = planted();
        fake.answer(
            "GetMicrovmImage",
            Answer::ok(fake::get_image_response("img", "CREATE_FAILED")),
        )
        .answer(
            "ListMicrovmImageVersions",
            Answer::ok(fake::list_versions_response("1")),
        )
        .answer(
            "ListMicrovmImageBuilds",
            Answer::ok(fake::list_builds_response("FAILED")),
        );

        let error = plane
            .wait_for_image("arn:image", SizeClass::DEFAULT, WaitOpts::default())
            .await
            .expect_err("a failed build is a failure");
        assert_eq!(error.kind(), ErrorKind::Platform);
        let message = error.to_string();
        assert!(
            message.contains("stated no cause"),
            "the absence has to be stated as an absence: {message}"
        );
        assert!(message.contains("/aws/lambda-microvms/img"), "{message}");
        assert!(
            message.contains("/aws/lambda/microvms/*"),
            "the wrong prefix has to be named as wrong: {message}"
        );
        assert!(message.contains("the plausible spelling"), "{message}");
    }

    /// **Issue #25.** A build failure reports the `stateReason` the service already stated,
    /// at both levels: the version's and the failed build's.
    ///
    /// `GetMicrovmImage` structurally cannot carry a reason — the shape has no such member —
    /// so before this the message could only name a log group and guess. The version reason
    /// is the general one ("one or more builds failed") and the build reason is the specific
    /// one, which is why both are surfaced rather than just the nearest.
    ///
    /// The failed build is on **page two** of the build listing, so this doubles as a
    /// pagination guard on the diagnosis path.
    ///
    /// # And now the build's own `GetMicrovmImageBuild`, for the sizes
    ///
    /// The listing's reason is not the whole of what the service will say. `GetMicrovmImageBuild`
    /// adds `snapshotBuild`, and on a real failure it comes back **partial** — measured
    /// 2026-08-16, a `FAILED` build answered `codeInstallSizeInBytes` alone with no memory and
    /// no disk snapshot, which is a build that installed the code and then never produced a
    /// snapshot. That distinguishes "the ready hook never answered" from "the Dockerfile broke
    /// before installing anything", and the listing has no member for it. The generation is in
    /// the line for the same reason: one create fans out per Graviton generation and they can
    /// disagree.
    ///
    /// **Falsification** — (a) Drop the `failure_reasons` call from `build_failure` and the
    /// message loses both reasons: the first two assertions go red. (b) Stop following
    /// `nextToken` in `builds_of_version` (return `Some(builds)` in the `Some(token)` arm) and
    /// only the version reason survives: the `no space left` assertion goes red. (c) Drop
    /// `state_reason` from `MicrovmImageBuildSummaryWire` and it does not compile. (d) Replace
    /// `describe_build_deeply` with `listed.describe()` and the size and generation assertions
    /// go red while the reason ones still pass — which is what makes the extra call earn itself.
    /// All four run.
    #[tokio::test]
    async fn a_failed_build_reports_the_reason_the_service_already_stated() {
        let (plane, fake, _) = planted();
        fake.answer(
            "GetMicrovmImage",
            Answer::ok(fake::get_image_response_failed("img", "CREATE_FAILED", "2")),
        )
        // Version "2" is on page two of the version listing, and it is the one
        // `latestFailedImageVersion` names — so a resolver reading one page finds the wrong
        // version, or none.
        .answer(
            "ListMicrovmImageVersions",
            Answer::ok(fake::list_versions_page(&["1"], Some("versions-page-2"))),
        )
        .answer(
            "ListMicrovmImageVersions",
            Answer::ok(fake::list_versions_response_failed(
                "2",
                "FAILED",
                Some("one or more builds failed"),
            )),
        )
        // The build that carries the specific reason is on page two of the build listing.
        .answer(
            "ListMicrovmImageBuilds",
            Answer::ok(fake::list_builds_page_with_reasons(
                &[("build-1", "SUCCESSFUL", None)],
                Some("builds-page-2"),
            )),
        )
        .answer(
            "ListMicrovmImageBuilds",
            Answer::ok(fake::list_builds_page_with_reasons(
                &[("build-2", "FAILED", Some("no space left on device"))],
                None,
            )),
        )
        // The per-build `GetMicrovmImageBuild`, answering the partial `snapshotBuild` a real
        // failed build carries: code installed, no snapshot produced.
        .answer(
            "GetMicrovmImageBuild",
            Answer::ok(fake::get_image_build_response(
                "build-2",
                "FAILED",
                "4",
                Some("no space left on device"),
                Some(r#"{"codeInstallSizeInBytes": 1724940288}"#),
            )),
        );

        let error = plane
            .wait_for_image("arn:image", SizeClass::DEFAULT, WaitOpts::default())
            .await
            .expect_err("a failed build is a failure");
        assert_eq!(error.kind(), ErrorKind::Platform);
        let message = error.to_string();

        assert!(
            message.contains("one or more builds failed"),
            "the version-level reason must reach the message: {message}"
        );
        assert!(
            message.contains("no space left on device"),
            "the build-level reason is the specific one, and it was on page two: {message}"
        );
        assert!(
            message.contains("build-2"),
            "the buildId is what GetMicrovmImageBuild takes, so it has to be named: {message}"
        );
        // The two things only `GetMicrovmImageBuild` can say. The size is the diagnosis a
        // listing structurally cannot carry, and the generation is which of the fan-out's
        // builds this was.
        assert!(
            message.contains("code 1724940288 bytes"),
            "the snapshot size the listing has no member for must reach the message: {message}"
        );
        assert!(
            message.contains("chipset generation 4"),
            "one create fans out per generation, so the failing one has to be named: {message}"
        );
        assert!(
            !message.contains("memory"),
            "a size the service did not report must not be invented: {message}"
        );
        assert_eq!(
            fake.call_count("GetMicrovmImageBuild"),
            1,
            "one call per failed build carrying a reason, not per listed build"
        );
        assert!(
            message.contains("The service stated the cause"),
            "{message}"
        );
        assert!(
            !message.contains("stated no cause"),
            "a message with reasons must not also claim there were none: {message}"
        );

        assert_eq!(
            fake.call_count("ListMicrovmImageVersions"),
            2,
            "both version pages were read to find the version the service named"
        );
        assert_eq!(
            fake.call_count("ListMicrovmImageBuilds"),
            2,
            "both build pages were read"
        );
    }

    /// A build failure whose listings cannot be read at all still reports the failure, and
    /// still names the log group. A diagnosis lookup must never turn a real build failure
    /// into a listing error.
    #[tokio::test]
    async fn an_unreadable_listing_still_reports_the_build_failure() {
        let (plane, fake, _) = planted();
        fake.answer(
            "GetMicrovmImage",
            Answer::ok(fake::get_image_response("img", "CREATE_FAILED")),
        )
        .answer(
            "ListMicrovmImageVersions",
            Answer::failure(429, "Rate exceeded"),
        );

        let error = plane
            .wait_for_image("arn:image", SizeClass::DEFAULT, WaitOpts::default())
            .await
            .expect_err("the build still failed");
        assert_eq!(
            error.kind(),
            ErrorKind::Platform,
            "a build failure, not a throttle: {error}"
        );
        assert!(error.to_string().contains("CREATE_FAILED"), "{error}");
        assert!(
            error.to_string().contains("/aws/lambda-microvms/img"),
            "{error}"
        );
    }

    /// Every model failure spelling is recognised, so a build that failed is never polled
    /// to the deadline.
    #[test]
    fn every_model_failure_state_is_recognised_as_a_failure() {
        for state in ["CREATE_FAILED", "UPDATE_FAILED", "DELETE_FAILED"] {
            assert!(Image::is_failed(state), "{state}");
            assert!(!Image::is_ready(state), "{state}");
        }
        for state in ["CREATING", "UPDATING", "DELETING", "DELETED"] {
            assert!(!Image::is_failed(state), "{state}");
            assert!(!Image::is_ready(state), "{state}");
        }
    }

    /// The wait ends at its deadline with a `Timeout` naming the last state seen — which is
    /// the difference between "we gave up" and "we gave up while it was still CREATING".
    #[tokio::test]
    async fn the_wait_ends_at_its_deadline_naming_the_last_state() {
        let (plane, fake, _) = planted();
        fake.answer(
            "GetMicrovmImage",
            Answer::ok(fake::get_image_response("img", "CREATING")),
        );

        let error = plane
            .wait_for_image(
                "arn:image",
                SizeClass::DEFAULT,
                WaitOpts {
                    timeout: Duration::from_secs(60),
                    poll_interval: Duration::from_secs(30),
                    stall_grace: Duration::MAX,
                },
            )
            .await
            .expect_err("the deadline elapses");
        assert_eq!(error.kind(), ErrorKind::Timeout);
        let message = error.to_string();
        assert!(message.contains("CREATING"), "{message}");
        assert!(message.contains("nothing was cancelled"), "{message}");
    }

    /// The log group is the measured prefix, not the plausible one.
    #[test]
    fn the_build_log_group_uses_the_measured_prefix() {
        let image = Image {
            identifier: "arn:image".to_string(),
            name: "agentd-conformance".to_string(),
            version: "1".to_string(),
            state: "CREATED".to_string(),
            size: SizeClass::DEFAULT,
        };
        assert_eq!(
            image.build_log_group(),
            "/aws/lambda-microvms/agentd-conformance"
        );
        assert!(
            !image.build_log_group().starts_with("/aws/lambda/microvms"),
            "the wrong prefix produces builds with no logs at all"
        );
    }

    /// Deletion drops every version but the first, then the image. The first is kept
    /// because the last remaining version cannot be deleted alone.
    #[tokio::test]
    async fn deletion_keeps_the_first_version_and_deletes_the_image() {
        let (plane, fake, _) = planted();
        fake.answer(
            "ListMicrovmImageVersions",
            Answer::ok(fake::list_versions_response("1")),
        )
        .answer(
            "DeleteMicrovmImage",
            Answer::ok(fake::delete_image_response()),
        );

        assert!(
            plane
                .delete_image("arn:image", 20, Duration::from_secs(15))
                .await
        );
        assert_eq!(
            fake.call_count("DeleteMicrovmImageVersion"),
            0,
            "one version means nothing to delete separately"
        );
        assert_eq!(fake.call_count("DeleteMicrovmImage"), 1);
    }

    /// **Issue #23, the worst of the three.** Deletion follows `nextToken` and drops the
    /// versions on page **two**, so an image with more versions than one page can hold is
    /// still deletable.
    ///
    /// Reading one page and then deleting the image is not a partial cleanup, it is a
    /// permanent one: page-two versions still exist, the final `DeleteMicrovmImage` conflicts,
    /// the retry loop re-reads the same first page every attempt, and `delete_image` returns
    /// `false` forever — a billing image nothing can delete through this client.
    ///
    /// The assertion is the **delete count**, not the call count: five versions across two
    /// pages means four `DeleteMicrovmImageVersion` calls, and the two on page two are named
    /// individually so a loop that read the second page but deleted from the first would
    /// still fail.
    ///
    /// **Falsification** — run 2026-08-15. Replace the `Some(token) => next_token = ...` arm
    /// with `Some(_) => break` and this fails with 1 deletion instead of 4, and the
    /// `v-4`/`v-5` path assertions go red. Restored.
    #[tokio::test]
    async fn deletion_follows_next_token_and_drops_the_versions_on_page_two() {
        let (plane, fake, _) = planted();
        fake.answer(
            "ListMicrovmImageVersions",
            Answer::ok(fake::list_versions_page(
                &["v-1", "v-2", "v-3"],
                Some("versions-page-2"),
            )),
        )
        .answer(
            "ListMicrovmImageVersions",
            Answer::ok(fake::list_versions_page(&["v-4", "v-5"], None)),
        )
        .answer(
            "DeleteMicrovmImageVersion",
            Answer::ok(fake::empty_response()),
        )
        .answer(
            "DeleteMicrovmImage",
            Answer::ok(fake::delete_image_response()),
        );

        assert!(
            plane
                .delete_image("arn:image", 20, Duration::from_secs(15))
                .await,
            "an image whose versions span two pages must still be deletable"
        );

        // The delete count first, because it is the outcome the bug produced: reading one
        // page deletes one version and leaves two billing. A call count would report the
        // same breakage as "one listing" and bury what that cost.
        assert_eq!(
            fake.call_count("DeleteMicrovmImageVersion"),
            4,
            "five versions, keeping the first: four deletions, two of them from page two"
        );

        // Named individually, because a loop that reads page two and then deletes from page
        // one would still make four calls.
        let deleted: Vec<String> = fake
            .calls()
            .into_iter()
            .filter(|call| call.operation == "DeleteMicrovmImageVersion")
            .map(|call| call.path)
            .collect();
        for version in ["v-2", "v-3", "v-4", "v-5"] {
            assert!(
                deleted.iter().any(|path| path.ends_with(version)),
                "version {version} was never deleted, so it still bills: {deleted:?}"
            );
        }
        assert!(
            !deleted.iter().any(|path| path.ends_with("v-1")),
            "the first version goes with the image: {deleted:?}"
        );
        assert_eq!(fake.call_count("DeleteMicrovmImage"), 1);
        assert_eq!(
            fake.call_count("ListMicrovmImageVersions"),
            2,
            "both pages were read"
        );

        // The second listing request carries the first page's cursor. Without this the loop
        // could re-read page one forever and still satisfy a count of two.
        let listings: Vec<String> = fake
            .calls()
            .into_iter()
            .filter(|call| call.operation == "ListMicrovmImageVersions")
            .map(|call| call.path)
            .collect();
        assert!(
            listings[1].contains("nextToken=versions-page-2"),
            "the second request must carry the first page's token: {}",
            listings[1]
        );
        assert!(
            !listings[0].contains("nextToken"),
            "the first request carries no cursor: {}",
            listings[0]
        );
    }

    /// **Issue #25.** A `DeleteMicrovmImage` that answers 2xx while reporting a `*_FAILED`
    /// state is a failure, not a success.
    ///
    /// That readback is why `DeleteImageResponseWire` is kept rather than deleted as dead:
    /// without it the service can accept the delete request, refuse the work, and have
    /// `delete_image` answer `true` — teardown reports clean and the image keeps billing.
    ///
    /// **Falsification** — run 2026-08-15. Drop the `Image::is_failed` check from
    /// `try_delete_image` and this reports `true` after one attempt: both assertions go red.
    /// Restored.
    #[tokio::test]
    async fn a_delete_that_reads_back_failed_is_not_reported_as_deleted() {
        let (plane, fake, _) = planted();
        fake.answer(
            "ListMicrovmImageVersions",
            Answer::ok(fake::list_versions_response("1")),
        )
        .answer(
            "DeleteMicrovmImage",
            Answer::ok(fake::delete_image_response_in("DELETE_FAILED")),
        );

        assert!(
            !plane
                .delete_image("arn:image", 2, Duration::from_secs(15))
                .await,
            "a DELETE_FAILED readback means the image still exists and still bills"
        );
        assert_eq!(
            fake.call_count("DeleteMicrovmImage"),
            2,
            "every attempt ran rather than the first being taken as success"
        );
    }

    /// `DELETING` and `DELETED` are **both** success. The deletion is asynchronous, so
    /// `DELETING` is the ordinary answer, and treating it as incomplete would re-issue a
    /// delete already in progress and then report failure on the conflict that comes back.
    #[tokio::test]
    async fn both_deleting_and_deleted_read_back_as_success() {
        for state in ["DELETING", "DELETED"] {
            let (plane, fake, _) = planted();
            fake.answer(
                "ListMicrovmImageVersions",
                Answer::ok(fake::list_versions_response("1")),
            )
            .answer(
                "DeleteMicrovmImage",
                Answer::ok(fake::delete_image_response_in(state)),
            );

            assert!(
                plane
                    .delete_image("arn:image", 20, Duration::from_secs(15))
                    .await,
                "{state} is the delete having worked"
            );
            assert_eq!(
                fake.call_count("DeleteMicrovmImage"),
                1,
                "{state} must not be retried"
            );
        }
    }

    /// Deletion retries a conflict — an image in `CREATING` refuses deletion, and a VM
    /// still terminating holds a reference — and gives up without raising, because the
    /// caller is a teardown path.
    #[tokio::test]
    async fn deletion_retries_a_conflict_and_gives_up_without_raising() {
        let (plane, fake, clock) = planted();
        fake.answer(
            "ListMicrovmImageVersions",
            Answer::ok(fake::list_versions_response("1")),
        )
        .answer(
            "DeleteMicrovmImage",
            Answer::failure(409, "image is in CREATING"),
        );

        let deleted = plane
            .delete_image("arn:image", 3, Duration::from_secs(15))
            .await;
        assert!(!deleted, "it reports failure rather than raising");
        assert_eq!(
            fake.call_count("DeleteMicrovmImage"),
            3,
            "every attempt ran"
        );
        assert_eq!(
            clock.now(),
            Duration::from_secs(30),
            "two backoffs between three attempts"
        );
    }

    /// A deletion that succeeds on the second attempt stops there.
    #[tokio::test]
    async fn deletion_stops_as_soon_as_it_succeeds() {
        let (plane, fake, _) = planted();
        fake.answer(
            "ListMicrovmImageVersions",
            Answer::ok(fake::list_versions_response("1")),
        )
        .answer("DeleteMicrovmImage", Answer::failure(409, "still CREATING"))
        .answer(
            "DeleteMicrovmImage",
            Answer::ok(fake::delete_image_response()),
        );

        assert!(
            plane
                .delete_image("arn:image", 20, Duration::from_secs(15))
                .await
        );
        assert_eq!(fake.call_count("DeleteMicrovmImage"), 2);
    }

    /// The artifact bytes are available without a control-plane call, which is what lets the
    /// byte-scan guard inspect them.
    #[test]
    fn the_artifact_is_buildable_without_touching_the_control_plane() {
        let (plane, fake, _) = planted();
        let bytes = plane
            .build_artifact_for(&a_request())
            .expect("builds the zip");
        assert!(!bytes.is_empty());
        assert_eq!(fake.calls().len(), 0);
    }

    // ── name resolution ──────────────────────────────────────────────────────

    /// A bare name resolves to its image's ARN through the listing, and the request
    /// carries the model's `nameFilter` query member.
    ///
    /// The listing here answers with a *substring* superset — `agentd-conformance-old`
    /// beside the exact name — because that is what `nameFilter` really returns, and the
    /// exact-match rule is what this test is for: `contains` and `is` are different
    /// questions, and only `is` is safe to launch from.
    ///
    /// **Falsification** — return the first item regardless of name match (replace the
    /// `item.name == name` comparison with `true`) and this resolves to `...-old`'s ARN:
    /// the equality below goes red. Run 2026-08-14; it failed exactly there and was
    /// restored.
    #[tokio::test]
    async fn a_bare_name_resolves_to_the_exactly_matching_images_arn() {
        let (plane, fake, _) = planted();
        fake.answer(
            "ListMicrovmImages",
            Answer::ok(fake::list_images_response(
                &["agentd-conformance-old", "agentd-conformance"],
                None,
            )),
        );

        let arn = plane
            .resolve_image_arn("agentd-conformance")
            .await
            .expect("resolves");
        assert_eq!(
            arn, "arn:aws:lambda:us-east-1:123456789012:microvm-image:agentd-conformance",
            "the exact match wins, not the first substring hit"
        );

        let calls = fake.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].method, super::super::transport::Method::Get);
        assert_eq!(
            calls[0].path,
            "/2025-09-09/microvm-images?nameFilter=agentd-conformance"
        );
    }

    /// An identifier already shaped like an ARN passes through untouched, with **zero**
    /// listing calls — the caller who holds the ARN pays nothing.
    #[tokio::test]
    async fn an_arn_identifier_passes_through_with_no_listing_call() {
        let (plane, fake, _) = planted();
        let arn = "arn:aws:lambda:us-east-1:123456789012:microvm-image:img";
        let resolved = plane.resolve_image_arn(arn).await.expect("passes through");
        assert_eq!(resolved, arn);
        assert_eq!(
            fake.call_count("ListMicrovmImages"),
            0,
            "an ARN must cost zero extra calls"
        );
        assert_eq!(fake.calls().len(), 0);
    }

    /// Zero matches is a local `Precondition` error naming the name and the remedy —
    /// not the service's "Malformed ARN", which says nothing about names.
    #[tokio::test]
    async fn an_unknown_name_is_a_precondition_error_naming_the_name_and_the_remedy() {
        let (plane, fake, _) = planted();
        fake.answer(
            "ListMicrovmImages",
            Answer::ok(fake::list_images_response(&[], None)),
        );

        let error = plane
            .resolve_image_arn("no-such-image")
            .await
            .expect_err("nothing to resolve");
        assert_eq!(error.kind(), ErrorKind::Precondition);
        assert_eq!(error.code(), "ERR_PRECONDITION");
        let message = error.to_string();
        assert!(message.contains("no-such-image"), "{message}");
        assert!(message.contains("microvm build"), "{message}");
        assert!(
            message.contains("last page"),
            "the message must say the whole listing was read: {message}"
        );
    }

    /// Resolution follows `nextToken` across pages: an image on page two is found, and
    /// the second request carries the first page's token.
    ///
    /// **Falsification** — stop the loop after the first page (replace the
    /// `Some(token)` arm with `return Ok(None)`) and this reports the name missing.
    /// Run 2026-08-14; it failed exactly there and was restored.
    #[tokio::test]
    async fn resolution_follows_next_token_to_an_image_on_the_second_page() {
        let (plane, fake, _) = planted();
        fake.answer(
            "ListMicrovmImages",
            Answer::ok(fake::list_images_response(
                &["some-other-image"],
                Some("page-2-token"),
            )),
        )
        .answer(
            "ListMicrovmImages",
            Answer::ok(fake::list_images_response(&["wanted-image"], None)),
        );

        let arn = plane
            .resolve_image_arn("wanted-image")
            .await
            .expect("found on page two");
        assert_eq!(
            arn,
            "arn:aws:lambda:us-east-1:123456789012:microvm-image:wanted-image"
        );
        assert_eq!(fake.call_count("ListMicrovmImages"), 2, "both pages read");

        let paths = fake.paths();
        assert!(
            paths[1].contains("nextToken=page-2-token"),
            "the second request must carry the first page's token: {}",
            paths[1]
        );
        assert!(
            paths[0].contains("nameFilter=wanted-image") && !paths[0].contains("nextToken"),
            "the first request has the filter and no token: {}",
            paths[0]
        );
    }

    // ── the four new operations ──────────────────────────────────────────────

    /// **A real build failure carries no version-level reason, and the build's line is the
    /// whole diagnosis.**
    ///
    /// Measured 2026-08-16 against a deliberately failing build (`RUN … && exit 42`, image
    /// `microvm-cli-cpc-fail`): `GetMicrovmImageVersion` answered `state: FAILED, status:
    /// INACTIVE` with **no `stateReason` member at all**, while both of the version's builds
    /// carried `The container image build failed.` That confirms the entry in
    /// docs/PLATFORM.md rather than merely restating it, and it is the case that decides how
    /// the diagnosis has to be assembled: a message built only from the version's reason would
    /// say nothing on a real failure.
    ///
    /// The fake's version here therefore carries **no** reason, unlike the sibling test above
    /// — so the message must still name the cause, and it can only get it from the builds.
    ///
    /// **Falsification** — run 2026-08-16. Drop the build-level extension from
    /// `failure_reasons` (keep only the version's reason) and the `stated no cause` assertion
    /// goes red, because with no version reason there is nothing left.
    #[tokio::test]
    async fn a_failure_with_no_version_reason_still_names_the_cause_from_the_builds() {
        let (plane, fake, _) = planted();
        fake.answer(
            "GetMicrovmImage",
            Answer::ok(fake::get_image_response_failed(
                "img",
                "CREATE_FAILED",
                "1.0",
            )),
        )
        // `FAILED` / `INACTIVE` with no `stateReason` — the shape a real failure has.
        .answer(
            "ListMicrovmImageVersions",
            Answer::ok(format!(
                r#"{{"items": [{}]}}"#,
                fake::get_image_version_response("1.0", "FAILED", "INACTIVE")
            )),
        )
        // Both builds of the fan-out failed with the same reason, which is what a real one does.
        .answer(
            "ListMicrovmImageBuilds",
            Answer::ok(fake::list_builds_page_with_reasons(
                &[
                    (
                        "ad8dc894-df5e-499c-800c-89711db15f21",
                        "FAILED",
                        Some("The container image build failed."),
                    ),
                    (
                        "fbf3cc24-baf7-4498-8f05-13de7c34f2a4",
                        "FAILED",
                        Some("The container image build failed."),
                    ),
                ],
                None,
            )),
        )
        // And `GetMicrovmImageBuild` on a real *container-build* failure carries **no**
        // `snapshotBuild` at all — measured 2026-08-16. So the line has the reason and the
        // generation and no sizes, which the renderer must handle without a dangling clause.
        .answer(
            "GetMicrovmImageBuild",
            Answer::ok(fake::get_image_build_response(
                "ad8dc894-df5e-499c-800c-89711db15f21",
                "FAILED",
                "4",
                Some("The container image build failed."),
                None,
            )),
        )
        .answer(
            "GetMicrovmImageBuild",
            Answer::ok(fake::get_image_build_response(
                "fbf3cc24-baf7-4498-8f05-13de7c34f2a4",
                "FAILED",
                "3",
                Some("The container image build failed."),
                None,
            )),
        );

        let error = plane
            .wait_for_image("arn:image", SizeClass::DEFAULT, WaitOpts::default())
            .await
            .expect_err("a failed build is a failure");
        let message = error.to_string();
        assert!(
            message.contains("The service stated the cause"),
            "the version carried no reason, so the builds have to supply it: {message}"
        );
        assert!(
            !message.contains("stated no cause"),
            "a message with build reasons must not claim there were none: {message}"
        );
        assert!(
            message.contains("The container image build failed."),
            "{message}"
        );
        // Both generations named, because one create fans out into one build per generation
        // and a report naming only one hides a partial failure.
        assert!(message.contains("chipset generation 4"), "{message}");
        assert!(message.contains("chipset generation 3"), "{message}");
        assert!(
            !message.contains("bytes"),
            "a container-build failure reports no snapshotBuild, so no size may be invented: \
             {message}"
        );
        assert_eq!(
            fake.call_count("GetMicrovmImageBuild"),
            2,
            "one per failed build, which is the fan-out's width"
        );
    }

    /// A version-level reason, when the service does give one, still reaches the message —
    /// and the version's `status` is read alongside its `state`.
    ///
    /// The pair with the test above: this is the case docs/PLATFORM.md records as *not* what a
    /// real failure looks like, kept because a diagnosis that only worked when the version was
    /// silent would break the moment AWS started populating the member.
    #[tokio::test]
    async fn a_version_level_reason_is_used_when_the_service_does_give_one() {
        let (plane, fake, _) = planted();
        fake.answer(
            "GetMicrovmImage",
            Answer::ok(fake::get_image_response_failed(
                "img",
                "CREATE_FAILED",
                "2.0",
            )),
        )
        .answer(
            "ListMicrovmImageVersions",
            Answer::ok(format!(
                r#"{{"items": [{}]}}"#,
                fake::get_image_version_response_with_reason(
                    "2.0",
                    "FAILED",
                    "INACTIVE",
                    "one or more builds failed",
                )
            )),
        )
        .answer(
            "ListMicrovmImageBuilds",
            Answer::ok(fake::list_builds_page_with_reasons(
                &[("build-1", "FAILED", Some("no space left on device"))],
                None,
            )),
        )
        .answer(
            "GetMicrovmImageBuild",
            Answer::ok(fake::get_image_build_response(
                "build-1",
                "FAILED",
                "4",
                Some("no space left on device"),
                Some(r#"{"codeInstallSizeInBytes": 1724940288}"#),
            )),
        );

        let error = plane
            .wait_for_image("arn:image", SizeClass::DEFAULT, WaitOpts::default())
            .await
            .expect_err("a failed build is a failure");
        let message = error.to_string();
        assert!(
            message.contains("version 2.0 is FAILED because one or more builds failed"),
            "the version's own reason has to survive: {message}"
        );
        assert!(
            message.contains("no space left on device"),
            "the build's reason is the specific one: {message}"
        );
        assert!(
            message.contains("code 1724940288 bytes"),
            "and the size only GetMicrovmImageBuild has: {message}"
        );
    }

    /// `GetMicrovmImageBuild` lands on the model's path and method, and answers with the
    /// snapshot sizes.
    ///
    /// The path is asserted as a literal rather than built from the helper, because the whole
    /// content of the change is one extra segment and a helper shared with the code under test
    /// would agree with a wrong helper.
    ///
    /// **Falsification** — run 2026-08-16. Change `paths::image_build` to append `/build/`
    /// instead of `/builds/` and the path assertion goes red; the request would otherwise
    /// succeed against a fake that keys on the operation name.
    #[tokio::test]
    async fn getting_one_build_emits_the_models_path_and_reads_the_snapshot_sizes() {
        let (plane, fake, _) = planted();
        fake.answer(
            "GetMicrovmImageBuild",
            Answer::ok(fake::get_image_build_response(
                "build-abc",
                "SUCCESSFUL",
                "4",
                None,
                Some(
                    r#"{"memorySnapshotSizeInBytes": 582238208,
                        "codeInstallSizeInBytes": 2355486720,
                        "diskSnapshotSizeInBytes": 23760896}"#,
                ),
            )),
        );

        let build = plane
            .get_image_build("arn:image", "1.0", "build-abc")
            .await
            .expect("reads");
        assert_eq!(build.build_state, "SUCCESSFUL");
        assert_eq!(build.chipset_generation, "4");
        let sizes = build.snapshot_build.expect("the sizes are why we called");
        assert_eq!(sizes.memory_snapshot_size_in_bytes, Some(582_238_208));

        let calls = fake.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].method, super::super::transport::Method::Get);
        assert_eq!(
            calls[0].path,
            "/2025-09-09/microvm-images/arn%3Aimage/versions/1.0/builds/build-abc"
        );
        assert_eq!(calls[0].body, None, "a GET carries no body");
    }

    /// `GetMicrovmImageVersion` lands on the version path with a `GET`, and reads the
    /// availability status.
    ///
    /// The same path a `DELETE` of the version uses, which is the model's own arrangement —
    /// so the assertion that matters is the **method**: a `DELETE` here would destroy the
    /// version this call exists to inspect.
    ///
    /// **Falsification** — run 2026-08-16. Build the call with `Call::delete` and the method
    /// assertion goes red while the path assertion still passes, which is the whole reason both
    /// are here.
    #[tokio::test]
    async fn getting_one_version_reads_it_with_a_get_on_the_delete_path() {
        let (plane, fake, _) = planted();
        fake.answer(
            "GetMicrovmImageVersion",
            Answer::ok(fake::get_image_version_response(
                "2.0",
                "SUCCESSFUL",
                "INACTIVE",
            )),
        );

        let version = plane
            .get_image_version("arn:image", "2.0")
            .await
            .expect("reads");
        assert_eq!(version.image_version, "2.0");
        assert_eq!(version.status, "INACTIVE");
        assert!(!version.is_active(), "INACTIVE will not launch");

        let calls = fake.calls();
        assert_eq!(
            calls[0].method,
            super::super::transport::Method::Get,
            "a DELETE on this path destroys the version this call inspects"
        );
        assert_eq!(
            calls[0].path,
            "/2025-09-09/microvm-images/arn%3Aimage/versions/2.0"
        );
    }

    /// **`UpdateMicrovmImageVersion` is a `PATCH` whose body is one member.**
    ///
    /// The method, the path, and the body's exact key set, because each is a separate way to
    /// get the model's only non-destructive retire wrong: a `POST` reaches a route the service
    /// does not declare, a path built from the listing carries a cursor, and a body echoing the
    /// URI parameters sends members the shape does not have.
    ///
    /// **Falsification** — run 2026-08-16, three ways. (a) `Call::post_json` instead of
    /// `patch_json`: the method assertion goes red. (b) Add `imageVersion` to
    /// `UpdateImageVersionWire`: the exact-key-set assertion goes red. (c) Send
    /// `VersionStatus::Active` while asking for `Inactive`: the body assertion goes red *and*
    /// the readback check in `set_image_version_status` raises, which is the next test.
    #[tokio::test]
    async fn retiring_a_version_patches_one_member_onto_the_version_path() {
        let (plane, fake, _) = planted();
        fake.answer(
            "UpdateMicrovmImageVersion",
            Answer::ok(fake::get_image_version_response(
                "2.0",
                "SUCCESSFUL",
                "INACTIVE",
            )),
        );

        let updated = plane
            .set_image_version_status("arn:image", "2.0", ops::VersionStatus::Inactive)
            .await
            .expect("the retire is accepted");
        assert_eq!(updated.status, "INACTIVE");
        assert!(!updated.is_active());
        assert_eq!(
            updated.state, "SUCCESSFUL",
            "a retire does not change the build state: the version still built fine, it is \
             just no longer launchable"
        );

        let calls = fake.calls();
        assert_eq!(calls[0].method, super::super::transport::Method::Patch);
        assert_eq!(
            calls[0].path,
            "/2025-09-09/microvm-images/arn%3Aimage/versions/2.0"
        );
        assert!(
            !calls[0].path.contains('?'),
            "the version path must carry no query member: {}",
            calls[0].path
        );

        let body = fake.first_body("UpdateMicrovmImageVersion");
        assert_eq!(body["status"], "INACTIVE");
        assert_eq!(
            body.as_object()
                .expect("an object")
                .keys()
                .collect::<Vec<_>>(),
            vec!["status"],
            "imageIdentifier and imageVersion are uri-located: {body}"
        );
    }

    /// **A 2xx whose readback disagrees with the request is a failure.**
    ///
    /// The same argument `ops::DeleteImageResponseWire` makes: a request the service accepts
    /// and does not apply is indistinguishable from one it applied, and here the consequence is
    /// a version the caller believes is retired while `RunMicrovm` still launches it. The
    /// readback is the only thing that can tell the two apart.
    ///
    /// **Falsification** — run 2026-08-16. Drop the `updated.status != status.as_str()` check
    /// from `set_image_version_status` and this reports `Ok` with an `ACTIVE` version, which is
    /// exactly the silent failure it exists to prevent.
    #[tokio::test]
    async fn a_retire_that_reads_back_active_is_reported_as_a_failure() {
        let (plane, fake, _) = planted();
        fake.answer(
            "UpdateMicrovmImageVersion",
            // A 200 whose body still says ACTIVE.
            Answer::ok(fake::get_image_version_response(
                "2.0",
                "SUCCESSFUL",
                "ACTIVE",
            )),
        );

        let error = plane
            .set_image_version_status("arn:image", "2.0", ops::VersionStatus::Inactive)
            .await
            .expect_err("a version that is still ACTIVE is one RunMicrovm will still launch");
        assert_eq!(error.kind(), ErrorKind::Platform);
        let message = error.to_string();
        assert!(message.contains("read back status ACTIVE"), "{message}");
        assert!(message.contains("INACTIVE"), "{message}");
        assert!(
            message.contains("RunMicrovm will still launch"),
            "the consequence has to be nameable: {message}"
        );
        assert_eq!(fake.call_count("UpdateMicrovmImageVersion"), 1);
    }

    /// A blank or whitespace-bearing version is refused **before** the call, on both the
    /// retire path and the launch path.
    ///
    /// The `Version` shape is `min 1, max 2048, pattern [^\s]+`, and issue #24 lists
    /// `NonBlankString` as the model's most-reused unguarded shape. The refusal is local
    /// because a retire is what someone does while rolling back, and a `ValidationException`
    /// about the request rather than about the version is the least useful failure available at
    /// that moment.
    ///
    /// **Falsification** — run 2026-08-16. Delete the `require_valid_version` call from
    /// `set_image_version_status` and the zero-call assertion goes red: the fake records a
    /// `PATCH` to `/versions/` with an empty segment, which addresses nothing.
    #[tokio::test]
    async fn a_blank_or_whitespace_version_reaches_no_control_plane_call() {
        for bad in ["", " ", "2.0\n", "a b", "\t2.0"] {
            let (plane, fake, _) = planted();
            let error = plane
                .set_image_version_status("arn:image", bad, ops::VersionStatus::Inactive)
                .await
                .expect_err(&format!("{bad:?} is not a legal Version"));
            assert_eq!(error.kind(), ErrorKind::InvalidArg, "{bad:?}: {error}");
            assert_eq!(
                fake.calls().len(),
                0,
                "{bad:?} must reach no control-plane call at all"
            );
        }
    }

    /// The version guard's three cases each get their own message, and none reaches the wire.
    #[tokio::test]
    async fn each_invalid_version_is_refused_locally_with_its_own_message() {
        let (plane, fake, _) = planted();

        let empty = plane
            .set_image_version_status("arn:image", "", ops::VersionStatus::Inactive)
            .await
            .expect_err("min is 1");
        assert_eq!(empty.kind(), ErrorKind::InvalidArg);
        assert!(
            empty.to_string().contains("Omit the member entirely"),
            "an absent version and a blank one are different requests: {empty}"
        );

        let newline = plane
            .set_image_version_status("arn:image", "2.0\n", ops::VersionStatus::Inactive)
            .await
            .expect_err("the pattern forbids whitespace anywhere");
        let message = newline.to_string();
        assert!(message.contains("whitespace"), "{message}");
        assert!(
            message.contains("trailing newline"),
            "the plausible cause has to be named: {message}"
        );

        let long = plane
            .set_image_version_status("arn:image", &"9".repeat(2049), ops::VersionStatus::Inactive)
            .await
            .expect_err("max is 2048");
        assert!(long.to_string().contains("2049 characters"), "{long}");

        assert_eq!(
            fake.calls().len(),
            0,
            "no invalid version reaches the control plane"
        );

        // And a legal one does reach it, so the guard is a comparison rather than a blanket
        // refusal.
        fake.answer(
            "UpdateMicrovmImageVersion",
            Answer::ok(fake::get_image_version_response(
                "2.0",
                "SUCCESSFUL",
                "INACTIVE",
            )),
        );
        plane
            .set_image_version_status("arn:image", "2.0", ops::VersionStatus::Inactive)
            .await
            .expect("2.0 is a legal Version");
        assert_eq!(fake.call_count("UpdateMicrovmImageVersion"), 1);
    }

    /// **`ListManagedMicrovmImageVersions` reads every page**, and the second request carries
    /// the first page's cursor.
    ///
    /// Every page for the reason the other listings give: a caller pinning "the newest" from a
    /// truncated page pins the wrong one, and a base version is what a build's reproducibility
    /// rests on.
    ///
    /// **Falsification** — run 2026-08-16. Replace the `Some(token)` arm with
    /// `None => return Ok(items)` semantics (stop after page one) and this fails with 2 versions
    /// instead of 3, and the page-two assertion goes red.
    #[tokio::test]
    async fn the_managed_version_listing_follows_next_token_to_the_last_page() {
        let (plane, fake, _) = planted();
        fake.answer(
            "ListManagedMicrovmImageVersions",
            Answer::ok(fake::list_managed_versions_page(
                &["2", "1"],
                Some("managed-page-2"),
            )),
        )
        .answer(
            "ListManagedMicrovmImageVersions",
            Answer::ok(fake::list_managed_versions_page(&["0"], None)),
        );

        let versions = plane
            .managed_base_versions("arn:aws:lambda:us-east-1:aws:microvm-image:al2023-1")
            .await
            .expect("lists");
        assert_eq!(versions.len(), 3, "three versions across two pages");
        let ids: Vec<&str> = versions
            .iter()
            .map(|version| version.image_version.as_str())
            .collect();
        assert_eq!(ids, ["2", "1", "0"]);
        assert!(
            ids.contains(&"0"),
            "the page-two version is the oldest and would be the one a truncated read missed"
        );

        let paths: Vec<String> = fake
            .calls()
            .into_iter()
            .filter(|call| call.operation == "ListManagedMicrovmImageVersions")
            .map(|call| call.path)
            .collect();
        assert_eq!(paths.len(), 2);
        assert!(
            paths[0].starts_with("/2025-09-09/managed-microvm-images/"),
            "the managed collection is its own route, not a filter on microvm-images: {}",
            paths[0]
        );
        assert!(!paths[0].contains('?'), "{}", paths[0]);
        assert!(
            paths[1].contains("nextToken=managed-page-2"),
            "the second request must carry the first page's token: {}",
            paths[1]
        );
    }

    /// A bare base-image name is refused **before** the call, naming the ARN the service wants.
    ///
    /// Measured 2026-08-16: `ListManagedMicrovmImageVersions --image-identifier al2023-1`
    /// answers `ValidationException: Invalid ARN format: al2023-1`, which names the value
    /// without saying which member wanted an ARN or that `BaseImage::arn` produces one. The
    /// local refusal says both.
    ///
    /// **Falsification** — run 2026-08-16. Delete the `starts_with("arn:")` guard and the
    /// zero-call assertion goes red: the request goes out and the service answers the message
    /// above.
    #[tokio::test]
    async fn a_bare_managed_base_name_is_refused_before_the_call_naming_the_arn() {
        let (plane, fake, _) = planted();
        let error = plane
            .managed_base_versions("al2023-1")
            .await
            .expect_err("the service wants a full ARN");
        assert_eq!(error.kind(), ErrorKind::Precondition);
        let message = error.to_string();
        assert!(message.contains("Invalid ARN format"), "{message}");
        assert!(
            message.contains("arn:aws:lambda:us-east-1:aws:microvm-image:al2023-1"),
            "the remedy has to be the ARN itself: {message}"
        );
        assert!(message.contains("BaseImage::al2023().arn"), "{message}");
        assert_eq!(fake.calls().len(), 0, "nothing reached the control plane");

        // And the ARN form does reach it.
        fake.answer(
            "ListManagedMicrovmImageVersions",
            Answer::ok(fake::list_managed_versions_page(&["1", "0"], None)),
        );
        let versions = plane
            .managed_base_versions(
                &super::super::BaseImage::al2023().arn(&crate::region::Region::UsEast1),
            )
            .await
            .expect("an ARN is accepted");
        assert_eq!(versions.len(), 2);
    }

    /// `ListManagedMicrovmImages` reads every page too, and its items carry no registry
    /// reference — which is why they are informational only.
    ///
    /// The absence is asserted through [`super::super::BaseImage`]: a discovered base has an
    /// ARN and nothing that could fill `docker_ref` or `working_dir`, so
    /// `require_matching_from` and `require_workdir` would both have to be skipped for one.
    /// That is the comment in `ops::ManagedMicrovmImageSummaryWire`, made a test.
    #[tokio::test]
    async fn the_managed_image_listing_pages_and_its_items_cannot_build_a_base_image() {
        let (plane, fake, _) = planted();
        fake.answer(
            "ListManagedMicrovmImages",
            Answer::ok(fake::list_managed_images_page(
                &["al2023-1"],
                Some("managed-images-page-2"),
            )),
        )
        .answer(
            "ListManagedMicrovmImages",
            Answer::ok(fake::list_managed_images_page(&["al2024-1"], None)),
        );

        let images = plane.managed_base_images().await.expect("lists");
        assert_eq!(images.len(), 2, "both pages were read");
        assert!(
            images[1].image_arn.ends_with("al2024-1"),
            "the page-two base is the one a caller checking for something new would want: {}",
            images[1].image_arn
        );

        // No registry reference anywhere in what the service sent, so the pairing a
        // `BaseImage` needs is not derivable from a discovered base.
        let known = super::super::BaseImage::al2023();
        for image in &images {
            assert!(
                !image.image_arn.contains("public.ecr.aws"),
                "the ARN carries no registry ref: {}",
                image.image_arn
            );
        }
        assert_eq!(
            known.docker_ref, "public.ecr.aws/amazonlinux/amazonlinux:2023-minimal",
            "the paired ref is a compile-time constant precisely because discovery cannot \
             supply it"
        );

        let paths = fake.paths();
        assert_eq!(paths[0], "/2025-09-09/managed-microvm-images");
        assert!(
            paths[1].contains("nextToken=managed-images-page-2"),
            "{}",
            paths[1]
        );
    }

    /// **`CreateMicrovmImage.baseImageVersion` reaches the wire when pinned**, and is absent
    /// when it is not.
    ///
    /// Read off the emitted body rather than off the request struct, which is the assertion
    /// that matters: a field on `CreateImageRequest` proves nothing about what got sent, and
    /// this member had a field-shaped hole in the wire struct until now.
    ///
    /// **Falsification** — run 2026-08-16. Drop the assignment in `create_image` and the
    /// pinned assertion goes red with `baseImageVersion` absent from the body.
    #[tokio::test]
    async fn a_pinned_base_image_version_reaches_the_create_body() {
        let (plane, fake, _) = planted();
        fake.answer(
            "CreateMicrovmImage",
            Answer::created(fake::create_image_response("img")),
        );
        let mut request = a_request();
        request.base_image_version = Some("1".to_string());
        plane.create_image(request).await.expect("creates");

        let body = fake.first_body("CreateMicrovmImage");
        assert_eq!(
            body["baseImageVersion"], "1",
            "the pinned base version has to reach the wire, or a build still floats: {body}"
        );
        // The ARN and the version are both sent: pinning does not replace the base.
        assert_eq!(
            body["baseImageArn"],
            "arn:aws:lambda:us-east-1:aws:microvm-image:al2023-1"
        );

        // Unpinned emits nothing, so a caller who never touches the field sends what this
        // client always sent.
        let (plane, fake, _) = planted();
        fake.answer(
            "CreateMicrovmImage",
            Answer::created(fake::create_image_response("img")),
        );
        plane.create_image(a_request()).await.expect("creates");
        assert!(
            fake.first_body("CreateMicrovmImage")
                .get("baseImageVersion")
                .is_none(),
            "an absent member takes the service default; a blank one is refused"
        );
    }

    /// An invalid `baseImageVersion` is refused **before** the artifact reaches the wire.
    ///
    /// The same ordering argument `require_valid_image_name` makes and the reason this guard is
    /// local: the create call happens after the caller's artifact upload, so the service's
    /// rejection costs them the upload first.
    ///
    /// **Falsification** — run 2026-08-16. Delete the `require_valid_version` call from
    /// `create_image` and the zero-call assertion goes red.
    #[tokio::test]
    async fn an_invalid_base_image_version_is_refused_before_any_call() {
        let (plane, fake, _) = planted();
        let mut request = a_request();
        request.base_image_version = Some("1 ".to_string());

        let error = plane
            .create_image(request)
            .await
            .expect_err("the Version pattern forbids whitespace");
        assert_eq!(error.kind(), ErrorKind::InvalidArg);
        assert!(error.to_string().contains("baseImageVersion"), "{error}");
        assert_eq!(
            fake.calls().len(),
            0,
            "nothing reached the control plane, so the caller did not pay for the upload first"
        );
    }

    /// The reuse hash derives the same Dockerfile a build of the request would, so the
    /// two agree — and a request whose Dockerfile differs hashes differently.
    #[test]
    fn the_reuse_hash_matches_the_build_paths_own_dockerfile_derivation() {
        let (plane, fake, _) = planted();
        let request = a_request();
        let hash = plane.artifact_content_hash_for(&request);
        assert_eq!(hash.len(), 64);
        assert_eq!(
            hash,
            plane.artifact_content_hash_for(&request),
            "deterministic"
        );

        let mut custom = a_request();
        custom.dockerfile = Some(artifact::default_dockerfile(
            9000,
            Some("/workspace"),
            &custom.base_image,
        ));
        assert_ne!(
            hash,
            plane.artifact_content_hash_for(&custom),
            "a different Dockerfile is a different image identity"
        );
        assert_eq!(fake.calls().len(), 0, "hashing is local");
    }

    // ── issue #24's guards on the create path, proved by the call count ──────

    /// **Every constrained member of `CreateMicrovmImage` is refused with zero calls.**
    ///
    /// One table rather than five tests, because the assertion is identical for each and the
    /// thing worth reading is the *list*: these are the members issue #24 named as reachable with
    /// no guard, and the create call is the one that happens after the caller's artifact upload.
    /// A row here is a member whose rejection no longer costs an upload.
    ///
    /// The zero-call assertion is the load-bearing one. `expect_err` alone would pass for a
    /// request the *service* refused, which is what was happening before; `fake.calls().len() ==
    /// 0` is what says the refusal was local.
    ///
    /// **Guard proof** — run 2026-08-16, one row at a time. Delete
    /// `require_valid_role_arn("buildRoleArn", …)` from `create_image` and the buildRoleArn rows
    /// go red on the call count (the fake answers `CreateMicrovmImage`, so the launch *succeeds*
    /// and the count is 1 rather than 0). Same for `require_non_blank` and the two URI rows,
    /// `require_valid_tags` and the four tag rows, and `require_valid_port` and the port row.
    #[tokio::test]
    async fn every_constrained_create_member_is_refused_with_zero_control_plane_calls() {
        /// What to break, and the substring the message must carry.
        type Mutate = fn(&mut CreateImageRequest);
        let rows: [(&str, Mutate, &str); 12] = [
            // `buildRoleArn` — the member issue #24 called out, because its rejection lands
            // after the upload.
            //
            // Two bare-name rows on either side of the 20-character minimum, which is where the
            // guard's two messages divide: a short name gets the "you probably meant this ARN"
            // one, and a *long* bare name — which the account's real role names are, at 26
            // characters — falls through to the pattern message instead. Both are the same
            // mistake, so both are covered.
            (
                "a short build role name",
                |request| {
                    request.build_role_arn = "build-role".to_string();
                },
                "role *name*",
            ),
            (
                "a long build role name, past the 20-character floor",
                |request| {
                    request.build_role_arn = "bonk-sandbox-microvm-build".to_string();
                },
                "a role name passed as an ARN",
            ),
            (
                "a build role with eleven account digits",
                |request| {
                    request.build_role_arn = "arn:aws:iam::12345678901:role/build".to_string();
                },
                "exactly twelve digits",
            ),
            (
                "a build role that is a function ARN",
                |request| {
                    request.build_role_arn =
                        "arn:aws:lambda:us-east-1:123456789012:function:handler".to_string();
                },
                "RoleArn pattern",
            ),
            (
                "an empty build role",
                |request| {
                    request.build_role_arn = String::new();
                },
                "RoleArn minimum",
            ),
            // `codeArtifact.uri` — `NonBlankString`.
            (
                "a blank artifact URI",
                |request| {
                    request.code_artifact_uri = String::new();
                },
                "codeArtifact.uri is empty",
            ),
            (
                "an artifact URI with a trailing newline",
                |request| {
                    request.code_artifact_uri = "s3://bucket/agentd.zip\n".to_string();
                },
                "contains whitespace",
            ),
            // `baseImageArn` — derived from `BaseImage` and the region, and `BaseImage`'s fields
            // are `pub`. Whitespace rather than an empty name, and the difference is the point:
            // an *empty* name still renders `arn:aws:lambda:us-east-1:aws:microvm-image:`, which
            // is non-blank, so `require_non_blank`'s min-1 branch is structurally unreachable for
            // this member. The whitespace branch is the one that fires, and it fires on the case
            // that actually happens — a base image name with a space in it, from a config file or
            // a copy-paste.
            (
                "a base image name with a space in it",
                |request| {
                    request.base_image.name = "al2023 1".to_string();
                },
                "baseImageArn",
            ),
            // `tags` — sent since tags existed, checked by nothing.
            (
                "an empty tag key",
                |request| {
                    request
                        .tags
                        .insert(String::new(), "conformance".to_string());
                },
                "TagKey requires at least 1 character",
            ),
            (
                "a 129-character tag key",
                |request| {
                    request.tags.insert("k".repeat(129), "v".to_string());
                },
                "over the TagKey ceiling",
            ),
            (
                "a tag key with a comma",
                |request| {
                    request
                        .tags
                        .insert("cost,centre".to_string(), "v".to_string());
                },
                "outside the TagKey pattern",
            ),
            (
                "a 257-character tag value",
                |request| {
                    request.tags.insert("owner".to_string(), "v".repeat(257));
                },
                "over the TagValue ceiling",
            ),
        ];

        for (label, mutate, expected) in rows {
            let (plane, fake, _) = planted();
            // Answered on purpose: if the guard were missing, the call would *succeed* and the
            // count would be 1. A fake with nothing queued would fail the call for the wrong
            // reason and the test would pass while proving nothing.
            fake.answer(
                "CreateMicrovmImage",
                Answer::created(fake::create_image_response("agentd-conformance")),
            );

            let mut request = a_request();
            mutate(&mut request);
            let error = plane
                .create_image(request)
                .await
                .expect_err(&format!("{label} must be refused"));
            assert_eq!(error.kind(), ErrorKind::InvalidArg, "{label}");
            assert!(
                error.to_string().contains(expected),
                "{label}: the message must contain {expected:?}, got {error}"
            );
            assert_eq!(
                fake.calls().len(),
                0,
                "{label}: the request was refused locally, so the caller did not pay for the \
                 artifact upload first — and the fake had an answer queued, so a missing guard \
                 would show as calls: 1 rather than as a different failure"
            );
        }

        // The control case, and it is not decoration: every row above would also pass if
        // `create_image` refused *everything*, and a guard that refuses every request is a
        // guard nobody notices is wrong until a build fails.
        let (plane, fake, _) = planted();
        fake.answer(
            "CreateMicrovmImage",
            Answer::created(fake::create_image_response("agentd-conformance")),
        );
        let mut good = a_request();
        good.tags
            .insert("cost centre".to_string(), "team/agents".to_string());
        good.tags.insert("empty".to_string(), String::new());
        good.build_role_arn =
            "arn:aws:iam::392583147479:role/bonk-sandbox-microvm-build".to_string();
        plane
            .create_image(good)
            .await
            .expect("a realistic request with a space in a tag key and an empty tag value");
        assert_eq!(fake.call_count("CreateMicrovmImage"), 1);
    }

    /// The `hooks.port` bound is enforced at [`ControlPlane::with_port`] and **nowhere on the
    /// create path**, and that is a deliberate absence rather than a gap.
    ///
    /// `port` is a private field and `with_port` is its only setter, so a `ControlPlane` whose
    /// port is 0 cannot exist — [`ControlPlane::with_transport`] takes the default. A
    /// `require_valid_port("hooks.port", self.port)` inside `create_image` would therefore be a
    /// branch no input can reach, which is worse than no branch: it reads as a guard, it appears
    /// in a coverage report as a guard, and no test can make it fire. The discipline this repo
    /// holds is that a guard must be falsifiable, so the check lives at the one place the value
    /// can be wrong.
    ///
    /// What this test asserts is the property that makes the absence safe: the port on the plane
    /// is legal by construction, so the `hooks.port` that reaches the wire is too.
    ///
    /// **Guard proof.** Make `with_port` infallible again — drop the `require_valid_port` call and
    /// return `Self` — and the first assertion here goes red, as does
    /// `the_port_guard_refuses_zero_and_says_what_zero_means` in `control/mod.rs`.
    #[tokio::test]
    async fn the_hooks_port_that_reaches_the_wire_is_legal_by_construction() {
        let (plane, fake, _) = planted();
        assert!(
            plane.with_port(0).is_err(),
            "with_port is the only setter for the field hooks.port is read from, so refusing 0 \
             here is what makes a zero hooks.port unrepresentable"
        );

        let (plane, fake2, _) = planted();
        let _ = fake;
        fake2.answer(
            "CreateMicrovmImage",
            Answer::created(fake::create_image_response("agentd-conformance")),
        );
        plane
            .with_port(9000)
            .expect("9000 is legal")
            .create_image(a_request())
            .await
            .expect("a legal port builds");
        assert_eq!(
            fake2.first_body("CreateMicrovmImage")["hooks"]["port"],
            9000,
            "and the port that was set is the port on the wire"
        );
    }

    /// **The identifier guard on every image operation, proved by the call count.**
    ///
    /// Ten of the twelve identifier members are URI parameters, and an empty one does not fail as
    /// a blank field — it collapses the path onto the collection. So `GetMicrovmImage` with an
    /// empty identifier is a request against the *listing*, and `DeleteMicrovmImage` with one is
    /// a delete against it. Those are the requests this proves are never sent.
    ///
    /// Every operation in one test, because the assertion is the same and the value is the
    /// coverage: issue #24 counted six implemented operations per identifier shape, and this is
    /// the enumeration.
    ///
    /// **Guard proof** — run 2026-08-16. Delete `require_valid_identifier` from any one of these
    /// and its assertion goes red with `calls: 1` (or, for `wait_for_image`, a poll loop against
    /// the listing).
    #[tokio::test]
    async fn no_image_operation_sends_an_empty_or_over_long_identifier() {
        for bad in ["", &"a".repeat(257)] {
            let (plane, fake, _) = planted();

            plane
                .get_image_version(bad, "1.0")
                .await
                .expect_err("GetMicrovmImageVersion");
            plane
                .get_image_build(bad, "1.0", "build-1")
                .await
                .expect_err("GetMicrovmImageBuild");
            plane
                .set_image_version_status(bad, "1.0", ops::VersionStatus::Inactive)
                .await
                .expect_err("UpdateMicrovmImageVersion");
            plane
                .list_image_versions(bad)
                .await
                .expect_err("ListMicrovmImageVersions");
            plane
                .list_image_builds(bad, "1.0")
                .await
                .expect_err("ListMicrovmImageBuilds");
            plane
                .wait_for_image(bad, SizeClass::DEFAULT, WaitOpts::default())
                .await
                .expect_err("GetMicrovmImage, in a loop");
            plane
                .managed_base_versions(bad)
                .await
                .expect_err("ListManagedMicrovmImageVersions");
            assert!(
                !plane.delete_image(bad, 20, Duration::from_secs(15)).await,
                "delete_image answers false rather than raising, because it is a teardown path"
            );

            assert_eq!(
                fake.calls().len(),
                0,
                "eight operations refused {bad:?} locally; an empty URI parameter would have \
                 addressed the collection instead of the resource, and the service can answer \
                 200 to that"
            );
        }
    }

    /// The `NonBlankString` URI and querystring members on the image operations, same proof.
    ///
    /// `nameFilter` is the one that is not a URI parameter, and it is the one worth having: it
    /// goes out as `?nameFilter=` and either 400s or filters differently from what was meant,
    /// and `find_image_by_name` is the resolver a launch depends on.
    ///
    /// **Guard proof.** Delete `require_non_blank("nameFilter", name)` and the `find_image_by_name`
    /// row goes red with `calls: 1` — a listing was read with a blank filter.
    #[tokio::test]
    async fn no_image_operation_sends_a_blank_version_build_id_or_name_filter() {
        for bad in ["", " ", "1.0\n", &"9".repeat(2049)] {
            let (plane, fake, _) = planted();

            plane
                .get_image_version("arn:image", bad)
                .await
                .expect_err("imageVersion");
            plane
                .get_image_build("arn:image", bad, "build-1")
                .await
                .expect_err("imageVersion");
            plane
                .get_image_build("arn:image", "1.0", bad)
                .await
                .expect_err("buildId");
            plane
                .list_image_builds("arn:image", bad)
                .await
                .expect_err("imageVersion");
            plane.find_image_by_name(bad).await.expect_err("nameFilter");

            assert_eq!(
                fake.calls().len(),
                0,
                "five NonBlankString members refused {bad:?} locally"
            );
        }
    }
}
