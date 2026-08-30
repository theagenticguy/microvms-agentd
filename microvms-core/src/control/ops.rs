// SPDX-License-Identifier: Apache-2.0
//! The wire shapes, transcribed member-for-member from the pinned service model.
//!
//! # The honest-fake rule
//!
//! Every field name here is copied from `service-2.json` for API version
//! [`crate::constants::MODEL_API_VERSION`], and every deserializer test in this file
//! parses **literal JSON in the model's spelling** rather than the output of this
//! module's own serializer. That is not a style preference; it is the fix for a specific
//! bug that shipped.
//!
//! `MicrovmImageBuildSummary` carries a member called `buildState`. It has no member
//! called `state`. The Python client read `b.get("state")`, which returned `None` from
//! every real response — so the stall probe that is the only thing separating a wedged
//! image from a slow build was **dead against live AWS while passing its unit test**,
//! because the test's fake returned `{"state": "PENDING"}`. The fake shared the client's
//! own misreading, so the two agreed with each other and with nothing else. Found
//! 2026-08-07 by diffing against the service model, after a ~15-hour wedge that this
//! guard existed to catch and did not.
//!
//! A round-trip test through this module's `Serialize` would have exactly the same
//! blind spot. So the tests below hold JSON.
//!
//! # What is `Option` and what is not
//!
//! The model's `required` list, not a guess about what the service usually sends. A
//! member the model marks required is a bare field; everything else is `Option`. Two
//! consequences worth stating:
//!
//! * `stateReason` is optional everywhere it appears, which is why TRAP-8's error says
//!   "no stateReason" rather than printing an empty string — the absence is information.
//! * `RunMicrovmRequest` requires only `imageIdentifier`. `executionRoleArn` is
//!   optional *on the wire* even though every real launch needs one.
//!
//! # Unknown members are dropped, deliberately
//!
//! No `#[serde(deny_unknown_fields)]` and no catch-all map. A member AWS adds to a
//! response must not fail a deserialization — that would turn an additive service change
//! into a broken client — and this crate has no consumer for a field it does not know
//! about. The drift gate (TRAP-12) is where a new member gets noticed.

use serde::{Deserialize, Serialize};

// ── requests ────────────────────────────────────────────────────────────────

/// `CreateMicrovmImageRequest`.
///
/// `clientToken` is `String` rather than `Option<String>` because this client always
/// sends one and it is always minted here (TRAP-1) — see [`crate::control::token`].
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateMicrovmImageWire {
    pub name: String,
    pub base_image_arn: String,
    /// `Version`, absent unless the caller pinned one.
    ///
    /// Omitted rather than sent empty when nobody pinned, because that is the difference
    /// between "whatever the service defaults to" and a `ValidationException` on a blank
    /// `Version` (min 1, pattern `[^\s]+`). The default has already moved once — the managed
    /// base carries versions `"0"` and `"1"` (docs/PLATFORM.md, 'The managed base image has
    /// two versions') — so an unpinned build is not reproducible, which is what this member
    /// exists to fix.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_image_version: Option<String>,
    pub build_role_arn: String,
    pub code_artifact: CodeArtifact,
    pub cpu_configurations: Vec<CpuConfiguration>,
    pub resources: Vec<Resources>,
    pub hooks: Hooks,
    /// `CapabilityList`. Omitted entirely rather than sent empty when identity repair
    /// was not asked for: the enum's only value is `ALL`, and an empty list is a
    /// different request from an absent member.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub additional_os_capabilities: Option<Vec<String>>,
    /// `Logging`, absent unless the caller configured build logging.
    ///
    /// Omitted rather than sent as `null` or `{}` when nothing was configured: an absent
    /// member takes the service default (a service-created log group under
    /// `/aws/lambda-microvms/<image-name>` with random stream names), and the union's own
    /// documentation says to specify exactly one member — an empty object is a malformed
    /// union, not a default.
    ///
    /// The `logStream` this carries is never a caller's value verbatim; see
    /// [`CloudWatchLogging::log_stream`] and the discriminator rule in
    /// [`crate::control::ControlPlane::create_image`].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub logging: Option<Logging>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tags: Option<std::collections::BTreeMap<String, String>>,
    /// The minted idempotency token.
    ///
    /// `pub(crate)` while every sibling field is `pub`, and that asymmetry is TRAP-1's last
    /// mile. `CreateImageRequest` has no token field, but this type is the wire shape behind
    /// it — and a `pub` field here would let a caller outside this module build the request
    /// body directly with a token of their choosing, which is the wedge with one extra step.
    /// Narrowing the visibility means the only way to populate it is
    /// [`crate::control::token::create_token`], from inside the crate.
    pub(crate) client_token: String,
}

/// `CodeArtifact`, a union whose only member this client uses is `uri`.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CodeArtifact {
    pub uri: String,
}

/// `Logging`, the model's tagged union: exactly one of `disabled` or `cloudWatch`.
///
/// The same wire shape as [`PortSpecification`] and modelled the same way: a Smithy union
/// serialises as **one key** — `{"disabled": {}}` or `{"cloudWatch": {...}}` — not a
/// discriminator field, so `#[serde(untagged)]` plus a `rename` per variant is what keeps
/// serde's default `{"Disabled": ...}` spelling off the wire.
///
/// Both `CreateMicrovmImageRequest` and `RunMicrovmRequest` carry a `logging` member of
/// this shape in the model; this client binds only the image-build side (issue #98 — build
/// logs are where the three-VM/three-stream topology and the exact-name collision live).
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(untagged)]
pub enum Logging {
    /// `LoggingDisabled`: no logs at all. An empty struct rather than a unit variant for
    /// [`AllPorts`]'s reason — the wire form is `{}` and a unit serialises to `null`,
    /// which the service rejects as a malformed union member.
    Disabled {
        #[serde(rename = "disabled")]
        disabled: LoggingDisabled,
    },
    /// `CloudWatchLogging`: a caller-named group and, optionally, stream.
    CloudWatch {
        #[serde(rename = "cloudWatch")]
        cloud_watch: CloudWatchLogging,
    },
}

impl Logging {
    /// Logging off.
    pub fn disabled() -> Self {
        Self::Disabled {
            disabled: LoggingDisabled {},
        }
    }

    /// Logs to `log_group`, with the service naming the streams inside it.
    pub fn cloud_watch(log_group: impl Into<String>, log_stream: Option<String>) -> Self {
        Self::CloudWatch {
            cloud_watch: CloudWatchLogging {
                log_group: Some(log_group.into()),
                log_stream,
            },
        }
    }

    /// The resolved `logStream` this configuration carries, when it carries one.
    pub fn log_stream(&self) -> Option<&str> {
        match self {
            Logging::Disabled { .. } => None,
            Logging::CloudWatch { cloud_watch } => cloud_watch.log_stream.as_deref(),
        }
    }
}

/// `LoggingDisabled`, which the model declares with no members. See [`AllPorts`] for why
/// this is an empty struct rather than a unit one.
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct LoggingDisabled {}

/// `CloudWatchLogging`: the group and stream names, both optional in the model.
///
/// # `logStream` is an EXACT stream name, and the client never sends a caller's verbatim
///
/// Measured 2026-08 (docs/PLATFORM.md, 'An image build is three VMs and three log
/// streams'): the member names one stream exactly — prefixes are unsupported — and one
/// image build runs three VMs that each want their own stream, so a fixed configured name
/// collapses all three (and every successive build's three) into one indistinguishable
/// stream. [`crate::control::ControlPlane::create_image`] therefore appends `/<16 hex>`
/// of fresh CSPRNG per create attempt before this struct reaches the wire, the same
/// mechanism as the `clientToken` nonce (TRAP-1).
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CloudWatchLogging {
    /// `CloudWatchLoggingLogGroupString`: 1..=512, pattern `[a-zA-Z0-9_\-/.#]+`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub log_group: Option<String>,
    /// `CloudWatchLoggingLogStreamString`: 1..=512, pattern `[^:*]*`. Always carries the
    /// per-build discriminator by the time it serialises — see the type docs.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub log_stream: Option<String>,
}

/// `CpuConfiguration`. One required member, whose enum has one value (`ARM_64`).
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CpuConfiguration {
    pub architecture: String,
}

/// `Resources` — note the plural. The model's `ResourcesList` member shape is
/// `Resources`, not `Resource`; there is no shape called `Resource` at all.
///
/// `minimumMemoryInMiB` selects a size class, it does not size the VM (TRAP-10, and
/// `crate::sizing`). The member name's `MiB` capitalisation is the model's.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Resources {
    #[serde(rename = "minimumMemoryInMiB")]
    pub minimum_memory_in_mib: u32,
}

/// `Hooks`.
#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Hooks {
    /// `HooksPortInteger`, `min: 1`. Checked by [`crate::control::require_valid_port`] before the
    /// block is built — a `hooks.port` of 0 is an image with no reachable hook endpoint.
    pub port: u16,
    pub microvm_hooks: MicrovmHooks,
    pub microvm_image_hooks: MicrovmImageHooks,
}

/// `HookState`, the model's two values.
///
/// # A typed enum rather than a validated `String`, and why this one and not the response states
///
/// This is the same trade [`VersionStatus`] makes and the reasoning is the same shape. A
/// response enum must stay a `String` — a third value AWS adds has to parse rather than fail,
/// which is what [`MicrovmImageBuildSummaryWire::architecture`] documents. `HookState` on the
/// request side is only ever **serialized**, so narrowing it costs nothing and buys the thing a
/// two-value enum buys: `"ENABLE"`, `"Enabled"`, and `"ACTIVE"` become compile errors instead of
/// a `ValidationException`.
///
/// It is worth more here than on any other member, because it appears **six times** on one
/// request. Issue #24 measured what was there before: six `String` fields with `"ENABLED"`
/// hardcoded as a `const &str` local to `hooks_block`, so no constant named either value and a
/// typo in one of the six was a rejected `CreateMicrovmImage` — after the artifact upload.
///
/// # `Deserialize` too, unlike [`VersionStatus`]
///
/// Because a `MicrovmHooks` block is a **request** shape in this module and the tests parse
/// model-spelled JSON into it (the honest-fake rule). Deriving `Deserialize` on a two-variant
/// enum means an unknown state fails that parse, which is correct for a shape nothing reads off
/// a real response — and `MicrovmImageVersionSummaryWire` deliberately does not read a hooks
/// block at all, for measured reasons that type records.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum HookState {
    /// The platform does not call this hook.
    #[serde(rename = "DISABLED")]
    Disabled,
    /// The platform calls this hook and waits for it. What this client always sends.
    #[serde(rename = "ENABLED")]
    Enabled,
}

impl HookState {
    /// The wire spelling, which is also what a diagnostic prints.
    pub fn as_str(self) -> &'static str {
        match self {
            HookState::Disabled => "DISABLED",
            HookState::Enabled => "ENABLED",
        }
    }
}

impl std::fmt::Display for HookState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// `MicrovmHooks` — the family that caps at 60 seconds.
///
/// The state members are [`HookState`], so neither value can be misspelled at a call site; the
/// timeouts are built from [`crate::hooks::RunHookTimeout`] before they get here, which is where
/// the ceiling is enforced.
#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MicrovmHooks {
    pub run: HookState,
    pub run_timeout_in_seconds: u32,
    pub resume: HookState,
    pub resume_timeout_in_seconds: u32,
    pub suspend: HookState,
    pub suspend_timeout_in_seconds: u32,
    pub terminate: HookState,
    pub terminate_timeout_in_seconds: u32,
}

/// `MicrovmImageHooks` — the build family, capping at 3600 seconds.
#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MicrovmImageHooks {
    pub ready: HookState,
    pub ready_timeout_in_seconds: u32,
    pub validate: HookState,
    pub validate_timeout_in_seconds: u32,
}

/// `RunMicrovmRequest`.
///
/// `runHookPayload` is the only per-VM secret channel the platform offers and is checked
/// against the 4096-byte ceiling before this struct is built (TRAP-5).
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RunMicrovmWire {
    pub image_identifier: String,
    /// `Version`, absent unless the caller pinned one.
    ///
    /// Absent means "whatever the image's `latestActiveImageVersion` is", which is what this
    /// client always sent. Pinning it is the launch half of the blue/green story: a canary
    /// launches against exactly the version it means to test, and a rollback re-pins to the
    /// previous one rather than hoping the image's notion of latest has moved back.
    ///
    /// A version set INACTIVE by `UpdateMicrovmImageVersion` refuses to launch when pinned
    /// here — measured, see [`crate::control::ControlPlane::set_image_version_status`].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image_version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub execution_role_arn: Option<String>,
    /// Derived ARNs, never caller strings (TRAP-4).
    pub ingress_network_connectors: Vec<String>,
    /// Absent rather than empty when egress was not asked for: omitting it is how you
    /// get a VM with no outbound network.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub egress_network_connectors: Option<Vec<String>>,
    pub idle_policy: IdlePolicy,
    pub maximum_duration_in_seconds: u32,
    pub run_hook_payload: String,
    /// The minted idempotency token. `pub(crate)` for the reason
    /// [`CreateMicrovmImageWire::client_token`] gives.
    pub(crate) client_token: String,
}

/// `IdlePolicy`. All three members are required by the model, in **both** directions.
///
/// # `suspendedDurationSeconds` comes back, measured
///
/// This comment used to say the member "exists only in the request". That was wrong twice
/// over. The model has one `IdlePolicy` shape, used by `RunMicrovmRequest`,
/// `RunMicrovmResponse`, and `GetMicrovmResponse` alike, and it marks all three members
/// required — so the model says the opposite. And the service agrees with the model:
/// `GetMicrovm` on a RUNNING VM answers
/// `"idlePolicy": {"maxIdleDurationSeconds": 1800, "suspendedDurationSeconds": 600,
/// "autoResumeEnabled": false}` (measured 2026-08-15, us-east-1, one read-only
/// `GetMicrovm`).
///
/// The claim mattered because STATE-12 was resting on it: if only the client could name the
/// suspended window, the client's own record is the only authority on it. The readback
/// means there is a second one, and [`crate::control::Microvm::idle_policy`] carries it —
/// so a suspended window that disagrees with what was asked for is now observable rather
/// than assumed.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct IdlePolicy {
    /// `min: 60`, checked by [`crate::control::require_idle_duration`] before the launch.
    ///
    /// The comment here used to say botocore enforced this one locally, so no guard was needed.
    /// Botocore does; this client does not use it. See that function for the whole account.
    pub max_idle_duration_seconds: u32,
    pub suspended_duration_seconds: u32,
    pub auto_resume_enabled: bool,
}

/// `UpdateMicrovmImageVersionRequest`'s **one** body member.
///
/// `imageIdentifier` and `imageVersion` are both URI parameters, so the body is a single
/// key. That is the whole request, and it is the model's only non-destructive retire
/// primitive: setting a version `INACTIVE` makes `RunMicrovm` refuse to launch it while
/// existing VMs keep running and the version stays readable through
/// `GetMicrovmImageVersion`. `DeleteMicrovmImageVersion` is the alternative and it is
/// irreversible.
///
/// `status` is a [`VersionStatus`] rather than a `String`, unlike the `HookState` members
/// above. The difference is that this enum is the *entire* request: a typo in it is a
/// `ValidationException` on the only field there is, spent on a call that was going to
/// change a version's availability. `MicrovmImageVersionStatus` has exactly two values and
/// naming them in the type means a caller cannot spell either wrong.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateImageVersionWire {
    pub status: VersionStatus,
}

/// `MicrovmImageVersionStatus`, the model's two values.
///
/// # Why this one is an enum where `HookState` is a bare `String`
///
/// A response is not a request (see [`MicrovmImageBuildSummaryWire::architecture`]): a value
/// AWS adds to a *response* enum must parse rather than fail, which is why the state fields
/// on every wire struct in this module are `String`. This type is only ever **serialized** —
/// the readback carries `status` as a `String` for exactly that reason — so narrowing it
/// costs nothing and buys the one thing a two-value enum can buy: `"INACTIVATE"`,
/// `"Inactive"`, and `"DISABLED"` are all compile errors rather than a rejected call.
///
/// The `Display`/`as_str` spelling is the model's own casing, asserted against a literal in
/// the tests below rather than derived from the variant name.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub enum VersionStatus {
    /// `RunMicrovm` may launch this version.
    #[serde(rename = "ACTIVE")]
    Active,
    /// `RunMicrovm` refuses to launch this version. Running VMs are untouched.
    #[serde(rename = "INACTIVE")]
    Inactive,
}

impl VersionStatus {
    /// The wire spelling, which is also what a diagnostic prints.
    pub fn as_str(self) -> &'static str {
        match self {
            VersionStatus::Active => "ACTIVE",
            VersionStatus::Inactive => "INACTIVE",
        }
    }
}

impl std::fmt::Display for VersionStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// `CreateMicrovmAuthTokenRequest`'s body members. `microvmIdentifier` is a URI
/// parameter and so is not in the body.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateAuthTokenWire {
    /// Maximum 60 minutes, the service ceiling. TRAP-9 refreshes below it.
    pub expiration_in_minutes: u32,
    pub allowed_ports: Vec<PortSpecification>,
}

/// `PortSpecification`, the model's tagged union: one port, a range, or all of them.
///
/// A union rather than the single-field struct this was, and the change is a defect fix
/// rather than completeness for its own sake. `Session::connect_headers(port)` and
/// `connect_subprotocols(port)` exist to reach *another* port on the same VM, and they were
/// built over a token the control plane had minted with `allowedPorts: [{port: 9000}]` — so
/// they returned a correct-looking port header behind a credential that did not authorize
/// it. Measured 2026-08-15 on one VM with a listener on 8080, varying only the mint:
///
/// | `allowedPorts` | `GET :8080` through the endpoint |
/// | --- | --- |
/// | `[{port: 9000}]` | **403 `Access to port denied`** |
/// | `[{port: 9000}, {port: 8080}]` | 200, the guest answered |
/// | `[{allPorts: {}}]` | 200 |
/// | `[{range: {startPort: 8000, endPort: 9100}}]` | 200 |
///
/// One variable, four outcomes, so the token's scope is the whole of it. On the WebSocket
/// path the same rejection is close code 1006 with no reason, which is why this was
/// invisible to a client-side test: the strings were right and the credential behind them
/// was not.
///
/// `#[serde(untagged)]` because the wire form is the *member name* as the key —
/// `{"port": 9000}`, `{"range": {..}}`, `{"allPorts": {}}` — not a discriminator field, and
/// serde's default enum representation would emit `{"Port": 9000}`. Each variant names its
/// own key through `rename`.
#[derive(Clone, Copy, Debug, Serialize)]
#[serde(untagged)]
pub enum PortSpecification {
    /// Exactly one port. What a session built for the agent port asks for.
    One {
        #[serde(rename = "port")]
        port: u16,
    },
    /// An inclusive range. Both members are required by the model.
    Range {
        #[serde(rename = "range")]
        range: PortRange,
    },
    /// Every port on the VM.
    ///
    /// Deliberately **not** what this client mints by default. A token good for every port
    /// is one whose leak is worth more, and the ports a caller actually needs are knowable:
    /// the agent port plus whatever it asked for. Present because the model has it and a
    /// caller building a general proxy needs it, reached only through an explicit scope.
    All {
        #[serde(rename = "allPorts")]
        all_ports: AllPorts,
    },
}

impl PortSpecification {
    /// One port.
    pub fn port(port: u16) -> Self {
        Self::One { port }
    }

    /// An inclusive range of ports.
    pub fn range(start: u16, end: u16) -> Self {
        Self::Range {
            range: PortRange {
                start_port: start,
                end_port: end,
            },
        }
    }

    /// Every port. See [`PortSpecification::All`] for why this is not a default.
    pub fn all() -> Self {
        Self::All {
            all_ports: AllPorts {},
        }
    }
}

/// `PortRange`. Inclusive at both ends, per the model's `min: 1, max: 65535` on each.
#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PortRange {
    pub start_port: u16,
    pub end_port: u16,
}

/// `AllPorts`, which the model declares with no members.
///
/// An empty struct rather than a unit one, because the wire form is `{}` and a unit struct
/// serialises to `null` — which the service rejects as a malformed union member.
#[derive(Clone, Copy, Debug, Serialize)]
pub struct AllPorts {}

// ── responses ───────────────────────────────────────────────────────────────

/// `CreateMicrovmImageResponse`, narrowed to the members this client reads.
///
/// The model marks eight members required; the four below are the ones anything
/// downstream uses. Deserializing a subset is safe — serde ignores the rest — and
/// listing all twenty would be twenty more chances to misspell one.
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateMicrovmImageResponseWire {
    pub image_arn: String,
    pub name: String,
    pub state: String,
    pub image_version: String,
}

/// `GetMicrovmImageOutput`.
///
/// `tags` is here rather than left to a `ListTags` this client does not implement: the
/// service already sends the map on every `GetMicrovmImage`, so reading it costs nothing
/// and a separate operation would cost a call. `createdAt` is one of the model's four
/// required members and was the only one missing.
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GetMicrovmImageResponseWire {
    pub image_arn: String,
    pub name: String,
    pub state: String,
    pub latest_active_image_version: Option<String>,
    pub latest_failed_image_version: Option<String>,
    /// Required by the model. Epoch seconds, possibly fractional.
    pub created_at: f64,
    /// Optional: the model marks only `createdAt` required, so an image never updated
    /// carries no `updatedAt` and the absence is not a parse failure.
    pub updated_at: Option<f64>,
    /// `Tags`, absent rather than empty when the image carries none — measured 2026-08-15
    /// as `"tags": {}` on a real `GetMicrovmImage`, but `Option` because the model does not
    /// require the member and an absent map must not fail the parse.
    pub tags: Option<std::collections::BTreeMap<String, String>>,
}

/// `ListMicrovmImageVersionsOutput`.
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ListImageVersionsResponseWire {
    pub items: Vec<MicrovmImageVersionSummaryWire>,
    pub next_token: Option<String>,
}

/// `MicrovmImageVersionSummary`, which is **also** `GetMicrovmImageVersionOutput` and
/// `UpdateMicrovmImageVersionResponse` — the three shapes are member-for-member identical
/// in the model, so one type serves all of them.
///
/// One type rather than three for the reason [`MicrovmResponseWire`] gives: a second copy
/// is a second place to keep twenty member spellings right, and the model itself declares
/// them as one set. Checked member-by-member against `service-2.json` on 2026-08-16 —
/// `GetMicrovmImageVersionOutput`, `UpdateMicrovmImageVersionResponse`, and
/// `MicrovmImageVersionSummary` list the same twenty members with the same eight required.
///
/// Note `state` here **is** the member's name — unlike the build summary below, and that
/// asymmetry is the whole trap.
///
/// # All eight required members, not the four this crate reads
///
/// The same argument [`MicrovmImageBuildSummaryWire`] records: the model marking a member
/// required is the strongest promise it makes about a response, and a required member absent
/// from the struct is one no drift check can notice going missing. `status` in particular was
/// `Option<String>` while the model requires it — so a service that stopped sending the
/// availability status would have read as `None`, which is exactly how a version blocked from
/// launching would look identical to one nobody had ever set. Confirmed present on all 22
/// versions in the conformance account (2026-08-16), so requiring it is a measurement rather
/// than a reading of the model alone.
///
/// # `hooks` is deliberately **not** here, and the reason is a measurement
///
/// The model marks every member of `Hooks`, `MicrovmHooks`, and `MicrovmImageHooks` optional,
/// while this module's request-side [`Hooks`] declares them all as bare fields — correct for
/// a request, since this client always sends all six. A real response is not so generous:
/// `GetMicrovmImageVersion` on an image built by another tool answered
/// `"microvmHooks": {"run": "ENABLED", "runTimeoutInSeconds": 30}` and nothing else
/// (measured 2026-08-16, us-east-1, `omnigent-host-vpc` version 3.0), so five of
/// `MicrovmHooks`'s eight members were absent. Reusing [`Hooks`] here would fail that parse
/// and turn a readable version into a client error. A response-shaped hooks type is the fix
/// if a caller ever needs it; until then the absence is the honest option, and the test
/// `a_versions_hooks_block_is_not_read_because_a_real_one_omits_members` pins the reason.
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MicrovmImageVersionSummaryWire {
    // ── the model's eight required members ────────────────────────────────────
    pub base_image_arn: String,
    pub build_role_arn: String,
    pub code_artifact: CodeArtifact,
    pub image_arn: String,
    pub image_version: String,
    pub state: String,
    /// `MicrovmImageVersionStatus`: `ACTIVE` or `INACTIVE`.
    ///
    /// A `String` rather than a [`VersionStatus`], because this is a response and a third
    /// value AWS adds must parse rather than fail — the same rule
    /// [`MicrovmImageBuildSummaryWire::architecture`] follows. [`VersionStatus`] is for the
    /// request, where narrowing turns a typo into a compile error.
    pub status: String,
    /// Epoch seconds, possibly fractional.
    pub created_at: f64,

    // ── the optional ones, which are the config readback ─────────────────────
    /// The base version this version was pinned to, when one was.
    ///
    /// The readback for `CreateMicrovmImage.baseImageVersion`, and the reason pinning is
    /// worth doing: an unpinned build still reports one here, and the value the service
    /// echoes is spelled differently from anything the base's own listing offers
    /// (`"1.0"` against the managed base's `"0"`/`"1"` — docs/PLATFORM.md). So this is the
    /// record of what was built, not a value to feed back into a request.
    pub base_image_version: Option<String>,
    pub description: Option<String>,
    /// `ResourcesList`, max 1. The only place a built image's size class is observable:
    /// `GetMicrovm` carries no memory figure at all.
    pub resources: Option<Vec<Resources>>,
    pub cpu_configurations: Option<Vec<CpuConfiguration>>,
    /// The image-level egress list. **Max 1** in the model, not the 10 the VM-level
    /// `NetworkConnectorList` allows — see [`crate::constants::MAX_IMAGE_EGRESS_CONNECTORS`].
    pub egress_network_connectors: Option<Vec<String>>,
    pub additional_os_capabilities: Option<Vec<String>>,
    pub environment_variables: Option<std::collections::BTreeMap<String, String>>,
    pub updated_at: Option<f64>,
    /// The version-level failure reason. Measured **null** on real failures while the
    /// build-level one is populated — docs/PLATFORM.md, "A failed build's `stateReason`
    /// lives on the build".
    pub state_reason: Option<String>,
    pub tags: Option<std::collections::BTreeMap<String, String>>,
}

impl MicrovmImageVersionSummaryWire {
    /// Whether `RunMicrovm` will launch this version.
    ///
    /// A comparison against the model's spelling through [`VersionStatus`] rather than
    /// against a literal here, so the two cannot drift: the request type and the readback
    /// agree on what `"ACTIVE"` is spelled like or neither compiles.
    pub fn is_active(&self) -> bool {
        self.status == VersionStatus::Active.as_str()
    }

    /// The version, its state, its availability status, and the reason when the service gave
    /// one — the line a retire or a rollback prints.
    ///
    /// An absent reason renders nothing rather than an empty parenthesis, for the reason
    /// [`MicrovmImageBuildSummaryWire::describe`] gives.
    pub fn describe(&self) -> String {
        match self.state_reason.as_deref() {
            Some(reason) => format!(
                "{} {} / {} ({reason})",
                self.image_version, self.state, self.status
            ),
            None => format!("{} {} / {}", self.image_version, self.state, self.status),
        }
    }
}

/// `ListMicrovmImageBuildsOutput`.
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ListImageBuildsResponseWire {
    pub items: Vec<MicrovmImageBuildSummaryWire>,
    pub next_token: Option<String>,
}

/// `MicrovmImageBuildSummary`.
///
/// **`buildState`, not `state`.** The model has no `state` member on this shape. A
/// `#[serde(rename)]` to `state` here, or a field named `state` relying on
/// `rename_all`, reproduces the bug described in this module's docs: the stall probe
/// reads `None` from every real response and TRAP-2 becomes unfalsifiable. The
/// deserializer test for this shape uses literal model-spelled JSON for that reason,
/// and asserts that `{"state": ...}` **fails** to deserialize.
/// Every member the model marks required is a bare field here, plus the optional
/// `stateReason`. All eight required ones rather than the four this crate reads today,
/// because the model requiring a member is the strongest promise it makes about a
/// response, and a required member absent from the struct is a member no drift check can
/// notice going missing.
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MicrovmImageBuildSummaryWire {
    pub image_arn: String,
    pub image_version: String,
    pub build_id: String,
    pub build_state: String,
    /// `ARM_64`, the enum's only value — carried as `String` rather than narrowed, because
    /// a response is not a request and a second value AWS adds must parse rather than fail.
    pub architecture: String,
    pub chipset: String,
    pub chipset_generation: String,
    /// A `Timestamp`, which rest-json sends as epoch seconds — possibly fractional, hence
    /// `f64` rather than `i64`.
    pub created_at: f64,
    pub state_reason: Option<String>,
}

impl MicrovmImageBuildSummaryWire {
    /// The build, as a stall diagnosis wants to read it: the state, the build id, and the
    /// reason when the service gave one.
    ///
    /// The `buildId` is in here because it is the identifier `GetMicrovmImageBuild` takes,
    /// so a reader handed a wedge verdict can go ask about a named build rather than about
    /// "one of the builds". The reason is included because the model documents it as "the
    /// reason for the build state, **if applicable**" — so it is populated on states other
    /// than a failure, and a probe that dropped it would be discarding the service's own
    /// account of why nothing was scheduled.
    ///
    /// An absent reason renders nothing at all rather than an empty parenthesis, for the
    /// reason [`MicrovmResponseWire::state_reason`] gives: "the service said nothing" and
    /// "the service said nothing useful" are different diagnoses.
    pub fn describe(&self) -> String {
        match self.state_reason.as_deref() {
            Some(reason) => format!("{} {} ({reason})", self.build_id, self.build_state),
            None => format!("{} {}", self.build_id, self.build_state),
        }
    }
}

/// `GetMicrovmImageBuildOutput`, which is `MicrovmImageBuildSummary` plus one member.
///
/// # Why this is not the same type as the summary
///
/// It differs by exactly one member — `snapshotBuild` — and that member is the whole reason
/// to make the call: it is the **only** place any API reports a size for anything this
/// platform builds. Nothing on `GetMicrovmImage`, `GetMicrovm`, or either listing carries a
/// byte count, and the three figures here are the quantities the snapshot read, write, and
/// storage line items bill on (`crate::cost`). A separate type rather than an `Option` field
/// on the summary, because the summary is a *listing* shape that never carries one, and an
/// `Option` there would read as "the service sometimes omits the sizes" rather than "the
/// listing does not have this member".
///
/// Every other member is required and identical to the summary's, so the same eight are bare
/// fields here.
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GetImageBuildResponseWire {
    pub image_arn: String,
    pub image_version: String,
    pub build_id: String,
    pub build_state: String,
    pub architecture: String,
    pub chipset: String,
    /// The Graviton generation this build targeted. One `CreateMicrovmImage` fans out into
    /// one build per generation, so a version's builds differ only in this member — and a
    /// partially failed image, one generation succeeding and the other not, is a state a
    /// caller should expect (docs/PLATFORM.md).
    pub chipset_generation: String,
    pub created_at: f64,
    pub state_reason: Option<String>,
    /// The three snapshot sizes, when the service reports them.
    ///
    /// `Option` because the model does not mark it required, and because a **failed** build
    /// answers a partial one: measured 2026-08-16 on a real `FAILED` build, `snapshotBuild`
    /// carried `codeInstallSizeInBytes` alone with no memory and no disk snapshot — which is
    /// exactly the shape of a build that installed the code and then never produced a
    /// snapshot.
    pub snapshot_build: Option<SnapshotBuild>,
}

impl GetImageBuildResponseWire {
    /// The build as a diagnosis reads it: the generation, the state, the reason when there is
    /// one, and the sizes when the service reported them.
    ///
    /// The generation is in the line because a version has one build per Graviton generation
    /// and they can disagree — "the build failed" is ambiguous where "generation 4 failed" is
    /// not. Sizes are appended only when present, for the reason the absent-reason rule
    /// gives: a rendered `0 bytes` would claim a measurement the service did not make.
    pub fn describe(&self) -> String {
        let mut line = match self.state_reason.as_deref() {
            Some(reason) => format!(
                "{} {} (chipset generation {}) — {reason}",
                self.build_id, self.build_state, self.chipset_generation
            ),
            None => format!(
                "{} {} (chipset generation {})",
                self.build_id, self.build_state, self.chipset_generation
            ),
        };
        if let Some(sizes) = &self.snapshot_build
            && let Some(rendered) = sizes.describe()
        {
            line.push_str(&format!(", {rendered}"));
        }
        line
    }
}

/// `SnapshotBuild`: the three byte counts, each optional in the model.
///
/// `u64` from the model's `Long`, and every one `Option` — the model marks none required, and
/// a failed build really does answer with a subset (see
/// [`GetImageBuildResponseWire::snapshot_build`]). A defaulted `0` would be a size claim
/// nobody made, which is the same distinction `stateReason`'s `None` carries.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotBuild {
    pub memory_snapshot_size_in_bytes: Option<u64>,
    pub code_install_size_in_bytes: Option<u64>,
    pub disk_snapshot_size_in_bytes: Option<u64>,
}

impl SnapshotBuild {
    /// The sizes the service reported, or `None` when it reported none of them.
    ///
    /// `None` rather than an empty string so a caller can tell "no sizes" from "sizes that
    /// render blank", and so [`GetImageBuildResponseWire::describe`] does not append a
    /// dangling comma.
    pub fn describe(&self) -> Option<String> {
        let parts: Vec<String> = [
            ("memory", self.memory_snapshot_size_in_bytes),
            ("code", self.code_install_size_in_bytes),
            ("disk", self.disk_snapshot_size_in_bytes),
        ]
        .into_iter()
        .filter_map(|(label, bytes)| bytes.map(|bytes| format!("{label} {bytes} bytes")))
        .collect();
        (!parts.is_empty()).then(|| parts.join(", "))
    }
}

/// `ListManagedMicrovmImageVersionsOutput`.
///
/// The versions of an AWS-managed base image, which is what closes the reproducibility hole:
/// every build this client made floated on whatever `baseImageVersion` the service defaulted
/// to, and that default has already moved once (`al2023-1` carries `"0"` and `"1"`).
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ListManagedVersionsResponseWire {
    pub items: Vec<ManagedMicrovmImageVersionWire>,
    pub next_token: Option<String>,
}

/// `ManagedMicrovmImageVersion`. Three required members and one optional.
///
/// Note how much thinner this is than [`MicrovmImageVersionSummaryWire`]: a managed base's
/// version carries no state, no status, no config — just an ARN, a version string, and two
/// timestamps. So there is nothing here to check before pinning one, which is why
/// [`crate::control::ControlPlane::managed_base_versions`] hands back the strings and lets
/// the caller choose.
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ManagedMicrovmImageVersionWire {
    pub image_arn: String,
    /// A **bare integer** for the managed base (`"0"`, `"1"`), where a custom image's
    /// versions are `"1.0"`. The two spellings are not comparable and code that parses one
    /// will not parse the other (docs/PLATFORM.md).
    pub image_version: String,
    pub created_at: f64,
    pub updated_at: Option<f64>,
}

/// `ListManagedMicrovmImagesOutput`.
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ListManagedImagesResponseWire {
    pub items: Vec<ManagedMicrovmImageSummaryWire>,
    pub next_token: Option<String>,
}

/// `ManagedMicrovmImageSummary`: an ARN and two timestamps, and **that is all the model
/// declares**.
///
/// # Why a discovered base cannot construct a [`crate::control::BaseImage`]
///
/// [`crate::control::BaseImage`] pairs three things that must agree: the `baseImageArn`, the
/// Dockerfile `FROM` that goes with it, and whether the image declares a `WORKDIR`. This
/// shape carries the first and says nothing about the other two — no registry reference, no
/// architecture, no working directory. So `require_matching_from` would have no `FROM` to
/// compare a caller's Dockerfile against, and `require_workdir` would have no answer to the
/// question it exists to ask; both guards would have to be skipped for a discovered base,
/// which is worse than not discovering one. The registry ref in particular is not derivable:
/// `al2023-1` pairs with `public.ecr.aws/amazonlinux/amazonlinux:2023-minimal`, and nothing
/// in the ARN says so.
///
/// So this is **informational only** — surfaced by `microvm doctor` so a caller can see
/// whether AWS has published a base this client does not know about, and never fed into a
/// create request. Measured 2026-08-16: the listing returns exactly one item, so a client
/// hardcoding `al2023-1` is currently missing nothing.
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ManagedMicrovmImageSummaryWire {
    pub image_arn: String,
    pub created_at: f64,
    pub updated_at: Option<f64>,
}

/// `RunMicrovmResponse` and `GetMicrovmResponse` — identical in every member this client
/// reads, so one type serves both.
///
/// One type rather than two because the two shapes agree on all seven required members
/// and both carry `state`, `endpoint`, and `stateReason`; a second type would be a
/// second place to keep the spelling right.
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MicrovmResponseWire {
    pub microvm_id: String,
    pub state: String,
    pub endpoint: String,
    pub image_arn: String,
    pub image_version: String,
    /// Optional in the model, and the absence is information: TRAP-8's message says so
    /// rather than printing an empty string.
    pub state_reason: Option<String>,
    pub idle_policy: Option<IdlePolicy>,
}

/// `CreateMicrovmAuthTokenResponse`.
///
/// `authToken` is a `TokenParts` **map**, not a string (TRAP-7). Reading it as a string
/// is a deserialization failure rather than a wrong value, which is the good direction —
/// but the map's `X-aws-proxy-auth` key is what the endpoint header takes, and a caller
/// who sends the map's `Debug` rendering gets a rejection that reads like a bad token.
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateAuthTokenResponseWire {
    pub auth_token: std::collections::BTreeMap<String, String>,
}

/// `ListMicrovmsResponse`.
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ListMicrovmsResponseWire {
    pub items: Vec<MicrovmItemWire>,
    pub next_token: Option<String>,
}

/// `MicrovmItem` — the list shape, which is narrower than `GetMicrovmResponse`: it
/// carries no `endpoint` and no `stateReason`.
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MicrovmItemWire {
    pub microvm_id: String,
    pub state: String,
    pub image_arn: String,
    pub image_version: String,
}

/// `DeleteMicrovmImageOutput`.
///
/// # Kept and read, rather than deleted as dead
///
/// This struct was parsed nowhere: `try_delete_image` discarded the reply, so the shape
/// existed and no code could observe drift in it. The choice was to delete it or to read
/// it, and reading it wins on one specific case — a 2xx whose `state` is a `*_FAILED`
/// spelling. That is the service accepting the delete request and refusing the work, and
/// without the readback it is indistinguishable from a successful delete: `delete_image`
/// answers `true`, teardown reports clean, and an image keeps billing. This project has
/// had exactly that failure once already, in `scripts/verify-clean.py`, which is why the
/// account is now asked what leaked rather than trusted to have been cleaned.
///
/// **`DELETING` and `DELETED` are both success** and the call site treats them alike. The
/// deletion is asynchronous, so `DELETING` is the ordinary answer; treating it as
/// incomplete would make the retry loop re-issue a delete that is already in progress and
/// then report failure on the `ConflictException` that comes back.
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeleteImageResponseWire {
    pub image_identifier: String,
    pub state: String,
}

/// `ListMicrovmImagesResponse`.
///
/// `nextToken` is how the account's listing paginates, and the name resolver **must**
/// follow it: an image on page two of a paginated listing is still an image, and a
/// resolver that read only the first page would report "no image named X" for a name
/// that exists — the confident wrong answer, which is worse than a failure.
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ListImagesResponseWire {
    pub items: Vec<MicrovmImageSummaryWire>,
    pub next_token: Option<String>,
}

/// `MicrovmImageSummary`, narrowed to the members name resolution reads.
///
/// The model marks `imageArn`, `name`, `state`, and `createdAt` required; the first
/// three are the ones anything downstream uses.
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MicrovmImageSummaryWire {
    pub image_arn: String,
    pub name: String,
    pub state: String,
}

/// The modeled error shapes, which all carry a `message` and several add fields.
///
/// One type for all seven because every one has `message` as its only member this client
/// reads, and the status code is what distinguishes them.
///
/// `message` is `Option` for a reason that is itself a finding: an unsupported region
/// answers `AccessDeniedException` with the message field **null** (TRAP-6). A required
/// `String` here would fail to deserialize and report a parse error instead of the
/// denial, hiding the one detail that identifies the trap.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServiceErrorWire {
    pub message: Option<String>,
    /// `ThrottlingException`/`ServiceQuotaExceededException`.
    pub quota_code: Option<String>,
    /// `ThrottlingException`/`ServiceQuotaExceededException`.
    pub service_code: Option<String>,
    /// `ConflictException`/`ResourceNotFoundException`/`ServiceQuotaExceededException`.
    pub resource_type: Option<String>,
    /// As above.
    pub resource_id: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The build summary, from JSON spelled exactly as `MicrovmImageBuildSummary`
    /// declares it — every required member present, in the model's casing.
    ///
    /// **This is the honest-fake test.** It parses text, not this module's own output, so
    /// it cannot agree with a misreading the way the Python fake did.
    #[test]
    fn a_build_summary_deserialises_from_the_models_own_spelling() {
        let json = r#"{
            "imageArn": "arn:aws:lambda:us-east-1:123456789012:microvm-image:img",
            "imageVersion": "1",
            "buildId": "build-abc",
            "buildState": "PENDING",
            "architecture": "ARM_64",
            "chipset": "GRAVITON",
            "chipsetGeneration": "1",
            "createdAt": 1754524800
        }"#;
        let parsed: MicrovmImageBuildSummaryWire =
            serde_json::from_str(json).expect("the model's own spelling parses");
        assert_eq!(parsed.build_state, "PENDING");
        assert_eq!(parsed.build_id, "build-abc");
        assert_eq!(parsed.image_version, "1");
        assert_eq!(parsed.state_reason, None);

        // The five members the struct used to drop, all of them model-required. A required
        // member absent from the struct is a member no drift check can notice going missing.
        assert_eq!(
            parsed.image_arn,
            "arn:aws:lambda:us-east-1:123456789012:microvm-image:img"
        );
        assert_eq!(parsed.architecture, "ARM_64");
        assert_eq!(parsed.chipset, "GRAVITON");
        assert_eq!(parsed.chipset_generation, "1");
        assert_eq!(parsed.created_at, 1_754_524_800.0);
    }

    /// A summary missing a member the model marks **required** fails to deserialize, which
    /// is what makes the five added members load-bearing rather than decorative.
    ///
    /// One case per member, because a single omission would prove only that *some* member is
    /// required. `stateReason` is checked in the other direction: it is the one optional
    /// member, so omitting it must still parse.
    #[test]
    fn a_build_summary_missing_a_required_member_fails_to_deserialise() {
        let members = [
            (
                "imageArn",
                r#""arn:aws:lambda:us-east-1:1:microvm-image:i""#,
            ),
            ("imageVersion", r#""1""#),
            ("buildId", r#""build-abc""#),
            ("buildState", r#""PENDING""#),
            ("architecture", r#""ARM_64""#),
            ("chipset", r#""GRAVITON""#),
            ("chipsetGeneration", r#""1""#),
            ("createdAt", "1754524800"),
        ];

        for omitted in members.iter().map(|(name, _)| *name) {
            let body: Vec<String> = members
                .iter()
                .filter(|(name, _)| *name != omitted)
                .map(|(name, value)| format!(r#""{name}": {value}"#))
                .collect();
            let json = format!("{{{}}}", body.join(", "));
            let error = serde_json::from_str::<MicrovmImageBuildSummaryWire>(&json)
                .expect_err(&format!("{omitted} is required by the model"));
            assert!(
                error.to_string().contains(omitted),
                "the error must name the member that is missing ({omitted}): {error}"
            );
        }

        // The one optional member: omitting `stateReason` parses, and the absence is `None`
        // rather than an empty string.
        let all: Vec<String> = members
            .iter()
            .map(|(name, value)| format!(r#""{name}": {value}"#))
            .collect();
        let parsed: MicrovmImageBuildSummaryWire =
            serde_json::from_str(&format!("{{{}}}", all.join(", ")))
                .expect("stateReason is optional");
        assert_eq!(parsed.state_reason, None);
    }

    /// `describe` names the build id, the state, and the reason when there is one — and
    /// renders **nothing** rather than an empty parenthesis when there is not.
    ///
    /// The build id is in there because it is what `GetMicrovmImageBuild` takes, so a reader
    /// handed a wedge verdict can ask about a named build rather than about "one of the
    /// builds".
    #[test]
    fn a_build_describes_itself_with_its_id_and_only_a_reason_it_has() {
        let with_reason: MicrovmImageBuildSummaryWire = serde_json::from_str(
            r#"{
                "imageArn": "arn:aws:lambda:us-east-1:1:microvm-image:i",
                "imageVersion": "1",
                "buildId": "build-abc",
                "buildState": "FAILED",
                "architecture": "ARM_64",
                "chipset": "GRAVITON",
                "chipsetGeneration": "1",
                "createdAt": 1754524800,
                "stateReason": "no space left on device"
            }"#,
        )
        .expect("parses");
        assert_eq!(
            with_reason.describe(),
            "build-abc FAILED (no space left on device)"
        );

        let without: MicrovmImageBuildSummaryWire = serde_json::from_str(
            r#"{
                "imageArn": "arn:aws:lambda:us-east-1:1:microvm-image:i",
                "imageVersion": "1",
                "buildId": "build-abc",
                "buildState": "PENDING",
                "architecture": "ARM_64",
                "chipset": "GRAVITON",
                "chipsetGeneration": "1",
                "createdAt": 1754524800
            }"#,
        )
        .expect("parses");
        assert_eq!(without.describe(), "build-abc PENDING");
        assert!(
            !without.describe().contains("()"),
            "an absent reason renders nothing, not an empty parenthesis: {}",
            without.describe()
        );
    }

    /// The bug, made a compile-and-test-time fact: a response carrying `state` instead of
    /// `buildState` **fails to deserialize** rather than yielding a summary whose state is
    /// silently absent.
    ///
    /// This is the falsification for the whole `buildState` discipline. Rename the field
    /// to `state` (or add a `#[serde(alias = "state")]` "for compatibility") and this
    /// test goes red — which is what the Python client had no equivalent of, and why its
    /// dead guard passed for a review round.
    #[test]
    fn a_summary_spelled_state_instead_of_build_state_fails_to_deserialise() {
        let json = r#"{
            "imageArn": "arn:aws:lambda:us-east-1:123456789012:microvm-image:img",
            "imageVersion": "1",
            "buildId": "build-abc",
            "state": "PENDING",
            "architecture": "ARM_64",
            "chipset": "GRAVITON",
            "chipsetGeneration": "1",
            "createdAt": 1754524800
        }"#;
        let parsed: Result<MicrovmImageBuildSummaryWire, _> = serde_json::from_str(json);
        let error = parsed.expect_err("MicrovmImageBuildSummary has no member called state");
        assert!(
            error.to_string().contains("buildState"),
            "the error must name the member that is actually missing: {error}"
        );
    }

    /// The asymmetry that makes the trap a trap: the *version* summary really does have a
    /// member called `state`, so "these shapes use `state`" is true of one and false of
    /// its neighbour. Both spellings are asserted here, together, so a reader sees why
    /// the two cannot be handled by one habit.
    #[test]
    fn a_version_summary_does_use_state_which_is_why_the_build_one_confuses() {
        let json = r#"{
            "baseImageArn": "arn:aws:lambda:us-east-1:aws:microvm-image:al2023-1",
            "buildRoleArn": "arn:aws:iam::123456789012:role/build",
            "codeArtifact": {"uri": "s3://bucket/img.zip"},
            "imageArn": "arn:aws:lambda:us-east-1:123456789012:microvm-image:img",
            "imageVersion": "1",
            "state": "SUCCESSFUL",
            "status": "ACTIVE",
            "createdAt": 1754524800
        }"#;
        let parsed: MicrovmImageVersionSummaryWire =
            serde_json::from_str(json).expect("the version summary's member is state");
        assert_eq!(parsed.state, "SUCCESSFUL");
        assert_eq!(parsed.status, "ACTIVE");
        assert!(parsed.is_active(), "ACTIVE is what RunMicrovm will launch");
    }

    /// The version summary's `stateReason`, which is the **version-level** failure reason
    /// `GetMicrovmImage` structurally cannot provide — that shape has no such member.
    ///
    /// It was parsed and never read, so a build failure could only be reported with a log
    /// group and a guess while the service's own account of the cause sat in hand.
    #[test]
    fn a_version_summary_carries_the_state_reason_get_image_cannot() {
        let json = r#"{
            "baseImageArn": "arn:aws:lambda:us-east-1:aws:microvm-image:al2023-1",
            "buildRoleArn": "arn:aws:iam::123456789012:role/build",
            "codeArtifact": {"uri": "s3://bucket/img.zip"},
            "imageArn": "arn:aws:lambda:us-east-1:123456789012:microvm-image:img",
            "imageVersion": "2",
            "state": "FAILED",
            "status": "INACTIVE",
            "createdAt": 1754524800,
            "stateReason": "one or more builds failed"
        }"#;
        let parsed: MicrovmImageVersionSummaryWire = serde_json::from_str(json).expect("parses");
        assert_eq!(parsed.state, "FAILED");
        assert_eq!(
            parsed.state_reason.as_deref(),
            Some("one or more builds failed")
        );

        // `GetMicrovmImageOutput` has no `stateReason` member at all, which is the whole
        // reason the version-level one is worth reading. A response carrying one parses and
        // the member is simply dropped, so this is asserted by there being no field to read
        // rather than by a failure.
        let image: GetMicrovmImageResponseWire = serde_json::from_str(
            r#"{"imageArn": "a", "name": "n", "state": "CREATE_FAILED",
                "createdAt": 1754524800, "stateReason": "ignored"}"#,
        )
        .expect("an unknown member is dropped");
        assert_eq!(image.state, "CREATE_FAILED");
    }

    /// `GetMicrovmImageOutput` carries `tags`, `createdAt`, and `updatedAt`.
    ///
    /// Reading `tags` here is a cheaper answer to "what are this image's tags" than
    /// implementing `ListTags`: the service already sends the map on every call. The literal
    /// below is the shape a real response has — measured 2026-08-15 against a live
    /// `GetMicrovmImage`, which answered `"tags": {}` and an `updatedAt` on an untagged image.
    #[test]
    fn a_get_image_response_carries_its_tags_and_timestamps() {
        let json = r#"{
            "imageArn": "arn:aws:lambda:us-east-1:123456789012:microvm-image:img",
            "name": "img",
            "state": "CREATED",
            "latestActiveImageVersion": "1.0",
            "createdAt": 1754524800,
            "updatedAt": 1754528400,
            "tags": {"owner": "conformance", "cost-centre": "agents"}
        }"#;
        let parsed: GetMicrovmImageResponseWire = serde_json::from_str(json).expect("parses");
        assert_eq!(parsed.created_at, 1_754_524_800.0);
        assert_eq!(parsed.updated_at, Some(1_754_528_400.0));
        let tags = parsed.tags.expect("the map was sent");
        assert_eq!(tags.get("owner").map(String::as_str), Some("conformance"));
        assert_eq!(tags.len(), 2);

        // An untagged image answers with an empty map, which is different from an absent
        // one: `Some({})` says the service answered and the image has no tags.
        let empty: GetMicrovmImageResponseWire = serde_json::from_str(
            r#"{"imageArn": "a", "name": "n", "state": "CREATED",
                "createdAt": 1754524800, "updatedAt": 1754528400, "tags": {}}"#,
        )
        .expect("parses");
        assert_eq!(empty.tags, Some(std::collections::BTreeMap::new()));

        // And an absent map is `None` rather than a parse failure, since the model does not
        // mark `tags` required.
        let absent: GetMicrovmImageResponseWire = serde_json::from_str(
            r#"{"imageArn": "a", "name": "n", "state": "CREATED", "createdAt": 1754524800}"#,
        )
        .expect("tags is optional");
        assert_eq!(absent.tags, None);
        assert_eq!(absent.updated_at, None);
    }

    /// `createdAt` is required by the model, so a `GetMicrovmImage` response without it
    /// fails rather than yielding an image whose creation time is silently zero.
    #[test]
    fn a_get_image_response_without_created_at_fails_to_deserialise() {
        let error = serde_json::from_str::<GetMicrovmImageResponseWire>(
            r#"{"imageArn": "a", "name": "n", "state": "CREATED"}"#,
        )
        .expect_err("createdAt is one of the model's four required members");
        assert!(error.to_string().contains("createdAt"), "{error}");
    }

    /// `IdlePolicy` round-trips through a **response** body, all three members present.
    ///
    /// The comment on the shape used to say `suspendedDurationSeconds` "exists only in the
    /// request". The JSON below is what a live `GetMicrovm` answered on 2026-08-15, so this
    /// test is the measurement rather than a restatement of the model.
    #[test]
    fn an_idle_policy_comes_back_on_a_response_with_all_three_members() {
        let json = r#"{
            "microvmId": "mvm-1",
            "state": "RUNNING",
            "endpoint": "https://e",
            "imageArn": "arn:aws:lambda:us-east-1:123456789012:microvm-image:img",
            "imageVersion": "1.0",
            "idlePolicy": {
                "maxIdleDurationSeconds": 1800,
                "suspendedDurationSeconds": 600,
                "autoResumeEnabled": false
            },
            "maximumDurationInSeconds": 10800
        }"#;
        let parsed: MicrovmResponseWire = serde_json::from_str(json).expect("parses");
        let policy = parsed.idle_policy.expect("the service sent one");
        assert_eq!(policy.max_idle_duration_seconds, 1800);
        assert_eq!(
            policy.suspended_duration_seconds, 600,
            "the member the comment claimed the response omits"
        );
        assert!(!policy.auto_resume_enabled);
    }

    /// A `DeleteMicrovmImage` readback parses, and the failure spelling is distinguishable
    /// from the two success ones — which is the whole reason this shape is read rather than
    /// deleted as dead.
    #[test]
    fn a_delete_readback_distinguishes_failure_from_deleting_and_deleted() {
        for state in ["DELETING", "DELETED", "DELETE_FAILED"] {
            let json = format!(
                r#"{{"imageIdentifier": "arn:aws:lambda:us-east-1:1:microvm-image:img",
                     "state": "{state}"}}"#
            );
            let parsed: DeleteImageResponseWire = serde_json::from_str(&json).expect("parses");
            assert_eq!(parsed.state, state);
            assert_eq!(
                parsed.image_identifier,
                "arn:aws:lambda:us-east-1:1:microvm-image:img"
            );
        }
    }

    /// `ListMicrovmImagesResponse` from the model's own spelling, `nextToken` included.
    ///
    /// The honest-fake rule again: this JSON is text transcribed from `service-2.json`,
    /// not a round trip through this module's serializer, so a misspelled member here
    /// cannot agree with a misspelled member in the struct.
    #[test]
    fn an_image_listing_deserialises_from_the_models_own_spelling() {
        let json = r#"{
            "items": [{
                "imageArn": "arn:aws:lambda:us-east-1:123456789012:microvm-image:img",
                "name": "img",
                "state": "ACTIVE",
                "latestActiveImageVersion": "1",
                "createdAt": 1754524800
            }],
            "nextToken": "opaque-page-2-token"
        }"#;
        let parsed: ListImagesResponseWire =
            serde_json::from_str(json).expect("the model's own spelling parses");
        assert_eq!(parsed.items.len(), 1);
        assert_eq!(parsed.items[0].name, "img");
        assert_eq!(
            parsed.items[0].image_arn,
            "arn:aws:lambda:us-east-1:123456789012:microvm-image:img"
        );
        assert_eq!(parsed.items[0].state, "ACTIVE");
        assert_eq!(parsed.next_token.as_deref(), Some("opaque-page-2-token"));

        // The final page: nextToken absent means None, which is what stops the loop.
        let last: ListImagesResponseWire =
            serde_json::from_str(r#"{"items": []}"#).expect("an absent nextToken parses");
        assert!(last.items.is_empty());
        assert_eq!(last.next_token, None);
    }

    /// `RunMicrovmResponse` from the model's spelling, including the optional
    /// `stateReason` that TRAP-8 reports.
    #[test]
    fn a_run_response_deserialises_with_its_state_reason() {
        let json = r#"{
            "microvmId": "mvm-123",
            "state": "TERMINATED",
            "endpoint": "https://mvm-123.microvm.amazonaws.com",
            "imageArn": "arn:aws:lambda:us-east-1:123456789012:microvm-image:img",
            "imageVersion": "1",
            "maximumDurationInSeconds": 3600,
            "startedAt": 1754524800,
            "stateReason": "run hook returned 500"
        }"#;
        let parsed: MicrovmResponseWire = serde_json::from_str(json).expect("parses");
        assert_eq!(parsed.state, "TERMINATED");
        assert_eq!(
            parsed.state_reason.as_deref(),
            Some("run hook returned 500")
        );
        assert_eq!(parsed.microvm_id, "mvm-123");
    }

    /// The same shape without `stateReason`, which the model permits. The field must be
    /// `None` rather than an empty string, because "the service said nothing" and "the
    /// service said nothing useful" are different diagnoses.
    #[test]
    fn an_absent_state_reason_is_none_rather_than_an_empty_string() {
        let json = r#"{
            "microvmId": "mvm-123",
            "state": "RUNNING",
            "endpoint": "https://mvm-123.microvm.amazonaws.com",
            "imageArn": "arn:aws:lambda:us-east-1:123456789012:microvm-image:img",
            "imageVersion": "1",
            "maximumDurationInSeconds": 3600,
            "startedAt": 1754524800
        }"#;
        let parsed: MicrovmResponseWire = serde_json::from_str(json).expect("parses");
        assert_eq!(parsed.state_reason, None);
    }

    /// `authToken` is a map (TRAP-7). The key is the header name, and reading the map's
    /// one interesting key is what the proxy needs.
    #[test]
    fn an_auth_token_response_is_a_header_map_not_a_string() {
        let json = r#"{"authToken": {"X-aws-proxy-auth": "opaque-token-value"}}"#;
        let parsed: CreateAuthTokenResponseWire = serde_json::from_str(json).expect("parses");
        assert_eq!(
            parsed
                .auth_token
                .get("X-aws-proxy-auth")
                .map(String::as_str),
            Some("opaque-token-value")
        );
    }

    /// A string `authToken` fails rather than being coerced. The failure is the good
    /// direction: a client that accepted both would send a stringified map as a header
    /// value and get a rejection that reads like a bad token.
    #[test]
    fn a_string_auth_token_fails_to_deserialise() {
        let json = r#"{"authToken": "opaque-token-value"}"#;
        let parsed: Result<CreateAuthTokenResponseWire, _> = serde_json::from_str(json);
        assert!(parsed.is_err(), "TokenParts is a map shape");
    }

    /// TRAP-6's null message survives deserialization. A required `String` here would
    /// fail to parse and report a parse error, which hides the one detail — the null —
    /// that tells an unsupported region apart from a real IAM denial.
    #[test]
    fn an_access_denied_with_a_null_message_still_deserialises() {
        let parsed: ServiceErrorWire =
            serde_json::from_str(r#"{"message": null}"#).expect("a null message must parse");
        assert_eq!(parsed.message, None);

        let empty: ServiceErrorWire = serde_json::from_str("{}").expect("an absent message parses");
        assert_eq!(empty.message, None);
    }

    /// An unknown member is dropped rather than failing, so a member AWS adds does not
    /// break the client. This is the deliberate absence of `deny_unknown_fields`.
    #[test]
    fn an_unknown_response_member_is_ignored() {
        let json = r#"{
            "microvmId": "mvm-123",
            "state": "RUNNING",
            "endpoint": "https://e",
            "imageArn": "arn",
            "imageVersion": "1",
            "somethingAwsAddedLater": {"nested": [1, 2, 3]}
        }"#;
        let parsed: MicrovmResponseWire =
            serde_json::from_str(json).expect("additive change is safe");
        assert_eq!(parsed.microvm_id, "mvm-123");
    }

    /// The create-image request serialises to the model's member names. Asserted against
    /// literal keys rather than a round trip, for the same reason the response tests hold
    /// JSON: `minimumMemoryInMiB` in particular is a casing `rename_all = "camelCase"`
    /// gets wrong on its own (it produces `minimumMemoryInMib`).
    #[test]
    fn the_create_request_serialises_to_the_models_member_names() {
        let wire = CreateMicrovmImageWire {
            name: "img".to_string(),
            base_image_arn: "arn:base".to_string(),
            base_image_version: None,
            build_role_arn: "arn:role".to_string(),
            code_artifact: CodeArtifact {
                uri: "s3://bucket/img.zip".to_string(),
            },
            cpu_configurations: vec![CpuConfiguration {
                architecture: "ARM_64".to_string(),
            }],
            resources: vec![Resources {
                minimum_memory_in_mib: 2048,
            }],
            hooks: Hooks {
                port: 9000,
                microvm_hooks: MicrovmHooks {
                    run: HookState::Enabled,
                    run_timeout_in_seconds: 30,
                    resume: HookState::Enabled,
                    resume_timeout_in_seconds: 30,
                    suspend: HookState::Enabled,
                    suspend_timeout_in_seconds: 30,
                    terminate: HookState::Enabled,
                    terminate_timeout_in_seconds: 30,
                },
                microvm_image_hooks: MicrovmImageHooks {
                    ready: HookState::Enabled,
                    ready_timeout_in_seconds: 300,
                    validate: HookState::Enabled,
                    validate_timeout_in_seconds: 300,
                },
            },
            additional_os_capabilities: Some(vec!["ALL".to_string()]),
            logging: None,
            tags: None,
            client_token: "create-img-0011223344556677".to_string(),
        };

        let value = serde_json::to_value(&wire).expect("serialises");
        let object = value.as_object().expect("an object");

        // The exact member names, from the model. `minimumMemoryInMiB` is the one
        // camelCase alone would misspell.
        assert_eq!(value["resources"][0]["minimumMemoryInMiB"], 2048);
        assert_eq!(value["baseImageArn"], "arn:base");
        assert_eq!(value["buildRoleArn"], "arn:role");
        assert_eq!(value["codeArtifact"]["uri"], "s3://bucket/img.zip");
        assert_eq!(value["cpuConfigurations"][0]["architecture"], "ARM_64");
        assert_eq!(
            value["hooks"]["microvmImageHooks"]["readyTimeoutInSeconds"],
            300
        );
        assert_eq!(value["hooks"]["microvmHooks"]["runTimeoutInSeconds"], 30);
        assert_eq!(value["additionalOsCapabilities"][0], "ALL");
        assert_eq!(value["clientToken"], "create-img-0011223344556677");
        assert!(!object.contains_key("tags"), "an absent tag map is omitted");

        // **Issue #24's wrong-by-10x hazard is closed by absence on this shape.** The model gives
        // `CreateMicrovmImageRequest` an `egressNetworkConnectors` with `max: 1` — a tenth of the
        // VM-level list's — and this wire type has no such field, so the client cannot send one at
        // all. That is why `MAX_IMAGE_EGRESS_CONNECTORS` guards nothing: there is nothing to guard.
        //
        // Asserted here, on the serialized object, rather than left as a comment. The hazard is a
        // future edit that adds the member and reuses `MAX_NETWORK_CONNECTORS` — permissive by an
        // order of magnitude, in a request whose rejection lands after the artifact upload. This
        // line is what makes adding the member a visible diff against a test that says the bound is
        // 1: whoever adds it has to delete this assertion and will read why while doing it.
        assert!(
            !object.contains_key("egressNetworkConnectors"),
            "the client sends no image-level egress list. If you are adding one, the ceiling is \
             MAX_IMAGE_EGRESS_CONNECTORS (1), NOT MAX_NETWORK_CONNECTORS (10) — the two differ by \
             10x and the permissive direction is the one that reaches the wire."
        );
    }

    /// `additionalOsCapabilities` is **absent** rather than `[]` when identity repair was
    /// not requested. An empty list is a different request, and the model's `CapabilityList`
    /// has no documented meaning for one.
    #[test]
    fn an_unrequested_capability_list_is_omitted_not_empty() {
        let mut wire = minimal_create_wire();
        wire.additional_os_capabilities = None;
        let value = serde_json::to_value(&wire).expect("serialises");
        assert!(
            !value
                .as_object()
                .expect("object")
                .contains_key("additionalOsCapabilities"),
            "omitted, not empty: {value}"
        );
    }

    /// The run request's member names, including the two connector lists and the payload.
    #[test]
    fn the_run_request_serialises_to_the_models_member_names() {
        let wire = RunMicrovmWire {
            image_identifier: "arn:image".to_string(),
            image_version: None,
            execution_role_arn: Some("arn:aws:iam::123456789012:role/exec".to_string()),
            ingress_network_connectors: vec!["arn:connector:ALL_INGRESS".to_string()],
            egress_network_connectors: None,
            idle_policy: IdlePolicy {
                max_idle_duration_seconds: 600,
                suspended_duration_seconds: 600,
                auto_resume_enabled: false,
            },
            maximum_duration_in_seconds: 3600,
            run_hook_payload: r#"{"agent_token":"t"}"#.to_string(),
            client_token: "run-arn-0011223344556677".to_string(),
        };

        let value = serde_json::to_value(&wire).expect("serialises");
        assert_eq!(value["imageIdentifier"], "arn:image");
        assert_eq!(
            value["ingressNetworkConnectors"][0],
            "arn:connector:ALL_INGRESS"
        );
        assert_eq!(value["idlePolicy"]["maxIdleDurationSeconds"], 600);
        assert_eq!(value["idlePolicy"]["suspendedDurationSeconds"], 600);
        assert_eq!(value["idlePolicy"]["autoResumeEnabled"], false);
        assert_eq!(value["maximumDurationInSeconds"], 3600);
        assert_eq!(value["runHookPayload"], r#"{"agent_token":"t"}"#);
        assert!(
            !value
                .as_object()
                .expect("object")
                .contains_key("egressNetworkConnectors"),
            "omitting egress is how you get no outbound network"
        );
    }

    /// The auth-token request's body. `microvmIdentifier` is a **URI** parameter and must
    /// not appear in the body — a serialized copy of it there is a member the shape does
    /// not declare.
    #[test]
    fn the_auth_token_request_body_carries_no_uri_parameter() {
        let wire = CreateAuthTokenWire {
            expiration_in_minutes: 60,
            allowed_ports: vec![PortSpecification::port(9000)],
        };
        let value = serde_json::to_value(&wire).expect("serialises");
        assert_eq!(value["expirationInMinutes"], 60);
        assert_eq!(value["allowedPorts"][0]["port"], 9000);
        assert!(
            !value
                .as_object()
                .expect("object")
                .contains_key("microvmIdentifier"),
            "it is a uri-located member: {value}"
        );
    }

    /// **Each union variant serialises as the member name the model declares, with nothing
    /// else alongside it.**
    ///
    /// The wire form of a Smithy union is one key, and serde's default enum representation is
    /// `{"One": {"port": 9000}}` — a shape the service rejects as an undeclared member. So
    /// `untagged` plus a `rename` per field is not a style choice, and this test is what says
    /// so: it asserts the *exact* key set, because a variant that serialised as two keys or as
    /// the wrong one would still be valid JSON and would fail only against real AWS.
    ///
    /// The three variants together are what the port-scope fix rests on. Measured 2026-08-15:
    /// a token minted with only `{"port": 9000}` answers 403 `Access to port denied` for a
    /// `GET :8080`, and all three of the forms below answer 200.
    ///
    /// **Guard proof.** Dropping `#[serde(untagged)]` makes every case fail with the variant
    /// name as the key (`{"One": ..}`). Renaming `all_ports`'s field to snake_case makes the
    /// `allPorts` case fail. Making `AllPorts` a unit struct serialises it as `null` and the
    /// object assertion fails. Each applied and observed.
    #[test]
    fn each_port_specification_variant_serialises_as_its_model_member() {
        let cases = [
            (
                PortSpecification::port(8080),
                "port",
                serde_json::json!(8080),
            ),
            (
                PortSpecification::range(8000, 9100),
                "range",
                serde_json::json!({ "startPort": 8000, "endPort": 9100 }),
            ),
            (PortSpecification::all(), "allPorts", serde_json::json!({})),
        ];
        for (spec, key, expected) in cases {
            let value = serde_json::to_value(spec).expect("serialises");
            let object = value.as_object().expect("a union member is an object");
            assert_eq!(
                object.keys().collect::<Vec<_>>(),
                vec![key],
                "a union serialises as exactly one member named by the model: {value}"
            );
            assert_eq!(object[key], expected, "{key} carries the model's shape");
        }
    }

    // ── GetMicrovmImageBuild ─────────────────────────────────────────────────

    /// `GetMicrovmImageBuildOutput` from the model's own spelling, `snapshotBuild` included.
    ///
    /// The literal is a **real response**, copied from a live `GetMicrovmImageBuild` on
    /// 2026-08-16 against `coding-agents-on-bedrock` version 1.0 — so the three byte counts
    /// and the `chipsetGeneration: "4"` are measurements rather than plausible numbers. That
    /// matters for `chipsetGeneration` in particular: it is a `NonBlankString` in the model,
    /// so it comes back as `"4"` and not `4`, and a struct typing it `u32` would fail to parse
    /// every real response.
    ///
    /// **Guard proof.** Type `chipset_generation` as `u32` and this fails with "invalid type:
    /// string". Drop `snapshot_build` and the size assertions do not compile — which is the
    /// point, since those sizes are the only reason this call exists.
    #[test]
    fn a_build_get_response_carries_the_snapshot_sizes_the_listing_has_no_member_for() {
        let json = r#"{
            "imageArn": "arn:aws:lambda:us-east-1:392583147479:microvm-image:coding-agents-on-bedrock",
            "imageVersion": "1.0",
            "buildId": "d39847fa-4c6b-43c6-ba69-e25e3b55197b",
            "buildState": "SUCCESSFUL",
            "architecture": "ARM_64",
            "chipset": "GRAVITON",
            "chipsetGeneration": "4",
            "createdAt": 1755150457.035,
            "snapshotBuild": {
                "memorySnapshotSizeInBytes": 582238208,
                "codeInstallSizeInBytes": 2355486720,
                "diskSnapshotSizeInBytes": 23760896
            }
        }"#;
        let parsed: GetImageBuildResponseWire =
            serde_json::from_str(json).expect("the model's own spelling parses");
        assert_eq!(parsed.build_state, "SUCCESSFUL");
        assert_eq!(parsed.build_id, "d39847fa-4c6b-43c6-ba69-e25e3b55197b");
        // A string, because the model types it NonBlankString. A `u32` field here would fail
        // on every real response.
        assert_eq!(parsed.chipset_generation, "4");
        assert_eq!(parsed.state_reason, None);

        let sizes = parsed.snapshot_build.expect("a successful build has sizes");
        assert_eq!(sizes.memory_snapshot_size_in_bytes, Some(582_238_208));
        assert_eq!(sizes.code_install_size_in_bytes, Some(2_355_486_720));
        assert_eq!(sizes.disk_snapshot_size_in_bytes, Some(23_760_896));
        assert_eq!(
            sizes.describe().as_deref(),
            Some("memory 582238208 bytes, code 2355486720 bytes, disk 23760896 bytes")
        );
    }

    /// **A failed build's `snapshotBuild` is partial, and the partial shape is the diagnosis.**
    ///
    /// Measured 2026-08-16 against a real `FAILED` build (`bonk-sandbox-v4`, ready hook timed
    /// out): `snapshotBuild` carried `codeInstallSizeInBytes` alone, with no memory and no disk
    /// snapshot. That is a build which installed 1.7 GB of code and then never produced a
    /// snapshot, which is a different failure from a Dockerfile that broke before installing
    /// anything — so the two absent members carry information and a defaulted `0` would erase
    /// it.
    ///
    /// **Guard proof.** Change the three `Option<u64>` fields to bare `u64` and this fails to
    /// deserialize on the two missing members. Make `describe` render absent sizes as `0
    /// bytes` and the assertion that the line names only `code` goes red.
    #[test]
    fn a_failed_builds_snapshot_sizes_are_partial_rather_than_zeroed() {
        let json = r#"{
            "imageArn": "arn:aws:lambda:us-east-1:392583147479:microvm-image:bonk-sandbox-v4",
            "imageVersion": "1.0",
            "buildId": "4a4c5e30-811f-47fa-9893-260ea6a37a8f",
            "buildState": "FAILED",
            "architecture": "ARM_64",
            "chipset": "GRAVITON",
            "chipsetGeneration": "4",
            "stateReason": "Ready hook invocation timed out after PT5M",
            "createdAt": 1751042593.476,
            "snapshotBuild": {"codeInstallSizeInBytes": 1724940288}
        }"#;
        let parsed: GetImageBuildResponseWire = serde_json::from_str(json).expect("parses");
        let sizes = parsed
            .snapshot_build
            .expect("the service sent a partial breakdown");
        assert_eq!(sizes.code_install_size_in_bytes, Some(1_724_940_288));
        assert_eq!(
            sizes.memory_snapshot_size_in_bytes, None,
            "no memory snapshot was produced, which is the finding — not a size of zero"
        );
        assert_eq!(sizes.disk_snapshot_size_in_bytes, None);

        let described = parsed.describe();
        assert!(
            described.contains("Ready hook invocation timed out"),
            "{described}"
        );
        assert!(
            described.contains("chipset generation 4"),
            "one CreateMicrovmImage fans out per generation, so the line has to name which: \
             {described}"
        );
        assert!(described.contains("code 1724940288 bytes"), "{described}");
        assert!(
            !described.contains("memory"),
            "an absent size must not render at all: {described}"
        );
    }

    /// A build the service reports with **no** `snapshotBuild` at all renders no sizes and no
    /// dangling comma.
    ///
    /// The model marks the member optional, and a `PENDING` build has nothing to report — so
    /// `None` has to render as absence rather than as an empty clause.
    #[test]
    fn a_build_with_no_snapshot_block_renders_no_size_clause() {
        let json = r#"{
            "imageArn": "arn:aws:lambda:us-east-1:1:microvm-image:img",
            "imageVersion": "1",
            "buildId": "build-abc",
            "buildState": "PENDING",
            "architecture": "ARM_64",
            "chipset": "GRAVITON",
            "chipsetGeneration": "3",
            "createdAt": 1754524800
        }"#;
        let parsed: GetImageBuildResponseWire = serde_json::from_str(json).expect("parses");
        assert_eq!(parsed.snapshot_build, None);
        assert_eq!(
            parsed.describe(),
            "build-abc PENDING (chipset generation 3)"
        );
        assert!(!parsed.describe().ends_with(", "), "{}", parsed.describe());

        // And an *empty* snapshot block is `Some` with three `None`s, which is different from
        // an absent one: the service answered and reported nothing.
        let empty: GetImageBuildResponseWire = serde_json::from_str(
            r#"{"imageArn": "a", "imageVersion": "1", "buildId": "b", "buildState": "PENDING",
                "architecture": "ARM_64", "chipset": "GRAVITON", "chipsetGeneration": "3",
                "createdAt": 1754524800, "snapshotBuild": {}}"#,
        )
        .expect("an empty block parses");
        assert_eq!(empty.snapshot_build.expect("Some").describe(), None);
        assert!(!empty.describe().contains("bytes"), "{}", empty.describe());
    }

    /// The build get response requires the same eight members the summary does.
    ///
    /// One case per member, for the reason the summary's own test gives: a single omission
    /// would prove only that *some* member is required.
    #[test]
    fn a_build_get_response_missing_a_required_member_fails_to_deserialise() {
        let members = [
            (
                "imageArn",
                r#""arn:aws:lambda:us-east-1:1:microvm-image:i""#,
            ),
            ("imageVersion", r#""1""#),
            ("buildId", r#""build-abc""#),
            ("buildState", r#""SUCCESSFUL""#),
            ("architecture", r#""ARM_64""#),
            ("chipset", r#""GRAVITON""#),
            ("chipsetGeneration", r#""4""#),
            ("createdAt", "1754524800"),
        ];
        for omitted in members.iter().map(|(name, _)| *name) {
            let body: Vec<String> = members
                .iter()
                .filter(|(name, _)| *name != omitted)
                .map(|(name, value)| format!(r#""{name}": {value}"#))
                .collect();
            let error = serde_json::from_str::<GetImageBuildResponseWire>(&format!(
                "{{{}}}",
                body.join(", ")
            ))
            .expect_err(&format!("{omitted} is required by the model"));
            assert!(error.to_string().contains(omitted), "{omitted}: {error}");
        }
    }

    // ── GetMicrovmImageVersion / UpdateMicrovmImageVersion ───────────────────

    /// **One struct for three shapes.** A real `GetMicrovmImageVersion` response parses as
    /// `MicrovmImageVersionSummaryWire`, config readback and all.
    ///
    /// The literal is a real response, copied from a live call on 2026-08-16 against
    /// `coding-agents-on-bedrock` version 1.0 — including the detail that
    /// `baseImageVersion` reads `"1.0"` while the managed base's own listing offers `"0"` and
    /// `"1"`. The two spellings are not comparable and this test records that they are not.
    ///
    /// `hooks` is present in the JSON and **not** read, deliberately: see the type's docs and
    /// the sibling test below.
    ///
    /// **Guard proof.** Make `status` an `Option<String>` and the `is_active` assertion still
    /// passes, which is why the *requiredness* test below exists instead. Remove `resources`
    /// and the size-class assertion does not compile.
    #[test]
    fn a_get_image_version_response_parses_as_the_summary_with_its_whole_config() {
        let json = r#"{
            "baseImageArn": "arn:aws:lambda:us-east-1:aws:microvm-image:al2023-1",
            "baseImageVersion": "1.0",
            "buildRoleArn": "arn:aws:iam::392583147479:role/agentd-conformance-build-b2111c56",
            "codeArtifact": {"uri": "s3://agentd-conformance-392583147479-b2111c56/coding-agents-on-bedrock.zip"},
            "egressNetworkConnectors": [
                "arn:aws:lambda:us-east-1:aws:network-connector:aws-network-connector:INTERNET_EGRESS"
            ],
            "cpuConfigurations": [{"architecture": "ARM_64"}],
            "resources": [{"minimumMemoryInMiB": 2048}],
            "hooks": {
                "port": 9000,
                "microvmHooks": {
                    "run": "ENABLED", "runTimeoutInSeconds": 30,
                    "resume": "ENABLED", "resumeTimeoutInSeconds": 30,
                    "suspend": "ENABLED", "suspendTimeoutInSeconds": 30,
                    "terminate": "ENABLED", "terminateTimeoutInSeconds": 30
                },
                "microvmImageHooks": {
                    "ready": "ENABLED", "readyTimeoutInSeconds": 30,
                    "validate": "ENABLED", "validateTimeoutInSeconds": 30
                }
            },
            "imageArn": "arn:aws:lambda:us-east-1:392583147479:microvm-image:coding-agents-on-bedrock",
            "imageVersion": "1.0",
            "state": "SUCCESSFUL",
            "status": "ACTIVE",
            "createdAt": 1755150457.035,
            "updatedAt": 1755150629.117
        }"#;
        let parsed: MicrovmImageVersionSummaryWire =
            serde_json::from_str(json).expect("a real GetMicrovmImageVersion response parses");

        assert_eq!(parsed.image_version, "1.0");
        assert_eq!(parsed.state, "SUCCESSFUL");
        assert_eq!(parsed.status, "ACTIVE");
        assert!(parsed.is_active());
        assert_eq!(
            parsed.base_image_arn,
            "arn:aws:lambda:us-east-1:aws:microvm-image:al2023-1"
        );
        // The spelling that is not comparable with the base's own listing: `"1.0"` here,
        // against `"0"` and `"1"` from ListManagedMicrovmImageVersions.
        assert_eq!(parsed.base_image_version.as_deref(), Some("1.0"));
        assert!(
            parsed
                .code_artifact
                .uri
                .ends_with("coding-agents-on-bedrock.zip")
        );

        // `resources` is the only place a built image's size class is observable — GetMicrovm
        // reports no memory figure at all.
        let resources = parsed
            .resources
            .as_ref()
            .expect("the readback carries the resources");
        assert_eq!(resources.len(), 1);
        assert_eq!(resources[0].minimum_memory_in_mib, 2048);

        // The image-level egress list, whose ceiling is 1 and not the VM-level 10.
        let egress = parsed
            .egress_network_connectors
            .as_ref()
            .expect("one connector");
        assert_eq!(egress.len(), 1);
        assert!(egress.len() <= crate::constants::MAX_IMAGE_EGRESS_CONNECTORS);
        assert!(
            egress.len() < crate::constants::MAX_NETWORK_CONNECTORS,
            "the two ceilings are different numbers; see the constants test that holds the \
             inequality"
        );

        assert_eq!(parsed.state_reason, None, "a successful version has none");
        assert_eq!(parsed.updated_at, Some(1_755_150_629.117));
        assert_eq!(parsed.describe(), "1.0 SUCCESSFUL / ACTIVE");
    }

    /// The version summary requires all eight members the model marks required, `status`
    /// included.
    ///
    /// `status` is the one worth the test. It was `Option<String>` while the model requires it,
    /// so a service that stopped sending it would have read as `None` — and a version blocked
    /// from launching would have looked identical to one nobody had ever set. Confirmed present
    /// on all 22 versions in the conformance account (2026-08-16).
    ///
    /// **Guard proof.** Restore `pub status: Option<String>` and the `status` case here passes
    /// where it should fail; `is_active` then cannot be written at all without a default, which
    /// is the second half of why the field is bare.
    #[test]
    fn a_version_summary_missing_a_required_member_fails_to_deserialise() {
        let members = [
            (
                "baseImageArn",
                r#""arn:aws:lambda:us-east-1:aws:microvm-image:al2023-1""#,
            ),
            ("buildRoleArn", r#""arn:aws:iam::1:role/build""#),
            ("codeArtifact", r#"{"uri": "s3://b/k"}"#),
            (
                "imageArn",
                r#""arn:aws:lambda:us-east-1:1:microvm-image:i""#,
            ),
            ("imageVersion", r#""1.0""#),
            ("state", r#""SUCCESSFUL""#),
            ("status", r#""ACTIVE""#),
            ("createdAt", "1754524800"),
        ];
        for omitted in members.iter().map(|(name, _)| *name) {
            let body: Vec<String> = members
                .iter()
                .filter(|(name, _)| *name != omitted)
                .map(|(name, value)| format!(r#""{name}": {value}"#))
                .collect();
            let error = serde_json::from_str::<MicrovmImageVersionSummaryWire>(&format!(
                "{{{}}}",
                body.join(", ")
            ))
            .expect_err(&format!("{omitted} is required by the model"));
            assert!(error.to_string().contains(omitted), "{omitted}: {error}");
        }
    }

    /// **`hooks` is not read, and a real response is why.**
    ///
    /// The model marks every member of `MicrovmHooks` optional, and a real
    /// `GetMicrovmImageVersion` on an image built by another tool answered
    /// `"microvmHooks": {"run": "ENABLED", "runTimeoutInSeconds": 30}` and nothing else —
    /// measured 2026-08-16 against `omnigent-host-vpc` version 3.0, five of eight members
    /// absent. This module's request-side [`Hooks`] declares all of them as bare fields, which
    /// is correct for a request this client always fills, so reusing it on the response would
    /// fail that parse and turn a readable version into a client error.
    ///
    /// So this test is the record of a deliberate omission: the sparse block parses fine
    /// *because* nothing reads it, and reusing `Hooks` here is the change that breaks it.
    ///
    /// **Guard proof.** Add `pub hooks: Option<Hooks>` to the summary and this test fails with
    /// "missing field `resume`".
    #[test]
    fn a_versions_hooks_block_is_not_read_because_a_real_one_omits_members() {
        let json = r#"{
            "baseImageArn": "arn:aws:lambda:us-east-1:aws:microvm-image:al2023-1",
            "baseImageVersion": "0.0",
            "buildRoleArn": "arn:aws:iam::392583147479:role/bonk-sandbox-microvm-build",
            "codeArtifact": {"uri": "s3://bucket/omnigent-host-overlay.zip"},
            "resources": [{"minimumMemoryInMiB": 2048}],
            "hooks": {
                "port": 9000,
                "microvmHooks": {"run": "ENABLED", "runTimeoutInSeconds": 30},
                "microvmImageHooks": {"ready": "ENABLED", "readyTimeoutInSeconds": 120}
            },
            "imageArn": "arn:aws:lambda:us-east-1:392583147479:microvm-image:omnigent-host-vpc",
            "imageVersion": "3.0",
            "state": "SUCCESSFUL",
            "status": "ACTIVE",
            "createdAt": 1752205430.83
        }"#;
        let parsed: MicrovmImageVersionSummaryWire = serde_json::from_str(json)
            .expect("a sparse hooks block parses precisely because nothing reads it");
        assert_eq!(parsed.image_version, "3.0");

        // And the request-side type really would refuse that block, which is the fact this
        // test exists to hold: the two directions need two shapes.
        let refused = serde_json::from_str::<Hooks>(
            r#"{"port": 9000,
                "microvmHooks": {"run": "ENABLED", "runTimeoutInSeconds": 30},
                "microvmImageHooks": {"ready": "ENABLED", "readyTimeoutInSeconds": 120}}"#,
        )
        .expect_err("the request-side Hooks declares all six enabled flags as required");
        assert!(refused.to_string().contains("resume"), "{refused}");
    }

    /// An `INACTIVE` version reads back as not launchable, and `describe` says so.
    ///
    /// The pair with the `ACTIVE` case above, so `is_active` is a comparison rather than a
    /// function that returns true.
    ///
    /// **Guard proof.** Invert `is_active` and this fails; compare against a bare `"Active"`
    /// literal instead of `VersionStatus::Active.as_str()` and it fails too.
    #[test]
    fn an_inactive_version_reads_back_as_not_launchable() {
        let json = r#"{
            "baseImageArn": "arn:aws:lambda:us-east-1:aws:microvm-image:al2023-1",
            "buildRoleArn": "arn:aws:iam::1:role/build",
            "codeArtifact": {"uri": "s3://b/k"},
            "imageArn": "arn:aws:lambda:us-east-1:1:microvm-image:img",
            "imageVersion": "2.0",
            "state": "SUCCESSFUL",
            "status": "INACTIVE",
            "createdAt": 1754524800,
            "updatedAt": 1754528400
        }"#;
        let parsed: MicrovmImageVersionSummaryWire = serde_json::from_str(json).expect("parses");
        assert!(
            !parsed.is_active(),
            "INACTIVE is the version RunMicrovm refuses to launch"
        );
        assert_eq!(parsed.describe(), "2.0 SUCCESSFUL / INACTIVE");
        assert_eq!(
            parsed.state, "SUCCESSFUL",
            "the state and the status are different questions: a SUCCESSFUL build can be \
             retired, which is the whole point of the status member"
        );
    }

    /// The version summary's `describe` carries a reason when there is one, and nothing when
    /// there is not.
    #[test]
    fn a_version_describes_itself_with_only_a_reason_it_has() {
        let json = r#"{
            "baseImageArn": "arn:aws:lambda:us-east-1:aws:microvm-image:al2023-1",
            "buildRoleArn": "arn:aws:iam::1:role/build",
            "codeArtifact": {"uri": "s3://b/k"},
            "imageArn": "arn:aws:lambda:us-east-1:1:microvm-image:img",
            "imageVersion": "2.0",
            "state": "FAILED",
            "status": "INACTIVE",
            "createdAt": 1754524800,
            "stateReason": "one or more builds failed"
        }"#;
        let parsed: MicrovmImageVersionSummaryWire = serde_json::from_str(json).expect("parses");
        assert_eq!(
            parsed.describe(),
            "2.0 FAILED / INACTIVE (one or more builds failed)"
        );
        assert!(!parsed.describe().contains("()"), "{}", parsed.describe());
    }

    /// The update request serialises to **one** member, `status`, spelled as the model spells
    /// it — and both enum values are asserted.
    ///
    /// The key set is asserted exactly, because `imageIdentifier` and `imageVersion` are URI
    /// parameters and a serialized copy of either in the body is a member the shape does not
    /// declare.
    ///
    /// **Guard proof.** Drop the `#[serde(rename = "ACTIVE")]` attributes and the values
    /// serialise as `"Active"`/`"Inactive"`, which the service refuses as an enum violation —
    /// both assertions go red.
    #[test]
    fn the_update_version_request_is_one_member_spelled_as_the_model_spells_it() {
        for (status, expected) in [
            (VersionStatus::Active, "ACTIVE"),
            (VersionStatus::Inactive, "INACTIVE"),
        ] {
            let value =
                serde_json::to_value(UpdateImageVersionWire { status }).expect("serialises");
            let object = value.as_object().expect("an object");
            assert_eq!(
                object.keys().collect::<Vec<_>>(),
                vec!["status"],
                "imageIdentifier and imageVersion are uri-located: {value}"
            );
            assert_eq!(object["status"], expected);
            assert_eq!(status.as_str(), expected);
            assert_eq!(status.to_string(), expected);
        }
    }

    /// The typed request spelling and the drift gate's published array are the same two
    /// values.
    ///
    /// The coupling worth pinning: `constants::IMAGE_VERSION_STATUSES` is what the gate holds
    /// against the model's shape, and [`VersionStatus`] is what a call site can construct. If
    /// they disagree the gate passes while the client sends a value the model does not declare.
    #[test]
    fn the_typed_statuses_are_exactly_the_published_ones() {
        let typed: Vec<&str> = [VersionStatus::Active, VersionStatus::Inactive]
            .iter()
            .map(|status| status.as_str())
            .collect();
        assert_eq!(typed, crate::constants::IMAGE_VERSION_STATUSES);
    }

    // ── the managed listings ─────────────────────────────────────────────────

    /// `ListManagedMicrovmImageVersionsOutput` from a **real** response.
    ///
    /// Copied from a live call on 2026-08-16: `al2023-1` answers two versions, `"1"` and
    /// `"0"`, newest first. The bare-integer spelling is the finding — a custom image's
    /// versions are `"1.0"` — and it is why a version string cannot be parsed by one rule.
    #[test]
    fn a_managed_version_listing_carries_bare_integer_versions_newest_first() {
        let json = r#"{
            "items": [
                {
                    "imageArn": "arn:aws:lambda:us-east-1:aws:microvm-image:al2023-1",
                    "imageVersion": "1",
                    "createdAt": 1753119464.231,
                    "updatedAt": 1753119644.78
                },
                {
                    "imageArn": "arn:aws:lambda:us-east-1:aws:microvm-image:al2023-1",
                    "imageVersion": "0",
                    "createdAt": 1750180266.531,
                    "updatedAt": 1750180537.679
                }
            ]
        }"#;
        let parsed: ListManagedVersionsResponseWire =
            serde_json::from_str(json).expect("the model's own spelling parses");
        assert_eq!(parsed.items.len(), 2);
        assert_eq!(parsed.items[0].image_version, "1");
        assert_eq!(parsed.items[1].image_version, "0");
        assert_eq!(parsed.next_token, None, "one page, so the loop stops");
        assert_eq!(
            parsed.items[0].image_arn,
            "arn:aws:lambda:us-east-1:aws:microvm-image:al2023-1"
        );
        assert_eq!(parsed.items[0].updated_at, Some(1_753_119_644.78));

        // Bare integers, not the `major.minor` a custom image uses. A client that parsed one
        // format would not parse the other, and neither can be compared with the `"1.0"` that
        // GetMicrovmImageVersion echoes as `baseImageVersion`.
        for item in &parsed.items {
            assert!(
                !item.image_version.contains('.'),
                "a managed base's versions are bare integers: {}",
                item.image_version
            );
        }
    }

    /// `ManagedMicrovmImageSummary` is an ARN and two timestamps, and **that is all**.
    ///
    /// Asserted as an absence, which is the reason this shape cannot construct a
    /// [`crate::control::BaseImage`]: no registry reference for `require_matching_from` to
    /// compare a Dockerfile against, and no working directory for `require_workdir` to check.
    /// The absence is checked by deserializing a response that *does* carry extra members and
    /// showing there is no field to read them into — the same technique
    /// `a_version_summary_carries_the_state_reason_get_image_cannot` uses.
    #[test]
    fn a_managed_image_summary_carries_no_registry_ref_so_it_cannot_build_a_base_image() {
        let json = r#"{
            "items": [{
                "imageArn": "arn:aws:lambda:us-east-1:aws:microvm-image:al2023-1",
                "createdAt": 1750180266.531,
                "updatedAt": 1753119644.78
            }]
        }"#;
        let parsed: ListManagedImagesResponseWire =
            serde_json::from_str(json).expect("the model's own spelling parses");
        assert_eq!(parsed.items.len(), 1, "measured 2026-08-16: exactly one");
        assert_eq!(
            parsed.items[0].image_arn,
            "arn:aws:lambda:us-east-1:aws:microvm-image:al2023-1"
        );

        // The whole struct, as JSON, is three keys. There is nothing here a BaseImage's
        // `docker_ref` or `working_dir` could come from, and the ARN does not imply either:
        // `al2023-1` pairs with public.ecr.aws/amazonlinux/amazonlinux:2023-minimal, and
        // nothing in the ARN says so.
        let base = crate::control::BaseImage::al2023();
        assert_eq!(
            base.docker_ref, "public.ecr.aws/amazonlinux/amazonlinux:2023-minimal",
            "the pairing a discovered base cannot supply"
        );
        assert!(
            !parsed.items[0].image_arn.contains(&base.docker_ref),
            "the ARN does not carry the registry ref, so discovery cannot derive it"
        );

        // A required member missing still fails, so the three that *are* there are load-bearing.
        let error = serde_json::from_str::<ManagedMicrovmImageSummaryWire>(
            r#"{"createdAt": 1750180266.531}"#,
        )
        .expect_err("imageArn is required");
        assert!(error.to_string().contains("imageArn"), "{error}");
    }

    // ── the two unsent request members ───────────────────────────────────────

    /// `CreateMicrovmImage.baseImageVersion` reaches the wire when pinned and is **absent**
    /// when it is not.
    ///
    /// Absent rather than null or empty, because an absent member means "whatever the service
    /// defaults to" while a blank one is a `ValidationException` on the `Version` shape's
    /// `min: 1`. The two are different requests and only one of them is legal.
    ///
    /// **Guard proof.** Remove `skip_serializing_if` and the absence assertion fails with a
    /// `"baseImageVersion": null` on every unpinned build — which the service rejects.
    #[test]
    fn a_pinned_base_image_version_reaches_the_wire_and_an_unpinned_one_is_omitted() {
        let mut wire = minimal_create_wire();
        wire.base_image_version = Some("1".to_string());
        let value = serde_json::to_value(&wire).expect("serialises");
        assert_eq!(value["baseImageVersion"], "1");

        let mut unpinned = minimal_create_wire();
        unpinned.base_image_version = None;
        let value = serde_json::to_value(&unpinned).expect("serialises");
        assert!(
            !value
                .as_object()
                .expect("object")
                .contains_key("baseImageVersion"),
            "omitted, not null: an absent member takes the service default, and a blank one \
             is a ValidationException: {value}"
        );
    }

    /// `RunMicrovm.imageVersion` reaches the wire when pinned and is **absent** when it is not.
    ///
    /// The absence is what this client always sent, so the field existing must not change an
    /// unpinned launch by a byte — a `"imageVersion": null` on every launch would be a new
    /// member on a request that has worked for months.
    ///
    /// **Guard proof.** Remove `skip_serializing_if` and the absence assertion fails.
    #[test]
    fn a_pinned_launch_version_reaches_the_wire_and_an_unpinned_one_is_omitted() {
        let mut wire = minimal_run_wire();
        wire.image_version = Some("2.0".to_string());
        let value = serde_json::to_value(&wire).expect("serialises");
        assert_eq!(value["imageVersion"], "2.0");
        assert_eq!(
            value["imageIdentifier"], "arn:image",
            "the version does not replace the identifier; both are sent"
        );

        let value = serde_json::to_value(minimal_run_wire()).expect("serialises");
        assert!(
            !value
                .as_object()
                .expect("object")
                .contains_key("imageVersion"),
            "an unpinned launch must emit byte-for-byte what this client always sent: {value}"
        );
    }

    fn minimal_run_wire() -> RunMicrovmWire {
        RunMicrovmWire {
            image_identifier: "arn:image".to_string(),
            image_version: None,
            execution_role_arn: None,
            ingress_network_connectors: Vec::new(),
            egress_network_connectors: None,
            idle_policy: IdlePolicy {
                max_idle_duration_seconds: 600,
                suspended_duration_seconds: 600,
                auto_resume_enabled: false,
            },
            maximum_duration_in_seconds: 3_600,
            run_hook_payload: String::new(),
            client_token: "run-arn-0011223344556677".to_string(),
        }
    }

    fn minimal_create_wire() -> CreateMicrovmImageWire {
        CreateMicrovmImageWire {
            name: "img".to_string(),
            base_image_arn: "arn:base".to_string(),
            base_image_version: None,
            build_role_arn: "arn:role".to_string(),
            code_artifact: CodeArtifact {
                uri: "s3://b/k".to_string(),
            },
            cpu_configurations: vec![CpuConfiguration {
                architecture: "ARM_64".to_string(),
            }],
            resources: vec![Resources {
                minimum_memory_in_mib: 2048,
            }],
            hooks: Hooks {
                port: 9000,
                microvm_hooks: MicrovmHooks {
                    run: HookState::Enabled,
                    run_timeout_in_seconds: 30,
                    resume: HookState::Enabled,
                    resume_timeout_in_seconds: 30,
                    suspend: HookState::Enabled,
                    suspend_timeout_in_seconds: 30,
                    terminate: HookState::Enabled,
                    terminate_timeout_in_seconds: 30,
                },
                microvm_image_hooks: MicrovmImageHooks {
                    ready: HookState::Enabled,
                    ready_timeout_in_seconds: 300,
                    validate: HookState::Enabled,
                    validate_timeout_in_seconds: 300,
                },
            },
            additional_os_capabilities: None,
            logging: None,
            tags: None,
            client_token: "create-img-0011223344556677".to_string(),
        }
    }

    // ── the logging union ─────────────────────────────────────────────────────

    /// **Each `Logging` variant serialises as the member name the model declares, with
    /// nothing else alongside it** — the same claim
    /// [`each_port_specification_variant_serialises_as_its_model_member`] holds for the
    /// other union this client sends.
    ///
    /// **Guard proof.** Dropping `#[serde(untagged)]` makes both cases fail with the
    /// variant name as the key (`{"Disabled": ..}`); making `LoggingDisabled` a unit
    /// struct serialises it as `null` and the object assertion fails. Both applied and
    /// observed.
    #[test]
    fn each_logging_variant_serialises_as_its_model_member() {
        let cases = [
            (Logging::disabled(), "disabled", serde_json::json!({})),
            (
                Logging::cloud_watch("/aws/lambda-microvms/builds", None),
                "cloudWatch",
                serde_json::json!({"logGroup": "/aws/lambda-microvms/builds"}),
            ),
            (
                Logging::cloud_watch(
                    "/aws/lambda-microvms/builds",
                    Some("img/deadbeef00010203".to_string()),
                ),
                "cloudWatch",
                serde_json::json!({
                    "logGroup": "/aws/lambda-microvms/builds",
                    "logStream": "img/deadbeef00010203"
                }),
            ),
        ];
        for (logging, key, expected) in cases {
            let value = serde_json::to_value(&logging).expect("serialises");
            let object = value.as_object().expect("a union member is an object");
            assert_eq!(
                object.keys().collect::<Vec<_>>(),
                vec![key],
                "a union serialises as exactly one member named by the model: {value}"
            );
            assert_eq!(object[key], expected, "{key} carries the model's shape");
        }
    }

    /// A configured `logging` reaches the wire under the model's member name, and an
    /// unconfigured one is **absent** — not `null`, not `{}`.
    ///
    /// The absence half is the compatibility claim: a create with no logging config must
    /// emit byte-for-byte the request this client always sent, because an absent member
    /// takes the service default and a `"logging": null` is a new member on a request
    /// that has worked for months.
    ///
    /// **Guard proof.** Remove `skip_serializing_if` from `logging` and the absence
    /// assertion fails with `"logging": null` on every unconfigured create.
    #[test]
    fn a_configured_logging_reaches_the_wire_and_an_unconfigured_one_is_omitted() {
        let mut wire = minimal_create_wire();
        wire.logging = Some(Logging::cloud_watch(
            "/aws/lambda-microvms/builds",
            Some("img/deadbeef00010203".to_string()),
        ));
        let value = serde_json::to_value(&wire).expect("serialises");
        assert_eq!(
            value["logging"]["cloudWatch"]["logGroup"],
            "/aws/lambda-microvms/builds"
        );
        assert_eq!(
            value["logging"]["cloudWatch"]["logStream"],
            "img/deadbeef00010203"
        );

        let value = serde_json::to_value(minimal_create_wire()).expect("serialises");
        assert!(
            !value.as_object().expect("object").contains_key("logging"),
            "omitted, not null: an absent member takes the service default: {value}"
        );
    }

    /// A group with no stream serialises without a `logStream` key at all: the member is
    /// optional in the model, and an absent stream means "the service names the streams"
    /// while an empty one is a `ValidationException` on the shape's `min: 1`.
    #[test]
    fn a_group_only_logging_config_omits_the_stream_member() {
        let value = serde_json::to_value(Logging::cloud_watch("/aws/lambda-microvms/b", None))
            .expect("serialises");
        let cloud_watch = value["cloudWatch"].as_object().expect("an object");
        assert_eq!(
            cloud_watch.keys().collect::<Vec<_>>(),
            vec!["logGroup"],
            "an absent stream is omitted, not empty: {value}"
        );
    }
}
