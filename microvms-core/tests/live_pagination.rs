// SPDX-License-Identifier: Apache-2.0
//! Live tier: the pagination cursor and the colon image ARN, through the **real** signing
//! path against the **real** service.
//!
//! # Why this file exists when 600-odd tests already cover pagination
//!
//! Every other test in this crate reaches the control plane through an injected
//! `Transport`, so the fake answers a `nextToken` this crate invented and the loop reads
//! it back. That proves the *loop* and cannot prove the *request*: `SignedTransport`,
//! `sign_in_place`, and the SigV4 canonical query are all past the seam. A cursor is the
//! one URL member where that gap bites, because a real `nextToken` is opaque and an
//! unencoded one desynchronises the canonical query from the query actually sent.
//!
//! # What a real cursor actually contains, measured
//!
//! Measured 2026-08-15, us-east-1, over 26 consecutive `ListMicrovmImages` cursors and 2
//! `ListMicrovmImageVersions` cursors: they are **688–800 bytes** and their alphabet is
//! URL-safe base64 — alphanumerics plus `-` and `_` in every cursor, plus `=` padding in
//! 6 of the 26. No cursor carried `+` or `/`.
//!
//! That is narrower than the encoder's docs assume, and it is worth writing down rather
//! than rounding to "opaque, so encode everything": `-` and `_` are RFC 3986 unreserved,
//! so [`paths::encode_segment`] passes them through untouched, and the **only** character
//! the encoding is load-bearing for today is `=`. It is still load-bearing. Measured on the
//! same day, the same signed request with the cursor's `=` left raw is refused:
//!
//! ```text
//! nextToken=…%3D  -> 200
//! nextToken=…=    -> 400 {"message":null}
//! ```
//!
//! A **400 with a null message** — not the 403 a signature mismatch would give, and not
//! anything that names a cursor. `{"message":null}` is this service's signature for a
//! request it cannot parse (`docs/PLATFORM.md` records the same null-message shape for an
//! unpriced region), so a caller who dropped the encoding would see a blank 400 on the
//! second page of a listing that worked on its first. The encoder is right and the reason
//! it is right is now a measurement rather than a guess about `+` and `/`.
//!
//! # Ignored by default, and that is not the usual apology
//!
//! `#[ignore]` here means "needs credentials and an account", not "flaky". `cargo test
//! --workspace` runs on a plane with no AWS, and a tier that fails there is a tier people
//! learn to skip. It runs from the live lane:
//!
//! ```text
//! AWS_REGION=us-east-1 cargo test -p microvms-core --test live_pagination -- --ignored
//! ```
//!
//! # Read-only, on purpose
//!
//! Nothing here creates, deletes, or launches anything: every check is a `GET` over
//! resources the account already holds. That is what makes it safe to leave in the live
//! lane at zero marginal cost — `ListMicrovmImages`, `ListMicrovmImageVersions`, and
//! `ListMicrovms` are free, and the assertions are about the *request* rather than about
//! any particular account's contents. A test that needed a 51-image account to say
//! anything would be a test nobody could run, which is how the single-page bugs survived.

use microvms_core::control::transport::{Call, SignedTransport, Transport, paths};
use microvms_core::region::Region;

/// The region every check below runs in. `us-east-1` rather than read from the
/// environment, because these assertions are about a signed request's shape and a region
/// that does not carry MicroVMs answers `AccessDeniedException` with a null message —
/// which would read as this test's failure rather than as the wrong region.
const REGION: Region = Region::UsEast1;

/// A cursor round-trips through the real signing path.
///
/// # What would fail, and how it would read
///
/// `paths::with_next_token` percent-encodes the cursor. The module docs above carry the
/// measured alphabet: today the character that matters is the `=` padding, present in 6 of
/// 26 sampled cursors. Left raw, the same signed request is refused **400
/// `{"message":null}`** — a blank parse failure on page two of a listing whose page one
/// worked, with nothing in it naming a cursor.
///
/// # Falsification, run 2026-08-15
///
/// Two ways, both run. Sending a real `=`-bearing cursor raw through a hand-signed request
/// answered 400 `{"message":null}` where the encoded form answered 200 — that is the
/// measurement in the module docs. And within this test, the page-inequality assertion
/// below is what fails if a cursor is accepted-but-ignored, which a status check alone
/// cannot see.
///
/// # `maxResults=1`, and why the client had to grow the ability to send it
///
/// The production path deliberately sends no page size, so at the service's default page
/// size no image in a normal account produces a cursor at all — this test skipped, and the
/// encoding stayed unexercised against the real signer, which is the whole gap it exists to
/// close. `paths::image_versions_paged` was added for exactly this: a `maxResults` that no
/// production call site can reach, whose only caller is this file. With it, **two versions
/// are enough** to mint a real cursor, and two versions is a condition a real account meets.
#[tokio::test]
#[ignore = "needs real AWS credentials and an account; runs in the live lane"]
async fn a_real_pagination_cursor_survives_the_real_signing_path() {
    let transport = SignedTransport::new(REGION)
        .await
        .expect("credentials resolve; run `aws sts get-caller-identity` to see the same failure");

    let Some((image_arn, cursor)) = first_cursor_from_any_image(&transport).await else {
        eprintln!(
            "SKIP: no image in this account has two versions, so even maxResults=1 mints \
             no cursor. UpdateMicrovmImage is the only operation that adds a version and \
             this client does not implement it, so a fresh account cannot produce one."
        );
        return;
    };

    // The property under test: the cursor came from the service, and sending it back
    // through the client's own path builder is accepted. A signature mismatch here is the
    // encoding bug; a 4xx of any other kind is a malformed path.
    let reply = transport
        .send(Call::get(
            "ListMicrovmImageVersions",
            paths::image_versions_paged(&image_arn, Some(&cursor), Some(1)),
        ))
        .await
        .expect("the paginated request completed");

    assert_eq!(
        reply.status,
        200,
        "a signed request carrying a real cursor must be accepted. A 400 whose body is \
         `{{\"message\":null}}` is the unencoded-cursor bug: the service could not parse \
         the query and said nothing about which member — observed exactly this way on \
         2026-08-15 by dropping `encode_segment` from the cursor. Body: {}",
        String::from_utf8_lossy(&reply.body)
    );

    // And the cursor addressed a *different* page rather than being ignored: a token the
    // service silently dropped would also answer 200, so the status alone proves nothing.
    let page: serde_json::Value =
        serde_json::from_slice(&reply.body).expect("the reply is the model's JSON");
    let items = page["items"]
        .as_array()
        .expect("`items` is required by the model");
    assert!(
        !items.is_empty(),
        "the second page carried no items, so the cursor was not honoured: {page}"
    );

    let first_page: serde_json::Value = serde_json::from_slice(
        &transport
            .send(Call::get(
                "ListMicrovmImageVersions",
                paths::image_versions_paged(&image_arn, None, Some(1)),
            ))
            .await
            .expect("the first-page request completed")
            .body,
    )
    .expect("the reply is the model's JSON");
    let first_versions = version_ids(&first_page);
    let second_versions = version_ids(&page);
    assert_ne!(
        first_versions, second_versions,
        "page two must not be page one; an ignored cursor is a truncated listing that \
         looks complete, which is issue #23's whole shape"
    );
}

/// What a real cursor's alphabet actually is, recorded rather than assumed.
///
/// # Why this reports and asserts only the invariant
///
/// The encoder's own docs justify themselves with `+`, `/`, and `=`. Two of those three
/// never appear: measured 2026-08-15 over 28 real cursors, the alphabet is URL-safe base64
/// — alphanumerics, `-`, `_`, and sometimes `=`. Asserting "a real cursor contains `+`"
/// would be asserting a property of AWS's token format that this project neither controls
/// nor needs, and it would go red the day they change padding.
///
/// So the alphabet is **printed** (it is a measurement, and a measurement's job is to be
/// re-readable on a later run) and the thing asserted is the property the client depends
/// on: whatever the service mints, the encoder emits a query member with no character that
/// could terminate or re-split it. That holds for any alphabet, which is what makes it
/// worth asserting.
#[tokio::test]
#[ignore = "needs real AWS credentials and an account; runs in the live lane"]
async fn a_real_cursors_alphabet_is_recorded_and_survives_the_encoder() {
    let transport = SignedTransport::new(REGION)
        .await
        .expect("credentials resolve");

    let Some((_, cursor)) = first_cursor_from_any_image(&transport).await else {
        eprintln!("SKIP: no image in this account has two versions; see the sibling test");
        return;
    };

    let mut unusual: Vec<char> = cursor
        .chars()
        .filter(|c| !c.is_ascii_alphanumeric())
        .collect();
    unusual.sort_unstable();
    unusual.dedup();
    println!(
        "a real ListMicrovmImageVersions cursor is {} bytes; its non-alphanumeric \
         characters are {unusual:?} (measured 2026-08-15: URL-safe base64, '-' and '_' \
         always, '=' padding in 6 of 26 sampled)",
        cursor.len(),
    );

    // The invariant, independent of AWS's alphabet: nothing survives the encoder that
    // could end the query member or start another one. `=` is in here because it is the
    // one character measurement shows a real cursor carries *and* the service refuses
    // raw — 400 with a null message, see the module docs.
    let encoded = paths::encode_segment(&cursor);
    for c in ['=', '&', '?', '+', '/', '#', ' ', '%'] {
        assert!(
            !encoded.contains(c),
            "the encoder must leave no {c:?} in the query member it emits, or the cursor \
             can be re-split by the service's parser: {encoded}"
        );
    }
    // And it is not encoding-by-destruction: the unreserved characters a URL-safe cursor is
    // mostly made of pass through, so the value the service gets back is the value it sent.
    assert!(
        encoded.contains('-') || encoded.contains('_') || !cursor.contains('-'),
        "RFC 3986 unreserved characters must survive untouched: {encoded}"
    );
}

/// The colon image ARN is what the real service accepts, and the slash form is the
/// `AccessDeniedException` the branch documented.
///
/// # Both halves, because only the pair is evidence
///
/// Asserting the colon form works proves the spelling is *sufficient*. Asserting the
/// slash form is refused proves it is *necessary* — and names the failure mode, which is
/// the part that misleads: IAM evaluates a malformed ARN as a resource no policy matches,
/// so the answer is a **permissions** message about a resource that exists. Twelve fakes
/// in this repo held the slash form, and nothing went red, because no test ever sent one
/// at the service.
///
/// **Falsification** — run 2026-08-15. The slash-form assertion is itself the
/// falsification of the colon-form claim: swap the two ARNs and the test fails, reporting
/// the AccessDenied it expected to be a 200. Confirmed independently the same day with
/// hand-signed requests: the client's exact segment encoding answered 200 for the colon
/// form and 403 `AccessDeniedException` for the slash form, five runs out of five.
#[tokio::test]
#[ignore = "needs real AWS credentials and an account; runs in the live lane"]
async fn a_colon_image_arn_is_accepted_and_the_slash_form_is_denied() {
    let transport = SignedTransport::new(REGION)
        .await
        .expect("credentials resolve");

    let Some(colon_arn) = any_image_arn(&transport).await else {
        eprintln!(
            "SKIP: this account holds no MicroVM images, so there is no real ARN to \
             address. Build one first: `microvm build <binary> --name <name>`."
        );
        return;
    };
    assert!(
        colon_arn.contains(":microvm-image:"),
        "the service's own listing must spell the resource with a colon, which is the \
         measurement the whole ARN change rests on: {colon_arn}"
    );

    let ok = transport
        .send(Call::get(
            "GetMicrovmImage",
            paths::microvm_image(&colon_arn),
        ))
        .await
        .expect("the request completed");
    assert_eq!(
        ok.status,
        200,
        "the colon form is the one the service returns and must be the one it accepts: {}",
        String::from_utf8_lossy(&ok.body)
    );

    // The same image, spelled the way the repo used to spell it everywhere.
    //
    // Retried past a 5xx, and only past a 5xx. The first live run of this test saw a
    // transient **502 Bad Gateway** (an nginx HTML body, not a service envelope) on this
    // exact request, where a hand-signed repeat of the identical URL answered 403 five
    // times out of five. So the gateway in front of this operation can answer for it, and
    // a test that read that 502 as "the slash form is not denied" would report a defect in
    // this branch's central claim on the strength of someone else's flap. The retry is
    // narrow on purpose: a 4xx is never retried, because a 4xx is the answer.
    let slash_arn = colon_arn.replacen(":microvm-image:", ":microvm-image/", 1);
    let mut denied = transport
        .send(Call::get(
            "GetMicrovmImage",
            paths::microvm_image(&slash_arn),
        ))
        .await
        .expect("the request completed");
    for _ in 0..3 {
        if denied.status < 500 {
            break;
        }
        eprintln!(
            "retrying past a {} from the gateway: {}",
            denied.status,
            String::from_utf8_lossy(&denied.body).trim()
        );
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        denied = transport
            .send(Call::get(
                "GetMicrovmImage",
                paths::microvm_image(&slash_arn),
            ))
            .await
            .expect("the request completed");
    }
    assert_eq!(
        denied.status,
        403,
        "the slash form must be refused, and as a *permissions* error rather than a \
         validation one — that misdirection is why it survived twelve fakes: {}",
        String::from_utf8_lossy(&denied.body)
    );
    let body = String::from_utf8_lossy(&denied.body);
    assert!(
        body.contains("AccessDenied") || body.contains("not authorized"),
        "the documented failure mode is an IAM denial for a resource that exists: {body}"
    );
}

/// `ListMicrovms` and `ListMicrovmImages` answer the client's own path builders.
///
/// A shape check rather than a pagination one, and the difference is worth stating: this
/// account cannot make either listing paginate at the service's default page size, so what
/// is proven here is that the two *unpaginated* paths are right — which is what the
/// `microvms_list`/`microvms` split in `paths` changed. The cursor branch of both is
/// covered by the fakes and, for the shape of the cursor itself, by the round trip above.
#[tokio::test]
#[ignore = "needs real AWS credentials and an account; runs in the live lane"]
async fn the_fleet_and_image_listings_answer_their_cursorless_paths() {
    let transport = SignedTransport::new(REGION)
        .await
        .expect("credentials resolve");

    for (operation, path) in [
        ("ListMicrovms", paths::microvms_list(None)),
        ("ListMicrovmImages", paths::microvm_images_list(None, None)),
    ] {
        assert!(
            !path.contains('?'),
            "a cursorless listing emits no query member at all: {path}"
        );
        let reply = transport
            .send(Call::get(operation, path.clone()))
            .await
            .expect("the request completed");
        assert_eq!(
            reply.status,
            200,
            "{operation} at {path} must be accepted: {}",
            String::from_utf8_lossy(&reply.body)
        );
        let page: serde_json::Value =
            serde_json::from_slice(&reply.body).expect("the reply is the model's JSON");
        assert!(
            page["items"].is_array(),
            "`items` is required by the model: {page}"
        );
    }
}

// ── helpers ──────────────────────────────────────────────────────────────────

/// The first `imageArn` the account's image listing carries, or `None` when it is empty.
async fn any_image_arn(transport: &SignedTransport) -> Option<String> {
    let reply = transport
        .send(Call::get(
            "ListMicrovmImages",
            paths::microvm_images_list(None, None),
        ))
        .await
        .ok()?;
    let page: serde_json::Value = serde_json::from_slice(&reply.body).ok()?;
    page["items"]
        .as_array()?
        .iter()
        .find_map(|item| item["imageArn"].as_str().map(str::to_string))
}

/// The first (image ARN, cursor) pair any image's version listing yields at
/// `maxResults=1`, or `None` when every image in the account has a single version.
///
/// Walks every image rather than guessing one, because which image is multi-version is an
/// account fact this test must not hardcode — and stops at the first cursor, because one
/// real cursor is all the signing path needs to be exercised with.
async fn first_cursor_from_any_image(transport: &SignedTransport) -> Option<(String, String)> {
    let reply = transport
        .send(Call::get(
            "ListMicrovmImages",
            paths::microvm_images_list(None, None),
        ))
        .await
        .ok()?;
    let listing: serde_json::Value = serde_json::from_slice(&reply.body).ok()?;
    let images: Vec<String> = listing["items"]
        .as_array()?
        .iter()
        .filter_map(|item| item["imageArn"].as_str().map(str::to_string))
        .collect();

    // `continue` on a per-image failure, never `?`. This read `.ok()?` and that was a real
    // defect in the search: one image whose version listing failed — an image mid-DELETING
    // is enough, and this account had one — aborted the whole walk and returned `None`,
    // which the callers report as "no image has two versions". A helper that cannot
    // distinguish "looked everywhere and found none" from "stopped looking" makes the two
    // cursor tests skip nondeterministically, and a skip is indistinguishable from a pass
    // in the summary. Observed 2026-08-15: the walk gave up on a DELETING image while a
    // 3-version image sat later in the same listing.
    let mut unreadable = 0_usize;
    for arn in &images {
        let Ok(reply) = transport
            .send(Call::get(
                "ListMicrovmImageVersions",
                // The smallest legal page, so two versions are enough to mint a cursor.
                paths::image_versions_paged(arn, None, Some(1)),
            ))
            .await
        else {
            unreadable += 1;
            continue;
        };
        if reply.status != 200 {
            unreadable += 1;
            continue;
        }
        let Ok(page) = serde_json::from_slice::<serde_json::Value>(&reply.body) else {
            unreadable += 1;
            continue;
        };
        if let Some(token) = page["nextToken"].as_str() {
            return Some((arn.clone(), token.to_string()));
        }
    }
    eprintln!(
        "no cursor found across {} image(s); {unreadable} could not be read (an image in \
         DELETING answers a listing error, which is data rather than a failure)",
        images.len(),
    );
    None
}

/// The `imageVersion` of every item in a version listing, in order.
fn version_ids(page: &serde_json::Value) -> Vec<String> {
    page["items"]
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item["imageVersion"].as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}
