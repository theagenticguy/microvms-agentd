//! File and tar transfer. Four routes, opaque bodies, one query type.
//!
//! There are no request or response *bodies* to model here — a file's contents and an
//! uncompressed tar are byte streams — so [`protocol::fs::FsQuery`] is the entire typed
//! surface, and its failures answer `text/plain` rather than the JSON error body.
//!
//! Two things the daemon's contract makes non-obvious, both of which this module has to
//! preserve rather than improve on:
//!
//! * `mode` is octal **as a string**. `"644"` and `"0644"` mean the same mode, and an
//!   integer would be read as decimal 644 by anything that stringifies it.
//! * A missing `path` key is 400, and a directory where a file was asked for is 400.
//!   Only a genuinely absent path is 404. That distinction is why
//!   [`file_exists`] can be written at all: it keys on 404 and lets every other refusal
//!   through, so a typo in the query does not read as a missing artifact.

use crate::error::{Error, WireKind};

use super::{HttpRequest, Transport};

/// Percent-encodes a query value.
///
/// Hand-rolled rather than a dependency because the character set is small and the
/// alternative is pulling `url` or `percent-encoding` into a crate that has no other
/// use for either. Unreserved characters pass through and everything else is escaped,
/// which is stricter than necessary and never wrong — a path with a space, a `#`, or a
/// `&` in it is the case that silently truncates a query otherwise.
fn encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*byte as char);
            }
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

/// `/v1/fs/file` with a path and an optional mode.
fn file_path(path: &str, mode: Option<&str>) -> String {
    match mode {
        Some(mode) => format!("/v1/fs/file?path={}&mode={}", encode(path), encode(mode)),
        None => format!("/v1/fs/file?path={}", encode(path)),
    }
}

/// `/v1/fs/tar` with a path.
fn tar_path(path: &str) -> String {
    format!("/v1/fs/tar?path={}", encode(path))
}

/// Writes one file, creating parents. `mode` is octal as a string.
pub(crate) async fn upload_file(
    transport: &Transport,
    path: &str,
    data: &[u8],
    mode: Option<&str>,
) -> Result<(), Error> {
    let mut request = HttpRequest::new("PUT", file_path(path, mode));
    request
        .headers
        .push(("content-type".into(), "application/octet-stream".into()));
    request.body = data.to_vec();
    transport.send(request).await?;
    Ok(())
}

/// Reads one file.
pub(crate) async fn download_file(transport: &Transport, path: &str) -> Result<Vec<u8>, Error> {
    let response = transport
        .send(HttpRequest::new("GET", file_path(path, None)))
        .await?;
    Ok(response.body)
}

/// Whether a path exists.
///
/// Only a 404 answers `false`. Every other refusal propagates, because a 400 from a
/// malformed query is not the same fact as an absent file and returning `false` for it
/// would report a client bug as a missing artifact.
pub(crate) async fn file_exists(transport: &Transport, path: &str) -> Result<bool, Error> {
    match download_file(transport, path).await {
        Ok(_) => Ok(true),
        Err(err) if err.wire_kind() == Some(WireKind::NotFound) => Ok(false),
        Err(err) => Err(err),
    }
}

/// Extracts pre-built tar bytes under `remote`, which must be absolute.
///
/// The archive is not inspected here. The daemon enforces the member rules — in-tree
/// symlinks survive, absolute link targets are refused with 400 — and a second check on
/// this side would be a second thing to keep in step with them, which is how the two
/// come to disagree.
pub(crate) async fn upload_tar(
    transport: &Transport,
    remote: &str,
    archive: &[u8],
) -> Result<(), Error> {
    let mut request = HttpRequest::new("PUT", tar_path(remote));
    request
        .headers
        .push(("content-type".into(), "application/x-tar".into()));
    request.body = archive.to_vec();
    transport.send(request).await?;
    Ok(())
}

/// The raw tar bytes of a remote tree, for a caller doing its own unpacking.
///
/// Bytes rather than an extraction, deliberately. The Python's `download_dir` extracts
/// with tarfile's `data` filter, which is the same contract the daemon enforces on
/// upload — the archive describes the VM's filesystem, and the VM is where untrusted
/// work runs, so trusting it on the way out would be the wrong direction of trust.
/// Rust has no equivalent filter in the standard library, and this crate declines to add
/// `tar` for it: an extraction that looked safe and was not is worse than none.
pub(crate) async fn download_tar(transport: &Transport, remote: &str) -> Result<Vec<u8>, Error> {
    let response = transport
        .send(HttpRequest::new("GET", tar_path(remote)))
        .await?;
    Ok(response.body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::testing::{Recorder, Reply, session_with};
    use std::sync::Arc;

    /// A mode is a string on the wire, and it only appears when one was given.
    ///
    /// Sending a default `mode` would be worse than omitting it: the daemon's own
    /// default is what an unset mode means, and naming one here would silently change
    /// every uploaded file's permissions.
    #[tokio::test]
    async fn an_upload_carries_its_octal_mode_as_a_string_only_when_given() {
        let recorder = Recorder::with([Reply::Body(200, Vec::new()), Reply::Body(200, Vec::new())]);
        let (session, _, _) = session_with(Arc::clone(&recorder));

        session
            .upload_file("/tmp/f", b"body", Some("0644"))
            .await
            .expect("uploads");
        let request = recorder.last();
        assert_eq!(request.method, "PUT");
        assert!(request.path.contains("mode=0644"), "{}", request.path);
        assert_eq!(request.body, b"body");

        session
            .upload_file("/tmp/g", b"", None)
            .await
            .expect("uploads");
        assert!(
            !recorder.last().path.contains("mode="),
            "an unset mode must not become a default one: {}",
            recorder.last().path
        );
    }

    /// A path with characters that would truncate a query is escaped.
    ///
    /// `&` is the one that matters: unescaped, `/tmp/a&b` makes `b` look like a second
    /// query parameter and the daemon writes to `/tmp/a`.
    #[tokio::test]
    async fn a_path_with_query_metacharacters_is_percent_encoded() {
        let recorder = Recorder::with([Reply::Body(200, Vec::new())]);
        let (session, _, _) = session_with(Arc::clone(&recorder));

        session
            .upload_file("/tmp/a&b c#d", b"x", None)
            .await
            .expect("uploads");
        let path = recorder.last().path;
        assert_eq!(path, "/v1/fs/file?path=%2Ftmp%2Fa%26b%20c%23d");
        assert!(!path.contains("&b"), "the path truncated the query: {path}");
    }

    /// A download hands back the bytes untouched, including non-UTF-8 ones.
    #[tokio::test]
    async fn a_download_returns_raw_bytes_including_invalid_utf8() {
        let bytes = vec![0xff, 0x00, 0xfe, b'a'];
        let recorder = Recorder::with([Reply::Body(200, bytes.clone())]);
        let (session, _, _) = session_with(Arc::clone(&recorder));

        let got = session.download_file("/tmp/bin").await.expect("downloads");
        assert_eq!(got, bytes);
        assert_eq!(recorder.last().method, "GET");
    }

    /// `file_exists` answers `false` only for a 404. A 400 is a client bug and must not
    /// be reported as an absent file — that conflation is how one defect hid for a full
    /// review round.
    #[tokio::test]
    async fn file_exists_distinguishes_an_absent_path_from_a_malformed_request() {
        let recorder = Recorder::with([
            Reply::Body(200, b"content".to_vec()),
            Reply::Body(404, b"not found".to_vec()),
            Reply::Body(400, b"path is a directory".to_vec()),
        ]);
        let (session, _, _) = session_with(recorder);

        assert!(session.file_exists("/tmp/there").await.expect("exists"));
        assert!(!session.file_exists("/tmp/absent").await.expect("absent"));

        let err = session
            .file_exists("/tmp/dir")
            .await
            .expect_err("a 400 is not an absence");
        assert_eq!(err.wire_kind(), Some(WireKind::ProtocolError));
    }

    /// Tar transfer uses the tar route and carries the archive bytes unmodified.
    #[tokio::test]
    async fn tar_transfer_uses_the_tar_route_and_does_not_touch_the_archive() {
        let archive = vec![b'u'; 1024];
        let recorder = Recorder::with([
            Reply::Body(200, Vec::new()),
            Reply::Body(200, archive.clone()),
        ]);
        let (session, _, _) = session_with(Arc::clone(&recorder));

        session
            .upload_tar("/workspace", &archive)
            .await
            .expect("uploads");
        let request = recorder.last();
        assert_eq!(request.path, "/v1/fs/tar?path=%2Fworkspace");
        assert_eq!(request.body, archive);

        let got = session.download_tar("/workspace").await.expect("downloads");
        assert_eq!(got, archive);
    }

    /// An over-cap upload is a typed 413 rather than a generic failure, so a caller can
    /// tell "too big" from "refused".
    #[tokio::test]
    async fn an_over_cap_upload_surfaces_as_too_large() {
        let recorder = Recorder::with([Reply::Body(413, b"body exceeds max_body_bytes".to_vec())]);
        let (session, _, _) = session_with(recorder);

        let err = session
            .upload_file("/tmp/huge", b"...", None)
            .await
            .expect_err("413");
        assert_eq!(err.wire_kind(), Some(WireKind::TooLarge));
    }

    /// The encoder leaves unreserved characters alone, so an ordinary path stays
    /// readable in a log.
    #[test]
    fn unreserved_characters_pass_through_the_encoder() {
        assert_eq!(encode("abcXYZ019-_.~"), "abcXYZ019-_.~");
        assert_eq!(encode("/"), "%2F");
    }
}
