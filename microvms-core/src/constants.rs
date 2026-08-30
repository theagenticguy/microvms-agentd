// SPDX-License-Identifier: Apache-2.0
//! Every hardcoded service constraint, and the JSON the build gate checks them with.
//!
//! Each number and pattern below is transcribed from the botocore service model for
//! `lambda-microvms`, API version [`MODEL_API_VERSION`]. The model is a
//! machine-readable statement of the service's own request validation, and restating
//! it by hand is how this project published a 16 KB `runHookPayload` ceiling against
//! a real ceiling of 4096 — wrong by 4x in the dangerous direction, since it told a
//! caller four times as much secret material fits as actually does.
//!
//! # Why they are checked locally at all
//!
//! Because the SDK does not check them. Measured 2026-08-07 by reading botocore's
//! `validate.py`: `VALIDATED_METADATA_ATTRS` is `{'required', 'min', 'document',
//! 'union'}`, so a `min` violation is caught before the wire while `max`, `pattern`,
//! and `enum` violations are serialized, sent, and answered with a
//! `ValidationException` — confirmed empirically for `max` (runHookPayload 4097,
//! maximumDurationInSeconds 28801, ImageName 65 chars, NetworkConnectorList 11 items,
//! clientToken 129 chars), for `pattern` (ImageName `"a b!"`), and for `enum`
//! (architecture X86_64, additionalOsCapabilities CAP_SYS_ADMIN). Every one reached
//! the wire. So the guards built on these values are load-bearing rather than
//! belt-and-braces, and the obvious future simplification — "the SDK validates the
//! model already, delete these" — silently reopens all of them.
//!
//! `IdlePolicy.maxIdleDurationSeconds` used to be listed here as the counter-example — the
//! one constraint with no constant, on the grounds that its `min: 60` is a `min` and
//! botocore enforces those locally. **That reasoning does not transfer to this client and
//! the exemption is gone** ([`MIN_IDLE_DURATION_SEC`]). It was true of the deleted Python
//! client, which called through botocore; this one signs with `aws-sigv4` and sends with
//! `reqwest`, and `validate.py` is never on the path. Every constraint here is enforced by
//! this crate or by nothing, `min` included.
//!
//! # The drift gate (TRAP-12)
//!
//! [`as_json`] emits every value in this module as one object, keyed with the names the
//! deleted Python client's `sandbox.py` used. That is what makes the gate possible:
//! `scripts/check-model-drift.py` reads this object and compares each constant against the
//! pinned model. It used to compare the Python module the same way and then the two
//! clients against each other; with one client left, the two values no model states —
//! `MICROVM_REGIONS` and `SIZE_CLASSES` — are compared against pinned literals in that
//! script instead, since a value compared only against itself passes by construction.
//!
//! The names are therefore a contract with a script, not a style choice. Renaming
//! `MAX_RUN_HOOK_PAYLOAD_BYTES` here without renaming it there does not fail
//! compilation; it makes a check silently stop comparing. The test at the bottom of
//! this file pins the key set for that reason.

use serde_json::{Value, json};

use crate::region::MICROVM_REGIONS;

/// The model version every constraint here was read from.
///
/// The drift gate hard-fails when this disagrees with the service directory it
/// resolves, rather than skipping: a constraint checked against a different API
/// version is a constraint that was not checked.
pub const MODEL_API_VERSION: &str = "2025-09-09";

/// `RunMicrovmRequestRunHookPayloadString.max`.
///
/// **Inclusive**: 4096 bytes passes the length check and 4097 is rejected, bracketed
/// 2026-08-07 by calling `RunMicrovm` with a deliberately bogus `imageIdentifier` so
/// nothing could be created or billed. This is the only per-VM secret channel the
/// platform offers — one bearer token fits, a cloud credential set does not.
///
/// # The model itself says 16,384, and it is wrong
///
/// Not only this repo's prose. `service-2.json`'s **documentation string** on
/// `RunMicrovmRequest.runHookPayload` reads, verbatim for API version 2025-09-09:
///
/// > Per-MicroVM initialization data delivered as the request body of the /run lifecycle
/// > hook. Use to pass tenant-specific configuration such as session IDs or secret
/// > references. Maximum: 16,384 bytes.
///
/// while the shape that member names — `RunMicrovmRequestRunHookPayloadString` — declares
/// `{"max": 4096, "min": 0}`. The shape is what the service validates against, and 4096 was
/// measured inclusive against the real service. **So a reader who "corrects" this constant
/// from the model's prose reintroduces the bug**, and they will be able to cite the model
/// while doing it: that 16 KB figure is where this project's own 16 KB claim came from, and
/// it survived several review passes because prose has nothing to disagree with. Issue #24
/// named this trap specifically. The drift gate compares against the *shape*, which is why it
/// stays green while the prose beside it says something else.
pub const MAX_RUN_HOOK_PAYLOAD_BYTES: usize = 4096;

/// The figure the model's own `runHookPayload` documentation string claims, which is **not**
/// the ceiling.
///
/// Pinned as a constant so the wrong number has a name and a comparison rather than only a
/// mention. [`MAX_RUN_HOOK_PAYLOAD_BYTES`] carries the full account; what this exists for is
/// the drift gate, which reads the model's documentation string and asserts the claim is
/// still 16,384 — because the day AWS fixes their prose is the day this constant and its
/// warning should be deleted, and nothing else would say so.
///
/// It is deliberately **never** used as a bound. The test below asserts it is 4x the real
/// ceiling, which is the shape of the hazard: wrong in the permissive direction, telling a
/// caller four times as much secret material fits as actually does.
pub const DOCUMENTED_RUN_HOOK_PAYLOAD_BYTES: usize = 16_384;

/// `ImageName.max`.
///
/// The minimum is 1 and this comment used to say botocore enforces that one. It does, and this
/// client does not use botocore — so the min is checked by
/// [`crate::control::require_valid_image_name`] like everything else, and always was.
pub const MAX_IMAGE_NAME_LEN: usize = 64;

/// `ImageName.pattern`, as the model spells it.
///
/// No dots and no slashes, which rules out the two separators a caller reaching for a
/// namespaced name writes first. Published as a string for the drift gate; the
/// matcher is [`is_valid_image_name`], a direct byte check over the four ranges.
pub const IMAGE_NAME_PATTERN: &str = "[a-zA-Z0-9-_]+";

/// `Version.max`, which is also `NonBlankString.max`.
///
/// Checked by [`crate::control::require_valid_version`] on the two members this client sends
/// as a `Version` — `CreateMicrovmImage.baseImageVersion` and `RunMicrovm.imageVersion`. Issue
/// #24 named `NonBlankString` as the model's most-reused unguarded shape; these are the two
/// places it is now guarded, and both are sent at a moment where the service's own rejection
/// is expensive (after an artifact upload, or in the middle of a rollback).
pub const MAX_VERSION_LEN: usize = 2048;

/// `Version.pattern` — **no whitespace anywhere**, not merely "not blank".
///
/// Published as a string for the drift gate; the check is hand-rolled beside it for the reason
/// [`IMAGE_NAME_PATTERN`] gives. A version copied out of a terminal carries a trailing newline
/// and satisfies "non-empty" while failing this, which is why the refusal names the character.
pub const VERSION_PATTERN: &str = "[^\\s]+";

/// `NonBlankString.max`, which today equals [`MAX_VERSION_LEN`] and is a different shape.
///
/// The model's most-reused constrained shape — 45 members name it — and the ones this client
/// *sends* are `CodeArtifact.uri`, `CreateMicrovmImage.baseImageArn`, and
/// `ListMicrovmImages.nameFilter`. Checked by [`crate::control::require_non_blank`].
///
/// A separate constant from [`MAX_VERSION_LEN`] rather than a reuse of it, and the reason is
/// the same one the two connector ceilings give: `Version` and `NonBlankString` are two shapes
/// in the model with two independent futures, and AWS can move one without moving the other.
/// One constant serving both would be a comparison that silently stops being about one of
/// them. The drift gate holds each against its own shape.
pub const MAX_NON_BLANK_LEN: usize = 2048;

/// `NonBlankString.pattern` — the same `[^\s]+` [`VERSION_PATTERN`] carries, held separately
/// for the reason [`MAX_NON_BLANK_LEN`] gives.
pub const NON_BLANK_PATTERN: &str = "[^\\s]+";

/// `MicrovmIdentifier.max` and `MicrovmImageIdentifier.max`, which are the same number.
///
/// Both shapes are `min: 1, max: 256`, and between them they cover **twelve** URI, body, and
/// querystring members across every implemented operation — every `GetMicrovm`, `Suspend`,
/// `Resume`, `Terminate`, `CreateMicrovmAuthToken`, `GetMicrovmImage`, `Delete*`, `List*`, and
/// `RunMicrovm.imageIdentifier`. Checked by [`crate::control::require_valid_identifier`].
///
/// # The model contradicts itself here, and the client resolves it toward 256
///
/// `MicrovmImageArn` is `min: 20, max: 2048`, and it is the shape the service **answers** with
/// on `GetMicrovmResponse.imageArn`, `MicrovmItem.imageArn`, and `RunMicrovmResponse.imageArn`.
/// So the model permits the service to return a 2048-character image ARN that is then illegal
/// to pass back as a `MicrovmImageIdentifier`. That is a real contradiction, not a reading
/// error, and only one side of it can be guarded: the request side.
///
/// This client refuses above 256 on the way out, because that is the bound the *request*
/// shapes state and a request over it is rejected by the service regardless of where the value
/// came from. Refusing there names the contradiction — see
/// [`crate::control::require_valid_identifier`], whose message says an over-long identifier
/// that came from a response is a service-side inconsistency to report rather than a caller
/// mistake. The alternative, accepting up to 2048 because a response might carry one, would
/// send a request that cannot succeed and report the service's `ValidationException` as though
/// the caller had chosen the length.
///
/// In practice the gap is theoretical: a real image ARN in the longest-named MicroVM region is
/// about 90 characters, and the test below pins that arithmetic so "it fits" is measured rather
/// than assumed.
pub const MAX_IDENTIFIER_LEN: usize = 256;

/// `MicrovmImageArn.max` — 2048, the number that contradicts [`MAX_IDENTIFIER_LEN`].
///
/// Pinned so the contradiction is a comparison rather than a comment: this is a
/// **response**-side shape, so nothing guards a value against it, and its only job here is to
/// be the second half of the assertion that the two disagree. If AWS ever narrows this to 256
/// the contradiction is gone and the paragraph in [`MAX_IDENTIFIER_LEN`] should go with it —
/// the drift gate is what would notice.
pub const MAX_IMAGE_ARN_LEN: usize = 2048;

/// `TagKey.max`. Minimum is 1, so a blank key is not a tag.
pub const MAX_TAG_KEY_LEN: usize = 128;

/// `TagValue.max`. Minimum is **0** — an empty tag value is legal, unlike an empty key.
pub const MAX_TAG_VALUE_LEN: usize = 256;

/// The `TagKey`/`TagValue` pattern, which both shapes share verbatim.
///
/// Unicode property classes — letters, separators, numbers, and
/// `_ . : / = + - @` — so this is the one pattern in this module that is **not** ASCII ranges,
/// and the matcher [`is_valid_tag_component`] is written against `char::is_alphabetic` and
/// friends rather than against byte ranges for that reason.
///
/// # The pattern is `*` and therefore matches everything, which is a trap
///
/// The whole thing is a `*`-quantified group, so the empty string matches and so does any
/// prefix — an unanchored `*` pattern rejects nothing at all. What actually bites a caller is
/// the character *set*: a tag key with a comma, a hash, or a percent in it is outside it. The
/// matcher therefore checks that **every** character is in the set rather than trying to
/// implement the regex, which is the useful reading of the constraint and the one the service
/// enforces. Published as a string for the drift gate.
pub const TAG_COMPONENT_PATTERN: &str = "([\\p{L}\\p{Z}\\p{N}_.:/=+\\-@]*)";

/// `CloudWatchLoggingLogGroupString.max` — the ceiling on `logging.cloudWatch.logGroup`.
///
/// Checked by [`crate::control::require_valid_log_group`] before the create call, which
/// happens after the artifact upload — the same ordering argument every other create-side
/// guard makes.
pub const MAX_LOG_GROUP_LEN: usize = 512;

/// `CloudWatchLoggingLogGroupString.pattern`, as the model spells it.
///
/// Letters, digits, and `_ - / . #` — no colon, no space. Published as a string for the
/// drift gate; the matcher is [`is_valid_log_group`], a direct byte check for the reason
/// [`IMAGE_NAME_PATTERN`] gives.
pub const LOG_GROUP_PATTERN: &str = "[a-zA-Z0-9_\\-/.#]+";

/// `CloudWatchLoggingLogStreamString.max` — the ceiling on `logging.cloudWatch.logStream`.
///
/// This bounds the value **on the wire**, which is never the caller's value verbatim: the
/// client always appends `/<16 hex>` (see [`MAX_USER_LOG_STREAM_LEN`] for why, and
/// `crate::control::image` for the mechanism), so the caller-facing ceiling is lower.
pub const MAX_LOG_STREAM_LEN: usize = 512;

/// `CloudWatchLoggingLogStreamString.pattern` — anything but `:` and `*`.
///
/// The two excluded characters are CloudWatch's own stream-name reserved set. Published as
/// a string for the drift gate; [`crate::control::require_valid_log_stream`] names it in
/// its refusal.
pub const LOG_STREAM_PATTERN: &str = "[^:*]*";

/// The ceiling on a **caller-supplied** log stream name: [`MAX_LOG_STREAM_LEN`] minus the
/// 17 characters of per-build discriminator this client always appends (`/` + 16 hex).
///
/// # Why the client never sends a caller's stream name verbatim
///
/// The `logStream` member is an **exact** stream name, not a prefix (prefixes are
/// unsupported — 2026-08 platform finding, docs/PLATFORM.md 'An image build is three VMs
/// and three log streams'). One build emits three log streams, and a fixed configured name
/// collapses all three — and every *successive* build's three — into one stream, making
/// concurrent builds of different images indistinguishable. Appending a fresh CSPRNG
/// discriminator per create attempt keeps a configured name useful as a *family* prefix
/// while every attempt stays tellable apart, which is the same shape as TRAP-1's token
/// nonce and reuses its mechanism.
///
/// The test beside these constants pins the arithmetic, so a change to the nonce width
/// fails here rather than as a `ValidationException` after the artifact upload.
pub const MAX_USER_LOG_STREAM_LEN: usize = 495;

/// `RoleArn.min` — 20 characters, which is the shortest thing that can be an IAM role ARN.
pub const MIN_ROLE_ARN_LEN: usize = 20;

/// `RoleArn.max`.
pub const MAX_ROLE_ARN_LEN: usize = 2048;

/// `RoleArn.pattern`, as the model spells it.
///
/// Checked by [`crate::control::require_valid_role_arn`] on `CreateMicrovmImage.buildRoleArn`
/// and `RunMicrovm.executionRoleArn`. The build role is the one that matters most: the create
/// call happens **after** the artifact upload, so the service's rejection of a malformed role
/// ARN costs the caller the upload — the exact ordering `create_image` is arranged to prevent,
/// and the reason issue #24 called this one out by name.
///
/// Hand-matched rather than compiled, for the reason [`IMAGE_NAME_PATTERN`] gives, and the
/// matcher is deliberately a **structural** check rather than a full regex implementation: see
/// [`is_valid_role_arn`] on what it does and does not decide.
pub const ROLE_ARN_PATTERN: &str = "arn:aws[a-z\\-]*:iam::[0-9]{12}:role/?[a-zA-Z_0-9+=,.@\\-_/]+";

/// `PortNumber.min`, and also `HooksPortInteger.min`. **1, not 0.**
///
/// Port 0 is what "let the kernel choose" means to a listener and it is not a port a proxy
/// token or a hooks block can name. `with_port(0)` was representable and sent `{"port": 0}`
/// (issue #24); [`crate::control::require_valid_port`] is what refuses it now.
pub const MIN_PORT: u16 = 1;

/// `PortNumber.max`, which equals [`MAX_HOOK_PORT`] and is a different shape.
///
/// Held apart for the reason [`MAX_NON_BLANK_LEN`] gives: `PortNumber` bounds
/// `PortSpecification.port` and both ends of a `PortRange`, `HooksPortInteger` bounds
/// `Hooks.port`, and they are two shapes AWS can move independently.
pub const MAX_PORT: u16 = 65_535;

/// `IdlePolicyMaxIdleDurationSecondsInteger.min` — 60 seconds, with no maximum.
///
/// # Why this has a constant now when it deliberately did not
///
/// The old comment here and on [`crate::control::RunMicrovmRequest::max_idle_sec`] justified
/// having no guard on the grounds that `min` is one of the four keys in botocore's
/// `VALIDATED_METADATA_ATTRS`, so botocore refuses it before the wire with a clear message.
/// That is true of botocore and **false of this client**: `microvms-core` signs with
/// `aws-sigv4` and sends with `reqwest`, and nothing in the dependency set imports
/// `botocore/validate.py`. The reasoning was inherited from the deleted Python client, where it
/// held, and it did not survive the port. Issue #24 measured the consequence:
/// `max_idle_sec: 59` reached the wire.
///
/// Checked by [`crate::control::require_idle_duration`]. There is no maximum in the model, and
/// the client adds none: a caller who wants a VM that never auto-suspends within its
/// `maximumDurationInSeconds` says so with a large number, and the eight-hour ceiling on the
/// VM's life is the real bound.
pub const MIN_IDLE_DURATION_SEC: u32 = 60;

/// `RunMicrovmRequestMaximumDurationInSecondsInteger.max` — eight hours, and the hard
/// ceiling on any single VM's life. A longer session needs a second VM, not a larger
/// number.
pub const MAX_DURATION_SEC: u32 = 28_800;

/// `MicrovmHooks*TimeoutInSecondsInteger.max` (run, resume, suspend, terminate).
///
/// See [`crate::hooks`] for the 60x gap and why it is two types.
pub const MAX_MICROVM_HOOK_TIMEOUT_SEC: u32 = 60;

/// `MicrovmImageHooks*TimeoutInSecondsInteger.max` (ready, validate).
pub const MAX_IMAGE_HOOK_TIMEOUT_SEC: u32 = 3_600;

/// `HooksPortInteger.max`.
pub const MAX_HOOK_PORT: u32 = 65_535;

/// The `Capability` enum, which is exactly this one value.
///
/// Which is why guest identity repair is a boolean intent flag rather than a
/// capability list (TRAP-3): there is no way to ask for `CAP_SYS_ADMIN` alone.
pub const CAPABILITIES: [&str; 1] = ["ALL"];

/// The `Architecture` enum, which is exactly this one value: a MicroVM cannot be x86.
///
/// Load-bearing for cost (COST-9) as well as for requests — the Pricing API returns
/// both ARM and non-ARM compute rates 17.9% apart, and only the ARM line can ever
/// apply.
pub const ARCHITECTURES: [&str; 1] = ["ARM_64"];

/// `NetworkConnectorList.max` — the **VM-level** list, which `RunMicrovm` sends.
pub const MAX_NETWORK_CONNECTORS: usize = 10;

/// The **image-level** egress list's max, which is `1` and not `10`.
///
/// Six shapes in the model declare an image-level `egressNetworkConnectors` with
/// `min: 0, max: 1` — `CreateMicrovmImageRequest`, `MicrovmImageVersionSummary`,
/// `GetMicrovmImageVersionOutput`, `UpdateMicrovmImageVersionResponse`, and two more. The
/// VM-level `NetworkConnectorList` above allows 10. Issue #24 named the hazard: reusing
/// [`MAX_NETWORK_CONNECTORS`] for an image-level list is wrong by an order of magnitude, and
/// wrong in the permissive direction, so the rejection would arrive from the service.
///
/// Pinned here rather than left as a comment because the version readback now deserializes
/// that list ([`crate::control::ops::MicrovmImageVersionSummaryWire`]), so the two ceilings
/// are both live in this crate and the drift gate can hold each against its own shape.
pub const MAX_IMAGE_EGRESS_CONNECTORS: usize = 1;

/// The `MicrovmImageVersionStatus` enum, in the model's order.
///
/// The values `UpdateMicrovmImageVersion` accepts and `GetMicrovmImageVersion` answers.
/// Published so the drift gate compares them against the shape rather than against a literal
/// inside the script — which is the gap the script's own comment admits for the five state
/// enums, and this one is a request member, so a value the model dropped would be a call this
/// client makes and the service refuses.
///
/// [`crate::control::ops::VersionStatus`] is the typed spelling; this array is what the gate
/// reads, and the test at the bottom of that module asserts the two agree.
pub const IMAGE_VERSION_STATUSES: [&str; 2] = ["ACTIVE", "INACTIVE"];

/// The `HookState` enum, in the model's order. `DISABLED` first.
///
/// The two values every one of the six hook flags takes. Published so the gate compares them
/// against the shape, and because this is a **request** enum on six members of every
/// `CreateMicrovmImage`: a value the model dropped is a call this client makes and the service
/// refuses, on six fields at once.
///
/// [`crate::control::ops::HookState`] is the typed spelling and is what the request carries.
/// This array is what the gate reads, and the test in that module asserts the two agree — the
/// same arrangement [`IMAGE_VERSION_STATUSES`] has with `ops::VersionStatus`.
pub const HOOK_STATES: [&str; 2] = ["DISABLED", "ENABLED"];

/// The `MicrovmState` enum, in the model's order.
///
/// # Why the state enums live here now
///
/// They were pinned against literals inside `scripts/check-model-drift.py`, which the script's own
/// comment admitted: the gate verified the model against the *script*, with no reader in the
/// client at all. Issue #24 named the consequence — a `MicrovmState` AWS adds fails the gate
/// with no compile-time consequence for the polling loops in
/// [`crate::control::microvm`] that branch on states, so the gate's failure and the loop's
/// blindness were unrelated facts.
///
/// Moving the set here does not make a new state a compile error — a wire string cannot be
/// exhaustively matched, and narrowing a *response* enum is the mistake
/// [`crate::control::ops::MicrovmImageBuildSummaryWire::architecture`] documents at length. What
/// it does buy is a reader in the crate that the loops' own sets are checked against: the tests
/// below assert [`TERMINAL_STATES`] and [`DEAD_STATES`] are subsets of this, and
/// [`crate::control::microvm::SUSPEND_WANTED`] is checked against it too. So a state removed or
/// respelled by AWS fails the gate *and* fails a test naming the loop that branches on it.
pub const MICROVM_STATES: [&str; 6] = [
    "PENDING",
    "RUNNING",
    "SUSPENDING",
    "SUSPENDED",
    "TERMINATING",
    "TERMINATED",
];

/// The `MicrovmImageState` enum, in the model's order.
///
/// What [`crate::control::image::Image::is_ready`] and `is_failed` are decided against. The
/// three `*_FAILED` spellings are why `is_failed` is a substring test rather than a set: a
/// fourth added later is still recognised as a failure rather than polled to the deadline, and
/// the test below asserts that reading is still true of every member here.
pub const IMAGE_STATES: [&str; 9] = [
    "CREATING",
    "CREATED",
    "CREATE_FAILED",
    "UPDATING",
    "UPDATED",
    "UPDATE_FAILED",
    "DELETING",
    "DELETE_FAILED",
    "DELETED",
];

/// The `MicrovmImageVersionState` enum, in the model's order.
///
/// What `MicrovmImageVersionSummary.state` carries. Note it is **not** the same set as
/// [`IMAGE_STATES`] and not the same as [`BUILD_STATES`] either, which is the reason all three
/// are here: a habit that works for one is wrong for the others.
pub const IMAGE_VERSION_STATES: [&str; 7] = [
    "PENDING",
    "IN_PROGRESS",
    "SUCCESSFUL",
    "FAILED",
    "DELETING",
    "DELETED",
    "DELETE_FAILED",
];

/// The `BuildState` enum, in the model's order.
///
/// `PENDING` is the one TRAP-2's stall probe reads: **all** builds still `PENDING` past the
/// stall grace is the `clientToken` replay signature. The probe compares against a literal
/// today; the test below asserts that literal is a member of this set, which is the check that
/// notices a respelling — the same class of bug as `buildState`/`state`, which was dead against
/// live AWS for a review round.
pub const BUILD_STATES: [&str; 4] = ["PENDING", "IN_PROGRESS", "SUCCESSFUL", "FAILED"];

/// The `Chipset` enum, which is exactly this one value.
///
/// Now deserialized: [`crate::control::ops::MicrovmImageBuildSummaryWire::chipset`] and
/// `GetImageBuildResponseWire::chipset` both read it, so the drift-gate observation in issue #24
/// — "`Chipset` is drift-checked but deserialized nowhere" — was closed by the
/// build-introspection work that added the model's five missing required members to those two
/// shapes. Pinned here so the gate reads the value from the crate rather than from a literal in
/// its own file.
///
/// Carried as a `String` on the wire types rather than narrowed to this set, for the reason
/// those types state: a second chipset AWS adds must parse rather than fail a build readback.
pub const CHIPSETS: [&str; 1] = ["GRAVITON"];

/// `ResourcesList.max`.
///
/// Only one entry is accepted, so "give the VM two memory floors" is not a thing that
/// can be asked.
pub const MAX_RESOURCES: usize = 1;

/// `Create/Run/UpdateMicrovmImageRequestClientTokenString.max`, all three 128.
///
/// Not a cosmetic cap. Run tokens fold in the image identifier, which is a full ARN,
/// and an ap-northeast-1 ARN is long: the drift gate mints a token for the worst legal
/// scope and asserts it fits, which is a check that found a real pre-launch bug.
pub const MAX_CLIENT_TOKEN_LEN: usize = 128;

/// The `MicrovmImageState` values that mean "built and usable", as the model spells
/// them.
///
/// `UPDATED` is here even though this client never calls `UpdateMicrovmImage`: an
/// image someone else updated is usable, and treating it as still-building is a
/// 45-minute wait on a state that will never change.
pub const MODEL_IMAGE_READY_STATES: [&str; 2] = ["CREATED", "UPDATED"];

/// Spellings that are *not* in the 2025-09-09 `MicrovmImageState` enum, tolerated
/// when the service answers with one.
///
/// Held apart from [`MODEL_IMAGE_READY_STATES`] so the drift gate can check the
/// model-derived set *exactly* instead of being told that two of the three values it
/// cannot find are fine. If a future model adds either, move it up. Kept at all
/// because the service has answered differently across API versions, and a hard
/// equality check on one spelling is how a working build looks like a stalled one.
pub const TOLERATED_IMAGE_READY_STATES: [&str; 2] = ["ACTIVE", "AVAILABLE"];

/// Terminal `MicrovmState` values.
///
/// Reaching any of them *before* RUNNING means the VM died during startup, which for a
/// hook-serving daemon almost always means a lifecycle hook failed — and `stateReason`
/// is where the answer is (TRAP-8).
pub const TERMINAL_STATES: [&str; 4] = ["TERMINATED", "TERMINATING", "SUSPENDED", "SUSPENDING"];

/// The subset of [`TERMINAL_STATES`] from which nothing comes back.
///
/// Separate because SUSPENDED is a death *before* RUNNING and an ordinary waypoint on
/// the resume path — a resume that failed fast on SUSPENDED would fail on every
/// resume, since that is the state the VM is in when the call is made.
pub const DEAD_STATES: [&str; 2] = ["TERMINATED", "TERMINATING"];

/// Whether `name` satisfies [`IMAGE_NAME_PATTERN`] and [`MAX_IMAGE_NAME_LEN`].
///
/// A direct byte check: the pattern is `[a-zA-Z0-9-_]+`, four character ranges, which
/// `is_ascii_alphanumeric` states without a second place the pattern is written down.
/// The length check is here too because a caller asking "is
/// this name legal" means both constraints — they arrive from the same model shape and
/// the service rejects on either.
pub fn is_valid_image_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_IMAGE_NAME_LEN
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// Whether `group` satisfies [`LOG_GROUP_PATTERN`] and [`MAX_LOG_GROUP_LEN`].
///
/// A direct byte check over the pattern's character set — letters, digits, `_ - / . #` —
/// for the reason [`is_valid_image_name`] gives. The length is checked here too, because a
/// caller asking "is this group name legal" means both constraints: they arrive from the
/// same model shape and the service rejects on either.
pub fn is_valid_log_group(group: &str) -> bool {
    !group.is_empty()
        && group.len() <= MAX_LOG_GROUP_LEN
        && group
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'/' | b'.' | b'#'))
}

/// Whether every character of `component` is in [`TAG_COMPONENT_PATTERN`]'s set.
///
/// **The length is not checked here**, unlike [`is_valid_image_name`], and the asymmetry is
/// deliberate: a key and a value share this one pattern and have *different* ceilings (128
/// against 256) and different minima (1 against 0). A combined check would need to know which
/// side it was looking at, which is what [`crate::control::require_valid_tags`] does — it names
/// the key or the value in its message, so a caller with twenty tags is told which one and
/// which half.
///
/// # Why the character set and not the regex
///
/// The model's pattern is one `*`-quantified group, and an unanchored `*` matches the empty
/// string and every prefix — as a predicate it rejects nothing. The service nonetheless refuses
/// a key with a comma or a percent in it, so the constraint that has any content is the
/// character *set*, and that is what this decides. Unicode classes rather than byte ranges,
/// because `\p{L}` includes `é` and `日` and a tag key in a non-Latin script is a legitimate
/// tag: a byte-range matcher would refuse it.
///
/// `\p{Z}` is the separator class, which includes the ordinary space — so `"cost centre"` is a
/// legal tag key, unlike an image name.
pub fn is_valid_tag_component(component: &str) -> bool {
    component.chars().all(|c| {
        c.is_alphabetic()
            || c.is_numeric()
            // `\p{Z}`: separators. `char::is_whitespace` also covers `\n` and `\t`, which are
            // `\p{Cc}` control characters and outside the class — so they are excluded here
            // rather than folded in, and a tag key with a newline in it is refused.
            || matches!(c, ' ' | '\u{00a0}' | '\u{1680}' | '\u{2000}'..='\u{200a}'
                | '\u{2028}' | '\u{2029}' | '\u{202f}' | '\u{205f}' | '\u{3000}')
            || matches!(c, '_' | '.' | ':' | '/' | '=' | '+' | '-' | '@')
    })
}

/// Whether `arn` is structurally an IAM role ARN, per [`ROLE_ARN_PATTERN`].
///
/// # What this decides, and what it deliberately does not
///
/// The pattern is `arn:aws[a-z\-]*:iam::[0-9]{12}:role/?[a-zA-Z_0-9+=,.@\-_/]+`, and this checks
/// every part of it: the `arn:aws` prefix with its optional partition suffix, the literal
/// `:iam::`, **exactly twelve digits** of account id, and a `:role` segment followed by a
/// non-empty name in the permitted character set.
///
/// The twelve digits are the part worth having. Every real mistake this catches is one of three:
/// a role *name* passed where an ARN was wanted, an ARN for the wrong service (a Lambda function
/// ARN, say), or an account id with a digit dropped — and the third is the one no eyeball catches
/// and no other check would. It is also the reason this is not a `starts_with("arn:")` test the
/// way [`crate::control::ControlPlane::managed_base_versions`]'s is: that one distinguishes an
/// ARN from a bare name, and this one has a specific ARN grammar to hold a value to.
///
/// What it does not decide is whether the role **exists**, whether it is assumable, or whether it
/// grants the `logs:*` on `/aws/lambda-microvms/*` that a build role needs. Those are the failures
/// that actually happen most often, and none of them is knowable locally — IAM is not in this
/// crate's dependency set. So this guard is narrow on purpose: it converts a class of malformed
/// values into a local refusal and leaves the semantic failures to the service, which is the only
/// party that can answer them.
pub fn is_valid_role_arn(arn: &str) -> bool {
    // `arn:aws` then the optional partition suffix, `[a-z\-]*`, up to the next `:`.
    let Some(rest) = arn.strip_prefix("arn:aws") else {
        return false;
    };
    let Some(colon) = rest.find(':') else {
        return false;
    };
    let (partition_suffix, rest) = rest.split_at(colon);
    if !partition_suffix
        .bytes()
        .all(|b| b.is_ascii_lowercase() || b == b'-')
    {
        return false;
    }

    // `:iam::` then exactly twelve digits then `:role`.
    let Some(rest) = rest.strip_prefix(":iam::") else {
        return false;
    };
    let Some(colon) = rest.find(':') else {
        return false;
    };
    let (account, rest) = rest.split_at(colon);
    if account.len() != 12 || !account.bytes().all(|b| b.is_ascii_digit()) {
        return false;
    }
    let Some(name) = rest.strip_prefix(":role") else {
        return false;
    };
    // `role/?` — the slash is optional in the pattern, and the name that follows is `+`, so it
    // must be non-empty either way. `role/admin` and `roleadmin` both match the model's regex;
    // only the first is a real ARN, and refusing the second would be this client being stricter
    // than the service on a value it has no other reason to inspect.
    let name = name.strip_prefix('/').unwrap_or(name);
    !name.is_empty()
        && name.bytes().all(|b| {
            b.is_ascii_alphanumeric()
                || matches!(b, b'_' | b'+' | b'=' | b',' | b'.' | b'@' | b'-' | b'/')
        })
}

/// Every constant in this module as one JSON object, keyed as `sandbox.py` names them.
///
/// The drift gate's second source (TRAP-12). Emitted through `serde_json` rather than
/// formatted by hand so the object is a function of the values above, which is the
/// only version of this that cannot drift from them.
///
/// `MICROVM_REGIONS` is in here and is explicitly **not** model-backed — no service
/// model states it and no API answers the question (see [`crate::region`]). It is
/// published so the gate can at least check the Rust and Python lists against each
/// other, which is the one comparison available for a measured-only value.
///
/// # A list of pairs rather than one `json!({...})` literal, and that is forced
///
/// It was one object literal until issue #24's constants took the count past 40, at which
/// point `json!` fails to compile: "recursion limit reached while expanding
/// `$crate::json_internal!`", because the macro recurses once per entry against the crate's
/// 128-deep default. The two ways out were a crate-wide `#![recursion_limit]` bump — a global
/// knob turned for one macro in one function — and this. Each entry is still `json!(CONST)`, so
/// every value is a function of the `pub const` above it, which is the property the gate rests
/// on.
pub fn as_json() -> Value {
    let entries: Vec<(&str, Value)> = vec![
        ("MODEL_API_VERSION", json!(MODEL_API_VERSION)),
        (
            "MAX_RUN_HOOK_PAYLOAD_BYTES",
            json!(MAX_RUN_HOOK_PAYLOAD_BYTES),
        ),
        (
            "DOCUMENTED_RUN_HOOK_PAYLOAD_BYTES",
            json!(DOCUMENTED_RUN_HOOK_PAYLOAD_BYTES),
        ),
        ("MAX_IMAGE_NAME_LEN", json!(MAX_IMAGE_NAME_LEN)),
        ("IMAGE_NAME_PATTERN", json!(IMAGE_NAME_PATTERN)),
        ("MAX_VERSION_LEN", json!(MAX_VERSION_LEN)),
        ("VERSION_PATTERN", json!(VERSION_PATTERN)),
        ("MAX_NON_BLANK_LEN", json!(MAX_NON_BLANK_LEN)),
        ("NON_BLANK_PATTERN", json!(NON_BLANK_PATTERN)),
        ("MAX_IDENTIFIER_LEN", json!(MAX_IDENTIFIER_LEN)),
        ("MAX_IMAGE_ARN_LEN", json!(MAX_IMAGE_ARN_LEN)),
        ("MAX_TAG_KEY_LEN", json!(MAX_TAG_KEY_LEN)),
        ("MAX_TAG_VALUE_LEN", json!(MAX_TAG_VALUE_LEN)),
        ("TAG_COMPONENT_PATTERN", json!(TAG_COMPONENT_PATTERN)),
        ("MAX_LOG_GROUP_LEN", json!(MAX_LOG_GROUP_LEN)),
        ("LOG_GROUP_PATTERN", json!(LOG_GROUP_PATTERN)),
        ("MAX_LOG_STREAM_LEN", json!(MAX_LOG_STREAM_LEN)),
        ("LOG_STREAM_PATTERN", json!(LOG_STREAM_PATTERN)),
        ("MAX_USER_LOG_STREAM_LEN", json!(MAX_USER_LOG_STREAM_LEN)),
        ("MIN_ROLE_ARN_LEN", json!(MIN_ROLE_ARN_LEN)),
        ("MAX_ROLE_ARN_LEN", json!(MAX_ROLE_ARN_LEN)),
        ("ROLE_ARN_PATTERN", json!(ROLE_ARN_PATTERN)),
        ("MIN_PORT", json!(MIN_PORT)),
        ("MAX_PORT", json!(MAX_PORT)),
        ("MIN_IDLE_DURATION_SEC", json!(MIN_IDLE_DURATION_SEC)),
        ("MAX_DURATION_SEC", json!(MAX_DURATION_SEC)),
        (
            "MAX_MICROVM_HOOK_TIMEOUT_SEC",
            json!(MAX_MICROVM_HOOK_TIMEOUT_SEC),
        ),
        (
            "MAX_IMAGE_HOOK_TIMEOUT_SEC",
            json!(MAX_IMAGE_HOOK_TIMEOUT_SEC),
        ),
        ("MAX_HOOK_PORT", json!(MAX_HOOK_PORT)),
        ("CAPABILITIES", json!(CAPABILITIES)),
        ("ARCHITECTURES", json!(ARCHITECTURES)),
        ("MAX_NETWORK_CONNECTORS", json!(MAX_NETWORK_CONNECTORS)),
        (
            "MAX_IMAGE_EGRESS_CONNECTORS",
            json!(MAX_IMAGE_EGRESS_CONNECTORS),
        ),
        ("IMAGE_VERSION_STATUSES", json!(IMAGE_VERSION_STATUSES)),
        ("HOOK_STATES", json!(HOOK_STATES)),
        ("MICROVM_STATES", json!(MICROVM_STATES)),
        ("IMAGE_STATES", json!(IMAGE_STATES)),
        ("IMAGE_VERSION_STATES", json!(IMAGE_VERSION_STATES)),
        ("BUILD_STATES", json!(BUILD_STATES)),
        ("CHIPSETS", json!(CHIPSETS)),
        ("MAX_RESOURCES", json!(MAX_RESOURCES)),
        ("MAX_CLIENT_TOKEN_LEN", json!(MAX_CLIENT_TOKEN_LEN)),
        ("MODEL_IMAGE_READY_STATES", json!(MODEL_IMAGE_READY_STATES)),
        (
            "TOLERATED_IMAGE_READY_STATES",
            json!(TOLERATED_IMAGE_READY_STATES),
        ),
        ("TERMINAL_STATES", json!(TERMINAL_STATES)),
        ("DEAD_STATES", json!(DEAD_STATES)),
        (
            "MICROVM_REGIONS",
            json!(
                MICROVM_REGIONS
                    .iter()
                    .map(|region| region.as_str())
                    .collect::<Vec<_>>()
            ),
        ),
        (
            "SIZE_CLASSES",
            json!(
                crate::sizing::SIZE_CLASSES
                    .iter()
                    .map(|row| json!({
                        "baseline_mib": row.baseline_mib,
                        "baseline_vcpu": row.baseline_vcpu,
                        "peak_mib": row.peak_mib,
                        "peak_vcpu": row.peak_vcpu,
                    }))
                    .collect::<Vec<_>>()
            ),
        ),
    ];
    Value::Object(
        entries
            .into_iter()
            .map(|(key, value)| (key.to_string(), value))
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The key set the drift gate reads, pinned.
    ///
    /// This is the guard for the one failure mode a rename here produces: the script
    /// looks a key up, does not find it, and either crashes or — worse, depending on
    /// how it is written — reports nothing disagreed. Compilation cannot catch that,
    /// because the coupling is a string in another language's file.
    #[test]
    fn as_json_carries_every_key_the_drift_gate_reads() {
        let emitted = as_json();
        let object = emitted.as_object().expect("an object");
        let mut keys: Vec<&str> = object.keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            [
                "ARCHITECTURES",
                "BUILD_STATES",
                "CAPABILITIES",
                "CHIPSETS",
                "DEAD_STATES",
                "DOCUMENTED_RUN_HOOK_PAYLOAD_BYTES",
                "HOOK_STATES",
                "IMAGE_NAME_PATTERN",
                "IMAGE_STATES",
                "IMAGE_VERSION_STATES",
                "IMAGE_VERSION_STATUSES",
                "LOG_GROUP_PATTERN",
                "LOG_STREAM_PATTERN",
                "MAX_CLIENT_TOKEN_LEN",
                "MAX_DURATION_SEC",
                "MAX_HOOK_PORT",
                "MAX_IDENTIFIER_LEN",
                "MAX_IMAGE_ARN_LEN",
                "MAX_IMAGE_EGRESS_CONNECTORS",
                "MAX_IMAGE_HOOK_TIMEOUT_SEC",
                "MAX_IMAGE_NAME_LEN",
                "MAX_LOG_GROUP_LEN",
                "MAX_LOG_STREAM_LEN",
                "MAX_MICROVM_HOOK_TIMEOUT_SEC",
                "MAX_NETWORK_CONNECTORS",
                "MAX_NON_BLANK_LEN",
                "MAX_PORT",
                "MAX_RESOURCES",
                "MAX_ROLE_ARN_LEN",
                "MAX_RUN_HOOK_PAYLOAD_BYTES",
                "MAX_TAG_KEY_LEN",
                "MAX_TAG_VALUE_LEN",
                "MAX_USER_LOG_STREAM_LEN",
                "MAX_VERSION_LEN",
                "MICROVM_REGIONS",
                "MICROVM_STATES",
                "MIN_IDLE_DURATION_SEC",
                "MIN_PORT",
                "MIN_ROLE_ARN_LEN",
                "MODEL_API_VERSION",
                "MODEL_IMAGE_READY_STATES",
                "NON_BLANK_PATTERN",
                "ROLE_ARN_PATTERN",
                "SIZE_CLASSES",
                "TAG_COMPONENT_PATTERN",
                "TERMINAL_STATES",
                "TOLERATED_IMAGE_READY_STATES",
                "VERSION_PATTERN",
            ]
        );
    }

    /// The values, as literals, against the same numbers `sandbox.py` carries. Written
    /// out rather than compared against the constants, so this test disagrees with a
    /// changed constant instead of following it.
    #[test]
    fn the_emitted_values_are_the_measured_ones() {
        let emitted = as_json();
        assert_eq!(emitted["MODEL_API_VERSION"], "2025-09-09");
        assert_eq!(emitted["MAX_RUN_HOOK_PAYLOAD_BYTES"], 4096);
        assert_eq!(emitted["MAX_IMAGE_NAME_LEN"], 64);
        assert_eq!(emitted["IMAGE_NAME_PATTERN"], "[a-zA-Z0-9-_]+");
        assert_eq!(emitted["MAX_VERSION_LEN"], 2048);
        assert_eq!(emitted["VERSION_PATTERN"], "[^\\s]+");
        assert_eq!(emitted["MAX_NON_BLANK_LEN"], 2048);
        assert_eq!(emitted["NON_BLANK_PATTERN"], "[^\\s]+");
        assert_eq!(emitted["MAX_IDENTIFIER_LEN"], 256);
        assert_eq!(emitted["MAX_IMAGE_ARN_LEN"], 2048);
        assert_eq!(emitted["MAX_TAG_KEY_LEN"], 128);
        assert_eq!(emitted["MAX_TAG_VALUE_LEN"], 256);
        assert_eq!(
            emitted["TAG_COMPONENT_PATTERN"],
            "([\\p{L}\\p{Z}\\p{N}_.:/=+\\-@]*)"
        );
        assert_eq!(emitted["MAX_LOG_GROUP_LEN"], 512);
        assert_eq!(emitted["LOG_GROUP_PATTERN"], "[a-zA-Z0-9_\\-/.#]+");
        assert_eq!(emitted["MAX_LOG_STREAM_LEN"], 512);
        assert_eq!(emitted["LOG_STREAM_PATTERN"], "[^:*]*");
        assert_eq!(emitted["MAX_USER_LOG_STREAM_LEN"], 495);
        assert_eq!(emitted["MIN_ROLE_ARN_LEN"], 20);
        assert_eq!(emitted["MAX_ROLE_ARN_LEN"], 2048);
        assert_eq!(
            emitted["ROLE_ARN_PATTERN"],
            "arn:aws[a-z\\-]*:iam::[0-9]{12}:role/?[a-zA-Z_0-9+=,.@\\-_/]+"
        );
        assert_eq!(emitted["MIN_PORT"], 1);
        assert_eq!(emitted["MAX_PORT"], 65_535);
        assert_eq!(emitted["MIN_IDLE_DURATION_SEC"], 60);
        assert_eq!(emitted["MAX_DURATION_SEC"], 28_800);
        assert_eq!(emitted["MAX_MICROVM_HOOK_TIMEOUT_SEC"], 60);
        assert_eq!(emitted["MAX_IMAGE_HOOK_TIMEOUT_SEC"], 3_600);
        assert_eq!(emitted["MAX_HOOK_PORT"], 65_535);
        assert_eq!(emitted["CAPABILITIES"], json!(["ALL"]));
        assert_eq!(emitted["ARCHITECTURES"], json!(["ARM_64"]));
        assert_eq!(emitted["MAX_NETWORK_CONNECTORS"], 10);
        // 1, not 10. The image-level list and the VM-level one differ by an order of
        // magnitude, and this is the assertion that says so in one place.
        assert_eq!(emitted["MAX_IMAGE_EGRESS_CONNECTORS"], 1);
        assert_eq!(
            emitted["IMAGE_VERSION_STATUSES"],
            json!(["ACTIVE", "INACTIVE"])
        );
        assert_eq!(emitted["HOOK_STATES"], json!(["DISABLED", "ENABLED"]));
        assert_eq!(
            emitted["MICROVM_STATES"],
            json!([
                "PENDING",
                "RUNNING",
                "SUSPENDING",
                "SUSPENDED",
                "TERMINATING",
                "TERMINATED"
            ])
        );
        assert_eq!(
            emitted["IMAGE_STATES"],
            json!([
                "CREATING",
                "CREATED",
                "CREATE_FAILED",
                "UPDATING",
                "UPDATED",
                "UPDATE_FAILED",
                "DELETING",
                "DELETE_FAILED",
                "DELETED"
            ])
        );
        assert_eq!(
            emitted["IMAGE_VERSION_STATES"],
            json!([
                "PENDING",
                "IN_PROGRESS",
                "SUCCESSFUL",
                "FAILED",
                "DELETING",
                "DELETED",
                "DELETE_FAILED"
            ])
        );
        assert_eq!(
            emitted["BUILD_STATES"],
            json!(["PENDING", "IN_PROGRESS", "SUCCESSFUL", "FAILED"])
        );
        assert_eq!(emitted["CHIPSETS"], json!(["GRAVITON"]));
        assert_eq!(emitted["MAX_RESOURCES"], 1);
        assert_eq!(emitted["MAX_CLIENT_TOKEN_LEN"], 128);
        assert_eq!(
            emitted["TERMINAL_STATES"],
            json!(["TERMINATED", "TERMINATING", "SUSPENDED", "SUSPENDING"])
        );
        assert_eq!(emitted["DEAD_STATES"], json!(["TERMINATED", "TERMINATING"]));
        assert_eq!(
            emitted["MODEL_IMAGE_READY_STATES"],
            json!(["CREATED", "UPDATED"])
        );
        assert_eq!(
            emitted["TOLERATED_IMAGE_READY_STATES"],
            json!(["ACTIVE", "AVAILABLE"])
        );
        assert_eq!(
            emitted["MICROVM_REGIONS"],
            json!([
                "us-east-1",
                "us-east-2",
                "us-west-2",
                "eu-west-1",
                "ap-northeast-1"
            ])
        );
    }

    /// The object survives a serialize/parse cycle unchanged, which is what the gate
    /// does to it: the CLI prints it and the script calls `json.loads`. A value that
    /// only compares equal in memory is a value the gate never sees.
    #[test]
    fn the_emitted_object_round_trips_through_a_string() {
        let emitted = as_json();
        let text = serde_json::to_string(&emitted).expect("serializes");
        let parsed: Value = serde_json::from_str(&text).expect("parses");
        assert_eq!(parsed, emitted);
    }

    /// The dead states are a subset of the terminal ones, which is the relationship
    /// the two lists exist to express. A DEAD_STATES entry that is not terminal would
    /// make the launch guard and the resume guard disagree about what SUSPENDED means.
    #[test]
    fn every_dead_state_is_also_a_terminal_state() {
        for state in DEAD_STATES {
            assert!(TERMINAL_STATES.contains(&state), "{state} is not terminal");
        }
        assert!(
            !DEAD_STATES.contains(&"SUSPENDED"),
            "SUSPENDED is terminal but not dead: it is the state a resume is called from"
        );
    }

    /// **Issue #24's wrong-by-10x hazard.** The two connector ceilings are different
    /// numbers, and the image-level one is the smaller.
    ///
    /// Stated as an inequality rather than only as two literals above, because the failure
    /// mode is a future edit that *unifies* them — "there is only one connector limit, delete
    /// the duplicate" — and the permissive direction is the one that reaches the wire. A
    /// caller who applied `MAX_NETWORK_CONNECTORS` to an image's `egressNetworkConnectors`
    /// would accept ten and have the service refuse two.
    #[test]
    fn the_image_level_connector_ceiling_is_one_and_not_the_vm_levels_ten() {
        assert_eq!(MAX_IMAGE_EGRESS_CONNECTORS, 1);
        assert_eq!(MAX_NETWORK_CONNECTORS, 10);
        // Read out of `as_json()` rather than compared as two `const`s, because clippy's
        // `assertions_on_constants` is right that a `const < const` assertion is decided at
        // compile time and proves nothing at runtime. Going through the emitted object makes it
        // a real comparison — and it is the object the drift gate reads, so this asserts the
        // published pair rather than two literals a reader could unify.
        let emitted = as_json();
        let image = emitted["MAX_IMAGE_EGRESS_CONNECTORS"]
            .as_u64()
            .expect("a number");
        let vm = emitted["MAX_NETWORK_CONNECTORS"]
            .as_u64()
            .expect("a number");
        assert!(
            image < vm,
            "collapsing the two would be permissive by 10x on the image-level list: \
             {image} against {vm}"
        );
    }

    /// The version statuses are exactly the two the model declares, spelled uppercase.
    ///
    /// The typed request-side spelling is checked against this array in
    /// `crate::control::ops`, so the two cannot drift; here what is pinned is the pair itself
    /// and the fact that neither is a lowercase or a past-tense variant of the other.
    #[test]
    fn the_two_version_statuses_are_active_and_inactive() {
        assert_eq!(IMAGE_VERSION_STATUSES, ["ACTIVE", "INACTIVE"]);
        for status in IMAGE_VERSION_STATUSES {
            assert_eq!(status, status.to_uppercase(), "{status}");
        }
        assert!(
            !IMAGE_VERSION_STATUSES.contains(&"DISABLED"),
            "DISABLED is HookState's spelling, not this enum's — mixing them is a \
             ValidationException on the only member the update request has"
        );
    }

    /// The ready-state sets are disjoint, so the gate can check the model-derived one
    /// exactly. An overlap would mean a tolerated spelling was being presented as
    /// model-backed.
    #[test]
    fn the_model_and_tolerated_ready_states_do_not_overlap() {
        for state in TOLERATED_IMAGE_READY_STATES {
            assert!(
                !MODEL_IMAGE_READY_STATES.contains(&state),
                "{state} is claimed as both model-backed and tolerated"
            );
        }
    }

    /// The pattern's four ranges and nothing else. The two rejected characters are the
    /// separators a caller reaching for a namespaced name writes first, which is why
    /// they are named.
    #[test]
    fn an_image_name_takes_letters_digits_hyphen_and_underscore_only() {
        assert!(is_valid_image_name("agentd-conformance_01"));
        assert!(is_valid_image_name("A"));
        assert!(is_valid_image_name(&"a".repeat(MAX_IMAGE_NAME_LEN)));

        assert!(!is_valid_image_name(""), "min is 1");
        assert!(
            !is_valid_image_name(&"a".repeat(MAX_IMAGE_NAME_LEN + 1)),
            "65 characters is one past the ceiling"
        );
        for rejected in [
            "my.image",
            "team/image",
            "my image",
            "img!",
            "imagé",
            "img\n",
        ] {
            assert!(!is_valid_image_name(rejected), "{rejected}");
        }
    }

    /// A multi-byte character cannot slip past the length check by being counted as
    /// one: the model's max is on bytes, and the character class rejects it anyway.
    #[test]
    fn a_multibyte_name_is_refused_by_the_character_class_not_by_luck() {
        assert!(!is_valid_image_name("é"));
        assert!(!is_valid_image_name("日本語"));
    }

    /// The log-group matcher takes exactly the model's set — letters, digits, `_ - / . #` —
    /// and both length bounds.
    ///
    /// `/aws/lambda-microvms/img` is the shape the service itself creates, so it has to
    /// pass; a colon is the character a caller pasting an ARN brings in first, so it is the
    /// named rejection.
    #[test]
    fn a_log_group_takes_the_models_character_set_and_bounds() {
        assert!(is_valid_log_group("/aws/lambda-microvms/img"));
        assert!(is_valid_log_group("builds_2026.08#a"));
        assert!(is_valid_log_group(&"a".repeat(MAX_LOG_GROUP_LEN)));

        assert!(!is_valid_log_group(""), "min is 1");
        assert!(
            !is_valid_log_group(&"a".repeat(MAX_LOG_GROUP_LEN + 1)),
            "513 characters is one past the ceiling"
        );
        for rejected in ["arn:aws:logs", "a group", "grp*", "grp\n", "imagé"] {
            assert!(!is_valid_log_group(rejected), "{rejected}");
        }
    }

    /// The user-facing stream ceiling leaves exactly the discriminator's room under the
    /// wire ceiling: `/` plus sixteen hex characters.
    ///
    /// Pinned as arithmetic so a change to the nonce width (`token.rs`'s eight bytes) or to
    /// either ceiling fails here rather than as a `ValidationException` after the artifact
    /// upload — the same shape as `token.rs`'s own cap test.
    #[test]
    fn the_user_stream_ceiling_leaves_room_for_the_slash_and_sixteen_hex() {
        assert_eq!(MAX_USER_LOG_STREAM_LEN + 1 + 16, MAX_LOG_STREAM_LEN);
    }

    /// **Issue #24's documentation trap, pinned as a comparison.** The model's own prose says
    /// 16,384 and its shape says 4096, and the shape is the one that is enforced.
    ///
    /// The pair is asserted rather than only commented because the failure mode is a future
    /// reader "correcting" [`MAX_RUN_HOOK_PAYLOAD_BYTES`] from the model's documentation string —
    /// which they would be able to cite while doing it. This test is what makes that a red build
    /// rather than a plausible-looking commit, and the 4x relationship is asserted so the shape of
    /// the hazard is in the test and not only in a doc comment.
    ///
    /// **Guard proof.** Set `MAX_RUN_HOOK_PAYLOAD_BYTES` to 16_384 and this fails on the
    /// inequality *and* on the ratio; delete `DOCUMENTED_RUN_HOOK_PAYLOAD_BYTES` and it does not
    /// compile.
    #[test]
    fn the_models_own_prose_claims_four_times_the_ceiling_its_shape_states() {
        let emitted = as_json();
        let real = emitted["MAX_RUN_HOOK_PAYLOAD_BYTES"]
            .as_u64()
            .expect("a number");
        let documented = emitted["DOCUMENTED_RUN_HOOK_PAYLOAD_BYTES"]
            .as_u64()
            .expect("a number");
        assert_eq!(real, 4096, "the shape, which is what the service enforces");
        assert_eq!(
            documented, 16_384,
            "the model's documentation string on RunMicrovmRequest.runHookPayload"
        );
        assert!(
            documented > real,
            "the model's prose is permissive against its own shape: {documented} against {real}"
        );
        assert_eq!(
            documented / real,
            4,
            "wrong by 4x, in the direction that tells a caller four times as much secret \
             material fits as actually does"
        );
    }

    /// **The model's identifier contradiction, pinned.** A legal `MicrovmImageArn` can be longer
    /// than a legal `MicrovmImageIdentifier`, so the service may answer with a value it would
    /// refuse as a request.
    ///
    /// Held as an inequality because the fact is a *relationship* between two shapes and no
    /// single shape states it. If AWS ever narrows `MicrovmImageArn` to 256 this goes red, which
    /// is the signal that the paragraph in [`MAX_IDENTIFIER_LEN`] about resolving the
    /// contradiction should be deleted.
    ///
    /// The second half is the reason it is theoretical in practice: a real ARN in the
    /// longest-named MicroVM region is nowhere near 256, and that arithmetic is measured here
    /// rather than asserted as a habit.
    #[test]
    fn a_legal_image_arn_can_be_longer_than_a_legal_identifier() {
        let emitted = as_json();
        let identifier = emitted["MAX_IDENTIFIER_LEN"].as_u64().expect("a number");
        let arn = emitted["MAX_IMAGE_ARN_LEN"].as_u64().expect("a number");
        assert!(
            arn > identifier,
            "the model permits a {arn}-character image ARN and a {identifier}-character \
             identifier, so a legal response value can be an illegal request value"
        );

        // The worst legal real ARN: the longest region name, a twelve-digit account, and a
        // 64-character image name. Built rather than written as a literal, so the arithmetic is
        // the test's rather than mine.
        let longest_region = MICROVM_REGIONS
            .iter()
            .map(|region| region.as_str().len())
            .max()
            .expect("five regions");
        let worst = format!(
            "arn:aws:lambda:{}:{}:microvm-image:{}",
            "x".repeat(longest_region),
            "9".repeat(12),
            "a".repeat(MAX_IMAGE_NAME_LEN),
        );
        assert!(
            worst.len() <= MAX_IDENTIFIER_LEN,
            "the worst legal image ARN is {} characters, which must fit the {MAX_IDENTIFIER_LEN} \
             identifier bound or the contradiction would be reachable rather than theoretical",
            worst.len(),
        );
    }

    /// Every state set the client's loops branch on is a subset of the model's enum for it.
    ///
    /// **This is what moving the five state enums out of the drift script buys.** They were
    /// pinned against literals inside `scripts/check-model-drift.py`, so a `MicrovmState` AWS
    /// respelled failed the gate with no consequence for the code that branches on it (issue
    /// #24). With the sets here, the same respelling fails this test as well, and this test names
    /// the loop.
    ///
    /// **Guard proof.** Respell one member of `MICROVM_STATES` — `"SUSPENDED"` to
    /// `"SUSPEND_PENDING"`, say — and this fails naming `TERMINAL_STATES`, `DEAD_STATES`, and
    /// `SUSPEND_WANTED` in turn. Drop `"PENDING"` from `BUILD_STATES` and the stall-probe
    /// assertion fails.
    #[test]
    fn every_state_set_the_polling_loops_use_is_a_subset_of_the_models_enum() {
        for state in TERMINAL_STATES {
            assert!(
                MICROVM_STATES.contains(&state),
                "TERMINAL_STATES has {state}, which is not a MicrovmState — \
                 ControlPlane::wait_for_running fails fast on it and would now never fire"
            );
        }
        for state in DEAD_STATES {
            assert!(
                MICROVM_STATES.contains(&state),
                "DEAD_STATES has {state}, which is not a MicrovmState — the resume path passes \
                 this set as its fail-fast list"
            );
        }
        for state in crate::control::microvm::SUSPEND_WANTED {
            assert!(
                MICROVM_STATES.contains(&state),
                "SUSPEND_WANTED has {state}, which is not a MicrovmState — a suspend would wait \
                 for a state the service cannot report and time out"
            );
        }
        for state in MODEL_IMAGE_READY_STATES {
            assert!(
                IMAGE_STATES.contains(&state),
                "MODEL_IMAGE_READY_STATES has {state}, which is not a MicrovmImageState — \
                 Image::is_ready would never answer true for it and a built image would poll to \
                 the 45-minute deadline"
            );
        }
        for state in TOLERATED_IMAGE_READY_STATES {
            assert!(
                !IMAGE_STATES.contains(&state),
                "{state} is in the model's enum now, so it is no longer a tolerated legacy \
                 spelling — promote it to MODEL_IMAGE_READY_STATES"
            );
        }
        // TRAP-2's stall probe compares `build_state == "PENDING"` against a literal. This is the
        // check that notices the literal going stale, which is the same class of bug as
        // `buildState`/`state` — dead against live AWS while its unit test passed.
        assert!(
            BUILD_STATES.contains(&"PENDING"),
            "the stall probe's replay signature is 'every build still PENDING'"
        );
        // `Image::is_failed` is a substring test on FAILED rather than a set, so that a fourth
        // failure spelling is still recognised. That reading has to stay true of the enum.
        let failed: Vec<&str> = IMAGE_STATES
            .iter()
            .copied()
            .filter(|state| state.contains("FAILED"))
            .collect();
        assert_eq!(
            failed,
            ["CREATE_FAILED", "UPDATE_FAILED", "DELETE_FAILED"],
            "the three spellings the substring test covers"
        );
    }

    /// The tag character set: letters and digits in any script, spaces, and eight punctuation
    /// marks. Commas, `#`, `%`, and newlines are out.
    ///
    /// The non-Latin cases are the ones a byte-range matcher would have got wrong, and they are
    /// legitimate tags — `\p{L}` is every letter, not every ASCII letter. The newline case is the
    /// one `char::is_whitespace` would have got wrong in the other direction: `\n` is `\p{Cc}`,
    /// a control character, not the `\p{Z}` separator class the pattern names.
    #[test]
    fn a_tag_component_takes_any_script_and_eight_punctuation_marks() {
        for accepted in [
            "owner",
            "cost-centre",
            "cost centre",
            "team/agents",
            "a.b:c=d+e-f@g_h",
            "日本語",
            "café",
            "123",
            "",
        ] {
            assert!(is_valid_tag_component(accepted), "{accepted:?}");
        }
        for rejected in [
            "a,b",
            "a#b",
            "50%",
            "line\nbreak",
            "tab\there",
            "a!b",
            "a(b",
        ] {
            assert!(!is_valid_tag_component(rejected), "{rejected:?}");
        }
    }

    /// The role-ARN matcher, and the twelve-digit account id is the part that earns it.
    ///
    /// Three real mistakes and one near-miss: a role name where an ARN was wanted, an ARN for
    /// another service, an account id with a digit dropped, and a `roleadmin` with no slash —
    /// which the model's regex accepts, so this does too rather than being stricter than the
    /// service on a value it has no other reason to inspect.
    #[test]
    fn a_role_arn_needs_twelve_account_digits_and_an_iam_role_path() {
        for accepted in [
            "arn:aws:iam::123456789012:role/build",
            "arn:aws:iam::123456789012:role/service-role/path/to/role",
            "arn:aws-us-gov:iam::123456789012:role/build",
            "arn:aws-cn:iam::123456789012:role/build",
            "arn:aws:iam::123456789012:role/name+with=every,allowed.char@here_x-y",
            // No slash: the model's `role/?` makes it optional, so this matches the pattern.
            "arn:aws:iam::123456789012:roleadmin",
        ] {
            assert!(is_valid_role_arn(accepted), "{accepted}");
        }

        for rejected in [
            // A role *name*, which is the most common mistake.
            "bonk-sandbox-microvm-build",
            // The right shape for the wrong service.
            "arn:aws:lambda:us-east-1:123456789012:function:handler",
            // Eleven digits. The one nothing else catches.
            "arn:aws:iam::12345678901:role/build",
            // Thirteen.
            "arn:aws:iam::1234567890123:role/build",
            // Non-digits where the account goes.
            "arn:aws:iam::abcdefghijkl:role/build",
            // A region where IAM has none, which is what a copied Lambda ARN looks like.
            "arn:aws:iam:us-east-1:123456789012:role/build",
            // Nothing after `:role`.
            "arn:aws:iam::123456789012:role",
            "arn:aws:iam::123456789012:role/",
            // A user, not a role.
            "arn:aws:iam::123456789012:user/laith",
            // Uppercase partition, which the pattern's `[a-z\-]*` forbids.
            "arn:AWS:iam::123456789012:role/build",
            "",
            "arn:",
        ] {
            assert!(!is_valid_role_arn(rejected), "{rejected}");
        }
    }

    /// The real conformance-account role ARNs match, which is what stops this guard from being a
    /// pattern that refuses every legitimate value.
    ///
    /// Written out as the account's actual ARNs rather than as a synthetic `role/build`: a
    /// matcher that accepts a fixture and refuses production is the failure mode a guard like this
    /// has, and it fires after the artifact upload.
    #[test]
    fn the_conformance_accounts_own_role_arns_are_accepted() {
        for real in [
            "arn:aws:iam::392583147479:role/bonk-sandbox-microvm-build",
            "arn:aws:iam::392583147479:role/bonk-sandbox-microvm-execution",
        ] {
            assert!(is_valid_role_arn(real), "{real}");
            assert!(real.len() >= MIN_ROLE_ARN_LEN, "{real}");
            assert!(real.len() <= MAX_ROLE_ARN_LEN, "{real}");
        }
    }

    /// The two port constants bracket what a `u16` can hold, with the floor at 1 and not 0.
    ///
    /// `MAX_PORT` equalling `u16::MAX` is the fact that makes the ceiling closed by the type and
    /// the floor the only live half — which is what the guard's docs claim, asserted rather than
    /// stated.
    #[test]
    fn the_port_floor_is_one_and_the_ceiling_is_what_a_u16_holds() {
        assert_eq!(MIN_PORT, 1, "0 is 'let the kernel choose', not a port");
        assert_eq!(MAX_PORT, u16::MAX);
        assert_eq!(
            u32::from(MAX_PORT),
            MAX_HOOK_PORT,
            "PortNumber and HooksPortInteger agree today; they are two shapes and this is where \
             it is noticed if they stop"
        );
    }

    /// The two `[^\s]+` shapes agree today, and the point of two constants is that this test is
    /// what says so rather than one constant making it unaskable.
    #[test]
    fn the_version_and_non_blank_shapes_agree_today_and_are_still_two_constants() {
        assert_eq!(MAX_VERSION_LEN, MAX_NON_BLANK_LEN);
        assert_eq!(VERSION_PATTERN, NON_BLANK_PATTERN);
        // Read out of the emitted object rather than compared as two `const`s, for the reason
        // the connector-ceiling test gives: a `const == const` assertion is decided at compile
        // time and proves nothing about the values the gate reads.
        let emitted = as_json();
        assert_eq!(emitted["MAX_VERSION_LEN"], emitted["MAX_NON_BLANK_LEN"]);
        assert_eq!(emitted["VERSION_PATTERN"], emitted["NON_BLANK_PATTERN"]);
    }
}
