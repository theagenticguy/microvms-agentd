// SPDX-License-Identifier: Apache-2.0
//! Lifecycle-hook wire types and the prefix the platform calls them under.
//!
//! Not client types. A consumer must never post to these paths — they are the
//! platform's, and `/run` in particular is the one route whose success is not
//! repeatable — but the shapes belong here anyway, because a client generator
//! reading the schema has to be told what the daemon accepts on them.

use std::collections::HashMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Prefix the platform uses for every lifecycle hook. Fixed by the service.
pub const HOOK_PREFIX: &str = "/aws/lambda-microvms/runtime/v1";

/// The envelope the platform posts to the run hook.
///
/// The `runHookPayload` string given to `RunMicrovm` is not delivered as the
/// request body: the platform wraps it, so the body is
/// `{"runHookPayload": "<the caller's string>"}` and the caller's own JSON is one
/// `serde_json` parse deeper. Measured 2026-08-05 — a daemon that reads
/// `agent_token` from the top level answers 400, and the platform then terminates
/// the VM with "Run lifecycle hook returned HTTP status 400" before any traffic is
/// forwarded, so the mistake is invisible from the outside.
#[derive(Debug, Deserialize, JsonSchema, Serialize)]
pub struct RunHookEnvelope {
    #[serde(rename = "runHookPayload")]
    pub run_hook_payload: Option<String>,
}

/// The caller's own payload, carrying the per-VM secret and an optional launch
/// environment.
///
/// Passing the token at launch is what keeps it out of the shared image snapshot.
/// It is safe because the platform forwards no external traffic until this hook
/// returns 200.
///
/// `env` rides the same channel because the channel is the only per-VM one the
/// platform offers, and a launch environment is the same kind of thing as the
/// token: something a caller knows at launch and cannot bake into a shared image.
/// The whole payload shares one 4096-byte budget, so a caller who fills `env`
/// with credentials is spending the token's room; `microvms-core` refuses an
/// over-budget payload locally, before the call.
#[derive(Debug, Default, Deserialize, JsonSchema, Serialize)]
pub struct RunHook {
    pub agent_token: String,
    /// Base environment applied to every subsequent exec, under the per-request
    /// `env`. Absent and empty mean the same thing, which is why this is a plain
    /// map rather than an `Option`: there is nothing a caller could express by
    /// sending `"env": {}` that omitting the key does not already say.
    #[serde(default)]
    pub env: HashMap<String, String>,
    /// The VM's own identity seed, base64 of 32 bytes, or `None`.
    ///
    /// An `Option` rather than a defaulted value because the absence *is*
    /// information here, unlike `env`: a VM launched without a seed has no key to
    /// prove anything with, and the tunnel must refuse `--verify-identity` against
    /// it (close code 4401) rather than downgrade silently. See
    /// [`super::identity`] for why the material travels on this channel at all.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity_seed: Option<String>,
    /// The launching host's public key, base64 of 32 bytes, or `None`.
    ///
    /// The other half of mutual authentication: the daemon pins this, so a
    /// handshake proves the peer is the host that launched this VM rather than
    /// anyone who came to hold the agent token. Separate from the seed because a
    /// caller could deliver one without the other, and the daemon reports which
    /// half is missing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity_host_public_key: Option<String>,
}

/// Why a `runHookPayload` string could not be read as a [`RunHook`].
///
/// A typed error rather than serde's own message, and the reason is the trust
/// contract rather than tidiness: this payload carries the agent token, and
/// `serde_json`'s messages quote the offending value. A message that quoted a
/// value would put payload contents into a log line and a response body, which is
/// the one thing `docs/TRUST.md` promises never happens. Each variant below names
/// a *key* or a *shape* and never a value.
#[derive(Debug, Eq, PartialEq)]
pub enum RunHookError {
    /// The payload string parsed as JSON but is not an object, or is not JSON.
    NotAnObject,
    /// No `agent_token` key, or one whose value is not a string.
    TokenMissingOrNotAString,
    /// `agent_token` is the empty string. Accepting it would install a credential
    /// every caller can guess.
    TokenEmpty,
    /// `env` is present but is not a JSON object.
    EnvNotAnObject,
    /// `env` holds a value that is not a string, under this key. Only the key is
    /// named — a value is where a secret would be.
    EnvValueNotAString(String),
    /// An identity key is present but is not a string. Names the key, never the
    /// value: both identity values are key material.
    IdentityNotAString(&'static str),
}

impl std::fmt::Display for RunHookError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RunHookError::NotAnObject => f.write_str(
                "runHookPayload is not a JSON object. The platform wraps the string given to \
                 RunMicrovm, so the payload is one parse deeper than the request body: \
                 {\"runHookPayload\": \"{\\\"agent_token\\\": \\\"...\\\"}\"}",
            ),
            RunHookError::TokenMissingOrNotAString => {
                f.write_str("runHookPayload has no agent_token string")
            }
            RunHookError::TokenEmpty => f.write_str("agent_token is empty"),
            RunHookError::EnvNotAnObject => f.write_str(
                "env must be a JSON object of string keys to string values, and it is not an \
                 object",
            ),
            RunHookError::EnvValueNotAString(key) => write!(
                f,
                "env[{key:?}] is not a string. Every launch-environment value is a string, \
                 because an environment variable is a string — a number or a nested object \
                 would have to be stringified by someone, and guessing which spelling the \
                 caller meant is how a credential arrives mangled"
            ),
            RunHookError::IdentityNotAString(key) => write!(
                f,
                "{key} must be a string: standard base64 of exactly 32 bytes. The value is \
                 not quoted here because it is key material"
            ),
        }
    }
}

impl RunHook {
    /// Reads the caller's payload, ignoring keys this version does not know.
    ///
    /// Unknown keys are ignored on purpose: a newer client sending a field this
    /// daemon has never heard of must still be able to bootstrap it, because the
    /// alternative is a 400 at the run hook and the platform terminating the VM
    /// before any traffic is forwarded. Forward compatibility here is the
    /// difference between an ignored field and a dead launch.
    ///
    /// Hand-walked rather than `serde_json::from_str::<RunHook>` so every refusal
    /// is one of the named variants above. `serde`'s own messages quote values,
    /// and the values here are secrets.
    pub fn parse(raw: &str) -> Result<Self, RunHookError> {
        let value: serde_json::Value =
            serde_json::from_str(raw).map_err(|_| RunHookError::NotAnObject)?;
        let object = value.as_object().ok_or(RunHookError::NotAnObject)?;

        let agent_token = object
            .get("agent_token")
            .and_then(serde_json::Value::as_str)
            .ok_or(RunHookError::TokenMissingOrNotAString)?;
        if agent_token.is_empty() {
            return Err(RunHookError::TokenEmpty);
        }

        let mut env = HashMap::new();
        match object.get("env") {
            None | Some(serde_json::Value::Null) => {}
            Some(raw_env) => {
                let entries = raw_env.as_object().ok_or(RunHookError::EnvNotAnObject)?;
                for (key, value) in entries {
                    let value = value
                        .as_str()
                        .ok_or_else(|| RunHookError::EnvValueNotAString(key.clone()))?;
                    env.insert(key.clone(), value.to_string());
                }
            }
        }

        // Both identity halves are optional and read the same way, so one closure rather than
        // two blocks: a second spelling is a second place for the "absent means None, present
        // but wrong means refuse" distinction to be got wrong.
        let identity_value = |key: &'static str| -> Result<Option<String>, RunHookError> {
            match object.get(key) {
                None | Some(serde_json::Value::Null) => Ok(None),
                Some(serde_json::Value::String(text)) => Ok(Some(text.clone())),
                Some(_) => Err(RunHookError::IdentityNotAString(key)),
            }
        };

        Ok(Self {
            agent_token: agent_token.to_string(),
            env,
            identity_seed: identity_value(crate::identity::SEED_KEY)?,
            identity_host_public_key: identity_value(crate::identity::HOST_PUBLIC_KEY_KEY)?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The envelope key is the platform's camelCase spelling, and getting it wrong
    /// terminates the VM with a 400 before any traffic is forwarded.
    #[test]
    fn the_envelope_key_is_the_platforms_own_camel_case_spelling() {
        let envelope: RunHookEnvelope =
            serde_json::from_str(r#"{"runHookPayload":"{}"}"#).expect("deserializes");
        assert_eq!(envelope.run_hook_payload.as_deref(), Some("{}"));
        assert_eq!(
            serde_json::to_string(&envelope).expect("serializes"),
            r#"{"runHookPayload":"{}"}"#
        );
    }

    /// An envelope with no payload parses rather than failing: the daemon answers
    /// 400 with a log line naming the omission, which a parse error could not.
    #[test]
    fn an_envelope_with_no_payload_parses_as_none() {
        let envelope: RunHookEnvelope = serde_json::from_str("{}").expect("deserializes");
        assert!(envelope.run_hook_payload.is_none());
    }

    /// The payload every launch before the launch-env feature sent still parses,
    /// with an empty environment. This is the compatibility floor: a client pinned
    /// to the old shape must keep bootstrapping.
    #[test]
    fn a_payload_with_only_a_token_parses_with_an_empty_environment() {
        let hook = RunHook::parse(r#"{"agent_token":"tok"}"#).expect("parses");
        assert_eq!(hook.agent_token, "tok");
        assert!(hook.env.is_empty());
    }

    /// The env map comes through with every pair, and an empty value survives —
    /// `FOO=` is a variable set to the empty string, which is a different fact
    /// from `FOO` being unset.
    #[test]
    fn a_payload_with_an_environment_carries_every_pair_including_an_empty_value() {
        let hook =
            RunHook::parse(r#"{"agent_token":"tok","env":{"A":"1","EMPTY":""}}"#).expect("parses");
        assert_eq!(hook.env.get("A").map(String::as_str), Some("1"));
        assert_eq!(hook.env.get("EMPTY").map(String::as_str), Some(""));
        assert_eq!(hook.env.len(), 2);
    }

    /// The identity halves arrive when sent, and are `None` when not.
    ///
    /// The absence is the case that matters: a VM launched without a seed has no key, and the
    /// tunnel refuses `--verify-identity` against it rather than downgrading. Parsing a
    /// missing seed as `Some("")` would make that refusal fire as a handshake failure instead,
    /// which points a reader at the wrong VM.
    #[test]
    fn the_identity_material_parses_when_present_and_is_none_when_absent() {
        let with = RunHook::parse(
            r#"{"agent_token":"tok","identity_seed":"c2VlZA==","identity_host_public_key":"aG9zdA=="}"#,
        )
        .expect("parses");
        assert_eq!(with.identity_seed.as_deref(), Some("c2VlZA=="));
        assert_eq!(with.identity_host_public_key.as_deref(), Some("aG9zdA=="));

        let without = RunHook::parse(r#"{"agent_token":"tok"}"#).expect("parses");
        assert!(without.identity_seed.is_none());
        assert!(without.identity_host_public_key.is_none());

        // An explicit null is the same as absent, for the reason `env: null` is: a generator
        // that fills unset optional fields with null must not fail the launch.
        let nulled =
            RunHook::parse(r#"{"agent_token":"tok","identity_seed":null}"#).expect("parses");
        assert!(nulled.identity_seed.is_none());
    }

    /// The struct's field names are the wire keys the identity module declares.
    ///
    /// Serde derives the wire spelling from the field name, and the *host* composes its
    /// payload from [`crate::identity`]'s constants. So a field renamed here without the
    /// constant — or the reverse — makes the daemon silently read no identity from a payload
    /// that carries one, and the symptom is close code 4401 against a VM that was launched
    /// correctly. This asserts the two spellings are one.
    #[test]
    fn the_identity_field_names_are_the_declared_wire_keys() {
        let hook = RunHook {
            agent_token: "tok".to_string(),
            env: HashMap::new(),
            identity_seed: Some("seed".to_string()),
            identity_host_public_key: Some("host".to_string()),
        };
        let value = serde_json::to_value(&hook).expect("serializes");
        assert_eq!(
            value
                .get(crate::identity::SEED_KEY)
                .and_then(|v| v.as_str()),
            Some("seed"),
            "the seed field must serialize under identity::SEED_KEY"
        );
        assert_eq!(
            value
                .get(crate::identity::HOST_PUBLIC_KEY_KEY)
                .and_then(|v| v.as_str()),
            Some("host"),
            "the host-key field must serialize under identity::HOST_PUBLIC_KEY_KEY"
        );
    }

    /// An identity value of the wrong JSON *type* is refused, naming the key and not the value.
    #[test]
    fn an_identity_value_that_is_not_a_string_is_refused_by_key() {
        let refusal = RunHook::parse(r#"{"agent_token":"tok","identity_seed":7}"#)
            .expect_err("a number is not a base64 string");
        assert_eq!(
            refusal,
            RunHookError::IdentityNotAString(crate::identity::SEED_KEY)
        );
        assert!(refusal.to_string().contains("identity_seed"));

        let host =
            RunHook::parse(r#"{"agent_token":"tok","identity_host_public_key":{"nested":true}}"#)
                .expect_err("an object is not a base64 string");
        assert_eq!(
            host,
            RunHookError::IdentityNotAString(crate::identity::HOST_PUBLIC_KEY_KEY)
        );
    }

    /// An unknown key is ignored rather than refused. A 400 at the run hook makes
    /// the platform terminate the VM before any traffic is forwarded, so a newer
    /// client sending a field this daemon has never heard of must still be able to
    /// bootstrap it.
    #[test]
    fn an_unknown_key_is_ignored_rather_than_failing_the_launch() {
        let hook = RunHook::parse(r#"{"agent_token":"tok","future_field":{"nested":true}}"#)
            .expect("an unknown key does not fail the launch");
        assert_eq!(hook.agent_token, "tok");
        assert!(hook.env.is_empty());
    }

    /// Each malformed shape gets its own named refusal, and none of them quotes a
    /// value. The payload carries the agent token, so a message that echoed a
    /// value would publish secret material into a log line.
    #[test]
    fn each_malformed_payload_names_its_own_problem_without_quoting_a_value() {
        /// Asserts one payload's refusal. A helper because `RunHook` itself is not
        /// `PartialEq` — comparing whole `Result`s would need a derive on a wire
        /// type for the benefit of one test.
        fn refuses(payload: &str, expected: RunHookError) {
            let found = RunHook::parse(payload).expect_err("refused");
            assert_eq!(found, expected, "payload {payload}");
        }

        refuses("not json", RunHookError::NotAnObject);
        refuses("[]", RunHookError::NotAnObject);
        refuses("{}", RunHookError::TokenMissingOrNotAString);
        refuses(
            r#"{"agent_token":7}"#,
            RunHookError::TokenMissingOrNotAString,
        );
        refuses(r#"{"agent_token":""}"#, RunHookError::TokenEmpty);
        refuses(
            r#"{"agent_token":"tok","env":"A=1"}"#,
            RunHookError::EnvNotAnObject,
        );
        refuses(
            r#"{"agent_token":"tok","env":["A=1"]}"#,
            RunHookError::EnvNotAnObject,
        );
        refuses(
            r#"{"agent_token":"tok","env":{"PORT":8080}}"#,
            RunHookError::EnvValueNotAString("PORT".to_string()),
        );

        // The one value in these payloads that is secret-shaped is the token, and
        // no message may carry it.
        let refusal =
            RunHook::parse(r#"{"agent_token":"s3cr3t","env":{"PORT":8080}}"#).expect_err("refused");
        let message = refusal.to_string();
        assert!(!message.contains("s3cr3t"), "{message}");
        assert!(message.contains("PORT"), "the key is nameable: {message}");
    }

    /// `"env": null` is the same as no `env` at all. A generator that fills unset
    /// optional fields with null is common enough that refusing it would be a 400
    /// nobody could debug from the outside.
    #[test]
    fn an_explicitly_null_environment_is_the_same_as_none() {
        let hook = RunHook::parse(r#"{"agent_token":"tok","env":null}"#).expect("parses");
        assert!(hook.env.is_empty());
    }
}
