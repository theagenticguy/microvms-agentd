//! Network connectors: a closed intent enum, and the ARN each one derives (TRAP-4).
//!
//! # Why an enum rather than a string
//!
//! Two reasons, and both are measurements.
//!
//! The API takes a **fully-qualified ARN** and rejects the bare name with "Malformed
//! network connector ARN" (measured 2026-08-05). `NetworkConnector` in the service model
//! is just `{type: string, max: 2048, min: 1}` — no pattern, no enum — so a free-form
//! parameter passes every check the model states and fails on the wire, and the value
//! that reads most natural to write (`"ALL_INGRESS"`) is exactly the one that fails.
//! Deriving the ARN from an intent means the caller states what they want and the
//! spelling is not theirs to get wrong.
//!
//! # TRAP-11: what is deliberately absent
//!
//! `SHELL_INGRESS` exists in the API and is **not** a variant here. It gates
//! `CreateMicrovmShellAuthToken`, whose documented flow is `ctr task exec` through a
//! console terminal — scoped to debugging, and recommended disabled in production. It is
//! not a programmatic exec path despite the name, and this client's whole reason to
//! exist is that no such path exists.
//!
//! Leaving it out of the enum is what makes requesting it **unwriteable** rather than
//! merely discouraged: there is no `ConnectorIntent` value that renders it, and
//! [`ConnectorIntent::ALL`] is the complete set a test can enumerate. The sibling half
//! of TRAP-11 — never calling the shell-auth operation — is closed the same way, by the
//! absence of a method on [`crate::control::ControlPlane`]; see that module's docs.

use crate::region::Region;

/// The Lambda-managed connectors this client will name, and no others.
///
/// Named for the *intent* rather than for the wire value, because the intent is what a
/// caller has: "let the proxy reach the VM" and "let the VM reach the internet". The
/// wire spellings ([`ConnectorIntent::wire_name`]) are an implementation detail of the
/// ARN.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ConnectorIntent {
    /// Lets the endpoint proxy reach the VM. Required for any session to work.
    AllIngress,
    /// Lets the VM reach the internet.
    ///
    /// Omitted by default, which is how you get a VM with no outbound network — the
    /// right default for a daemon that needs none.
    Egress,
}

impl ConnectorIntent {
    /// Both intents, so a test can enumerate the complete set.
    ///
    /// The TRAP-11 assertion reads this: a `SHELL_INGRESS` variant added later would
    /// appear here and fail the test that pins the set to two.
    pub const ALL: [ConnectorIntent; 2] = [ConnectorIntent::AllIngress, ConnectorIntent::Egress];

    /// The connector's name as the ARN spells it.
    ///
    /// `INTERNET_EGRESS` rather than `EGRESS`: the wire name is not the intent's name,
    /// which is the second reason this is a table rather than a `Display` derive.
    pub fn wire_name(self) -> &'static str {
        match self {
            ConnectorIntent::AllIngress => "ALL_INGRESS",
            ConnectorIntent::Egress => "INTERNET_EGRESS",
        }
    }

    /// The fully-qualified ARN for this connector in `region`.
    ///
    /// One interpolation for both directions. The Python client's earlier shape derived
    /// the egress ARN by string-replacing `ALL_INGRESS` inside the ingress one, which
    /// produced a valid ARN only as long as the two names never became substrings of
    /// each other (`sandbox.py:391-398`).
    ///
    /// The doubled `aws-network-connector` segment is not a typo: the resource type and
    /// the resource name are both present, and the format is copied from
    /// `sandbox.py:398` rather than reconstructed from the ARN grammar.
    pub fn arn(self, region: &Region) -> String {
        format!(
            "arn:aws:lambda:{}:aws:network-connector:aws-network-connector:{}",
            region.as_str(),
            self.wire_name()
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The exact ARN, as a literal, for the region the measurements were taken in. The
    /// format came from a measurement rather than from the ARN grammar, so a
    /// reconstruction of it in the test would agree with a reconstruction in the code.
    #[test]
    fn the_ingress_arn_is_the_measured_literal() {
        assert_eq!(
            ConnectorIntent::AllIngress.arn(&Region::UsEast1),
            "arn:aws:lambda:us-east-1:aws:network-connector:aws-network-connector:ALL_INGRESS"
        );
    }

    /// The egress ARN, likewise — and note the wire name is `INTERNET_EGRESS` while the
    /// variant is `Egress`, which is the thing a `Display` derive would have got wrong.
    #[test]
    fn the_egress_arn_uses_the_internet_egress_wire_name() {
        assert_eq!(
            ConnectorIntent::Egress.arn(&Region::UsEast1),
            "arn:aws:lambda:us-east-1:aws:network-connector:aws-network-connector:INTERNET_EGRESS"
        );
    }

    /// The region is interpolated rather than fixed, for every region including the
    /// escape hatch. TRAP-4 is "for the request region", and a hardcoded `us-east-1`
    /// would pass both tests above.
    #[test]
    fn the_arn_carries_the_request_region_for_every_region() {
        for region in crate::region::MICROVM_REGIONS {
            let arn = ConnectorIntent::AllIngress.arn(&region);
            assert!(
                arn.contains(&format!(":lambda:{}:aws:", region.as_str())),
                "{arn}"
            );
        }
        let unlisted = Region::unlisted("me-south-1");
        assert!(
            ConnectorIntent::Egress
                .arn(&unlisted)
                .contains(":lambda:me-south-1:aws:"),
            "the escape hatch still derives an ARN for its own region"
        );
    }

    /// TRAP-11, the absence half. Two intents, neither of them shell — and the check is
    /// on the rendered ARNs as well as on the variant count, because a variant named
    /// something else could still render `SHELL_INGRESS`.
    ///
    /// **Falsification** — add `ConnectorIntent::ShellIngress` with wire name
    /// `SHELL_INGRESS` and this test fails on both assertions.
    #[test]
    fn no_intent_names_shell_ingress() {
        assert_eq!(
            ConnectorIntent::ALL.len(),
            2,
            "two intents, and shell is not one"
        );
        for intent in ConnectorIntent::ALL {
            let arn = intent.arn(&Region::UsEast1);
            assert!(
                !arn.contains("SHELL"),
                "SHELL_INGRESS gates a debug console, not programmatic exec: {arn}"
            );
            assert!(!intent.wire_name().contains("SHELL"));
        }
    }

    /// The two intents render different ARNs. The string-replace shape this replaced
    /// could produce two identical ARNs if the names ever became substrings of one
    /// another, and that is the failure this asserts against.
    #[test]
    fn the_two_intents_do_not_render_the_same_arn() {
        let ingress = ConnectorIntent::AllIngress.arn(&Region::EuWest1);
        let egress = ConnectorIntent::Egress.arn(&Region::EuWest1);
        assert_ne!(ingress, egress);
        assert!(!ingress.contains("INTERNET"), "{ingress}");
    }

    /// Every derived ARN clears the model's `NetworkConnector` bounds (min 1, max 2048),
    /// including for the longest region name. The model states no pattern, so length is
    /// the only constraint there is to check — and it is checked here rather than
    /// trusted because a 2048-character ARN would be rejected on the wire with the same
    /// "malformed" message a bare name gets.
    #[test]
    fn every_derived_arn_fits_the_models_connector_bounds() {
        for region in crate::region::MICROVM_REGIONS {
            for intent in ConnectorIntent::ALL {
                let arn = intent.arn(&region);
                assert!((1..=2048).contains(&arn.len()), "{arn}");
                assert!(arn.starts_with("arn:aws:lambda:"), "{arn}");
            }
        }
    }
}
