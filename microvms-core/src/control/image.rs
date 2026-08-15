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
    /// costs the caller the upload first. Everything checkable is checked here.
    ///
    /// The artifact is built here too, but **not** uploaded — S3 is not in this crate's
    /// dependency set, so [`CreateImageRequest::code_artifact_uri`] is where the caller
    /// says they have already put it. [`ControlPlane::build_artifact_for`] produces the
    /// bytes to upload.
    pub async fn create_image(&self, request: CreateImageRequest) -> Result<Image, Error> {
        super::require_valid_image_name(&request.name)?;

        if request.inherit_workdir {
            artifact::require_workdir(&request.base_image, request.dockerfile.as_deref())?;
        }
        if let Some(dockerfile) = request.dockerfile.as_deref() {
            artifact::require_matching_from(&request.base_image, dockerfile)?;
        }

        let wire = ops::CreateMicrovmImageWire {
            name: request.name.clone(),
            base_image_arn: request.base_image.arn(&self.region),
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
            reasons.extend(
                builds
                    .iter()
                    .filter(|build| build.state_reason.is_some() && build.build_state != "PENDING")
                    .map(|build| format!("build {}", build.describe())),
            );
        }
        reasons
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
    /// # Why the first version is kept
    ///
    /// The last remaining version cannot be deleted on its own, only together with the
    /// image. Trying produces a `ConflictException` that reads like a permissions problem.
    ///
    /// Returns `true` when the image was deleted, `false` when every attempt failed. It
    /// does not raise, because the caller is a teardown path and the original failure is
    /// the one worth reading.
    pub async fn delete_image(&self, identifier: &str, attempts: u32, backoff: Duration) -> bool {
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
    /// **Falsification** — three, all run 2026-08-15. (a) Drop the `failure_reasons` call
    /// from `build_failure` and the message loses both reasons: the first two assertions go
    /// red. (b) Stop following `nextToken` in `builds_of_version` (return `Some(builds)` in
    /// the `Some(token)` arm) and only the version reason survives: the `no space left`
    /// assertion goes red. (c) Drop `state_reason` from `MicrovmImageBuildSummaryWire` and
    /// it does not compile.
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
}
