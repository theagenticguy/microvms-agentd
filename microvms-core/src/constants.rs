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
//! `IdlePolicy.maxIdleDurationSeconds` is the counter-example and deliberately has no
//! constant here: its constraint is `min: 60`, which botocore *does* enforce locally
//! with a clear message.
//!
//! # The drift gate (TRAP-12)
//!
//! [`as_json`] emits every value in this module as one object, keyed with the names the
//! deleted Python client's `sandbox.py` used. That is what makes the gate possible:
//! `scripts/check-model-drift` reads this object and compares each constant against the
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
pub const MAX_RUN_HOOK_PAYLOAD_BYTES: usize = 4096;

/// `ImageName.max`. Minimum is 1, and botocore does enforce that one.
pub const MAX_IMAGE_NAME_LEN: usize = 64;

/// `ImageName.pattern`, as the model spells it.
///
/// No dots and no slashes, which rules out the two separators a caller reaching for a
/// namespaced name writes first. Published as a string for the drift gate; the
/// matcher is [`is_valid_image_name`], which is hand-rolled rather than a regex
/// dependency because the character class is four ranges.
pub const IMAGE_NAME_PATTERN: &str = "[a-zA-Z0-9-_]+";

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

/// `NetworkConnectorList.max`.
pub const MAX_NETWORK_CONNECTORS: usize = 10;

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
/// Hand-rolled: the pattern is `[a-zA-Z0-9-_]+`, four character ranges, and a regex
/// crate for that is a dependency plus a compiled automaton plus a second place the
/// pattern is written down. The length check is here too because a caller asking "is
/// this name legal" means both constraints — they arrive from the same model shape and
/// the service rejects on either.
pub fn is_valid_image_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_IMAGE_NAME_LEN
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
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
pub fn as_json() -> Value {
    json!({
        "MODEL_API_VERSION": MODEL_API_VERSION,
        "MAX_RUN_HOOK_PAYLOAD_BYTES": MAX_RUN_HOOK_PAYLOAD_BYTES,
        "MAX_IMAGE_NAME_LEN": MAX_IMAGE_NAME_LEN,
        "IMAGE_NAME_PATTERN": IMAGE_NAME_PATTERN,
        "MAX_DURATION_SEC": MAX_DURATION_SEC,
        "MAX_MICROVM_HOOK_TIMEOUT_SEC": MAX_MICROVM_HOOK_TIMEOUT_SEC,
        "MAX_IMAGE_HOOK_TIMEOUT_SEC": MAX_IMAGE_HOOK_TIMEOUT_SEC,
        "MAX_HOOK_PORT": MAX_HOOK_PORT,
        "CAPABILITIES": CAPABILITIES,
        "ARCHITECTURES": ARCHITECTURES,
        "MAX_NETWORK_CONNECTORS": MAX_NETWORK_CONNECTORS,
        "MAX_RESOURCES": MAX_RESOURCES,
        "MAX_CLIENT_TOKEN_LEN": MAX_CLIENT_TOKEN_LEN,
        "MODEL_IMAGE_READY_STATES": MODEL_IMAGE_READY_STATES,
        "TOLERATED_IMAGE_READY_STATES": TOLERATED_IMAGE_READY_STATES,
        "TERMINAL_STATES": TERMINAL_STATES,
        "DEAD_STATES": DEAD_STATES,
        "MICROVM_REGIONS": MICROVM_REGIONS
            .iter()
            .map(|region| region.as_str())
            .collect::<Vec<_>>(),
        "SIZE_CLASSES": crate::sizing::SIZE_CLASSES
            .iter()
            .map(|row| json!({
                "baseline_mib": row.baseline_mib,
                "baseline_vcpu": row.baseline_vcpu,
                "peak_mib": row.peak_mib,
                "peak_vcpu": row.peak_vcpu,
            }))
            .collect::<Vec<_>>(),
    })
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
                "CAPABILITIES",
                "DEAD_STATES",
                "IMAGE_NAME_PATTERN",
                "MAX_CLIENT_TOKEN_LEN",
                "MAX_DURATION_SEC",
                "MAX_HOOK_PORT",
                "MAX_IMAGE_HOOK_TIMEOUT_SEC",
                "MAX_IMAGE_NAME_LEN",
                "MAX_MICROVM_HOOK_TIMEOUT_SEC",
                "MAX_NETWORK_CONNECTORS",
                "MAX_RESOURCES",
                "MAX_RUN_HOOK_PAYLOAD_BYTES",
                "MICROVM_REGIONS",
                "MODEL_API_VERSION",
                "MODEL_IMAGE_READY_STATES",
                "SIZE_CLASSES",
                "TERMINAL_STATES",
                "TOLERATED_IMAGE_READY_STATES",
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
        assert_eq!(emitted["MAX_DURATION_SEC"], 28_800);
        assert_eq!(emitted["MAX_MICROVM_HOOK_TIMEOUT_SEC"], 60);
        assert_eq!(emitted["MAX_IMAGE_HOOK_TIMEOUT_SEC"], 3_600);
        assert_eq!(emitted["MAX_HOOK_PORT"], 65_535);
        assert_eq!(emitted["CAPABILITIES"], json!(["ALL"]));
        assert_eq!(emitted["ARCHITECTURES"], json!(["ARM_64"]));
        assert_eq!(emitted["MAX_NETWORK_CONNECTORS"], 10);
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
}
