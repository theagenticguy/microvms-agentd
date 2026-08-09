// SPDX-License-Identifier: Apache-2.0
//! Lifecycle-hook wire types and the prefix the platform calls them under.
//!
//! Not client types. A consumer must never post to these paths — they are the
//! platform's, and `/run` in particular is the one route whose success is not
//! repeatable — but the shapes belong here anyway, because a client generator
//! reading the schema has to be told what the daemon accepts on them.

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

/// The caller's own payload, carrying the per-VM secret.
///
/// Passing the token at launch is what keeps it out of the shared image snapshot.
/// It is safe because the platform forwards no external traffic until this hook
/// returns 200.
#[derive(Debug, Deserialize, JsonSchema, Serialize)]
pub struct RunHook {
    pub agent_token: String,
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
}
