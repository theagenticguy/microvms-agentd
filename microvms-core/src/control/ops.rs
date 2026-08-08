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
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Hooks {
    pub port: u16,
    pub microvm_hooks: MicrovmHooks,
    pub microvm_image_hooks: MicrovmImageHooks,
}

/// `MicrovmHooks` — the family that caps at 60 seconds.
///
/// The `HookState` members are `String` holding `ENABLED`/`DISABLED`; the timeouts are
/// built from [`crate::hooks::RunHookTimeout`] before they get here, which is where the
/// ceiling is enforced.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MicrovmHooks {
    pub run: String,
    pub run_timeout_in_seconds: u32,
    pub resume: String,
    pub resume_timeout_in_seconds: u32,
    pub suspend: String,
    pub suspend_timeout_in_seconds: u32,
    pub terminate: String,
    pub terminate_timeout_in_seconds: u32,
}

/// `MicrovmImageHooks` — the build family, capping at 3600 seconds.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MicrovmImageHooks {
    pub ready: String,
    pub ready_timeout_in_seconds: u32,
    pub validate: String,
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

/// `IdlePolicy`. All three members are required by the model.
///
/// `suspendedDurationSeconds` exists **only in the request** — `GetMicrovm` returns an
/// `idlePolicy` but the client is still the only party that can name the window it asked
/// for at launch, which is what STATE-12 rests on.
#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct IdlePolicy {
    /// `min: 60`, one of the few constraints botocore enforces locally, so there is
    /// deliberately no guard for it here.
    pub max_idle_duration_seconds: u32,
    pub suspended_duration_seconds: u32,
    pub auto_resume_enabled: bool,
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

/// `PortSpecification`, a union. This client only ever names a single port.
#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PortSpecification {
    pub port: u16,
}

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
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GetMicrovmImageResponseWire {
    pub image_arn: String,
    pub name: String,
    pub state: String,
    pub latest_active_image_version: Option<String>,
    pub latest_failed_image_version: Option<String>,
}

/// `ListMicrovmImageVersionsOutput`.
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ListImageVersionsResponseWire {
    pub items: Vec<MicrovmImageVersionSummaryWire>,
    pub next_token: Option<String>,
}

/// `MicrovmImageVersionSummary`, narrowed. Note `state` here **is** the member's name —
/// unlike the build summary below, and that asymmetry is the whole trap.
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MicrovmImageVersionSummaryWire {
    pub image_version: String,
    pub state: String,
    pub status: Option<String>,
    pub state_reason: Option<String>,
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
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MicrovmImageBuildSummaryWire {
    pub build_id: String,
    pub build_state: String,
    pub image_version: String,
    pub state_reason: Option<String>,
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
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeleteImageResponseWire {
    pub image_identifier: String,
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
            "imageArn": "arn:aws:lambda:us-east-1:123456789012:microvm-image/img",
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
            "imageArn": "arn:aws:lambda:us-east-1:123456789012:microvm-image/img",
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
            "baseImageArn": "arn:aws:lambda:us-east-1:aws:microvm-image/al2023-1",
            "buildRoleArn": "arn:aws:iam::123456789012:role/build",
            "codeArtifact": {"uri": "s3://bucket/img.zip"},
            "imageArn": "arn:aws:lambda:us-east-1:123456789012:microvm-image/img",
            "imageVersion": "1",
            "state": "SUCCESSFUL",
            "status": "ACTIVE",
            "createdAt": 1754524800
        }"#;
        let parsed: MicrovmImageVersionSummaryWire =
            serde_json::from_str(json).expect("the version summary's member is state");
        assert_eq!(parsed.state, "SUCCESSFUL");
        assert_eq!(parsed.status.as_deref(), Some("ACTIVE"));
    }

    /// `RunMicrovmResponse` from the model's spelling, including the optional
    /// `stateReason` that TRAP-8 reports.
    #[test]
    fn a_run_response_deserialises_with_its_state_reason() {
        let json = r#"{
            "microvmId": "mvm-123",
            "state": "TERMINATED",
            "endpoint": "https://mvm-123.microvm.amazonaws.com",
            "imageArn": "arn:aws:lambda:us-east-1:123456789012:microvm-image/img",
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
            "imageArn": "arn:aws:lambda:us-east-1:123456789012:microvm-image/img",
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
                    run: "ENABLED".to_string(),
                    run_timeout_in_seconds: 30,
                    resume: "ENABLED".to_string(),
                    resume_timeout_in_seconds: 30,
                    suspend: "ENABLED".to_string(),
                    suspend_timeout_in_seconds: 30,
                    terminate: "ENABLED".to_string(),
                    terminate_timeout_in_seconds: 30,
                },
                microvm_image_hooks: MicrovmImageHooks {
                    ready: "ENABLED".to_string(),
                    ready_timeout_in_seconds: 300,
                    validate: "ENABLED".to_string(),
                    validate_timeout_in_seconds: 300,
                },
            },
            additional_os_capabilities: Some(vec!["ALL".to_string()]),
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
            allowed_ports: vec![PortSpecification { port: 9000 }],
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

    fn minimal_create_wire() -> CreateMicrovmImageWire {
        CreateMicrovmImageWire {
            name: "img".to_string(),
            base_image_arn: "arn:base".to_string(),
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
                    run: "ENABLED".to_string(),
                    run_timeout_in_seconds: 30,
                    resume: "ENABLED".to_string(),
                    resume_timeout_in_seconds: 30,
                    suspend: "ENABLED".to_string(),
                    suspend_timeout_in_seconds: 30,
                    terminate: "ENABLED".to_string(),
                    terminate_timeout_in_seconds: 30,
                },
                microvm_image_hooks: MicrovmImageHooks {
                    ready: "ENABLED".to_string(),
                    ready_timeout_in_seconds: 300,
                    validate: "ENABLED".to_string(),
                    validate_timeout_in_seconds: 300,
                },
            },
            additional_os_capabilities: None,
            tags: None,
            client_token: "create-img-0011223344556677".to_string(),
        }
    }
}
