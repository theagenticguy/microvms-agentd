//! Authorization, decided before any request body byte is read.
//!
//! Two properties this module exists to hold:
//!
//! * An unauthenticated caller cannot make the daemon allocate. The Python
//!   predecessor buffered the body first and checked authorization second, so an
//!   unauthorized request could force a 256 MB allocation on a VM whose baseline
//!   can be 512 MiB.
//! * A hostile header cannot take the listener down. `hmac.compare_digest` raises
//!   `TypeError` on `str` inputs containing non-ASCII characters, which killed the
//!   predecessor's handler thread and returned `RemoteDisconnected` to the client
//!   instead of a status it could act on. Any caller controls that header, so the
//!   comparison happens on bytes.

use axum::extract::{Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use http_body_util::BodyExt;
use subtle::ConstantTimeEq;

use crate::state::AppState;

/// Constant-time byte comparison. Length inequality is not itself secret — the
/// token length is fixed by the client that minted it — but `subtle` handles the
/// short-circuit correctly so we do not hand-roll it.
pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.ct_eq(b).into()
}

/// Extracts the bearer token bytes from an `Authorization` header.
///
/// Operates on raw bytes throughout: a header value is arbitrary bytes as far as
/// any client is concerned, and refusing to decode it as UTF-8 first is what keeps
/// a `tökén` header from becoming a crash.
pub fn bearer_bytes(headers: &axum::http::HeaderMap) -> Option<&[u8]> {
    let raw = headers.get(header::AUTHORIZATION)?.as_bytes();
    let (scheme, rest) = split_once_byte(raw, b' ')?;
    if !scheme.eq_ignore_ascii_case(b"bearer") {
        return None;
    }
    Some(rest)
}

fn split_once_byte(haystack: &[u8], needle: u8) -> Option<(&[u8], &[u8])> {
    let idx = haystack.iter().position(|&b| b == needle)?;
    Some((&haystack[..idx], &haystack[idx + 1..]))
}

/// Middleware guarding every `/v1/*` control route.
///
/// Runs before the body is polled, so a rejected request never causes the daemon
/// to buffer. On rejection it drains a bounded prefix of the body: leaving unread
/// bytes in the kernel buffer makes hyper close the connection with a TCP RST,
/// which surfaces to a pooled client as a transport error rather than the status
/// we just chose. Draining without a cap would itself be a denial-of-service
/// vector, so a body past the cap gets the status and a closed connection.
pub async fn require_token(
    State(state): State<AppState>,
    request: Request,
    next: Next,
) -> Response {
    let presented = bearer_bytes(request.headers()).map(<[u8]>::to_vec);

    let verdict = match presented {
        // No token installed yet: the control API is not open. 503 rather than
        // 401, and never 404 — clients map 404 onto "file not found", which turns
        // a protocol error into a phantom missing artifact.
        None if !state.is_bootstrapped() => Some(StatusCode::SERVICE_UNAVAILABLE),
        None => Some(StatusCode::UNAUTHORIZED),
        Some(token) => match state.token_matches(&token) {
            None => Some(StatusCode::SERVICE_UNAVAILABLE),
            Some(true) => None,
            Some(false) => Some(StatusCode::UNAUTHORIZED),
        },
    };

    let Some(status) = verdict else {
        return next.run(request).await;
    };

    let drain_cap = state.config().max_drain_bytes;
    drain_bounded(request, drain_cap).await;
    status.into_response()
}

/// Reads and discards up to `cap` bytes of a rejected request's body.
async fn drain_bounded(request: Request, cap: usize) {
    let mut body = request.into_body();
    let mut seen = 0usize;
    while let Some(frame) = body.frame().await {
        let Ok(frame) = frame else { return };
        if let Some(data) = frame.data_ref() {
            seen += data.len();
            if seen >= cap {
                // Past the cap: stop reading and let the connection close.
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::{HeaderMap, HeaderValue};

    #[test]
    fn equal_tokens_compare_equal_and_differing_ones_do_not() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"abcd"));
        assert!(constant_time_eq(b"", b""));
    }

    #[test]
    fn a_non_ascii_header_is_compared_not_crashed() {
        // The predecessor died on exactly this input. Bytes never decode, so
        // there is nothing to raise on.
        let mut headers = HeaderMap::new();
        let hostile = HeaderValue::from_bytes("Bearer tökén".as_bytes()).expect("byte header");
        headers.insert(header::AUTHORIZATION, hostile);

        let token = bearer_bytes(&headers).expect("bearer parsed from raw bytes");
        assert_eq!(token, "tökén".as_bytes());
        assert!(!constant_time_eq(token, b"expected"));
    }

    #[test]
    fn bearer_scheme_is_case_insensitive_and_other_schemes_are_ignored() {
        let mut headers = HeaderMap::new();
        headers.insert(header::AUTHORIZATION, HeaderValue::from_static("bEaReR t"));
        assert_eq!(bearer_bytes(&headers), Some(&b"t"[..]));

        headers.insert(header::AUTHORIZATION, HeaderValue::from_static("Basic t"));
        assert_eq!(bearer_bytes(&headers), None);
    }

    #[test]
    fn a_missing_or_malformed_header_yields_no_token() {
        assert_eq!(bearer_bytes(&HeaderMap::new()), None);

        let mut headers = HeaderMap::new();
        headers.insert(header::AUTHORIZATION, HeaderValue::from_static("Bearer"));
        assert_eq!(bearer_bytes(&headers), None, "no space, so no token");
    }
}
