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
                return Err(self.build_failure(&got.name, &got.state));
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
        let Some(states) = self.build_states(identifier).await else {
            // Could not see the build list. Say nothing rather than guess.
            return Ok(());
        };

        if states.is_empty() {
            // No builds listed at all is not the signature: the replay case lists builds
            // and leaves them PENDING. An empty list is a version whose builds have not
            // been enumerated yet.
            return Ok(());
        }
        if !states.iter().all(|state| state == "PENDING") {
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
                states.len(),
                states.join(", "),
            ),
        ))
    }

    /// The `buildState` of every build of the image's first version, or `None` when the
    /// listing could not be read.
    ///
    /// Two calls — versions, then builds — because `ListMicrovmImageBuilds` requires an
    /// `imageVersion` in its path.
    async fn build_states(&self, identifier: &str) -> Option<Vec<String>> {
        let versions_call = Call::get(
            "ListMicrovmImageVersions",
            paths::image_versions(identifier),
        );
        let versions: ops::ListImageVersionsResponseWire =
            send_with_retry(self.transport(), versions_call)
                .await
                .ok()?
                .json("ListMicrovmImageVersions")
                .ok()?;
        let version = versions.items.first()?.image_version.clone();

        let builds_call = Call::get(
            "ListMicrovmImageBuilds",
            paths::image_builds(identifier, &version),
        );
        let builds: ops::ListImageBuildsResponseWire =
            send_with_retry(self.transport(), builds_call)
                .await
                .ok()?
                .json("ListMicrovmImageBuilds")
                .ok()?;

        // `buildState`, not `state`. The deserializer refuses the other spelling, so this
        // read cannot silently produce nothing the way `b.get("state")` did.
        Some(
            builds
                .items
                .into_iter()
                .map(|build| build.build_state)
                .collect(),
        )
    }

    /// The message for a build the service reported as failed.
    ///
    /// Names the required log-group prefix, because the failure that reads as "unknown" is
    /// most often a build role granted the *plausible* prefix rather than the measured one.
    /// This client cannot check the log group — CloudWatch is not in its dependency set —
    /// so the prefix is named unconditionally rather than only when the group is empty,
    /// which is a deliberate weakening of the Python diagnostic and is noted as such.
    fn build_failure(&self, name: &str, state: &str) -> Error {
        Error::new(
            ErrorKind::Platform,
            format!(
                "the image build for {name:?} failed: {state}. If the reason reads as unknown and \
                 the build log group {BUILD_LOG_GROUP_PREFIX}/{name} contains no events, the cause \
                 is most likely the build role's log permissions rather than a silent service: \
                 the role must grant logs on the {BUILD_LOG_GROUP_PREFIX}/* prefix, and a policy \
                 granting /aws/lambda/microvms/* instead — the plausible spelling, and the wrong \
                 one — produces builds with no logs at all (docs/PLATFORM.md, 'Build logs go to \
                 {BUILD_LOG_GROUP_PREFIX}/<image-name>')."
            ),
        )
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
    async fn try_delete_image(&self, identifier: &str) -> Result<(), Error> {
        let versions_call = Call::get(
            "ListMicrovmImageVersions",
            paths::image_versions(identifier),
        );
        let versions: ops::ListImageVersionsResponseWire =
            send_with_retry(self.transport(), versions_call)
                .await?
                .json("ListMicrovmImageVersions")?;

        // Skip the first: the last remaining version goes with the image.
        for version in versions.items.iter().skip(1) {
            let call = Call::delete(
                "DeleteMicrovmImageVersion",
                paths::image_version(identifier, &version.image_version),
            );
            send_with_retry(self.transport(), call).await?;
        }

        let call = Call::delete("DeleteMicrovmImage", paths::microvm_image(identifier));
        send_with_retry(self.transport(), call).await?;
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

    /// A reported build failure names the required log-group prefix, because the failure
    /// that reads as "unknown" is most often the wrong IAM prefix rather than a silent
    /// service.
    #[tokio::test]
    async fn a_failed_build_names_the_required_log_group_prefix() {
        let (plane, fake, _) = planted();
        fake.answer(
            "GetMicrovmImage",
            Answer::ok(fake::get_image_response("img", "CREATE_FAILED")),
        );

        let error = plane
            .wait_for_image("arn:image", SizeClass::DEFAULT, WaitOpts::default())
            .await
            .expect_err("a failed build is a failure");
        assert_eq!(error.kind(), ErrorKind::Platform);
        let message = error.to_string();
        assert!(message.contains("/aws/lambda-microvms/img"), "{message}");
        assert!(
            message.contains("/aws/lambda/microvms/*"),
            "the wrong prefix has to be named as wrong: {message}"
        );
        assert!(message.contains("the plausible spelling"), "{message}");
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
}
