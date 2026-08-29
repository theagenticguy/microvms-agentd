// SPDX-License-Identifier: Apache-2.0
//! The TCP relay's wire types and close codes.
//!
//! Here rather than in `agentd` for ARCH-2's reason: the daemon serves this route and the
//! client drives it, so the shape belongs to the crate both compile against. A close code
//! spelled independently on each side is a code that can drift, and the drift would surface
//! as a tunnel that reports "nothing listening" for a relay failure.
//!
//! The transport this describes rests on a measured platform property: the endpoint proxy
//! carries **binary** WebSocket frames on a **port-scoped** token, byte-exact in both
//! directions (`docs/PLATFORM.md`, 2026-08-29). There is no CONNECT method on the endpoint,
//! which is why arbitrary TCP has to ride inside frames at all.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// `?port=<n>[&identity=true]` — the guest port to relay to, and whether to prove identity.
#[derive(Debug, Default, Deserialize, JsonSchema, Serialize)]
pub struct TunnelQuery {
    /// The guest port. Dialled on `127.0.0.1` only.
    ///
    /// `0` parses and is refused *after* the upgrade with [`close::BAD_PORT`], rather than
    /// rejected as a parse error: a caller who named it gets a reason naming the port
    /// instead of a 400 that could equally mean a missing parameter.
    pub port: u16,
    /// Whether to run a Noise KK handshake before relaying any bytes.
    ///
    /// On this struct rather than in a second query type because one route has one query
    /// contract, and the schema is generated from exactly that: a separate type would need a
    /// second extractor and would not appear in `docs/schema.json` at all.
    ///
    /// Absent means `false`, which is what keeps every layer-2 client working unchanged. True
    /// against a VM launched without a seed is refused with [`close::NO_IDENTITY`] rather than
    /// silently downgraded — a caller who asked to verify identity and got an unverified
    /// tunnel would believe a proof it never received. See [`super::identity`] for what the
    /// handshake proves and its honest limit.
    #[serde(default)]
    pub identity: bool,
}

/// The bare marker subprotocol a MicroVM WebSocket handshake offers first.
///
/// Spelled here as well as in `microvms-core`'s proxy module because both the daemon and the
/// client need it and for different reasons: the client *offers* it, and the daemon *echoes*
/// it on the direct path. The proxied path never shows it to the daemon at all — the proxy
/// consumes all three values and supplies this one to the client itself (measured,
/// `docs/PLATFORM.md`). One definition means the echo cannot disagree with the offer, which
/// would surface as a handshake refused for a reason naming neither side.
pub const WS_MARKER_SUBPROTOCOL: &str = "lambda-microvms";

/// The close codes the relay sends, and the only close codes in this system that mean
/// anything.
///
/// Every WebSocket failure the *platform* produces is 1006 with no reason string (measured),
/// so a caller cannot learn anything from a close it did not originate. These are drawn from
/// RFC 6455's 4000–4999 private range precisely so they cannot be confused with one.
pub mod close {
    /// Normal completion: one side finished and the relay drained. The tunnel worked.
    ///
    /// Spelled here beside the failures so a client's match is exhaustive over one list.
    pub const NORMAL: u16 = 1000;
    /// The guest port refused the connection, or did not accept in time. Nothing is
    /// listening — the credential and the scope were fine.
    pub const NO_LISTENER: u16 = 4502;
    /// `?port=0`, which cannot be dialled.
    pub const BAD_PORT: u16 = 4400;
    /// A read on an established guest connection failed mid-relay.
    pub const RELAY_FAILED: u16 = 4500;
    /// `?identity=1` against a VM launched without an identity seed.
    ///
    /// Its own code rather than a generic refusal, and it is the difference between a
    /// diagnosable mistake and a mystery: the fix is at *launch* time, in a VM that no longer
    /// exists by the time anyone reads this. A downgrade to an unverified tunnel would be
    /// worse than either — the caller asked for proof and would believe one it never got.
    pub const NO_IDENTITY: u16 = 4401;
    /// The Noise handshake failed: a wrong pin, a replayed record, or a corrupted stream.
    ///
    /// Deliberately one code for all three. Under `KK` both static keys are mixed into the
    /// handshake hash, so the daemon cannot distinguish "the caller pinned the wrong VM" from
    /// "the caller is not the launching host" — both arrive as a decryption failure, and a
    /// code that claimed to tell them apart would be guessing. The client knows which key it
    /// offered and can say more; the daemon cannot.
    pub const IDENTITY_REFUSED: u16 = 4403;

    /// A human-readable explanation for a close code this relay may have sent.
    ///
    /// `None` for a code the relay does not originate — notably 1006, which is what every
    /// platform-side failure collapses to and therefore explains nothing on its own. A
    /// client that printed a guess for 1006 would be inventing a diagnosis.
    pub fn explanation(code: u16, port: u16) -> Option<String> {
        match code {
            NORMAL => Some("the tunnel closed cleanly".to_string()),
            NO_LISTENER => Some(format!(
                "nothing is listening on 127.0.0.1:{port} inside the guest. The credential \
                 and its port scope were accepted — this is a dead server, not an auth \
                 problem."
            )),
            BAD_PORT => Some("port 0 cannot be dialled; name the guest port".to_string()),
            RELAY_FAILED => Some(format!(
                "the connection to 127.0.0.1:{port} failed after it had been established"
            )),
            NO_IDENTITY => Some(
                "this VM was launched without an identity seed, so there is no key to prove \
                 anything with. The seed is delivered in the run-hook payload at launch and \
                 cannot be added to a running VM — relaunch with identity enabled, or drop \
                 --verify-identity to use an unverified tunnel."
                    .to_string(),
            ),
            IDENTITY_REFUSED => Some(
                "the identity handshake failed. Either the pinned key is not this VM's — a \
                 record copied from another launch would do that — or the caller does not hold \
                 the launching host's private key. The daemon cannot tell those apart: both \
                 static keys are mixed into the handshake hash, so both arrive as one \
                 decryption failure."
                    .to_string(),
            ),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_failure_code_is_in_the_rfc_private_range_and_distinct() {
        // 4000-4999 is RFC 6455's application range, which is what keeps these from
        // colliding with the platform's 1006.
        let failures = [
            close::NO_LISTENER,
            close::BAD_PORT,
            close::RELAY_FAILED,
            close::NO_IDENTITY,
            close::IDENTITY_REFUSED,
        ];
        for code in failures {
            assert!(
                (4000..5000).contains(&code),
                "{code} is outside the private range and could collide with a platform code"
            );
        }
        let mut seen = std::collections::BTreeSet::new();
        for code in failures {
            assert!(
                seen.insert(code),
                "{code} is used for two different outcomes"
            );
        }
        assert!(!failures.contains(&close::NORMAL), "1000 is not a failure");
    }

    #[test]
    fn a_platform_close_is_not_explained() {
        // 1006 is what a refused handshake, a wrong-scope token, and a dead TCP connection
        // all collapse to. Explaining it would be a guess presented as a diagnosis.
        assert!(close::explanation(1006, 8080).is_none());
        assert!(close::explanation(4999, 8080).is_none());
    }

    /// The two identity refusals are told apart, and each names where the fix is.
    ///
    /// They are the pair most likely to be confused, because both read as "identity did not
    /// work". 4401 is fixable only at launch, in a VM that no longer exists by the time anyone
    /// reads the message; 4403 is a wrong pin or a wrong caller against a VM that is fine. A
    /// reader sent to the wrong one relaunches a healthy VM or debugs a key that was never
    /// delivered.
    #[test]
    fn the_identity_refusals_point_at_different_fixes() {
        let missing = close::explanation(close::NO_IDENTITY, 5432).expect("4401 explains itself");
        assert!(
            missing.contains("relaunch"),
            "the fix for a missing seed is at launch time: {missing}"
        );
        assert!(missing.contains("run-hook payload"), "{missing}");

        let refused =
            close::explanation(close::IDENTITY_REFUSED, 5432).expect("4403 explains itself");
        assert!(
            refused.contains("pinned key"),
            "a failed handshake is about the key, not the launch: {refused}"
        );
        // The daemon genuinely cannot distinguish the two causes, and the message must say so
        // rather than pick one and sound certain.
        assert!(refused.contains("cannot tell those apart"), "{refused}");
        assert_ne!(missing, refused);
    }

    #[test]
    fn the_dead_server_explanation_names_the_port_and_clears_the_credential() {
        let text = close::explanation(close::NO_LISTENER, 5432).expect("4502 explains itself");
        assert!(text.contains("5432"), "{text}");
        assert!(text.contains("not an auth problem"), "{text}");
    }
}
