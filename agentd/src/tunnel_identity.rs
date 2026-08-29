// SPDX-License-Identifier: Apache-2.0
//! The daemon's half of the tunnel identity proof: a Noise KK responder over the relay.
//!
//! Separate from [`crate::identity`], which repairs the snapshot-shared machine identifiers
//! and has nothing to do with the tunnel. The name is close because both are "identity"; the
//! concerns do not touch, and this module never reads a file.
//!
//! # What is proved, and to whom
//!
//! The host generates a 32-byte seed per VM and delivers it in the run-hook payload beside
//! the agent token — the one per-VM secret channel the platform offers. The daemon derives
//! its x25519 static from that seed and keeps it in memory. So:
//!
//! * A caller that completes a handshake against the pinned public key learns the far end
//!   holds the seed, and only the launching host and this VM ever had it. That is the proof
//!   the endpoint proxy cannot give, because the proxy terminates TLS itself.
//! * The daemon requires the *host's* static in return, so it learns the peer is the host
//!   that launched it rather than anyone who came to hold the agent token. The token is a
//!   bearer credential the proxy carries on every request; a private key is not.
//!
//! `protocol::identity` carries the full argument for Noise over rustls (a measured
//! cross-compile constraint, not a preference) and the honest ptrace limit. Read it before
//! changing anything here.
//!
//! # The key never leaves this process, and never reaches a child
//!
//! Held in [`crate::state::AppState`] behind the same lock discipline as the agent token, and
//! deliberately in a different slot from the launch environment: the environment reaches
//! every child by design, and key material must not. The existing `env_clear` tests cover the
//! child boundary, and the image byte-scan covers the artifact — the seed is delivered at
//! launch, so it cannot be in the shared snapshot at all.

use std::sync::Arc;

use protocol::identity::{HOST_PUBLIC_KEY_KEY, SEED_BYTES, SEED_KEY, SeedError, seed_from_bytes};

/// A VM's tunnel identity: its own static secret, and the host key it will accept.
///
/// Both halves or neither. A seed with no host key would let the daemon prove *itself* to a
/// caller while accepting a handshake from anyone holding the agent token, which is half a
/// mutual proof presented as a whole one — so [`Material::from_payload`] refuses the pair
/// unless both arrived.
#[derive(Clone)]
pub struct Material {
    /// The seed, which *is* the x25519 static secret. 32 bytes, from the run-hook payload.
    seed: [u8; SEED_BYTES],
    /// The launching host's public key, pinned. A handshake from any other key fails.
    host_public_key: [u8; SEED_BYTES],
}

/// Prints nothing but the fact that material exists.
///
/// The seed is a private key. A derived `Debug` would print it into every log line that
/// formats the state holding it, which is the one thing `docs/TRUST.md` promises never
/// happens — the same reasoning `RunHookPayload`'s hand-written `Debug` records.
impl std::fmt::Debug for Material {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Material(<a seed and a pinned host key>)")
    }
}

/// Why a payload's identity material could not be used.
#[derive(Debug, Eq, PartialEq)]
pub enum MaterialError {
    /// Exactly one half arrived. Names which is missing, because the fix differs: a missing
    /// seed is the host not generating one, a missing host key is the host not publishing its
    /// own — and a caller who read "identity material is invalid" would not know which.
    HalfDelivered { missing: &'static str },
    /// A half is present but is not valid base64.
    NotBase64 { key: &'static str },
    /// A half decoded but is not a usable 32-byte value.
    NotASeed { key: &'static str, error: SeedError },
}

impl std::fmt::Display for MaterialError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MaterialError::HalfDelivered { missing } => write!(
                f,
                "the run-hook payload carries one half of the tunnel identity and not the \
                 other: {missing} is missing. Both halves are required, because a seed \
                 without a pinned host key would prove this VM to a caller while accepting a \
                 handshake from anyone holding the agent token"
            ),
            MaterialError::NotBase64 { key } => write!(
                f,
                "{key} is not valid base64. It travels as standard base64 of exactly \
                 {SEED_BYTES} bytes; the value is not quoted here because it is key material"
            ),
            MaterialError::NotASeed { key, error } => write!(f, "{key}: {error}"),
        }
    }
}

impl std::error::Error for MaterialError {}

impl Material {
    /// Reads both halves out of a parsed run hook, or reports why it could not.
    ///
    /// `Ok(None)` is the ordinary case for a VM launched without identity: absence is not an
    /// error, and a launch must never fail because a caller did not ask for a feature. The
    /// run hook returning 400 makes the platform terminate the VM before any traffic is
    /// forwarded, so the only thing worth failing a launch over is a payload that is
    /// self-contradictory — which is what the error cases below are.
    pub fn from_payload(hook: &protocol::hook::RunHook) -> Result<Option<Self>, MaterialError> {
        let (Some(seed), Some(host)) = (
            hook.identity_seed.as_deref(),
            hook.identity_host_public_key.as_deref(),
        ) else {
            // Neither half is a plain launch. One half is a mistake worth naming.
            return match (
                hook.identity_seed.is_some(),
                hook.identity_host_public_key.is_some(),
            ) {
                (false, false) => Ok(None),
                (true, false) => Err(MaterialError::HalfDelivered {
                    missing: HOST_PUBLIC_KEY_KEY,
                }),
                (false, true) => Err(MaterialError::HalfDelivered { missing: SEED_KEY }),
                // Unreachable: the `let else` above only fires when a half is `None`.
                (true, true) => Ok(None),
            };
        };

        Ok(Some(Self {
            seed: decode(seed, SEED_KEY)?,
            host_public_key: decode(host, HOST_PUBLIC_KEY_KEY)?,
        }))
    }

    /// This VM's public key, derived from the seed.
    ///
    /// Published so a caller can confirm the pin it holds without opening a tunnel, and
    /// because a host that lost its ledger record can still recognise the VM it launched.
    /// Deriving rather than storing: one derivation in the process means the published key
    /// cannot disagree with the key the handshake uses.
    pub fn public_key(&self) -> [u8; SEED_BYTES] {
        let secret = x25519_dalek::StaticSecret::from(self.seed);
        *x25519_dalek::PublicKey::from(&secret).as_bytes()
    }

    /// A Noise responder that will accept only the pinned host key.
    ///
    /// Built per connection, because a `HandshakeState` carries nonces and cannot be reused —
    /// sharing one across tunnels would reuse a nonce, which is the failure mode that turns
    /// an AEAD into plaintext.
    pub fn responder(&self) -> Result<snow::HandshakeState, snow::Error> {
        snow::Builder::new(
            protocol::identity::NOISE_PATTERN
                .parse()
                .expect("the pattern is a compile-time constant this crate's tests parse"),
        )
        .local_private_key(&self.seed)?
        .remote_public_key(&self.host_public_key)?
        .build_responder()
    }
}

/// Decodes one base64 half into a validated 32-byte value.
fn decode(encoded: &str, key: &'static str) -> Result<[u8; SEED_BYTES], MaterialError> {
    use base64::Engine as _;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .map_err(|_| MaterialError::NotBase64 { key })?;
    // The length and all-zero checks live in the protocol crate, so the host refuses exactly
    // what the daemon refuses. A seed the host accepted and the daemon rejects would report a
    // launch as identity-capable and then fail every handshake against it.
    seed_from_bytes(&bytes).map_err(|error| MaterialError::NotASeed { key, error })
}

/// The identity material a tunnel handshake needs, shared behind an `Arc`.
///
/// `None` on a VM launched without a seed, which the tunnel route turns into close code
/// [`protocol::tunnel::close::NO_IDENTITY`] rather than a downgrade.
pub type Shared = Option<Arc<Material>>;

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine as _;

    fn b64(bytes: &[u8]) -> String {
        base64::engine::general_purpose::STANDARD.encode(bytes)
    }

    fn hook(seed: Option<String>, host: Option<String>) -> protocol::hook::RunHook {
        protocol::hook::RunHook {
            agent_token: "tok".to_string(),
            env: std::collections::HashMap::new(),
            identity_seed: seed,
            identity_host_public_key: host,
        }
    }

    #[test]
    fn a_launch_without_identity_is_not_an_error() {
        // The ordinary case, and the one that must never fail a launch: a 400 at the run hook
        // makes the platform terminate the VM before any traffic is forwarded.
        let parsed = Material::from_payload(&hook(None, None)).expect("absence is legal");
        assert!(parsed.is_none());
    }

    #[test]
    fn both_halves_produce_material_whose_public_key_is_derived_from_the_seed() {
        let seed = [3_u8; SEED_BYTES];
        let host = [5_u8; SEED_BYTES];
        let material = Material::from_payload(&hook(Some(b64(&seed)), Some(b64(&host))))
            .expect("valid")
            .expect("present");

        // The published key is x25519(seed), which is what the host derives independently.
        let expected =
            *x25519_dalek::PublicKey::from(&x25519_dalek::StaticSecret::from(seed)).as_bytes();
        assert_eq!(material.public_key(), expected);
        // And it is emphatically not the seed itself — publishing that would hand out the
        // private key.
        assert_ne!(material.public_key(), seed);
    }

    /// One half is refused, and the refusal names which half is missing.
    ///
    /// The security reason rather than tidiness: a seed with no pinned host key would let the
    /// daemon prove itself while accepting a handshake from any agent-token holder, which is
    /// half a mutual proof presented as a whole one.
    #[test]
    fn one_half_of_the_identity_material_is_refused_by_name() {
        let seed_only = Material::from_payload(&hook(Some(b64(&[3_u8; SEED_BYTES])), None))
            .expect_err("half is refused");
        assert_eq!(
            seed_only,
            MaterialError::HalfDelivered {
                missing: HOST_PUBLIC_KEY_KEY
            }
        );
        assert!(seed_only.to_string().contains(HOST_PUBLIC_KEY_KEY));

        let host_only = Material::from_payload(&hook(None, Some(b64(&[5_u8; SEED_BYTES]))))
            .expect_err("half is refused");
        assert_eq!(
            host_only,
            MaterialError::HalfDelivered { missing: SEED_KEY }
        );
    }

    #[test]
    fn a_malformed_half_names_its_key_and_never_its_value() {
        let not_base64 = Material::from_payload(&hook(
            Some("not base64!".to_string()),
            Some(b64(&[5_u8; 32])),
        ))
        .expect_err("refused");
        assert_eq!(not_base64, MaterialError::NotBase64 { key: SEED_KEY });

        // Valid base64, wrong length.
        let short = Material::from_payload(&hook(Some(b64(&[3_u8; 16])), Some(b64(&[5_u8; 32]))))
            .expect_err("refused");
        assert!(matches!(
            short,
            MaterialError::NotASeed {
                key: SEED_KEY,
                error: SeedError::WrongLength(16)
            }
        ));

        // An all-zero seed is refused for the reason the protocol crate gives: every caller
        // with a broken RNG produces it identically, so it proves nothing about which VM
        // answered.
        let zero = Material::from_payload(&hook(Some(b64(&[0_u8; 32])), Some(b64(&[5_u8; 32]))))
            .expect_err("refused");
        assert!(matches!(
            zero,
            MaterialError::NotASeed {
                error: SeedError::AllZero,
                ..
            }
        ));

        // No message may carry the material it rejected.
        let secret = b64(&[0xAB_u8; 16]);
        let message = Material::from_payload(&hook(Some(secret.clone()), Some(b64(&[5_u8; 32]))))
            .expect_err("refused")
            .to_string();
        assert!(!message.contains(&secret), "{message}");
    }

    /// The seed does not appear in a debug rendering of the material.
    ///
    /// A derived `Debug` would print a private key into every log line that formats the state
    /// holding it. The same guard `RunHookPayload` carries, for the same reason.
    #[test]
    fn debug_does_not_print_the_seed() {
        let seed = [0x5A_u8; SEED_BYTES];
        let material = Material::from_payload(&hook(Some(b64(&seed)), Some(b64(&[5_u8; 32]))))
            .expect("valid")
            .expect("present");
        let rendered = format!("{material:?}");
        assert!(!rendered.contains("5A"), "{rendered}");
        assert!(!rendered.contains("90"), "{rendered}");
        assert!(!rendered.contains(&b64(&seed)), "{rendered}");
    }

    /// A responder builds, and a second one is a *different* state.
    ///
    /// Per-connection rather than shared, because a `HandshakeState` carries nonces: reusing
    /// one across tunnels reuses a nonce, which is what turns an AEAD into plaintext.
    #[test]
    fn a_responder_is_built_per_connection() {
        let material = Material::from_payload(&hook(
            Some(b64(&[3_u8; SEED_BYTES])),
            Some(b64(&[5_u8; SEED_BYTES])),
        ))
        .expect("valid")
        .expect("present");
        assert!(material.responder().is_ok());
        assert!(material.responder().is_ok(), "a second tunnel gets its own");
    }

    /// The whole point, as a test: a handshake from the pinned host succeeds and one from any
    /// other key fails.
    ///
    /// Driven in memory rather than over a socket, because what is under test is the key
    /// binding rather than the transport. The relay's own tests cover the wire.
    #[test]
    fn only_the_pinned_host_key_completes_a_handshake() {
        let vm_seed = [7_u8; SEED_BYTES];
        let host_seed = [9_u8; SEED_BYTES];
        let host_public =
            *x25519_dalek::PublicKey::from(&x25519_dalek::StaticSecret::from(host_seed)).as_bytes();

        let material = Material::from_payload(&hook(Some(b64(&vm_seed)), Some(b64(&host_public))))
            .expect("valid")
            .expect("present");

        /// One handshake attempt from a host holding `initiator_seed`, against `material`.
        fn attempt(material: &Material, initiator_seed: [u8; SEED_BYTES]) -> bool {
            let vm_public = material.public_key();
            let mut initiator =
                snow::Builder::new(protocol::identity::NOISE_PATTERN.parse().expect("parses"))
                    .local_private_key(&initiator_seed)
                    .expect("a 32-byte secret")
                    .remote_public_key(&vm_public)
                    .expect("a 32-byte key")
                    .build_initiator()
                    .expect("builds");
            let mut responder = material.responder().expect("builds");

            let mut first = [0_u8; 1024];
            let written = initiator.write_message(&[], &mut first).expect("writes");
            let mut scratch = [0_u8; 1024];
            responder
                .read_message(&first[..written], &mut scratch)
                .is_ok()
        }

        assert!(
            attempt(&material, host_seed),
            "the launching host must be able to connect"
        );
        // A different key is refused: under KK both statics are mixed into the handshake hash,
        // so the responder's decryption fails rather than any check being skippable.
        assert!(
            !attempt(&material, [1_u8; SEED_BYTES]),
            "a caller holding the agent token but not the host key must be refused"
        );
    }
}
