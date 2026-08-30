// SPDX-License-Identifier: Apache-2.0
//! Idempotency tokens, unique per attempt by construction (TRAP-1).
//!
//! # What a `clientToken` actually is
//!
//! A **permanent** idempotency key, not a request deduplication window. Measured
//! 2026-08-02, the expensive way: delete an image, recreate it from the same bytes
//! under the same name with a token derived from that identity, and the service
//! replays the original create as a no-op. The image then sits in `CREATING` with its
//! builds never scheduled, cannot be deleted (`CREATING` forbids it), and its only
//! version cannot be dropped either (it is the last one). Two images were wedged that
//! way for roughly fifteen hours.
//!
//! # Why there is no way to pass one
//!
//! The previous Python shape defaulted correctly and *accepted* `client_token=<content
//! digest>` — which is precisely the value that wedges an image, offered as the
//! natural thing to pass. So the closure here is the absence of the parameter:
//! [`create_token`] and [`run_token`] take a **scope label** and mint the token
//! themselves. A label lands *next to* the nonce and can never replace it.
//!
//! That is the difference between a default a caller can override and a mistake a
//! caller cannot write. `grep` for `client_token` across this module's public surface
//! finds nothing, and the test at the bottom of this file asserts the label cannot
//! reach the nonce's position no matter what is passed.
//!
//! # The shape, and which end gets truncated
//!
//! `<verb>-<label>-<16 hex>`, with `verb` one of `create` or `run` and the nonce eight
//! bytes of fresh randomness rendered as sixteen hex characters.
//!
//! The label is truncated to its **tail**, not its head: `run`'s scope defaults to the
//! image identifier, which is a full ARN, and an ARN's distinguishing part is the
//! resource name at the end while every ARN in a region shares its prefix. Truncating
//! the head would collapse two different images onto one label.
//!
//! The nonce is **never** truncated. That ordering is the whole property: a shortened
//! label cannot make two attempts collide, because collision-freedom lives entirely in
//! the sixteen hex characters and the truncation happens before them. Found 2026-08-07
//! by the drift checker's coverage report naming `RunMicrovmRequestClientTokenString`
//! as unbound — an ap-northeast-1 ARN carrying a legal 64-character image name mints a
//! 142-character token, over the 128 ceiling, and botocore does not check `max`, so it
//! would have gone to the wire and failed a launch on a field the caller never set.

use crate::constants::MAX_CLIENT_TOKEN_LEN;

/// Bytes of randomness folded into every token: eight, so two attempts one second
/// apart do not collide even under a retry storm.
///
/// Rendered as [`TOKEN_NONCE_HEX_LEN`] hex characters. Both constants exist because
/// the *hex* length is what the ceiling arithmetic needs and deriving it in three
/// places is three places to get the factor of two wrong.
const TOKEN_NONCE_BYTES: usize = 8;

/// The nonce's rendered width, `TOKEN_NONCE_BYTES * 2`.
///
/// Test-only, and deliberately so: production code renders the nonce by iterating the bytes,
/// so a second constant it *used* would be a second thing to keep in step. The tests below
/// need it to check the ceiling arithmetic — and they compare it against
/// `TOKEN_NONCE_BYTES * 2` rather than trusting it, so a change to either is caught.
#[cfg(test)]
const TOKEN_NONCE_HEX_LEN: usize = TOKEN_NONCE_BYTES * 2;

/// How much of the scope label survives into a token.
///
/// Not a cosmetic cap — see the module docs for the 142-character token that motivated
/// it. Sized so that the worst legal scope still fits [`MAX_CLIENT_TOKEN_LEN`]: the
/// test at the bottom of this file asserts the arithmetic rather than trusting it.
const MAX_TOKEN_SCOPE_LEN: usize = 64;

/// The verb prefix, closed over the two operations that carry an idempotency token on
/// this client's paths.
///
/// An enum rather than a `&str` parameter because a third verb is a new operation and
/// should be a change to this file, not a string a call site invents. `UpdateMicrovmImage`
/// is the third `idempotencyToken: true` member in the model and is deliberately absent:
/// this client never calls it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Verb {
    Create,
    Run,
}

impl Verb {
    fn as_str(self) -> &'static str {
        match self {
            Verb::Create => "create",
            Verb::Run => "run",
        }
    }
}

/// An image-create idempotency token, unique per attempt.
///
/// `scope` is a **label only** — a readability aid for whoever reads CloudTrail, folded
/// in beside the nonce. It cannot become the token, and there is no parameter that can:
/// see the module docs for the fifteen-hour wedge that closure exists to prevent
/// (`docs/PLATFORM.md`, "`clientToken` is a permanent idempotency key").
pub fn create_token(scope: &str) -> String {
    mint(Verb::Create, scope, &nonce_bytes())
}

/// A run idempotency token, unique per attempt. Same rule as [`create_token`].
///
/// Cheaper to get wrong than the image case — a replayed run returns the original
/// MicroVM rather than wedging anything — but the failure is worse to read: a caller
/// who asked for a second VM gets the first one's id back, and two callers then drive
/// the same guest.
pub fn run_token(scope: &str) -> String {
    mint(Verb::Run, scope, &nonce_bytes())
}

/// A caller's log-stream prefix with a fresh per-build discriminator: `<prefix>/<16 hex>`.
///
/// # The same mechanism as the token nonce, for the same class of reason
///
/// `logging.cloudWatch.logStream` is an **exact** stream name on the wire — prefixes are
/// unsupported (docs/PLATFORM.md, 'An image build is three VMs and three log streams') —
/// and one image build emits three log streams. A caller's fixed name sent verbatim
/// collapses all three of every build, concurrent builds of different images included,
/// into one stream nothing can tell apart. So the caller's value is a prefix by contract
/// ([`crate::control::CreateImageRequest::log_stream`]), and this appends eight bytes of
/// fresh CSPRNG rendered as sixteen hex characters per create attempt — the exact nonce
/// [`create_token`] folds into an idempotency token, reused rather than reinvented.
///
/// The arithmetic that keeps the result legal is `MAX_USER_LOG_STREAM_LEN`'s: the caller's
/// prefix is capped at 495 before this runs, so `495 + 1 + 16` is exactly the shape's 512
/// ceiling. The debug assertion holds that rather than trusting it.
pub(crate) fn resolve_log_stream(prefix: &str) -> String {
    let nonce = nonce_bytes();
    let mut resolved = String::with_capacity(prefix.len() + 1 + nonce.len() * 2);
    resolved.push_str(prefix);
    resolved.push('/');
    resolved.push_str(&const_hex::encode(nonce));
    debug_assert!(
        resolved.len() <= crate::constants::MAX_LOG_STREAM_LEN,
        "resolved a {}-character stream name, over the {} ceiling: {resolved}",
        resolved.len(),
        crate::constants::MAX_LOG_STREAM_LEN,
    );
    resolved
}

/// The one place a token is assembled, with the randomness passed in.
///
/// Split from the two public functions so a test can drive the *format* with a fixed
/// nonce and still exercise the real assembly, rather than reimplementing the format in
/// the test and asserting a copy of it against itself.
fn mint(verb: Verb, scope: &str, nonce: &[u8]) -> String {
    // The tail, and on a character boundary: `scope` is arbitrary caller text, so
    // slicing by byte index would panic on a multi-byte character straddling the cut.
    // `char_indices` finds the first boundary at or after the cut point, which keeps at
    // most MAX_TOKEN_SCOPE_LEN bytes — never more, which is what the ceiling needs.
    let label = tail(scope, MAX_TOKEN_SCOPE_LEN);

    let mut token = String::with_capacity(verb.as_str().len() + 2 + label.len() + nonce.len() * 2);
    token.push_str(verb.as_str());
    token.push('-');
    token.push_str(label);
    token.push('-');
    // Two lowercase hex digits per byte, so the rendered width is exactly
    // 2 * nonce.len() and the ceiling arithmetic below holds.
    token.push_str(&const_hex::encode(nonce));

    debug_assert!(
        token.len() <= MAX_CLIENT_TOKEN_LEN,
        "minted a {}-character token, over the {MAX_CLIENT_TOKEN_LEN} ceiling: {token}",
        token.len()
    );
    token
}

/// The last `max_bytes` bytes of `text`, cut at a character boundary.
///
/// Returns a possibly-shorter slice when the boundary falls inside a multi-byte
/// character, which is the safe direction: the ceiling is an upper bound.
fn tail(text: &str, max_bytes: usize) -> &str {
    if text.len() <= max_bytes {
        return text;
    }
    let cut = text.len() - max_bytes;
    match text.char_indices().find(|(index, _)| *index >= cut) {
        Some((index, _)) => &text[index..],
        // Unreachable in practice — the loop above finds the final boundary at worst —
        // but an empty label is a correct token and a panic is not.
        None => "",
    }
}

/// Eight bytes of fresh randomness per call, from the kernel CSPRNG via `getrandom`.
///
/// TRAP-1 needs per-attempt distinctness — the token must not repeat across two
/// attempts; it does not have to be unguessable. `getrandom` gives the stronger
/// property anyway, and fails only when the OS pool is genuinely unavailable, in
/// which state a clock-derived nonce (the old fallback here) risks the exact
/// collision TRAP-1 exists to prevent.
fn nonce_bytes() -> [u8; TOKEN_NONCE_BYTES] {
    let mut bytes = [0u8; TOKEN_NONCE_BYTES];
    getrandom::fill(&mut bytes).expect("the OS random pool is available");
    bytes
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use proptest::prelude::*;

    use super::*;

    /// The worst legal scope, from the discovery that motivated the cap: a full
    /// ap-northeast-1 image ARN carrying a 64-character image name. This is the value
    /// that minted 142 characters before the label was truncated.
    fn worst_legal_scope() -> String {
        format!(
            "arn:aws:lambda:ap-northeast-1:123456789012:microvm-image:{}",
            "a".repeat(crate::constants::MAX_IMAGE_NAME_LEN)
        )
    }

    /// The shape, against a fixed nonce so the assertion is on the format rather than
    /// on a copy of the format. `create-`, the label, `-`, sixteen hex characters.
    #[test]
    fn a_token_is_the_verb_the_label_and_sixteen_hex_characters() {
        let token = mint(
            Verb::Create,
            "agentd-conformance",
            &[0xde, 0xad, 0xbe, 0xef, 0, 1, 2, 3],
        );
        assert_eq!(token, "create-agentd-conformance-deadbeef00010203");

        let token = mint(Verb::Run, "vm-7", &[0xff; TOKEN_NONCE_BYTES]);
        assert_eq!(token, "run-vm-7-ffffffffffffffff");
    }

    /// The two verbs are the two this client uses, and they are distinguishable in the
    /// minted value — which is the only reason the verb is in there at all: a CloudTrail
    /// reader seeing `create-…` knows which call it came from.
    #[test]
    fn the_verb_prefix_distinguishes_a_create_from_a_run() {
        assert!(create_token("img").starts_with("create-"));
        assert!(run_token("img").starts_with("run-"));
    }

    /// TRAP-1's headline property, at the scale the success criteria name: 200 draws,
    /// 200 distinct values.
    ///
    /// **Falsification** — replace `nonce_bytes()` in `create_token` with a digest of
    /// the scope (the exact mistake the Python client's `client_token=` parameter
    /// invited) and this collapses to one distinct value out of 200. Verified by doing
    /// it; see the packet's guard proofs.
    #[test]
    fn two_hundred_create_tokens_for_one_scope_are_two_hundred_distinct_values() {
        let scope = "agentd-conformance";
        let minted: HashSet<String> = (0..200).map(|_| create_token(scope)).collect();
        assert_eq!(minted.len(), 200, "a repeated token is a wedged image");
    }

    /// The same for the run path, and across the two verbs at once: 200 of each, 400
    /// distinct, so a create token can never equal a run token either.
    #[test]
    fn create_and_run_tokens_never_collide_with_each_other() {
        let scope = "arn:aws:lambda:us-east-1:123456789012:microvm-image:img";
        let mut minted = HashSet::new();
        for _ in 0..200 {
            minted.insert(create_token(scope));
            minted.insert(run_token(scope));
        }
        assert_eq!(minted.len(), 400);
    }

    /// The ceiling, at the value that discovered it. 142 characters before truncation,
    /// and the assertion is on the real minted length rather than on the arithmetic.
    #[test]
    fn the_worst_legal_scope_fits_the_hundred_twenty_eight_character_ceiling() {
        let scope = worst_legal_scope();
        assert!(
            scope.len() > MAX_TOKEN_SCOPE_LEN,
            "the point of this scope is that it is longer than the label cap"
        );

        for token in [create_token(&scope), run_token(&scope)] {
            assert!(
                token.len() <= MAX_CLIENT_TOKEN_LEN,
                "{} characters, over the {MAX_CLIENT_TOKEN_LEN} ceiling: {token}",
                token.len()
            );
        }
    }

    /// The arithmetic behind the cap, stated so a change to any of the three numbers
    /// fails here rather than on a launch. `run-` is the shorter verb, so `create-`
    /// bounds the total.
    #[test]
    fn the_label_cap_leaves_room_for_the_longest_verb_and_the_whole_nonce() {
        let longest_verb = Verb::Create.as_str().len();
        let worst = longest_verb + 1 + MAX_TOKEN_SCOPE_LEN + 1 + TOKEN_NONCE_HEX_LEN;
        assert!(
            worst <= MAX_CLIENT_TOKEN_LEN,
            "the worst legal token is {worst} characters against a ceiling of {MAX_CLIENT_TOKEN_LEN}"
        );
        assert_eq!(TOKEN_NONCE_HEX_LEN, TOKEN_NONCE_BYTES * 2);
    }

    /// The truncation keeps the **tail**. Two ARNs differing only in their resource
    /// name must produce different labels — truncating the head would collapse them,
    /// because every ARN in a region shares its prefix.
    ///
    /// The image names are long enough that the shared prefix really is what gets cut:
    /// the ARN prefix alone is 57 characters, so a short name leaves the whole scope
    /// under the 64-byte cap and the test would pass without exercising truncation at
    /// all. That near-miss is the reason the length is asserted first.
    #[test]
    fn truncation_keeps_the_tail_so_two_arns_stay_distinguishable() {
        let prefix = "arn:aws:lambda:ap-northeast-1:123456789012:microvm-image:";
        let first_scope = format!("{prefix}alpha-conformance-image-name");
        let second_scope = format!("{prefix}omega-conformance-image-name");
        assert!(
            first_scope.len() > MAX_TOKEN_SCOPE_LEN,
            "these scopes must actually be truncated or the test proves nothing"
        );

        let first = mint(Verb::Run, &first_scope, &[0; TOKEN_NONCE_BYTES]);
        let second = mint(Verb::Run, &second_scope, &[0; TOKEN_NONCE_BYTES]);

        assert_ne!(first, second, "the distinguishing part is at the end");
        assert!(first.contains("alpha-conformance-image-name"), "{first}");
        assert!(second.contains("omega-conformance-image-name"), "{second}");
        assert!(
            !first.contains("arn:aws"),
            "the shared prefix is what got cut: {first}"
        );
    }

    /// The nonce survives truncation intact, whatever the label does. This is the
    /// property that makes collision-freedom independent of the scope: the last sixteen
    /// characters are always the sixteen hex digits.
    #[test]
    fn the_nonce_is_never_the_part_that_gets_cut() {
        let scope = "z".repeat(4096);
        let token = mint(Verb::Create, &scope, &[0xab; TOKEN_NONCE_BYTES]);
        assert!(
            token.ends_with("abababababababab"),
            "the whole nonce must survive: {token}"
        );
        assert_eq!(
            token.len(),
            "create".len() + 1 + MAX_TOKEN_SCOPE_LEN + 1 + TOKEN_NONCE_HEX_LEN
        );
    }

    /// A multi-byte scope does not panic and does not overshoot the ceiling. The cut
    /// point can land inside a character, and slicing by byte index there is a panic in
    /// a code path a caller reaches by naming an image in their own language.
    #[test]
    fn a_multibyte_scope_cuts_on_a_character_boundary_rather_than_panicking() {
        for scope in ["日本語".repeat(64), "é".repeat(200), "🔥".repeat(50)] {
            let token = create_token(&scope);
            assert!(token.len() <= MAX_CLIENT_TOKEN_LEN, "{token}");
            assert!(token.is_char_boundary(token.len()));
        }
    }

    /// An empty scope is a legal token rather than a panic or an empty string: the
    /// model's minimum is 1, and `create--<nonce>` clears it comfortably.
    #[test]
    fn an_empty_scope_still_mints_a_usable_token() {
        let token = create_token("");
        assert_eq!(token, format!("create--{}", &token[8..]));
        // The clientToken shapes state `min: 1`, and nothing outside this crate would catch a
        // violation of it — the note here used to say botocore enforces `min` locally, which is
        // true of botocore and irrelevant to a client that signs with `aws-sigv4` and sends with
        // `reqwest` (issue #24). What makes the constraint unreachable is not a guard but the
        // minter: the verb and the nonce alone are 23 characters, so the empty label cannot
        // collapse the token, and that is what the assertion below says.
        assert_eq!(
            token.len(),
            "create".len() + 2 + TOKEN_NONCE_HEX_LEN,
            "the model's clientToken min is 1 and this clears it: {token}"
        );
    }

    proptest! {
        /// The ceiling and the nonce, over arbitrary caller text. A hand-picked scope
        /// set is exactly where the multi-byte boundary case hides, and the property
        /// that matters — every minted token fits, and every one ends in its full nonce
        /// — has to hold for text this test did not choose.
        #[test]
        fn every_scope_mints_a_token_that_fits_and_keeps_its_whole_nonce(scope: String) {
            let token = mint(Verb::Create, &scope, &[0x5a; TOKEN_NONCE_BYTES]);
            prop_assert!(token.len() <= MAX_CLIENT_TOKEN_LEN, "{}", token);
            prop_assert!(token.ends_with("5a5a5a5a5a5a5a5a"), "{}", token);
            prop_assert!(token.starts_with("create-"), "{}", token);
        }

        /// Two calls with the *same* scope differ, for any scope. The distinctness test
        /// above uses one fixed scope; this one asserts the scope cannot be the thing
        /// that makes two attempts agree — which is precisely what a digest-derived
        /// token would do.
        #[test]
        fn no_scope_makes_two_attempts_produce_the_same_token(scope: String) {
            prop_assert_ne!(create_token(&scope), create_token(&scope));
            prop_assert_ne!(run_token(&scope), run_token(&scope));
        }
    }

    /// A resolved log stream is the caller's prefix, a `/`, and sixteen hex characters —
    /// never the prefix verbatim — and two resolutions of one prefix differ.
    ///
    /// The distinctness is the whole point: one image build is three VMs writing three
    /// streams under an exact-name member, so a resolution that could repeat would
    /// collapse two builds' streams the way a verbatim name collapses every build's.
    ///
    /// **Falsification** — return `prefix.to_string()` from `resolve_log_stream` and the
    /// format assertions and the distinctness assertion all go red.
    #[test]
    fn a_resolved_log_stream_carries_the_prefix_a_slash_and_sixteen_fresh_hex() {
        let first = resolve_log_stream("ci-image");
        let second = resolve_log_stream("ci-image");

        for resolved in [&first, &second] {
            assert!(resolved.starts_with("ci-image/"), "{resolved}");
            let suffix = &resolved["ci-image/".len()..];
            assert_eq!(suffix.len(), TOKEN_NONCE_HEX_LEN, "{resolved}");
            assert!(
                suffix.bytes().all(|b| b.is_ascii_hexdigit()),
                "the discriminator is hex: {resolved}"
            );
            assert_ne!(resolved.as_str(), "ci-image", "never the prefix verbatim");
        }
        assert_ne!(
            first, second,
            "two creates from one request must resolve to two streams, or concurrent \
             builds collapse into one"
        );
    }

    /// The worst legal prefix — `MAX_USER_LOG_STREAM_LEN` characters — resolves to exactly
    /// the shape's 512 ceiling, which is the arithmetic the 495 cap exists to hold.
    #[test]
    fn the_longest_legal_prefix_resolves_to_exactly_the_shapes_ceiling() {
        let prefix = "s".repeat(crate::constants::MAX_USER_LOG_STREAM_LEN);
        let resolved = resolve_log_stream(&prefix);
        assert_eq!(resolved.len(), crate::constants::MAX_LOG_STREAM_LEN);
    }

    /// The randomness source produces distinct draws. A `nonce_bytes` that returned a
    /// constant would pass a format test and fail this one, and it is the single point
    /// every distinctness property above rests on.
    #[test]
    fn the_nonce_source_does_not_repeat_itself() {
        let drawn: HashSet<[u8; TOKEN_NONCE_BYTES]> = (0..200).map(|_| nonce_bytes()).collect();
        assert_eq!(drawn.len(), 200);
        assert!(
            !drawn.contains(&[0u8; TOKEN_NONCE_BYTES]),
            "an all-zero draw is the signature of a read that silently did nothing"
        );
    }
}
