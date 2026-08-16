// SPDX-License-Identifier: Apache-2.0
//! Live tier: the four control-plane operations this branch added, through the **real** signed
//! transport against the **real** service.
//!
//! # Why this file exists when the fakes already cover the loops
//!
//! Same argument `live_pagination.rs` makes and one more. Every other test of these operations
//! reaches the control plane through an injected `Transport`, so the fake answers JSON this
//! crate wrote and the deserializer reads it back — which proves the *loop* and cannot prove
//! the *shape*. And two of these four have a wire property no fake can hold:
//!
//! * **`UpdateMicrovmImageVersion` is a `PATCH`.** Nothing else in this client sends one, so
//!   `Method::Patch` reaching the canonical request correctly is unexercised until a real
//!   signature is computed over it. A method the enum spells and the signer signs differently
//!   is rejected in a way that reads like bad credentials.
//! * **`INACTIVE` is only a retire if the service enforces it.** A readback saying `INACTIVE`
//!   is the service echoing a field. The property the feature rests on is that `RunMicrovm`
//!   then *refuses*, and only the service can demonstrate that.
//!
//! # Read-only, except for the one test that says otherwise in its name
//!
//! Three of the four tests here are `GET`s over resources the account already holds — free, and
//! safe to leave in the live lane at zero marginal cost. The fourth flips a version's status
//! and flips it back, and it is the only one that writes: it names its own subject, restores it
//! on every exit path, and asserts the restore. See its docs for why that risk is taken.
//!
//! # Ignored by default
//!
//! `#[ignore]` means "needs credentials and an account", not "flaky". It runs from the live
//! lane:
//!
//! ```text
//! AWS_REGION=us-east-1 cargo test -p microvms-core --test live_versions -- --ignored
//! ```

use microvms_core::control::{ControlPlane, ops};
use microvms_core::region::Region;

/// The region every check runs in, for the reason `live_pagination.rs` gives: these assertions
/// are about a signed request's shape, and a region that does not carry MicroVMs answers a
/// null-message denial that would read as this test's failure.
const REGION: Region = Region::UsEast1;

/// **`GetMicrovmImageBuild` answers snapshot sizes no other operation reports.**
///
/// The whole reason to make the call. Nothing on `GetMicrovmImage`, `GetMicrovm`, or either
/// listing carries a byte count, so before this a storage estimate had only the size class's
/// baseline to multiply. Measured 2026-08-16 against a real successful build:
///
/// | field | value |
/// | --- | --- |
/// | `memorySnapshotSizeInBytes` | 574869504 |
/// | `codeInstallSizeInBytes` | 214093824 |
/// | `diskSnapshotSizeInBytes` | 23474176 |
///
/// The assertion is not on those numbers — they are properties of one image — but on the
/// structure: a `SUCCESSFUL` build reports all three, and the listing reports none.
#[tokio::test]
#[ignore = "needs real AWS credentials and an account; runs in the live lane"]
async fn a_successful_builds_snapshot_sizes_come_back_and_the_listing_has_none() {
    let plane = ControlPlane::new(REGION)
        .await
        .expect("credentials resolve; `aws sts get-caller-identity` shows the same failure");

    let Some((image, version)) = any_successful_version(&plane).await else {
        eprintln!("SKIP: this account holds no image with a SUCCESSFUL version");
        return;
    };

    let builds = plane
        .list_image_builds(&image, &version)
        .await
        .expect("the build listing reads");
    let Some(listed) = builds
        .iter()
        .find(|build| build.build_state == "SUCCESSFUL")
    else {
        eprintln!("SKIP: {image} version {version} has no SUCCESSFUL build");
        return;
    };

    let deeper = plane
        .get_image_build(&image, &version, &listed.build_id)
        .await
        .expect("GetMicrovmImageBuild reads");

    // Every member the listing carries agrees, so the get is the same build and not another.
    assert_eq!(deeper.build_id, listed.build_id);
    assert_eq!(deeper.build_state, listed.build_state);
    assert_eq!(deeper.chipset_generation, listed.chipset_generation);
    assert_eq!(deeper.architecture, "ARM_64");
    assert_eq!(deeper.chipset, "GRAVITON");

    // And the one member it adds. A successful build reports all three sizes; the whole point
    // is that the listing shape has no member for any of them.
    let sizes = deeper
        .snapshot_build
        .expect("a SUCCESSFUL build reports its snapshot sizes");
    println!(
        "GetMicrovmImageBuild on {} generation {}: {}",
        listed.build_id,
        deeper.chipset_generation,
        sizes.describe().unwrap_or_else(|| "no sizes".to_string()),
    );
    assert!(
        sizes.memory_snapshot_size_in_bytes.is_some_and(|n| n > 0),
        "a successful build wrote a memory snapshot: {sizes:?}"
    );
    assert!(
        sizes.code_install_size_in_bytes.is_some_and(|n| n > 0),
        "a successful build installed code: {sizes:?}"
    );
    assert!(
        sizes.disk_snapshot_size_in_bytes.is_some_and(|n| n > 0),
        "a successful build wrote a disk snapshot: {sizes:?}"
    );
}

/// **`GetMicrovmImageVersion` echoes the whole creation request back**, and its `status` is a
/// separate field from its `state`.
///
/// The two being separate is the fact the retire feature rests on: a `SUCCESSFUL` version can be
/// `INACTIVE`, which is a build that worked and is no longer launchable. A client that read
/// `state` alone could not tell a retired version from a live one.
///
/// Measured 2026-08-16, and the finding worth recording is the one about `baseImageVersion`: a
/// build pinned with `--base-image-version 1` reads back `"1.0"`, not `"1"`. The service
/// normalises, so the value it echoes cannot be fed back into a request or compared against
/// `ListManagedMicrovmImageVersions`'s own strings.
#[tokio::test]
#[ignore = "needs real AWS credentials and an account; runs in the live lane"]
async fn a_versions_readback_carries_its_config_and_a_status_beside_its_state() {
    let plane = ControlPlane::new(REGION)
        .await
        .expect("credentials resolve");

    let Some((image, version)) = any_successful_version(&plane).await else {
        eprintln!("SKIP: this account holds no image with a SUCCESSFUL version");
        return;
    };

    let got = plane
        .get_image_version(&image, &version)
        .await
        .expect("GetMicrovmImageVersion reads");
    println!("GetMicrovmImageVersion on {image}: {}", got.describe());

    assert_eq!(got.image_version, version);
    assert_eq!(got.state, "SUCCESSFUL");
    assert!(
        got.status == "ACTIVE" || got.status == "INACTIVE",
        "the model declares exactly two statuses; the service answered {:?}",
        got.status
    );
    // The two members are different questions, and both are required. A version can be
    // SUCCESSFUL and INACTIVE at once, which is what a retire produces.
    assert!(
        !got.status.is_empty(),
        "status is required by the model and was present on all 22 versions measured"
    );

    // The config readback: `resources` is the only place a built image's size class is
    // observable at all, since `GetMicrovm` carries no memory figure.
    let resources = got
        .resources
        .as_ref()
        .expect("the readback carries the resources it was built with");
    assert_eq!(
        resources.len(),
        1,
        "ResourcesList is max 1, so two memory floors is unaskable"
    );
    assert!(resources[0].minimum_memory_in_mib >= 512);
    assert!(
        got.code_artifact.uri.starts_with("s3://"),
        "the artifact URI comes back: {}",
        got.code_artifact.uri
    );
    assert!(got.base_image_arn.contains(":microvm-image:"));

    // The image-level egress list, whose ceiling is 1 and not the VM-level 10.
    if let Some(egress) = got.egress_network_connectors.as_ref() {
        assert!(
            egress.len() <= microvms_core::constants::MAX_IMAGE_EGRESS_CONNECTORS,
            "the image-level egress list is max 1, not the VM-level 10: {egress:?}"
        );
    }

    // The normalisation finding. Recorded rather than asserted as a literal, because it is a
    // property of AWS's spelling that this project neither controls nor depends on — what is
    // asserted is that it exists at all, which is what makes it uncomparable.
    if let Some(base_version) = got.base_image_version.as_deref() {
        println!(
            "baseImageVersion echoes as {base_version:?} — measured 2026-08-16, a build pinned \
             with `--base-image-version 1` reads back \"1.0\", so the echoed value is not the \
             value that was sent and cannot be compared with the managed listing's strings"
        );
    }
}

/// **`ListManagedMicrovmImageVersions` answers the versions a build may pin**, and a bare name
/// is refused before the call.
///
/// Measured 2026-08-16: `al2023-1` answers two versions, `"1"` and `"0"`, newest first, as bare
/// integers. That the list has *two* is the finding — a client omitting `baseImageVersion` takes
/// a default that has already moved once.
///
/// The bare-name half is asserted through the client's own local refusal rather than by sending
/// one: the service answers `ValidationException: Invalid ARN format: al2023-1`, and the point
/// of the guard is that the caller never pays for that round trip.
#[tokio::test]
#[ignore = "needs real AWS credentials and an account; runs in the live lane"]
async fn the_managed_base_answers_its_versions_and_a_bare_name_is_refused_locally() {
    let plane = ControlPlane::new(REGION)
        .await
        .expect("credentials resolve");
    let base = microvms_core::control::BaseImage::al2023();
    let arn = base.arn(&REGION);

    let versions = plane
        .managed_base_versions(&arn)
        .await
        .expect("the managed base's version listing reads");
    let names: Vec<&str> = versions
        .iter()
        .map(|version| version.image_version.as_str())
        .collect();
    println!("{}: {} version(s) — {names:?}", base.name, names.len());

    assert!(
        !versions.is_empty(),
        "the managed base must have at least one version, or nothing could be built"
    );
    // Bare integers, not the `major.minor` a custom image uses. A caller who parsed one format
    // would not parse the other, and neither matches the `"1.0"` the version readback echoes.
    for name in &names {
        assert!(
            !name.contains('.'),
            "a managed base's versions are bare integers, measured 2026-08-16: {name:?}"
        );
        assert!(!name.is_empty(), "an empty version string is not a Version");
    }
    for version in &versions {
        assert_eq!(
            version.image_arn, arn,
            "every item names the base it belongs to"
        );
    }

    // The local refusal, which is what makes the ARN requirement legible. The service's own
    // message names the value without saying which member wanted an ARN.
    let refused = plane
        .managed_base_versions(&base.name)
        .await
        .expect_err("a bare name is refused before the call");
    let message = refused.to_string();
    assert!(message.contains("Invalid ARN format"), "{message}");
    assert!(
        message.contains(&arn),
        "the remedy is the ARN itself: {message}"
    );

    // The base listing too, which is informational only: `ManagedMicrovmImageSummary` carries
    // no registry reference, so a discovered base cannot be paired with a Dockerfile FROM.
    let bases = plane
        .managed_base_images()
        .await
        .expect("the managed base listing reads");
    println!(
        "ListManagedMicrovmImages in {REGION}: {:?}",
        bases
            .iter()
            .map(|base| base.image_arn.as_str())
            .collect::<Vec<_>>()
    );
    assert!(
        bases.iter().any(|listed| listed.image_arn == arn),
        "the base this client builds on must be in the listing: {bases:?}"
    );
    for listed in &bases {
        assert!(
            !listed.image_arn.contains("public.ecr.aws"),
            "no registry reference is derivable from the ARN, which is why a discovered base \
             cannot construct a BaseImage: {}",
            listed.image_arn
        );
    }
}

/// **The whole retire feature, round-tripped: INACTIVE, refuse the launch, ACTIVE, launch.**
///
/// This is the one test in this file that **writes**, and it is the only way the feature can be
/// demonstrated. A readback saying `INACTIVE` is the service echoing a field; the property the
/// feature rests on is that `RunMicrovm` then refuses, and only the service can show that.
///
/// # What it does to the account, and why that is acceptable
///
/// It picks an image whose name starts with the conformance prefix and whose *only* version is
/// `SUCCESSFUL` and `ACTIVE`, sets that version `INACTIVE`, attempts a pinned launch, and then
/// sets it back `ACTIVE` — restoring on **every** exit path, including a failed assertion, and
/// asserting the restore afterwards. Setting a version `INACTIVE` starts nothing and terminates
/// nothing: running VMs are untouched, and the only observable effect is that new launches of
/// that image are refused for the seconds the test holds it.
///
/// # A launch cannot be attempted "safely", and that cost a leaked VM to learn
///
/// The first version of this test made the launch with a **deliberately absent execution role**,
/// on the reasoning that the request would be refused before anything was created — the technique
/// docs/PLATFORM.md records for bracketing `runHookPayload`. That reasoning was wrong.
/// `executionRoleArn` is optional *in the model* and the service means it: measured 2026-08-16,
/// the incomplete launch **succeeded** and created `microvm-337c4bb6-…`, which this helper's own
/// panic caught and which was terminated by hand a minute later.
///
/// So there is no incomplete `RunMicrovm` that fails locally, and the launch here is a **real**
/// one. The ACTIVE half therefore creates a VM and terminates it immediately; the INACTIVE half
/// is refused and creates nothing. That asymmetry is not a flaw in the test, it *is* the
/// measurement — one VM's worth of seconds is what the whole feature costs to verify.
///
/// # Falsification
///
/// Run 2026-08-16. With the version INACTIVE, `RunMicrovm` answers **`ResourceNotFoundException:
/// No active version found for MicroVM image <arn> and version 1.0`**; with it ACTIVE the same
/// request launches. If the service treated `INACTIVE` as advisory, the INACTIVE half would
/// launch too and the `expect_err` below would go red.
#[tokio::test]
#[ignore = "needs real AWS credentials and an account; MUTATES a version's status, launches one VM, and restores both"]
async fn a_version_set_inactive_refuses_a_launch_and_active_restores_it() {
    let plane = ControlPlane::new(REGION)
        .await
        .expect("credentials resolve");

    let Some((image, version)) = conformance_active_version(&plane).await else {
        eprintln!(
            "SKIP: no image named `microvm-cli-*` in this account has a single SUCCESSFUL/ACTIVE \
             version. This test refuses to touch an image it did not identify as this \
             project's."
        );
        return;
    };
    println!("subject: {image} version {version}");

    // Retire it. From here every exit path restores.
    let retired = plane
        .set_image_version_status(&image, &version, ops::VersionStatus::Inactive)
        .await
        .expect("UpdateMicrovmImageVersion is accepted");
    assert_eq!(
        retired.status, "INACTIVE",
        "the readback is checked inside set_image_version_status; this is belt and braces"
    );

    // Read back through a *separate* GET, so the status is not merely the PATCH's own echo of
    // the field it was handed, then try the launch the retire is supposed to refuse.
    let read_back = plane.get_image_version(&image, &version).await;
    let refused = pinned_launch(&plane, &image, &version).await;

    // Restored before anything is asserted, so a failing assertion does not leave a retired
    // version behind. This is the whole reason the assertions are below rather than inline.
    let restored = plane
        .set_image_version_status(&image, &version, ops::VersionStatus::Active)
        .await;

    let read_back = read_back.expect("GetMicrovmImageVersion reads the retired version");
    assert_eq!(
        read_back.status,
        "INACTIVE",
        "an independent GET must see the retire, not just the PATCH's echo: {}",
        read_back.describe()
    );
    assert!(
        !read_back.is_active(),
        "is_active must agree with the wire: {}",
        read_back.describe()
    );
    assert_eq!(
        read_back.state,
        "SUCCESSFUL",
        "a retire does not change the build state — the version still built fine, it is just \
         no longer launchable: {}",
        read_back.describe()
    );

    // **The property the whole feature rests on.** A retired version does not launch, and the
    // service says so about the *version* rather than about the request.
    let error = match refused {
        Ok(id) => {
            // Terminate before failing, so a wrong verdict does not also leak a VM.
            let _ = plane.terminate(&id).await;
            panic!(
                "a version set INACTIVE launched anyway and created {id} (terminated). The \
                 retire would then be advisory and the whole feature would not exist."
            );
        }
        Err(error) => error,
    };
    let message = error.to_string();
    println!("INACTIVE -> {message}");
    assert!(
        message.contains("No active version found"),
        "the refusal has to be about the version rather than about the request, which is what \
         makes it a retire: {message}"
    );
    assert_eq!(
        error.kind(),
        microvms_core::ErrorKind::Platform,
        "a 404 about a version is a platform failure, not a credentials one: {message}"
    );

    let restored = restored.expect("the version is restored to ACTIVE");
    assert_eq!(
        restored.status,
        "ACTIVE",
        "the account must be left as it was found: {}",
        restored.describe()
    );
    let confirmed = plane
        .get_image_version(&image, &version)
        .await
        .expect("the restore reads back");
    assert!(
        confirmed.is_active(),
        "an independent GET must confirm the restore: {}",
        confirmed.describe()
    );

    // And the launch works again, which closes the round trip. This is the half that creates a
    // VM, so it is terminated on the next line — see the docs on why there is no cheaper way.
    let launched = pinned_launch(&plane, &image, &version)
        .await
        .expect("once ACTIVE again, the same pinned launch must be accepted");
    println!("ACTIVE   -> launched {launched}, terminating");
    plane
        .terminate(&launched)
        .await
        .expect("the verification VM is terminated immediately");
}

// ── helpers ──────────────────────────────────────────────────────────────────

/// A **real** launch pinned to `version`, answering the VM id or the service's refusal.
///
/// Real because there is no incomplete `RunMicrovm` that fails locally: `executionRoleArn` is
/// optional in the model and the service means it, so a launch with the role omitted succeeds
/// and creates a VM — measured 2026-08-16, at the cost of one leaked VM the earlier version of
/// this helper created and then panicked about. Every caller terminates what this returns.
///
/// Five minutes of `maximumDurationInSeconds`, which is the floor a launch needs and the ceiling
/// on what an abandoned one could cost if a caller somehow failed to terminate it.
async fn pinned_launch(
    plane: &ControlPlane,
    image: &str,
    version: &str,
) -> Result<String, microvms_core::Error> {
    use microvms_core::control::{RunHookPayload, RunMicrovmRequest};

    let payload = RunHookPayload::for_agent_token("live-verification").expect("a token fits");
    let mut request = RunMicrovmRequest::new(image, payload).with_image_version(version);
    request.execution_role_arn = std::env::var("MICROVM_EXECUTION_ROLE_ARN").ok();
    request.max_duration_sec = 300;
    plane.run_microvm(request).await.map(|vm| vm.id)
}

/// The first (image ARN, version) pair whose version is `SUCCESSFUL`, or `None`.
///
/// Walks every image and `continue`s past a per-image failure rather than `?`-ing out, for the
/// reason `live_pagination.rs` records at length: a helper that cannot distinguish "looked
/// everywhere" from "stopped looking" makes its callers skip nondeterministically, and a skip
/// reads like a pass in the summary.
async fn any_successful_version(plane: &ControlPlane) -> Option<(String, String)> {
    let mut unreadable = 0_usize;
    let images = plane.list_images().await.ok()?;
    for image in &images {
        let Ok(versions) = plane.list_image_versions(&image.image_arn).await else {
            unreadable += 1;
            continue;
        };
        if let Some(version) = versions
            .iter()
            .find(|version| version.state == "SUCCESSFUL")
        {
            return Some((image.image_arn.clone(), version.image_version.clone()));
        }
    }
    eprintln!(
        "no SUCCESSFUL version across {} image(s); {unreadable} could not be read (an image in \
         DELETING answers a listing error, which is data rather than a failure)",
        images.len(),
    );
    None
}

/// An image this project created, with exactly one `SUCCESSFUL` and `ACTIVE` version.
///
/// # Why the subject is narrowed this hard
///
/// The retire test mutates it. So it must be an image *this project* made — the name prefix is
/// the only attribution available, since `RunMicrovm` takes no tags and a MicroVM cannot be
/// tagged at all (docs/PLATFORM.md) — and it must have exactly one version, because retiring one
/// of several would change which version an unpinned launch picks for anyone else running at the
/// same time. One version means the only thing the test can affect is pinned launches of the
/// image it named.
async fn conformance_active_version(plane: &ControlPlane) -> Option<(String, String)> {
    let images = plane.list_images().await.ok()?;
    for image in &images {
        if !image.name.starts_with("microvm-cli-") {
            continue;
        }
        let Ok(versions) = plane.list_image_versions(&image.image_arn).await else {
            continue;
        };
        if versions.len() != 1 {
            continue;
        }
        let only = &versions[0];
        if only.state == "SUCCESSFUL" && only.is_active() {
            return Some((image.image_arn.clone(), only.image_version.clone()));
        }
    }
    None
}
