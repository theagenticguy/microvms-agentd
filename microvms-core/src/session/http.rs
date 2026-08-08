//! The HTTP seam: one request/response shape, two backends.
//!
//! # Why a trait rather than reqwest directly
//!
//! Everything worth testing about this client sits *above* HTTP — whether both proxy
//! headers went out, whether a mint happened inside the retry path, whether a
//! reconnect resumed at the right byte. reqwest opens real sockets, so none of those
//! are reachable from a test that does not stand up a server, and the two that matter
//! most are only reachable by inspecting a request that was already sent.
//!
//! So [`HttpBackend`] is the seam. Production is [`ReqwestBackend`]; a test supplies a
//! recorder that keeps every request head, or a backend that writes bytes at a
//! simulated network. The daemon made the mirror-image choice with
//! `axum::serve::Listener`, and for the same reason: the simulator stays out of the
//! shipping artifact.
//!
//! # Streaming is a separate method
//!
//! [`HttpBackend::send`] collects the whole body, which for an SSE attach means it
//! returns once the command is over — and every property worth checking about a stream
//! is only observable partway through. [`HttpBackend::open_stream`] hands back a chunk
//! source instead.

use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use futures_util::future::BoxFuture;

use crate::error::{Error, WireKind};

/// One request, already fully addressed: absolute URL, every header, whole body.
///
/// Owned rather than borrowed because a backend may need to move it into a spawned
/// task, and a recorder needs to keep it after the call returns.
#[derive(Clone, Debug)]
pub struct HttpRequest {
    pub method: &'static str,
    /// Path plus query string. Not a full URL: the base is the backend's, so a
    /// rebound endpoint changes one field rather than every call site.
    pub path: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    /// How long the whole exchange may take. `None` means the backend's default.
    pub timeout: Option<Duration>,
}

impl HttpRequest {
    pub fn new(method: &'static str, path: impl Into<String>) -> Self {
        Self {
            method,
            path: path.into(),
            headers: Vec::new(),
            body: Vec::new(),
            timeout: None,
        }
    }

    /// One header's value, matched case-insensitively.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }
}

/// One response, body already collected.
#[derive(Clone, Debug)]
pub struct HttpResponse {
    pub status: u16,
    pub headers: HashMap<String, String>,
    pub body: Vec<u8>,
}

impl HttpResponse {
    /// The typed error for this response, or `Ok` for a 2xx.
    ///
    /// Every non-2xx goes through here, so the status-to-[`WireKind`] table is applied
    /// in exactly one place. A status the daemon never chooses becomes a
    /// [`WireKind::ServerError`]-free plain protocol failure rather than being mapped
    /// to the nearest neighbour — see [`crate::error::WireKind::from_status`] on why
    /// there is no generic 4xx fallback.
    pub fn error_for_status(&self, method: &str, path: &str) -> Result<(), Error> {
        if (200..300).contains(&self.status) {
            return Ok(());
        }
        // Capped at 512 bytes, as the Python does: a detail string is for a human
        // reading a log, and a 256 MB error body in a message is its own incident.
        let detail = String::from_utf8_lossy(&self.body[..self.body.len().min(512)])
            .trim()
            .to_string();
        let message = if detail.is_empty() {
            format!("{method} {path} -> {}", self.status)
        } else {
            format!("{method} {path} -> {}: {detail}", self.status)
        };
        match crate::error::WireKind::from_status(self.status) {
            Some(wire) => Err(Error::wire(wire, message)),
            // A status outside the daemon's vocabulary. `Protocol` rather than
            // `Retryable`, because nothing says a retry would land differently, and
            // rather than a specific wire kind, because inventing one would be this
            // client claiming to know a meaning the daemon never assigned.
            None => Err(Error::new(crate::error::ErrorKind::Protocol, message)),
        }
    }
}

/// A body arriving in pieces.
///
/// One method, because that is all a cursor-driven stream needs: `Ok(None)` is the end
/// of the body, however it ended. Which *way* it ended is not this trait's business —
/// the protocol answers that with the presence or absence of a terminal `exit` event,
/// which is exactly why the transport is framed.
pub trait ChunkSource: Send {
    fn next_chunk(&mut self) -> BoxFuture<'_, Result<Option<Vec<u8>>, Error>>;
}

/// A streaming response: the head, and the body still to arrive.
///
/// Named rather than spelled inline at [`HttpBackend::open_stream`], because the two
/// halves are one thing — the status has to be readable before the first body byte, so a
/// backend cannot hand back one without the other.
pub type OpenStream = (HttpResponse, Box<dyn ChunkSource>);

/// The seam. Production and simulated transports implement this.
pub trait HttpBackend: Send + Sync {
    /// Sends one request and collects the whole body, whatever the status.
    ///
    /// Deliberately does not fail on a status: a conformance suite asserts on 401 and
    /// 409 as expected outcomes, and a client that could only reach them through an
    /// error would be a client that cannot test the protocol.
    fn send(&self, request: HttpRequest) -> BoxFuture<'_, Result<HttpResponse, Error>>;

    /// Opens a streaming response, reading the status before any body byte.
    ///
    /// Status first so a 404 on an unknown exec id surfaces as `NotFound` rather than
    /// as an empty stream. `idle_timeout` bounds *silence* rather than duration: an SSE
    /// body is idle by design between chunks, so the useful bound is how long a gap may
    /// last. Without one, a half-open connection — the failure a NAT or proxy timeout
    /// produces, where no FIN ever arrives — hangs forever and the reconnect logic
    /// never runs.
    fn open_stream(
        &self,
        request: HttpRequest,
        idle_timeout: Duration,
    ) -> BoxFuture<'_, Result<OpenStream, Error>>;
}

/// The production backend: one pooled reqwest client.
///
/// Pooled rather than a client per request, because the daemon drains a bounded prefix
/// of a rejected body specifically so pooled connections keep working, and throwing the
/// pool away per request discards that.
pub struct ReqwestBackend {
    client: reqwest::Client,
    base_url: String,
    timeout: Duration,
}

impl ReqwestBackend {
    /// A backend rooted at `base_url`.
    ///
    /// A bare host is read as `https`. The endpoint the platform hands back is a
    /// hostname, and defaulting to plain HTTP there would send a bearer token in
    /// clear text on the strength of a missing prefix.
    pub fn new(base_url: &str, timeout: Duration) -> Result<Self, Error> {
        let client = reqwest::Client::builder()
            .timeout(timeout)
            .build()
            .map_err(|err| {
                Error::new(
                    crate::error::ErrorKind::Unexpected,
                    format!("could not build an HTTP client: {err}"),
                )
                .with_source(err)
            })?;
        Ok(Self {
            client,
            base_url: normalize_base_url(base_url),
            timeout,
        })
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base_url)
    }

    fn build(&self, request: &HttpRequest) -> reqwest::RequestBuilder {
        let mut builder = self
            .client
            .request(
                reqwest::Method::from_bytes(request.method.as_bytes())
                    .unwrap_or(reqwest::Method::GET),
                self.url(&request.path),
            )
            .timeout(request.timeout.unwrap_or(self.timeout));
        for (name, value) in &request.headers {
            builder = builder.header(name, value);
        }
        if !request.body.is_empty() {
            builder = builder.body(request.body.clone());
        }
        builder
    }
}

/// A bare host becomes `https://`, and a trailing slash is dropped so a path can be
/// concatenated without producing a double one.
fn normalize_base_url(base_url: &str) -> String {
    let with_scheme = if base_url.starts_with("http://") || base_url.starts_with("https://") {
        base_url.to_string()
    } else {
        format!("https://{base_url}")
    };
    with_scheme.trim_end_matches('/').to_string()
}

/// A transport failure. Retryable, because it says nothing about the daemon's state.
fn transport_error(method: &str, path: &str, err: reqwest::Error) -> Error {
    Error::wire(
        WireKind::Transport,
        format!("{method} {path} failed on the wire: {err}"),
    )
    .with_source(err)
}

fn collect_headers(response: &reqwest::Response) -> HashMap<String, String> {
    response
        .headers()
        .iter()
        .map(|(name, value)| {
            (
                name.as_str().to_string(),
                String::from_utf8_lossy(value.as_bytes()).into_owned(),
            )
        })
        .collect()
}

impl HttpBackend for ReqwestBackend {
    fn send(&self, request: HttpRequest) -> BoxFuture<'_, Result<HttpResponse, Error>> {
        Box::pin(async move {
            let response = self
                .build(&request)
                .send()
                .await
                .map_err(|err| transport_error(request.method, &request.path, err))?;
            let status = response.status().as_u16();
            let headers = collect_headers(&response);
            let body = response
                .bytes()
                .await
                .map_err(|err| transport_error(request.method, &request.path, err))?
                .to_vec();
            Ok(HttpResponse {
                status,
                headers,
                body,
            })
        })
    }

    fn open_stream(
        &self,
        request: HttpRequest,
        idle_timeout: Duration,
    ) -> BoxFuture<'_, Result<OpenStream, Error>> {
        Box::pin(async move {
            let response = self
                .build(&request)
                // No overall timeout on a stream: the bound is `idle_timeout`, applied
                // per chunk below. An overall one would cut a healthy long-running
                // command off mid-output.
                .timeout(Duration::MAX)
                .send()
                .await
                .map_err(|err| transport_error(request.method, &request.path, err))?;
            let status = response.status().as_u16();
            let headers = collect_headers(&response);

            // The status is read before any body byte. On a failure the body is
            // collected so the typed error carries the daemon's detail string.
            if !(200..300).contains(&status) {
                let body = response
                    .bytes()
                    .await
                    .map_err(|err| transport_error(request.method, &request.path, err))?
                    .to_vec();
                return Ok((
                    HttpResponse {
                        status,
                        headers,
                        body,
                    },
                    Box::new(EmptyChunks) as Box<dyn ChunkSource>,
                ));
            }

            let head = HttpResponse {
                status,
                headers,
                body: Vec::new(),
            };
            let chunks = ReqwestChunks {
                response: Some(response),
                idle_timeout,
                method: request.method,
                path: request.path,
            };
            Ok((head, Box::new(chunks) as Box<dyn ChunkSource>))
        })
    }
}

/// No body at all, for the failure path where the head already carries everything.
struct EmptyChunks;

impl ChunkSource for EmptyChunks {
    fn next_chunk(&mut self) -> BoxFuture<'_, Result<Option<Vec<u8>>, Error>> {
        Box::pin(async { Ok(None) })
    }
}

struct ReqwestChunks {
    response: Option<reqwest::Response>,
    idle_timeout: Duration,
    method: &'static str,
    path: String,
}

impl ChunkSource for ReqwestChunks {
    fn next_chunk(&mut self) -> BoxFuture<'_, Result<Option<Vec<u8>>, Error>> {
        Box::pin(async move {
            let Some(response) = self.response.as_mut() else {
                return Ok(None);
            };
            match tokio::time::timeout(self.idle_timeout, response.chunk()).await {
                Ok(Ok(Some(bytes))) => Ok(Some(bytes.to_vec())),
                Ok(Ok(None)) => {
                    self.response = None;
                    Ok(None)
                }
                Ok(Err(err)) => {
                    self.response = None;
                    Err(transport_error(self.method, &self.path, err))
                }
                Err(_) => {
                    self.response = None;
                    // Retryable by construction: silence past the keepalive interval
                    // means the connection is dead, and the exec is untouched.
                    Err(Error::wire(
                        WireKind::Transport,
                        format!(
                            "{} {} went silent for {}s, longer than the keepalive \
                             interval, so the connection is treated as dead",
                            self.method,
                            self.path,
                            self.idle_timeout.as_secs()
                        ),
                    ))
                }
            }
        })
    }
}

impl fmt::Debug for ReqwestBackend {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ReqwestBackend")
            .field("base_url", &self.base_url)
            .field("timeout", &self.timeout)
            .finish()
    }
}

/// A backend behind an `Arc`, which is what a [`crate::session::Session`] holds.
pub type SharedBackend = Arc<dyn HttpBackend>;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::ErrorKind;

    /// A bare host is `https`, because the platform hands back a hostname and plain
    /// HTTP there would put a bearer token on the wire in clear text.
    #[test]
    fn a_bare_endpoint_host_is_read_as_https() {
        assert_eq!(
            normalize_base_url("vm-abc.microvms.aws"),
            "https://vm-abc.microvms.aws"
        );
        assert_eq!(
            normalize_base_url("http://127.0.0.1:9000"),
            "http://127.0.0.1:9000",
            "an explicit scheme is honoured, so a local daemon is still reachable"
        );
        assert_eq!(
            normalize_base_url("https://host/"),
            "https://host",
            "a trailing slash would produce a double one on every path"
        );
    }

    /// Each status the daemon chooses becomes its own variant, and an unmapped 4xx
    /// becomes none of them.
    #[test]
    fn a_response_status_becomes_the_wire_kind_the_daemon_meant() {
        let response = |status: u16| HttpResponse {
            status,
            headers: HashMap::new(),
            body: b"{\"error\":\"unknown_exec\",\"detail\":\"e1\"}".to_vec(),
        };
        assert!(response(200).error_for_status("GET", "/v1/health").is_ok());

        let err = response(404)
            .error_for_status("GET", "/v1/exec/e1")
            .expect_err("404 is an error");
        assert_eq!(err.wire_kind(), Some(WireKind::NotFound));
        assert!(
            err.to_string().contains("unknown_exec"),
            "the daemon's detail must survive into the message: {err}"
        );

        let err = response(429)
            .error_for_status("GET", "/v1/health")
            .expect_err("an unmapped status is still an error");
        assert_eq!(err.kind(), ErrorKind::Protocol);
        assert_eq!(
            err.wire_kind(),
            None,
            "a status the daemon never chooses must not be given an invented meaning"
        );
    }

    /// A body long enough to be its own problem is truncated in the message.
    #[test]
    fn an_enormous_error_body_is_capped_in_the_message() {
        let response = HttpResponse {
            status: 500,
            headers: HashMap::new(),
            body: vec![b'x'; 100_000],
        };
        let err = response
            .error_for_status("PUT", "/v1/fs/file")
            .expect_err("500 is an error");
        assert!(err.to_string().len() < 700, "{}", err.to_string().len());
    }

    /// Header lookup on a request is case-insensitive, since a caller may spell a
    /// header either way and the proxy compares insensitively.
    #[test]
    fn a_request_header_is_found_whatever_case_it_was_set_in() {
        let mut request = HttpRequest::new("GET", "/v1/health");
        request
            .headers
            .push(("X-Aws-Proxy-Port".into(), "9000".into()));
        assert_eq!(request.header("x-aws-proxy-port"), Some("9000"));
        assert_eq!(request.header("authorization"), None);
    }
}
