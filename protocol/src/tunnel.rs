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

/// `?port=<n>` — the guest port to relay to.
#[derive(Debug, Default, Deserialize, JsonSchema, Serialize)]
pub struct TunnelQuery {
    /// The guest port. Dialled on `127.0.0.1` only.
    ///
    /// `0` parses and is refused *after* the upgrade with [`close::BAD_PORT`], rather than
    /// rejected as a parse error: a caller who named it gets a reason naming the port
    /// instead of a 400 that could equally mean a missing parameter.
    pub port: u16,
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
        let failures = [close::NO_LISTENER, close::BAD_PORT, close::RELAY_FAILED];
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

    #[test]
    fn the_dead_server_explanation_names_the_port_and_clears_the_credential() {
        let text = close::explanation(close::NO_LISTENER, 5432).expect("4502 explains itself");
        assert!(text.contains("5432"), "{text}");
        assert!(text.contains("not an auth problem"), "{text}");
    }
}
