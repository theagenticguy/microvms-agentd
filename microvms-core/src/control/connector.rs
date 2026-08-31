// SPDX-License-Identifier: Apache-2.0
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
//! # TRAP-11, revised: the shell connector is a variant, on measured ground
//!
//! `SHELL_INGRESS` **is** a variant here, and it was not always. This module used to
//! omit it on the claim that it gated a console-only debugging flow — "not a
//! programmatic exec path despite the name" — and that omission made requesting it
//! unwriteable. `docs/PLATFORM.md` (measured 2026-08-15) refutes the claim: the shell
//! endpoint is a real PTY over a WebSocket, and it is programmatically drivable. The
//! ground that actually holds is narrower — **one interactive session is not
//! programmatic exec**: no exec ids, no idempotency, no separated stdout/stderr, no exit
//! codes. So the variant exists for callers that want the PTY, and the exec path never
//! requests it; the lifecycle test in [`crate::control::microvm`] asserts a launch
//! carries exactly the connectors its caller asked for.
//!
//! Two measured constraints travel with the variant (both from `docs/PLATFORM.md`):
//! `ALL_INGRESS` cannot combine with any other ingress connector, and the platform says
//! so only at token-mint time — `RunMicrovm` accepts the invalid set, the VM reaches
//! RUNNING, and it bills until something asks for a shell token.
//! [`crate::control::ControlPlane::run_microvm`] refuses the combination locally
//! instead. The pair that works is `[HTTP_INGRESS, SHELL_INGRESS]`.
//!
//! The sibling half of TRAP-11 — the shell-auth operation — is still closed by the
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
    ///
    /// The union that cannot be intersected: the platform refuses to combine it with
    /// any other ingress connector, and only at token-mint time — see the module docs.
    /// A VM that needs a shell requests [`ConnectorIntent::HttpIngress`] plus
    /// [`ConnectorIntent::ShellIngress`] instead of this.
    AllIngress,
    /// Lets the endpoint proxy reach the VM's HTTP surface, without the shell.
    ///
    /// The finer-grained sibling of [`ConnectorIntent::AllIngress`], and the half of the
    /// measured pair `[HTTP_INGRESS, SHELL_INGRESS]` that keeps the daemon reachable
    /// (`docs/PLATFORM.md`, measured 2026-08-15).
    HttpIngress,
    /// Lets the VM mint shell tokens and serve its PTY WebSocket.
    ///
    /// One interactive session, not programmatic exec — the module docs carry the
    /// revision of TRAP-11 that admitted this variant. Never combined with
    /// [`ConnectorIntent::AllIngress`]; the pair that launches and mints is with
    /// [`ConnectorIntent::HttpIngress`].
    ShellIngress,
    /// Lets the VM reach the internet.
    ///
    /// Omitted by default, which is how you get a VM with no outbound network — the
    /// right default for a daemon that needs none.
    Egress,
}

impl ConnectorIntent {
    /// Every intent, so a test can enumerate the complete set.
    ///
    /// Maintained by hand; the tests below assert its length, so an edit here is a
    /// deliberate one. The set grew from two to four when TRAP-11 was revised — see the
    /// module docs.
    pub const ALL: [ConnectorIntent; 4] = [
        ConnectorIntent::AllIngress,
        ConnectorIntent::HttpIngress,
        ConnectorIntent::ShellIngress,
        ConnectorIntent::Egress,
    ];

    /// The connector's name as the ARN spells it.
    ///
    /// `INTERNET_EGRESS` rather than `EGRESS`: the wire name is not the intent's name,
    /// which is the second reason this is a table rather than a `Display` derive.
    pub fn wire_name(self) -> &'static str {
        match self {
            ConnectorIntent::AllIngress => "ALL_INGRESS",
            ConnectorIntent::HttpIngress => "HTTP_INGRESS",
            ConnectorIntent::ShellIngress => "SHELL_INGRESS",
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

    /// TRAP-11, rewritten on the ground that holds. This test used to assert no intent
    /// names `SHELL_INGRESS`, standing on the claim that the shell was a console-only
    /// debugging path — a claim `docs/PLATFORM.md` (measured 2026-08-15) refutes: the
    /// shell endpoint is a real PTY and programmatically drivable. What holds instead is
    /// that **one interactive session is not programmatic exec**, so the variant exists,
    /// exactly one intent renders it, and the check on the rendered ARNs stays — a
    /// variant named something else must not render `SHELL_INGRESS` either.
    ///
    /// The other half of the revised guard — a launch carries exactly the connectors its
    /// caller asked for — lives with the lifecycle test in `microvm.rs`.
    ///
    /// **Falsification** — add a second variant whose wire name contains `SHELL`, or
    /// rename [`ConnectorIntent::ShellIngress`]'s wire name, and this fails.
    #[test]
    fn shell_ingress_is_one_deliberate_intent() {
        assert_eq!(
            ConnectorIntent::ALL.len(),
            4,
            "four intents, and shell is deliberately one of them"
        );
        let shells: Vec<ConnectorIntent> = ConnectorIntent::ALL
            .into_iter()
            .filter(|intent| intent.wire_name().contains("SHELL"))
            .collect();
        assert_eq!(
            shells,
            vec![ConnectorIntent::ShellIngress],
            "exactly one intent names the shell, and it is the one that says so"
        );
        assert_eq!(
            ConnectorIntent::ShellIngress.arn(&Region::UsEast1),
            "arn:aws:lambda:us-east-1:aws:network-connector:aws-network-connector:SHELL_INGRESS"
        );
    }

    /// `HTTP_INGRESS`, the measured literal — the finer-grained ingress that pairs with
    /// the shell connector, since `ALL_INGRESS` cannot combine with either
    /// (`docs/PLATFORM.md`, measured 2026-08-15).
    #[test]
    fn the_http_ingress_arn_is_the_measured_literal() {
        assert_eq!(
            ConnectorIntent::HttpIngress.arn(&Region::UsEast1),
            "arn:aws:lambda:us-east-1:aws:network-connector:aws-network-connector:HTTP_INGRESS"
        );
    }

    /// No two intents render the same ARN. The string-replace shape this replaced
    /// could produce two identical ARNs if the names ever became substrings of one
    /// another, and that is the failure this asserts against — now across the whole
    /// set rather than the original pair.
    #[test]
    fn no_two_intents_render_the_same_arn() {
        let arns: Vec<String> = ConnectorIntent::ALL
            .iter()
            .map(|intent| intent.arn(&Region::EuWest1))
            .collect();
        for (i, a) in arns.iter().enumerate() {
            for b in &arns[i + 1..] {
                assert_ne!(a, b);
            }
        }
        assert!(!arns[0].contains("INTERNET"), "{}", arns[0]);
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
